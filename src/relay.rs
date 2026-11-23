//! The microphone relay: copies a real microphone into the playback side of the virtual microphone.
//!
//! Applications record from the virtual microphone, and the virtual microphone only carries what is
//! played into its playback side. The vendor's service makes that copy itself, so without a
//! resident process the virtual microphone is silent. This is our own copy: a shared-mode capture
//! stream, a shared-mode render stream, and a small resampler between the two because the devices
//! do not share a clock. By default the sound is passed through as it is, apart from the gain; with
//! `effects`, it first goes through the installed microphone effects engine, block by block.
//!
//! The daemon runs it in a worker process of its own (`worker`), which it replaces when the
//! effects engine has grown (`recycle`): only ending the process gives that memory back. A worker
//! started to take over from another is `hold`: it opens its streams and its engine, runs the
//! microphone through the engine, and sends nothing until `release`, so that the two never feed
//! the virtual microphone together. It publishes the microphone's peak level (`take_peak`) and a
//! count of its wake-ups (`ticks`) for the worker to report.

use crate::com;
use crate::devices::{Devices, Endpoint};
use crate::dsp::{Correction, DriftControl, Framer, Resampler, peak, queue_plan, remap};
use crate::fxprobe::{self, MicEffects};
use crate::wasapi::{AudioClient, CONVERT, EVENT_DRIVEN, Event, Format, prioritize_audio_thread};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Audio kept queued at the destination when nothing else is asked, in milliseconds.
pub const DEFAULT_LATENCY_MS: u32 = 30;
pub const LATENCY_RANGE_MS: std::ops::RangeInclusive<u32> = 20..=150;
/// Room asked for in both stream buffers: enough to ride out a stall without losing audio.
const BUFFER_MS: u32 = 250;

pub struct Config<'a> {
    pub source: &'a Endpoint,
    pub target: &'a Endpoint,
    /// Linear volume applied to the microphone, 0 to 1.
    pub gain: f32,
    pub latency_ms: u32,
    /// Stop after this long; run until interrupted when `None`.
    pub seconds: Option<f64>,
    /// Print levels this often.
    pub report: Option<f64>,
    /// Run the microphone through the installed effects engine before sending it.
    pub effects: Option<Effects>,
    /// Open everything, then send nothing until `release` (a worker taking over from another).
    pub hold: bool,
}

/// Where the effects engine is.
pub struct Effects {
    pub dll: PathBuf,
}

/// Frames the effects engine is given at a time: 10 ms at 48 kHz, one capture period.
const EFFECTS_BLOCK: usize = 480;

/// The effects engine across reopenings of the streams.
#[derive(Default)]
struct EffectsState {
    engine: Option<MicEffects>,
    /// The engine worked at least once in this run: a later failure is survivable.
    worked: bool,
    /// The format it last failed to open for, so that it is not retried every second.
    failed_for: Option<Format>,
}

impl EffectsState {
    /// Says to other threads whether an engine exists now (the daemon keeps its echo references
    /// playing while one does).
    fn publish(&self) {
        ENGINE_ALIVE.store(self.engine.is_some(), Ordering::SeqCst);
    }
}

impl Drop for EffectsState {
    fn drop(&mut self) {
        // The engine goes with the state, when a run ends.
        ENGINE_ALIVE.store(false, Ordering::SeqCst);
    }
}

/// Set by Ctrl+C: the relay then stops cleanly and prints its summary.
static STOP: AtomicBool = AtomicBool::new(false);
/// Set by the daemon's backup switch: the running relay ends cleanly so that the daemon can start
/// it again on another microphone; the process goes on.
static RESTART: AtomicBool = AtomicBool::new(false);
/// Set while something must be put right before the process may end (the daemon's cleanup:
/// default devices back, channels disabled), cleared once it is done.
static CLEANUP_PENDING: AtomicBool = AtomicBool::new(false);
/// What the relay is doing (`Activity` as a number), for a supervisor on another thread.
static ACTIVITY: AtomicU8 = AtomicU8::new(IDLE);
/// The effects engine could not start when the relay began: it relays nothing then.
static EFFECTS_REFUSED: AtomicBool = AtomicBool::new(false);
/// An effects engine exists: its loopback captures of the echo references may be running.
static ENGINE_ALIVE: AtomicBool = AtomicBool::new(false);
/// A held relay (`Config::hold`) may send: set by `release`.
static RELEASED: AtomicBool = AtomicBool::new(false);
/// The microphone's highest peak level since it was last taken, as the bits of an `f32` (for
/// non-negative levels, the order of the bits is the order of the numbers; a NaN sorts above all).
static PEAK: AtomicU32 = AtomicU32::new(0);
/// Wake-ups of the relay's loop: a count that stops moving means its thread is stuck.
static TICKS: AtomicU32 = AtomicU32::new(0);
/// The last problem met by the relay or the effects engine, for the daemon's status.
static LAST_PROBLEM: Mutex<String> = Mutex::new(String::new());

const IDLE: u8 = 0;
const STREAMING_RAW: u8 = 1;
const STREAMING_EFFECTS: u8 = 2;
const INTERRUPTED: u8 = 3;
const READY: u8 = 4;

/// What the relay is doing, as seen from another thread (the daemon publishes it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    /// Not running, or opening the devices for the first time.
    Idle,
    /// Audio flows; `effects` says whether it goes through the effects engine.
    Streaming { effects: bool },
    /// Audio flowed, then a device failed: waiting to reopen the streams.
    Interrupted,
    /// Held (`Config::hold`): the streams and the engine are open, nothing is sent until `release`.
    Ready,
}

pub fn activity() -> Activity {
    match ACTIVITY.load(Ordering::SeqCst) {
        STREAMING_RAW => Activity::Streaming { effects: false },
        STREAMING_EFFECTS => Activity::Streaming { effects: true },
        INTERRUPTED => Activity::Interrupted,
        READY => Activity::Ready,
        _ => Activity::Idle,
    }
}

/// Lets a held relay send (`Config::hold`). Safe to call from any thread.
pub fn release() {
    RELEASED.store(true, Ordering::SeqCst);
}

/// Whether `release` was called.
pub fn released() -> bool {
    RELEASED.load(Ordering::SeqCst)
}

/// The microphone's highest peak level since the last call (0 when nothing was read; exact zero
/// while the relay is muted, whatever the microphone hears).
pub fn take_peak() -> f32 {
    f32::from_bits(PEAK.swap(0, Ordering::SeqCst))
}

/// How many times the relay's loop has woken up so far (wrapping).
pub fn ticks() -> u32 {
    TICKS.load(Ordering::SeqCst)
}

/// The last problem the relay met (device lost, effects engine failed), as technical text.
pub fn last_problem() -> String {
    LAST_PROBLEM.lock().map(|p| p.clone()).unwrap_or_default()
}

/// Whether the last `run` ended because the effects engine it was asked for could not start.
pub fn effects_refused() -> bool {
    EFFECTS_REFUSED.load(Ordering::SeqCst)
}

fn set_problem(problem: &str) {
    if let Ok(mut last) = LAST_PROBLEM.lock() {
        *last = problem.to_string();
    }
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetConsoleCtrlHandler(handler: Option<unsafe extern "system" fn(u32) -> i32>, add: i32) -> i32;
}

const CTRL_CLOSE_EVENT: u32 = 2;
const CTRL_LOGOFF_EVENT: u32 = 5;
const CTRL_SHUTDOWN_EVENT: u32 = 6;
/// How long the console handler holds the process for the cleanup: Windows ends it after 5 s.
const CLEANUP_WAIT: Duration = Duration::from_secs(4);

/// Makes Ctrl+C ask for a clean stop instead of ending the process (`run` does it too).
pub fn install_ctrl_handler() {
    unsafe {
        SetConsoleCtrlHandler(Some(on_console_event), 1);
    }
}

/// Asks a running relay to stop cleanly, as Ctrl+C does. Safe to call from any thread.
pub fn request_stop() {
    STOP.store(true, Ordering::SeqCst);
}

/// Whether a stop was asked for (Ctrl+C or `request_stop`).
pub fn stop_requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

/// Ends the running relay cleanly without stopping anything else: `run` returns, and the caller
/// may start it again with another source. Safe to call from any thread.
pub fn request_restart() {
    RESTART.store(true, Ordering::SeqCst);
}

/// Takes a restart asked for, for a relay that runs in a worker process: the daemon then ends that
/// worker and starts another on the microphone wanted now. True once per request.
pub fn take_restart() -> bool {
    RESTART.swap(false, Ordering::SeqCst)
}

/// Whether an effects engine exists now (opened by a running relay, kept across reopenings of its
/// streams).
pub fn effects_engine_alive() -> bool {
    ENGINE_ALIVE.load(Ordering::SeqCst)
}

/// Says that a cleanup must run before the process may end (`true`), or that it has run.
pub fn set_cleanup_pending(pending: bool) {
    CLEANUP_PENDING.store(pending, Ordering::SeqCst);
}

/// Waits until no cleanup is pending, at most `timeout`. True when the cleanup is done (or none
/// was pending), false on time-out.
pub fn wait_for_cleanup(timeout: Duration) -> bool {
    let started = Instant::now();
    while CLEANUP_PENDING.load(Ordering::SeqCst) {
        if started.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

unsafe extern "system" fn on_console_event(event: u32) -> i32 {
    STOP.store(true, Ordering::SeqCst);
    // Windows ends the process as soon as this returns from these three (Ctrl+C lets it run on):
    // give the cleanup its chance first. Logoff and shutdown only come here without a window;
    // with one, the tray's window gets WM_ENDSESSION instead.
    if matches!(event, CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT) {
        wait_for_cleanup(CLEANUP_WAIT);
    }
    1
}

/// Everything that is decided before a stream is opened.
struct Plan {
    source_mix: Format,
    target_mix: Format,
    /// What the microphone is asked for: its own channels, at the destination's rate.
    capture: Format,
    /// What is sent: the destination's own layout and rate.
    render: Format,
    /// Frames the destination consumes at a time.
    period: usize,
    /// Queue length aimed for right after a write, and the limits beyond which it is forced back.
    target: usize,
    low: usize,
    high: usize,
}

impl Plan {
    fn new(devices: &Devices, source: &AudioClient, target: &AudioClient, latency_ms: u32) -> com::Result<Plan> {
        let source_mix = source.mix_format(devices.ole())?;
        let target_mix = target.mix_format(devices.ole())?;
        let rate = target_mix.rate;
        let period = target.period_frames(rate)?.max(1);
        let queue = ((u64::from(rate) * u64::from(latency_ms) / 1000) as usize).max(2 * period);
        Ok(Plan {
            source_mix,
            target_mix,
            capture: source_mix.as_float_at(rate),
            render: target_mix.as_float_at(rate),
            period,
            target: queue,
            // Under one period queued, the destination's next pull cannot be served: a drop-out
            // is certain, so the queue is refilled. Refilling any earlier would cause the very
            // gap it is meant to avoid.
            low: period,
            high: queue + 4 * period,
        })
    }

    fn milliseconds(&self, frames: usize) -> f64 {
        frames as f64 * 1000.0 / f64::from(self.render.rate)
    }
}

/// Shows what the relay would do, without opening any stream.
pub fn check(devices: &Devices, config: &Config) -> Result<(), String> {
    let text = |e: com::Error| e.to_string();
    let source = AudioClient::activate(devices, &config.source.id).map_err(text)?;
    let target = AudioClient::activate(devices, &config.target.id).map_err(text)?;
    let mut plan = Plan::new(devices, &source, &target, config.latency_ms).map_err(text)?;
    if config.effects.is_some() {
        plan.target += EFFECTS_BLOCK;
        plan.high += EFFECTS_BLOCK;
    }
    println!("source       {}: {}", config.source.name, plan.source_mix);
    println!("destination  {}: {}", config.target.name, plan.target_mix);
    println!("captured as  {}", plan.capture);
    println!("sent as      {}", plan.render);
    println!("gain         {}", config.gain);
    println!("effects      {}", if config.effects.is_some() { "through the installed effects engine (not loaded by --check)" } else { "none" });
    println!(
        "queue        aim {} frames ({:.1} ms), refill below {}, drop above {}; the destination takes {} frames ({:.1} ms) at a time",
        plan.target,
        plan.milliseconds(plan.target),
        plan.low,
        plan.high,
        plan.period,
        plan.milliseconds(plan.period),
    );
    Ok(())
}

/// Peak levels before and after the relay, one reading per wake-up (about 10 ms of audio).
#[derive(Default)]
struct Levels {
    readings: u32,
    in_sum: f64,
    out_sum: f64,
    in_max: f32,
    out_max: f32,
}

impl Levels {
    fn add(&mut self, input: f32, output: f32) {
        self.readings += 1;
        self.in_sum += f64::from(input);
        self.out_sum += f64::from(output);
        self.in_max = self.in_max.max(input);
        self.out_max = self.out_max.max(output);
    }

    fn mean_in(&self) -> f64 {
        self.in_sum / f64::from(self.readings.max(1))
    }

    fn mean_out(&self) -> f64 {
        self.out_sum / f64::from(self.readings.max(1))
    }
}

#[derive(Default)]
struct Totals {
    levels: Levels,
    frames_in: u64,
    frames_out: u64,
    rate: u32,
    /// Times the queue ran low after start-up and was refilled with silence: each one is a drop-out.
    refills: u32,
    /// Times frames were discarded because too much was queued, and how many frames in all.
    drops: u32,
    dropped_frames: u64,
    /// Times the streams had to be reopened (device unplugged, disabled, format changed).
    restarts: u32,
    correction_ppm: f64,
}

/// Runs the relay until the time is up or Ctrl+C. A failure to open the devices at the start is an
/// error; once audio has flowed, later failures are waited out and the streams reopened.
pub fn run(devices: &Devices, config: &Config) -> Result<(), String> {
    unsafe {
        SetConsoleCtrlHandler(Some(on_console_event), 1);
    }
    prioritize_audio_thread();
    let started = Instant::now();
    let deadline = config.seconds.map(|seconds| started + Duration::from_secs_f64(seconds));
    // A restart asked before this run began was meant for the previous one.
    RESTART.store(false, Ordering::SeqCst);
    let finished = || STOP.load(Ordering::SeqCst) || RESTART.load(Ordering::SeqCst) || deadline.is_some_and(|deadline| Instant::now() >= deadline);

    let mut totals = Totals::default();
    let mut has_streamed = false;
    let mut last_error = String::new();
    // The effects engine outlives a reopening of the streams, unless the format changes.
    let mut engine = EffectsState::default();
    ACTIVITY.store(IDLE, Ordering::SeqCst);
    EFFECTS_REFUSED.store(false, Ordering::SeqCst);
    while !finished() {
        let mut streaming = false;
        let result = stream(devices, config, started, &finished, &mut totals, &mut streaming, &mut engine);
        if streaming {
            has_streamed = true;
            last_error.clear();
        }
        let Err(error) = result else { break };
        if !has_streamed {
            ACTIVITY.store(IDLE, Ordering::SeqCst);
            set_problem(&error.to_string());
            return Err(error.to_string());
        }
        ACTIVITY.store(INTERRUPTED, Ordering::SeqCst);
        totals.restarts += 1;
        let error = error.to_string();
        set_problem(&error);
        if error != last_error {
            eprintln!("platinum: relay interrupted ({error}); waiting for the devices");
            last_error = error;
        }
        for _ in 0..10 {
            if finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    ACTIVITY.store(IDLE, Ordering::SeqCst);
    RESTART.store(false, Ordering::SeqCst);

    let rate = f64::from(totals.rate.max(1));
    println!("relayed {:.1} s of audio: {} -> {}", totals.frames_in as f64 / rate, config.source.name, config.target.name);
    println!(
        "mean level in {:.4}, out {:.4} (highest {:.4}, {:.4}); {} frames in, {} out",
        totals.levels.mean_in(),
        totals.levels.mean_out(),
        totals.levels.in_max,
        totals.levels.out_max,
        totals.frames_in,
        totals.frames_out,
    );
    println!(
        "queue refilled {} time(s), {} frame(s) dropped in {} time(s), streams reopened {} time(s), last clock correction {:+.0} ppm",
        totals.refills, totals.dropped_frames, totals.drops, totals.restarts, totals.correction_ppm,
    );
    Ok(())
}

/// Opens both streams and copies audio until `finished` says so or a device fails.
fn stream(devices: &Devices, config: &Config, started: Instant, finished: &dyn Fn() -> bool, totals: &mut Totals, streaming: &mut bool, effects_state: &mut EffectsState) -> com::Result<()> {
    // Created first so that it outlives the stream that signals it.
    let event = Event::new()?;
    let source = AudioClient::activate(devices, &config.source.id)?;
    let target = AudioClient::activate(devices, &config.target.id)?;
    let mut plan = Plan::new(devices, &source, &target, config.latency_ms)?;
    if let Some(effects) = &config.effects {
        // The engine works on the microphone as captured: its channels, at the destination's rate.
        let current = effects_state.engine.as_ref().map(MicEffects::format);
        if current != Some(plan.capture) && effects_state.failed_for != Some(plan.capture) {
            effects_state.engine = None;
            match fxprobe::open_engine(devices, &effects.dll, config.source, plan.capture, EFFECTS_BLOCK) {
                Ok(opened) => {
                    println!("effects engine ready: {}, {EFFECTS_BLOCK}-frame blocks", plan.capture);
                    effects_state.engine = Some(opened);
                    effects_state.worked = true;
                    effects_state.failed_for = None;
                }
                // Asked for at the start and not available: say so instead of relaying raw audio
                // (the daemon may then choose to relay without effects).
                Err(error) if !effects_state.worked => {
                    EFFECTS_REFUSED.store(true, Ordering::SeqCst);
                    return Err(com::Error::Invalid(error));
                }
                // Lost later (device change): raw audio is better than silence.
                Err(error) => {
                    eprintln!("platinum: effects engine failed ({error}); relaying without effects");
                    set_problem(&format!("effects engine failed ({error})"));
                    effects_state.failed_for = Some(plan.capture);
                }
            }
        }
    }
    effects_state.publish();
    if effects_state.engine.is_some() {
        // Captured audio can wait up to one block for the engine: keep that much more in reserve.
        plan.target += EFFECTS_BLOCK;
        plan.high += EFFECTS_BLOCK;
    }
    let engine = &mut effects_state.engine;
    source.initialize(EVENT_DRIVEN | CONVERT, BUFFER_MS, &plan.capture)?;
    target.initialize(CONVERT, BUFFER_MS, &plan.render)?;
    source.set_event(&event)?;
    let capture = source.capture_client()?;
    let render = target.render_client()?;
    let capacity = target.buffer_frames()?;
    let streaming_now = if engine.is_some() { STREAMING_EFFECTS } else { STREAMING_RAW };
    // Held: the destination stays stopped and empty until the relay is released; the microphone
    // runs through the engine meanwhile, so that it is warm when it takes over.
    let mut sending = !config.hold || RELEASED.load(Ordering::SeqCst);
    if sending {
        target.start()?;
    }
    source.start()?;
    if sending {
        *streaming = true;
    }
    ACTIVITY.store(if sending { streaming_now } else { READY }, Ordering::SeqCst);
    totals.rate = plan.render.rate;

    let (cin, cout) = (usize::from(plan.capture.channels), usize::from(plan.render.channels));
    let mut resampler = Resampler::new(cout);
    let mut control = DriftControl::new(plan.render.rate, plan.target);
    // `mapped`: captured audio in the destination's layout; `pending`: the same after the resampler.
    let (mut mapped, mut pending) = (Vec::<f32>::new(), Vec::<f32>::new());
    let (mut ratio, mut primed) = (1.0f64, false);
    let mut window = Levels::default();
    let mut next_report = config.report.map(|seconds| Instant::now() + Duration::from_secs_f64(seconds));
    // With effects, captured audio waits in `framer` until a whole block can go through the engine.
    let mut framer = engine.as_ref().map(|_| Framer::new(cin, EFFECTS_BLOCK));
    let mut block = vec![0.0f32; EFFECTS_BLOCK * cin];

    while !finished() {
        event.wait(100);
        TICKS.fetch_add(1, Ordering::SeqCst);
        if !sending && RELEASED.load(Ordering::SeqCst) {
            // Released: the first write below fills the destination's queue up to its target.
            target.start()?;
            sending = true;
            *streaming = true;
            ACTIVITY.store(streaming_now, Ordering::SeqCst);
        }

        let (mut frames_in, mut in_peak) = (0usize, 0.0f32);
        while capture.read(cin, |samples, frames| {
            frames_in += frames;
            match samples {
                // A muted relay sends exact silence whatever the microphone delivers.
                Some(samples) if config.gain > 0.0 => {
                    in_peak = in_peak.max(peak(samples));
                    match framer.as_mut() {
                        Some(framer) => framer.push(samples),
                        None => remap(samples, cin, cout, config.gain, &mut mapped),
                    }
                }
                Some(samples) => {
                    in_peak = in_peak.max(peak(samples));
                    mapped.resize(mapped.len() + frames * cout, 0.0);
                }
                None => match framer.as_mut() {
                    Some(framer) => framer.push_silence(frames),
                    None => mapped.resize(mapped.len() + frames * cout, 0.0),
                },
            }
        })? {}
        if frames_in == 0 {
            continue;
        }
        if let (Some(framer), Some(engine)) = (framer.as_mut(), engine.as_mut()) {
            framer.drain(|samples| {
                block.copy_from_slice(samples);
                engine.process(&mut block);
                remap(&block, cin, cout, config.gain, &mut mapped);
            });
        }
        // A muted relay sends silence whatever the microphone does: its level is zero.
        let level = if config.gain > 0.0 { in_peak } else { 0.0 };
        // Without its sign the order of the bits is the order of the levels; not a number is loud.
        let level = if level.is_nan() { f32::INFINITY } else { level.abs() };
        PEAK.fetch_max(level.to_bits(), Ordering::SeqCst);
        if !sending {
            mapped.clear();
            continue;
        }

        resampler.process(&mapped, ratio, &mut pending);
        mapped.clear();
        // Nothing outside the normal range ever reaches the destination.
        for sample in &mut pending {
            *sample = if sample.is_finite() { sample.clamp(-1.0, 1.0) } else { 0.0 };
        }
        let out_peak = peak(&pending);
        let mut frames_out = pending.len() / cout;
        let mut skipped = 0;
        let mut queued = target.queued_frames()?;
        // The very first write always fills the queue up to the target.
        match queue_plan(queued, frames_out, plan.target, if primed { plan.low } else { plan.target }, plan.high) {
            Correction::None => {}
            Correction::Prime(silence) => {
                let silence = silence.min(capacity.saturating_sub(queued));
                render.write_silence(silence)?;
                queued += silence;
                if primed {
                    totals.refills += 1;
                }
                control.rebase();
            }
            Correction::Drop(frames) => {
                skipped = frames;
                frames_out -= frames;
                totals.drops += 1;
                totals.dropped_frames += frames as u64;
                control.rebase();
            }
        }
        primed = true;
        let fit = frames_out.min(capacity.saturating_sub(queued));
        totals.dropped_frames += (frames_out - fit) as u64;
        render.write(&pending[skipped * cout..(skipped + fit) * cout], cout)?;
        pending.clear();
        ratio = control.update((queued + fit) as f64, frames_in);

        totals.frames_in += frames_in as u64;
        totals.frames_out += fit as u64;
        totals.correction_ppm = control.correction_ppm();
        totals.levels.add(in_peak, out_peak);
        window.add(in_peak, out_peak);
        if let (Some(at), Some(every)) = (next_report, config.report) {
            if Instant::now() >= at {
                println!(
                    "[{:>4.0} s] in {:.4}  out {:.4}  queue {:>5.1} ms  correction {:+5.0} ppm  refills {}  drops {}",
                    started.elapsed().as_secs_f64(),
                    window.mean_in(),
                    window.mean_out(),
                    plan.milliseconds(queued + fit),
                    control.correction_ppm(),
                    totals.refills,
                    totals.drops,
                );
                window = Levels::default();
                next_report = Some(at + Duration::from_secs_f64(every));
            }
        }
    }
    // The streams close when their clients are released; stopping first avoids a click.
    let _ = source.stop();
    let _ = target.stop();
    Ok(())
}
