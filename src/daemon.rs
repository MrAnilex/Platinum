//! `platinum daemon`: everything GG's Sonar does for the sound, in one long-running process.
//!
//! It enables the virtual channels and makes them the defaults, relays the real microphone into
//! the virtual one (through the vendor's effects engine unless told otherwise), keeps going across
//! device changes, and on the way out puts the previous defaults back and disables the channels.
//!
//! Its settings come from Platinum's configuration file (`config`); arguments on its command line
//! override them for one run. It publishes what it is doing in status.txt (`status`), for the
//! window and the tray icon.
//!
//! It never fights GG: it refuses to start while GG's Sonar runs, and if Sonar comes back while it
//! runs, it steps aside without touching the devices, which Sonar then manages again.
//! `platinum daemon --stop` asks a running daemon to stop, from any console.
//!
//! With the backup device switch on (`auto_switch` in the configuration, see `fallback`), a thread
//! of its own looks at the devices every 2 s: the routed output channels follow the output priority
//! list, and the microphone relay moves to a backup microphone while the chosen one is missing, and
//! back when it returns. Every switch is logged and published in status.txt.
//!
//! The keyboard shortcuts (`shortcuts`) are carried out by that same thread, which owns the
//! channels' settings stores, and their overlays are shown by the shortcuts thread. At start, the
//! channels get the names platinum.ini chooses, Platinum's by default, put back where Windows reset
//! them (`names::apply_at_start`).
//!
//! A microphone worker that ends with a crash code leaves a crash report (`crash`): what the daemon
//! saw is added to the worker's own report when it wrote one, else recorded as one.
//!
//! The microphone relay and the vendor's effects engine run in a worker process (`worker`) that the
//! daemon starts, follows and replaces (`MicHost`): the engine gathers memory in its DLL's global
//! state, which only ending the process gives back. Once the worker's private memory passes
//! `engine_recycle_mb` and it has run an hour, a new worker gets ready and takes over during a
//! silence of the microphone (`recycle`); a worker that ends on its own is started again at once.
//! Should no worker start at all, the daemon relays the microphone itself as it used to, and its
//! log says so. At start and after each replacement, the "stream added" and "stream removed"
//! events the engine leaves in the registry are deleted (`regclean`).
//!
//! The watcher thread also writes the daemon's memory in its log, with the worker's: at start,
//! every hour, and the first time the daemon's private memory crosses 100, 200 and 400 MB
//! (`MemoryWatch`).
//!
//! While an effects engine runs, the mixer thread keeps a silent stream open on each output its
//! echo cancellation listens to (`keepalive`), as GG's service does: the engine's loopback captures
//! then always get audio, and the engine no longer grows while those outputs play nothing.
//!
//! In streamer mode (`streamer`), the same mixer thread keeps the Stream virtual device enabled
//! while the configuration asks for the `stream` channel, and disables it again once it does not.
//!
//! It also follows remote streaming sessions (`remote`): while Sunshine or Steam Remote Play holds
//! the default output, the channels play to that output and the applications on it go through
//! Gaming; the backup switch and the device shortcuts leave the outputs alone meanwhile. Everything
//! goes back when the session ends, when the daemon stops, or at the next start after a crash.
//!
//! It also does GG's QuickSet (`games`): while a game linked to a Game preset runs, Gaming uses
//! that preset, and the preset of before comes back once the game exits, from the same 2 s tick
//! and with the stores it keeps. What it changed is written down first; an override still under
//! way when the daemon stops stays written down, and the next start goes on with it or puts the
//! preset of before back. GG's Arena 7 rule runs whenever the daemon applies a preset or moves an
//! output to Arena 7 speakers (`outputs::arena_rule`).

use crate::apo::{Channel, Store, Stores};
use crate::com::wide;
use crate::config::{self, Arguments, Config};
use crate::devices::{Devices, Endpoint, Flow, NameField, Role, State};
use crate::games::{self, Choice};
use crate::hotkeys::Action;
use crate::i18n::{self, mark, tr};
use crate::library::{self, Library};
use crate::mixer::STRIPS;
use crate::overlay::{Content, Glyph, Kind};
use crate::preset::{self, Preset};
use crate::presetfiles::{self, OwnPreset, PresetRef};
use crate::remote::{self, Look, Step};
use crate::routing::Routing;
use crate::shortcuts::{self, Overlays, Shortcuts};
use crate::status::{self, State as Phase, Status};
use crate::streamer;
use crate::keepalive::{KeepAlive, StoreClock};
use crate::recycle::{Decision, Moment, Replacement, Restarts};
use crate::worker::{self, Command, Event, Job, Report, Worker};
use crate::{crash, fallback, gamescan, keepalive, names, outputs, recycle, regclean, relay, routing, session};
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

/// Process of GG's Sonar service: while it runs, it owns the virtual devices.
const SONAR_PROCESS: &str = "SteelSeriesSonar.exe";
const INSTANCE_NAME: &str = "Local\\platinum-daemon";
const STOP_EVENT_NAME: &str = "Local\\platinum-daemon-stop";
/// The channels brought up by default: the stream mix needs GG's stream redirection, not done yet.
pub const DEFAULT_CHANNELS: [&str; 5] = ["game", "chat", "media", "aux", "mic"];
/// Status message while no microphone is chosen anywhere.
const NO_MICROPHONE: &str = mark("Aucun micro choisi : le micro virtuel reste muet");

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateMutexW(security: *mut c_void, initial_owner: i32, name: *const u16) -> *mut c_void;
    fn CreateEventW(security: *mut c_void, manual_reset: i32, initial_state: i32, name: *const u16) -> *mut c_void;
    fn OpenEventW(access: u32, inherit: i32, name: *const u16) -> *mut c_void;
    fn OpenMutexW(access: u32, inherit: i32, name: *const u16) -> *mut c_void;
    fn SetEvent(event: *mut c_void) -> i32;
    fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn GetLastError() -> u32;
}

const ERROR_ALREADY_EXISTS: u32 = 183;
const EVENT_MODIFY_STATE: u32 = 0x0002;
const WAIT_OBJECT_0: u32 = 0;

pub struct Options {
    /// Name or identifier of the real microphone. `None`: none chosen; the configuration is read
    /// again before every attempt, so that one chosen meanwhile is picked up.
    pub microphone: Option<String>,
    pub effects: bool,
    pub channels: Vec<String>,
    pub gain: f32,
    /// Devices to make the defaults when the daemon stops, instead of those it found at start
    /// (which, right after GG, are whatever Windows picked when Sonar went away).
    pub fallback_output: Option<String>,
    pub fallback_input: Option<String>,
    /// Show Platinum's icon next to the clock while the daemon runs.
    pub tray: bool,
    /// The channels come from the command line (`--channels`): the configuration's mode and
    /// channels no longer decide whether the Stream virtual device is enabled.
    pub channels_overridden: bool,
    /// Private memory in MB past which the microphone worker is replaced (0: never).
    pub engine_recycle_mb: u32,
}

impl Options {
    /// The configuration, with the daemon's command-line arguments over it
    /// (`[microphone] [--no-effects] [--channels <list>] …`, see `config::Arguments`).
    pub fn from_arguments(args: &[&str]) -> Result<Options, String> {
        let arguments = Arguments::parse(args)?;
        let mut config = config::load();
        config.apply(&arguments);
        Ok(Options {
            microphone: config.microphone,
            effects: config.effects,
            channels: config.channels,
            gain: config.gain,
            fallback_output: config.fallback_output,
            fallback_input: config.fallback_input,
            tray: !arguments.no_tray,
            channels_overridden: arguments.channels.is_some(),
            engine_recycle_mb: config.engine_recycle_mb,
        })
    }

    /// Whether the Stream virtual device should be enabled, given the configuration as it is now.
    fn wants_stream(&self, settings: &Config) -> bool {
        if self.channels_overridden { streamer::has_stream_channel(&self.channels) } else { streamer::wants_stream_channel(settings.mode, &settings.channels) }
    }
}

fn log(message: impl AsRef<str>) {
    println!("[{}] {}", crate::guard::stamp(), message.as_ref());
}

/// Whether GG's Sonar service runs (it then owns the virtual devices).
pub fn sonar_running() -> bool {
    routing::processes(SONAR_PROCESS).is_ok()
}

/// Whether a daemon runs in this session: its single-instance mutex exists.
pub fn is_running() -> bool {
    const SYNCHRONIZE: u32 = 0x0010_0000;
    unsafe {
        let mutex = OpenMutexW(SYNCHRONIZE, 0, wide(INSTANCE_NAME).as_ptr());
        if mutex.is_null() {
            return false;
        }
        CloseHandle(mutex);
        true
    }
}

/// `platinum daemon --stop`: asks the running daemon to stop. It then puts the default devices
/// back and exits; `wait_stopped` waits for that.
pub fn request_stop() -> Result<(), String> {
    unsafe {
        let event = OpenEventW(EVENT_MODIFY_STATE, 0, wide(STOP_EVENT_NAME).as_ptr());
        if event.is_null() {
            return Err("no platinum daemon is running".into());
        }
        SetEvent(event);
        CloseHandle(event);
    }
    println!("stop requested; the daemon puts the previous default devices back and exits");
    Ok(())
}

/// Waits until no daemon runs, at most `timeout`; true when none does.
pub fn wait_stopped(timeout: Duration) -> bool {
    let started = Instant::now();
    while is_running() {
        if started.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

/// Starts the daemon in the background the way Windows does at logon: `platinum-start.exe daemon`,
/// found next to the running program, else in the install folder. The daemon reads the
/// configuration file. Returns once the launcher runs, with the launcher's path (it logs next to
/// itself): `status::read` then tells what the daemon does (it refuses to start while GG's Sonar
/// runs or another daemon runs). Error in French.
pub fn launch() -> Result<PathBuf, String> {
    let here = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|folder| folder.join("platinum-start.exe")));
    let installed = crate::autostart::install_dir().ok().map(|folder| folder.join("platinum-start.exe"));
    let launcher = [here, installed].into_iter().flatten().find(|path| path.exists()).ok_or("platinum-start.exe est introuvable (ni à côté de ce programme, ni dans le dossier d'installation)")?;
    let mut command = std::process::Command::new(&launcher);
    command.arg("daemon");
    if let Some(folder) = launcher.parent() {
        command.current_dir(folder);
    }
    command.spawn().map_err(|e| format!("impossible de lancer {} : {e}", launcher.display()))?;
    Ok(launcher)
}

/// `platinum daemon --status`: what the daemon last published, and whether it still runs.
pub fn print_status() -> Result<(), String> {
    let file = status::path()?;
    match status::read() {
        None => println!("no status published yet ({} does not exist): no daemon of this version has run", file.display()),
        Some(published) => {
            println!("state       {} ({}){}", published.state.key(), published.state.label(), if published.vanished { ", its process is gone without cleaning up" } else { "" });
            println!("summary     {}", published.summary());
            println!("pid         {}", published.pid);
            println!("microphone  {}", published.microphone);
            println!("effects     {}", published.effects);
            println!("message     {}", published.message);
            println!("remote      {}", if published.remote.is_empty() { "no remote streaming session followed".to_string() } else { format!("following the streaming output {}", published.remote) });
            println!("mic worker  {}", worker_text(published.state.is_live(), published.worker));
            println!("replaced    {}", if published.replaced.is_empty() { "no replacement of the mic worker yet" } else { published.replaced.as_str() });
            println!("updated     {}", published.updated);
        }
    }
    println!("instance    {}", if is_running() { "a daemon runs in this session" } else { "no daemon runs in this session" });
    println!("file        {}", file.display());
    Ok(())
}

/// The microphone worker as `daemon --status` and `doctor` say it: its process and memory, read
/// from here (`guard::process_memory`), or why there is none.
pub fn worker_text(daemon_live: bool, pid: u32) -> String {
    if !daemon_live {
        return "none (the daemon does not run)".into();
    }
    if pid == 0 {
        return "none (no microphone relayed, or relayed inside the daemon)".into();
    }
    match crate::guard::process_memory(pid) {
        Some((private, working_set)) => format!("pid {pid}, private {:.1} MB, working set {:.1} MB", crate::guard::megabytes(private), crate::guard::megabytes(working_set)),
        None => format!("pid {pid} (its memory cannot be read: it may have just ended)"),
    }
}

/// The daemon's state as published in status.txt: changed by the main thread at each step and by
/// the watcher while the relay runs, written whenever it changes.
struct Reporter {
    status: Mutex<Status>,
    /// The effects asked for in this run.
    effects_wanted: bool,
    /// The relay runs inside the daemon (no worker could start): its activity decides between
    /// running, degraded and waiting.
    relaying: AtomicBool,
    /// The microphone worker relaying now (0: none), for the memory lines.
    worker_pid: AtomicU32,
    /// An effects engine runs in a worker (the one relaying, or one getting ready to take over).
    worker_engine: AtomicBool,
    /// The file could not be written once: said once in the log, then tried quietly.
    write_failed: AtomicBool,
    /// The last backup switch, in French: the message while the daemon runs normally.
    switched: Mutex<String>,
    /// The microphone the relay runs on (its identifier), for the backup switch; `None` between
    /// attempts.
    relayed: Mutex<Option<String>>,
}

impl Reporter {
    fn new(options: &Options) -> Reporter {
        let mut status = Status::new(Phase::Starting);
        status.microphone = options.microphone.clone().unwrap_or_default();
        Reporter {
            status: Mutex::new(status),
            effects_wanted: options.effects,
            relaying: AtomicBool::new(false),
            worker_pid: AtomicU32::new(0),
            worker_engine: AtomicBool::new(false),
            write_failed: AtomicBool::new(false),
            switched: Mutex::new(String::new()),
            relayed: Mutex::new(None),
        }
    }

    /// Publishes a backup switch: it stays the message while nothing goes wrong.
    fn switched(&self, message: String) {
        *self.switched.lock().unwrap_or_else(PoisonError::into_inner) = message.clone();
        self.update(|status| status.message = message);
    }

    fn last_switch(&self) -> String {
        self.switched.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn set_relayed(&self, id: Option<String>) {
        *self.relayed.lock().unwrap_or_else(PoisonError::into_inner) = id;
    }

    fn relayed(&self) -> Option<String> {
        self.relayed.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Whether an effects engine runs now, in a worker or in the daemon's own relay: the keep-alive
    /// streams follow it.
    fn engine_alive(&self) -> bool {
        self.worker_engine.load(Ordering::SeqCst) || relay::effects_engine_alive()
    }

    fn worker_pid(&self) -> u32 {
        self.worker_pid.load(Ordering::SeqCst)
    }

    /// The worker relaying the microphone now (0: none), published in status.txt.
    fn set_worker(&self, pid: u32) {
        self.worker_pid.store(pid, Ordering::SeqCst);
        self.update(|status| status.worker = pid);
    }

    /// Writes the status as it stands (at start, when nothing has changed yet).
    fn publish(&self) {
        let status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        self.write(&status);
    }

    fn write(&self, status: &Status) {
        if let Err(error) = status::write(status) {
            if !self.write_failed.swap(true, Ordering::SeqCst) {
                log(format!("cannot publish the daemon's status: {error}"));
            }
        }
    }

    /// Changes the status; writes it when something changed.
    fn update(&self, change: impl FnOnce(&mut Status)) {
        let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        let before = status.clone();
        change(&mut status);
        if *status != before {
            status.updated = crate::guard::stamp();
            self.write(&status);
        }
    }

    fn set(&self, state: Phase, message: impl Into<String>) {
        let message = message.into();
        self.update(|status| {
            status.state = state;
            status.message = message;
        });
    }

    /// Publishes what the relay does (`relay_status`), wherever it runs.
    fn show_relay(&self, activity: relay::Activity, problem: &str) {
        let Some((state, effects, message)) = relay_status(activity, self.effects_wanted, problem, &self.last_switch()) else { return };
        self.update(|status| {
            status.state = state;
            status.effects = effects;
            status.message = message;
        });
    }

    /// While the relay runs inside the daemon, publishes what it does (a worker's reports are
    /// published as they come, see `MicHost`).
    fn follow_relay(&self) {
        if self.relaying.load(Ordering::SeqCst) {
            self.show_relay(relay::activity(), &relay::last_problem());
        }
    }
}

/// What status.txt says while the relay does `activity`: running, degraded (no effects although
/// they were asked for) or waiting for its devices, with whether effects apply and the message;
/// `None` while there is nothing new to say (not started yet, or held ready to take over).
/// `last_switch` is the message while all goes well, `problem` the relay's last problem.
fn relay_status(activity: relay::Activity, effects_wanted: bool, problem: &str, last_switch: &str) -> Option<(Phase, bool, String)> {
    match activity {
        relay::Activity::Idle | relay::Activity::Ready => None,
        relay::Activity::Streaming { effects } if effects || !effects_wanted => Some((Phase::Running, effects, last_switch.to_string())),
        relay::Activity::Streaming { .. } => Some((Phase::Degraded, false, with_detail(mark("Effets du micro indisponibles"), problem))),
        relay::Activity::Interrupted => Some((Phase::WaitingDevices, false, with_detail(mark("Micro interrompu, en attente de ses périphériques"), problem))),
    }
}

/// A French sentence followed by the technical detail, when there is one.
fn with_detail(text: &str, detail: &str) -> String {
    if detail.is_empty() { text.to_string() } else { format!("{text} : {detail}") }
}

pub fn run(options: &Options) -> Result<(), String> {
    // From the very start, Ctrl+C asks for a clean stop, so the defaults are always put back.
    // Closing the console window waits for that cleanup too (see the handler).
    relay::install_ctrl_handler();
    // One daemon at a time: two relays would both feed the virtual microphone.
    let instance = unsafe { CreateMutexW(std::ptr::null_mut(), 0, wide(INSTANCE_NAME).as_ptr()) };
    if instance.is_null() || unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        if !instance.is_null() {
            unsafe { CloseHandle(instance) };
        }
        // status.txt belongs to the daemon that runs: left alone.
        return Err("a platinum daemon is already running (`platinum daemon --stop` stops it)".into());
    }
    // The tray icon and the overlays speak the configured language from the start (the mixer
    // thread follows later changes).
    i18n::set_language(config::load().language.resolve());
    // From here on this process owns status.txt, and the end of the session waits for its cleanup.
    let reporter = Reporter::new(options);
    reporter.publish();
    if sonar_running() {
        reporter.set(Phase::Failed, mark("SteelSeries GG est ouvert : fermez GG pour que Platinum prenne le relais"));
        unsafe { CloseHandle(instance) };
        return Err("GG's Sonar is running and owns the virtual devices: close GG first".into());
    }
    let stop_event = unsafe { CreateEventW(std::ptr::null_mut(), 1, 0, wide(STOP_EVENT_NAME).as_ptr()) };
    if stop_event.is_null() {
        reporter.set(Phase::Failed, mark("Impossible de créer l'événement d'arrêt de la tâche de fond"));
        unsafe { CloseHandle(instance) };
        return Err("cannot create the daemon's stop event".into());
    }
    relay::set_cleanup_pending(true);
    let tray = start_tray(options);
    // The shortcuts thread registers the combinations; the mixer thread carries them out.
    let (actions, pressed) = std::sync::mpsc::channel();
    let shortcuts = Shortcuts::start(actions);

    let result = supervise(options, &reporter, stop_event as usize, pressed, shortcuts.overlays());

    // The combinations go back to Windows before the devices are left.
    drop(shortcuts);
    // The cleanup is done: whoever waits for it (end of session, console closing) may let the
    // process go. Before the tray is dropped, since dropping it waits for its thread.
    relay::set_cleanup_pending(false);
    drop(tray);
    unsafe {
        CloseHandle(stop_event);
        CloseHandle(instance);
    }
    log("daemon stopped");
    result
}

/// The icon next to the clock (a click opens the window, its menu can stop the daemon), or with
/// `--no-tray` only its hidden window, which hears the end of the session.
fn start_tray(options: &Options) -> Option<crate::tray::Tray> {
    let tray = crate::tray::show_with(crate::tray::Handlers {
        tip: Box::new(|| status::read().map_or_else(|| "Platinum".to_string(), |s| s.tooltip())),
        stop: Box::new(|| {
            let _ = request_stop();
        }),
        wait_cleanup: Box::new(relay::wait_for_cleanup),
        confirm_stop: true,
        icon: options.tray,
    });
    if options.tray && tray.is_some() {
        std::thread::sleep(Duration::from_millis(500));
        log(if crate::tray::shown() { "icon shown next to the clock" } else { "icon not accepted yet: offered again every 2 s" });
    }
    tray
}

/// Serves until a stop, then puts the devices back, publishing every step.
fn supervise(options: &Options, reporter: &Reporter, stop_event: usize, pressed: Receiver<Action>, overlays: Overlays) -> Result<(), String> {
    // A watcher thread turns `daemon --stop` and Sonar's return into a clean stop of the relay,
    // and publishes what the relay does.
    let sonar_returned = AtomicBool::new(false);
    let finished = AtomicBool::new(false);
    let result = std::thread::scope(|scope| {
        scope.spawn(|| watch(reporter, stop_event, &sonar_returned, &finished));
        let (finished_ref, gg_ref) = (&finished, &sonar_returned);
        scope.spawn(move || switch_devices(options, reporter, finished_ref, gg_ref, pressed, &overlays));
        let result = serve(options, reporter);
        finished.store(true, Ordering::SeqCst);
        result
    });

    // Putting the defaults back only makes sense if GG has not taken the devices over.
    let gg = sonar_returned.load(Ordering::SeqCst);
    let cleanup = if gg {
        reporter.set(Phase::Stopping, mark("SteelSeries GG a repris la main"));
        Ok(())
    } else {
        reporter.set(Phase::Stopping, mark("Remise en place des périphériques par défaut"));
        log("putting the previous default devices back and disabling the channels");
        Devices::open().map_err(|e| e.to_string()).and_then(|devices| {
            session::stop(&devices)?;
            apply_fallbacks(&devices, options);
            Ok(())
        })
    };
    let (state, message) = match (&result, &cleanup) {
        (Err(error), _) => (Phase::Failed, with_detail(mark("Arrêtée sur une erreur"), error)),
        (Ok(()), Err(error)) => (Phase::Failed, with_detail(mark("Remise en place incomplète"), error)),
        (Ok(()), Ok(())) if gg => (Phase::Stopped, mark("SteelSeries GG gère de nouveau les périphériques").to_string()),
        (Ok(()), Ok(())) => (Phase::Stopped, mark("Périphériques par défaut remis en place").to_string()),
    };
    reporter.update(|status| {
        status.state = state;
        status.message = message;
        status.effects = false;
    });
    result.and(cleanup)
}

/// How often the daemon writes its memory in the log, whatever it is.
const MEMORY_PERIOD: Duration = Duration::from_secs(3600);
/// Private memory, in MB, whose first crossing is logged at once.
const MEMORY_THRESHOLDS: [f64; 3] = [100.0, 200.0, 400.0];

/// When the daemon writes its memory in daemon.log: once at start, every hour, and the first time
/// its private memory crosses each of `MEMORY_THRESHOLDS`. A daemon was once found at 738 MB with
/// nothing in its log; these lines tell when such a rise happens and alongside what.
struct MemoryWatch {
    next: Instant,
    /// How many thresholds have been crossed and logged.
    crossed: usize,
}

impl MemoryWatch {
    fn new(now: Instant) -> MemoryWatch {
        MemoryWatch { next: now + MEMORY_PERIOD, crossed: 0 }
    }

    /// Why a line is due now, given the private memory in MB; `None` when none is. A jump over
    /// several thresholds at once is one line, for the highest. Crossing a threshold leaves the
    /// hourly lines where they were; an hourly line missed while the machine slept is written once.
    fn due(&mut self, now: Instant, private_mb: f64) -> Option<String> {
        let reached = MEMORY_THRESHOLDS.iter().take_while(|threshold| private_mb >= **threshold).count();
        if reached > self.crossed {
            self.crossed = reached;
            return Some(format!("crossed {:.0} MB", MEMORY_THRESHOLDS[reached - 1]));
        }
        if now >= self.next {
            self.next = now + MEMORY_PERIOD;
            return Some("hourly".into());
        }
        None
    }
}

/// One line for the log: private memory, working set, handles, threads, whether the window runs
/// (the window's own activity was the lead when the 738 MB were found), how many keep-alive
/// streams are open (an echo reference without one let the effects engine grow), and the
/// microphone worker's process and memory (`worker`, 0: none).
fn memory_line(reason: &str, worker: u32) -> String {
    let (private, working_set) = crate::guard::memory();
    let table = routing::process_table().unwrap_or_default();
    let me = std::process::id();
    let threads = table.iter().find(|(pid, _, _)| *pid == me).map_or(0, |(_, _, threads)| *threads);
    let window = table.iter().any(|(_, exe, _)| exe.eq_ignore_ascii_case("platinum-ui.exe"));
    let streams = keepalive::open_streams();
    let worker = match (worker, crate::guard::process_memory(worker)) {
        (0, _) => "no mic worker".to_string(),
        (pid, Some((private, working_set))) => format!("mic worker {pid}: private {:.1} MB, working set {:.1} MB", crate::guard::megabytes(private), crate::guard::megabytes(working_set)),
        (pid, None) => format!("mic worker {pid}: memory unreadable"),
    };
    format!(
        "memory ({reason}): private {:.1} MB, working set {:.1} MB, {} handles, {threads} threads, window {}, {streams} keep-alive stream{}; {worker}",
        crate::guard::megabytes(private),
        crate::guard::megabytes(working_set),
        crate::guard::handle_count(),
        if window { "open" } else { "closed" },
        if streams == 1 { "" } else { "s" },
    )
}

/// The watcher: waits for a stop request, looks for Sonar every 2 s, follows a relay that runs
/// inside the daemon, and logs the daemon's memory and its worker's now and then (`MemoryWatch`).
fn watch(reporter: &Reporter, stop_event: usize, sonar_returned: &AtomicBool, finished: &AtomicBool) {
    let mut ticks = 0u32;
    let mut memory = MemoryWatch::new(Instant::now());
    log(memory_line("at start", reporter.worker_pid()));
    while !finished.load(Ordering::SeqCst) {
        if unsafe { WaitForSingleObject(stop_event as *mut c_void, 500) } == WAIT_OBJECT_0 {
            log("stop requested");
            reporter.set(Phase::Stopping, mark("Arrêt demandé"));
            relay::request_stop();
            return;
        }
        ticks += 1;
        if ticks % 4 == 0 && sonar_running() {
            log("GG's Sonar started: stepping aside and leaving the devices to it");
            sonar_returned.store(true, Ordering::SeqCst);
            reporter.set(Phase::Stopping, mark("SteelSeries GG a démarré : Platinum lui laisse la main"));
            relay::request_stop();
            return;
        }
        if ticks % 4 == 0 {
            let private = crate::guard::megabytes(crate::guard::memory().0);
            if let Some(reason) = memory.due(Instant::now(), private) {
                log(memory_line(&reason, reporter.worker_pid()));
            }
        }
        reporter.follow_relay();
    }
}

/// How often the backup switch looks at the devices.
const SWITCH_PERIOD: Duration = Duration::from_secs(2);

/// The mixer thread: every 2 s while the daemon serves, the backup device switch puts the channels
/// and the microphone where the priority lists say, a remote streaming session is followed, and a
/// running game's preset goes on Gaming; in between, it carries out the keyboard shortcuts as they
/// are pressed. While the switch is off, no session is followed, no game runs and no shortcut is
/// pressed, it only reads the configuration, the default output, Steam's running application and,
/// every 5 s, the process list: no device is listed and no settings store is opened, but the
/// microphone's, once, for its echo references (read again only when its registry key changes),
/// which get their silent streams while the effects engine runs (`keepalive`). On the way
/// out, a session followed is left (`gg`: unless GG came back), before the daemon disables the
/// channels; a game's preset stays written down for the next start.
fn switch_devices(options: &Options, reporter: &Reporter, finished: &AtomicBool, gg: &AtomicBool, pressed: Receiver<Action>, overlays: &Overlays) {
    let mut switcher = Switcher::default();
    let going = || !finished.load(Ordering::SeqCst) && !relay::stop_requested();
    let mut next_look = Instant::now();
    while going() {
        if Instant::now() >= next_look {
            switcher.tick(options, reporter);
            next_look = Instant::now() + SWITCH_PERIOD;
        }
        let wait = next_look.saturating_duration_since(Instant::now()).min(Duration::from_millis(250));
        match pressed.recv_timeout(wait) {
            // A stop asked meanwhile (GG coming back among them): its stores are left alone.
            Ok(action) if going() => switcher.shortcut(action, overlays),
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            // No shortcuts thread: only the switch's looks are left.
            Err(RecvTimeoutError::Disconnected) => std::thread::sleep(wait),
        }
    }
    switcher.finish(gg.load(Ordering::SeqCst), reporter);
}

/// What the backup switch keeps from one look to the next.
#[derive(Default)]
struct Switcher {
    devices: Option<Devices>,
    /// The channels' settings stores, opened on the first look that needs them and kept: the
    /// vendor's DLL leaks memory on every opening.
    stores: Option<Stores>,
    /// Bluetooth hands-free endpoints, by identifier (a property read each, kept).
    hands_free: HashMap<String, bool>,
    /// The last problem logged, so that a lasting one is said once.
    announced: String,
    /// The device list as last read, and when: a held microphone shortcut repeats thirty times a
    /// second, and listing every endpoint each time would cost far more than the change.
    listed: Option<(Instant, Vec<Endpoint>)>,
    /// Whether the Stream virtual device is meant to be enabled now (`None`: not looked at yet; it
    /// starts as the daemon brought it up).
    stream_channel: Option<bool>,
    /// When enabling or disabling it last failed: tried again after `STREAM_RETRY`.
    stream_failed: Option<Instant>,
    /// The remote streaming session followed, if any (`remote`).
    remote: RemoteFollow,
    /// The preset per game (`games`).
    games: GameFollow,
    /// The silent streams on the echo-cancellation references (`keepalive`).
    echo: EchoFollow,
}

/// What keeping the echo-cancellation references playing keeps from one look to the next.
#[derive(Default)]
struct EchoFollow {
    /// The keep-alive thread, started the first time an effects engine runs.
    keeper: Option<KeepAlive>,
    /// When the microphone's settings last changed (`None` until first needed).
    clock: Option<StoreClock>,
    /// The clock's reading when the references were last read; `None` when they must be read at
    /// the next look (never read, a read failed, the engine was gone, or the clock says nothing).
    read_at: Option<u64>,
    /// The references last handed to the keeper.
    sent: Vec<String>,
}

/// How often the running processes are looked at for games (Steam's running application is read
/// at every look).
const GAME_SCAN_PERIOD: Duration = Duration::from_secs(5);
/// How often the install folders Epic, GOG and Ubisoft Connect record are read again.
const GAME_INSTALLS_PERIOD: Duration = Duration::from_secs(60);
/// How many times in a row putting the preset of before back may fail before the override is let
/// go (a setting the module keeps refusing would otherwise be tried every 2 s forever).
const GAME_LEAVE_TRIES: u32 = 5;

/// What the preset per game keeps from one look to the next.
#[derive(Default)]
struct GameFollow {
    tracker: games::Tracker,
    /// The override under way, as written in its file; `None` before the file is first read.
    saved: Option<Option<games::Saved>>,
    /// GG's links (Game presets naming games), the Store families they name, and the modification
    /// time of GG's database when they were read (`None` before the first read).
    gg: Vec<games::GgLink>,
    families: Vec<String>,
    gg_time: Option<Option<SystemTime>>,
    /// The user's links, and their file's modification time when read.
    user: Vec<games::UserLink>,
    user_time: Option<Option<SystemTime>>,
    /// The linked games Epic, GOG and Ubisoft Connect installed, with their folders, and when they
    /// were read.
    installs: Vec<games::Installed>,
    installs_read: Option<Instant>,
    scanner: gamescan::Scanner,
    processes: Vec<games::Process>,
    scanned: Option<Instant>,
    /// Steam's running application: its id, when it was first seen (its start, as far as the
    /// daemon knows) and its name.
    steam: Option<(u32, u64, String)>,
    /// state.json's modification time and the override the user ended there, read while an
    /// override is under way.
    ended: Option<(Option<SystemTime>, String)>,
    /// The override whose end by the user was logged.
    ended_said: String,
    /// Putting the preset of before back failed this many times in a row.
    leave_failures: u32,
    /// A game whose preset could not be put on Gaming (it cannot be found, or nothing could be
    /// written down), by game and preset: not tried again until either changes or GG's library
    /// does, rather than reading everything again every 2 s.
    refused: Option<(String, String)>,
}

impl GameFollow {
    fn session(&self) -> Option<&games::Saved> {
        self.saved.as_ref().and_then(Option::as_ref)
    }

    /// Reads the override file the first time: an override written down by a previous run is
    /// followed on, or put back, from the first look.
    fn load(&mut self) {
        if self.saved.is_some() {
            return;
        }
        let saved = games::load().unwrap_or_else(|error| {
            log(format!("game presets: {error}; ignored"));
            None
        });
        if let Some(saved) = &saved {
            log(format!("game presets: {} put {} on Gaming when Platinum last ran (since {}); {} comes back once it no longer runs", saved.game, saved.preset_name, saved.since, restore_title(saved)));
        }
        self.saved = Some(saved);
    }

    /// The feature is off and nothing is under way: what was kept for it goes.
    fn idle(&mut self) {
        if self.scanned.is_none() && self.gg_time.is_none() {
            return;
        }
        *self = GameFollow { saved: self.saved.take(), ..GameFollow::default() };
    }

    /// GG's links again when its database changed (GG ran in between), and the user's when their
    /// file did. A database that cannot be read keeps the links read before.
    fn refresh_links(&mut self) {
        let gg_time = modified(&library::source_path());
        if self.gg_time != Some(gg_time) {
            self.gg_time = Some(gg_time);
            match Library::open_default() {
                Ok(library) => {
                    self.gg = games::gg_links(&library);
                    self.families = games::store_families(&self.gg);
                    // The installs worth watching depend on the links, and a preset that could
                    // not be found may be there now.
                    self.installs_read = None;
                    self.refused = None;
                }
                Err(library::Error::Missing(_)) => self.gg.clear(),
                Err(error) => log(format!("game presets: {error}; GG's links read before are kept")),
            }
        }
        let user_time = games::links_path().ok().and_then(|file| modified(&file));
        if self.user_time != Some(user_time) {
            self.user_time = Some(user_time);
            self.user = games::load_links();
            self.refused = None;
        }
    }

    /// The games running now and the preset Gaming should use for them.
    fn look(&mut self) -> Option<Choice> {
        self.refresh_links();
        if self.gg.is_empty() && self.user.is_empty() {
            return None;
        }
        let mut running: Vec<games::Running> = Vec::new();
        match gamescan::steam_running() {
            Some(app) => {
                if self.steam.as_ref().is_none_or(|(known, _, _)| *known != app) {
                    let name = gamescan::steam_name(app).unwrap_or_else(|| format!("Steam {app}"));
                    self.steam = Some((app, gamescan::now(), name));
                }
                if let Some((app, since, name)) = &self.steam {
                    running.push(games::Running { key: games::GameKey::Steam(*app), name: name.clone(), started: *since });
                }
            }
            None => self.steam = None,
        }
        if self.scanned.is_none_or(|at| at.elapsed() >= GAME_SCAN_PERIOD) {
            if self.installs_read.is_none_or(|at| at.elapsed() >= GAME_INSTALLS_PERIOD) {
                let gg = &self.gg;
                self.installs = gamescan::folder_installs().into_iter().filter(|game| gg.iter().any(|link| link.games.iter().any(|id| game.key.matches(id)))).collect();
                self.installs_read = Some(Instant::now());
            }
            self.processes = self.scanner.scan();
            self.scanned = Some(Instant::now());
        }
        running.extend(games::running_games(&self.processes, &self.installs, &self.families, &self.user));
        games::choose(&games::once_each(running), &self.gg, &self.user)
    }

    /// The override the user ended in the window (state.json, read again only when it changed).
    fn ended_mark(&mut self) -> String {
        let time = presetfiles::state_path().ok().and_then(|file| modified(&file));
        match &self.ended {
            Some((known, mark)) if *known == time => mark.clone(),
            _ => {
                let mark = presetfiles::load_state().game_ended;
                self.ended = Some((time, mark.clone()));
                mark
            }
        }
    }
}

/// A file's modification time, `None` when it is missing.
fn modified(file: &Path) -> Option<SystemTime> {
    std::fs::metadata(file).and_then(|m| m.modified()).ok()
}

/// How the log names the preset that comes back after a game.
fn restore_title(saved: &games::Saved) -> String {
    if PresetRef::parse(&saved.restore) == PresetRef::Custom || saved.restore_name.is_empty() { "the custom settings of before".to_string() } else { saved.restore_name.clone() }
}

/// GG's library and the user's own Game presets, to find what a preset applies.
struct Lookup {
    library: Option<Library>,
    own: Vec<OwnPreset>,
}

impl Lookup {
    fn load() -> Lookup {
        let own = presetfiles::presets_root().map(|root| presetfiles::load_own(&root, preset::Role::Game).0).unwrap_or_default();
        Lookup { library: Library::open_default().ok(), own }
    }

    /// What choosing `key` on Gaming applies, with the user's edits of it, and its name.
    fn resolve(&self, key: &PresetRef, state: &presetfiles::State) -> Option<(String, Preset)> {
        presetfiles::resolve(key, 0, state, self.library.as_ref(), &self.own)
    }
}

/// What following remote streaming keeps from one look to the next.
#[derive(Default)]
struct RemoteFollow {
    tracker: remote::Tracker,
    /// The session followed, as written in its state file; `None` before the file is first read.
    saved: Option<Option<remote::Saved>>,
    /// The session came from the file at start and no look has seen it go on: leaving it then puts
    /// back what a previous run left behind (it crashed, or stopped while GG came back).
    left_over: bool,
    /// The default output's identifier and name, as last read: a name is read once per new default.
    default_name: Option<(String, String)>,
    /// Applications Windows refused to route during this session: not tried again.
    refused: Vec<String>,
    /// Processes seen playing on the sink that need nothing (the host's own recording above all,
    /// which plays all session long): while only they play there, the process list is not read.
    quiet: Vec<u32>,
    /// The backup switch has been said to leave the outputs alone during this session.
    paused_said: bool,
    /// Leaving failed to write an output back and is being tried again: said once, not each time.
    retrying: bool,
}

impl RemoteFollow {
    fn session(&self) -> Option<&remote::Saved> {
        self.saved.as_ref().and_then(Option::as_ref)
    }

    /// Reads the state file the first time: a session written down by a previous run is followed
    /// on, or put back, from the first look.
    fn load(&mut self) {
        if self.saved.is_some() {
            return;
        }
        let saved = remote::load().unwrap_or_else(|error| {
            log(format!("remote streaming: {error}; ignored"));
            None
        });
        if let Some(saved) = &saved {
            log(format!("remote streaming: a session on {} was followed when Platinum last ran (since {})", saved.sink_name, saved.since));
        }
        self.left_over = saved.is_some();
        self.saved = Some(saved);
    }
}

/// How long a device list stays good enough for a shortcut.
const LIST_REUSE: Duration = Duration::from_secs(1);
/// How long after a failure the Stream virtual device is enabled or disabled again.
const STREAM_RETRY: Duration = Duration::from_secs(30);

impl Switcher {
    fn tick(&mut self, options: &Options, reporter: &Reporter) {
        let settings = config::load();
        // The language may have changed in the window: the tray, the overlays and the status
        // follow it from this tick on.
        i18n::set_language(settings.language.resolve());
        self.follow_stream_channel(options, &settings);
        let streaming = self.follow_remote(&settings, reporter);
        self.follow_games(&settings);
        self.follow_echo(reporter.engine_alive());
        if !settings.auto_switch {
            return;
        }
        let Some(endpoints) = self.endpoints() else { return };
        let hands_free = self.hands_free(&endpoints);
        if !settings.output_priority.is_empty() {
            if !streaming {
                self.follow_outputs(&settings, &endpoints, &hands_free, reporter);
            } else if !self.remote.paused_said {
                log("backup switch: outputs left alone while a remote streaming session is followed");
                self.remote.paused_said = true;
            }
        }
        if options.channels.iter().any(|c| c == "mic") {
            follow_microphone(options, &settings, &endpoints, &hands_free, reporter);
        }
    }

    /// Streamer mode (`streamer`): keeps the Stream virtual device enabled while the configuration
    /// asks for it, and disables it once it no longer does (back to classic mode). Nothing is
    /// listed or written while that does not change, which is all the time outside a mode switch.
    fn follow_stream_channel(&mut self, options: &Options, settings: &Config) {
        let wanted = options.wants_stream(settings);
        // At first, what the daemon brought up when it started.
        let current = *self.stream_channel.get_or_insert_with(|| streamer::has_stream_channel(&options.channels));
        if wanted == current || self.stream_failed.is_some_and(|when| when.elapsed() < STREAM_RETRY) || relay::stop_requested() {
            return;
        }
        if self.devices.is_none() {
            self.devices = Devices::open().ok();
        }
        let outcome = match &self.devices {
            Some(devices) if wanted => session::start(devices, &[streamer::STREAM_CHANNEL]),
            Some(devices) => session::disable(devices, streamer::STREAM_CHANNEL),
            None => Err("Windows audio devices unavailable".into()),
        };
        match outcome {
            Ok(()) => {
                log(if wanted { "streamer mode: Stream device enabled" } else { "classic mode: Stream device disabled" });
                self.stream_channel = Some(wanted);
                self.stream_failed = None;
            }
            Err(error) => {
                say_once(&mut self.announced, format!("Stream device not {} ({error}); trying again every 30 s", if wanted { "enabled" } else { "disabled" }));
                self.stream_failed = Some(Instant::now());
                // The device API may be stale (Windows' audio service restarted).
                self.devices = None;
            }
        }
    }

    /// Keeps a silent stream on each output the microphone's echo cancellation listens to, while
    /// an effects engine runs (`keepalive`; `engine_alive`, in a worker or in the daemon), and none
    /// otherwise. The references are read through the stores kept here, and only when the
    /// microphone's settings changed since the last read (`StoreClock`); with the engine stopped,
    /// nothing is read at all. A change made in the window, by the backup switch or by a remote
    /// streaming session is followed within one look.
    fn follow_echo(&mut self, engine_alive: bool) {
        if !engine_alive || relay::stop_requested() {
            if let (Some(keeper), false) = (&self.echo.keeper, self.echo.sent.is_empty()) {
                keeper.want(Vec::new(), "the effects engine stopped");
            }
            self.echo.sent.clear();
            self.echo.read_at = None;
            return;
        }
        let stamp = self.echo.clock.get_or_insert_with(|| StoreClock::new(Channel::Mic)).last_write();
        if stamp.is_some() && stamp == self.echo.read_at {
            return;
        }
        let stores = self.stores.get_or_insert_with(Stores::load_default);
        let held = match stores.get(Channel::Mic).ok_or_else(|| "the microphone's settings cannot be opened".to_string()).and_then(outputs::references_held) {
            Ok(held) => held,
            Err(error) => {
                // Read again at the next look; the streams stay as they are meanwhile.
                say_once(&mut self.announced, format!("keep-alive: the echo references cannot be read ({error})"));
                self.echo.read_at = None;
                return;
            }
        };
        self.echo.read_at = stamp;
        let wanted = keepalive::references(&held);
        if wanted == self.echo.sent {
            return;
        }
        if self.echo.keeper.is_none() {
            match KeepAlive::start() {
                Ok(keeper) => self.echo.keeper = Some(keeper),
                Err(error) => {
                    say_once(&mut self.announced, format!("keep-alive: {error}"));
                    self.echo.read_at = None;
                    return;
                }
            }
        }
        if let Some(keeper) = &self.echo.keeper {
            keeper.want(wanted.clone(), "no longer an echo reference");
        }
        self.echo.sent = wanted;
    }

    /// Carries out one keyboard shortcut with the stores and devices kept here, then shows its
    /// overlay. A device change is logged; a problem is logged once while it lasts.
    fn shortcut(&mut self, action: Action, overlays: &Overlays) {
        // During a remote streaming session the outputs follow the streaming output: a device
        // shortcut would take the sound away from the remote device.
        if matches!(action, Action::PreviousOutput | Action::NextOutput) && self.remote.session().is_some() {
            let title = if action == Action::NextOutput { tr("Périphérique suivant") } else { tr("Périphérique précédent") };
            say_once(&mut self.announced, format!("shortcut {}: outputs left alone while a remote streaming session is followed", action.key()));
            overlays.show(Content::unavailable(Kind::Device, Glyph::Device, title, tr("Streaming à distance en cours")));
            return;
        }
        let settings = config::load();
        let (endpoints, hands_free) = if shortcuts::needs_devices(action) {
            let Some(endpoints) = self.recent_endpoints() else {
                say_once(&mut self.announced, format!("shortcut {}: Windows audio devices unavailable", action.key()));
                return;
            };
            let hands_free = self.hands_free(&endpoints);
            (endpoints, hands_free)
        } else {
            (Vec::new(), Vec::new())
        };
        let stores = self.stores.get_or_insert_with(Stores::load_default);
        let outcome = shortcuts::carry_out(action, stores, self.devices.as_ref(), &endpoints, &hands_free, &settings);
        let moved = matches!(action, Action::PreviousOutput | Action::NextOutput) && outcome.log.is_some();
        if let Some(line) = outcome.log {
            log(line);
        }
        if moved {
            // The device shortcuts move the four channels at once.
            self.arena_after_moves(&settings, &endpoints, &outputs::SURROUND);
        }
        if let Some(problem) = outcome.problem {
            say_once(&mut self.announced, problem);
        }
        if let Some(content) = outcome.overlay {
            overlays.show(content);
        }
    }

    /// The device list, read again when the last one is more than `LIST_REUSE` old.
    fn recent_endpoints(&mut self) -> Option<Vec<Endpoint>> {
        if let Some((when, list)) = &self.listed {
            if when.elapsed() < LIST_REUSE {
                return Some(list.clone());
            }
        }
        self.endpoints()
    }

    /// The device list, reopening the device API when Windows' audio service has restarted.
    fn endpoints(&mut self) -> Option<Vec<Endpoint>> {
        let list = self.list_endpoints();
        self.listed = list.clone().map(|list| (Instant::now(), list));
        list
    }

    fn list_endpoints(&mut self) -> Option<Vec<Endpoint>> {
        if let Some(list) = self.devices.as_ref().and_then(|d| d.list().ok()) {
            return Some(list);
        }
        self.devices = Devices::open().ok();
        self.devices.as_ref().and_then(|d| d.list().ok())
    }

    /// The usable-looking endpoints that are a Bluetooth headset's hands-free side.
    fn hands_free(&mut self, endpoints: &[Endpoint]) -> Vec<String> {
        let Some(devices) = &self.devices else { return Vec::new() };
        let known = &mut self.hands_free;
        endpoints
            .iter()
            .filter(|e| e.state == State::Active && !fallback::is_virtual(e))
            .filter(|e| *known.entry(e.id.clone()).or_insert_with(|| devices.is_hands_free(&e.id)))
            .map(|e| e.id.clone())
            .collect()
    }

    /// Moves the routed output channels to the first usable device of the output list, then points
    /// the microphone's echo cancellation at it, as the window does after a change of output.
    fn follow_outputs(&mut self, settings: &Config, endpoints: &[Endpoint], hands_free: &[String], reporter: &Reporter) {
        let stores = self.stores.get_or_insert_with(Stores::load_default);
        if let Some(error) = stores.api_error() {
            say_once(&mut self.announced, format!("backup switch: the channels' settings are unavailable ({error})"));
            return;
        }
        let routed = routed_outputs(stores);
        let plan = fallback::plan_outputs(&settings.output_priority, endpoints, hands_free, &routed);
        let Some(target) = plan.target else { return };
        // A stop asked meanwhile (GG coming back among them): its stores are left alone.
        if relay::stop_requested() {
            return;
        }
        let mut moved: Vec<(&str, &String)> = Vec::new();
        let mut channels: Vec<Channel> = Vec::new();
        for (index, from) in &plan.moves {
            let (channel, title) = outputs::CHANNELS[*index];
            let Some(store) = stores.get(channel) else { continue };
            match outputs::set_output(store, Some(&target.id)) {
                Ok(()) => {
                    moved.push((title, from));
                    channels.push(channel);
                }
                Err(error) => say_once(&mut self.announced, format!("backup switch: {} not moved to {} ({error})", channel.label(), target.name)),
            }
        }
        if moved.is_empty() {
            return;
        }
        log(format!("backup switch to {}: {}", target.name, moves_text(&moved, "from", endpoints, &settings.output_priority)));
        let titles: Vec<&str> = moved.iter().map(|(title, _)| *title).collect();
        reporter.switched(with_detail(mark("Bascule de secours"), &format!("{} → {}", titles.join(", "), target.name)));
        if let Err(error) = sync_echo_now(stores, settings, endpoints) {
            log(format!("backup switch: echo cancellation not pointed at the new output ({error})"));
        }
        self.arena_after_moves(settings, endpoints, &channels);
    }

    /// Remote streaming (`remote`): reads the default output, then enters, follows or leaves a
    /// session, and publishes the streaming output followed. Returns whether a session is followed
    /// after the look. With following off and no session, nothing is read.
    fn follow_remote(&mut self, settings: &Config, reporter: &Reporter) -> bool {
        self.remote.load();
        let following = self.remote.session().map(|saved| saved.sink.clone());
        if settings.follow_streaming || following.is_some() {
            let look = self.look_default(&settings.streaming_sinks);
            match self.remote.tracker.decide(settings.follow_streaming, &look, following.as_deref()) {
                Step::Nothing => {}
                Step::Enter => {
                    if let Look::Sink(id) = &look {
                        self.enter_remote(id, settings);
                    }
                }
                Step::Follow => {
                    self.remote.left_over = false;
                    self.route_remote_apps(settings);
                }
                Step::Leave => {
                    let why = if !settings.follow_streaming {
                        "following turned off"
                    } else if self.remote.left_over {
                        "left over from the previous run"
                    } else {
                        "it is no longer the default output"
                    };
                    self.leave_remote(settings, why);
                }
            }
        }
        let shown = self.remote.session().map(|saved| remote::short_name(&saved.sink_name).to_string()).unwrap_or_default();
        reporter.update(|status| status.remote = shown);
        self.remote.session().is_some()
    }

    /// The default output for the console role, as a streaming sink or not: one call per look, and
    /// its name read once per new default.
    fn look_default(&mut self, sinks: &[String]) -> Look {
        if self.devices.is_none() {
            self.devices = Devices::open().ok();
        }
        let Some(devices) = &self.devices else { return Look::Unknown };
        let id = match devices.default(Flow::Render, Role::Console) {
            Ok(Some(id)) => id,
            Ok(None) => return Look::Other,
            Err(_) => {
                // The device API may be stale (Windows' audio service restarted).
                self.devices = None;
                return Look::Unknown;
            }
        };
        let name = match &self.remote.default_name {
            Some((known, name)) if *known == id => name.clone(),
            _ => {
                let name = devices.name(&id, NameField::Friendly).unwrap_or_default();
                if !name.is_empty() {
                    self.remote.default_name = Some((id.clone(), name.clone()));
                }
                name
            }
        };
        if remote::is_sink_name(&name, sinks) { Look::Sink(id) } else { Look::Other }
    }

    /// A session began on `sink_id`: writes down where the channels play, then moves those that
    /// play somewhere to the sink, points the echo cancellation at it, and routes the applications
    /// playing on it. Nothing is changed while the state file cannot be written: a crash could not
    /// put it back.
    fn enter_remote(&mut self, sink_id: &str, settings: &Config) {
        // A stop asked meanwhile (GG coming back among them): its stores are left alone.
        if relay::stop_requested() {
            return;
        }
        let Some(endpoints) = self.endpoints() else { return };
        let Some(sink) = remote::sink(sink_id, &endpoints, &settings.streaming_sinks).cloned() else {
            say_once(&mut self.announced, format!("remote streaming: the default output {sink_id} is not a streaming output Platinum can play to; not followed"));
            return;
        };
        let stores = self.stores.get_or_insert_with(Stores::load_default);
        if let Some(error) = stores.api_error() {
            say_once(&mut self.announced, format!("remote streaming: the channels' settings are unavailable ({error}); {} not followed", sink.name));
            return;
        }
        let now = routed_outputs(stores);
        let before: [Option<String>; STRIPS] = std::array::from_fn(|strip| now.get(strip).cloned().flatten());
        let target = session::channel_output(&endpoints, "game").map(|e| e.id.clone()).unwrap_or_default();
        let saved = remote::Saved::new(&sink, before, &target, &crate::guard::stamp());
        if let Err(error) = remote::save(&saved) {
            say_once(&mut self.announced, format!("remote streaming: {error}; {} not followed, since nothing may change that a crash could not put back", sink.name));
            return;
        }
        let mut moved: Vec<(&str, &String)> = Vec::new();
        for strip in saved.moved() {
            let (channel, title) = outputs::CHANNELS[strip];
            let Some(from) = saved.before[strip].as_ref() else { continue };
            let outcome = stores.get(channel).ok_or_else(|| "its settings cannot be read".to_string()).and_then(|store| outputs::set_output(store, Some(&sink.id)));
            match outcome {
                Ok(()) => moved.push((title, from)),
                Err(error) => log(format!("remote streaming: {} not moved to {} ({error})", channel.label(), sink.name)),
            }
        }
        let what = if moved.is_empty() { "no channel had an output to move".to_string() } else { format!("moved {}", moves_text(&moved, "from", &endpoints, &[])) };
        log(format!("remote streaming: {} is the default output, following it: {what}", sink.name));
        if let Err(error) = sync_echo_now(stores, settings, &endpoints) {
            log(format!("remote streaming: echo cancellation not pointed at {} ({error})", sink.name));
        }
        self.remote.saved = Some(Some(saved));
        self.remote.left_over = false;
        self.remote.refused.clear();
        self.remote.quiet.clear();
        self.remote.paused_said = false;
        self.remote.retrying = false;
        self.route_remote_apps(settings);
    }

    /// Routes to Gaming each application playing on the sink that follows the default output
    /// (`remote::should_route`), writing it down before it is routed. Most looks find nothing but
    /// the host's recording there, and then list neither the processes nor the devices.
    fn route_remote_apps(&mut self, settings: &Config) {
        if !settings.streaming_route_apps || relay::stop_requested() {
            return;
        }
        let Some(mut saved) = self.remote.session().cloned() else { return };
        let sessions = match self.devices.as_ref().map(|devices| devices.sessions(&saved.sink)) {
            Some(Ok(sessions)) => sessions,
            Some(Err(error)) => {
                say_once(&mut self.announced, format!("remote streaming: the applications playing on {} cannot be listed ({error})", saved.sink_name));
                return;
            }
            None => return,
        };
        let me = std::process::id();
        let quiet = &self.remote.quiet;
        if !sessions.iter().any(|s| s.active && s.pid != 0 && s.pid != me && !quiet.contains(&s.pid)) {
            return;
        }
        let table = routing::all_processes().unwrap_or_default();
        let found: Vec<(u32, String, bool)> = sessions.iter().filter_map(|s| table.iter().find(|(pid, _)| *pid == s.pid).map(|(_, exe)| (s.pid, exe.clone(), s.active))).collect();
        let refused = &self.remote.refused;
        let candidates: Vec<(String, Vec<u32>)> = remote::candidates(&found, me).into_iter().filter(|(exe, _)| !saved.routed(exe) && !refused.iter().any(|r| r.eq_ignore_ascii_case(exe))).collect();
        // Whatever plays there now and is no candidate needs nothing until it stops.
        for (pid, _, _) in found.iter().filter(|(pid, _, playing)| *playing && !candidates.iter().any(|(_, pids)| pids.contains(pid))) {
            if !self.remote.quiet.contains(pid) {
                self.remote.quiet.push(*pid);
            }
        }
        if candidates.is_empty() {
            return;
        }
        // Routed to Gaming, an application is heard where Gaming plays: only worth it while Gaming
        // plays to the sink (without an output, or out of the personal mix, it would go silent).
        let gaming_output = self.stores.get_or_insert_with(Stores::load_default).get(Channel::Game).and_then(outputs::routed);
        if gaming_output.as_deref() != Some(saved.sink.as_str()) {
            say_once(&mut self.announced, format!("remote streaming: Gaming does not play to {}; the applications playing there are left on it", saved.sink_name));
            return;
        }
        // Gaming's playback side must be up to take them (the channels may not be up yet).
        let Some(endpoints) = self.endpoints() else { return };
        let Some(gaming) = endpoints.iter().find(|e| e.id == saved.target && e.state == State::Active).cloned() else { return };
        let routing = match Routing::open() {
            Ok(routing) => routing,
            Err(error) => {
                say_once(&mut self.announced, format!("remote streaming: per-application routing unavailable ({error})"));
                return;
            }
        };
        for (exe, pids) in candidates {
            let routes: Vec<Option<String>> = pids.iter().map(|pid| routing.get(*pid, Flow::Render).ok().flatten()).collect();
            if !remote::should_route(&saved, &exe, &routes) {
                self.remote.quiet.extend(pids);
                continue;
            }
            saved.apps.push(exe.clone());
            if let Err(error) = remote::save(&saved) {
                saved.apps.pop();
                say_once(&mut self.announced, format!("remote streaming: {error}; {exe} not routed"));
                break;
            }
            let processes = routing::processes(&exe).unwrap_or_default();
            let accepted = processes.iter().filter(|(pid, _)| routing.set(*pid, Flow::Render, Some(&gaming.id)).is_ok()).count();
            if accepted == 0 {
                saved.apps.pop();
                let _ = remote::save(&saved);
                log(format!("remote streaming: {exe} not routed to {}: Windows refused", gaming.name));
                self.remote.quiet.extend(pids.iter().copied());
                self.remote.refused.push(exe);
                continue;
            }
            log(format!("remote streaming: {exe} routed to {}", gaming.name));
        }
        self.remote.saved = Some(Some(saved));
    }

    /// The session is over, following was turned off, or the daemon stops: the outputs still on
    /// the sink go back (`remote::Saved::restore`), the echo cancellation follows, the applications
    /// Platinum routed go back to the default output while their route is still Platinum's, and
    /// platinum.ini forgets the sink. Then the state file goes. An output that cannot be written
    /// back keeps the file, and the next look tries again.
    fn leave_remote(&mut self, settings: &Config, why: &str) {
        let Some(mut saved) = self.remote.session().cloned() else { return };
        let endpoints = self.endpoints().unwrap_or_default();
        let stores = self.stores.get_or_insert_with(Stores::load_default);
        if let Some(error) = stores.api_error() {
            say_once(&mut self.announced, format!("remote streaming: the channels' settings are unavailable ({error}); outputs not put back yet"));
            return;
        }
        let now = routed_outputs(stores);
        let mut back: Vec<(&str, String)> = Vec::new();
        let mut problems: Vec<String> = Vec::new();
        let mut failed = false;
        for (strip, device) in saved.restore(&now) {
            let (channel, title) = outputs::CHANNELS[strip];
            let outcome = stores.get(channel).ok_or_else(|| "its settings cannot be read".to_string()).and_then(|store| outputs::set_output(store, Some(&device)));
            match outcome {
                Ok(()) => back.push((title, device)),
                Err(error) => {
                    failed = true;
                    problems.push(format!("remote streaming: {} not put back on {} ({error}); tried again in a moment", channel.label(), describe(&device, &endpoints, &[])));
                }
            }
        }
        if let Err(error) = sync_echo_now(stores, settings, &endpoints) {
            problems.push(format!("remote streaming: echo cancellation not pointed back at the outputs ({error})"));
        }
        let moved: Vec<(&str, &String)> = back.iter().map(|(title, device)| (*title, device)).collect();
        if self.remote.retrying {
            // Said in full on the first attempt: a lasting problem is said once.
            if !moved.is_empty() {
                log(format!("remote streaming: put back {}", moves_text(&moved, "on", &endpoints, &[])));
            }
            if !problems.is_empty() {
                say_once(&mut self.announced, problems.join("; "));
            }
        } else {
            let what = if moved.is_empty() { "no output to put back".to_string() } else { format!("put back {}", moves_text(&moved, "on", &endpoints, &[])) };
            log(format!("remote streaming: leaving {} ({why}): {what}", saved.sink_name));
            problems.iter().for_each(log);
        }
        if !back.is_empty() {
            let channels: Vec<Channel> = back.iter().filter_map(|(title, _)| outputs::CHANNELS.iter().find(|(_, t)| t == title).map(|(channel, _)| *channel)).collect();
            self.arena_after_moves(settings, &endpoints, &channels);
        }
        unroute_apps(&saved, &endpoints);
        repair_config(&saved);
        if failed {
            // The applications and platinum.ini are done: only the outputs are tried again.
            saved.apps.clear();
            if let Err(error) = remote::save(&saved) {
                log(format!("remote streaming: {error}"));
            }
            self.remote.saved = Some(Some(saved));
            self.remote.retrying = true;
            return;
        }
        if let Err(error) = remote::remove() {
            log(format!("remote streaming: {error}; the next start will look at it again"));
        }
        self.remote.saved = Some(None);
        self.remote.left_over = false;
        self.remote.retrying = false;
    }

    /// The preset per game (`games`): looks at the games running, then puts a game's preset on
    /// Gaming, switches to another game's, puts the preset of before back, or lets go of an
    /// override the user ended in the window. With the feature off and nothing under way, nothing
    /// is read.
    fn follow_games(&mut self, settings: &Config) {
        self.games.load();
        let under_way = self.games.session().cloned();
        if !settings.game_presets && under_way.is_none() {
            self.games.idle();
            return;
        }
        let choice = if settings.game_presets { self.games.look() } else { None };
        if choice.is_none() {
            // The next game start is tried afresh.
            self.games.refused = None;
        }
        let dismissed = under_way.as_ref().is_some_and(|saved| self.games.ended_mark() == saved.id());
        if let (true, Some(saved)) = (dismissed, &under_way) {
            if self.games.ended_said != saved.id() {
                log(format!("game presets: a preset chosen in the window ends {}'s preset; Gaming stays as the user set it", saved.game));
                self.games.ended_said = saved.id();
            }
        }
        let refused = |choice: &Choice| self.games.refused.as_ref().is_some_and(|(game, preset)| *game == choice.game_key && *preset == choice.preset);
        match self.games.tracker.decide(settings.game_presets, choice.as_ref(), under_way.as_ref(), dismissed) {
            games::Step::Nothing => {}
            games::Step::Enter(choice) | games::Step::Switch(choice) if refused(&choice) => {}
            games::Step::Enter(choice) => self.put_game_preset(&choice, None, settings),
            games::Step::Switch(choice) => self.put_game_preset(&choice, under_way.as_ref(), settings),
            games::Step::Leave => self.leave_game(settings, if settings.game_presets { "it no longer runs" } else { "preset per game turned off" }),
            games::Step::Drop => {
                if let Some(saved) = &under_way {
                    log(format!("game presets: {} no longer runs; the preset chosen in the window stays on Gaming", saved.game));
                }
                if let Err(error) = games::remove() {
                    log(format!("game presets: {error}"));
                }
                self.games.saved = Some(None);
            }
        }
    }

    /// Puts a game's preset on Gaming, as the window applies a preset: the user's edits of it, the
    /// channel's "Effets" switch kept, GG's Arena 7 rule, only the settings that change. Writes the
    /// override down first (what Gaming used before, from state.json, and its settings as the
    /// module holds them), so that a crash can put it back. `switching` is the override under way
    /// when another game takes over: what comes back stays the same.
    fn put_game_preset(&mut self, choice: &Choice, switching: Option<&games::Saved>, settings: &Config) {
        // A stop asked meanwhile (GG coming back among them): its stores are left alone.
        if relay::stop_requested() {
            return;
        }
        let endpoints = self.endpoints().unwrap_or_default();
        let stores = self.stores.get_or_insert_with(Stores::load_default);
        if let Some(error) = stores.api_error() {
            say_once(&mut self.announced, format!("game presets: the channels' settings are unavailable ({error}); {} not applied", choice.preset_name));
            return;
        }
        let Some(store) = stores.get(Channel::Game) else {
            say_once(&mut self.announced, format!("game presets: Gaming's settings cannot be read; {} not applied", choice.preset_name));
            return;
        };
        let current = match preset::from_store(store, preset::Role::Game) {
            Ok(current) => current,
            Err(error) => {
                say_once(&mut self.announced, format!("game presets: Gaming's settings cannot be read ({error}); {} not applied", choice.preset_name));
                return;
            }
        };
        let state = presetfiles::load_state();
        let lookup = Lookup::load();
        let Some((name, data)) = lookup.resolve(&PresetRef::parse(&choice.preset), &state) else {
            say_once(&mut self.announced, format!("game presets: {} is linked to {}, which cannot be found; Gaming left as it is", choice.game, choice.preset_name));
            self.games.refused = Some((choice.game_key.clone(), choice.preset.clone()));
            return;
        };
        let saved = match switching {
            Some(previous) => games::Saved { game: choice.game.clone(), game_key: choice.game_key.clone(), preset: choice.preset.clone(), preset_name: name.clone(), ..previous.clone() },
            None => {
                let restore = &state.channels[0];
                let restore_name = lookup.resolve(&restore.selection, &state).map_or_else(|| restore.name.clone(), |(name, _)| name);
                games::Saved {
                    game: choice.game.clone(),
                    game_key: choice.game_key.clone(),
                    preset: choice.preset.clone(),
                    preset_name: name.clone(),
                    restore: restore.selection.key(),
                    restore_name,
                    before: Some(current),
                    since: crate::guard::stamp(),
                }
            }
        };
        if let Err(error) = games::save(&saved) {
            say_once(&mut self.announced, format!("game presets: {error}; {} not applied, since nothing may change that a crash could not put back", name));
            self.games.refused = Some((choice.game_key.clone(), choice.preset.clone()));
            return;
        }
        let (written, arena) = apply_on_gaming(store, data.with_effects_of(&current), settings, &endpoints);
        let what = match written {
            Ok(count) => format!("{count} settings written{}", if arena { ", speaker mode for the Arena 7" } else { "" }),
            Err(error) => format!("applied in part: {error}"),
        };
        let link = if choice.by_user { "your link" } else { "GG's link" };
        match switching {
            Some(_) => log(format!("game presets: {} took over ({link}): Gaming uses {name} ({what}); {} still comes back at the end", choice.game, restore_title(&saved))),
            None => log(format!("game presets: {} runs ({link}): Gaming uses {name} ({what}); {} comes back when it exits", choice.game, restore_title(&saved))),
        }
        self.games.saved = Some(Some(saved));
        self.games.leave_failures = 0;
        self.games.refused = None;
    }

    /// The game exited, or the feature was turned off: the preset Gaming used before comes back,
    /// with the user's edits of it as state.json holds them now (else the settings of before, as
    /// written down), the "Effets" switch as it is now. The file goes once Gaming is back; a
    /// failure keeps it, and the next look tries again (`GAME_LEAVE_TRIES` times).
    fn leave_game(&mut self, settings: &Config, why: &str) {
        let Some(saved) = self.games.session().cloned() else { return };
        if relay::stop_requested() {
            return;
        }
        let endpoints = self.endpoints().unwrap_or_default();
        let stores = self.stores.get_or_insert_with(Stores::load_default);
        if let Some(error) = stores.api_error() {
            say_once(&mut self.announced, format!("game presets: the channels' settings are unavailable ({error}); {} not put back yet", restore_title(&saved)));
            return;
        }
        let Some(store) = stores.get(Channel::Game) else {
            say_once(&mut self.announced, format!("game presets: Gaming's settings cannot be read; {} not put back yet", restore_title(&saved)));
            return;
        };
        let state = presetfiles::load_state();
        let data = Lookup::load().resolve(&PresetRef::parse(&saved.restore), &state).map(|(_, data)| data).or(saved.before);
        let nothing = data.is_none();
        let outcome = match data {
            Some(mut data) => {
                if let Ok(preset::Value::Bool(on)) = preset::read_setting(store, 4) {
                    data.set_effects(on);
                }
                apply_on_gaming(store, data, settings, &endpoints)
            }
            None => (Ok(0), false),
        };
        match outcome {
            (Ok(count), arena) => {
                let detail = if nothing { "nothing to put back".to_string() } else { format!("{count} settings written{}", if arena { ", speaker mode for the Arena 7" } else { "" }) };
                log(format!("game presets: {} ({why}): Gaming back on {} ({detail})", saved.game, restore_title(&saved)));
            }
            (Err(error), _) => {
                self.games.leave_failures += 1;
                if self.games.leave_failures < GAME_LEAVE_TRIES {
                    say_once(&mut self.announced, format!("game presets: {} not put back in full ({error}); tried again in a moment", restore_title(&saved)));
                    return;
                }
                log(format!("game presets: {} put back in part after {GAME_LEAVE_TRIES} tries ({error}); let go", restore_title(&saved)));
            }
        }
        if let Err(error) = games::remove() {
            log(format!("game presets: {error}; the next start will look at it again"));
        }
        self.games.saved = Some(None);
        self.games.leave_failures = 0;
    }

    /// GG's Arena 7 rule after the daemon moved the output of `moved` channels: each of them with
    /// spatial audio that now plays to Arena 7 speakers in headphone mode goes to speaker mode. The
    /// channels it did not move keep the mode the user picked.
    fn arena_after_moves(&mut self, settings: &Config, endpoints: &[Endpoint], moved: &[Channel]) {
        if !settings.arena_speakers || relay::stop_requested() {
            return;
        }
        let Some(stores) = self.stores.as_mut() else { return };
        for channel in outputs::SURROUND.into_iter().filter(|c| moved.contains(c)) {
            let Some(store) = stores.get(channel) else { continue };
            match outputs::arena_rule(store, endpoints) {
                Ok(true) => log(format!("arena 7: {} now plays to the Arena 7 with spatial audio in headphone mode; speaker mode", channel.label())),
                Ok(false) => {}
                Err(error) => say_once(&mut self.announced, format!("arena 7: {} not checked ({error})", channel.label())),
            }
        }
    }

    /// The daemon stops: a session followed is left first, while the channels are still up, unless
    /// GG came back (its outputs are its own; the state file stays for Platinum's next start). A
    /// game's preset still on Gaming stays written down: the next start goes on with it, or puts
    /// the preset of before back if the game no longer runs (a restart for a new microphone, in the
    /// middle of a game, then changes nothing).
    fn finish(&mut self, gg: bool, reporter: &Reporter) {
        self.games.load();
        if let Some(saved) = self.games.session().cloned() {
            if self.games.ended_mark() != saved.id() {
                log(format!("game presets: {}'s preset stays on Gaming; the next start puts {} back once the game no longer runs", saved.game, restore_title(&saved)));
            }
        }
        self.remote.load();
        if self.remote.session().is_none() {
            return;
        }
        if gg {
            log("remote streaming: GG took over; the session's state file stays for Platinum's next start");
            return;
        }
        self.leave_remote(&config::load(), "Platinum stops");
        if self.remote.session().is_none() {
            reporter.update(|status| status.remote.clear());
        }
    }
}

/// Applies a whole preset on Gaming as the window applies one chosen (`preset::apply_whole`), GG's
/// Arena 7 rule first when Gaming plays to Arena 7 speakers; returns what was written and whether
/// the rule put speaker mode.
fn apply_on_gaming(store: &Store, mut next: Preset, settings: &Config, endpoints: &[Endpoint]) -> (Result<usize, String>, bool) {
    let arena = settings.arena_speakers && outputs::plays_to_arena(store, endpoints) && next.arena_speakers();
    (preset::apply_whole(store, preset::Role::Game, &next), arena)
}

/// Where each output channel plays now, Gaming to Aux (`None`: no output, or unreadable).
fn routed_outputs(stores: &mut Stores) -> Vec<Option<String>> {
    outputs::CHANNELS.iter().map(|(channel, _)| stores.get(*channel).and_then(outputs::routed)).collect()
}

/// Points the microphone's echo cancellation at the outputs the channels play to now, as the
/// window does after a change of output.
fn sync_echo_now(stores: &mut Stores, settings: &Config, endpoints: &[Endpoint]) -> Result<usize, String> {
    let routed = routed_outputs(stores);
    let stream_device = settings.stream_device.resolve(endpoints);
    let stream = outputs::StreamReference::of(settings.mode, &settings.channels, stream_device.as_deref());
    match stores.get(Channel::Mic) {
        Some(mic) => outputs::sync_echo(mic, &routed, stream),
        None => Err("the microphone's settings are unavailable".into()),
    }
}

/// The channels moved, grouped by the device each left or reached: "gaming, chat from
/// Haut-parleurs (SteelSeries Arena 7) (active); aux from Casque (AirPods Pro) (absent)".
fn moves_text(moved: &[(&str, &String)], word: &str, endpoints: &[Endpoint], list: &[fallback::PriorityDevice]) -> String {
    let group = |device: &String| {
        let channels: Vec<String> = moved.iter().filter(|(_, d)| *d == device).map(|(title, _)| title.to_lowercase()).collect();
        format!("{} {word} {}", channels.join(", "), describe(device, endpoints, list))
    };
    distinct_sources(moved).into_iter().map(group).collect::<Vec<_>>().join("; ")
}

/// At the end of a session, each application Platinum routed goes back to the default output while
/// its route is still the one Platinum gave it; one routed elsewhere since keeps that route. One
/// that is not running cannot be given back (Windows routes running processes): it keeps its
/// route to Gaming, which changes nothing while Gaming is the default.
fn unroute_apps(saved: &remote::Saved, endpoints: &[Endpoint]) {
    if saved.apps.is_empty() {
        return;
    }
    let routing = match Routing::open() {
        Ok(routing) => routing,
        Err(error) => {
            log(format!("remote streaming: per-application routing unavailable ({error}); {} keep their route to Gaming", saved.apps.join(", ")));
            return;
        }
    };
    let gaming = endpoints.iter().find(|e| e.id == saved.target).map_or("Gaming", |e| e.name.as_str());
    for exe in &saved.apps {
        let processes = routing::processes(exe).unwrap_or_default();
        if processes.is_empty() {
            log(format!("remote streaming: {exe} is not running and keeps its route to {gaming}"));
            continue;
        }
        let (mut given_back, mut elsewhere) = (0, false);
        for (pid, _) in &processes {
            let route = routing.get(*pid, Flow::Render).ok().flatten();
            if remote::still_ours(route.as_deref(), &saved.target) {
                given_back += usize::from(routing.set(*pid, Flow::Render, None).is_ok());
            } else if route.is_some() {
                elsewhere = true;
            }
        }
        if given_back > 0 {
            log(format!("remote streaming: {exe} back on the default output"));
        } else if elsewhere {
            log(format!("remote streaming: {exe} was routed elsewhere during the session; left there"));
        }
    }
}

/// Takes the sink out of platinum.ini at the end of a session (`remote::Saved::repair`).
fn repair_config(saved: &remote::Saved) {
    let Ok(mut config) = config::load_full().for_saving() else { return };
    if saved.repair(&mut config) {
        match config.save() {
            Ok(()) => log("remote streaming: platinum.ini no longer names the streaming output (personal mix, classic outputs)"),
            Err(error) => log(format!("remote streaming: platinum.ini still names the streaming output ({error})")),
        }
    }
}

/// While the relay runs on another microphone than the one wanted now (the chosen one came back,
/// or the one relayed went away and a backup is plugged in), ends that relay run: the daemon starts
/// it again at once on the wanted one. Nothing while no relay runs: its next attempt chooses.
fn follow_microphone(options: &Options, settings: &Config, endpoints: &[Endpoint], hands_free: &[String], reporter: &Reporter) {
    let Some(relayed) = reporter.relayed() else { return };
    let configured = options.microphone.clone().or_else(|| settings.microphone.clone());
    let Ok(wanted) = fallback::microphone(configured.as_deref(), &settings.input_priority, settings.auto_switch, endpoints, hands_free) else { return };
    if wanted.id == relayed {
        return;
    }
    log(format!("backup switch: microphone {} instead of {}; restarting the relay", wanted.name, describe(&relayed, endpoints, &settings.input_priority)));
    reporter.switched(with_detail(mark("Micro de secours"), &wanted.name));
    relay::request_restart();
}

/// The microphone to relay now (see `fallback::microphone`): the configured one, or with the
/// backup switch on, the first usable one of the input list while it is missing. On a first run,
/// with no microphone chosen anywhere, Windows' default real microphone, so that the virtual one
/// is not silent until the user opens the window (`fallback::default_microphone`).
fn pick_microphone<'a>(devices: &Devices, options: &Options, settings: &Config, endpoints: &'a [Endpoint]) -> Result<&'a Endpoint, String> {
    let configured = options.microphone.clone().or_else(|| settings.microphone.clone());
    let inputs: Vec<Endpoint> = endpoints.iter().filter(|e| e.flow == Flow::Capture).cloned().collect();
    let hands_free = hands_free_ids(devices, &inputs);
    match fallback::microphone(configured.as_deref(), &settings.input_priority, settings.auto_switch, endpoints, &hands_free) {
        Err(error) if error == fallback::NO_MICROPHONE => {
            // Windows' defaults now (the virtual microphone, once Platinum made it the default, is
            // passed over), then the ones recorded before Platinum changed them.
            let mut candidates: Vec<String> = [Role::Communications, Role::Console, Role::Multimedia].into_iter().filter_map(|role| devices.default(Flow::Capture, role).ok().flatten()).collect();
            candidates.extend(session::previous_defaults().into_iter().filter(|(flow, _, _)| *flow == Flow::Capture).map(|(_, _, id)| id));
            match fallback::default_microphone(&candidates, endpoints, &hands_free) {
                Some(endpoint) => {
                    log(format!("first run: no microphone chosen in platinum.ini; relaying Windows' default microphone, {}", endpoint.name));
                    Ok(endpoint)
                }
                None => Err(NO_MICROPHONE.to_string()),
            }
        }
        other => other,
    }
}

/// Logs a problem unless it is the one logged last.
fn say_once(announced: &mut String, message: String) {
    if *announced != message {
        log(&message);
        *announced = message;
    }
}

/// Each device the channels left, once, in order.
fn distinct_sources<'a>(moved: &[(&str, &'a String)]) -> Vec<&'a String> {
    let mut sources: Vec<&String> = Vec::new();
    for (_, from) in moved {
        if !sources.contains(from) {
            sources.push(from);
        }
    }
    sources
}

/// A device for the log: its name and state, or the name the list remembers when it is gone.
fn describe(id: &str, endpoints: &[Endpoint], list: &[fallback::PriorityDevice]) -> String {
    match endpoints.iter().find(|e| e.id == id) {
        Some(endpoint) => format!("{} ({})", endpoint.name, endpoint.state.label()),
        None => match list.iter().find(|e| e.id == id).filter(|e| !e.name.is_empty()) {
            Some(entry) => format!("{} (absent)", entry.name),
            None => format!("{id} (absent)"),
        },
    }
}

/// The real devices that are the hands-free side of a Bluetooth headset (a property read each).
fn hands_free_ids(devices: &Devices, endpoints: &[Endpoint]) -> Vec<String> {
    endpoints.iter().filter(|e| e.state == State::Active && !fallback::is_virtual(e) && devices.is_hands_free(&e.id)).map(|e| e.id.clone()).collect()
}

/// Makes a device the default for every role.
fn make_default(devices: &Devices, endpoint: &Endpoint, why: &str) {
    for role in crate::devices::Role::ALL {
        let _ = devices.set_default(&endpoint.id, role);
    }
    log(format!("default {} {} ({why})", endpoint.flow.label(), endpoint.name));
}

/// Makes the defaults Windows keeps once the daemon is gone. With priority lists, the device the
/// backup switch would choose now: the first usable output of the list, and for the input the
/// chosen microphone, else the first usable one of the list. Otherwise, or when none of the list is
/// plugged in, the fallback devices, when they are given and present.
fn apply_fallbacks(devices: &Devices, options: &Options) {
    let Ok(endpoints) = devices.list() else { return };
    let settings = config::load();
    let hands_free = hands_free_ids(devices, &endpoints);
    let microphone = options.microphone.clone().or_else(|| settings.microphone.clone());
    let from_lists = [
        (!settings.output_priority.is_empty()).then(|| fallback::choose(&settings.output_priority, &endpoints, Flow::Render, &hands_free)),
        (!settings.input_priority.is_empty()).then(|| fallback::microphone(microphone.as_deref(), &settings.input_priority, true, &endpoints, &hands_free).ok()),
    ];
    for ((spec, flow), from_list) in [(&options.fallback_output, Flow::Render), (&options.fallback_input, Flow::Capture)].into_iter().zip(from_lists) {
        match from_list {
            Some(Some(endpoint)) => {
                make_default(devices, endpoint, "first available of the priority list");
                continue;
            }
            Some(None) => log(format!("no device of the {} priority list is available", flow.label())),
            None => {}
        }
        let Some(spec) = spec else { continue };
        let candidates: Vec<_> = endpoints.iter().filter(|e| e.flow == flow).cloned().collect();
        match crate::devices::find_device(&candidates, spec) {
            Ok(endpoint) if endpoint.state == State::Active => make_default(devices, endpoint, "fallback"),
            Ok(endpoint) => log(format!("fallback {} is {}: not used", endpoint.name, endpoint.state.label())),
            Err(error) => log(format!("fallback device: {error}")),
        }
    }
}

/// Gives the channels the names platinum.ini chooses (`names::apply_at_start`), once the choice is
/// written down in a platinum.ini from before `channel_names` existed (`config::settle_channel_names`);
/// quietly: only the daemon's log says it.
fn apply_channel_names(devices: &Devices) {
    match config::settle_channel_names() {
        Ok(Some(choice)) => log(format!("platinum.ini predates channel_names: channel_names = {} written{}", choice.key(), if choice == names::ChannelNames::SteelSeries { " (the channels had gone back to SteelSeries' names)" } else { "" })),
        Ok(None) => {}
        Err(error) => log(format!("channel_names not written in platinum.ini ({error})")),
    }
    match names::apply_at_start(devices, config::load().channel_names) {
        Ok(done) => done.iter().for_each(log),
        Err(error) => log(format!("channel names not checked ({error})")),
    }
}

/// Sleeps 5 s, or less if a stop is requested meanwhile.
fn pause_unless_stopped() {
    for _ in 0..20 {
        if relay::stop_requested() {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Brings the channels up and relays the microphone until a stop is requested.
fn serve(options: &Options, reporter: &Reporter) -> Result<(), String> {
    let devices = Devices::open().map_err(|e| format!("Windows audio devices unavailable: {e}"))?;
    let channels: Vec<&str> = options.channels.iter().map(String::as_str).collect();
    log(format!("bringing up the channels: {}", channels.join(", ")));
    // At logon the audio devices may not be ready yet: keep trying until they are.
    let mut announced = String::new();
    loop {
        match session::start(&devices, &channels) {
            Ok(()) => break,
            Err(error) => {
                if error != announced {
                    log(format!("cannot bring the channels up yet ({error}); retrying every 5 s"));
                    reporter.set(Phase::WaitingDevices, with_detail(mark("Canaux pas encore prêts"), &error.to_string()));
                    announced = error;
                }
                pause_unless_stopped();
                if relay::stop_requested() {
                    return Ok(());
                }
            }
        }
    }
    apply_channel_names(&devices);
    if !channels.contains(&"mic") {
        log("no microphone channel asked for: waiting for a stop request");
        reporter.update(|status| {
            status.state = Phase::Running;
            status.microphone.clear();
            status.effects = false;
            status.message = mark("Canaux actifs, sans micro").into();
        });
        while !relay::stop_requested() {
            std::thread::sleep(Duration::from_millis(250));
        }
        return Ok(());
    }
    reporter.set(Phase::Starting, mark("Canaux actifs, ouverture du micro"));
    // What earlier engines left in the registry (`regclean`).
    clean_stream_events("at start");

    // Turned off for the rest of the run if the effects engine cannot start: the raw microphone
    // is better than a silent one, and the status says it is relayed without effects.
    let mut effects = options.effects;
    let mut announced = String::new();
    let mut host = MicHost::new(options.engine_recycle_mb);
    while !relay::stop_requested() {
        // Looked up again on every attempt: devices come and go while the daemon lives, and the
        // backup switch may have asked for another microphone.
        let attempt = (|| -> Result<Relayed, String> {
            let settings = config::load();
            let endpoints = devices.list().map_err(|e| e.to_string())?;
            let source = pick_microphone(&devices, options, &settings, &endpoints)?;
            let target = session::microphone_feed(&endpoints).ok_or("the virtual microphone is missing")?;
            if target.state != State::Active {
                return Err(format!("{} is {}", target.name, target.state.label()));
            }
            log(format!("relaying {} into {}{}", source.name, target.name, if effects { " through the effects engine" } else { "" }));
            reporter.update(|status| status.microphone = source.name.clone());
            let dll = effects.then(|| crate::sonarfiles::engine_dll().0);
            let args = worker::Args { source: source.id.clone(), target: target.id.clone(), effects_dll: dll.clone(), gain: options.gain, latency_ms: relay::DEFAULT_LATENCY_MS, hold: true, simulate: None };
            reporter.set_relayed(Some(source.id.clone()));
            let relayed = match host.run(&args, reporter) {
                Hosted::Ended(relayed) => relayed,
                Hosted::NoWorker(error) => {
                    log(format!("no mic worker ({error}): the daemon relays the microphone itself this time; the effects engine's memory then only comes back when Platinum restarts"));
                    relay_here(&devices, source, target, dll, options.gain, reporter)
                }
            };
            reporter.set_relayed(None);
            Ok(relayed)
        })();
        let attempt = match attempt {
            Ok(Relayed::Done) => Ok(()),
            Ok(Relayed::EffectsRefused(error)) if effects => {
                log(format!("the effects engine cannot start ({error}); relaying the microphone without effects"));
                reporter.update(|status| {
                    status.state = Phase::Degraded;
                    status.effects = false;
                    status.message = with_detail(mark("Effets du micro indisponibles"), &error);
                });
                effects = false;
                Ok(())
            }
            Ok(Relayed::EffectsRefused(error) | Relayed::Failed(error)) | Err(error) => Err(error),
        };
        match attempt {
            Ok(()) => announced.clear(),
            Err(error) => {
                // Said once per distinct problem, then retried quietly.
                if error != announced {
                    log(format!("cannot relay the microphone yet ({error}); retrying every 5 s"));
                    announced = error.clone();
                }
                let (state, no_microphone, message) = unrelayed_status(&error);
                reporter.update(|status| {
                    status.state = state;
                    status.effects = false;
                    if no_microphone {
                        status.microphone.clear();
                    }
                    status.message = message;
                });
                pause_unless_stopped();
            }
        }
    }
    // Any worker still there ends with the host (and with the job, whatever happens).
    drop(host);
    Ok(())
}

/// What status.txt says while the microphone cannot be relayed (`error`, from an attempt): the
/// state, whether no microphone exists at all (the status then names none), and the message. No
/// microphone at all (none chosen, none plugged in, as in a virtual machine) is "running without a
/// microphone": the channels play, only the virtual microphone stays silent, and a wait would look
/// like a failure. Anything else is a wait, with why.
fn unrelayed_status(error: &str) -> (Phase, bool, String) {
    if error == NO_MICROPHONE {
        (Phase::Running, true, NO_MICROPHONE.to_string())
    } else {
        (Phase::WaitingDevices, false, with_detail(mark("Micro pas encore relayé"), error))
    }
}

/// Deletes the "stream added" and "stream removed" events the effects engine leaves in the
/// registry (`regclean`), and says how many.
fn clean_stream_events(when: &str) {
    match regclean::clean() {
        Ok(regclean::Cleaned::Removed { count, problem }) => {
            log(format!("registry: {count} stream event value{} of the effects engine removed ({when})", plural(count as u64)));
            if let Some(problem) = problem {
                log(format!("registry: {problem}"));
            }
        }
        Ok(regclean::Cleaned::GgRunning) => log(format!("registry: the effects engine's stream events left to GG's Sonar, which runs ({when})")),
        Err(error) => log(format!("registry: the effects engine's stream events not cleaned ({when}): {error}")),
    }
}

/// "s" after a count other than one.
fn plural(count: u64) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// A length of time for the log: "41 s", "12 min 5 s", "1 h 32 min".
fn span(time: Duration) -> String {
    let seconds = time.as_secs();
    match seconds {
        0..60 => format!("{seconds} s"),
        60..3600 => format!("{} min {} s", seconds / 60, seconds % 60),
        _ => format!("{} h {} min", seconds / 3600, seconds % 3600 / 60),
    }
}

/// How one attempt at relaying the microphone ended.
#[derive(Clone, Debug, PartialEq)]
enum Relayed {
    /// Stopped, or ended to start again on another microphone, as asked.
    Done,
    /// The relay could not start: tried again in 5 s.
    Failed(String),
    /// The effects engine could not start, or kept bringing its worker down: raw from now on.
    EffectsRefused(String),
}

/// Whether the microphone could be given to a worker.
enum Hosted {
    Ended(Relayed),
    /// No worker can start, or none keeps running: the daemon relays the microphone itself.
    NoWorker(String),
}

/// The microphone relayed inside the daemon, as before workers existed: only when no worker can
/// start. The watcher publishes what it does (`Reporter::follow_relay`).
fn relay_here(devices: &Devices, source: &Endpoint, target: &Endpoint, dll: Option<PathBuf>, gain: f32, reporter: &Reporter) -> Relayed {
    let config = relay::Config { source, target, gain, latency_ms: relay::DEFAULT_LATENCY_MS, seconds: None, report: None, effects: dll.map(|dll| relay::Effects { dll }), hold: false };
    reporter.relaying.store(true, Ordering::SeqCst);
    let result = relay::run(devices, &config);
    reporter.relaying.store(false, Ordering::SeqCst);
    match result {
        Ok(()) => Relayed::Done,
        Err(error) if relay::effects_refused() => Relayed::EffectsRefused(error),
        Err(error) => Relayed::Failed(error),
    }
}

/// How long a worker may take to open its devices and its engine and say it is ready.
const WORKER_START_LIMIT: Duration = Duration::from_secs(20);
/// A worker silent this long is stuck (it reports its level twice a second).
const WORKER_SILENCE_LIMIT: Duration = Duration::from_secs(5);
/// How long a worker started to take over may take to get ready.
const TAKEOVER_READY_LIMIT: Duration = Duration::from_secs(10);
/// How long a ready worker waits for a silence to take over before it is let go; the next silence
/// starts another (the half hour after which the old one is replaced anyway still counts).
const READY_WAIT_LIMIT: Duration = Duration::from_secs(60);
/// How long the old worker may take to close its streams before it is ended outright and the new
/// one sends anyway.
const HANDOVER_LIMIT: Duration = Duration::from_millis(300);
/// How long the new worker may take to send once told to.
const GO_LIMIT: Duration = Duration::from_secs(5);
/// How long a worker told to stop may take to end before it is ended outright: after a
/// replacement, and when the daemon stops (shorter: the defaults wait to be put back).
const RETIRE_LIMIT: Duration = Duration::from_secs(2);
const STOP_LIMIT: Duration = Duration::from_secs(1);
/// How often the worker's memory is looked at.
const WORKER_MEMORY_PERIOD: Duration = Duration::from_secs(2);
/// How long the daemon waits for an ended worker's last lines before it decides what the end was.
const OUTPUT_GRACE: Duration = Duration::from_millis(500);
/// A level report older than this says nothing about the silence now.
const LEVEL_FRESH: Duration = Duration::from_millis(1500);
/// Status message while a worker that ended on its own is being replaced.
const RESTARTING: &str = mark("Micro interrompu, redémarrage");

/// What the daemon knows of a worker from its reports.
#[derive(Clone, Debug, PartialEq)]
struct Heard {
    activity: relay::Activity,
    engine: bool,
    problem: String,
    /// The last level report: when it came, and how long the microphone had been quiet then.
    quiet: Option<(Instant, Duration)>,
    ready: bool,
    /// When it first sent the microphone.
    streaming_since: Option<Instant>,
    stopped: bool,
    stalled: bool,
    refused: Option<String>,
    failed: Option<String>,
}

impl Heard {
    fn new() -> Heard {
        Heard { activity: relay::Activity::Idle, engine: false, problem: String::new(), quiet: None, ready: false, streaming_since: None, stopped: false, stalled: false, refused: None, failed: None }
    }

    /// Takes one report heard at `now`; true when what status.txt shows of the worker may change.
    fn hear(&mut self, report: Report, now: Instant) -> bool {
        match report {
            Report::Ready => {
                self.ready = true;
                self.activity = relay::Activity::Ready;
                false
            }
            Report::Streaming { effects } => {
                self.streaming_since.get_or_insert(now);
                self.activity = relay::Activity::Streaming { effects };
                true
            }
            Report::Interrupted => {
                self.activity = relay::Activity::Interrupted;
                true
            }
            Report::Problem(text) => {
                self.problem = text;
                true
            }
            Report::Engine(on) => {
                self.engine = on;
                false
            }
            Report::Level { quiet, .. } => {
                self.quiet = Some((now, quiet));
                false
            }
            Report::Stalled => {
                self.stalled = true;
                false
            }
            Report::Stopped => {
                self.stopped = true;
                false
            }
            Report::Refused(text) => {
                self.refused = Some(text);
                false
            }
            Report::Failed(text) => {
                self.failed = Some(text);
                false
            }
            Report::Done => false,
        }
    }

    /// How long the microphone has been quiet, from a fresh level report; zero otherwise (an
    /// unknown silence never counts as one). Not stretched past the report: the user may have
    /// spoken since.
    fn quiet_for(&self, now: Instant) -> Duration {
        match self.quiet {
            Some((at, quiet)) if now.saturating_duration_since(at) <= LEVEL_FRESH => quiet,
            _ => Duration::ZERO,
        }
    }
}

/// A worker process and what the daemon knows of it.
struct Tracked {
    worker: Worker,
    heard: Heard,
    /// When it was last heard from (any line).
    last_heard: Instant,
    go_sent: bool,
    /// Why the daemon ended it, if it did.
    ended_by_daemon: Option<String>,
    /// Its output has closed: every line it printed has been heard.
    closed: bool,
    /// When the daemon first saw that it had ended.
    end_seen: Option<Instant>,
}

impl Tracked {
    fn new(worker: Worker) -> Tracked {
        Tracked { worker, heard: Heard::new(), last_heard: Instant::now(), go_sent: false, ended_by_daemon: None, closed: false, end_seen: None }
    }

    fn pid(&self) -> u32 {
        self.worker.pid()
    }

    /// Its exit code once it has ended and its last lines have been heard (its output closed, or
    /// `OUTPUT_GRACE` went by): a worker says why it could not start just before it ends, and that
    /// decides what the daemon does next. `None` while it runs, or while its last lines may be on
    /// their way.
    fn finished(&mut self, now: Instant) -> Option<Option<i32>> {
        let code = self.worker.ended()?;
        let seen = *self.end_seen.get_or_insert(now);
        (self.closed || now.saturating_duration_since(seen) >= OUTPUT_GRACE).then_some(code)
    }

    /// Why it ended, for the log.
    fn why_ended(&self, code: Option<i32>) -> String {
        let reason = self.ended_by_daemon.clone().or_else(|| self.heard.failed.clone()).or_else(|| self.heard.refused.clone()).or_else(|| (!self.heard.problem.is_empty()).then(|| self.heard.problem.clone()));
        match reason {
            Some(reason) => format!("{reason}; {}", worker::exit_text(code)),
            None => worker::exit_text(code),
        }
    }

    /// Why a worker that should be getting ready or taking over can no longer: it ended, refused,
    /// failed, is stuck or silent, or (`ready_by`) was not ready in time.
    fn unfit(&mut self, now: Instant, ready_by: Option<Instant>) -> Option<String> {
        if let Some(code) = self.finished(now) {
            note_worker_crash(self, code, "ended while getting ready to take over");
            return Some(format!("ended ({})", self.why_ended(code)));
        }
        if let Some(error) = self.heard.refused.clone().or_else(|| self.heard.failed.clone()) {
            return Some(format!("could not start ({error})"));
        }
        if self.heard.stalled {
            return Some("has its audio thread stuck".into());
        }
        if now.saturating_duration_since(self.last_heard) >= WORKER_SILENCE_LIMIT {
            return Some(format!("said nothing for {} s", WORKER_SILENCE_LIMIT.as_secs()));
        }
        if !self.heard.ready && ready_by.is_some_and(|by| now >= by) {
            return Some(format!("was not ready in {} s", TAKEOVER_READY_LIMIT.as_secs()));
        }
        None
    }

    /// Tells it to stop and closes its input: it ends on its own, or by `deadline` outright.
    fn retire(mut self, deadline: Instant) -> (Tracked, Instant) {
        self.worker.send(Command::Stop);
        self.worker.close_input();
        (self, deadline)
    }
}

/// What the daemon saw of a worker that crashed, for its crash report: `None` when the end was no
/// crash (an exit the worker chose, or one the daemon caused by ending it outright, stuck or not
/// ready in time).
fn worker_crash_lines(pid: u32, code: Option<i32>, ended_by_daemon: bool, context: &str, ran: Duration, heard: &Heard) -> Option<Vec<String>> {
    if ended_by_daemon || !crash::is_crash_code(code) {
        return None;
    }
    let mut lines = vec![format!("mic worker {pid} {context} after {}, {}", span(ran), crash::exit_description(code))];
    let state = match heard.activity {
        relay::Activity::Streaming { effects: true } => "relaying the microphone through the effects engine",
        relay::Activity::Streaming { effects: false } => "relaying the microphone without effects",
        relay::Activity::Interrupted => "waiting for its devices",
        relay::Activity::Ready => "ready, held",
        relay::Activity::Idle => "starting",
    };
    lines.push(format!("it was {state}{}", if heard.engine { ", effects engine loaded" } else { "" }));
    for (what, text) in [("last problem", Some(&heard.problem).filter(|p| !p.is_empty())), ("failed", heard.failed.as_ref()), ("refused", heard.refused.as_ref())] {
        if let Some(text) = text {
            lines.push(format!("{what}: {text}"));
        }
    }
    Some(lines)
}

/// Writes the crash report of a worker that crashed (`worker_crash_lines`): what the daemon saw is
/// added to the worker's own report when it wrote one (a panic, an exception it caught), else
/// recorded as one, with the daemon's log.
fn note_worker_crash(tracked: &Tracked, code: Option<i32>, context: &str) {
    let Some(seen) = worker_crash_lines(tracked.pid(), code, tracked.ended_by_daemon.is_some(), context, tracked.worker.age(), &tracked.heard) else { return };
    let written = match crash::recent_report_of(crash::WORKER, tracked.pid(), Duration::from_secs(120)) {
        Some(file) => crash::append(&file, "seen by the daemon", &seen).map(|()| file),
        None => {
            let reason = format!("mic worker ended unexpectedly, {}", crash::exit_description(code));
            crash::record(crash::WORKER, tracked.pid(), reason, seen, crash::default_log(crash::DAEMON).as_deref())
        }
    };
    match written {
        Ok(file) => log(format!("crash report of mic worker {}: {}", tracked.pid(), file.display())),
        Err(error) => log(format!("crash report of mic worker {} not written: {error}", tracked.pid())),
    }
}

/// Where a replacement of the worker stands. `moment` says why it was started, `from_mb` the old
/// worker's private memory then, `quiet` how long the microphone had been quiet at the handover.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Swap {
    Idle,
    /// A new worker is getting ready, held.
    Preparing { since: Instant, moment: Moment, from_mb: f64 },
    /// It is ready and waits for a silence.
    Waiting { since: Instant, moment: Moment, from_mb: f64 },
    /// The old one was told to stop at `stop_sent`; the new one goes once it has closed its streams.
    Handing { stop_sent: Instant, old: u32, moment: Moment, from_mb: f64, quiet: Duration },
    /// The new one was told to go at `go_sent`.
    Going { stop_sent: Instant, go_sent: Instant, old: u32, moment: Moment, from_mb: f64, quiet: Duration },
}

/// The microphone's worker processes: starts the one that relays, follows it, starts it again if
/// it ends on its own, and replaces it when it has grown (`recycle::Replacement`). Lives as long as
/// the daemon serves: its job object ends every worker with the daemon, however the daemon ends.
struct MicHost {
    exe: Option<PathBuf>,
    job: Option<Job>,
    sender: Sender<(u32, Event)>,
    events: Receiver<(u32, Event)>,
    /// The worker whose sound the virtual microphone carries (or will, once told to go).
    active: Option<Tracked>,
    /// A worker getting ready to take over, or taking over.
    candidate: Option<Tracked>,
    /// Workers told to stop, each ended outright at its deadline if still there.
    retiring: Vec<(Tracked, Instant)>,
    swap: Swap,
    policy: Replacement,
    restarts: Restarts,
    /// When to start a worker again after the last one ended on its own.
    restart_at: Option<Instant>,
    next_memory: Instant,
    /// A replacement happened: the registry is cleaned once the old worker has ended.
    cleanup_due: bool,
    /// The job object could not take a worker: said once.
    job_said: bool,
}

impl MicHost {
    fn new(recycle_mb: u32) -> MicHost {
        let (sender, events) = std::sync::mpsc::channel();
        let job = Job::new().map_err(|error| log(format!("mic worker: {error}; a worker still ends when the daemon's end of its input closes"))).ok();
        MicHost {
            exe: std::env::current_exe().ok(),
            job,
            sender,
            events,
            active: None,
            candidate: None,
            retiring: Vec::new(),
            swap: Swap::Idle,
            policy: Replacement::new(recycle_mb),
            restarts: Restarts::default(),
            restart_at: None,
            next_memory: Instant::now(),
            cleanup_due: false,
            job_said: false,
        }
    }

    fn spawn(&mut self, args: &worker::Args) -> Result<Tracked, String> {
        let exe = self.exe.as_deref().ok_or("the daemon's own program cannot be found")?;
        let (worker, unjobbed) = Worker::spawn(exe, args, self.job.as_ref(), self.sender.clone())?;
        if let Some(problem) = unjobbed.filter(|_| !self.job_said) {
            log(format!("mic worker {}: {problem}; it still ends when the daemon's end of its input closes", worker.pid()));
            self.job_said = true;
        }
        Ok(Tracked::new(worker))
    }

    fn start_active(&mut self, args: &worker::Args, reporter: &Reporter) -> Result<(), String> {
        let tracked = self.spawn(args)?;
        reporter.set_worker(tracked.pid());
        self.active = Some(tracked);
        Ok(())
    }

    /// Relays the microphone through workers until a stop or a restart is asked, or until the
    /// relay cannot go on with these arguments. Every worker is gone when it returns.
    fn run(&mut self, args: &worker::Args, reporter: &Reporter) -> Hosted {
        // A restart asked before this attempt began was meant for the previous one.
        relay::take_restart();
        self.restart_at = None;
        if let Err(error) = self.start_active(args, reporter) {
            return Hosted::NoWorker(error);
        }
        let ending = loop {
            // Quick while the microphone changes hands, at ease otherwise: reports wake it anyway.
            let wait = if matches!(self.swap, Swap::Handing { .. } | Swap::Going { .. }) { Duration::from_millis(5) } else { Duration::from_millis(200) };
            if let Ok(first) = self.events.recv_timeout(wait) {
                self.hear(first, reporter);
                while let Ok(next) = self.events.try_recv() {
                    self.hear(next, reporter);
                }
            }
            let now = Instant::now();
            if relay::stop_requested() || relay::take_restart() {
                break Hosted::Ended(Relayed::Done);
            }
            self.reap(now);
            if let Err(ending) = self.follow_active(args, now, reporter) {
                break ending;
            }
            self.follow_swap(now, reporter);
            if now >= self.next_memory {
                self.next_memory = now + WORKER_MEMORY_PERIOD;
                self.look_at_memory(args, now);
            }
        };
        self.stop_all(reporter);
        ending
    }

    fn find_mut(&mut self, pid: u32) -> Option<&mut Tracked> {
        self.active.iter_mut().chain(self.candidate.iter_mut()).chain(self.retiring.iter_mut().map(|(tracked, _)| tracked)).find(|tracked| tracked.pid() == pid)
    }

    /// One line from a worker: its other lines go to the log, its reports update what the daemon
    /// knows, and the status follows the worker that relays.
    fn hear(&mut self, (pid, event): (u32, Event), reporter: &Reporter) {
        let now = Instant::now();
        let relaying = self.active.as_ref().is_some_and(|active| active.pid() == pid);
        if let Some(tracked) = self.find_mut(pid) {
            tracked.last_heard = now;
            tracked.closed |= event == Event::Closed;
            if let Event::Report(report) = &event {
                let first_stream = matches!(report, Report::Streaming { .. }) && tracked.heard.streaming_since.is_none();
                if tracked.heard.hear(report.clone(), now) && relaying {
                    reporter.show_relay(tracked.heard.activity, &tracked.heard.problem);
                }
                if first_stream && relaying {
                    log(format!("mic worker {pid} relays the microphone{}, {} ms after it started", if matches!(report, Report::Streaming { effects: true }) { " through the effects engine" } else { "" }, tracked.worker.age().as_millis()));
                }
            }
        }
        if let Event::Text(line) = &event {
            if !line.trim().is_empty() {
                log(format!("mic worker {pid}: {}", worker::strip_stamp(line)));
            }
        }
        let engine = self.active.iter().chain(self.candidate.iter()).any(|tracked| tracked.heard.engine);
        reporter.worker_engine.store(engine, Ordering::SeqCst);
    }

    /// Ends outright the workers told to stop that are still there at their deadline, forgets
    /// those that ended, then cleans the registry when a replacement's old worker is gone.
    fn reap(&mut self, now: Instant) {
        for (tracked, deadline) in &mut self.retiring {
            if now >= *deadline && tracked.worker.ended().is_none() {
                log(format!("mic worker {} did not end in time after being told to stop; ended outright", tracked.pid()));
                tracked.worker.kill();
            }
        }
        self.retiring.retain_mut(|(tracked, _)| tracked.worker.ended().is_none());
        if self.cleanup_due && self.retiring.is_empty() && !matches!(self.swap, Swap::Handing { .. } | Swap::Going { .. }) {
            self.cleanup_due = false;
            clean_stream_events("after the replacement");
        }
    }

    /// The worker that relays: tells it to go once ready, notices that it ended or is stuck, and
    /// starts the next one when it is time. `Err` when this attempt cannot go on.
    fn follow_active(&mut self, args: &worker::Args, now: Instant, reporter: &Reporter) -> Result<(), Hosted> {
        let Some(active) = self.active.as_mut() else {
            // During a handover the new worker is on its way; after an end, the next one starts
            // when its time comes.
            if !matches!(self.swap, Swap::Handing { .. } | Swap::Going { .. }) && self.restart_at.is_some_and(|at| now >= at) {
                self.restart_at = None;
                self.start_active(args, reporter).map_err(Hosted::NoWorker)?;
            }
            return Ok(());
        };
        if active.heard.ready && !active.go_sent {
            active.go_sent = true;
            active.worker.send(Command::Go);
        }
        if let Some(code) = active.finished(now) {
            let Some(ended) = self.active.take() else { return Ok(()) };
            reporter.set_worker(0);
            reporter.worker_engine.store(self.candidate.as_ref().is_some_and(|c| c.heard.engine), Ordering::SeqCst);
            return self.active_ended(ended, code, args, now, reporter);
        }
        if active.heard.streaming_since.is_some_and(|since| now.saturating_duration_since(since) >= recycle::STEADY) && self.restarts.streak() > 0 {
            let ends = self.restarts.steady();
            log(format!("mic worker {} relays steadily again, after {ends} unexpected end{}", active.pid(), plural(u64::from(ends))));
        }
        let stuck = if active.heard.stalled {
            Some("its audio thread is stuck".to_string())
        } else if now.saturating_duration_since(active.last_heard) >= WORKER_SILENCE_LIMIT {
            Some(format!("it said nothing for {} s", WORKER_SILENCE_LIMIT.as_secs()))
        } else if !active.heard.ready && active.worker.age() >= WORKER_START_LIMIT {
            Some(format!("it was not ready in {} s", WORKER_START_LIMIT.as_secs()))
        } else {
            None
        };
        // One that has ended already is only waiting for its last lines to be heard.
        if let Some(why) = stuck.filter(|_| active.ended_by_daemon.is_none() && active.worker.ended().is_none()) {
            log(format!("mic worker {}: {why}; ending it", active.pid()));
            active.ended_by_daemon = Some(why);
            active.worker.kill();
        }
        Ok(())
    }

    /// The worker that relayed has ended. One that never relayed and said why is the relay's own
    /// first failure (tried again in 5 s, or raw when the engine refused); any other end is
    /// unexpected (a crash, Task Manager, stuck): the next worker starts at once, then with growing
    /// waits while they keep ending, and after `CRASHES_BEFORE_RAW` in a row the effects engine is
    /// left out, then the worker itself.
    fn active_ended(&mut self, ended: Tracked, code: Option<i32>, args: &worker::Args, now: Instant, reporter: &Reporter) -> Result<(), Hosted> {
        if !matches!(self.swap, Swap::Idle) {
            self.abandon_swap("the worker it was to replace ended");
        }
        if ended.heard.streaming_since.is_none() && ended.ended_by_daemon.is_none() {
            if let Some(error) = &ended.heard.refused {
                return Err(Hosted::Ended(Relayed::EffectsRefused(error.clone())));
            }
            if let Some(error) = &ended.heard.failed {
                return Err(Hosted::Ended(Relayed::Failed(error.clone())));
            }
        }
        note_worker_crash(&ended, code, "ended unexpectedly");
        let why = ended.why_ended(code);
        let ran = ended.worker.age();
        let delay = self.restarts.ended(ran);
        let streak = self.restarts.streak();
        if streak >= recycle::CRASHES_BEFORE_RAW {
            let summary = format!("the mic worker ended {streak} times in a row, last {why}");
            return Err(if args.effects_dll.is_some() { Hosted::Ended(Relayed::EffectsRefused(summary)) } else { Hosted::NoWorker(summary) });
        }
        match streak {
            1 => log(format!("mic worker {} ended unexpectedly after {} ({why}); starting another now", ended.pid(), span(ran))),
            _ => log(format!("mic worker {} ended again after {} ({why}); the next one starts in {} s", ended.pid(), span(ran), delay.as_secs())),
        }
        reporter.update(|status| {
            status.state = Phase::WaitingDevices;
            status.effects = false;
            status.message = RESTARTING.to_string();
        });
        self.restart_at = Some(now + delay);
        Ok(())
    }

    /// Looks at the relaying worker's memory every `WORKER_MEMORY_PERIOD`, and starts a new worker
    /// to take over when the policy says so.
    fn look_at_memory(&mut self, args: &worker::Args, now: Instant) {
        if !matches!(self.swap, Swap::Idle) {
            return;
        }
        let Some(active) = &self.active else { return };
        if !matches!(active.heard.activity, relay::Activity::Streaming { .. }) {
            return;
        }
        let pid = active.pid();
        let Some((private, _)) = crate::guard::process_memory(pid) else { return };
        let mb = crate::guard::megabytes(private);
        let age = active.worker.age();
        match self.policy.decide(now, age, mb, active.heard.quiet_for(now)) {
            Decision::Keep | Decision::Wait { first: false } => {}
            Decision::Wait { first: true } => log(format!(
                "mic worker {pid} at {mb:.0} MB after {}, over engine_recycle_mb = {}: replaced at the next {} s silence of the microphone, in {} min at the latest",
                span(age),
                self.policy.threshold_mb(),
                recycle::QUIET_FOR.as_secs(),
                recycle::LONGEST_WAIT.as_secs() / 60
            )),
            Decision::Replace(moment) => match self.spawn(args) {
                Ok(candidate) => {
                    self.candidate = Some(candidate);
                    self.swap = Swap::Preparing { since: now, moment, from_mb: mb };
                }
                Err(error) => {
                    let wait = self.policy.failed(now);
                    log(format!("mic worker replacement not started ({error}); pid {pid} goes on, next try in {} min", wait.as_secs() / 60));
                }
            },
        }
    }

    /// Moves a replacement on: the new worker gets ready, waits for a silence, the old one stops,
    /// the new one goes; or the replacement fails and the old one goes on.
    fn follow_swap(&mut self, now: Instant, reporter: &Reporter) {
        match self.swap {
            Swap::Idle => {}
            Swap::Preparing { since, moment, from_mb } | Swap::Waiting { since, moment, from_mb } => {
                let preparing = matches!(self.swap, Swap::Preparing { .. });
                let Some(candidate) = self.candidate.as_mut() else {
                    self.swap = Swap::Idle;
                    return;
                };
                if let Some(why) = candidate.unfit(now, preparing.then(|| since + TAKEOVER_READY_LIMIT)) {
                    self.swap_failed(now, &why);
                    return;
                }
                if !candidate.heard.ready {
                    return;
                }
                let quiet = self.active.as_ref().map_or(Duration::ZERO, |active| active.heard.quiet_for(now));
                if moment == Moment::WaitedTooLong || quiet >= recycle::QUIET_FOR {
                    self.hand_over(now, moment, from_mb, quiet);
                } else if preparing {
                    self.swap = Swap::Waiting { since: now, moment, from_mb };
                } else if now.saturating_duration_since(since) >= READY_WAIT_LIMIT {
                    let pid = candidate.pid();
                    log(format!("mic worker {pid} was ready to take over but the microphone found no {} s silence in {} s: let go, another starts at the next silence", recycle::QUIET_FOR.as_secs(), READY_WAIT_LIMIT.as_secs()));
                    if let Some(candidate) = self.candidate.take() {
                        self.retiring.push(candidate.retire(now + RETIRE_LIMIT));
                    }
                    self.swap = Swap::Idle;
                }
            }
            Swap::Handing { stop_sent, old, moment, from_mb, quiet } => {
                if let Some(why) = self.candidate.as_mut().and_then(|candidate| candidate.unfit(now, None)) {
                    self.takeover_failed(&why);
                    return;
                }
                let old_done = self.retiring.iter_mut().find(|(tracked, _)| tracked.pid() == old).is_none_or(|(tracked, _)| tracked.heard.stopped || tracked.worker.ended().is_some());
                if !old_done && now.saturating_duration_since(stop_sent) < HANDOVER_LIMIT {
                    return;
                }
                if !old_done {
                    if let Some((tracked, _)) = self.retiring.iter_mut().find(|(tracked, _)| tracked.pid() == old) {
                        log(format!("mic worker {old} did not close its streams within {} ms of being told to stop; ended outright", HANDOVER_LIMIT.as_millis()));
                        tracked.worker.kill();
                    }
                }
                if let Some(candidate) = self.candidate.as_mut() {
                    candidate.go_sent = true;
                    candidate.worker.send(Command::Go);
                }
                self.swap = Swap::Going { stop_sent, go_sent: now, old, moment, from_mb, quiet };
            }
            Swap::Going { stop_sent, go_sent, old, moment, from_mb, quiet } => {
                let Some(candidate) = self.candidate.as_mut() else {
                    self.swap = Swap::Idle;
                    return;
                };
                if let Some(streaming) = candidate.heard.streaming_since {
                    self.took_over(streaming.saturating_duration_since(stop_sent), old, moment, from_mb, quiet, reporter);
                    return;
                }
                let why = candidate.unfit(now, None).or_else(|| (now.saturating_duration_since(go_sent) >= GO_LIMIT).then(|| format!("did not send within {} s of being told to", GO_LIMIT.as_secs())));
                if let Some(why) = why {
                    self.takeover_failed(&why);
                }
            }
        }
    }

    /// The silence has come (or waited too long): the old worker stops; the new one goes once the
    /// old one has closed its streams (`Swap::Handing`).
    fn hand_over(&mut self, now: Instant, moment: Moment, from_mb: f64, quiet: Duration) {
        let Some(old) = self.active.take() else { return };
        let pid = old.pid();
        self.retiring.push(old.retire(now + RETIRE_LIMIT));
        self.swap = Swap::Handing { stop_sent: now, old: pid, moment, from_mb, quiet };
    }

    /// The new worker sends: it relays from now on.
    fn took_over(&mut self, gap: Duration, old: u32, moment: Moment, from_mb: f64, quiet: Duration, reporter: &Reporter) {
        let Some(new) = self.candidate.take() else { return };
        let pid = new.pid();
        let when = match moment {
            Moment::Quiet => format!("silence {:.1} s", quiet.as_secs_f64()),
            Moment::WaitedTooLong => format!("no silence in {} min, replaced anyway", recycle::LONGEST_WAIT.as_secs() / 60),
        };
        log(format!("mic worker replaced at {from_mb:.0} MB (pid {old} → {pid}), gap {} ms, {when}", gap.as_millis()));
        reporter.set_worker(pid);
        reporter.update(|status| status.replaced = format!("{}, {from_mb:.0} MB, gap {} ms", crate::guard::stamp(), gap.as_millis()));
        reporter.show_relay(new.heard.activity, &new.heard.problem);
        self.active = Some(new);
        self.policy.replaced();
        self.cleanup_due = true;
        self.swap = Swap::Idle;
    }

    /// The new worker could not get ready: ended, and the old one goes on; the next try waits
    /// (`Replacement::failed`).
    fn swap_failed(&mut self, now: Instant, why: &str) {
        let pid = self.candidate.as_ref().map_or(0, Tracked::pid);
        if let Some(mut candidate) = self.candidate.take() {
            candidate.worker.kill();
        }
        let wait = self.policy.failed(now);
        let old = self.active.as_ref().map_or(0, Tracked::pid);
        log(format!("mic worker replacement failed: the new worker {pid} {why}; pid {old} goes on, next try in {} min", wait.as_secs() / 60));
        self.swap = Swap::Idle;
    }

    /// The new worker failed once the old one was told to stop: there is no worker left to keep, so
    /// it becomes the one that relays, ended, and `follow_active` starts another as after any
    /// unexpected end.
    fn takeover_failed(&mut self, why: &str) {
        if let Some(mut candidate) = self.candidate.take() {
            log(format!("the new mic worker {} could not take over: it {why}", candidate.pid()));
            candidate.ended_by_daemon = Some(format!("it could not take over: it {why}"));
            candidate.worker.kill();
            self.active = Some(candidate);
        }
        self.swap = Swap::Idle;
    }

    /// The relaying worker ended while a replacement was getting ready: the replacement goes too
    /// (the end is handled as any other), without counting as a failed replacement.
    fn abandon_swap(&mut self, why: &str) {
        if let Some(mut candidate) = self.candidate.take() {
            log(format!("mic worker replacement abandoned: {why}; worker {} ended", candidate.pid()));
            candidate.worker.kill();
        }
        self.swap = Swap::Idle;
    }

    /// Tells every worker to stop, waits for them at most `STOP_LIMIT`, then ends those left.
    fn stop_all(&mut self, reporter: &Reporter) {
        let deadline = Instant::now() + STOP_LIMIT;
        for tracked in self.active.take().into_iter().chain(self.candidate.take()) {
            self.retiring.push(tracked.retire(deadline));
        }
        for (_, until) in &mut self.retiring {
            *until = (*until).min(deadline);
        }
        self.swap = Swap::Idle;
        self.restart_at = None;
        while !self.retiring.is_empty() {
            while let Ok(event) = self.events.try_recv() {
                self.hear(event, reporter);
            }
            self.reap(Instant::now());
            if !self.retiring.is_empty() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        reporter.set_worker(0);
        reporter.worker_engine.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_microphone_at_all_is_running_without_one() {
        // A virtual machine without a microphone: the sound goes through, only the mic is silent.
        let (state, no_microphone, message) = unrelayed_status(NO_MICROPHONE);
        assert_eq!((state, no_microphone, message.as_str()), (Phase::Running, true, NO_MICROPHONE));
        let mut status = Status::new(state);
        status.message = message;
        assert_eq!(status.summary(), "Son actif, sans micro");
        assert_eq!(status.tooltip(), "Platinum\nSon actif, sans micro");
        // A microphone that is there but cannot be opened yet is a wait, with why.
        let (state, no_microphone, message) = unrelayed_status("Microphone (Seiren) is disabled");
        assert_eq!((state, no_microphone, message.as_str()), (Phase::WaitingDevices, false, "Micro pas encore relayé : Microphone (Seiren) is disabled"));
    }

    #[test]
    fn only_a_crash_the_daemon_did_not_cause_makes_a_report() {
        let mut heard = Heard::new();
        heard.activity = relay::Activity::Streaming { effects: true };
        heard.engine = true;
        heard.problem = "capture device lost".into();
        let crashed = Some(0xC000_0005_u32 as i32);
        let lines = worker_crash_lines(812, crashed, false, "ended unexpectedly", Duration::from_secs(4000), &heard).unwrap();
        assert_eq!(lines[0], "mic worker 812 ended unexpectedly after 1 h 6 min, exit code 0xC0000005 (access violation)");
        assert_eq!(lines[1], "it was relaying the microphone through the effects engine, effects engine loaded");
        assert_eq!(lines[2], "last problem: capture device lost");
        // Its own exits (stopped, relay failed, engine refused) and the daemon's own kills are not crashes.
        for code in [Some(0), Some(1), Some(2), None] {
            assert!(worker_crash_lines(812, code, false, "ended", Duration::ZERO, &heard).is_none(), "{code:?}");
        }
        assert!(worker_crash_lines(812, crashed, true, "ended", Duration::ZERO, &heard).is_none());
    }

    #[test]
    fn switches_are_told_in_a_sentence() {
        let (airpods, screen) = ("airpods".to_string(), "screen".to_string());
        assert_eq!(distinct_sources(&[("Gaming", &airpods), ("Chat", &screen), ("Aux", &airpods)]), [&airpods, &screen]);
        let list = [fallback::PriorityDevice::new("airpods", "Casque (AirPods Pro)")];
        assert_eq!(describe("airpods", &[], &list), "Casque (AirPods Pro) (absent)");
        assert_eq!(describe("gone", &[], &list), "gone (absent)");
    }

    #[test]
    fn the_status_follows_the_relay_wherever_it_runs() {
        use relay::Activity;
        let switch = "Bascule de secours : Gaming → Arena 7";
        // Nothing to say before it streams, nor while a worker is held ready.
        assert_eq!(relay_status(Activity::Idle, true, "", switch), None);
        assert_eq!(relay_status(Activity::Ready, true, "", switch), None);
        assert_eq!(relay_status(Activity::Streaming { effects: true }, true, "old problem", switch), Some((Phase::Running, true, switch.to_string())));
        // Raw as asked is running; raw although effects were asked is degraded, with the problem.
        assert_eq!(relay_status(Activity::Streaming { effects: false }, false, "", ""), Some((Phase::Running, false, String::new())));
        assert_eq!(relay_status(Activity::Streaming { effects: false }, true, "effects engine failed (x)", switch), Some((Phase::Degraded, false, "Effets du micro indisponibles : effects engine failed (x)".to_string())));
        assert_eq!(relay_status(Activity::Interrupted, true, "", switch), Some((Phase::WaitingDevices, false, "Micro interrompu, en attente de ses périphériques".to_string())));
        assert_eq!(relay_status(Activity::Interrupted, true, "device lost", switch).map(|(_, _, m)| m), Some("Micro interrompu, en attente de ses périphériques : device lost".to_string()));
    }

    #[test]
    fn a_workers_reports_tell_where_it_stands() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut heard = Heard::new();
        assert!(!heard.hear(Report::Engine(true), at(10)) && heard.engine);
        assert!(!heard.hear(Report::Ready, at(400)) && heard.ready && heard.activity == relay::Activity::Ready);
        // Streaming changes what the status says, and is remembered from the first time.
        assert!(heard.hear(Report::Streaming { effects: true }, at(450)));
        assert!(heard.hear(Report::Problem("device lost".into()), at(9000)));
        assert!(heard.hear(Report::Interrupted, at(9000)));
        assert!(heard.hear(Report::Streaming { effects: true }, at(12_000)));
        assert_eq!(heard.streaming_since, Some(at(450)));
        assert_eq!((heard.activity, heard.problem.as_str()), (relay::Activity::Streaming { effects: true }, "device lost"));
        // The microphone's silence, from a fresh report only, and never stretched past it.
        assert_eq!(heard.quiet_for(at(13_000)), Duration::ZERO);
        assert!(!heard.hear(Report::Level { peak: 0.001, quiet: Duration::from_millis(2600) }, at(13_000)));
        assert_eq!(heard.quiet_for(at(13_400)), Duration::from_millis(2600));
        assert_eq!(heard.quiet_for(at(13_000) + LEVEL_FRESH), Duration::from_millis(2600));
        assert_eq!(heard.quiet_for(at(14_501)), Duration::ZERO);
        // The end of a run.
        for report in [Report::Stalled, Report::Stopped, Report::Engine(false), Report::Done] {
            assert!(!heard.hear(report, at(20_000)));
        }
        assert!(heard.stalled && heard.stopped && !heard.engine);
        let mut refused = Heard::new();
        refused.hear(Report::Refused("class not registered".into()), at(0));
        refused.hear(Report::Failed("x".into()), at(0));
        assert_eq!((refused.refused.as_deref(), refused.failed.as_deref(), refused.streaming_since), (Some("class not registered"), Some("x"), None));
    }

    #[test]
    fn lengths_of_time_read_naturally_in_the_log() {
        assert_eq!(span(Duration::from_millis(41_900)), "41 s");
        assert_eq!(span(Duration::from_secs(725)), "12 min 5 s");
        assert_eq!(span(Duration::from_secs(5520)), "1 h 32 min");
        assert_eq!((plural(1), plural(0), plural(8)), ("", "s", "s"));
        assert_eq!(worker_text(false, 1234), "none (the daemon does not run)");
        assert!(worker_text(true, 0).starts_with("none"));
        // This test program stands in for a worker: its memory can be read.
        assert!(worker_text(true, std::process::id()).starts_with(&format!("pid {}, private ", std::process::id())));
    }

    #[test]
    fn memory_is_logged_hourly_and_at_each_threshold_once() {
        let start = Instant::now();
        let mut watch = MemoryWatch::new(start);
        let at = |minutes: u64| start + Duration::from_secs(minutes * 60);
        assert_eq!(watch.due(at(1), 40.0), None);
        assert_eq!(watch.due(at(10), 120.0).as_deref(), Some("crossed 100 MB"));
        // Crossed once: staying above it says nothing more, nor does falling back and rising again.
        assert_eq!(watch.due(at(11), 150.0), None);
        assert_eq!(watch.due(at(12), 80.0), None);
        assert_eq!(watch.due(at(13), 130.0), None);
        // The hour is counted from the start, whatever the thresholds did.
        assert_eq!(watch.due(at(60), 130.0).as_deref(), Some("hourly"));
        assert_eq!(watch.due(at(61), 130.0), None);
        // A jump over two thresholds is one line, for the highest; the lower one is not said later.
        assert_eq!(watch.due(at(70), 450.0).as_deref(), Some("crossed 400 MB"));
        assert_eq!(watch.due(at(71), 250.0), None);
        // Hours missed while the machine slept make one line, then the next comes an hour later.
        assert_eq!(watch.due(at(400), 250.0).as_deref(), Some("hourly"));
        assert_eq!(watch.due(at(401), 250.0), None);
        assert_eq!(watch.due(at(460), 250.0).as_deref(), Some("hourly"));
    }
}
