//! What the daemon is doing, published for the window and the tray in
//! `%USERPROFILE%\Platinum\status.txt`.
//!
//! The daemon rewrites the file at every change of state (through a temporary file renamed over
//! it, so that a reader never sees half of it). The file is plain `key = value` lines:
//! `state`, `pid`, `microphone` (device name), `effects`, `message` (short, may be technical),
//! `remote` (the streaming output followed during a remote session, see `remote`; empty outside
//! one), `worker` (the microphone worker's process, see `worker`; 0 when none runs), `replaced`
//! (the last replacement of that worker: when, at how much memory, how long the microphone was
//! cut; empty before the first), `audio_frozen` (when the daemon found Windows' audio service no
//! longer answering, see `audiowatch`; empty while it answers) and `updated` (local time,
//! `YYYYMMDD-HHMMSS`).
//!
//! A daemon killed outright (Task Manager, a crash) cannot say it stopped: the reader checks that
//! the process that wrote the file still runs, and reports it stopped otherwise.

use crate::i18n::{self, mark, tr};
use crate::trf;
use std::ffi::c_void;
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = "status.txt";
/// What the window, the tray and `daemon --status` say while Windows' audio service is frozen.
pub const AUDIO_FROZEN: &str = mark("Le service Audio de Windows ne répond plus : le son est figé");

/// The daemon's state, as written in the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Started, nothing brought up yet.
    Starting,
    /// The channels or the microphone are not ready (devices missing, disabled, unplugged), or no
    /// microphone is chosen: retried every 5 s.
    WaitingDevices,
    /// The channels are up and the microphone is relayed as asked.
    Running,
    /// The microphone is relayed without the effects that were asked for (the engine failed).
    Degraded,
    /// Putting the previous default devices back.
    Stopping,
    Stopped,
    /// Ended on an error, or refused to start (the message says why).
    Failed,
}

impl State {
    pub const ALL: [State; 7] = [State::Starting, State::WaitingDevices, State::Running, State::Degraded, State::Stopping, State::Stopped, State::Failed];

    /// The word used in the file.
    pub fn key(self) -> &'static str {
        match self {
            State::Starting => "starting",
            State::WaitingDevices => "waiting_devices",
            State::Running => "running",
            State::Degraded => "degraded",
            State::Stopping => "stopping",
            State::Stopped => "stopped",
            State::Failed => "failed",
        }
    }

    pub fn from_key(key: &str) -> Option<State> {
        State::ALL.into_iter().find(|state| state.key() == key.trim())
    }

    /// A short label, for a status line, in the current language.
    pub fn label(self) -> &'static str {
        match self {
            State::Starting => tr("Démarrage"),
            State::WaitingDevices => tr("En attente"),
            State::Running => tr("Actif"),
            State::Degraded => tr("Sans effets"),
            State::Stopping => tr("Arrêt en cours"),
            State::Stopped => tr("Arrêté"),
            State::Failed => tr("Échec"),
        }
    }

    /// Whether the daemon process is still there in this state (it exits after the last two).
    pub fn is_live(self) -> bool {
        !matches!(self, State::Stopped | State::Failed)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    pub state: State,
    /// The daemon's process.
    pub pid: u32,
    /// The real microphone's device name (or, before it is found, the name asked for).
    pub microphone: String,
    /// Whether the microphone goes through the effects engine right now.
    pub effects: bool,
    /// Why the daemon is waiting, degraded or failed, or what it is doing; may be empty.
    pub message: String,
    /// During a remote streaming session followed by the daemon, the streaming output the channels
    /// play to (its short name, "Steam Streaming Speakers"); empty otherwise.
    pub remote: String,
    /// The microphone worker's process (it hosts the relay and the effects engine, see `worker`);
    /// 0 while none runs (no microphone relayed, or relayed inside the daemon).
    pub worker: u32,
    /// The last replacement of the microphone worker, for `platinum doctor` and `daemon --status`:
    /// "20261122-153000, 412 MB, gap 140 ms"; empty before the first.
    pub replaced: String,
    /// When the daemon found Windows' audio service frozen (local time, `YYYYMMDD-HHMMSS`); empty
    /// while the service answers. Every program's sound is stuck meanwhile (`audiowatch`).
    pub audio_frozen: String,
    /// When the file was written, local time, `YYYYMMDD-HHMMSS`.
    pub updated: String,
    /// Set by the reader, never written: the file says the daemon runs, but its process is gone,
    /// so it ended without its cleanup (killed, or crashed). `state` then reads `Stopped`.
    pub vanished: bool,
}

impl Status {
    /// A status of this process, stamped now.
    pub fn new(state: State) -> Status {
        Status {
            state,
            pid: std::process::id(),
            microphone: String::new(),
            effects: false,
            message: String::new(),
            remote: String::new(),
            worker: 0,
            replaced: String::new(),
            audio_frozen: String::new(),
            updated: crate::guard::stamp(),
            vanished: false,
        }
    }

    /// The file's text.
    pub fn to_text(&self) -> String {
        format!(
            "# État de la tâche de fond de Platinum, réécrit par elle à chaque changement.\nstate = {}\npid = {}\nmicrophone = {}\neffects = {}\nmessage = {}\nremote = {}\nworker = {}\nreplaced = {}\naudio_frozen = {}\nupdated = {}\n",
            self.state.key(),
            self.pid,
            single_line(&self.microphone, 200),
            self.effects,
            single_line(&self.message, 300),
            single_line(&self.remote, 200),
            self.worker,
            single_line(&self.replaced, 200),
            single_line(&self.audio_frozen, 40),
            single_line(&self.updated, 40),
        )
    }

    /// Reads the file's text; `None` without a valid `state`. Missing keys stay empty.
    pub fn parse(text: &str) -> Option<Status> {
        let mut status = Status { state: State::Stopped, pid: 0, microphone: String::new(), effects: false, message: String::new(), remote: String::new(), worker: 0, replaced: String::new(), audio_frozen: String::new(), updated: String::new(), vanished: false };
        let mut state = None;
        for line in text.trim_start_matches('\u{feff}').lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim();
            match key.trim() {
                "state" => state = State::from_key(value),
                "pid" => status.pid = value.parse().unwrap_or(0),
                "microphone" => status.microphone = value.to_string(),
                "effects" => status.effects = value == "true",
                "message" => status.message = value.to_string(),
                "remote" => status.remote = value.to_string(),
                "worker" => status.worker = value.parse().unwrap_or(0),
                "replaced" => status.replaced = value.to_string(),
                "audio_frozen" => status.audio_frozen = value.to_string(),
                "updated" => status.updated = value.to_string(),
                _ => {}
            }
        }
        status.state = state?;
        Some(status)
    }

    /// The message in the current language. The daemon writes its messages in French, the
    /// catalogs' keys, so that the window and the tray can each say them in their own language.
    pub fn message_text(&self) -> String {
        i18n::tr_message(&self.message)
    }

    /// One sentence for the window, in the current language: what the sound and the microphone
    /// are doing.
    pub fn summary(&self) -> String {
        let with_message = |text: &str| if self.message.is_empty() { tr(text).to_string() } else { trf!("{text} : {detail}", text = tr(text), detail = self.message_text()) };
        if self.audio_frozen() {
            return tr(AUDIO_FROZEN).into();
        }
        match self.state {
            State::Starting => tr("Démarrage…").into(),
            State::WaitingDevices if self.message.is_empty() => tr("En attente des périphériques audio").into(),
            State::WaitingDevices => self.message_text(),
            State::Running if self.microphone.is_empty() => tr("Son actif, sans micro").into(),
            State::Running if self.effects => tr("Son et micro actifs, avec effets").into(),
            State::Running => tr("Son et micro actifs, sans effets").into(),
            State::Degraded => with_message(mark("Micro relayé sans effets")),
            State::Stopping => tr("Arrêt en cours…").into(),
            State::Stopped if self.vanished => tr("Arrêté brusquement, sans remettre les périphériques en place").into(),
            State::Stopped => tr("Arrêté").into(),
            State::Failed => with_message(mark("Échec")),
        }
    }

    /// Whether the daemon says Windows' audio service no longer answers: while it runs, and after a
    /// stop it had to force because of that service (`Failed`, the freeze outlasting it). A daemon
    /// that stopped normally, or vanished, says nothing any more.
    pub fn audio_frozen(&self) -> bool {
        !self.audio_frozen.is_empty() && ((self.state.is_live() && !self.vanished) || self.state == State::Failed)
    }

    /// The tray icon's tooltip: Platinum, the summary, the microphone while it is relayed, and the
    /// streaming output during a remote session. Cut to the 127 characters Windows shows.
    pub fn tooltip(&self) -> String {
        let mut text = format!("Platinum\n{}", self.summary());
        if matches!(self.state, State::Running | State::Degraded) && !self.microphone.is_empty() {
            text.push('\n');
            text.push_str(&self.microphone);
        }
        if self.state.is_live() && !self.remote.is_empty() {
            text.push('\n');
            text.push_str(&trf!("Streaming à distance : {output}", output = self.remote));
        }
        shorten(&text, 127)
    }
}

/// `%USERPROFILE%\Platinum\status.txt`.
pub fn path() -> Result<PathBuf, String> {
    Ok(crate::autostart::install_dir()?.join(FILE_NAME))
}

/// The daemon's last published status; `None` when it never wrote one. A status left by a process
/// that is gone reads `Stopped`, with `vanished` set.
pub fn read() -> Option<Status> {
    read_from(&path().ok()?)
}

pub fn read_from(file: &Path) -> Option<Status> {
    read_checked(file, process_alive)
}

/// Reads a status, telling whether its writer still runs with `alive`.
fn read_checked(file: &Path, alive: impl Fn(u32) -> bool) -> Option<Status> {
    let bytes = std::fs::read(file).ok()?;
    let mut status = Status::parse(&String::from_utf8_lossy(&bytes))?;
    if status.state.is_live() && !alive(status.pid) {
        status.state = State::Stopped;
        status.vanished = true;
    }
    Some(status)
}

/// Publishes a status (the daemon does this; nothing else should).
pub fn write(status: &Status) -> Result<(), String> {
    write_to(&path()?, status)
}

pub fn write_to(file: &Path, status: &Status) -> Result<(), String> {
    crate::config::write_atomically(file, &status.to_text())
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
    fn GetExitCodeProcess(process: *mut c_void, code: *mut u32) -> i32;
    fn QueryFullProcessImageNameW(process: *mut c_void, flags: u32, name: *mut u16, size: *mut u32) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const STILL_ACTIVE: u32 = 259;

/// The programs a daemon runs as: `platinum.exe`, or `platinum.old.exe` after a swap while it ran.
/// The window (`platinum-ui.exe`) and the launcher (`platinum-start.exe`) never write a live status,
/// so a pid of theirs is a dead daemon's pid reused.
const DAEMON_PROGRAMS: [&str; 2] = ["platinum.exe", "platinum.old.exe"];

/// Whether `pid` is a running daemon program. Checking the exact name keeps a pid reused by another
/// program, Platinum's window and launcher included, from passing.
pub fn process_alive(pid: u32) -> bool {
    process_named(pid, &DAEMON_PROGRAMS)
}

/// Whether `pid` runs and its program's file name is one of `names` (ignoring case).
fn process_named(pid: u32, names: &[&str]) -> bool {
    let Some(path) = process_path(pid) else { return false };
    let file = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    names.iter().any(|name| file.eq_ignore_ascii_case(name))
}

/// The full path of the program `pid` runs, while it runs (the window finds the daemon's log next
/// to it); `None` once it has ended or when it cannot be opened.
pub fn process_path(pid: u32) -> Option<PathBuf> {
    if pid == 0 {
        return None;
    }
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return None;
        }
        let mut code = 0u32;
        let running = GetExitCodeProcess(process, &mut code) != 0 && code == STILL_ACTIVE;
        let mut name = [0u16; 1024];
        let mut size = name.len() as u32;
        let named = QueryFullProcessImageNameW(process, 0, name.as_mut_ptr(), &mut size) != 0;
        CloseHandle(process);
        (running && named).then(|| PathBuf::from(String::from_utf16_lossy(&name[..size as usize])))
    }
}

/// The file name of this program, for the tests (`platinum-<hash>.exe`).
#[cfg(test)]
fn own_program() -> String {
    std::env::current_exe().ok().and_then(|exe| exe.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default()
}

/// A value kept on one line and to a reasonable length.
fn single_line(value: &str, limit: usize) -> String {
    let flat: String = value.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    shorten(flat.trim(), limit)
}

/// At most `limit` UTF-16 units (what Windows counts), ending with "…" when cut.
fn shorten(text: &str, limit: usize) -> String {
    if text.encode_utf16().count() <= limit {
        return text.to_string();
    }
    let mut kept = String::new();
    let mut units = 0;
    for c in text.chars() {
        if units + c.len_utf16() > limit - 1 {
            break;
        }
        units += c.len_utf16();
        kept.push(c);
    }
    kept.push('…');
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(state: State) -> Status {
        Status {
            state,
            pid: std::process::id(),
            microphone: "Microphone (Razer Seiren Mini)".into(),
            effects: true,
            message: "a = b".into(),
            remote: String::new(),
            worker: 0,
            replaced: String::new(),
            audio_frozen: String::new(),
            updated: "20261115-190000".into(),
            vanished: false,
        }
    }

    #[test]
    fn every_state_survives_a_round_trip() {
        for state in State::ALL {
            assert_eq!(State::from_key(state.key()), Some(state));
            let mut status = sample(state);
            assert_eq!(Status::parse(&status.to_text()), Some(status.clone()));
            status.remote = "Steam Streaming Speakers".into();
            assert_eq!(Status::parse(&status.to_text()), Some(status.clone()));
            status.worker = 21_360;
            status.replaced = "20261122-153000, 412 MB, gap 140 ms".into();
            assert_eq!(Status::parse(&status.to_text()), Some(status.clone()));
            status.audio_frozen = "20261126-213345".into();
            assert_eq!(Status::parse(&status.to_text()), Some(status));
        }
        // A file written before `remote` and `worker` existed reads as no session and no worker.
        assert_eq!(Status::parse("state = running\npid = 4\n").map(|s| (s.remote, s.worker, s.replaced, s.audio_frozen)), Some((String::new(), 0, String::new(), String::new())));
        assert_eq!(State::from_key("asleep"), None);
        assert_eq!(Status::parse("pid = 4\n"), None);
    }

    #[test]
    fn values_stay_on_one_line() {
        let mut status = sample(State::Failed);
        status.message = "first line\r\nsecond line".into();
        let read = Status::parse(&status.to_text()).unwrap();
        assert_eq!(read.message, "first line  second line");
        status.message = "x".repeat(1000);
        assert_eq!(Status::parse(&status.to_text()).unwrap().message.chars().count(), 300);
    }

    #[test]
    fn a_dead_writer_reads_as_stopped() {
        let folder = std::env::temp_dir().join(format!("platinum-status-test-{}", std::process::id()));
        let file = folder.join(FILE_NAME);
        // This test program (platinum-<hash>.exe) stands in for a live daemon.
        let own = own_program();
        let alive = |pid: u32| process_named(pid, &[own.as_str()]);
        write_to(&file, &sample(State::Running)).unwrap();
        let read = read_checked(&file, alive).unwrap();
        assert_eq!((read.state, read.vanished), (State::Running, false));
        // Read as the window reads it, a program not named like the daemon is a pid reused.
        let read = read_from(&file).unwrap();
        assert_eq!((read.state, read.vanished), (State::Stopped, true));
        let read = read_checked(&file, alive).unwrap();
        assert_eq!((read.state, read.vanished), (State::Running, false));
        let mut gone = sample(State::Degraded);
        gone.pid = 0;
        write_to(&file, &gone).unwrap();
        let read = read_from(&file).unwrap();
        assert_eq!((read.state, read.vanished), (State::Stopped, true));
        assert!(read.summary().contains("brusquement"), "{}", read.summary());
        // A finished daemon is not "vanished": it said it stopped.
        let mut done = sample(State::Failed);
        done.pid = 0;
        write_to(&file, &done).unwrap();
        let read = read_from(&file).unwrap();
        assert_eq!((read.state, read.vanished), (State::Failed, false));
        assert!(read_from(&folder.join("missing.txt")).is_none());
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn process_check_needs_a_platinum_program() {
        assert!(process_named(std::process::id(), &[own_program().as_str()]));
        // The test program is not platinum.exe: a daemon pid reused by it does not pass.
        assert!(!process_alive(std::process::id()));
        assert!(!process_alive(0));
        assert!(!process_alive(u32::MAX - 2));
    }

    #[test]
    fn tooltips_fit_and_say_what_happens() {
        let running = sample(State::Running);
        assert_eq!(running.tooltip(), "Platinum\nSon et micro actifs, avec effets\nMicrophone (Razer Seiren Mini)");
        let mut degraded = sample(State::Degraded);
        degraded.message = "x".repeat(500);
        let tip = degraded.tooltip();
        assert_eq!(tip.encode_utf16().count(), 127);
        assert!(tip.starts_with("Platinum\nMicro relayé sans effets : xxx") && tip.ends_with('…'), "{tip}");
        let mut waiting = sample(State::WaitingDevices);
        waiting.message = "Aucun micro choisi".into();
        assert_eq!(waiting.tooltip(), "Platinum\nAucun micro choisi");
        let mut remote = sample(State::Running);
        remote.remote = "Steam Streaming Speakers".into();
        assert_eq!(remote.tooltip(), "Platinum\nSon et micro actifs, avec effets\nMicrophone (Razer Seiren Mini)\nStreaming à distance : Steam Streaming Speakers");
        let mut frozen = sample(State::Running);
        frozen.audio_frozen = "20261126-213345".into();
        assert!(frozen.audio_frozen());
        assert_eq!(frozen.tooltip(), "Platinum\nLe service Audio de Windows ne répond plus : le son est figé\nMicrophone (Razer Seiren Mini)");
        // A stop forced by the freeze leaves it said; a daemon that stopped normally says nothing.
        frozen.state = State::Failed;
        assert!(frozen.audio_frozen());
        assert_eq!(frozen.summary(), "Le service Audio de Windows ne répond plus : le son est figé");
        frozen.state = State::Stopped;
        assert!(!frozen.audio_frozen());
        assert_eq!(frozen.summary(), "Arrêté");
        assert_eq!(shorten("abc", 3), "abc");
        assert_eq!(shorten("abcd", 3), "ab…");
    }
}
