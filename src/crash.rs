//! Crash reports: what Platinum writes down when one of its programs ends abruptly, and how the
//! user may send it, read first and anonymised, as a GitHub issue.
//!
//! Capture is local and always on, in every program (`install`): the command line and the daemon
//! (`platinum.exe`), the microphone worker (`platinum mic-worker`), the window, the launcher and the
//! setup. Without it a crash leaves nothing behind: the release builds abort on panic and have no
//! console, and the worker hosts the vendor's DLL, which can fault on its own.
//! - A Rust panic goes through a panic hook, which runs before the abort: the report holds the
//!   message, where it happened and the stack (module + offset: the release builds are stripped,
//!   so `std::backtrace` resolves names only in builds that keep them).
//! - A native crash (an access violation inside SteelSeries' DLL, a stack overflow) goes through
//!   the unhandled exception filter, which writes a shorter report from fixed buffers, without
//!   allocating (the heap may be what broke): the exception, the faulting module's file name and
//!   offset, the thread, the stack, the log's tail. A small minidump (`MiniDumpNormal`) goes next
//!   to it; it stays on this PC and is never part of what is sent.
//! - The daemon records a report when the microphone worker ends with a crash code, and the setup
//!   when its administrator part does (`record`, `append_to_recent`).
//!
//! Reports are plain text, `%USERPROFILE%\Platinum\crashes\<stamp>-<program>.txt` (the setup's
//! land there too, next to its install.log): a header of `key: value` lines (program, version,
//! Windows build, time, process, thread, reason), then sections (details, stack, the last
//! `LOG_LINES` lines of the program's log). The `KEEP` newest are kept.
//!
//! Nothing is ever sent by Platinum, and this feature makes no network call at all. To send a
//! report, the user opens it in the window, reads it anonymised (`anonymize`: profile paths and
//! the user name, the computer name, e-mail addresses, the names of real audio devices, which can
//! hold people's names), and clicks: the browser opens GitHub's "new issue" page pre-filled
//! (`issue_link`), and the user sends it there, or not. Whether the window offers it after a crash
//! is `Consent` in platinum.ini (`crash_reports = off|ask`, off by default).
//!
//! The repository is `ISSUES_REPO`: Platinum's own (`OFFICIAL_REPO`), or the one a fork names at
//! build time with the `PLATINUM_REPO` environment variable. While it is no `owner/name` (or the
//! placeholder), `repository` says so and no link is made.

use std::ffi::c_void;
use std::fmt::Write as _;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// What a fork's `PLATINUM_REPO` may still say before it names its repository: never linked.
pub const REPO_PLACEHOLDER: &str = "OWNER/REPO";
/// Platinum's own repository, where its issues are.
pub const OFFICIAL_REPO: &str = "MrAnilex/Platinum";
/// The GitHub repository the issues go to, `owner/name`: `PLATINUM_REPO` when the build had it
/// (a fork: `set PLATINUM_REPO=someone/platinum` before `cargo build`), else `OFFICIAL_REPO`.
pub const ISSUES_REPO: &str = match option_env!("PLATINUM_REPO") {
    Some(repo) => repo,
    None => OFFICIAL_REPO,
};
/// How many reports are kept, newest first.
pub const KEEP: usize = 20;
/// How many lines of the program's log a report carries.
pub const LOG_LINES: usize = 50;
/// The longest link given to the browser: GitHub and the browsers take about 8 KB.
pub const MAX_URL: usize = 7000;
/// The folder's name, in the install folder.
pub const FOLDER: &str = "crashes";
/// The reports already offered for sending by the window, one file name per line.
const OFFERED: &str = "offered.txt";
/// Bytes of a log read for its tail: enough for `LOG_LINES` lines of the daemon's log.
const TAIL_BYTES: usize = 16 * 1024;

/// The programs, as reports and file names name them.
pub const CLI: &str = "platinum";
pub const DAEMON: &str = "daemon";
pub const WORKER: &str = "mic-worker";
pub const WINDOW: &str = "platinum-ui";
pub const LAUNCHER: &str = "platinum-start";
pub const SETUP: &str = "setup";
/// The setup's administrator part (its crashes are recorded by the setup itself).
pub const SETUP_ADMIN: &str = "setup-admin";

/// Whether the window offers to send a report after a crash (`crash_reports` in platinum.ini).
/// Reports are written either way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Consent {
    /// Reports stay on this PC; nothing is offered.
    #[default]
    Off,
    /// After a crash, the window offers to send the report (read first, anonymised).
    Ask,
}

impl Consent {
    pub fn key(self) -> &'static str {
        match self {
            Consent::Off => "off",
            Consent::Ask => "ask",
        }
    }

    pub fn parse(text: &str) -> Option<Consent> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "non" | "no" | "0" => Some(Consent::Off),
            "ask" | "true" | "oui" | "yes" | "1" => Some(Consent::Ask),
            _ => None,
        }
    }
}

/// The repository issues go to, `None` when the build's `PLATINUM_REPO` is the placeholder or no
/// `owner/name`.
pub fn repository() -> Option<&'static str> {
    valid_repository(ISSUES_REPO)
}

/// `owner/name`, made of letters, digits, `-`, `_` and `.`; never the placeholder.
fn valid_repository(text: &str) -> Option<&str> {
    let text = text.trim();
    let (owner, name) = text.split_once('/')?;
    let fine = |part: &str| !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    (text != REPO_PLACEHOLDER && fine(owner) && fine(name)).then_some(text)
}

/// `%USERPROFILE%\Platinum\crashes`.
pub fn folder() -> Result<PathBuf, String> {
    Ok(crate::autostart::install_dir()?.join(FOLDER))
}

/// The log a program's report carries: install.log for the setup, the daemon's log otherwise
/// (next to this program, where the launcher writes it, else in the install folder).
pub fn default_log(program: &str) -> Option<PathBuf> {
    let install = crate::autostart::install_dir().ok();
    if program == SETUP || program == SETUP_ADMIN {
        return install.map(|folder| folder.join("install.log"));
    }
    let here = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|folder| folder.join("daemon.log")));
    match here {
        Some(file) if file.exists() => Some(file),
        _ => install.map(|folder| folder.join("daemon.log")),
    }
}

// ---------------------------------------------------------------------------------------------
// Reports.

/// One report, as written.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub program: String,
    pub version: String,
    /// "10.0.26200".
    pub windows: String,
    /// Local time, "2026-11-22 15:30:00".
    pub time: String,
    pub pid: u32,
    pub thread: String,
    /// One line: what happened.
    pub reason: String,
    /// The program that wrote the report when it is not the one that crashed ("daemon").
    pub recorded_by: String,
    pub details: Vec<String>,
    pub stack: Vec<String>,
    /// The log's file name ("daemon.log") and its last lines.
    pub log_name: String,
    pub log_tail: Vec<String>,
}

impl Report {
    /// A report of this process, now, with what every report says.
    pub fn new(program: &str, reason: String) -> Report {
        let windows = crate::machine::windows_version();
        Report {
            program: program.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            windows: format!("{}.{}.{}", windows.major, windows.minor, windows.build),
            time: readable_stamp(&crate::guard::stamp()),
            pid: std::process::id(),
            thread: String::new(),
            reason,
            ..Report::default()
        }
    }

    /// The file's text.
    pub fn to_text(&self) -> String {
        let mut text = String::from("Platinum crash report\n");
        let _ = writeln!(text, "program: {}", one_line(&self.program));
        let _ = writeln!(text, "version: {}", one_line(&self.version));
        let _ = writeln!(text, "windows: {}", one_line(&self.windows));
        let _ = writeln!(text, "time: {}", one_line(&self.time));
        let _ = writeln!(text, "pid: {}", self.pid);
        if !self.thread.is_empty() {
            let _ = writeln!(text, "thread: {}", one_line(&self.thread));
        }
        let _ = writeln!(text, "reason: {}", one_line(&self.reason));
        if !self.recorded_by.is_empty() {
            let _ = writeln!(text, "recorded by: {}", one_line(&self.recorded_by));
        }
        section(&mut text, "details", &self.details);
        section(&mut text, "stack (module+offset)", &self.stack);
        if !self.log_name.is_empty() {
            let title = format!("{}, last {} lines", self.log_name, self.log_tail.len());
            section(&mut text, &title, &self.log_tail);
        }
        text
    }
}

fn section(text: &mut String, title: &str, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    let _ = writeln!(text, "\n{title}:");
    for line in lines {
        let _ = writeln!(text, "  {line}");
    }
}

/// A value kept on one line.
fn one_line(value: &str) -> String {
    value.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().trim().to_string()
}

/// "20261122-153000" as "2026-11-22 15:30:00"; anything else as it is.
pub fn readable_stamp(stamp: &str) -> String {
    match stamp.split_once('-') {
        Some((date, time)) if date.len() == 8 && time.len() == 6 && date.bytes().chain(time.bytes()).all(|b| b.is_ascii_digit()) => {
            format!("{}-{}-{} {}:{}:{}", &date[..4], &date[4..6], &date[6..8], &time[..2], &time[2..4], &time[4..6])
        }
        _ => stamp.to_string(),
    }
}

/// A report as the lists show it, from its file name and header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub file: PathBuf,
    /// The file name, which identifies the report.
    pub name: String,
    /// "20261122-153000", from the file name.
    pub stamp: String,
    pub program: String,
    pub reason: String,
}

/// The header's program and reason (empty when missing).
pub fn header(text: &str) -> (String, String) {
    let (mut program, mut reason) = (String::new(), String::new());
    for line in text.lines() {
        if line.trim().is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("program: ") {
            program = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("reason: ") {
            reason = value.trim().to_string();
        }
    }
    (program, reason)
}

/// Whether a file name is a report's: `<8 digits>-<6 digits>-<program>[-n].txt`.
fn is_report_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    name.ends_with(".txt") && bytes.len() > 20 && bytes[8] == b'-' && bytes[15] == b'-' && bytes[..8].iter().chain(&bytes[9..15]).all(u8::is_ascii_digit)
}

/// The report file names in a folder, newest first (the stamp leads the name).
fn report_names(folder: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(folder).map(|entries| entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|name| is_report_name(name)).collect()).unwrap_or_default();
    names.sort_by(|a, b| b.cmp(a));
    names
}

/// The reports of `crashes\`, newest first. Reads only the start of each file.
pub fn list() -> Vec<Summary> {
    folder().map(|folder| list_in(&folder)).unwrap_or_default()
}

pub fn list_in(folder: &Path) -> Vec<Summary> {
    report_names(folder)
        .into_iter()
        .map(|name| {
            let file = folder.join(&name);
            let start = read_start(&file, 4096);
            let (program, reason) = header(&start);
            let program = if program.is_empty() { name.get(16..).unwrap_or("").trim_end_matches(".txt").to_string() } else { program };
            Summary { stamp: name.get(..15).unwrap_or("").to_string(), file, name, program, reason }
        })
        .collect()
}

fn read_start(file: &Path, bytes: u64) -> String {
    let mut text = Vec::new();
    if let Ok(handle) = std::fs::File::open(file) {
        let _ = handle.take(bytes).read_to_end(&mut text);
    }
    String::from_utf8_lossy(&text).into_owned()
}

/// A report's full text.
pub fn read(file: &Path) -> Result<String, String> {
    std::fs::read(file).map(|bytes| String::from_utf8_lossy(&bytes).into_owned()).map_err(|e| format!("cannot read {}: {e}", file.display()))
}

/// The file names a report may take: `<stamp>-<program>.txt`, then `-2` to `-9` for crashes of the
/// same program within the same second.
fn candidate_names(stamp: &str, program: &str) -> impl Iterator<Item = String> {
    let (stamp, program) = (stamp.to_string(), program.to_string());
    (1..=9).map(move |n| if n == 1 { format!("{stamp}-{program}.txt") } else { format!("{stamp}-{program}-{n}.txt") })
}

/// Writes a report in `folder` under a name of its own, then keeps the `KEEP` newest.
pub fn write_in(folder: &Path, report: &Report) -> Result<PathBuf, String> {
    std::fs::create_dir_all(folder).map_err(|e| format!("cannot create {}: {e}", folder.display()))?;
    let stamp = crate::guard::stamp();
    let text = report.to_text();
    for name in candidate_names(&stamp, &report.program) {
        let file = folder.join(&name);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&file) {
            Ok(mut handle) => {
                use std::io::Write;
                handle.write_all(text.as_bytes()).map_err(|e| format!("cannot write {}: {e}", file.display()))?;
                drop(handle);
                prune(folder, KEEP);
                return Ok(file);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("cannot write {}: {error}", file.display())),
        }
    }
    Err(format!("too many reports this second in {}", folder.display()))
}

/// Keeps the `keep` newest reports, with their minidumps; removes the other minidumps.
pub fn prune(folder: &Path, keep: usize) {
    let names = report_names(folder);
    for name in names.iter().skip(keep) {
        let _ = std::fs::remove_file(folder.join(name));
    }
    let kept: Vec<String> = names.iter().take(keep).map(|name| name.trim_end_matches(".txt").to_string()).collect();
    if let Ok(entries) = std::fs::read_dir(folder) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(stem) = name.strip_suffix(".dmp") {
                if !kept.iter().any(|k| k == stem) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}

/// Records a report for a program that could not write its own (the microphone worker, seen
/// ending by the daemon; the setup's administrator part, seen by the setup): `pid` is its process
/// (0 when unknown), `reason` and `details` say what was seen, the log is the recorder's. Returns
/// the file.
pub fn record(program: &str, pid: u32, reason: String, details: Vec<String>, log: Option<&Path>) -> Result<PathBuf, String> {
    let mut report = Report::new(program, reason);
    report.pid = pid;
    report.recorded_by = recorder().to_string();
    report.details = details;
    attach_log(&mut report, log);
    write_in(&folder()?, &report)
}

/// The program recording a report for another (the one that installed the hooks).
fn recorder() -> &'static str {
    HOOKED.get().copied().unwrap_or(CLI)
}

/// The latest report `program` wrote itself for process `pid` within `within`, if any: a worker's
/// own panic or exception report, to which the daemon adds what it saw rather than writing a
/// second one.
pub fn recent_report_of(program: &str, pid: u32, within: std::time::Duration) -> Option<PathBuf> {
    let folder = folder().ok()?;
    let wanted = format!("pid: {pid}\n");
    report_names(&folder).into_iter().take(KEEP).map(|name| folder.join(name)).find(|file| {
        let fresh = std::fs::metadata(file).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age <= within);
        let start = read_start(file, 1024);
        fresh && header(&start).0 == program && start.contains(&wanted)
    })
}

/// Adds a section at the end of a report.
pub fn append(file: &Path, title: &str, lines: &[String]) -> Result<(), String> {
    use std::io::Write;
    let mut text = String::new();
    section(&mut text, title, lines);
    let mut handle = std::fs::OpenOptions::new().append(true).open(file).map_err(|e| format!("cannot open {}: {e}", file.display()))?;
    handle.write_all(text.as_bytes()).map_err(|e| format!("cannot write {}: {e}", file.display()))
}

fn attach_log(report: &mut Report, log: Option<&Path>) {
    if let Some(log) = log {
        let lines = tail_lines(log, LOG_LINES);
        if !lines.is_empty() {
            report.log_name = log.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            report.log_tail = lines;
        }
    }
}

/// The last `count` lines of a file (its last `TAIL_BYTES`), without a line cut at the start.
pub fn tail_lines(file: &Path, count: usize) -> Vec<String> {
    let Ok(mut handle) = std::fs::File::open(file) else { return Vec::new() };
    let length = handle.seek(SeekFrom::End(0)).unwrap_or(0);
    let start = length.saturating_sub(TAIL_BYTES as u64);
    if handle.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    let _ = handle.read_to_end(&mut bytes);
    last_lines(&String::from_utf8_lossy(&bytes), count, start > 0)
}

/// The last `count` non-empty lines of `text`; the first line is dropped when `cut` (it may be a
/// part of one).
fn last_lines(text: &str, count: usize, cut: bool) -> Vec<String> {
    let mut lines: Vec<&str> = text.lines().collect();
    if cut && !lines.is_empty() {
        lines.remove(0);
    }
    let lines: Vec<String> = lines.into_iter().map(str::trim_end).filter(|line| !line.is_empty()).map(str::to_string).collect();
    lines[lines.len().saturating_sub(count)..].to_vec()
}

/// Deletes every report and minidump (and the record of those offered); returns how many reports.
pub fn clear() -> Result<usize, String> {
    clear_in(&folder()?)
}

pub fn clear_in(folder: &Path) -> Result<usize, String> {
    let names = report_names(folder);
    let mut failed = Vec::new();
    for name in &names {
        if let Err(error) = std::fs::remove_file(folder.join(name)) {
            failed.push(format!("{name}: {error}"));
        }
    }
    prune(folder, 0);
    let _ = std::fs::remove_file(folder.join(OFFERED));
    if failed.is_empty() { Ok(names.len()) } else { Err(failed.join("; ")) }
}

/// The reports already offered for sending.
pub fn offered() -> Vec<String> {
    folder().map(|folder| offered_in(&folder)).unwrap_or_default()
}

fn offered_in(folder: &Path) -> Vec<String> {
    std::fs::read_to_string(folder.join(OFFERED)).map(|text| text.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect()).unwrap_or_default()
}

/// Notes reports as offered (the window asked once, or the user turned the offer on with these
/// already there), keeping only names of reports that still exist.
pub fn mark_offered(names: &[String]) -> Result<(), String> {
    let folder = folder()?;
    let existing = report_names(&folder);
    let mut kept: Vec<String> = offered_in(&folder).into_iter().chain(names.iter().cloned()).filter(|name| existing.contains(name)).collect();
    kept.sort();
    kept.dedup();
    if kept.is_empty() {
        let _ = std::fs::remove_file(folder.join(OFFERED));
        return Ok(());
    }
    crate::config::write_atomically(&folder.join(OFFERED), &(kept.join("\n") + "\n"))
}

/// The report the window offers to send: the newest one not offered yet.
pub fn to_offer<'a>(reports: &'a [Summary], offered: &[String]) -> Option<&'a Summary> {
    reports.iter().find(|report| !offered.contains(&report.name))
}

// ---------------------------------------------------------------------------------------------
// Exit codes and exceptions.

/// Whether a process's exit code is a crash: a Windows exception status (warning or error
/// severity), not an exit the program chose. Ctrl+C and a closed console are not crashes.
pub fn is_crash_code(code: Option<i32>) -> bool {
    match code {
        Some(code) => {
            let code = code as u32;
            code >= 0x8000_0000 && code != STATUS_CONTROL_C_EXIT
        }
        None => false,
    }
}

const STATUS_CONTROL_C_EXIT: u32 = 0xC000_013A;
const STATUS_ACCESS_VIOLATION: u32 = 0xC000_0005;

/// The name of a Windows exception status, in English; empty when unknown.
pub fn exception_name(code: u32) -> &'static str {
    match code {
        0x8000_0003 => "breakpoint",
        0x8000_0004 => "single step",
        STATUS_ACCESS_VIOLATION => "access violation",
        0xC000_0006 => "in-page error",
        0xC000_001D => "illegal instruction",
        0xC000_0025 => "non-continuable exception",
        0xC000_008C => "array bounds exceeded",
        0xC000_008D => "floating-point denormal operand",
        0xC000_008E => "floating-point division by zero",
        0xC000_008F => "floating-point inexact result",
        0xC000_0090 => "floating-point invalid operation",
        0xC000_0091 => "floating-point overflow",
        0xC000_0092 => "floating-point stack check",
        0xC000_0093 => "floating-point underflow",
        0xC000_0094 => "integer division by zero",
        0xC000_0095 => "integer overflow",
        0xC000_0096 => "privileged instruction",
        0xC000_00FD => "stack overflow",
        0xC000_0135 => "DLL not found",
        0xC000_0139 => "entry point not found",
        0xC000_0142 => "DLL initialization failed",
        0xC000_0374 => "heap corruption",
        0xC000_0409 => "fast fail (a Rust panic aborts this way)",
        0xC000_0417 => "invalid C runtime parameter",
        0xC000_0602 => "fail-fast exception",
        0xE06D_7363 => "uncaught C++ exception",
        _ => "",
    }
}

/// An exit code as reports say it: "exit code 0xC0000005 (access violation)".
pub fn exit_description(code: Option<i32>) -> String {
    match code {
        None => "no exit code".into(),
        Some(code) if (0..256).contains(&code) => format!("exit code {code}"),
        Some(code) => match exception_name(code as u32) {
            "" => format!("exit code 0x{:08X}", code as u32),
            name => format!("exit code 0x{:08X} ({name})", code as u32),
        },
    }
}

// ---------------------------------------------------------------------------------------------
// Capture: the panic hook and the unhandled exception filter.

/// The program whose hooks are installed in this process.
static HOOKED: OnceLock<&'static str> = OnceLock::new();

/// Installs the panic hook and the exception filter of `program`, its log being the default one
/// (`default_log`). Once per process; nothing is written until a crash.
pub fn install(program: &'static str) {
    install_with_log(program, default_log(program));
}

/// Like `install`, with the log the reports carry.
pub fn install_with_log(program: &'static str, log: Option<PathBuf>) {
    if HOOKED.set(program).is_err() {
        return;
    }
    let Ok(folder) = folder() else { return };
    install_filter(program, &folder, log.as_deref());
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write_panic(program, &folder, log.as_deref(), info);
        previous(info);
    }));
}

/// The panic hook's report. Allocates (the heap is fine after a panic), never panics.
fn write_panic(program: &str, folder: &Path, log: Option<&Path>, info: &std::panic::PanicHookInfo<'_>) {
    let message = if let Some(text) = info.payload().downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = info.payload().downcast_ref::<String>() {
        text.clone()
    } else {
        "(no message)".to_string()
    };
    let place = info.location().map(|l| format!(" (at {}:{}:{})", l.file(), l.line(), l.column())).unwrap_or_default();
    let mut report = Report::new(program, format!("panic: {message}{place}"));
    let current = std::thread::current();
    report.thread = format!("{} ({})", unsafe { GetCurrentThreadId() }, current.name().unwrap_or("unnamed"));
    report.stack = stack_lines(3);
    let symbols = std::backtrace::Backtrace::force_capture().to_string();
    // Names only when the build kept its symbols (debug and test builds).
    if symbols.contains("::") {
        report.details = symbols.lines().map(|line| line.trim_end().to_string()).filter(|line| !line.is_empty()).take(60).collect();
        report.details.insert(0, "backtrace:".into());
    }
    attach_log(&mut report, log);
    let _ = write_in(folder, &report);
}

/// This thread's stack, as module+offset lines, skipping `skip` frames (the hook's own).
fn stack_lines(skip: u32) -> Vec<String> {
    let mut frames = [std::ptr::null_mut::<c_void>(); 48];
    let count = unsafe { RtlCaptureStackBackTrace(skip, frames.len() as u32, frames.as_mut_ptr(), std::ptr::null_mut()) } as usize;
    let mut module = [0u16; MODULE_UNITS];
    frames[..count.min(frames.len())]
        .iter()
        .map(|frame| match module_at(*frame as usize, &mut module) {
            Some((length, offset)) => format!("{}+0x{offset:X}", String::from_utf16_lossy(&module[..length])),
            None => format!("0x{:X}", *frame as usize),
        })
        .collect()
}

const MODULE_UNITS: usize = 300;

/// The file name (not the path: it would hold the user's folder) of the module holding `address`,
/// written in `name` (its length is returned), and the offset of `address` in it. Allocates nothing.
fn module_at(address: usize, name: &mut [u16; MODULE_UNITS]) -> Option<(usize, usize)> {
    const FROM_ADDRESS: u32 = 0x4;
    const UNCHANGED_REFCOUNT: u32 = 0x2;
    if address == 0 {
        return None;
    }
    let mut module = std::ptr::null_mut();
    unsafe {
        if GetModuleHandleExW(FROM_ADDRESS | UNCHANGED_REFCOUNT, address as *const u16, &mut module) == 0 || module.is_null() {
            return None;
        }
        let length = GetModuleFileNameW(module, name.as_mut_ptr(), MODULE_UNITS as u32) as usize;
        let length = length.min(MODULE_UNITS);
        let start = name[..length].iter().rposition(|unit| *unit == u16::from(b'\\') || *unit == u16::from(b'/')).map_or(0, |at| at + 1);
        name.copy_within(start..length, 0);
        Some((length - start, address.wrapping_sub(module as usize)))
    }
}

/// What the exception filter needs, prepared when it is installed so that a crash allocates
/// nothing.
struct Native {
    program: &'static str,
    /// The reports' folder, without a terminating zero, and its parent (the install folder, which
    /// a setup that crashes before installing has not created yet), zero-terminated.
    folder: Vec<u16>,
    parent: Vec<u16>,
    /// The log, zero-terminated, and its file name.
    log: Option<(Vec<u16>, String)>,
    windows: (u32, u32, u32),
    main_thread: u32,
}

static NATIVE: OnceLock<Native> = OnceLock::new();
/// Set by the first thread that crashes: only it writes.
static WRITING: AtomicBool = AtomicBool::new(false);

const REPORT_BYTES: usize = 24 * 1024;
const PATH_UNITS: usize = 600;
const FRAMES: usize = 48;

/// Buffers of the exception filter, in static memory: a stack overflow leaves the filter little
/// stack, and the heap may be what broke. Used only by the thread that set `WRITING`.
struct Scratch {
    report: std::cell::UnsafeCell<[u8; REPORT_BYTES]>,
    log: std::cell::UnsafeCell<[u8; TAIL_BYTES]>,
    path: std::cell::UnsafeCell<[u16; PATH_UNITS]>,
    module: std::cell::UnsafeCell<[u16; MODULE_UNITS]>,
    frames: std::cell::UnsafeCell<[*mut c_void; FRAMES]>,
}

// Only the thread that won `WRITING` ever touches it.
unsafe impl Sync for Scratch {}

static SCRATCH: Scratch = Scratch {
    report: std::cell::UnsafeCell::new([0; REPORT_BYTES]),
    log: std::cell::UnsafeCell::new([0; TAIL_BYTES]),
    path: std::cell::UnsafeCell::new([0; PATH_UNITS]),
    module: std::cell::UnsafeCell::new([0; MODULE_UNITS]),
    frames: std::cell::UnsafeCell::new([std::ptr::null_mut(); FRAMES]),
};

fn install_filter(program: &'static str, folder: &Path, log: Option<&Path>) {
    use std::os::windows::ffi::OsStrExt;
    let windows = crate::machine::windows_version();
    let native = Native {
        program,
        folder: folder.as_os_str().encode_wide().collect(),
        parent: folder.parent().map(|p| p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()).unwrap_or_default(),
        log: log.map(|file| (file.as_os_str().encode_wide().chain(std::iter::once(0)).collect(), file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())),
        windows: (windows.major, windows.minor, windows.build),
        main_thread: unsafe { GetCurrentThreadId() },
    };
    if NATIVE.set(native).is_ok() {
        unsafe { SetUnhandledExceptionFilter(Some(on_exception)) };
    }
}

/// The unhandled exception filter: writes the report (first crashing thread only), then lets the
/// process end with the exception's code, as it would have (without Windows Error Reporting).
unsafe extern "system" fn on_exception(pointers: *const ExceptionPointers) -> i32 {
    const EXCEPTION_EXECUTE_HANDLER: i32 = 1;
    if !WRITING.swap(true, Ordering::SeqCst) {
        if let Some(native) = NATIVE.get() {
            unsafe { write_native(native, pointers) };
        }
    }
    EXCEPTION_EXECUTE_HANDLER
}

/// A fixed buffer written with `core::fmt`, cut when full. Allocates nothing.
struct Buf<'a> {
    bytes: &'a mut [u8],
    len: usize,
}

impl Buf<'_> {
    fn put(&mut self, data: &[u8]) {
        let room = self.bytes.len().saturating_sub(self.len);
        let n = data.len().min(room);
        if let (Some(target), Some(source)) = (self.bytes.get_mut(self.len..self.len + n), data.get(..n)) {
            target.copy_from_slice(source);
            self.len += n;
        }
    }

    /// UTF-16 text (a module's file name) as UTF-8.
    fn put_wide(&mut self, units: &[u16]) {
        for c in char::decode_utf16(units.iter().copied()) {
            let mut encoded = [0u8; 4];
            self.put(c.unwrap_or('\u{FFFD}').encode_utf8(&mut encoded).as_bytes());
        }
    }

    fn filled(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }
}

impl std::fmt::Write for Buf<'_> {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        self.put(text.as_bytes());
        Ok(())
    }
}

/// A fixed zero-terminated UTF-16 path, built without allocating.
struct WidePath<'a> {
    units: &'a mut [u16],
    len: usize,
}

impl WidePath<'_> {
    fn push_units(&mut self, data: &[u16]) {
        for unit in data {
            if let Some(slot) = self.units.get_mut(self.len) {
                *slot = *unit;
                self.len += 1;
            }
        }
    }

    fn push(&mut self, text: &str) {
        for unit in text.encode_utf16() {
            self.push_units(&[unit]);
        }
    }

    fn push_number(&mut self, value: u32, width: usize) {
        let mut digits = [0u8; 10];
        let mut count = 0;
        let mut rest = value;
        while count < digits.len() && (rest > 0 || count < width) {
            digits[count] = b'0' + (rest % 10) as u8;
            rest /= 10;
            count += 1;
        }
        for digit in digits[..count].iter().rev() {
            self.push_units(&[u16::from(*digit)]);
        }
    }

    /// Zero-terminates it at `len`; false when it does not fit.
    fn terminate(&mut self) -> bool {
        match self.units.get_mut(self.len) {
            Some(slot) => {
                *slot = 0;
                true
            }
            None => false,
        }
    }
}

/// The native report and the minidump. Runs inside the exception filter: static buffers only,
/// Win32 calls only, nothing that panics.
unsafe fn write_native(native: &Native, pointers: *const ExceptionPointers) {
    let (report, log, path, module, frames) = unsafe { (&mut *SCRATCH.report.get(), &mut *SCRATCH.log.get(), &mut *SCRATCH.path.get(), &mut *SCRATCH.module.get(), &mut *SCRATCH.frames.get()) };
    let mut time = SystemTime::default();
    unsafe { GetLocalTime(&mut time) };

    // The folder (and the install folder above it), then a name of its own.
    if native.parent.last() == Some(&0) {
        unsafe { CreateDirectoryW(native.parent.as_ptr(), std::ptr::null_mut()) };
    }
    let mut folder = WidePath { units: &mut path[..], len: 0 };
    folder.push_units(&native.folder);
    if folder.terminate() {
        unsafe { CreateDirectoryW(path.as_ptr(), std::ptr::null_mut()) };
    }
    let mut file = INVALID_HANDLE;
    let mut stem = 0;
    for attempt in 1..=9u32 {
        let mut name = WidePath { units: &mut path[..], len: 0 };
        name.push_units(&native.folder);
        name.push("\\");
        name.push_number(u32::from(time.year), 4);
        name.push_number(u32::from(time.month), 2);
        name.push_number(u32::from(time.day), 2);
        name.push("-");
        name.push_number(u32::from(time.hour), 2);
        name.push_number(u32::from(time.minute), 2);
        name.push_number(u32::from(time.second), 2);
        name.push("-");
        name.push(native.program);
        if attempt > 1 {
            name.push("-");
            name.push_number(attempt, 1);
        }
        stem = name.len;
        name.push(".txt");
        if !name.terminate() {
            return;
        }
        file = unsafe { CreateFileW(path.as_ptr(), GENERIC_WRITE, FILE_SHARE_READ, std::ptr::null_mut(), CREATE_NEW, FILE_ATTRIBUTE_NORMAL, std::ptr::null_mut()) };
        if file != INVALID_HANDLE {
            break;
        }
    }
    if file == INVALID_HANDLE {
        return;
    }

    let record = unsafe { pointers.as_ref().and_then(|p| p.record.as_ref()) };
    let (code, address) = record.map_or((0, 0), |r| (r.code, r.address as usize));
    let mut text = Buf { bytes: &mut report[..], len: 0 };
    let thread = unsafe { GetCurrentThreadId() };
    let _ = write!(text, "Platinum crash report\nprogram: {}\nversion: {}\nwindows: {}.{}.{}\n", native.program, env!("CARGO_PKG_VERSION"), native.windows.0, native.windows.1, native.windows.2);
    let _ = write!(text, "time: {:04}-{:02}-{:02} {:02}:{:02}:{:02}\npid: {}\nthread: {thread}{}\n", time.year, time.month, time.day, time.hour, time.minute, time.second, unsafe { GetCurrentProcessId() }, if thread == native.main_thread { " (main)" } else { "" });
    let name = match exception_name(code) {
        "" => "exception",
        name => name,
    };
    let _ = write!(text, "reason: {name} (0x{code:08X})");
    match module_at(address, module) {
        Some((length, offset)) => {
            text.put(b" in ");
            text.put_wide(module.get(..length).unwrap_or(&[]));
            let _ = write!(text, "+0x{offset:X}\n");
        }
        None => {
            let _ = write!(text, " at 0x{address:X}\n");
        }
    }
    if let Some(record) = record {
        let _ = write!(text, "\ndetails:\n  exception address 0x{address:X}, flags 0x{:X}\n", record.flags);
        if code == STATUS_ACCESS_VIOLATION && record.parameters >= 2 {
            let what = match record.information[0] {
                0 => "read",
                1 => "write",
                8 => "execute",
                _ => "access",
            };
            let _ = write!(text, "  attempted to {what} address 0x{:X}\n", record.information[1]);
        }
    }

    let count = unsafe { RtlCaptureStackBackTrace(0, FRAMES as u32, frames.as_mut_ptr(), std::ptr::null_mut()) } as usize;
    if count > 0 {
        text.put(b"\nstack (module+offset):\n");
        for frame in frames.iter().take(count) {
            match module_at(*frame as usize, module) {
                Some((length, offset)) => {
                    text.put(b"  ");
                    text.put_wide(module.get(..length).unwrap_or(&[]));
                    let _ = write!(text, "+0x{offset:X}\n");
                }
                None => {
                    let _ = write!(text, "  0x{:X}\n", *frame as usize);
                }
            }
        }
    }
    if let Some((log_path, log_name)) = &native.log {
        let tail = unsafe { read_tail(log_path, log) };
        if !tail.is_empty() {
            let lines = tail.iter().filter(|b| **b == b'\n').count() + usize::from(tail.last() != Some(&b'\n'));
            let _ = write!(text, "\n{log_name}, last {lines} lines:\n  ");
            for (index, byte) in tail.iter().enumerate() {
                if *byte == b'\r' {
                    continue;
                }
                text.put(&[*byte]);
                if *byte == b'\n' && index + 1 < tail.len() {
                    text.put(b"  ");
                }
            }
            if tail.last() != Some(&b'\n') {
                text.put(b"\n");
            }
        }
    }
    let mut written = 0u32;
    let filled = text.filled();
    unsafe {
        WriteFile(file, filled.as_ptr(), filled.len() as u32, &mut written, std::ptr::null_mut());
        CloseHandle(file);
    }
    unsafe { write_minidump(path, stem, pointers) };
}

/// The minidump next to the report (`<stem>.dmp`), then a line saying so at the end of the report.
/// The report is complete before: should dbghelp fail or hang here, nothing of it is lost.
unsafe fn write_minidump(path: &mut [u16; PATH_UNITS], stem: usize, pointers: *const ExceptionPointers) {
    let mut name = WidePath { units: &mut path[..], len: stem };
    name.push(".dmp");
    if !name.terminate() {
        return;
    }
    let mut library = [0u16; 16];
    let mut dll = WidePath { units: &mut library[..], len: 0 };
    dll.push("dbghelp.dll");
    if !dll.terminate() {
        return;
    }
    unsafe {
        let module = LoadLibraryW(library.as_ptr());
        if module.is_null() {
            return;
        }
        let function = GetProcAddress(module, c"MiniDumpWriteDump".as_ptr().cast());
        if function.is_null() {
            return;
        }
        let write_dump: MiniDumpWriteDumpFn = std::mem::transmute(function);
        let dump = CreateFileW(path.as_ptr(), GENERIC_WRITE, 0, std::ptr::null_mut(), CREATE_NEW, FILE_ATTRIBUTE_NORMAL, std::ptr::null_mut());
        if dump == INVALID_HANDLE {
            return;
        }
        let exception = MinidumpExceptionInformation { thread: GetCurrentThreadId(), pointers, client_pointers: 0 };
        const MINI_DUMP_NORMAL: u32 = 0;
        let done = write_dump(GetCurrentProcess(), GetCurrentProcessId(), dump, MINI_DUMP_NORMAL, &exception, std::ptr::null(), std::ptr::null());
        CloseHandle(dump);
        if done == 0 {
            return;
        }
        // Back to the report, to say where the dump is (its file name only).
        let mut report = WidePath { units: &mut path[..], len: stem };
        report.push(".txt");
        if !report.terminate() {
            return;
        }
        let file = CreateFileW(path.as_ptr(), FILE_APPEND_DATA, FILE_SHARE_READ, std::ptr::null_mut(), OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, std::ptr::null_mut());
        if file == INVALID_HANDLE {
            return;
        }
        let mut line = [0u8; 160];
        let mut text = Buf { bytes: &mut line[..], len: 0 };
        text.put(b"\nminidump:\n  ");
        let start = path[..stem].iter().rposition(|unit| *unit == u16::from(b'\\')).map_or(0, |at| at + 1);
        text.put_wide(path.get(start..stem).unwrap_or(&[]));
        text.put(b".dmp, kept on this PC only (never part of what is sent)\n");
        let mut written = 0u32;
        let filled = text.filled();
        WriteFile(file, filled.as_ptr(), filled.len() as u32, &mut written, std::ptr::null_mut());
        CloseHandle(file);
    }
}

/// The last `LOG_LINES` lines of the log, read into `buffer`; empty when it cannot be read.
unsafe fn read_tail<'a>(log: &[u16], buffer: &'a mut [u8; TAIL_BYTES]) -> &'a [u8] {
    const GENERIC_READ: u32 = 0x8000_0000;
    const SHARE_ALL: u32 = 0x7;
    unsafe {
        let file = CreateFileW(log.as_ptr(), GENERIC_READ, SHARE_ALL, std::ptr::null_mut(), OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, std::ptr::null_mut());
        if file == INVALID_HANDLE {
            return &[];
        }
        let mut size = 0i64;
        GetFileSizeEx(file, &mut size);
        let start = (size - TAIL_BYTES as i64).max(0);
        SetFilePointerEx(file, start, std::ptr::null_mut(), 0);
        let mut read = 0u32;
        let ok = ReadFile(file, buffer.as_mut_ptr(), TAIL_BYTES as u32, &mut read, std::ptr::null_mut()) != 0;
        CloseHandle(file);
        if !ok {
            return &[];
        }
        let data = buffer.get(..read as usize).unwrap_or(&[]);
        // A line cut by the start of the read is left out.
        let data = if start > 0 { data.iter().position(|b| *b == b'\n').and_then(|at| data.get(at + 1..)).unwrap_or(&[]) } else { data };
        // The last `LOG_LINES` lines (a final line break ends the last one).
        let body = data.strip_suffix(b"\n").unwrap_or(data);
        let mut seen = 0;
        let mut from = 0;
        for (index, byte) in body.iter().enumerate().rev() {
            if *byte == b'\n' {
                seen += 1;
                if seen == LOG_LINES {
                    from = index + 1;
                    break;
                }
            }
        }
        data.get(from..).unwrap_or(&[])
    }
}

#[repr(C)]
#[derive(Default)]
struct SystemTime {
    year: u16,
    month: u16,
    weekday: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    milliseconds: u16,
}

#[repr(C)]
struct ExceptionRecord {
    code: u32,
    flags: u32,
    record: *mut ExceptionRecord,
    address: *mut c_void,
    parameters: u32,
    information: [usize; 15],
}

#[repr(C)]
struct ExceptionPointers {
    record: *mut ExceptionRecord,
    context: *mut c_void,
}

/// dbghelp packs its structures on 4 bytes.
#[repr(C, packed(4))]
struct MinidumpExceptionInformation {
    thread: u32,
    pointers: *const ExceptionPointers,
    client_pointers: i32,
}

type MiniDumpWriteDumpFn = unsafe extern "system" fn(*mut c_void, u32, *mut c_void, u32, *const MinidumpExceptionInformation, *const c_void, *const c_void) -> i32;
type ExceptionFilter = unsafe extern "system" fn(*const ExceptionPointers) -> i32;

const INVALID_HANDLE: *mut c_void = -1isize as *mut c_void;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_APPEND_DATA: u32 = 0x4;
const FILE_SHARE_READ: u32 = 0x1;
const CREATE_NEW: u32 = 1;
const OPEN_EXISTING: u32 = 3;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetUnhandledExceptionFilter(filter: Option<ExceptionFilter>) -> *mut c_void;
    fn GetLocalTime(time: *mut SystemTime);
    fn GetCurrentThreadId() -> u32;
    fn GetCurrentProcessId() -> u32;
    fn GetCurrentProcess() -> *mut c_void;
    fn CreateDirectoryW(path: *const u16, security: *mut c_void) -> i32;
    fn CreateFileW(path: *const u16, access: u32, share: u32, security: *mut c_void, disposition: u32, flags: u32, template: *mut c_void) -> *mut c_void;
    fn WriteFile(file: *mut c_void, data: *const u8, length: u32, written: *mut u32, overlapped: *mut c_void) -> i32;
    fn ReadFile(file: *mut c_void, data: *mut u8, length: u32, read: *mut u32, overlapped: *mut c_void) -> i32;
    fn GetFileSizeEx(file: *mut c_void, size: *mut i64) -> i32;
    fn SetFilePointerEx(file: *mut c_void, distance: i64, position: *mut i64, method: u32) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn GetModuleHandleExW(flags: u32, name: *const u16, module: *mut *mut c_void) -> i32;
    fn GetModuleFileNameW(module: *mut c_void, name: *mut u16, size: u32) -> u32;
    fn RtlCaptureStackBackTrace(skip: u32, count: u32, frames: *mut *mut c_void, hash: *mut u32) -> u16;
    fn LoadLibraryW(name: *const u16) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
}

// ---------------------------------------------------------------------------------------------
// Anonymising: what is removed from a report before the user sees what would be sent.

/// An audio device whose name may be personal ("Casque (AirPods de Jean)").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceName {
    pub name: String,
    /// A microphone or other input (else an output).
    pub input: bool,
}

/// What identifies this user and PC, to be taken out of a report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Identity {
    /// The profile folder, "C:\Users\Jean".
    pub profile: Option<String>,
    /// The Windows user name.
    pub user: Option<String>,
    pub computer: Option<String>,
    /// The audio devices known on this PC (Platinum's and SteelSeries' channels are left alone).
    pub devices: Vec<DeviceName>,
}

impl Identity {
    /// This user's and PC's names, from the environment, with the devices given.
    pub fn of_this_pc(devices: Vec<DeviceName>) -> Identity {
        let variable = |name: &str| std::env::var(name).ok().map(|value| value.trim().to_string()).filter(|value| !value.is_empty());
        Identity { profile: variable("USERPROFILE"), user: variable("USERNAME"), computer: variable("COMPUTERNAME"), devices }
    }
}

/// The audio devices whose names a report may hold: those Windows lists (`endpoints`, in any
/// state), then those platinum.ini remembers (the priority lists, the microphone and the fallback
/// devices when named rather than identified); the virtual channels are left out.
pub fn device_names(endpoints: &[crate::devices::Endpoint], config: &crate::config::Config) -> Vec<DeviceName> {
    let mut names: Vec<DeviceName> = endpoints.iter().filter(|e| e.virtual_role.is_none()).map(|e| DeviceName { name: e.name.clone(), input: e.flow == crate::devices::Flow::Capture }).collect();
    let remembered = config.output_priority.iter().map(|d| (d.name.as_str(), false)).chain(config.input_priority.iter().map(|d| (d.name.as_str(), true)));
    let named = [(config.microphone.as_deref(), true), (config.fallback_input.as_deref(), true), (config.fallback_output.as_deref(), false)].into_iter().filter_map(|(name, input)| name.map(|n| (n, input)));
    for (name, input) in remembered.chain(named) {
        let name = name.trim();
        if !name.is_empty() && !name.starts_with('{') && !names.iter().any(|known| known.name.eq_ignore_ascii_case(name)) {
            names.push(DeviceName { name: name.to_string(), input });
        }
    }
    names
}

/// Whether a device name is one of Platinum's or SteelSeries' virtual channels, which say nothing
/// of the user and help reading the report.
pub fn is_channel_name(name: &str) -> bool {
    name.contains("(Platinum)") || name.contains("SteelSeries Sonar")
}

/// Starts and ends the markers put in during anonymising (Private Use Area: never in a report,
/// never a letter), replaced by their labels at the end.
const MARK_OPEN: char = '\u{E000}';
const MARK_CLOSE: char = '\u{E001}';

/// The report as it may be sent: the profile paths and the user name become `<user>`, the computer
/// name `<pc>`, e-mail addresses `<e-mail>`, Windows account identifiers `<sid>`, hardware
/// addresses `<mac>`, and the names of real audio devices generic labels (`<output 1>`,
/// `<input 1>`), Platinum's and SteelSeries' channels excepted.
pub fn anonymize(text: &str, identity: &Identity) -> String {
    let mut labels: Vec<String> = Vec::new();
    let mut text: String = text.chars().map(|c| if c == MARK_OPEN || c == MARK_CLOSE { '?' } else { c }).collect();
    let mark = |labels: &mut Vec<String>, label: String| -> String {
        let index = labels.iter().position(|l| *l == label).unwrap_or_else(|| {
            labels.push(label);
            labels.len() - 1
        });
        format!("{MARK_OPEN}{index}{MARK_CLOSE}")
    };

    // Devices first, longest names first: a name may contain another's.
    let mut devices: Vec<(String, String)> = Vec::new();
    let (mut outputs, mut inputs) = (0, 0);
    for device in &identity.devices {
        let name = device.name.trim();
        if name.chars().count() < 3 || is_channel_name(name) || devices.iter().any(|(known, _)| known.eq_ignore_ascii_case(name)) {
            continue;
        }
        let label = if device.input {
            inputs += 1;
            format!("<input {inputs}>")
        } else {
            outputs += 1;
            format!("<output {outputs}>")
        };
        devices.push((name.to_string(), label.clone()));
        // "Casque (AirPods de Jean)": the part in brackets alone too, where the person is.
        if let Some(inner) = name.split_once('(').and_then(|(_, rest)| rest.strip_suffix(')')).map(str::trim) {
            if inner.chars().count() >= 4 && !is_channel_name(inner) && !inner.eq_ignore_ascii_case("Platinum") {
                devices.push((inner.to_string(), label));
            }
        }
    }
    devices.sort_by_key(|(name, _)| std::cmp::Reverse(name.chars().count()));
    for (name, label) in &devices {
        let marker = mark(&mut labels, label.clone());
        text = replace_text(&text, name, &marker, false);
    }

    text = replace_emails(&text, &mark(&mut labels, "<e-mail>".into()));
    text = replace_sids(&text, &mark(&mut labels, "<sid>".into()));
    text = replace_macs(&text, &mark(&mut labels, "<mac>".into()));
    let user_marker = mark(&mut labels, "<user>".into());
    if let Some(profile) = identity.profile.as_deref().map(|p| p.trim_end_matches(['\\', '/'])).filter(|p| p.len() >= 3) {
        // The profile's own path, with either separator, keeps all but its last part, the user's
        // folder name (which may differ from the user name, and may hold spaces).
        for path in [profile.replace('/', "\\"), profile.replace('\\', "/")] {
            text = replace_text_with(&text, &path, true, |found| match found.rfind(['\\', '/']) {
                Some(at) => format!("{}{user_marker}", &found[..=at]),
                None => user_marker.clone(),
            });
        }
        if let Some((_, folder)) = profile.rsplit_once(['\\', '/']).filter(|(_, folder)| folder.chars().count() >= 3) {
            text = replace_text(&text, folder, &user_marker, true);
        }
    }
    text = replace_users_folders(&text, &user_marker);
    if let Some(user) = identity.user.as_deref().filter(|u| u.chars().count() >= 3) {
        text = replace_text(&text, user, &user_marker, true);
    }
    if let Some(computer) = identity.computer.as_deref().filter(|c| c.chars().count() >= 3) {
        let marker = mark(&mut labels, "<pc>".into());
        text = replace_text(&text, computer, &marker, true);
    }

    // The markers become their labels.
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(start) = rest.find(MARK_OPEN) {
        out.push_str(&rest[..start]);
        let after = &rest[start + MARK_OPEN.len_utf8()..];
        match after.find(MARK_CLOSE) {
            Some(end) => {
                let label = after[..end].parse::<usize>().ok().and_then(|i| labels.get(i)).map_or("<removed>", String::as_str);
                out.push_str(label);
                rest = &after[end + MARK_CLOSE.len_utf8()..];
            }
            None => {
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Replaces every occurrence of `word` in `text`, ignoring case; with `whole`, only where it is not
/// part of a longer word.
fn replace_text(text: &str, word: &str, with: &str, whole: bool) -> String {
    replace_text_with(text, word, whole, |_| with.to_string())
}

/// Like `replace_text`, the replacement made from the text found.
fn replace_text_with(text: &str, word: &str, whole: bool, with: impl Fn(&str) -> String) -> String {
    if word.is_empty() {
        return text.to_string();
    }
    let lower_word: Vec<char> = word.chars().flat_map(char::to_lowercase).collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    let bytes = text.len();
    while index < bytes {
        let rest = &text[index..];
        if let Some(length) = matches_at(rest, &lower_word) {
            let before_ok = !whole || text[..index].chars().next_back().is_none_or(|c| !is_word_char(c));
            let after_ok = !whole || rest[length..].chars().next().is_none_or(|c| !is_word_char(c));
            if before_ok && after_ok {
                out.push_str(&with(&rest[..length]));
                index += length;
                continue;
            }
        }
        let Some(c) = rest.chars().next() else { break };
        out.push(c);
        index += c.len_utf8();
    }
    out
}

/// A letter, a digit or `_`: what continues a word (a marker's digits are inside its brackets).
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The length in bytes of the match of `lower_word` at the start of `text`, ignoring case.
fn matches_at(text: &str, lower_word: &[char]) -> Option<usize> {
    let mut wanted = lower_word.iter();
    let mut length = 0;
    let mut chars = text.chars();
    let mut pending: Vec<char> = Vec::new();
    loop {
        if pending.is_empty() {
            if wanted.as_slice().is_empty() {
                return Some(length);
            }
            let c = chars.next()?;
            length += c.len_utf8();
            pending = c.to_lowercase().collect();
            pending.reverse();
        }
        let next = pending.pop()?;
        if wanted.next() != Some(&next) {
            return None;
        }
    }
}

/// `\Users\<name>` and `/Users/<name>` in any path: the folder name becomes `with`.
fn replace_users_folders(text: &str, with: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let found = ["\\users\\", "/users/"].iter().filter_map(|pattern| find_ignoring_case(rest, pattern).map(|at| (at, pattern.len()))).min();
        let Some((at, length)) = found else { break };
        let (head, tail) = rest.split_at(at + length);
        out.push_str(head);
        let end = tail.find(|c: char| matches!(c, '\\' | '/' | '"' | '\'' | '\n' | '\r' | '\t' | ' ' | ')' | ']' | ',' | ';')).unwrap_or(tail.len());
        let name = &tail[..end];
        let generic = ["public", "default", "all users", "default user"].iter().any(|g| name.eq_ignore_ascii_case(g));
        if name.is_empty() || generic || name.starts_with(MARK_OPEN) {
            out.push_str(name);
        } else {
            out.push_str(with);
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// Where `pattern` (lower-case ASCII) starts in `text`, ignoring ASCII case.
fn find_ignoring_case(text: &str, pattern: &str) -> Option<usize> {
    let (haystack, needle) = (text.as_bytes(), pattern.as_bytes());
    (0..=haystack.len().checked_sub(needle.len())?).find(|&at| haystack.get(at..at + needle.len()).is_some_and(|window| window.eq_ignore_ascii_case(needle)))
}

/// E-mail addresses: `local@domain.tld`.
fn replace_emails(text: &str, with: &str) -> String {
    let local = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-');
    let domain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-');
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('@') {
        let before = &rest[..at];
        let start = before.char_indices().rev().take_while(|(_, c)| local(*c)).last().map_or(at, |(i, _)| i);
        let after = &rest[at + 1..];
        let end = after.char_indices().take_while(|(_, c)| domain(*c)).last().map_or(0, |(i, c)| i + c.len_utf8());
        let host = after[..end].trim_end_matches(['.', '-']);
        let tld_ok = host.rsplit_once('.').is_some_and(|(name, tld)| !name.is_empty() && tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()));
        if start < at && tld_ok {
            out.push_str(&rest[..start]);
            out.push_str(with);
            rest = &after[host.len()..];
        } else {
            out.push_str(&rest[..=at]);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Windows account identifiers of users (`S-1-5-21-…`), which name an account on a machine.
fn replace_sids(text: &str, with: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = find_ignoring_case(rest, "s-1-5-21-") {
        let after = &rest[at + "s-1-5-21-".len()..];
        let end = after.find(|c: char| !(c.is_ascii_digit() || c == '-')).unwrap_or(after.len());
        let body = after[..end].trim_end_matches('-');
        out.push_str(&rest[..at]);
        if body.is_empty() {
            out.push_str(&rest[at..at + "s-1-5-21-".len()]);
            rest = after;
        } else {
            out.push_str(with);
            rest = &after[body.len()..];
        }
    }
    out.push_str(rest);
    out
}

/// Hardware addresses (a Bluetooth headset's, in a device path): six pairs of hex digits joined by
/// `:` or `-`, or twelve hex digits after `_`, `&`, `#` or `\` (a GUID's last group, after `-`,
/// stays).
fn replace_macs(text: &str, with: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let hex = |i: usize| chars.get(i).is_some_and(|c| c.is_ascii_hexdigit());
    let word = |i: Option<usize>| i.and_then(|i| chars.get(i)).is_some_and(|c| c.is_ascii_alphanumeric());
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let before = i.checked_sub(1);
        if !word(before) {
            // aa:bb:cc:dd:ee:ff or aa-bb-cc-dd-ee-ff
            let separated = (0..6).all(|pair| hex(i + pair * 3) && hex(i + pair * 3 + 1) && (pair == 5 || matches!(chars.get(i + pair * 3 + 2), Some(':') | Some('-'))))
                && chars.get(i + 2) == chars.get(i + 5)
                && !word(Some(i + 17));
            if separated {
                out.push_str(with);
                i += 17;
                continue;
            }
            let run = (0..12).all(|k| hex(i + k)) && !word(Some(i + 12)) && before.and_then(|b| chars.get(b)).is_some_and(|c| matches!(c, '_' | '&' | '#' | '\\'));
            if run && (0..12).any(|k| chars.get(i + k).is_some_and(|c| c.is_ascii_digit())) {
                out.push_str(with);
                i += 12;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The GitHub issue.

/// The link that opens GitHub's "new issue" page pre-filled, and whether the report had to be cut
/// to fit in it (the full text then goes to the clipboard, and the issue says so).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueLink {
    pub url: String,
    pub truncated: bool,
}

/// The issue's title: the program, the version and the reason, short.
pub fn issue_title(anonymized_report: &str) -> String {
    let (program, reason) = header(anonymized_report);
    let version = anonymized_report.lines().find_map(|line| line.strip_prefix("version: ")).unwrap_or("").trim().to_string();
    let mut title = format!("Crash: {} {} — {}", if program.is_empty() { "Platinum" } else { &program }, version, reason);
    if title.chars().count() > 110 {
        title = title.chars().take(109).collect::<String>() + "…";
    }
    title
}

/// The note put at the end of a cut report.
pub const TRUNCATED_NOTE: &str = "(Report cut to fit in the link: the full anonymised text was copied to the clipboard; paste it below.)";

/// The issue's body: a line saying where it comes from, the report in a code block, and the note
/// when it was cut.
fn issue_body(report: &str, truncated: bool) -> String {
    let mut body = String::from("Crash report written by Platinum, anonymised on the user's PC and read by them before sending.\n\n```text\n");
    body.push_str(report.trim_end());
    body.push_str("\n```\n");
    if truncated {
        body.push('\n');
        body.push_str(TRUNCATED_NOTE);
        body.push('\n');
    }
    body
}

/// The "new issue" link for `repo`, with the anonymised report, at most `MAX_URL` long: a longer
/// report is cut at a line, keeping its start (the header and the reason).
pub fn issue_link(repo: &str, anonymized_report: &str) -> IssueLink {
    issue_link_within(repo, anonymized_report, MAX_URL)
}

fn issue_link_within(repo: &str, report: &str, limit: usize) -> IssueLink {
    let base = format!("https://github.com/{repo}/issues/new?title={}&body=", percent_encode(&issue_title(report)));
    let full = format!("{base}{}", percent_encode(&issue_body(report, false)));
    if full.len() <= limit {
        return IssueLink { url: full, truncated: false };
    }
    // Lines are kept while the link stays within the limit (an encoded text's length is the sum of
    // its parts').
    let lines: Vec<&str> = report.lines().collect();
    let mut length = base.len() + percent_encode(&issue_body("", true)).len();
    let mut count = 0;
    for line in &lines {
        let more = percent_encode(line).len() + if count > 0 { 3 } else { 0 };
        if length + more > limit {
            break;
        }
        length += more;
        count += 1;
    }
    IssueLink { url: format!("{base}{}", percent_encode(&issue_body(&lines[..count].join("\n"), true))), truncated: true }
}

/// Percent-encoding for a URL's query: everything but `A-Z a-z 0-9 - . _ ~`, as UTF-8 bytes.
pub fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3);
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> Identity {
        Identity {
            profile: Some(r"C:\Users\Jean Dupont".into()),
            user: Some("jdupont".into()),
            computer: Some("JEAN-PC".into()),
            devices: vec![
                DeviceName { name: "Casque (AirPods de Jean)".into(), input: false },
                DeviceName { name: "Microphone (Razer Seiren Mini)".into(), input: true },
                DeviceName { name: "Gaming (Platinum)".into(), input: false },
                DeviceName { name: "SteelSeries Sonar - Chat (SteelSeries Sonar Virtual Audio Device)".into(), input: false },
                DeviceName { name: "Haut-parleurs (Realtek(R) Audio)".into(), input: false },
            ],
        }
    }

    #[test]
    fn the_user_and_the_pc_are_taken_out() {
        let text = "relay from C:\\Users\\Jean Dupont\\Platinum\\platinum.exe and c:/users/jean dupont/AppData\nuser JDUPONT on jean-pc (JEAN-PC.local); C:\\Users\\Someone\\x and C:\\Users\\Public\\y\nmail jean.dupont+gg@example.co.uk, not an@address, nor @x.y\nSID S-1-5-21-1004336348-1177238915-682003330-1001\\Software";
        let out = anonymize(text, &identity());
        assert_eq!(
            out,
            "relay from C:\\Users\\<user>\\Platinum\\platinum.exe and c:/users/<user>/AppData\nuser <user> on <pc> (<pc>.local); C:\\Users\\<user>\\x and C:\\Users\\Public\\y\nmail <e-mail>, not an@address, nor @x.y\nSID <sid>\\Software"
        );
        // The folder name alone too, whatever its case; a longer word keeps its letters.
        assert_eq!(anonymize("JEAN DUPONT said; jdupontx stays", &identity()), "<user> said; jdupontx stays");
    }

    #[test]
    fn real_devices_get_generic_labels_and_channels_stay() {
        let text = "relaying Microphone (Razer Seiren Mini) into Microphone (Platinum)\ndefault output Casque (AirPods de Jean) (fallback)\nAirPods de Jean unplugged; Gaming (Platinum) plays to Haut-parleurs (Realtek(R) Audio)\nSteelSeries Sonar - Chat (SteelSeries Sonar Virtual Audio Device)";
        let out = anonymize(text, &identity());
        assert_eq!(out, "relaying <input 1> into Microphone (Platinum)\ndefault output <output 1> (fallback)\n<output 1> unplugged; Gaming (Platinum) plays to <output 2>\nSteelSeries Sonar - Chat (SteelSeries Sonar Virtual Audio Device)");
        assert!(is_channel_name("Stream (Platinum)") && is_channel_name("SteelSeries Sonar - Gaming") && !is_channel_name("Haut-parleurs (SteelSeries Arena 7)"));
    }

    #[test]
    fn the_device_names_come_from_windows_and_platinum_ini() {
        use crate::devices::{Endpoint, Flow, State};
        let gaming = Endpoint { id: "{g}".into(), name: "Gaming (Platinum)".into(), flow: Flow::Render, state: State::Active, virtual_role: Some(2) };
        let airpods = Endpoint { id: "{a}".into(), name: "Casque (AirPods de Jean)".into(), flow: Flow::Render, state: State::Unplugged, virtual_role: None };
        let seiren = Endpoint { id: "{s}".into(), name: "Microphone (Razer Seiren Mini)".into(), flow: Flow::Capture, state: State::Active, virtual_role: None };
        let config = crate::config::Config {
            microphone: Some("Seiren".into()),
            fallback_output: Some("{0.0.0.00000000}.{x}".into()),
            output_priority: vec![crate::fallback::PriorityDevice::new("{old}", "Enceintes de Marie"), crate::fallback::PriorityDevice::new("{a}", "casque (airpods de jean)")],
            ..crate::config::Config::default()
        };
        let names = device_names(&[gaming, airpods, seiren], &config);
        let shown: Vec<(&str, bool)> = names.iter().map(|d| (d.name.as_str(), d.input)).collect();
        assert_eq!(shown, [("Casque (AirPods de Jean)", false), ("Microphone (Razer Seiren Mini)", true), ("Enceintes de Marie", false), ("Seiren", true)]);
    }

    #[test]
    fn markers_and_odd_names_cannot_be_abused() {
        // A user called "user" does not eat the labels; a text holding the marker characters loses them.
        let id = Identity { user: Some("user".into()), computer: Some("pc1".into()), ..Identity::default() };
        assert_eq!(anonymize("user \u{E000}0\u{E001} pc1", &id), "<user> ?0? <pc>");
        // Names too short to be told from words are left to the path rule.
        let short = Identity { user: Some("al".into()), profile: Some(r"C:\Users\al".into()), ..Identity::default() };
        assert_eq!(anonymize(r"al is in C:\Users\al\x, also /Users/al", &short), r"al is in C:\Users\<user>\x, also /Users/<user>");
        // A profile outside C:\Users.
        let elsewhere = Identity { profile: Some(r"D:\Profils\marie".into()), ..Identity::default() };
        assert_eq!(anonymize(r"D:\profils\Marie\Platinum\daemon.log by marie", &elsewhere), r"D:\profils\<user>\Platinum\daemon.log by <user>");
        assert_eq!(anonymize("", &identity()), "");
    }

    #[test]
    fn hardware_addresses_go_and_guids_stay() {
        let text = r"BTHENUM\{0000111e-0000-1000-8000-00805f9b34fb}_VID&0002004c_PID&200e\7&1c8e2b&0&A0B1C2D3E4F5_C00000000 a0:b1:c2:d3:e4:f5 {0.0.0.00000000}.{83e15a44-1234-5678-9abc-def012345678}";
        let out = anonymize(text, &Identity::default());
        assert!(out.contains("&0&<mac>_C00000000") && out.contains(" <mac> "), "{out}");
        assert!(out.contains("{83e15a44-1234-5678-9abc-def012345678}") && out.contains("00805f9b34fb}"), "{out}");
    }

    #[test]
    fn reports_round_trip_through_their_header() {
        let report = Report {
            program: "mic-worker".into(),
            version: "0.1.0".into(),
            windows: "10.0.26200".into(),
            time: "2026-11-22 15:30:00".into(),
            pid: 4242,
            thread: "17 (main)".into(),
            reason: "access violation (0xC0000005) in Sonar.APO.dll+0x1A2B\nsecond line".into(),
            recorded_by: "daemon".into(),
            details: vec!["exit code 0xC0000005 (access violation)".into()],
            stack: vec!["platinum.exe+0x10".into()],
            log_name: "daemon.log".into(),
            log_tail: vec!["[20261122-152959] relaying".into()],
        };
        let text = report.to_text();
        assert!(text.starts_with("Platinum crash report\nprogram: mic-worker\nversion: 0.1.0\n"), "{text}");
        assert!(text.contains("\nreason: access violation (0xC0000005) in Sonar.APO.dll+0x1A2B second line\nrecorded by: daemon\n"), "{text}");
        assert!(text.contains("\ndaemon.log, last 1 lines:\n  [20261122-152959] relaying\n"), "{text}");
        assert_eq!(header(&text), ("mic-worker".to_string(), "access violation (0xC0000005) in Sonar.APO.dll+0x1A2B second line".to_string()));
        assert_eq!(readable_stamp("20261122-153000"), "2026-11-22 15:30:00");
        assert_eq!(readable_stamp("later"), "later");
    }

    #[test]
    fn reports_are_kept_listed_offered_and_cleared() {
        let folder = std::env::temp_dir().join(format!("platinum-crash-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        for second in 0..25 {
            let name = format!("20261122-1530{second:02}-daemon.txt");
            std::fs::write(folder.join(&name), format!("Platinum crash report\nprogram: daemon\nreason: panic {second}\n")).unwrap();
            std::fs::write(folder.join(name.replace(".txt", ".dmp")), b"MDMP").unwrap();
        }
        std::fs::write(folder.join("20261122-999999-orphan.dmp"), b"MDMP").unwrap();
        std::fs::write(folder.join("notes.txt"), b"not a report").unwrap();
        prune(&folder, KEEP);
        let listed = list_in(&folder);
        assert_eq!(listed.len(), 20);
        assert_eq!((listed[0].reason.as_str(), listed[0].stamp.as_str(), listed[0].program.as_str()), ("panic 24", "20261122-153024", "daemon"));
        assert_eq!(listed[19].reason, "panic 5");
        assert!(!folder.join("20261122-153004-daemon.dmp").exists() && folder.join("20261122-153005-daemon.dmp").exists());
        assert!(!folder.join("20261122-999999-orphan.dmp").exists() && folder.join("notes.txt").exists());
        // A second report in the same second takes the next name.
        let names: Vec<String> = candidate_names("20261122-153024", "daemon").take(2).collect();
        assert_eq!(names, ["20261122-153024-daemon.txt", "20261122-153024-daemon-2.txt"]);
        // The newest not yet offered is offered.
        let offered = vec![listed[0].name.clone()];
        assert_eq!(to_offer(&listed, &offered).map(|s| s.reason.as_str()), Some("panic 23"));
        assert_eq!(to_offer(&listed[..1], &offered), None);
        assert_eq!(clear_in(&folder).unwrap(), 20);
        assert!(list_in(&folder).is_empty() && folder.join("notes.txt").exists());
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_report_written_twice_in_a_second_keeps_both() {
        let folder = std::env::temp_dir().join(format!("platinum-crash-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        let report = Report::new("platinum", "panic: test".into());
        let first = write_in(&folder, &report).unwrap();
        let second = write_in(&folder, &report).unwrap();
        assert_ne!(first, second);
        assert_eq!(list_in(&folder).len(), 2);
        append(&first, "seen by the daemon", &["ended after 3 s".into()]).unwrap();
        assert!(read(&first).unwrap().ends_with("\nseen by the daemon:\n  ended after 3 s\n"));
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn log_tails_keep_whole_lines() {
        assert_eq!(last_lines("cut line\na\n\nb\nc\n", 2, true), ["b", "c"]);
        assert_eq!(last_lines("a\nb", 5, false), ["a", "b"]);
        assert!(last_lines("", 5, false).is_empty());
        assert!(tail_lines(Path::new(r"C:\nowhere\missing.log"), 5).is_empty());
    }

    #[test]
    fn crash_codes_are_windows_statuses() {
        assert!(is_crash_code(Some(0xC000_0005_u32 as i32)) && is_crash_code(Some(0xC000_0409_u32 as i32)) && is_crash_code(Some(0xE06D_7363_u32 as i32)));
        assert!(!is_crash_code(Some(0)) && !is_crash_code(Some(1)) && !is_crash_code(Some(2)) && !is_crash_code(None));
        assert!(!is_crash_code(Some(0xC000_013A_u32 as i32)));
        assert_eq!(exit_description(Some(0xC000_0005_u32 as i32)), "exit code 0xC0000005 (access violation)");
        assert_eq!(exit_description(Some(0xC123_4567_u32 as i32)), "exit code 0xC1234567");
        assert_eq!(exit_description(Some(1)), "exit code 1");
        assert!(exception_name(0xC000_00FD).contains("stack overflow"));
    }

    #[test]
    fn the_repository_is_known_only_once_set() {
        assert_eq!(valid_repository(REPO_PLACEHOLDER), None);
        assert_eq!(valid_repository("someone/platinum"), Some("someone/platinum"));
        assert_eq!(valid_repository(" a-b_c.d/e.f "), Some("a-b_c.d/e.f"));
        for bad in ["", "platinum", "a/b/c", "/b", "a/", "a b/c", "a/b?x=1"] {
            assert_eq!(valid_repository(bad), None, "{bad}");
        }
        assert_eq!(valid_repository(OFFICIAL_REPO), Some(OFFICIAL_REPO));
        // This build: Platinum's own repository unless PLATINUM_REPO was set.
        assert_eq!(repository(), option_env!("PLATINUM_REPO").map_or(Some(OFFICIAL_REPO), valid_repository));
    }

    #[test]
    fn issue_links_are_encoded_and_fit() {
        let report = "Platinum crash report\nprogram: daemon\nversion: 0.1.0\nreason: panic: é & ?\n";
        let link = issue_link("someone/platinum", report);
        assert!(!link.truncated);
        assert!(link.url.starts_with("https://github.com/someone/platinum/issues/new?title=Crash%3A%20daemon%200.1.0%20%E2%80%94%20panic%3A%20%C3%A9%20%26%20%3F&body="), "{}", link.url);
        assert!(link.url.contains("%60%60%60text%0APlatinum%20crash%20report%0A"));
        assert_eq!(percent_encode("a-b_c.d~e f/é"), "a-b_c.d~e%20f%2F%C3%A9");
        // A long report is cut at a line, says so, and stays within the limit.
        let long: String = report.to_string() + &(0..2000).map(|i| format!("  line {i} of the log\n")).collect::<String>();
        let link = issue_link("someone/platinum", &long);
        assert!(link.truncated && link.url.len() <= MAX_URL, "{}", link.url.len());
        assert!(link.url.contains(&percent_encode(TRUNCATED_NOTE)) && link.url.contains("program%3A%20daemon"));
        assert!(link.url.contains(&percent_encode("\n```\n")));
        let title = issue_title(&format!("program: daemon\nversion: 0.1.0\nreason: {}\n", "x".repeat(300)));
        assert_eq!(title.chars().count(), 110);
    }

    #[test]
    fn consent_is_read_leniently() {
        assert_eq!(Consent::default(), Consent::Off);
        assert_eq!(Consent::parse(" Ask "), Some(Consent::Ask));
        assert_eq!(Consent::parse("off"), Some(Consent::Off));
        assert_eq!(Consent::parse("maybe"), None);
        for consent in [Consent::Off, Consent::Ask] {
            assert_eq!(Consent::parse(consent.key()), Some(consent));
        }
    }

    #[test]
    fn native_buffers_cut_rather_than_overflow() {
        let mut bytes = [0u8; 8];
        let mut buf = Buf { bytes: &mut bytes, len: 0 };
        let _ = write!(buf, "0x{:08X}!", 0xC000_0005_u32);
        assert_eq!(buf.filled(), b"0xC00000");
        let mut wide = [0u16; 12];
        let mut path = WidePath { units: &mut wide, len: 0 };
        path.push("ab");
        path.push_number(7, 2);
        path.push_number(2026, 4);
        assert!(path.terminate());
        assert_eq!(String::from_utf16_lossy(&wide[..8]), "ab072026");
        let mut tiny = [0u16; 2];
        let mut full = WidePath { units: &mut tiny, len: 0 };
        full.push("abc");
        assert!(!full.terminate());
        let mut module = [0u16; MODULE_UNITS];
        let (length, _) = module_at(stack_lines as fn(u32) -> Vec<String> as usize, &mut module).unwrap();
        assert!(String::from_utf16_lossy(&module[..length]).to_ascii_lowercase().ends_with(".exe"));
        assert!(!stack_lines(0).is_empty());
    }
}
