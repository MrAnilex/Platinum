//! The vendor's microphone effects engine, hosted in this process the way the vendor's own service
//! hosts it (see analysis/sonar/native/host-library.md), and `platinum fx-probe`, which feeds it a
//! synthetic test signal and measures what it does.
//!
//! The probe opens no audio device itself and writes no setting. The engine, however, opens loopback
//! captures of the outputs named in its echo-cancellation settings and may write entries in its own
//! registry branch; the probe watches for both while the engine lives and reports them, whatever
//! happens, including after an error or Ctrl+C.

use crate::apo::{Apo as SettingsApi, Channel};
use crate::com::{Com, Guid, Ole};
use crate::debuglog::DebugLog;
use crate::devices::{Devices, Endpoint, State};
use crate::fxhost::{Engine, IID_APO, IID_APO_CONFIGURATION, IID_APO_RT, InitBase, InitSystemEffects2, Locked, MODE_DEFAULT, Negotiation, PropertyKey, Registration, StoreBuilder, single_device_collection};
use crate::guard::{self, Entry};
use crate::testsignal::{Part, Segment, bounds, delay, linear_to_db, render_counting_clips, rms, tone_rms, write_wav};
use crate::wasapi::{Format, Meter};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Where GG installs its effects engine. Without GG, `sonarfiles::engine_dll` finds the copy of
/// the installed driver package, whose folder holds the same libraries.
pub const DEFAULT_ENGINE_DLL: &str = r"C:\Program Files\SteelSeries\GG\apps\sonar\driver\apoDriverPackage\Sonar.APO.dll";
/// The microphone effects object in that DLL ("Sonar Mic Audio Effects").
const MIC_EFFECTS: Guid = Guid { d1: 0x4B77_58C2, d2: 0x9361, d3: 0x4F88, d4: [0x83, 0xE9, 0x44, 0xD8, 0xF0, 0x51, 0x84, 0xEF] };
/// The interface its INF declares; the vendor's host asks the factory for it.
const MIC_INTERFACE: Guid = Guid { d1: 0x658A_4079, d2: 0xB277, d3: 0x4D14, d4: [0x97, 0xE1, 0x03, 0x56, 0x04, 0x4D, 0x81, 0x10] };
/// PKEY_SS_VAD_Setting_repository: the name of the settings branch the engine reads.
const REPOSITORY_KEY: PropertyKey = PropertyKey { format: Guid { d1: 0xF141_E7CC, d2: 0x921B, d3: 0x466D, d4: [0x80, 0x2F, 0x87, 0x9D, 0x6D, 0x4A, 0xEC, 0x17] }, id: 1 };
/// The engine's settings live in the registry under this branch.
const SETTINGS_BRANCH: &str = r"HKLM\SOFTWARE\SteelSeries ApS\Sonar.APO";
/// Microphone settings worth showing before a run (numbers in the engine's numbering).
const MIC_SETTINGS_SHOWN: [i32; 8] = [5, 185, 186, 72, 216, 56, 63, 204];
/// The echo-cancellation reference outputs (kSet_AEC{Game,ChatRender,Media,Aux}DeviceId).
const AEC_REFERENCES: [i32; 4] = [205, 206, 207, 208];

const RATE: u32 = 48_000;
const BLOCK: usize = 480;
/// How long the engine is kept running after the signal, still watched for streams of its own.
const HOLD: Duration = Duration::from_millis(1500);

/// The engine, initialised and locked, with the host objects it was given kept alive as long as it.
pub struct MicEffects {
    // Declared first: dropped (unlocked and released) before the host objects below.
    locked: Locked,
    _host_objects: Vec<Com>,
}

impl MicEffects {
    pub fn format(&self) -> Format {
        self.locked.format()
    }

    /// Runs one block through the engine in place. False when the engine returned silence.
    pub fn process(&mut self, samples: &mut [f32]) -> bool {
        self.locked.process(samples)
    }
}

/// An engine created and initialised, not locked yet.
struct Prepared {
    engine: Engine,
    registration: Registration,
    host_objects: Vec<Com>,
}

/// Creates the engine and initialises it as the vendor's host does for the microphone `mic`:
/// the settings branch is named by the virtual microphone, every other endpoint property comes
/// from the real microphone, the "system effects" store is empty, and the device collection holds
/// the playback side of the virtual microphone, where the processed sound goes.
fn prepare(devices: &Devices, dll: &Path, mic: &Endpoint) -> Result<Prepared, String> {
    let text = |e: crate::com::Error| e.to_string();
    let ole = Ole::load().map_err(text)?;
    let endpoints = devices.list().map_err(text)?;
    let feed = crate::session::microphone_feed(&endpoints).ok_or("no virtual microphone on this machine: the audio driver does not seem to be installed")?;

    let engine = Engine::create(dll, &MIC_EFFECTS, &MIC_INTERFACE).map_err(text)?;
    let registration = engine.registration(&ole).map_err(text)?;

    let endpoint_store = StoreBuilder::new().route(REPOSITORY_KEY, devices.properties(&feed.id).map_err(text)?).fallback(devices.properties(&mic.id).map_err(text)?).build();
    let effects_store = StoreBuilder::new().build();
    let collection = single_device_collection(devices.device(&feed.id).map_err(text)?);
    // Zeroed first, padding included, before it is handed to foreign code; then filled in.
    // SAFETY: every field is an integer, a GUID or a raw pointer, for which all-zero is valid.
    let mut data: InitSystemEffects2 = unsafe { std::mem::zeroed() };
    // The engine reads neither field of the base; the vendor's host puts the interface identifier here.
    data.base = InitBase { size: std::mem::size_of::<InitSystemEffects2>() as u32, clsid: IID_APO };
    data.endpoint_properties = endpoint_store.0;
    data.system_effects_properties = effects_store.0;
    data.device_collection = collection.0;
    data.processing_mode = MODE_DEFAULT;
    engine.initialize(&data).map_err(text)?;
    Ok(Prepared { engine, registration, host_objects: vec![endpoint_store, effects_store, collection] })
}

/// Creates, initialises and locks the engine for `format`, in blocks of `block` frames, ready to
/// process the live stream of the microphone `mic`.
pub fn open_engine(devices: &Devices, dll: &Path, mic: &Endpoint, format: Format, block: usize) -> Result<MicEffects, String> {
    let prepared = prepare(devices, dll, mic).map_err(|e| format!("effects engine: {e}"))?;
    match prepared.engine.output_supported(Some(format), format) {
        Negotiation::Accepted => {}
        other => return Err(format!("the effects engine does not take {format}: {}", negotiation_text(&other))),
    }
    let locked = prepared.engine.lock(format, block).map_err(|(_, e)| format!("effects engine: {e}"))?;
    Ok(MicEffects { locked, _host_objects: prepared.host_objects })
}

pub struct Options {
    pub dll: PathBuf,
    pub channels: u16,
    /// Ask the engine for its diagnostic messages and print them.
    pub verbose: bool,
    /// Write the test signal before and after the engine as WAV files with this path prefix.
    pub save: Option<PathBuf>,
    /// Feed the engine as fast as it goes instead of at the pace of real time.
    pub fast: bool,
}

fn negotiation_text(answer: &Negotiation) -> String {
    match answer {
        Negotiation::Accepted => "accepted".into(),
        Negotiation::Suggested(format) => format!("refused, suggests {format}"),
        Negotiation::Refused(hresult) => format!("refused (HRESULT 0x{:08X})", *hresult as u32),
    }
}

/// The test signal. Levels are RMS; the synthetic voice peaks about 13.6 dB above its RMS, so the
/// loud voice stays at -15 dB to keep its peaks below full scale.
pub(crate) fn test_segments() -> Vec<Segment> {
    vec![
        Segment { name: "silence", part: Part::Silence, seconds: 1.0 },
        Segment { name: "noise -40 dB", part: Part::Noise { db: -40.0 }, seconds: 3.0 },
        Segment { name: "noise -25 dB", part: Part::Noise { db: -25.0 }, seconds: 3.0 },
        Segment { name: "tone 1 kHz -20 dB", part: Part::Tone { hz: 1000.0, db: -20.0 }, seconds: 2.0 },
        Segment { name: "voice -20 dB", part: Part::Voice { db: -20.0 }, seconds: 4.0 },
        Segment { name: "voice -20 in noise -35", part: Part::VoiceInNoise { voice_db: -20.0, noise_db: -35.0 }, seconds: 4.0 },
        Segment { name: "voice -15 dB (loud)", part: Part::Voice { db: -15.0 }, seconds: 3.0 },
        Segment { name: "clicks -12 dB", part: Part::Clicks { db: -12.0 }, seconds: 2.0 },
    ]
}

/// Set by Ctrl+C: the probe stops feeding the engine, releases it and still prints its report.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetConsoleCtrlHandler(handler: Option<unsafe extern "system" fn(u32) -> i32>, add: i32) -> i32;
}

unsafe extern "system" fn on_console_event(_event: u32) -> i32 {
    // The first Ctrl+C asks for a clean stop. A second one falls through to the default handler,
    // which ends the process, in case the engine is stuck inside a call.
    if INTERRUPTED.swap(true, Ordering::SeqCst) { 0 } else { 1 }
}

/// Peak meter of one echo-cancellation reference output. Reading it opens no stream.
struct Reference {
    name: String,
    meter: Meter,
    highest: f32,
    sum: f64,
    readings: u32,
}

/// Every endpoint on which this process was seen with a running stream, and how often it looked.
#[derive(Default)]
struct StreamWatch {
    seen: BTreeSet<String>,
    samples: usize,
    failures: usize,
}

impl StreamWatch {
    fn sample(&mut self, devices: &Devices) {
        match guard::own_streams(devices) {
            Ok(found) => {
                self.samples += 1;
                self.seen.extend(found);
            }
            Err(_) => self.failures += 1,
        }
    }
}

/// What a run measured.
struct Measured {
    blocks: usize,
    silent_blocks: usize,
    worst: f64,
    busy: f64,
    wall: f64,
    cpu: f64,
    memory: [usize; 3],
    interrupted: bool,
    references: Vec<Reference>,
}

/// Value of a setting in a snapshot, without the quotes of text values.
fn snapshot_value(entries: &[Entry], channel: Channel, id: i32) -> Option<(String, String)> {
    entries.iter().find(|e| e.channel == channel && e.id == id).map(|e| (e.name.trim_start_matches("kSet_").to_string(), e.value.trim_matches('"').to_string()))
}

/// Loads, initialises and locks the engine, feeds it the signal in `output` (processed in place),
/// keeps it running a little longer, then releases it. Everything it creates is released before it
/// returns, on every path.
fn exercise(devices: &Devices, mic: &Endpoint, options: &Options, output: &mut [f32], references: &[(String, String)], watch: &mut StreamWatch, private_before: usize) -> Result<Measured, String> {
    let started = Instant::now();
    let prepared = prepare(devices, &options.dll, mic)?;
    let (private_init, _) = guard::memory();
    watch.sample(devices);
    let registration = &prepared.registration;
    println!("engine created and initialised in {:.0} ms", started.elapsed().as_secs_f64() * 1000.0);
    println!("registration: \"{}\" {}, version {}.{}, flags 0x{:X}, inputs {}..{}, outputs {}..{}, max instances {}", registration.name, registration.clsid, registration.version.0, registration.version.1, registration.flags, registration.inputs.0, registration.inputs.1, registration.outputs.0, registration.outputs.1, registration.max_instances);
    println!("              \"{}\", interfaces {}", registration.copyright, registration.interfaces.iter().map(Guid::to_string).collect::<Vec<_>>().join(", "));
    for (name, iid) in [("IAudioProcessingObject", IID_APO), ("IAudioProcessingObjectRT", IID_APO_RT), ("IAudioProcessingObjectConfiguration", IID_APO_CONFIGURATION), ("declared mic interface", MIC_INTERFACE)] {
        println!("              {:<38} {}", name, if prepared.engine.supports(&iid) { "yes" } else { "no" });
    }

    let channel_mask = if options.channels == 1 { 0x4 } else { 0x3 };
    let format = Format { rate: RATE, channels: options.channels, channel_mask, bits: 32, float: true };
    let output_answer = prepared.engine.output_supported(Some(format), format);
    println!("output format {format}: {}", negotiation_text(&output_answer));
    println!("input format {format}: {}", negotiation_text(&prepared.engine.input_supported(None, format)));
    match prepared.engine.latency() {
        Ok(seconds) => println!("latency reported: {:.1} ms", seconds * 1000.0),
        Err(e) => println!("latency: {e}"),
    }
    if !matches!(output_answer, Negotiation::Accepted) {
        return Err(format!("the engine does not take {format}"));
    }
    let mut engine = MicEffects { locked: prepared.engine.lock(format, BLOCK).map_err(|(_, e)| e.to_string())?, _host_objects: prepared.host_objects };
    let (private_lock, _) = guard::memory();
    watch.sample(devices);
    println!("locked for {BLOCK}-frame blocks; feeding {:.0} s of test signal{}", output.len() as f64 / f64::from(options.channels) / f64::from(RATE), if options.fast { " as fast as possible" } else { " at the pace of real time" });

    let mut meters: Vec<Reference> = references.iter().filter_map(|(id, name)| Meter::activate(devices, id).ok().map(|meter| Reference { name: name.clone(), meter, highest: 0.0, sum: 0.0, readings: 0 })).collect();
    let channels = usize::from(options.channels);
    let (mut worst, mut busy, mut silent_blocks, mut blocks, mut interrupted) = (0.0f64, 0.0f64, 0usize, 0usize, false);
    let cpu_start = guard::cpu_seconds();
    let run_start = Instant::now();
    for (index, block) in output.chunks_mut(BLOCK * channels).enumerate() {
        if INTERRUPTED.load(Ordering::SeqCst) {
            interrupted = true;
            break;
        }
        if !options.fast {
            // Absolute deadlines: the loop catches up after the slower iterations below.
            let deadline = run_start + Duration::from_millis(10 * index as u64);
            if let Some(wait) = deadline.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
        }
        let began = Instant::now();
        if !engine.process(block) {
            silent_blocks += 1;
        }
        let spent = began.elapsed().as_secs_f64();
        worst = worst.max(spent);
        busy += spent;
        blocks += 1;
        for reference in &mut meters {
            if let Ok(peak) = reference.meter.peak() {
                reference.highest = reference.highest.max(peak);
                reference.sum += f64::from(peak);
                reference.readings += 1;
            }
        }
        if index % 100 == 50 {
            watch.sample(devices);
        }
    }
    let wall = run_start.elapsed().as_secs_f64();
    let cpu = guard::cpu_seconds() - cpu_start;
    let (private_run, _) = guard::memory();
    if !interrupted {
        let until = Instant::now() + HOLD;
        while Instant::now() < until && !INTERRUPTED.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(250));
            watch.sample(devices);
        }
    }
    drop(engine);
    Ok(Measured {
        blocks,
        silent_blocks,
        worst,
        busy,
        wall,
        cpu,
        memory: [private_init.saturating_sub(private_before), private_lock.saturating_sub(private_before), private_run.saturating_sub(private_before)],
        interrupted,
        references: meters,
    })
}

/// Where the probe keeps its registry copies: they must outlive the temporary folder.
fn work_folder() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("Platinum").join("fx-probe")
}

pub fn run(devices: &Devices, settings: &SettingsApi, mic: &Endpoint, options: &Options) -> Result<(), String> {
    let work = work_folder();
    let stamp = guard::stamp();

    // The signal is made first, so that its memory is not counted against the engine.
    let segments = test_segments();
    let channels = usize::from(options.channels);
    let (input, clipped) = render_counting_clips(&segments, RATE, channels, 1);
    if clipped > 0 {
        return Err(format!("the test signal clips in {clipped} frames: fix its levels first"));
    }
    let mut output = input.clone();

    println!("safety: recording every setting of the five channels and the engine's registry branch");
    let before = guard::snapshot(settings)?;
    let original = work.join("fx-probe-original.reg");
    if !original.exists() {
        guard::registry_snapshot(SETTINGS_BRANCH, &original)?;
        println!("safety: first run, the engine's registry branch is saved as it was in {}", original.display());
    }
    let registry_before = guard::registry_snapshot(SETTINGS_BRANCH, &work.join(format!("fx-probe-{stamp}-before.reg")))?;
    let streams_before = guard::own_streams(devices)?;

    println!();
    println!("microphone effects as set now (read from the settings):");
    for id in MIC_SETTINGS_SHOWN {
        if let Some((name, value)) = snapshot_value(&before, Channel::Mic, id) {
            println!("    {name:<36} {value}");
        }
    }
    let endpoints = devices.list().map_err(|e| e.to_string())?;
    let mut references = Vec::new();
    for id in AEC_REFERENCES {
        let Some((name, device_id)) = snapshot_value(&before, Channel::Mic, id) else { continue };
        if device_id.is_empty() {
            continue;
        }
        match endpoints.iter().find(|e| e.id == device_id) {
            Some(endpoint) if endpoint.state == State::Active => {
                println!("    {name:<36} {} (active: the engine listens to it as its echo reference)", endpoint.name);
                references.push((device_id.clone(), endpoint.name.clone()));
            }
            Some(endpoint) => println!("    {name:<36} {} ({})", endpoint.name, endpoint.state.label()),
            None => println!("    {name:<36} {device_id} (not present)"),
        }
    }
    println!("note: the test signal holds no echo, so echo cancellation is not evaluated; sound playing on the reference outputs can still influence the result");
    println!();

    INTERRUPTED.store(false, Ordering::SeqCst);
    unsafe {
        SetConsoleCtrlHandler(Some(on_console_event), 1);
    }
    let (debug_log, debug_note) = if options.verbose {
        // Read by the engine when it loads. Setting a variable is always sound on Windows.
        unsafe { std::env::set_var("AudioGraphAPOVerbose", "1") };
        let (log, note) = DebugLog::start();
        (Some(log), note)
    } else {
        (None, None)
    };
    if let Some(note) = &debug_note {
        println!("note: {note}");
    }

    let (private_before, _) = guard::memory();
    let mut watch = StreamWatch::default();
    let outcome = exercise(devices, mic, options, &mut output, &references, &mut watch, private_before);

    // The engine is gone. Each check below runs even if another one fails.
    let streams_after = guard::own_streams(devices);
    let after = guard::snapshot(settings);
    let registry_after = guard::registry_snapshot(SETTINGS_BRANCH, &work.join(format!("fx-probe-{stamp}-after.reg")));
    let debug_lines = debug_log.as_ref().map(DebugLog::lines).unwrap_or_default();
    drop(debug_log);
    unsafe {
        SetConsoleCtrlHandler(Some(on_console_event), 0);
    }

    if let Ok(measured) = &outcome {
        print_measurements(&segments, &input, &output, channels, measured);
    }
    if options.verbose {
        println!();
        println!("engine messages ({}):", debug_lines.len());
        for line in debug_lines.iter().take(80) {
            println!("    {line}");
        }
        if debug_lines.len() > 80 {
            println!("    ... {} more", debug_lines.len() - 80);
        }
    }

    println!();
    let opened: Vec<&String> = watch.seen.iter().filter(|s| !streams_before.contains(s)).collect();
    if opened.is_empty() {
        println!("safety: no audio stream of this process was seen in {} looks while the engine was alive ({} failed)", watch.samples, watch.failures);
    } else {
        println!("safety: while the engine was alive, this process had streams on: {}", opened.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("; "));
    }
    match &streams_after {
        Ok(left) if left.iter().any(|s| !streams_before.contains(s)) => println!("safety: streams still open after the engine was released: {}", left.join("; ")),
        Ok(_) => println!("safety: no stream left open after the engine was released"),
        Err(e) => println!("safety: could not look for streams left open: {e}"),
    }
    match &after {
        Ok(after) => {
            let changed = guard::changes(&before, after);
            if changed.is_empty() {
                println!("safety: all {} settings of the five channels are unchanged", before.len());
            } else {
                println!("safety: {} setting(s) changed during the probe:", changed.len());
                for (old, new) in &changed {
                    println!("    {} {} {}: {} -> {}", old.channel.label(), old.id, old.name, old.value, new.map_or("(gone)", |n| n.value.as_str()));
                }
            }
        }
        Err(e) => println!("safety: could not read the settings again: {e}"),
    }
    match &registry_after {
        Ok(registry_after) => report_registry(&registry_before, registry_after, &work, &stamp),
        Err(e) => println!("safety: could not export the engine's registry branch again: {e}"),
    }

    if let (Some(prefix), Ok(_)) = (&options.save, &outcome) {
        for (suffix, samples) in [("in", &input), ("out", &output)] {
            let path = PathBuf::from(format!("{}-{suffix}.wav", prefix.display()));
            match write_wav(&path, samples, options.channels, RATE) {
                Ok(()) => println!("saved {}", path.display()),
                Err(e) => println!("warning: cannot write {}: {e}", path.display()),
            }
        }
    }
    outcome.map(|_| ())
}

fn report_registry(before: &[String], after: &[String], work: &Path, stamp: &str) {
    let (added, removed) = guard::registry_changes(before, after);
    if added.is_empty() && removed.is_empty() {
        println!("safety: the engine's registry branch is unchanged ({} lines)", before.len());
        return;
    }
    let undo = guard::undo_commands(before, &added, &removed);
    let mut text = String::new();
    for line in &added {
        text.push_str(&format!("+ {line}\n"));
    }
    for line in &removed {
        text.push_str(&format!("- {line}\n"));
    }
    text.push_str("\nTo undo, if wanted:\n");
    for command in &undo {
        text.push_str(&format!("{command}\n"));
    }
    let file = work.join(format!("fx-probe-{stamp}-changes.txt"));
    let saved = std::fs::write(&file, text).is_ok();
    println!("safety: the engine's registry branch changed: {} line(s) added, {} removed{}", added.len(), removed.len(), if saved { format!(" (details in {})", file.display()) } else { String::new() });
    for line in added.iter().take(30) {
        println!("    + {line}");
    }
    for line in removed.iter().take(30) {
        println!("    - {line}");
    }
    println!("safety: to undo these changes on purpose:");
    for command in &undo {
        println!("    {command}");
    }
}

fn print_measurements(segments: &[Segment], input: &[f32], output: &[f32], channels: usize, measured: &Measured) {
    let processed = measured.blocks * BLOCK * channels;
    let (input, output) = (&input[..processed.min(input.len())], &output[..processed.min(output.len())]);
    println!();
    if measured.interrupted {
        println!("interrupted after {} blocks: the figures below cover that part only", measured.blocks);
    }
    let audio = measured.blocks as f64 * 0.01;
    println!(
        "processing: {} blocks of 10 ms, {:.3} ms on average, {:.3} ms at worst ({:.1} % of real time on the calling thread), {} returned as silence",
        measured.blocks,
        measured.busy / measured.blocks.max(1) as f64 * 1000.0,
        measured.worst * 1000.0,
        measured.busy / audio.max(1e-9) * 100.0,
        measured.silent_blocks
    );
    println!("processor time of the whole process during the run: {:.2} s for {:.1} s of audio over {:.1} s ({:.1} % of one core)", measured.cpu, audio, measured.wall, measured.cpu / measured.wall.max(1e-9) * 100.0);
    println!("memory above the starting point: {:.1} MB after initialisation, {:.1} MB after locking, {:.1} MB after the run", guard::megabytes(measured.memory[0]), guard::megabytes(measured.memory[1]), guard::megabytes(measured.memory[2]));
    for reference in &measured.references {
        let mean = reference.sum / f64::from(reference.readings.max(1));
        println!("echo reference {}: mean peak {:.4}, highest {:.4}{}", reference.name, mean, reference.highest, if reference.highest > 0.001 { " (not silent: the result may depend on what was playing)" } else { " (silent)" });
    }
    let differing = input.iter().zip(output).filter(|(a, b)| (*a - *b).abs() > 1e-6).count();
    let largest = input.iter().zip(output).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
    if differing == 0 {
        println!("result: the output is identical to the input: the engine let the audio through untouched");
    } else {
        println!("result: {differing} of {} samples changed (largest change {largest:.4})", input.len());
    }

    let spans = bounds(segments, RATE);
    let settle = (RATE / 2) as usize;
    let available = input.len() / channels;
    println!();
    println!("{:<26} {:>9} {:>9} {:>8}", "segment", "in dB", "out dB", "change");
    for (segment, (start, end)) in segments.iter().zip(&spans) {
        // The first half second of each segment is left out, while the effects adapt.
        let (from, to) = ((*start + settle).min(*end), (*end).min(available));
        if from >= to {
            continue;
        }
        let (level_in, level_out) = (linear_to_db(rms(input, channels, from, to)), linear_to_db(rms(output, channels, from, to)));
        let change = if level_in.is_finite() && level_out.is_finite() { format!("{:+.1}", level_out - level_in) } else { "-".into() };
        println!("{:<26} {:>9.1} {:>9.1} {:>8}", segment.name, level_in, level_out, change);
    }
    if let Some(((start, end), _)) = spans.iter().zip(segments).find(|(_, s)| matches!(s.part, Part::Tone { .. })) {
        let (from, to) = (start + settle, (*end).min(available));
        if from < to {
            println!("tone component: in {:.1} dB, out {:.1} dB", linear_to_db(tone_rms(input, channels, RATE, 1000.0, from, to)), linear_to_db(tone_rms(output, channels, RATE, 1000.0, from, to)));
        }
    }
    if let Some(((start, end), _)) = spans.iter().zip(segments).find(|(_, s)| matches!(s.part, Part::Voice { db } if db < -18.0)) {
        // The engine's own delay was measured at 85 ms: search well beyond it.
        let (lag, correlation) = delay(input, output, channels, start + settle, (*end).min(available), 12_000);
        println!("delay on the voice: {lag} frames ({:.1} ms), correlation {correlation:.3} (searched up to 250 ms)", lag as f64 * 1000.0 / f64::from(RATE));
    }
}

/// Default path prefix of a recording: the current folder, which the user can always see.
/// (%LOCALAPPDATA% was the first choice, but when platinum runs under a packaged application,
/// Windows redirects writes there to the package's private folder, where Explorer cannot see them.)
fn default_recording_prefix() -> PathBuf {
    PathBuf::from(format!("platinum-{}", guard::stamp()))
}

/// `platinum fx-record`: records the microphone for `seconds`, as captured and after the effects
/// engine, into two WAV files, so that the two can be compared by ear. The microphone is opened in
/// shared mode, as any recording application does; nothing is played anywhere and no setting is
/// written. Ctrl+C stops early and still writes what was recorded.
pub fn record(devices: &Devices, mic: &Endpoint, dll: &Path, seconds: f64, prefix: Option<PathBuf>) -> Result<(), String> {
    let text = |e: crate::com::Error| e.to_string();
    let client = crate::wasapi::AudioClient::activate(devices, &mic.id).map_err(text)?;
    let format = client.mix_format(devices.ole()).map_err(text)?;
    let format = format.as_float_at(format.rate);
    let channels = usize::from(format.channels);
    // 10 ms blocks, the period the engine is used to.
    let block = (format.rate / 100) as usize;
    let mut engine = open_engine(devices, dll, mic, format, block)?;
    let event = crate::wasapi::Event::new().map_err(text)?;
    client.initialize(crate::wasapi::EVENT_DRIVEN | crate::wasapi::CONVERT, 250, &format).map_err(text)?;
    client.set_event(&event).map_err(text)?;
    let capture = client.capture_client().map_err(text)?;

    let wanted = (seconds * f64::from(format.rate)) as usize;
    let mut raw: Vec<f32> = Vec::with_capacity(wanted * channels);
    let mut processed: Vec<f32> = Vec::with_capacity(wanted * channels);
    let mut framer = crate::dsp::Framer::new(channels, block);
    let mut scratch = vec![0.0f32; block * channels];

    INTERRUPTED.store(false, Ordering::SeqCst);
    unsafe {
        SetConsoleCtrlHandler(Some(on_console_event), 1);
    }
    client.start().map_err(text)?;
    println!("recording {seconds:.0} s from {} ({format}): speak now, with pauses, and make some everyday noise too", mic.name);
    let mut line_from = 0usize;
    let looped: Result<(), String> = (|| {
        while raw.len() / channels < wanted && !INTERRUPTED.load(Ordering::SeqCst) {
            event.wait(100);
            while capture
                .read(channels, |samples, frames| match samples {
                    Some(samples) => {
                        raw.extend_from_slice(samples);
                        framer.push(samples);
                    }
                    None => {
                        raw.resize(raw.len() + frames * channels, 0.0);
                        framer.push_silence(frames);
                    }
                })
                .map_err(text)?
            {}
            framer.drain(|samples| {
                scratch.copy_from_slice(samples);
                engine.process(&mut scratch);
                processed.extend_from_slice(&scratch);
            });
            // One line per second: what the microphone gave and what the engine made of it.
            let second = format.rate as usize * channels;
            if processed.len() >= line_from + second {
                let range = line_from..line_from + second;
                println!("[{:>3} s] microphone {:>6.1} dB   after the effects {:>6.1} dB", (line_from + second) / second, linear_to_db(rms(&raw[range.clone()], channels, 0, format.rate as usize)), linear_to_db(rms(&processed[range], channels, 0, format.rate as usize)));
                line_from += second;
            }
        }
        Ok(())
    })();
    let _ = client.stop();
    unsafe {
        SetConsoleCtrlHandler(Some(on_console_event), 0);
    }
    drop(engine);
    looped?;

    let prefix = prefix.unwrap_or_else(default_recording_prefix);
    for (suffix, samples) in [("raw", &raw), ("effects", &processed)] {
        let path = PathBuf::from(format!("{}-{suffix}.wav", prefix.display()));
        write_wav(&path, samples, format.channels, format.rate).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        let shown = std::fs::canonicalize(&path).map(|full| full.display().to_string().trim_start_matches(r"\\?\").to_string()).unwrap_or_else(|_| path.display().to_string());
        println!("saved {shown}");
    }
    // Quiet moments show the background noise, loud ones the voice (when there was talking).
    let window = format.rate as usize / 10;
    let (before, after) = (crate::testsignal::window_levels(&raw, channels, window), crate::testsignal::window_levels(&processed, channels, window));
    let db = |levels: &[f64], fraction: f64| linear_to_db(crate::testsignal::percentile(levels, fraction));
    println!("background noise (quietest tenth): microphone {:.1} dB, after the effects {:.1} dB", db(&before, 0.1), db(&after, 0.1));
    println!("loud moments (loudest tenth):      microphone {:.1} dB, after the effects {:.1} dB", db(&before, 0.9), db(&after, 0.9));
    println!("note: the processed file runs about 85 ms behind the raw one, the engine's own delay");
    Ok(())
}

/// `platinum fx-soak`: looks for a memory leak in the effects engine, out of line. One engine, as
/// the relay opens it, is fed `minutes` of each kind of sound in turn (silence, noise, synthetic
/// voice, voice in noise), as fast as it goes, and this process's memory is printed after every
/// minute of sound. Opens no audio device, plays nothing and writes no setting; the engine's
/// registry branch is compared before and after, as `fx-probe` does.
pub fn soak(devices: &Devices, dll: &Path, mic: &Endpoint, minutes: u32) -> Result<(), String> {
    let work = work_folder();
    let stamp = guard::stamp();
    let registry_before = guard::registry_snapshot(SETTINGS_BRANCH, &work.join(format!("fx-soak-{stamp}-before.reg")))?;
    let format = Format { rate: RATE, channels: 2, channel_mask: 0x3, bits: 32, float: true };
    let (private_start, _) = guard::memory();
    let mut engine = open_engine(devices, dll, mic, format, BLOCK)?;
    let (private_open, _) = guard::memory();
    println!("engine opened: private memory {:.1} MB before, {:.1} MB after", guard::megabytes(private_start), guard::megabytes(private_open));

    INTERRUPTED.store(false, Ordering::SeqCst);
    unsafe { SetConsoleCtrlHandler(Some(on_console_event), 1) };
    let kinds = [
        ("silence", Part::Silence),
        ("noise -40 dB", Part::Noise { db: -40.0 }),
        ("voice -20 dB", Part::Voice { db: -20.0 }),
        ("voice -20 in noise -35", Part::VoiceInNoise { voice_db: -20.0, noise_db: -35.0 }),
    ];
    let mut block = vec![0.0f32; BLOCK * 2];
    'kinds: for (name, part) in kinds {
        // Ten seconds of the sound, looped: one minute is six passes.
        let chunk = crate::testsignal::render(&[Segment { name, part, seconds: 10.0 }], RATE, 2, 7);
        let started = Instant::now();
        let (private_before, _) = guard::memory();
        for minute in 1..=minutes {
            let cpu = guard::cpu_seconds();
            for _ in 0..6 {
                for samples in chunk.chunks_exact(BLOCK * 2) {
                    if INTERRUPTED.load(Ordering::SeqCst) {
                        break 'kinds;
                    }
                    block.copy_from_slice(samples);
                    engine.process(&mut block);
                }
            }
            let (private, working) = guard::memory();
            println!(
                "{name:<24} {minute:>3} min of sound  private {:>7.1} MB ({:+.2} MB since the start of this sound)  working set {:>7.1} MB  {:.1} s of processor",
                guard::megabytes(private),
                guard::megabytes(private) - guard::megabytes(private_before),
                guard::megabytes(working),
                guard::cpu_seconds() - cpu,
            );
        }
        println!("{name}: {} min of sound in {:.0} s", minutes, started.elapsed().as_secs_f64());
    }
    drop(engine);
    let (private_end, _) = guard::memory();
    println!("engine released: private memory {:.1} MB", guard::megabytes(private_end));
    let registry_after = guard::registry_snapshot(SETTINGS_BRANCH, &work.join(format!("fx-soak-{stamp}-after.reg")))?;
    report_registry(&registry_before, &registry_after, &work, &stamp);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_test_signal_never_clips() {
        for channels in [1, 2] {
            let (_, clipped) = render_counting_clips(&test_segments(), RATE, channels, 1);
            assert_eq!(clipped, 0);
        }
    }
}
