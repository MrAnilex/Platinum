use platinum::devices::find_device;
use platinum::{apo, autostart, config, crash, daemon, devices, fallback, fxprobe, hotkeys, meter, names, relay, routing, session};

use apo::{Apo, Channel, Store, Value};
use devices::{Devices, Endpoint, Flow, Role, State};
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
platinum - small native controller for per-channel audio mixing and effects

settings (channels: game, chat, media, aux, mic):
  platinum status                           overview of the five channels
  platinum list <channel> [filter]          every setting of a channel, with its value and range
  platinum get <channel> <setting>          one setting, by name or number
  platinum set <channel> <setting> <value>  write one setting (applies to live audio at once)

presets (read from GG's own database, read-only; Platinum adds its neutral preset):
  platinum presets <channel> [filter]       the channel's presets in GG's order; * = GG's selected,
                                            F1..F9 = GG's favorites
  platinum preset show <channel> <name>     the settings applying this preset would change now, in
                                            GG's order (writes nothing)
  platinum preset current <channel>         the channel's current settings as a preset, in GG's JSON
  platinum preset apply <channel> <name>    apply the preset (applies to live audio at once)

devices:
  platinum devices                          audio devices, their state and the Windows defaults
  platinum devices --priority               the backup switch's priority lists (platinum.ini), and
                                            what the daemon would choose now; reads only
  platinum enable <device>                  enable a device
  platinum disable <device>                 disable a device
  platinum default <device> [role]          make a device the default (console, multimedia,
                                            communications; all three when no role is given)
  platinum routes <app>                     where an application's sound goes
  platinum route <app> <target>             send an application to a channel (game, chat, media, aux),
                                            to a device, or back to the default device (\"default\")
  platinum sessions [device ...]            which applications have a stream on each device (every
                                            device in use when none is given)
  platinum remote                           remote streaming (Sunshine, Steam Remote Play): the
                                            default output, the session followed, and what the
                                            daemon would do now; reads only
  platinum games                            preset per game (GG's QuickSet): the installed games
                                            GG's presets name, your links, the linked games running,
                                            the preset the daemon would put on Gaming, and GG's
                                            Arena 7 rule on each channel; reads only
  platinum start [channel ...]              enable the virtual channels and make them the defaults
                                            (game, chat, media, aux, mic, stream; all when none given)
  platinum stop                             put the previous defaults back and disable the channels
  platinum names                            the names the virtual channel devices carry now
  platinum rename                           give them Platinum's names (\"Gaming (Platinum)\", ...); the
                                            names in place are kept for --undo. The daemon gives them
                                            the names platinum.ini chooses when it starts
                                            (channel_names = platinum, the default, or steelseries)
  platinum rename --undo                    put the names back as they were before the first rename;
                                            platinum.ini is left as it is

replacing GG's Sonar (GG must be closed):
  platinum daemon [microphone] [--no-effects] [--channels <list>] [--gain <0..1>]
                                            bring the channels up (game,chat,media,aux,mic by default),
                                            relay the microphone through its effects, keep going across
                                            device changes; on Ctrl+C or `daemon --stop`, put the previous
                                            defaults back and disable the channels. Steps aside if GG starts.
                                            Settings come from %USERPROFILE%\\Platinum\\platinum.ini; the
                                            arguments given override them for this run. State published
                                            in status.txt next to it.
      --effects                             use the microphone effects even if the file turns them off
      --fallback-output <device>            make this output the default when the daemon stops
      --fallback-input <device>             make this input the default when the daemon stops
      --no-tray                             no icon next to the clock (it opens the window and can
                                            stop the daemon)
  platinum daemon --stop                    stop the running daemon
  platinum daemon --status                  what the daemon last published (status.txt); reads only
  platinum tray-preview [seconds]           show the icon next to the clock on its own (20 s when not
                                            given); touches no audio device
  platinum shortcuts                        the keyboard shortcuts and overlay options of
                                            platinum.ini (the daemon registers them); reads only
  platinum shortcuts --check [keys ...]     whether each combination (\"Ctrl+Alt+Up\"; those of
                                            platinum.ini when none is given) is free: registers it
                                            for an instant and gives it back at once
  platinum overlay-preview [volume|mute|device|chatmix] [--position <place>] [--seconds <n>]
                                            show a sample overlay (top_left, top_right, center_left,
                                            center_right, bottom_left, bottom_right); writes nothing
                                            and prints the rectangle it takes on the screen
  platinum autostart enable [daemon arguments]
                                            install into %USERPROFILE%\\Platinum and start the daemon
                                            there, without a window, at every logon (output in
                                            daemon.log next to it). Arguments given are written to
                                            platinum.ini; without any, the file is kept as it is.
  platinum autostart disable                no longer start the daemon at logon
  platinum autostart status                 whether Platinum, and GG, start at logon, the
                                            configuration and the daemon's state; reads only
  platinum doctor                           what Platinum needs on this machine (Windows, Sonar
                                            driver and devices, effects engine, VC++ runtime, GG,
                                            presets) and what to do about each problem; reads only

crash reports (written on this PC when a Platinum program crashes; Platinum never sends anything):
  platinum crashes                          the recent crash reports, newest first; reads only
  platinum crashes --show <n>               report n, anonymised as it would be sent from the window
                                            (user, PC, e-mail addresses and audio device names taken
                                            out); reads only

microphone:
  platinum relay <microphone> [options]     copy a real microphone into the virtual microphone until
                                            Ctrl+C; without this copy the virtual microphone is silent.
                                            The sound is passed as it is: no effect is applied.
      --gain <0..1>                         microphone volume (1 when not given)
      --mute                                send silence
      --latency <ms>                        audio held in reserve against drop-outs (30 when not given)
      --seconds <n>                         stop after n seconds
      --report <n>                          print the levels every n seconds
      --to <device>                         copy to this output instead of the virtual microphone
      --effects                             apply the microphone effects set in the vendor's settings,
                                            through its installed effects engine (experimental)
      --check                               show the devices and formats that would be used, open nothing
  platinum meter <device> [...] [--seconds <n>]
                                            sound level of each device, once a second (5 s when not
                                            given). Reading a microphone opens it; nothing is recorded.
  platinum fx-record <microphone> [--seconds <n>] [--save <prefix>]
                                            record the microphone for n seconds (30 when not given),
                                            as captured and after the effects engine, into two WAV
                                            files (platinum-<date>-raw/effects.wav in the current
                                            folder, unless --save gives another path prefix).
                                            Opens the microphone in shared mode; plays nothing.
  platinum fx-soak <microphone> [--minutes <n>]
                                            look for a memory leak in the effects engine: feed it n
                                            minutes (10 when not given) of silence, noise, voice and
                                            voice in noise, as fast as it goes, and print the memory
                                            after each minute of sound. Opens no device; plays nothing.
  platinum fx-probe <microphone> [--channels 1|2] [--verbose] [--save <prefix>] [--fast]
                                            load the installed microphone effects engine as set up for
                                            this microphone, feed it a synthetic test signal and measure
                                            what it does (--save writes it before and after as WAV).
                                            Opens no device itself; reports what the engine opens and
                                            any change to the settings and the engine's registry branch.

A device is given by its identifier or by part of its name; prefix with render: or capture: when
the same name exists as an output and as an input.
relay, meter and fx-record refuse the microphone of a Bluetooth headset, because opening it switches
the headset to hands-free mode and degrades its sound; add --allow-bluetooth to open it anyway.
The audio module's DLL is found at its default install path; set PLATINUM_APO_DLL to override.";

// Identifiers of the settings shown by `status`, in the audio module's numbering.
const RENDER_ENABLED: i32 = 4;
const CAPTURE_ENABLED: i32 = 5;
const OUTPUT_ENABLED: i32 = 163;
const OUTPUT_DEVICE: i32 = 164;
const OUTPUT_VOLUME: i32 = 212;
const OUTPUT_MUTED: i32 = 213;
const CHAT_OUTPUT_ENABLED: i32 = 269;
const CHAT_OUTPUT_DEVICE: i32 = 270;
const CHAT_OUTPUT_VOLUME: i32 = 273;
const CHAT_OUTPUT_MUTED: i32 = 274;
const EQ_ENABLED: i32 = 108;
const CAPTURE_EQ_ENABLED: i32 = 216;
const BASS: (i32, i32) = (44, 45);
const VOICE_CLARITY: (i32, i32) = (165, 166);
const TREBLE: (i32, i32) = (99, 100);
const SMART_VOLUME: (i32, i32) = (6, 7);
const SURROUND_ENABLED: i32 = 79;
const NOISE_GATE: (i32, i32) = (56, 59);
const NOISE_REDUCTION: (i32, i32) = (63, 64);
const AI_NOISE_CANCELLING: (i32, i32) = (185, 186);
const VOLUME_STABILIZER: (i32, i32) = (72, 73);
const ECHO_CANCELLING: i32 = 204;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    install_crash_reports(&args);
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("platinum: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Every run writes a crash report if it crashes (`crash`): the daemon and the microphone worker
/// under their own names, any other command as the command line. A simulated worker (the
/// protocol's test) writes none.
fn install_crash_reports(args: &[String]) {
    let second = args.get(1).map(String::as_str);
    let program = match args.first().map(String::as_str) {
        Some("daemon") if !matches!(second, Some("--stop" | "--status")) => crash::DAEMON,
        Some(platinum::worker::SUBCOMMAND) if args.iter().any(|a| a == "--simulate") => return,
        Some(platinum::worker::SUBCOMMAND) => crash::WORKER,
        _ => crash::CLI,
    };
    crash::install(program);
}

fn run(args: &[String]) -> Result<(), String> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        [] | ["help" | "-h" | "--help"] => {
            println!("{USAGE}");
            Ok(())
        }
        ["status"] => status(&load()?),
        ["list", channel] => list(&load()?, channel, None),
        ["list", channel, filter] => list(&load()?, channel, Some(filter)),
        ["get", channel, setting] => get(&load()?, channel, setting),
        ["set", channel, setting, value] => set(&load()?, channel, setting, value),
        ["presets", channel] => list_presets(channel, None),
        ["presets", channel, filter] => list_presets(channel, Some(filter)),
        ["preset", "show", channel, name] => show_preset(&load()?, channel, name),
        ["preset", "current", channel] => current_preset(&load()?, channel),
        ["preset", "apply", channel, name] => apply_preset(&load()?, channel, name),
        ["devices"] => list_devices(&open_devices()?),
        ["devices", "--priority"] => show_priority(&open_devices()?),
        ["remote"] => show_remote(&open_devices()?),
        ["games"] => show_games(),
        ["enable", device] => set_enabled(&open_devices()?, device, true),
        ["disable", device] => set_enabled(&open_devices()?, device, false),
        ["default", device] => set_default(&open_devices()?, device, &Role::ALL),
        ["default", device, role] => {
            let role = Role::parse(role).ok_or_else(|| format!("unknown role \"{role}\" (expected console, multimedia or communications)"))?;
            set_default(&open_devices()?, device, &[role])
        }
        ["routes", app] => show_routes(&open_devices()?, app),
        ["route", app, target] => route(&open_devices()?, app, target),
        ["start", channels @ ..] => session::start(&open_devices()?, channels),
        ["stop"] => session::stop(&open_devices()?),
        ["names"] => names::show(),
        ["rename"] => names::apply(),
        ["rename", "--undo"] => names::undo(),
        ["sessions", specs @ ..] => sessions(&open_devices()?, specs),
        ["relay", rest @ ..] => relay(&open_devices()?, rest),
        ["meter", rest @ ..] => meter(&open_devices()?, rest),
        ["fx-probe", rest @ ..] => fx_probe(rest),
        ["fx-record", rest @ ..] => fx_record(rest),
        ["fx-soak", rest @ ..] => fx_soak(rest),
        ["daemon", "--stop"] => daemon::request_stop(),
        ["daemon", "--status"] => daemon::print_status(),
        ["tray-preview"] => tray_preview(20),
        ["tray-preview", seconds] => tray_preview(number("tray-preview", seconds, 1.0..=600.0)? as u64),
        ["shortcuts"] => show_shortcuts(),
        ["shortcuts", "--check", keys @ ..] => check_shortcuts(keys),
        ["overlay-preview", rest @ ..] => overlay_preview(rest),
        ["daemon", rest @ ..] => run_daemon(rest),
        // Started by the daemon, not by people: not in the help.
        [platinum::worker::SUBCOMMAND, rest @ ..] => platinum::worker::serve(rest),
        ["autostart", "enable", rest @ ..] => autostart_enable(rest),
        ["autostart", "disable"] => autostart::disable(),
        ["autostart", "status"] | ["autostart"] => autostart::status(),
        ["doctor"] => {
            platinum::doctor::print(&platinum::doctor::run());
            Ok(())
        }
        ["crashes"] => list_crashes(),
        ["crashes", "--show", number] => show_crash(number),
        _ => Err(format!("unrecognised arguments\n\n{USAGE}")),
    }
}

/// `platinum crashes`: the reports in `crashes\`, newest first, numbered for `--show`.
fn list_crashes() -> Result<(), String> {
    let folder = crash::folder()?;
    let reports = crash::list();
    if reports.is_empty() {
        println!("no crash report in {}", folder.display());
    }
    for (index, report) in reports.iter().enumerate() {
        let reason: String = report.reason.chars().take(110).collect();
        println!("{:>2}. {}  {:<14} {reason}", index + 1, crash::readable_stamp(&report.stamp), report.program);
    }
    if !reports.is_empty() {
        println!("\n{} report{} in {} (the {} newest are kept); `platinum crashes --show <n>` prints one as it would be sent", reports.len(), if reports.len() == 1 { "" } else { "s" }, folder.display(), crash::KEEP);
    }
    let consent = config::load().crash_reports;
    println!(
        "crash_reports = {} in platinum.ini: {}",
        consent.key(),
        match consent {
            crash::Consent::Off => "the window does not offer to send them",
            crash::Consent::Ask => "after a crash, the window offers to send the report, anonymised, as a GitHub issue the user reads and sends",
        }
    );
    match crash::repository() {
        Some(repo) => println!("issues go to https://github.com/{repo}/issues, only from the window and only when the user clicks"),
        None => println!("this build's PLATINUM_REPO names no GitHub repository (it is {} or no owner/name): the window cannot open an issue", crash::REPO_PLACEHOLDER),
    }
    Ok(())
}

/// `platinum crashes --show <n>`: report n (from 1, newest first), anonymised as the window shows
/// it before sending. Reads only: the device names come from Windows' list and platinum.ini.
fn show_crash(number: &str) -> Result<(), String> {
    let reports = crash::list();
    let index: usize = number.parse().ok().filter(|n| (1..=reports.len()).contains(n)).ok_or_else(|| format!("no crash report number {number} (`platinum crashes` lists {})", reports.len()))?;
    let report = &reports[index - 1];
    let text = crash::read(&report.file)?;
    let endpoints = Devices::open().and_then(|devices| devices.list()).unwrap_or_default();
    let identity = crash::Identity::of_this_pc(crash::device_names(&endpoints, &config::load()));
    println!("{}", crash::anonymize(&text, &identity).trim_end());
    println!("\n({}, anonymised; the file itself is unchanged)", report.file.display());
    Ok(())
}

/// Lists the applications that have an audio stream on the given devices, or on every device in use.
fn sessions(devices: &Devices, specs: &[&str]) -> Result<(), String> {
    let endpoints = devices.list().map_err(|e| e.to_string())?;
    let chosen: Vec<&Endpoint> = if specs.is_empty() {
        endpoints.iter().filter(|e| e.state == State::Active).collect()
    } else {
        specs.iter().map(|spec| find_device(&endpoints, spec)).collect::<Result<_, _>>()?
    };
    let processes = routing::all_processes().map_err(|e| e.to_string())?;
    for endpoint in chosen {
        let sessions = match devices.sessions(&endpoint.id) {
            Ok(sessions) => sessions,
            Err(error) => {
                println!("{:<7} {}: {error}", endpoint.flow.label(), endpoint.name);
                continue;
            }
        };
        // With no device named, only the devices something is using are shown.
        if sessions.is_empty() && specs.is_empty() {
            continue;
        }
        println!("{:<7} {}", endpoint.flow.label(), endpoint.name);
        for session in sessions {
            let name = match processes.iter().find(|(pid, _)| *pid == session.pid) {
                _ if session.pid == 0 => "(system sounds)",
                Some((_, exe)) => exe.as_str(),
                None => "(process ended)",
            };
            let state = match (session.active, endpoint.flow) {
                (true, Flow::Render) => "playing",
                (true, Flow::Capture) => "recording",
                (false, _) => "idle",
            };
            println!("    {state:<9} {:>6} {name}", session.pid);
        }
    }
    Ok(())
}

/// Splits arguments into positional values and options. `flags` take no value, `valued` take one.
fn split_options<'a>(args: &[&'a str], flags: &[&str], valued: &[&str]) -> Result<(Vec<&'a str>, Vec<(&'a str, &'a str)>), String> {
    let (mut positional, mut options) = (Vec::new(), Vec::new());
    let mut rest = args.iter();
    while let Some(&arg) = rest.next() {
        if flags.contains(&arg) {
            options.push((arg, ""));
        } else if valued.contains(&arg) {
            let value = rest.next().ok_or_else(|| format!("{arg} needs a value"))?;
            options.push((arg, *value));
        } else if arg.starts_with("--") {
            return Err(format!("unknown option {arg}"));
        } else {
            positional.push(arg);
        }
    }
    Ok((positional, options))
}

/// A number given to an option, checked against the values it may take.
fn number(option: &str, text: &str, allowed: std::ops::RangeInclusive<f64>) -> Result<f64, String> {
    match text.parse::<f64>() {
        Ok(value) if allowed.contains(&value) => Ok(value),
        _ => Err(format!("{option} {text}: expected a number from {} to {}", allowed.start(), allowed.end())),
    }
}

/// The endpoints of one direction, so that a name shared by an output and an input is not ambiguous.
fn only(endpoints: &[Endpoint], flow: Flow) -> Vec<Endpoint> {
    endpoints.iter().filter(|e| e.flow == flow).cloned().collect()
}

fn relay(devices: &Devices, args: &[&str]) -> Result<(), String> {
    let (positional, options) = split_options(args, &["--mute", "--check", "--effects", "--allow-bluetooth"], &["--gain", "--latency", "--seconds", "--report", "--to"])?;
    let [source] = positional.as_slice() else {
        return Err("relay needs one microphone, by name or identifier (`platinum devices` lists them)".into());
    };
    let option = |name: &str| options.iter().rev().find(|(option, _)| *option == name).map(|(_, value)| *value);

    let endpoints = devices.list().map_err(|e| e.to_string())?;
    let (inputs, outputs) = (only(&endpoints, Flow::Capture), only(&endpoints, Flow::Render));
    let source = find_device(&inputs, source)?;
    if session::virtual_kind(source).is_some() {
        return Err(format!("{} is a virtual device: the relay needs a real microphone as its source", source.name));
    }
    refuse_hands_free(devices, source, option("--allow-bluetooth").is_some())?;
    let target = match option("--to") {
        Some(spec) => find_device(&outputs, spec)?,
        None => session::microphone_feed(&outputs).ok_or("no virtual microphone on this machine: the audio driver does not seem to be installed")?,
    };
    for endpoint in [source, target] {
        if endpoint.state != State::Active {
            let hint = if session::virtual_kind(endpoint).is_some() { " (`platinum start mic` enables the virtual microphone)" } else { "" };
            return Err(format!("{} is {}{hint}", endpoint.name, endpoint.state.label()));
        }
    }

    let gain = match (option("--mute"), option("--gain")) {
        (Some(_), _) => 0.0,
        (None, Some(text)) => number("--gain", text, 0.0..=1.0)? as f32,
        (None, None) => 1.0,
    };
    let latency = relay::LATENCY_RANGE_MS;
    let config = relay::Config {
        source,
        target,
        gain,
        latency_ms: match option("--latency") {
            Some(text) => number("--latency", text, f64::from(*latency.start())..=f64::from(*latency.end()))? as u32,
            None => relay::DEFAULT_LATENCY_MS,
        },
        seconds: option("--seconds").map(|text| number("--seconds", text, 0.1..=86_400.0)).transpose()?,
        report: option("--report").map(|text| number("--report", text, 0.1..=3_600.0)).transpose()?,
        effects: option("--effects").map(|_| relay::Effects { dll: platinum::sonarfiles::engine_dll().0 }),
        hold: false,
    };
    if option("--check").is_some() { relay::check(devices, &config) } else { relay::run(devices, &config) }
}

fn fx_probe(args: &[&str]) -> Result<(), String> {
    let (positional, options) = split_options(args, &["--verbose", "--fast"], &["--channels", "--dll", "--save"])?;
    let [mic] = positional.as_slice() else {
        return Err("fx-probe needs the real microphone whose effects are tested, by name or identifier; it is not opened".into());
    };
    let option = |name: &str| options.iter().rev().find(|(option, _)| *option == name).map(|(_, value)| *value);
    let channels = match option("--channels") {
        Some("1") => 1,
        Some("2") | None => 2,
        Some(other) => return Err(format!("--channels {other}: expected 1 or 2")),
    };
    let devices = open_devices()?;
    let endpoints = devices.list().map_err(|e| e.to_string())?;
    let inputs = only(&endpoints, Flow::Capture);
    let mic = find_device(&inputs, mic)?;
    if session::virtual_kind(mic).is_some() {
        return Err(format!("{} is a virtual device: name the real microphone", mic.name));
    }
    let settings = load()?;
    fxprobe::run(
        &devices,
        &settings,
        mic,
        &fxprobe::Options {
            dll: engine_dll(option("--dll")),
            channels,
            verbose: option("--verbose").is_some(),
            save: option("--save").map(PathBuf::from),
            fast: option("--fast").is_some(),
        },
    )
}

fn fx_soak(args: &[&str]) -> Result<(), String> {
    let (positional, options) = split_options(args, &[], &["--minutes", "--dll"])?;
    let [mic] = positional.as_slice() else {
        return Err("fx-soak needs the real microphone whose effects are tested, by name or identifier; it is not opened".into());
    };
    let option = |name: &str| options.iter().rev().find(|(option, _)| *option == name).map(|(_, value)| *value);
    let minutes = match option("--minutes") {
        Some(text) => number("--minutes", text, 1.0..=600.0)? as u32,
        None => 10,
    };
    let devices = open_devices()?;
    let endpoints = devices.list().map_err(|e| e.to_string())?;
    let inputs = only(&endpoints, Flow::Capture);
    let mic = find_device(&inputs, mic)?;
    if session::virtual_kind(mic).is_some() {
        return Err(format!("{} is a virtual device: name the real microphone", mic.name));
    }
    fxprobe::soak(&devices, &engine_dll(option("--dll")), mic, minutes)
}

fn fx_record(args: &[&str]) -> Result<(), String> {
    let (positional, options) = split_options(args, &["--allow-bluetooth"], &["--seconds", "--save", "--dll"])?;
    let [mic] = positional.as_slice() else {
        return Err("fx-record needs the real microphone to record, by name or identifier".into());
    };
    let option = |name: &str| options.iter().rev().find(|(option, _)| *option == name).map(|(_, value)| *value);
    let seconds = match option("--seconds") {
        Some(text) => number("--seconds", text, 5.0..=120.0)?,
        None => 30.0,
    };
    let devices = open_devices()?;
    let endpoints = devices.list().map_err(|e| e.to_string())?;
    let inputs = only(&endpoints, Flow::Capture);
    let mic = find_device(&inputs, mic)?;
    if session::virtual_kind(mic).is_some() {
        return Err(format!("{} is a virtual device: name the real microphone", mic.name));
    }
    if mic.state != State::Active {
        return Err(format!("{} is {}", mic.name, mic.state.label()));
    }
    refuse_hands_free(&devices, mic, option("--allow-bluetooth").is_some())?;
    fxprobe::record(&devices, mic, &engine_dll(option("--dll")), seconds, option("--save").map(PathBuf::from))
}

/// Shows the icon next to the clock on its own for a while, without touching any audio device.
fn tray_preview(seconds: u64) -> Result<(), String> {
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    let tray = platinum::tray::show("Platinum (aperçu de l'icône)", move || flag.store(true, std::sync::atomic::Ordering::SeqCst)).ok_or("the icon is already shown")?;
    std::thread::sleep(std::time::Duration::from_millis(800));
    println!("icon {} next to the clock for {seconds} s (its menu's Stop entry ends this preview)", if platinum::tray::shown() { "shown" } else { "NOT accepted by Windows" });
    for _ in 0..seconds * 10 {
        if stop.load(std::sync::atomic::Ordering::SeqCst) {
            println!("stop chosen in the menu");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    drop(tray);
    println!("icon removed");
    Ok(())
}

/// `platinum shortcuts`: what platinum.ini binds, and the overlay options.
fn show_shortcuts() -> Result<(), String> {
    let loaded = config::load_full();
    for problem in &loaded.problems {
        println!("skipped: {problem}");
    }
    let settings = &loaded.config;
    let overlays = settings.overlays;
    println!("overlays    {} at {} (volume {}, mute {}, device {})", if overlays.enabled { "on" } else { "off" }, overlays.position.key(), overlays.volume, overlays.mute, overlays.device);
    if settings.shortcuts.is_empty() {
        println!("no keyboard shortcut is bound");
        return Ok(());
    }
    let conflicts = hotkeys::conflicts(&settings.shortcuts);
    for (action, hotkey) in &settings.shortcuts {
        println!("{:<26} {:<22} {}{}", action.key(), hotkey.to_text(), hotkeys::GROUPS[action.group()], if conflicts.contains(action) { "  (combination used twice)" } else { "" });
    }
    Ok(())
}

/// `platinum shortcuts --check [keys]`: registers each combination for an instant, then gives it
/// back, to tell which ones another program holds.
fn check_shortcuts(keys: &[&str]) -> Result<(), String> {
    let wanted: Vec<hotkeys::Hotkey> = if keys.is_empty() {
        config::load().shortcuts.iter().map(|(_, hotkey)| *hotkey).collect()
    } else {
        keys.iter().map(|key| hotkeys::Hotkey::parse(key).map_err(|problem| format!("{key}: {problem}"))).collect::<Result<_, _>>()?
    };
    if wanted.is_empty() {
        println!("no combination to check (none is bound in platinum.ini)");
    }
    for (hotkey, error) in platinum::shortcuts::check(&wanted) {
        match error {
            None => println!("{:<22} free", hotkey.to_text()),
            Some(1409) => println!("{:<22} taken by another program (or by the running daemon)", hotkey.to_text()),
            Some(code) => println!("{:<22} refused (error {code})", hotkey.to_text()),
        }
    }
    Ok(())
}

/// `platinum overlay-preview`: a sample overlay, as a shortcut would show it. Writes nothing.
fn overlay_preview(args: &[&str]) -> Result<(), String> {
    use platinum::overlay::{self, Content, Glyph, Position};
    let mut content = Content::volume(Glyph::Game, "Gaming", 0.45, false);
    let mut position = Position::TopRight;
    let mut seconds = overlay::HOLD.as_secs_f64();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match *arg {
            "volume" => content = Content::volume(Glyph::Game, "Gaming", 0.45, false),
            "mute" => content = Content::mute(Glyph::Chat, "Chat", true, 0.7),
            "device" => content = Content::device("Haut-parleurs (SteelSeries Arena 7)", true),
            "chatmix" => content = Content::chatmix(0.3, platinum::mixer::chatmix_gains(0.3)),
            "--position" => {
                let text = rest.next().ok_or("--position needs a value")?;
                position = Position::parse(text).ok_or_else(|| format!("unknown position \"{text}\""))?;
            }
            "--seconds" => seconds = number("--seconds", rest.next().ok_or("--seconds needs a value")?, 0.2..=30.0)?,
            other => return Err(format!("unknown overlay-preview argument \"{other}\"")),
        }
    }
    let (x, y, width, height) = overlay::screen_rect(&content, position);
    println!("overlay at x={x} y={y} width={width} height={height} for {seconds} s, then a fade");
    overlay::preview(&content, position, std::time::Duration::from_secs_f64(seconds))
}

/// `platinum daemon [microphone] [options]`: the configuration file, with the arguments over it.
fn run_daemon(args: &[&str]) -> Result<(), String> {
    daemon::run(&daemon::Options::from_arguments(args)?)
}

/// `platinum autostart enable [daemon arguments]`: the arguments are checked as the daemon will,
/// then written to the configuration file.
fn autostart_enable(args: &[&str]) -> Result<(), String> {
    autostart::enable(args)
}

/// The effects engine's DLL: the given path made absolute (the loader needs a full path to find
/// the libraries next to it), or the installed one (`sonarfiles::engine_dll`).
fn engine_dll(given: Option<&str>) -> PathBuf {
    match given {
        Some(path) => std::path::absolute(path).unwrap_or_else(|_| PathBuf::from(path)),
        None => platinum::sonarfiles::engine_dll().0,
    }
}

/// Refuses to open the microphone of a Bluetooth headset unless asked to: opening it switches the
/// headset to its hands-free profile, which degrades everything it plays until it is closed.
fn refuse_hands_free(devices: &Devices, endpoint: &Endpoint, allowed: bool) -> Result<(), String> {
    if endpoint.flow == Flow::Capture && !allowed && devices.is_hands_free(&endpoint.id) {
        return Err(format!(
            "{} is the microphone of a Bluetooth headset: opening it switches the headset to hands-free mode and degrades its sound until it is closed (add --allow-bluetooth to do it anyway)",
            endpoint.name
        ));
    }
    Ok(())
}

fn meter(devices: &Devices, args: &[&str]) -> Result<(), String> {
    let (positional, options) = split_options(args, &["--allow-bluetooth"], &["--seconds"])?;
    if positional.is_empty() {
        return Err("meter needs at least one device, by name or identifier (`platinum devices` lists them)".into());
    }
    let option = |name: &str| options.iter().rev().find(|(option, _)| *option == name).map(|(_, value)| *value);
    let seconds = match option("--seconds") {
        Some(text) => number("--seconds", text, 0.1..=3_600.0)?,
        None => 5.0,
    };
    let endpoints = devices.list().map_err(|e| e.to_string())?;
    let chosen: Vec<&Endpoint> = positional.iter().map(|spec| find_device(&endpoints, spec)).collect::<Result<_, _>>()?;
    for endpoint in &chosen {
        if endpoint.state != State::Active {
            return Err(format!("{} is {}", endpoint.name, endpoint.state.label()));
        }
        refuse_hands_free(devices, endpoint, option("--allow-bluetooth").is_some())?;
    }
    meter::run(devices, &chosen, seconds)
}

fn open_devices() -> Result<Devices, String> {
    Devices::open().map_err(|e| e.to_string())
}

fn show_routes(devices: &Devices, app: &str) -> Result<(), String> {
    let endpoints = devices.list().map_err(|e| e.to_string())?;
    let routing = routing::Routing::open().map_err(|e| e.to_string())?;
    for (pid, exe) in routing::processes(app).map_err(|e| e.to_string())? {
        for flow in Flow::ALL {
            // Windows refuses some helper processes of an application; they have no routing of their own.
            let target = match routing.get(pid, flow) {
                Ok(Some(id)) => endpoints.iter().find(|e| e.id == id).map_or(id, |e| e.name.clone()),
                Ok(None) => "(default device)".into(),
                Err(_) => continue,
            };
            println!("{pid:>6} {exe:<28} {:<7} -> {target}", flow.label());
        }
    }
    Ok(())
}

/// Routes every process of an application to a channel, to a device, or back to the default device.
fn route(devices: &Devices, app: &str, target: &str) -> Result<(), String> {
    let endpoints = devices.list().map_err(|e| e.to_string())?;
    let destination = if target == "default" {
        None
    } else if let Some(endpoint) = session::channel_output(&endpoints, target) {
        Some(endpoint)
    } else {
        Some(find_device(&endpoints, target)?)
    };
    if let Some(endpoint) = destination {
        if endpoint.state != devices::State::Active {
            return Err(format!("{} is {}: enable it first", endpoint.name, endpoint.state.label()));
        }
    }
    let routing = routing::Routing::open().map_err(|e| e.to_string())?;
    let flow = destination.map_or(Flow::Render, |e| e.flow);
    let processes = routing::processes(app).map_err(|e| e.to_string())?;
    // Windows refuses some helper processes of an application; one accepted process is enough.
    let mut accepted = 0;
    let mut last_error = None;
    for (pid, _) in &processes {
        match routing.set(*pid, flow, destination.map(|e| e.id.as_str())) {
            Ok(()) => accepted += 1,
            Err(e) => last_error = Some(e.to_string()),
        }
    }
    if accepted == 0 {
        return Err(last_error.unwrap_or_else(|| "no process accepted the route".into()));
    }
    let name = processes.first().map(|(_, exe)| exe.as_str()).unwrap_or(app);
    println!("{name} ({accepted} of {} processes) {} -> {}", processes.len(), flow.label(), destination.map_or("(default device)", |e| e.name.as_str()));
    Ok(())
}

fn list_devices(devices: &Devices) -> Result<(), String> {
    let endpoints = devices.list().map_err(|e| e.to_string())?;
    let width = endpoints.iter().map(|e| e.name.chars().count()).max().unwrap_or(0);
    for flow in Flow::ALL {
        let mut defaults: Vec<(Role, String)> = Vec::new();
        for role in Role::ALL {
            if let Some(id) = devices.default(flow, role).map_err(|e| e.to_string())? {
                defaults.push((role, id));
            }
        }
        for endpoint in endpoints.iter().filter(|e| e.flow == flow) {
            let roles: Vec<&str> = defaults.iter().filter(|(_, id)| *id == endpoint.id).map(|(role, _)| role.label()).collect();
            let roles = if roles.is_empty() { String::new() } else { format!("default: {}", roles.join(", ")) };
            println!("{:<7} {:<9} {:<width$}  {:<44} {}", flow.label(), endpoint.state.label(), endpoint.name, roles, endpoint.id);
        }
    }
    Ok(())
}

/// `platinum devices --priority`: the backup switch's lists, each entry's state, and what the
/// daemon would do now with the same rules (`fallback`). Reads the channels' routing; writes nothing.
fn show_priority(devices: &Devices) -> Result<(), String> {
    let loaded = config::load_full();
    let settings = &loaded.config;
    let endpoints = devices.list().map_err(|e| e.to_string())?;
    let hands_free: Vec<String> = endpoints.iter().filter(|e| e.state == State::Active && !fallback::is_virtual(e) && devices.is_hands_free(&e.id)).map(|e| e.id.clone()).collect();
    let file = config::path().map(|p| p.display().to_string()).unwrap_or_default();
    println!("backup device switch  {}  (auto_switch in {file})", if settings.auto_switch { "on" } else { "off: the daemon switches nothing by itself" });
    for problem in &loaded.problems {
        println!("  skipped: {problem}");
    }

    println!("\noutputs, first to last (shared by game, chat, media and aux):");
    print_list(&settings.output_priority, &endpoints, Flow::Render, &hands_free);
    let chosen = fallback::choose(&settings.output_priority, &endpoints, Flow::Render, &hands_free);
    println!("  first available: {}", chosen.map_or("none".to_string(), |e| e.name.clone()));
    // Where the channels play now, read from their stores; the same plan the daemon makes.
    match load() {
        Ok(apo) => {
            let routed: Vec<Option<String>> = platinum::outputs::CHANNELS.iter().map(|(channel, _)| apo.open(*channel).ok().and_then(|store| platinum::outputs::routed(&store))).collect();
            let name = |id: &str| endpoints.iter().find(|e| e.id == id).map_or(format!("{id} (absent)"), |e| e.name.clone());
            for ((channel, _), now) in platinum::outputs::CHANNELS.iter().zip(&routed) {
                println!("  {:<6} plays to {}", channel.label(), now.as_deref().map_or("(not routed)".to_string(), name));
            }
            let plan = fallback::plan_outputs(&settings.output_priority, &endpoints, &hands_free, &routed);
            match (plan.target, plan.moves.is_empty()) {
                (_, true) => println!("  the daemon would move no channel"),
                (Some(target), false) => {
                    let moved: Vec<&str> = plan.moves.iter().map(|(index, _)| platinum::outputs::CHANNELS[*index].0.label()).collect();
                    println!("  the daemon would move {} to {}{}", moved.join(", "), target.name, if settings.auto_switch { "" } else { " (if the switch were on)" });
                }
                (None, false) => {}
            }
        }
        Err(error) => println!("  where the channels play cannot be read: {error}"),
    }

    println!("\nmicrophones, behind the one chosen in the Micro column:");
    println!("  chosen: {}", settings.microphone.as_deref().unwrap_or("(none)"));
    print_list(&settings.input_priority, &endpoints, Flow::Capture, &hands_free);
    match fallback::microphone(settings.microphone.as_deref(), &settings.input_priority, settings.auto_switch, &endpoints, &hands_free) {
        Ok(endpoint) => println!("  the daemon would relay {}", endpoint.name),
        Err(error) => println!("  the daemon would relay nothing: {error}"),
    }
    if let Some(published) = platinum::status::read().filter(|s| s.state.is_live() && !s.microphone.is_empty()) {
        println!("  relayed now: {}", published.microphone);
    }
    Ok(())
}

/// `platinum remote`: remote streaming as the daemon sees it (`platinum::remote`): the options, the
/// default output, the session written down, and what the daemon's looks would do now with the
/// same rules. Reads the channels' routing and the applications' routes; writes nothing.
/// `platinum games`: what the preset per game sees now and what the daemon would do, with the
/// daemon's own rules. Reads the registry, Epic's and Steam's manifests, GG's database, the
/// running processes and the channels' settings; writes nothing.
fn show_games() -> Result<(), String> {
    use platinum::games::{self, GameKey, Running, Step};
    use platinum::{gamescan, library::Library, outputs, presetfiles};
    use std::time::Instant;
    let ms = |since: Instant| since.elapsed().as_secs_f64() * 1000.0;
    let loaded = config::load_full();
    let settings = &loaded.config;
    let file = config::path().map(|p| p.display().to_string()).unwrap_or_default();
    println!("preset per game      {}  (game_presets in {file})", if settings.game_presets { "on" } else { "off" });
    println!("arena 7 rule         {}  (arena_speakers)", if settings.arena_speakers { "on" } else { "off" });
    let started = Instant::now();
    let library = Library::open_default();
    let gg = library.as_ref().map(games::gg_links).unwrap_or_default();
    match &library {
        Ok(_) => println!("GG's links           {} Game presets name games (database read in {:.0} ms)", gg.len(), ms(started)),
        Err(error) => println!("GG's links           none: {error}"),
    }
    let user = games::load_links();
    println!("your links           {} ({})", user.len(), games::links_path().map(|p| p.display().to_string()).unwrap_or_default());
    for link in &user {
        println!("  {} ({}) -> {}", link.title, link.exe, link.preset_name);
    }
    let started = Instant::now();
    let installed = gamescan::installed();
    let took = ms(started);
    let linked = games::installed_links(&installed, &gg);
    println!("installed games      {} found from their launchers in {took:.0} ms; {} of GG's presets name one of them (the window's \"Par jeu\"):", installed.len(), linked.len());
    for (key, labels) in &linked {
        let name = gg.iter().find(|l| l.preset == *key).map_or(key.as_str(), |l| l.name.as_str());
        println!("  {name:<40} {}", labels.join(", "));
    }
    let steam = gamescan::steam_running();
    let steam_name = steam.map(|app| gamescan::steam_name(app).unwrap_or_else(|| format!("Steam {app}")));
    println!("steam                {}", match (steam, &steam_name) {
        (Some(app), Some(name)) => format!("{name} runs ({app})"),
        _ => "no application runs (RunningAppID = 0)".to_string(),
    });
    let mut scanner = gamescan::Scanner::default();
    let started = Instant::now();
    let processes = scanner.scan();
    let first = ms(started);
    let started = Instant::now();
    let _ = scanner.scan();
    let next = ms(started);
    println!(
        "processes            {} ({} with a path, {} packaged); first scan {first:.1} ms, the next ones {next:.1} ms",
        processes.len(),
        processes.iter().filter(|p| !p.path.is_empty()).count(),
        processes.iter().filter(|p| !p.family.is_empty()).count()
    );
    let installs: Vec<games::Installed> = gamescan::folder_installs().into_iter().filter(|game| gg.iter().any(|link| link.games.iter().any(|id| game.key.matches(id)))).collect();
    println!("game folders         {} watched for running processes: {}", installs.len(), installs.iter().map(|game| format!("{} ({})", game.name, game.key.source())).collect::<Vec<_>>().join(", "));
    let families = games::store_families(&gg);
    let mut running: Vec<Running> = Vec::new();
    if let (Some(app), Some(name)) = (steam, steam_name) {
        running.push(Running { key: GameKey::Steam(app), name, started: gamescan::now() });
    }
    running.extend(games::running_games(&processes, &installs, &families, &user));
    let running: Vec<Running> = games::once_each(running).into_iter().filter(|game| matches!(game.key, GameKey::Exe(_)) || gg.iter().any(|l| l.games.iter().any(|id| game.key.matches(id)))).collect();
    println!("linked games running {}", if running.is_empty() { "none".to_string() } else { running.len().to_string() });
    for game in &running {
        println!("  {} ({}, {})", game.name, game.key.source(), game.key.text());
    }
    let choice = games::choose(&running, &gg, &user);
    println!("Gaming would use     {}", choice.as_ref().map_or("its own preset (no linked game runs)".to_string(), |c| format!("{} for {} ({})", c.preset_name, c.game, if c.by_user { "your link" } else { "GG's link" })));
    let saved = games::load()?;
    let state = presetfiles::load_state();
    let dismissed = saved.as_ref().is_some_and(|s| state.game_ended == s.id());
    match &saved {
        Some(s) => println!(
            "written down         {} put {} on Gaming since {}; {} comes back when it exits{}",
            s.game,
            s.preset_name,
            s.since,
            if s.restore_name.is_empty() { "the custom settings of before" } else { s.restore_name.as_str() },
            if dismissed { " (ended by a choice in the window: nothing comes back)" } else { "" }
        ),
        None => println!("written down         none"),
    }
    let current = &state.channels[0];
    println!("Gaming's preset      {} (state.json: {})", if current.name.is_empty() { "Personnalisé" } else { current.name.as_str() }, current.selection.key());
    // The daemon's looks with the same rules, as many as it takes it to decide.
    let mut tracker = games::Tracker::default();
    let mut step = Step::Nothing;
    for _ in 0..games::LEAVE_AFTER {
        step = tracker.decide(settings.game_presets, choice.as_ref(), saved.as_ref(), dismissed);
        if step != Step::Nothing {
            break;
        }
    }
    println!(
        "the daemon would     {}",
        match &step {
            Step::Nothing if saved.is_some() => "go on as it is".to_string(),
            Step::Nothing => "do nothing".to_string(),
            Step::Enter(c) => format!("put {} on Gaming for {}", c.preset_name, c.game),
            Step::Switch(c) => format!("switch Gaming to {} for {}", c.preset_name, c.game),
            Step::Leave => "put the preset of before back on Gaming".to_string(),
            Step::Drop => "forget the game's preset, the window's choice staying".to_string(),
        }
    );
    // GG's Arena 7 rule: where each surround channel plays, and what the rule would do there.
    let endpoints = open_devices()?.list().map_err(|e| e.to_string())?;
    let apo = load()?;
    for channel in outputs::SURROUND {
        let store = apo.open(channel).map_err(|e| e.to_string())?;
        let output = outputs::routed(&store);
        let name = output.as_deref().map_or("(not routed)".to_string(), |id| endpoints.iter().find(|e| e.id == id).map_or(format!("{id} (absent)"), |e| e.name.clone()));
        let surround = platinum::preset::read_setting(&store, 79).is_ok_and(|v| v == platinum::preset::Value::Bool(true));
        let speakers = platinum::preset::read_setting(&store, 209).is_ok_and(|v| v == platinum::preset::Value::Number(1.0));
        let arena = outputs::plays_to_arena(&store, &endpoints);
        let rule = match (arena, surround, speakers) {
            (false, ..) => "not the Arena 7",
            (true, false, _) => "Arena 7, spatial audio off: nothing to do",
            (true, true, true) => "Arena 7, spatial audio in speaker mode already",
            (true, true, false) => "Arena 7, spatial audio in headphone mode: the rule would switch it to speakers",
        };
        println!("  {:<6} plays to {name}: {rule}", channel.label());
    }
    Ok(())
}

fn show_remote(devices: &Devices) -> Result<(), String> {
    use platinum::remote::{self, Look, Step};
    let loaded = config::load_full();
    let settings = &loaded.config;
    let endpoints = devices.list().map_err(|e| e.to_string())?;
    let name = |id: &str| endpoints.iter().find(|e| e.id == id).map_or(format!("{id} (absent)"), |e| e.name.clone());
    let file = config::path().map(|p| p.display().to_string()).unwrap_or_default();
    println!("follow remote streaming  {}  (follow_streaming in {file})", if settings.follow_streaming { "on" } else { "off" });
    println!("streaming outputs        {}", if settings.streaming_sinks.is_empty() { "(none)".to_string() } else { settings.streaming_sinks.join(", ") });
    println!("route applications       {}", if settings.streaming_route_apps { "on, to Gaming" } else { "off" });
    let default = devices.default(Flow::Render, Role::Console).map_err(|e| e.to_string())?;
    let sink = default.as_deref().and_then(|id| remote::sink(id, &endpoints, &settings.streaming_sinks));
    println!("default output           {}{}", default.as_deref().map_or("(none)".to_string(), name), if sink.is_some() { "  <- a streaming output" } else { "" });
    let saved = remote::load()?;
    match &saved {
        Some(saved) => println!("session written down     {} since {} ({} application(s) routed by Platinum)", saved.sink_name, saved.since, saved.apps.len()),
        None => println!("session written down     none"),
    }
    // The daemon's looks with the same rules, as many as it takes it to decide.
    let look = sink.map_or(Look::Other, |sink| Look::Sink(sink.id.clone()));
    let mut tracker = remote::Tracker::default();
    let mut step = Step::Nothing;
    for _ in 0..remote::LEAVE_AFTER {
        step = tracker.decide(settings.follow_streaming, &look, saved.as_ref().map(|s| s.sink.as_str()));
        if step != Step::Nothing {
            break;
        }
    }
    println!(
        "the daemon would         {}",
        match step {
            Step::Nothing => "do nothing",
            Step::Enter => "enter a session: move the channels to the streaming output",
            Step::Follow => "follow the session on: route the applications that start playing there",
            Step::Leave => "leave the session: put the channels and the applications back",
        }
    );
    // Whether Gaming plays to the streaming output: the daemon routes applications only then.
    let mut gaming_output: Option<String> = None;
    match load() {
        Ok(apo) => {
            let routed: Vec<Option<String>> = platinum::outputs::CHANNELS.iter().map(|(channel, _)| apo.open(*channel).ok().and_then(|store| platinum::outputs::routed(&store))).collect();
            gaming_output = routed.first().cloned().flatten();
            for ((channel, _), now) in platinum::outputs::CHANNELS.iter().zip(&routed) {
                println!("  {:<6} plays to {}", channel.label(), now.as_deref().map_or("(not routed)".to_string(), name));
            }
            let labels = |strips: &[usize]| strips.iter().map(|s| platinum::outputs::CHANNELS[*s].0.label()).collect::<Vec<_>>().join(", ");
            match (step, sink, &saved) {
                (Step::Enter, Some(sink), _) => {
                    let before: [Option<String>; platinum::mixer::STRIPS] = std::array::from_fn(|strip| routed.get(strip).cloned().flatten());
                    let moved = remote::Saved::new(sink, before, "", "").moved();
                    println!("  it would move {} to {}", if moved.is_empty() { "no channel".to_string() } else { labels(&moved) }, sink.name);
                }
                (Step::Leave, _, Some(saved)) => {
                    for (strip, device) in saved.restore(&routed) {
                        println!("  it would put {} back on {}", labels(&[strip]), name(&device));
                    }
                }
                _ => {}
            }
        }
        Err(error) => println!("  where the channels play cannot be read: {error}"),
    }
    // The applications playing on the streaming output, and what a look would do with each.
    let Some(sink_id) = sink.map(|s| s.id.clone()).or_else(|| saved.as_ref().map(|s| s.sink.clone())) else { return Ok(()) };
    let sessions = devices.sessions(&sink_id).map_err(|e| e.to_string())?;
    let table = routing::all_processes().map_err(|e| e.to_string())?;
    let found: Vec<(u32, String, bool)> = sessions.iter().filter(|s| s.pid != 0).filter_map(|s| table.iter().find(|(pid, _)| *pid == s.pid).map(|(_, exe)| (s.pid, exe.clone(), s.active))).collect();
    let candidates = remote::candidates(&found, std::process::id());
    let routable = if gaming_output.as_deref() == Some(sink_id.as_str()) { "would be routed to Gaming" } else { "would be routed to Gaming once Gaming plays to the streaming output" };
    let routing = routing::Routing::open().map_err(|e| e.to_string())?;
    let empty = remote::Saved::new(&endpoints.iter().find(|e| e.id == sink_id).cloned().unwrap_or(Endpoint { id: sink_id.clone(), name: String::new(), flow: Flow::Render, state: State::Other, virtual_role: None }), Default::default(), "", "");
    let written = saved.as_ref().unwrap_or(&empty);
    println!("applications on {}:", name(&sink_id));
    for (pid, exe, playing) in &found {
        let route = routing.get(*pid, Flow::Render).ok().flatten();
        let verdict = if !candidates.iter().any(|(_, pids)| pids.contains(pid)) {
            if *playing { "never routed (a streaming host, Windows or Platinum)" } else { "idle" }
        } else if written.routed(exe) {
            "routed by Platinum, still here (it opens the device itself)"
        } else if route.is_some() {
            "has a route of its own: left alone"
        } else {
            routable
        };
        println!("  {pid:>6} {exe:<28} {:<8} {verdict}", if *playing { "playing" } else { "idle" });
    }
    if found.is_empty() {
        println!("  (none)");
    }
    Ok(())
}

/// One priority list, numbered, each entry with its state now.
fn print_list(list: &[fallback::PriorityDevice], endpoints: &[Endpoint], flow: Flow, hands_free: &[String]) {
    if list.is_empty() {
        println!("  (empty list)");
    }
    for (index, entry) in list.iter().enumerate() {
        let endpoint = endpoints.iter().find(|e| e.id == entry.id);
        let state = match endpoint {
            None => "absent".to_string(),
            Some(e) if e.flow != flow => "wrong direction".to_string(),
            Some(e) if hands_free.contains(&e.id) => "Bluetooth hands-free, never chosen".to_string(),
            Some(e) => e.state.label().to_string(),
        };
        let name = endpoint.map_or(entry.name.as_str(), |e| e.name.as_str());
        let name = if name.is_empty() { entry.id.as_str() } else { name };
        let excluded = if entry.excluded { ", excluded" } else { "" };
        println!("  {:>2}. {name:<50} {state}{excluded}  {}", index + 1, entry.id);
    }
}

fn set_enabled(devices: &Devices, spec: &str, enabled: bool) -> Result<(), String> {
    let endpoints = devices.list().map_err(|e| e.to_string())?;
    let endpoint = find_device(&endpoints, spec)?;
    devices.set_enabled(&endpoint.id, enabled).map_err(|e| e.to_string())?;
    println!("{} {} {}", if enabled { "enabled" } else { "disabled" }, endpoint.flow.label(), endpoint.name);
    Ok(())
}

fn set_default(devices: &Devices, spec: &str, roles: &[Role]) -> Result<(), String> {
    let endpoints = devices.list().map_err(|e| e.to_string())?;
    let endpoint = find_device(&endpoints, spec)?;
    for role in roles {
        devices.set_default(&endpoint.id, *role).map_err(|e| e.to_string())?;
    }
    let roles: Vec<&str> = roles.iter().map(|r| r.label()).collect();
    println!("default {} {} ({})", endpoint.flow.label(), endpoint.name, roles.join(", "));
    Ok(())
}

fn load() -> Result<Apo, String> {
    let (dll, _) = platinum::sonarfiles::api_dll();
    Apo::load(&dll).map_err(|e| e.to_string())
}

fn open<'a>(apo: &'a Apo, channel: &str) -> Result<Store<'a>, String> {
    let channel = Channel::parse(channel).ok_or_else(|| format!("unknown channel \"{channel}\" (expected game, chat, media, aux or mic)"))?;
    apo.open(channel).map_err(|e| e.to_string())
}

/// A setting given either as a number or as a name.
fn lookup<'a>(store: &Store<'a>, setting: &str) -> Result<apo::Setting<'a>, String> {
    match setting.parse::<i32>() {
        Ok(id) => store.setting(id),
        Err(_) => store.find(setting),
    }
    .map_err(|e| e.to_string())
}

fn read(store: &Store, id: i32) -> String {
    match store.setting(id).and_then(|s| s.value()) {
        Ok(Value::Text(text)) => text,
        Ok(value) => value.to_string(),
        Err(_) => "?".into(),
    }
}

fn is_on(store: &Store, id: i32) -> bool {
    matches!(store.setting(id).and_then(|s| s.value()), Ok(Value::Bool(true)))
}

/// "off", or the amount when the effect is on.
fn effect(store: &Store, (enabled, amount): (i32, i32), unit: &str) -> String {
    if is_on(store, enabled) { format!("{}{unit}", read(store, amount)) } else { "off".into() }
}

fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

fn status(apo: &Apo) -> Result<(), String> {
    // Device names are a nicety: when they cannot be read, the identifiers are shown instead.
    let endpoints = Devices::open().and_then(|devices| devices.list()).unwrap_or_default();
    let output = |store: &Store, enabled: i32, device: i32| {
        if !is_on(store, enabled) {
            return "(not routed)".to_string();
        }
        let id = read(store, device);
        endpoints.iter().find(|e| e.id == id).map_or(id, |e| e.name.clone())
    };
    for channel in Channel::ALL {
        let store = apo.open(channel).map_err(|e| e.to_string())?;
        if channel == Channel::Chat {
            // Chat is an output, but its module keeps processing and routing under the capture names.
            println!(
                "{:<6} processing {:<3}  volume {:<5} {:<5}  eq {:<3}  noise reduction {:<5}  gate {:<7}  stabilizer {:<5}  -> {}",
                channel.label(),
                on_off(is_on(&store, CAPTURE_ENABLED)),
                read(&store, CHAT_OUTPUT_VOLUME),
                if is_on(&store, CHAT_OUTPUT_MUTED) { "muted" } else { "" },
                on_off(is_on(&store, CAPTURE_EQ_ENABLED)),
                effect(&store, NOISE_REDUCTION, ""),
                effect(&store, NOISE_GATE, " dB"),
                effect(&store, VOLUME_STABILIZER, ""),
                output(&store, CHAT_OUTPUT_ENABLED, CHAT_OUTPUT_DEVICE),
            );
        } else if channel.is_capture() {
            println!(
                "{:<6} processing {:<3}  eq {:<3}  ai noise cancelling {:<6}  noise reduction {:<5}  gate {:<7}  stabilizer {:<5}  echo cancelling {}",
                channel.label(),
                on_off(is_on(&store, CAPTURE_ENABLED)),
                on_off(is_on(&store, CAPTURE_EQ_ENABLED)),
                effect(&store, AI_NOISE_CANCELLING, ""),
                effect(&store, NOISE_REDUCTION, ""),
                effect(&store, NOISE_GATE, " dB"),
                effect(&store, VOLUME_STABILIZER, ""),
                on_off(is_on(&store, ECHO_CANCELLING)),
            );
        } else {
            let output = output(&store, OUTPUT_ENABLED, OUTPUT_DEVICE);
            println!(
                "{:<6} processing {:<3}  volume {:<5} {:<5}  eq {:<3}  bass {:<7}  voice {:<7}  treble {:<7}  smart volume {:<5}  surround {:<3}  -> {}",
                channel.label(),
                on_off(is_on(&store, RENDER_ENABLED)),
                read(&store, OUTPUT_VOLUME),
                if is_on(&store, OUTPUT_MUTED) { "muted" } else { "" },
                on_off(is_on(&store, EQ_ENABLED)),
                effect(&store, BASS, " dB"),
                effect(&store, VOICE_CLARITY, " dB"),
                effect(&store, TREBLE, " dB"),
                effect(&store, SMART_VOLUME, ""),
                on_off(is_on(&store, SURROUND_ENABLED)),
                output,
            );
        }
    }
    Ok(())
}

fn list(apo: &Apo, channel: &str, filter: Option<&str>) -> Result<(), String> {
    let store = open(apo, channel)?;
    let filter = filter.map(str::to_ascii_lowercase);
    for index in 0..store.len().map_err(|e| e.to_string())? {
        let setting = store.setting_at(index).map_err(|e| e.to_string())?;
        let name = setting.name().map_err(|e| e.to_string())?;
        if filter.as_ref().is_some_and(|f| !name.to_ascii_lowercase().contains(f)) {
            continue;
        }
        print_setting(&setting, &name)?;
    }
    Ok(())
}

fn print_setting(setting: &apo::Setting, name: &str) -> Result<(), String> {
    let id = setting.id().map_err(|e| e.to_string())?;
    let value = setting.value().map_err(|e| e.to_string())?;
    match setting.range_text().map_err(|e| e.to_string())? {
        Some(range) => println!("{id:>4}  {name:<44} {value:<12} ({range})"),
        None => println!("{id:>4}  {name:<44} {value}"),
    }
    Ok(())
}

fn get(apo: &Apo, channel: &str, setting: &str) -> Result<(), String> {
    let store = open(apo, channel)?;
    let setting = lookup(&store, setting)?;
    let name = setting.name().map_err(|e| e.to_string())?;
    print_setting(&setting, &name)
}

fn set(apo: &Apo, channel: &str, setting: &str, value: &str) -> Result<(), String> {
    let store = open(apo, channel)?;
    let setting = lookup(&store, setting)?;
    let name = setting.name().map_err(|e| e.to_string())?;
    let before = setting.value().map_err(|e| e.to_string())?;
    setting.set_from_text(value).map_err(|e| format!("{name}: {e}"))?;
    // Re-open the setting so the value shown comes from the store, not from this object.
    let id = setting.id().map_err(|e| e.to_string())?;
    let after = store.setting(id).and_then(|s| s.value()).map_err(|e| e.to_string())?;
    println!("{} {name}: {before} -> {after}", store.channel.label());
    Ok(())
}

/// The preset kind of a channel named on the command line.
fn preset_role(channel: &str) -> Result<platinum::preset::Role, String> {
    let parsed = Channel::parse(channel).ok_or_else(|| format!("unknown channel \"{channel}\" (expected game, chat, media, aux or mic)"))?;
    Ok(platinum::preset::Role::from_channel(parsed))
}

/// Where a preset of GG's library comes from, and whether the user changed it in GG.
fn preset_notes(config: &platinum::library::Config) -> String {
    let mut notes = vec![if config.is_preset { "GG library" } else { "GG, made by the user" }.to_string()];
    if config.is_edited() {
        notes.push("edited in GG".into());
    }
    if let Some(version) = &config.release_version {
        notes.push(format!("new in {version}"));
    }
    if !config.game_ids.is_empty() {
        let count = config.game_ids.len();
        notes.push(format!("{count} game id{}", if count > 1 { "s" } else { "" }));
    }
    notes.join(", ")
}

/// `platinum presets <channel> [filter]`: Platinum's neutral preset, then GG's library in GG's
/// order. Reads GG's database only.
fn list_presets(channel: &str, filter: Option<&str>) -> Result<(), String> {
    use platinum::{library::Library, preset};
    let role = preset_role(channel)?;
    let filter = filter.map(str::to_lowercase);
    let wanted = |name: &str| filter.as_ref().is_none_or(|f| name.to_lowercase().contains(f));
    let mut shown = 0;
    if wanted(preset::NEUTRAL_NAME) {
        println!("      {:<46} Platinum's own", preset::NEUTRAL_NAME);
        shown += 1;
    }
    match Library::open_default() {
        Ok(library) => {
            let selected = library.selected(role).map(|c| c.id.clone());
            for config in library.configs(role).into_iter().filter(|c| wanted(&c.name)) {
                let mark = if selected.as_deref() == Some(config.id.as_str()) { "*" } else { " " };
                let favorite = if config.is_favorite { format!("F{}", config.favorite_position + 1) } else { String::new() };
                println!("{mark} {favorite:<3} {:<46} {}", config.name, preset_notes(config));
                shown += 1;
            }
            if !library.skipped.is_empty() {
                println!("({} rows of GG's library could not be read: {})", library.skipped.len(), library.skipped.join("; "));
            }
        }
        Err(error) => println!("({error})"),
    }
    if shown == 0 {
        println!("no {} preset matches", role.api_name());
    }
    Ok(())
}

/// A preset named on the command line: Platinum's neutral one, or one of GG's library by its exact
/// name or by a part of its name that no other preset of the channel has.
fn find_preset(role: platinum::preset::Role, name: &str) -> Result<(String, platinum::preset::Preset), String> {
    use platinum::{library::Library, preset};
    let lower = name.to_lowercase();
    if ["neutre", "neutral", "platinum", preset::NEUTRAL_NAME].iter().any(|n| n.to_lowercase() == lower) {
        return Ok((format!("{} (Platinum's own)", preset::NEUTRAL_NAME), preset::flat(role)));
    }
    let library = Library::open_default().map_err(|e| e.to_string())?;
    let config = match library.find(role, name) {
        Some(config) => config,
        None => match library.search(role, name).as_slice() {
            [one] => *one,
            [] => return Err(format!("no {} preset named \"{name}\" (`platinum presets` lists them)", role.api_name())),
            many => {
                let names: Vec<&str> = many.iter().take(8).map(|c| c.name.as_str()).collect();
                return Err(format!("\"{name}\" matches {} {} presets: {}{}", many.len(), role.api_name(), names.join(", "), if many.len() > 8 { ", ..." } else { "" }));
            }
        },
    };
    let selected = if library.selected(role).is_some_and(|c| c.id == config.id) { ", selected in GG" } else { "" };
    Ok((format!("{} ({}{selected})", config.name, preset_notes(config)), config.data))
}

/// `platinum preset show <channel> <name>`: what applying the preset would write now, without
/// writing. Like GG, only the settings whose value differs from the store's are listed.
fn show_preset(apo: &Apo, channel: &str, name: &str) -> Result<(), String> {
    use platinum::preset;
    let role = preset_role(channel)?;
    let (title, chosen) = find_preset(role, name)?;
    let store = apo.open(role.channel()).map_err(|e| e.to_string())?;
    let all = preset::plan(None, &chosen, role)?;
    let changes = preset::pending(&store, &all)?;
    println!("{title}: {} settings, {} differ from the {} channel now", all.len(), changes.len(), role.channel().label());
    for write in &changes {
        let current = preset::read_setting(&store, write.id).map_or_else(|_| "?".to_string(), |v| v.to_string());
        let setting = store.setting(write.id).and_then(|s| s.name()).unwrap_or_default();
        println!("  {:>4}  {setting:<44} {current:>9} -> {:<9} {}", write.id, write.value.to_string(), write.field);
    }
    // The same question through the store read back as a preset, which is how edits are planned.
    let current = preset::from_store(&store, role)?;
    let through: Vec<i32> = preset::plan(Some(&current), &chosen, role)?.iter().map(|w| w.id).collect();
    let direct: Vec<i32> = changes.iter().map(|w| w.id).collect();
    if through != direct {
        println!("note: against the settings read back as a preset, the changes would be {through:?}");
    }
    Ok(())
}

/// `platinum preset current <channel>`: the channel's settings as a preset, in GG's JSON.
fn current_preset(apo: &Apo, channel: &str) -> Result<(), String> {
    let role = preset_role(channel)?;
    let store = apo.open(role.channel()).map_err(|e| e.to_string())?;
    println!("{}", platinum::preset::from_store(&store, role)?.to_json().to_pretty());
    Ok(())
}

/// `platinum preset apply <channel> <name>`: writes the settings that differ, in GG's order.
fn apply_preset(apo: &Apo, channel: &str, name: &str) -> Result<(), String> {
    use platinum::preset;
    let role = preset_role(channel)?;
    let (title, chosen) = find_preset(role, name)?;
    let store = apo.open(role.channel()).map_err(|e| e.to_string())?;
    let changes = preset::pending(&store, &preset::plan(None, &chosen, role)?)?;
    let written = preset::apply(&store, &changes)?;
    println!("{title}: {written} settings written to the {} channel", role.channel().label());
    Ok(())
}
