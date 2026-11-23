#![windows_subsystem = "windows"]
//! platinum-start: starts `platinum.exe`, found next to this program, with the arguments given,
//! without any window, its output appended to `daemon.log` in the same folder, and exits at once.
//! This is what Windows runs at logon: `platinum-start.exe daemon` (the daemon then reads its
//! configuration file).
//!
//! Having no window, it cannot show its own failures: it appends them, time-stamped, to the log,
//! or to `%TEMP%\platinum-daemon.log` when the log itself cannot be opened, and publishes a failed
//! status for the window when the daemon cannot be started at all.

use std::fs::File;
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// CREATE_NO_WINDOW: the console program gets a console nobody sees.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Past this size the log is set aside once, so it never grows without bound.
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

fn main() {
    // A crash of the launcher leaves a report too (`platinum::crash`).
    platinum::crash::install(platinum::crash::LAUNCHER);
    let Ok(me) = std::env::current_exe() else { return };
    let Some(folder) = me.parent() else { return };
    let mut log = open_log(&folder.join("daemon.log"));
    let program = folder.join("platinum.exe");
    let mut command = Command::new(&program);
    command.args(std::env::args_os().skip(1)).current_dir(folder).stdin(Stdio::null()).creation_flags(CREATE_NO_WINDOW);
    // Without a log the daemon still starts: the sound matters more than its output.
    match log.as_ref().and_then(|file| Some((file.try_clone().ok()?, file.try_clone().ok()?))) {
        Some((out, err)) => command.stdout(out).stderr(err),
        None => command.stdout(Stdio::null()).stderr(Stdio::null()),
    };
    if let Err(error) = command.spawn() {
        note(&mut log, &format!("cannot start {}: {error}", program.display()));
        // The window reads this; a daemon that already runs keeps its own status.
        if !platinum::daemon::is_running() {
            let mut status = platinum::status::Status::new(platinum::status::State::Failed);
            status.pid = 0;
            status.message = format!("Impossible de lancer {} : {error}", program.display());
            let _ = platinum::status::write(&status);
        }
    }
}

/// The log, set aside first when it grew too large; when it cannot be opened, a log in %TEMP%
/// that says why.
fn open_log(path: &Path) -> Option<File> {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let _ = std::fs::rename(path, path.with_file_name("daemon.old.log"));
    }
    let error = match append(path) {
        Ok(file) => return Some(file),
        Err(error) => error,
    };
    let fallback: PathBuf = std::env::temp_dir().join("platinum-daemon.log");
    let mut file = append(&fallback).ok();
    note(&mut file, &format!("cannot open {} ({error}): the daemon's output goes to {}", path.display(), fallback.display()));
    file
}

fn append(path: &Path) -> std::io::Result<File> {
    std::fs::OpenOptions::new().create(true).append(true).open(path)
}

/// One time-stamped line from the launcher itself.
fn note(log: &mut Option<File>, message: &str) {
    if let Some(file) = log {
        let _ = writeln!(file, "[{}] platinum-start: {message}", platinum::guard::stamp());
    }
}
