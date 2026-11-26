//! The microphone worker: the relay and the vendor's microphone effects engine, in a process of
//! their own that the daemon starts, follows and replaces.
//!
//! Why a process: the engine (`Sonar.APO.dll`) gathers memory in its global state while echo
//! cancellation has references, about 4 MB a minute (17–18/11/2026), and the DLL can never be
//! unloaded; rebuilding the engine in the same process gave almost nothing back. Ending the process
//! gives everything back. So the daemon runs the relay here (`platinum mic-worker …`, hidden from
//! the help), and replaces the worker during a silence of the user once it has grown (`recycle`).
//!
//! The two talk in lines. The worker's standard output carries its reports, each line starting
//! with `PREFIX` (`Report`); any other line it prints (the relay's own messages) goes to the
//! daemon's log as it is. Its standard input carries the daemon's commands (`Command`): `go` lets
//! a worker started held (`--hold`) send the microphone, `stop` ends it cleanly. A worker whose
//! input closes (the daemon is gone) stops and ends within `EXIT_GRACE`, and the daemon puts its
//! workers in a job object that ends them with it (`Job`), so that none is ever left behind.
//!
//! A worker started to take over from another is held: it opens its streams and its engine, runs
//! the microphone through the engine, says `ready`, and sends nothing until `go`. The daemon tells
//! the old worker `stop` first, so the two never feed the virtual microphone together.
//!
//! The worker reports the microphone's level twice a second with how long it has been quiet
//! (`recycle::QuietClock`), the activity of its relay, whether an effects engine exists (the daemon
//! keeps the echo references playing while one does, see `keepalive`), its problems, and a relay
//! thread that no longer moves (`stalled`).
//!
//! `--simulate quiet|loud|crash` plays a worker without opening any device or loading anything of
//! the vendor's, for the protocol's test (`tests/worker.rs`).

use crate::devices::{Devices, State};
use crate::recycle::QuietClock;
use crate::relay::{self, Activity};
use std::ffi::c_void;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

/// What starts every report line on the worker's output.
pub const PREFIX: &str = "#pw ";
/// The command line's word for the worker.
pub const SUBCOMMAND: &str = "mic-worker";
/// How often the worker says the microphone's level (and that it is alive).
pub const LEVEL_EVERY: Duration = Duration::from_millis(500);
/// A relay thread that has not woken up for this long, while it should, is stuck.
pub const STALL_AFTER: Duration = Duration::from_secs(3);
/// How long a worker whose input closed may take to end before it ends itself outright.
pub const EXIT_GRACE: Duration = Duration::from_secs(3);

/// What a worker tells the daemon, one line each.
#[derive(Clone, Debug, PartialEq)]
pub enum Report {
    /// Held: the streams and the engine are open, the microphone runs through it, nothing is sent.
    Ready,
    /// The microphone goes to the virtual one, through the effects engine or raw.
    Streaming { effects: bool },
    /// A device failed after audio flowed: the worker waits for it and reopens its streams.
    Interrupted,
    /// An effects engine exists now, or no longer.
    Engine(bool),
    /// The last problem met (device lost, engine failed), as technical text.
    Problem(String),
    /// The microphone's highest peak level since the last report, and how long it has been quiet.
    Level { peak: f32, quiet: Duration },
    /// The relay's thread has not woken up for `STALL_AFTER`: the daemon should end this worker.
    Stalled,
    /// After `stop`: the worker's streams are closed, it sends nothing more; it ends next.
    Stopped,
    /// The effects engine asked for could not start: nothing was relayed.
    Refused(String),
    /// The relay could not start (devices missing…): nothing was relayed.
    Failed(String),
    /// The relay ended as asked; the worker ends.
    Done,
}

impl Report {
    /// The report as a line of the worker's output (without the line break).
    pub fn line(&self) -> String {
        let body = match self {
            Report::Ready => "ready".to_string(),
            Report::Streaming { effects } => format!("streaming {}", if *effects { "effects" } else { "raw" }),
            Report::Interrupted => "interrupted".to_string(),
            Report::Engine(on) => format!("engine {}", if *on { "on" } else { "off" }),
            Report::Problem(text) => format!("problem {}", single_line(text)),
            Report::Level { peak, quiet } => format!("level {:.4} {}", if peak.is_finite() { peak.abs() } else { 1.0 }, quiet.as_millis()),
            Report::Stalled => "stalled".to_string(),
            Report::Stopped => "stopped".to_string(),
            Report::Refused(text) => format!("refused {}", single_line(text)),
            Report::Failed(text) => format!("failed {}", single_line(text)),
            Report::Done => "done".to_string(),
        };
        format!("{PREFIX}{body}")
    }

    /// A line of the worker's output as a report; `None` for any other line (the relay's own
    /// messages, or a report this daemon does not know).
    pub fn parse(line: &str) -> Option<Report> {
        let body = line.trim_end_matches(['\r', '\n']).strip_prefix(PREFIX)?;
        let (word, rest) = body.split_once(' ').unwrap_or((body, ""));
        Some(match word {
            "ready" => Report::Ready,
            "streaming" => match rest {
                "effects" => Report::Streaming { effects: true },
                "raw" => Report::Streaming { effects: false },
                _ => return None,
            },
            "interrupted" => Report::Interrupted,
            "engine" => match rest {
                "on" => Report::Engine(true),
                "off" => Report::Engine(false),
                _ => return None,
            },
            "problem" => Report::Problem(rest.to_string()),
            "level" => {
                let (peak, quiet) = rest.split_once(' ')?;
                Report::Level { peak: peak.parse().ok()?, quiet: Duration::from_millis(quiet.trim().parse().ok()?) }
            }
            "stalled" => Report::Stalled,
            "stopped" => Report::Stopped,
            "refused" => Report::Refused(rest.to_string()),
            "failed" => Report::Failed(rest.to_string()),
            "done" => Report::Done,
            _ => return None,
        })
    }
}

/// What the daemon tells a worker, one line each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Send the microphone (a worker started held).
    Go,
    /// Close the streams and end.
    Stop,
}

impl Command {
    pub fn line(self) -> &'static str {
        match self {
            Command::Go => "go",
            Command::Stop => "stop",
        }
    }

    pub fn parse(line: &str) -> Option<Command> {
        match line.trim() {
            "go" => Some(Command::Go),
            "stop" => Some(Command::Stop),
            _ => None,
        }
    }
}

/// A worker that plays its part without any device, for the protocol's test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Simulation {
    /// The microphone stays quiet.
    Quiet,
    /// The user speaks all the time.
    Loud,
    /// The worker ends abruptly a moment after it starts sending.
    Crash,
}

impl Simulation {
    fn key(self) -> &'static str {
        match self {
            Simulation::Quiet => "quiet",
            Simulation::Loud => "loud",
            Simulation::Crash => "crash",
        }
    }
}

/// The worker's command line, after `mic-worker`: what the daemon would give the relay itself.
#[derive(Clone, Debug, PartialEq)]
pub struct Args {
    /// The real microphone's identifier.
    pub source: String,
    /// The virtual microphone's playback side, its identifier.
    pub target: String,
    /// The effects engine's DLL, when the microphone goes through it.
    pub effects_dll: Option<PathBuf>,
    pub gain: f32,
    pub latency_ms: u32,
    /// Send nothing until `go`.
    pub hold: bool,
    pub simulate: Option<Simulation>,
}

impl Args {
    /// The arguments to pass after `mic-worker`.
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec!["--source".to_string(), self.source.clone(), "--target".to_string(), self.target.clone()];
        if let Some(dll) = &self.effects_dll {
            args.push("--effects-dll".into());
            args.push(dll.display().to_string());
        }
        args.extend(["--gain".to_string(), self.gain.to_string(), "--latency".to_string(), self.latency_ms.to_string()]);
        if self.hold {
            args.push("--hold".into());
        }
        if let Some(simulation) = self.simulate {
            args.push("--simulate".into());
            args.push(simulation.key().into());
        }
        args
    }

    /// Reads the arguments after `mic-worker` (errors in English, like the rest of the command
    /// line). The microphone and its destination are required, unless simulated.
    pub fn parse<S: AsRef<str>>(args: &[S]) -> Result<Args, String> {
        let mut parsed = Args { source: String::new(), target: String::new(), effects_dll: None, gain: 1.0, latency_ms: relay::DEFAULT_LATENCY_MS, hold: false, simulate: None };
        let mut rest = args.iter().map(AsRef::as_ref);
        while let Some(arg) = rest.next() {
            let mut value = || rest.next().ok_or_else(|| format!("{arg} needs a value"));
            match arg {
                "--source" => parsed.source = value()?.to_string(),
                "--target" => parsed.target = value()?.to_string(),
                "--effects-dll" => parsed.effects_dll = Some(PathBuf::from(value()?)),
                "--gain" => {
                    let text = value()?;
                    parsed.gain = text.parse().ok().filter(|gain: &f32| (0.0..=1.0).contains(gain)).ok_or_else(|| format!("--gain {text}: expected a number from 0 to 1"))?;
                }
                "--latency" => {
                    let text = value()?;
                    parsed.latency_ms = text.parse().ok().filter(|ms| relay::LATENCY_RANGE_MS.contains(ms)).ok_or_else(|| format!("--latency {text}: expected {} to {} ms", relay::LATENCY_RANGE_MS.start(), relay::LATENCY_RANGE_MS.end()))?;
                }
                "--hold" => parsed.hold = true,
                "--simulate" => {
                    parsed.simulate = Some(match value()? {
                        "quiet" => Simulation::Quiet,
                        "loud" => Simulation::Loud,
                        "crash" => Simulation::Crash,
                        other => return Err(format!("--simulate {other}: expected quiet, loud or crash")),
                    })
                }
                other => return Err(format!("unknown mic-worker argument {other}")),
            }
        }
        if parsed.simulate.is_none() && (parsed.source.is_empty() || parsed.target.is_empty()) {
            return Err("mic-worker needs --source and --target (it is started by the daemon)".into());
        }
        Ok(parsed)
    }
}

/// A value kept on one line.
fn single_line(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().trim().to_string()
}

/// A line the worker printed, as the daemon's log shows it: without the time stamp the relay puts
/// in front of its own lines ("[20261122-153000] "), since the daemon's log line has one.
pub fn strip_stamp(line: &str) -> &str {
    let bytes = line.as_bytes();
    // Bytes only: the 18 bytes checked are ASCII, so cutting after them is safe in any text.
    let stamped = bytes.len() > 18 && bytes[0] == b'[' && bytes[9] == b'-' && bytes[16] == b']' && bytes[17] == b' ' && bytes[1..9].iter().chain(&bytes[10..16]).all(u8::is_ascii_digit);
    if stamped { &line[18..] } else { line }
}

/// An exit code as the log says it: a small number as it is, a Windows status (a crash) in hex.
pub fn exit_text(status: Option<i32>) -> String {
    match status {
        None => "no exit code".to_string(),
        Some(code) if (0..256).contains(&code) => format!("exit code {code}"),
        Some(code) => format!("exit code 0x{:08X}", code as u32),
    }
}

// ---------------------------------------------------------------------------------------------
// The worker's side: `platinum mic-worker …`.

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut c_void;
    fn TerminateProcess(process: *mut c_void, code: u32) -> i32;
    fn CreateJobObjectW(security: *mut c_void, name: *const u16) -> *mut c_void;
    fn SetInformationJobObject(job: *mut c_void, class: u32, info: *const c_void, size: u32) -> i32;
    fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn GetLastError() -> u32;
}

/// Writes one report on the worker's output, at once (a line at a time, whatever else prints).
fn emit(report: &Report) {
    let mut out = std::io::stdout().lock();
    // The daemon gone, nobody reads: the worker stops anyway when its input closes.
    let _ = writeln!(out, "{}", report.line());
    let _ = out.flush();
}

/// Ends this process at once with `code`. In the worker, the engine has been released by then;
/// ending outright spares the vendor's DLLs their unload, which nothing needs and which could hang
/// a process that must not linger (the daemon's `stop_deadline` ends it the same way).
pub(crate) fn end_process(code: u32) -> ! {
    let _ = std::io::stdout().lock().flush();
    unsafe {
        TerminateProcess(GetCurrentProcess(), code);
    }
    std::process::exit(code as i32)
}

/// `platinum mic-worker …`: relays the microphone as the daemon asked, reporting on the standard
/// output and obeying the standard input, until `stop` or the input closes. Returns only when the
/// arguments are wrong; otherwise the process ends here (0 when stopped as asked, 1 when the relay
/// could not start, 2 when the effects engine refused).
pub fn serve<S: AsRef<str>>(args: &[S]) -> Result<(), String> {
    let args = Args::parse(args)?;
    std::thread::Builder::new().name("worker-input".into()).spawn(read_commands).map_err(|e| format!("cannot start the worker's input thread: {e}"))?;
    if let Some(simulation) = args.simulate {
        simulate(&args, simulation);
    }
    match relay_once(&args) {
        Ok(()) => {
            emit(&Report::Done);
            end_process(0)
        }
        Err(error) if relay::effects_refused() => {
            emit(&Report::Refused(error));
            end_process(2)
        }
        Err(error) => {
            emit(&Report::Failed(error));
            end_process(1)
        }
    }
}

/// Reads the daemon's commands until the input closes; then stops the relay and makes sure the
/// process ends, cleanly within `EXIT_GRACE` or outright.
fn read_commands() {
    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => match Command::parse(&line) {
                Some(Command::Go) => relay::release(),
                Some(Command::Stop) => relay::request_stop(),
                None => {}
            },
        }
    }
    relay::request_stop();
    std::thread::sleep(EXIT_GRACE);
    end_process(0)
}

/// Opens the devices the daemon named and runs the relay once, its activity reported meanwhile.
fn relay_once(args: &Args) -> Result<(), String> {
    let devices = Devices::open().map_err(|e| format!("Windows audio devices unavailable: {e}"))?;
    let endpoints = devices.list().map_err(|e| e.to_string())?;
    let source = endpoints.iter().find(|e| e.id == args.source).ok_or_else(|| format!("the microphone {} is missing", args.source))?;
    let target = endpoints.iter().find(|e| e.id == args.target).ok_or("the virtual microphone is missing")?;
    for endpoint in [source, target] {
        if endpoint.state != State::Active {
            return Err(format!("{} is {}", endpoint.name, endpoint.state.label()));
        }
    }
    let config = relay::Config {
        source,
        target,
        gain: args.gain,
        latency_ms: args.latency_ms,
        seconds: None,
        report: None,
        effects: args.effects_dll.clone().map(|dll| relay::Effects { dll }),
        hold: args.hold,
    };
    let watcher = Watcher::start();
    let result = relay::run(&devices, &config);
    drop(watcher);
    result
}

/// The thread that turns the relay's state into reports.
struct Watcher {
    done: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watcher {
    fn start() -> Watcher {
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        // Without the thread the worker still relays; the daemon then ends it as silent.
        let thread = std::thread::Builder::new().name("worker-reports".into()).spawn(move || watch(&flag)).ok();
        Watcher { done, thread }
    }
}

impl Drop for Watcher {
    /// Its last look is reported before the worker's final line.
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// What the watcher said last, to report changes only.
struct Said {
    activity: Activity,
    engine: bool,
    problem: String,
    stopped: bool,
}

fn watch(done: &AtomicBool) {
    let mut said = Said { activity: Activity::Idle, engine: false, problem: String::new(), stopped: false };
    let mut clock = QuietClock::default();
    let mut loudest = 0.0f32;
    let mut next_level = Instant::now();
    let (mut ticks, mut moved) = (relay::ticks(), Instant::now());
    let mut stalled = false;
    loop {
        let finishing = done.load(Ordering::SeqCst);
        let now = Instant::now();
        let peak = relay::take_peak();
        clock.add(now, peak);
        loudest = loudest.max(peak);
        let engine = relay::effects_engine_alive();
        if engine != said.engine {
            emit(&Report::Engine(engine));
            said.engine = engine;
        }
        // The problem first: the daemon shows it with the activity that follows.
        let problem = relay::last_problem();
        if problem != said.problem && !problem.is_empty() {
            emit(&Report::Problem(problem.clone()));
        }
        said.problem = problem;
        let activity = relay::activity();
        if activity != said.activity {
            match activity {
                Activity::Ready => emit(&Report::Ready),
                Activity::Streaming { effects } => emit(&Report::Streaming { effects }),
                Activity::Interrupted => emit(&Report::Interrupted),
                Activity::Idle => {}
            }
            said.activity = activity;
        }
        // The streams are closed once the relay is idle again after a stop.
        if activity == Activity::Idle && relay::stop_requested() && !said.stopped {
            emit(&Report::Stopped);
            said.stopped = true;
        }
        let count = relay::ticks();
        if count != ticks {
            (ticks, moved, stalled) = (count, now, false);
        } else if matches!(activity, Activity::Streaming { .. } | Activity::Ready) && now.saturating_duration_since(moved) >= STALL_AFTER && !stalled {
            emit(&Report::Stalled);
            stalled = true;
        }
        if now >= next_level {
            emit(&Report::Level { peak: loudest, quiet: clock.quiet_for(now) });
            loudest = 0.0;
            next_level = now + LEVEL_EVERY;
        }
        if finishing {
            return;
        }
        // Quick while something is changing hands (the daemon measures that gap), slow otherwise.
        let changing = relay::stop_requested() || activity == Activity::Ready || (relay::released() && !matches!(activity, Activity::Streaming { .. }));
        std::thread::sleep(Duration::from_millis(if changing { 5 } else { 100 }));
    }
}

/// A worker without devices: says what a real one would, on the same commands. Opens no device,
/// loads nothing of the vendor's, writes nothing.
fn simulate(args: &Args, simulation: Simulation) -> ! {
    let effects = args.effects_dll.is_some();
    if effects {
        emit(&Report::Engine(true));
    }
    std::thread::sleep(Duration::from_millis(50));
    if args.hold {
        emit(&Report::Ready);
        while !relay::released() && !relay::stop_requested() {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    let started = Instant::now();
    if !relay::stop_requested() {
        emit(&Report::Streaming { effects });
    }
    let mut next_level = started;
    while !relay::stop_requested() {
        let now = Instant::now();
        if simulation == Simulation::Crash && now.saturating_duration_since(started) >= Duration::from_millis(300) {
            end_process(0xC000_0005);
        }
        if now >= next_level {
            let (peak, quiet) = if simulation == Simulation::Loud { (0.2, Duration::ZERO) } else { (0.001, now.saturating_duration_since(started)) };
            emit(&Report::Level { peak, quiet });
            next_level = now + LEVEL_EVERY;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    emit(&Report::Stopped);
    if effects {
        emit(&Report::Engine(false));
    }
    emit(&Report::Done);
    end_process(0)
}

// ---------------------------------------------------------------------------------------------
// The daemon's side: starting workers and hearing them.

/// JOBOBJECT_BASIC_LIMIT_INFORMATION.
#[repr(C)]
#[derive(Default)]
struct BasicLimits {
    per_process_user_time_limit: i64,
    per_job_user_time_limit: i64,
    limit_flags: u32,
    minimum_working_set_size: usize,
    maximum_working_set_size: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

/// IO_COUNTERS.
#[repr(C)]
#[derive(Default)]
struct IoCounters {
    read_operations: u64,
    write_operations: u64,
    other_operations: u64,
    read_bytes: u64,
    write_bytes: u64,
    other_bytes: u64,
}

/// JOBOBJECT_EXTENDED_LIMIT_INFORMATION.
#[repr(C)]
#[derive(Default)]
struct ExtendedLimits {
    basic: BasicLimits,
    io: IoCounters,
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: u32 = 9;
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
/// The worker gets a console nobody sees, of its own: a Ctrl+C in the daemon's console is the
/// daemon's to handle, which then stops its workers in order.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A job object that ends every process in it when it is closed, which Windows does when the
/// daemon ends, however it ends.
pub struct Job {
    handle: isize,
}

impl Job {
    pub fn new() -> Result<Job, String> {
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
            if handle.is_null() {
                return Err(format!("cannot create a job object (Windows error {})", GetLastError()));
            }
            let job = Job { handle: handle as isize };
            let mut limits = ExtendedLimits::default();
            limits.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let info: *const ExtendedLimits = &limits;
            if SetInformationJobObject(handle, JOB_OBJECT_EXTENDED_LIMIT_INFORMATION, info.cast(), std::mem::size_of::<ExtendedLimits>() as u32) == 0 {
                return Err(format!("cannot make the job end its processes with it (Windows error {})", GetLastError()));
            }
            Ok(job)
        }
    }

    fn assign(&self, child: &Child) -> Result<(), String> {
        if unsafe { AssignProcessToJobObject(self.handle as *mut c_void, child.as_raw_handle()) } == 0 {
            return Err(format!("cannot put the worker in the daemon's job (Windows error {})", unsafe { GetLastError() }));
        }
        Ok(())
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle as *mut c_void) };
    }
}

/// What the daemon hears from a worker, tagged with the worker's process identifier.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Report(Report),
    /// Any other line the worker printed.
    Text(String),
    /// Its output closed: it is ending, or has ended.
    Closed,
}

/// A worker process, as the daemon holds it. Dropping it ends the process if it still runs.
pub struct Worker {
    child: Child,
    stdin: Option<ChildStdin>,
    pid: u32,
    started: Instant,
    exit: Option<ExitStatus>,
}

impl Worker {
    /// Starts `exe mic-worker <args>`, its output heard on its own thread and sent to `events`.
    /// Returns the worker, and why it could not be put in `job` if it could not (it still ends
    /// when the daemon's end of its input closes).
    pub fn spawn(exe: &Path, args: &Args, job: Option<&Job>, events: Sender<(u32, Event)>) -> Result<(Worker, Option<String>), String> {
        let mut command = std::process::Command::new(exe);
        command.arg(SUBCOMMAND).args(args.to_args()).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).creation_flags(CREATE_NO_WINDOW);
        if let Some(folder) = exe.parent() {
            command.current_dir(folder);
        }
        let mut child = command.spawn().map_err(|e| format!("cannot start {}: {e}", exe.display()))?;
        let pid = child.id();
        let unjobbed = job.and_then(|job| job.assign(&child).err());
        let stdin = child.stdin.take();
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("the worker's output is not connected".into());
        };
        let reader = std::thread::Builder::new().name("worker-output".into()).spawn(move || read_reports(pid, stdout, &events));
        if let Err(error) = reader {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("cannot start the thread that hears the worker: {error}"));
        }
        Ok((Worker { child, stdin, pid, started: Instant::now(), exit: None }, unjobbed))
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// How long ago it was started.
    pub fn age(&self) -> Duration {
        self.started.elapsed()
    }

    /// Sends a command; false when its input is closed or broken.
    pub fn send(&mut self, command: Command) -> bool {
        let Some(stdin) = self.stdin.as_mut() else { return false };
        let sent = writeln!(stdin, "{}", command.line()).and_then(|()| stdin.flush()).is_ok();
        if !sent {
            self.stdin = None;
        }
        sent
    }

    /// Closes its input: it stops and ends within `EXIT_GRACE` if nothing else made it.
    pub fn close_input(&mut self) {
        self.stdin = None;
    }

    /// Its exit code once it has ended (`Some(None)` when it ended without one); `None` while it runs.
    pub fn ended(&mut self) -> Option<Option<i32>> {
        if self.exit.is_none() {
            self.exit = self.child.try_wait().ok().flatten();
        }
        self.exit.map(|status| status.code())
    }

    /// Ends it outright.
    pub fn kill(&mut self) {
        if self.ended().is_none() {
            let _ = self.child.kill();
            self.exit = self.child.wait().ok();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Hears a worker's output until it closes. Never stops reading before that, so that a worker
/// never blocks on a full pipe, even once the daemon no longer listens.
fn read_reports(pid: u32, stdout: ChildStdout, events: &Sender<(u32, Event)>) {
    let mut reader = BufReader::new(stdout);
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        match reader.read_until(b'\n', &mut bytes) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let text = String::from_utf8_lossy(&bytes);
                let line = text.trim_end();
                let event = Report::parse(line).map_or_else(|| Event::Text(line.to_string()), Event::Report);
                let _ = events.send((pid, event));
            }
        }
    }
    let _ = events.send((pid, Event::Closed));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_report_survives_its_line() {
        let reports = [
            Report::Ready,
            Report::Streaming { effects: true },
            Report::Streaming { effects: false },
            Report::Interrupted,
            Report::Engine(true),
            Report::Engine(false),
            Report::Problem("device lost (0x88890004)".into()),
            Report::Problem(String::new()),
            Report::Level { peak: 0.0125, quiet: Duration::from_millis(2500) },
            Report::Level { peak: 0.0, quiet: Duration::ZERO },
            Report::Stalled,
            Report::Stopped,
            Report::Refused("effects engine: class not registered".into()),
            Report::Failed("Microphone (Razer Seiren Mini) is unplugged".into()),
            Report::Done,
        ];
        for report in reports {
            let line = report.line();
            assert!(line.starts_with(PREFIX) && !line.contains('\n'), "{line}");
            assert_eq!(Report::parse(&line), Some(report.clone()), "{line}");
            assert_eq!(Report::parse(&format!("{line}\r\n")), Some(report), "{line}");
        }
    }

    #[test]
    fn other_lines_are_not_reports() {
        assert_eq!(Report::parse("effects engine ready: 2 ch 48000 Hz float, 480-frame blocks"), None);
        assert_eq!(Report::parse("[20261122-153000] relayed 12.0 s of audio"), None);
        // A report of a later version, or a damaged one, is heard as text rather than misread.
        for line in ["#pw dancing", "#pw streaming loud", "#pw engine maybe", "#pw level 0.1", "#pw level x 10", "#pwready", "pw ready"] {
            assert_eq!(Report::parse(line), None, "{line}");
        }
        // Texts stay on one line; levels that are not numbers are sent as loud.
        assert_eq!(Report::Problem("first\nsecond\r".into()).line(), "#pw problem first second");
        assert_eq!(Report::Level { peak: f32::NAN, quiet: Duration::from_millis(7) }.line(), "#pw level 1.0000 7");
    }

    #[test]
    fn commands_are_single_words() {
        for command in [Command::Go, Command::Stop] {
            assert_eq!(Command::parse(command.line()), Some(command));
            assert_eq!(Command::parse(&format!("  {}\r\n", command.line())), Some(command));
        }
        assert_eq!(Command::parse("halt"), None);
        assert_eq!(Command::parse(""), None);
    }

    #[test]
    fn the_worker_gets_what_the_daemon_gives_the_relay() {
        let args = Args {
            source: "{0.0.1.00000000}.{c3}".into(),
            target: "{0.0.0.00000000}.{f1}".into(),
            effects_dll: Some(PathBuf::from(r"C:\Program Files\SteelSeries\GG\apps\sonar\Sonar.APO.dll")),
            gain: 0.73,
            latency_ms: 30,
            hold: true,
            simulate: None,
        };
        let line = args.to_args();
        assert_eq!(Args::parse(&line), Ok(args.clone()));
        let raw = Args { effects_dll: None, hold: false, gain: 1.0, ..args };
        assert_eq!(Args::parse(&raw.to_args()), Ok(raw));
        let simulated = Args::parse(&["--simulate", "loud", "--hold"]).unwrap();
        assert_eq!((simulated.simulate, simulated.hold, simulated.source.as_str()), (Some(Simulation::Loud), true, ""));
        for wrong in [&["--source", "a"][..], &["--source", "a", "--target", "b", "--gain", "2"], &["--source", "a", "--target", "b", "--latency", "5"], &["--simulate", "sing"], &["--target"], &["--loud"]] {
            assert!(Args::parse(wrong).is_err(), "{wrong:?}");
        }
    }

    #[test]
    fn the_log_shows_worker_lines_without_their_stamp_and_crashes_in_hex() {
        assert_eq!(strip_stamp("[20261122-153000] effects engine recycled"), "effects engine recycled");
        assert_eq!(strip_stamp("effects engine ready: 480-frame blocks"), "effects engine ready: 480-frame blocks");
        assert_eq!(strip_stamp("[ 120 s] in 0.0100"), "[ 120 s] in 0.0100");
        assert_eq!(strip_stamp("[2026112x-153000] x"), "[2026112x-153000] x");
        assert_eq!(strip_stamp("[20261122-153000]"), "[20261122-153000]");
        assert_eq!(exit_text(Some(0)), "exit code 0");
        assert_eq!(exit_text(Some(2)), "exit code 2");
        assert_eq!(exit_text(Some(0xC000_0005_u32 as i32)), "exit code 0xC0000005");
        assert_eq!(exit_text(None), "no exit code");
    }

    #[test]
    fn job_limits_have_windows_layout() {
        // JOBOBJECT_EXTENDED_LIMIT_INFORMATION is 144 bytes on x64; a wrong size is refused.
        assert_eq!(std::mem::size_of::<BasicLimits>(), 64);
        assert_eq!(std::mem::size_of::<ExtendedLimits>(), 144);
    }
}
