//! The daemon as the window sees and drives it.
//!
//! What the daemon publishes in status.txt, its single-instance mutex and GG's Sonar process are
//! turned into one state, shown in the header, the banner and the popover (`view`). Starting,
//! stopping and restarting it are jobs that wait on threads of their own (`begin`), so that the
//! engine keeps answering the window while the daemon takes its seconds.
//!
//! The window starts the daemon with `daemon::launch` (`platinum-start.exe daemon` next to this
//! program, else in the install folder): the daemon reads Platinum's configuration file, so nothing
//! depends on the logon command in the registry.

use platinum::daemon;
use platinum::i18n::tr;
use platinum::trf;
use platinum::status::{self, State, Status};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

/// How long a start may take before the window calls it failed.
pub const START_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the daemon may take to put the devices back when asked to stop.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(15);
/// The pause between the stop and the start of a restart.
const SETTLE: Duration = Duration::from_millis(500);

/// The state shown for the daemon.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    Running,
    /// Relays the microphone without the effects that were asked for.
    Degraded,
    Starting,
    /// Up, but the channels or the microphone wait for their devices (or no microphone is chosen).
    Waiting,
    Stopping,
    #[default]
    Stopped,
    Failed,
    /// GG's Sonar runs and keeps the devices.
    Gg,
}

impl Phase {
    /// The name the interface description knows the phase by.
    pub fn key(self) -> &'static str {
        match self {
            Phase::Running => "running",
            Phase::Degraded => "degraded",
            Phase::Starting => "starting",
            Phase::Waiting => "waiting",
            Phase::Stopping => "stopping",
            Phase::Stopped => "stopped",
            Phase::Failed => "failed",
            Phase::Gg => "gg",
        }
    }

    /// The word in the header, in the current language.
    pub fn label(self) -> &'static str {
        match self {
            Phase::Running => tr("actif"),
            Phase::Degraded => tr("sans effets"),
            Phase::Starting => tr("démarrage…"),
            Phase::Waiting => tr("en attente des périphériques"),
            Phase::Stopping => tr("arrêt…"),
            Phase::Stopped => tr("arrêté"),
            Phase::Failed => tr("erreur"),
            Phase::Gg => tr("GG ouvert"),
        }
    }

    /// Whether the sound goes through Platinum in this phase (no banner then).
    pub fn carries_sound(self) -> bool {
        matches!(self, Phase::Running | Phase::Degraded | Phase::Waiting)
    }
}

/// Everything the window shows about the daemon.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DaemonView {
    pub phase: Phase,
    /// One French sentence: what the sound and the microphone are doing.
    pub summary: String,
    /// Why it waits, runs without effects or failed; empty when all is well.
    pub detail: String,
    /// The real microphone relayed (or chosen, when the daemon does not say).
    pub microphone: String,
    pub can_start: bool,
    pub can_stop: bool,
    /// Choosing another microphone restarts the daemon (it relays one now); a daemon waiting for
    /// its first microphone picks the new choice up by itself.
    pub restarts_on_mic_change: bool,
    /// The streaming output the daemon follows during a remote session ("Steam Streaming
    /// Speakers"); empty outside one, or when no daemon runs.
    pub remote: String,
}

/// The part of a job under way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Stopping,
    Starting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Start,
    Stop,
    /// Stop, then start again: a new microphone is taken at start.
    Restart,
}

#[derive(Clone, Debug)]
pub struct Job {
    pub stage: Stage,
    /// Set when the job is over: `Err` holds the French explanation.
    pub outcome: Option<Result<(), String>>,
}

pub type SharedJob = Arc<Mutex<Option<Job>>>;

/// The job under way, if any (a finished one included, until `take_outcome`).
pub fn current(job: &SharedJob) -> Option<Job> {
    job.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// The stage of a job still running.
pub fn active_stage(job: &SharedJob) -> Option<Stage> {
    current(job).filter(|j| j.outcome.is_none()).map(|j| j.stage)
}

/// Removes a finished job and returns how it ended.
pub fn take_outcome(job: &SharedJob) -> Option<Result<(), String>> {
    let mut slot = job.lock().unwrap_or_else(PoisonError::into_inner);
    if slot.as_ref().is_some_and(|j| j.outcome.is_some()) {
        return slot.take().and_then(|j| j.outcome);
    }
    None
}

/// Starts a job on a thread of its own; false when one is already under way.
pub fn begin(job: &SharedJob, kind: Kind) -> bool {
    {
        let mut slot = job.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_some() {
            return false;
        }
        let stage = if kind == Kind::Start { Stage::Starting } else { Stage::Stopping };
        *slot = Some(Job { stage, outcome: None });
    }
    let shared = job.clone();
    let spawned = std::thread::Builder::new().name("tâche de fond".into()).spawn(move || {
        let outcome = run(kind, &shared);
        if let Some(job) = shared.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
            job.outcome = Some(outcome);
        }
    });
    if spawned.is_err() {
        *job.lock().unwrap_or_else(PoisonError::into_inner) = None;
        return false;
    }
    true
}

fn run(kind: Kind, job: &SharedJob) -> Result<(), String> {
    if kind != Kind::Start {
        stop()?;
        if kind == Kind::Stop {
            return Ok(());
        }
        if let Some(job) = job.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
            job.stage = Stage::Starting;
        }
        // The daemon has put the devices back: let Windows settle before they are taken again.
        std::thread::sleep(SETTLE);
    }
    start()
}

fn stop() -> Result<(), String> {
    if !daemon::is_running() {
        return Ok(());
    }
    daemon::request_stop().map_err(|_| tr("Platinum ne répond pas à la demande d'arrêt").to_string())?;
    if daemon::wait_stopped(STOP_TIMEOUT) {
        Ok(())
    } else {
        Err(trf!("Platinum ne s'est pas arrêté en {seconds} secondes", seconds = STOP_TIMEOUT.as_secs()))
    }
}

/// Waits until no job runs any more, at most as long as the longest job (a restart) can take:
/// the window closing must not cut a restart between its stop and its start, which would leave
/// the user without Platinum. True when no job runs.
pub fn wait_idle(job: &SharedJob) -> bool {
    let started = Instant::now();
    let longest = STOP_TIMEOUT + SETTLE + START_TIMEOUT + Duration::from_secs(1);
    while active_stage(job).is_some() {
        if started.elapsed() >= longest {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

/// Launches the daemon, then waits until it says it runs, refuses, or the time is up.
fn start() -> Result<(), String> {
    // What status.txt held before: in a restart, the old daemon's last word ("stopped") was
    // written a moment ago and must not pass for the new daemon's.
    let before = status_stamp();
    let since = Instant::now();
    let launcher = daemon::launch()?;
    while since.elapsed() < START_TIMEOUT {
        std::thread::sleep(Duration::from_millis(250));
        let fresh = fresh_status(before.as_ref());
        match fresh.as_ref().map(|s| s.state) {
            Some(State::Running | State::Degraded | State::WaitingDevices) => return Ok(()),
            Some(State::Failed | State::Stopped) => return Err(refusal(fresh.as_ref())),
            Some(State::Starting | State::Stopping) => {}
            // A daemon of an older version publishes nothing: its mutex is all there is to see.
            None if daemon::is_running() && since.elapsed() > Duration::from_secs(3) => return Ok(()),
            None => {}
        }
    }
    let detail = fresh_status(before.as_ref()).map(|s| s.summary()).filter(|s| !s.is_empty()).or_else(|| log_tail(&launcher));
    Err(match detail {
        Some(detail) => trf!("Platinum n'a pas démarré en {seconds} secondes : {detail}", seconds = START_TIMEOUT.as_secs(), detail = detail),
        None => trf!("Platinum n'a pas démarré en {seconds} secondes, sans rien dire de plus", seconds = START_TIMEOUT.as_secs()),
    })
}

/// Why a daemon that was just started stopped at once.
fn refusal(status: Option<&Status>) -> String {
    match status {
        Some(status) if !status.message.is_empty() => status.message_text(),
        Some(status) => status.summary(),
        None => tr("Platinum s'est arrêté au démarrage").into(),
    }
}

/// When status.txt was last written, and by which process.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Stamp {
    written: SystemTime,
    pid: u32,
}

fn status_stamp() -> Option<Stamp> {
    let file = status::path().ok()?;
    let written = std::fs::metadata(&file).and_then(|m| m.modified()).ok()?;
    let pid = status::read_from(&file).map_or(0, |s| s.pid);
    Some(Stamp { written, pid })
}

/// Whether a status stamped `now` was written after the one stamped `before`: later, or by
/// another process (the new daemon, or platinum-start's own failure with pid 0), in case the file
/// system's clock is too coarse to tell two writes apart. No slack: the old daemon's last status,
/// written half a second before the launch, must not count.
fn is_newer(now: Stamp, before: Option<&Stamp>) -> bool {
    before.is_none_or(|before| now.written > before.written || now.pid != before.pid)
}

/// status.txt, if it was written since `before` was taken.
fn fresh_status(before: Option<&Stamp>) -> Option<Status> {
    let file = status::path().ok()?;
    let written = std::fs::metadata(&file).and_then(|m| m.modified()).ok()?;
    let status = status::read_from(&file)?;
    is_newer(Stamp { written, pid: status.pid }, before).then_some(status)
}

/// The log the launcher writes next to itself (or in %TEMP% when it cannot open that one), as
/// platinum-start.exe does: the window may have started the build folder's launcher, not the
/// installed one.
fn log_files(launcher: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = launcher.parent().map(|folder| folder.join("daemon.log")).into_iter().collect();
    files.push(std::env::temp_dir().join("platinum-daemon.log"));
    files
}

/// The last line of the log of the launcher that was started (it writes its own failures there),
/// from the most recently written of its two logs.
fn log_tail(launcher: &Path) -> Option<String> {
    let newest = log_files(launcher).into_iter().filter_map(|file| std::fs::metadata(&file).and_then(|m| m.modified()).ok().map(|time| (time, file))).max_by_key(|(time, _)| *time)?;
    last_line(&newest.1)
}

fn last_line(file: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut log = std::fs::File::open(file).ok()?;
    let length = log.seek(SeekFrom::End(0)).ok()?;
    log.seek(SeekFrom::Start(length.saturating_sub(2048))).ok()?;
    let mut tail = Vec::new();
    log.read_to_end(&mut tail).ok()?;
    let text = String::from_utf8_lossy(&tail);
    text.lines().rev().map(str::trim).find(|line| !line.is_empty()).map(|line| line.chars().take(200).collect())
}

/// What the window sees of the daemon right now.
#[derive(Clone, Debug, Default)]
pub struct Seen {
    pub status: Option<Status>,
    /// The daemon's mutex exists.
    pub running: bool,
    /// GG's Sonar service runs.
    pub sonar: bool,
}

impl Seen {
    pub fn now() -> Seen {
        Seen { status: status::read(), running: daemon::is_running(), sonar: daemon::sonar_running() }
    }
}

/// The daemon's state for the window, from what is seen, the job under way, the last failure of
/// a start asked from the window, and the microphone the configuration names.
pub fn view(seen: &Seen, stage: Option<Stage>, last_error: Option<&str>, configured_microphone: &str) -> DaemonView {
    let live = seen.status.as_ref().filter(|s| s.state.is_live() && !s.vanished && seen.running);
    let (phase, summary, detail) = match (stage, live) {
        (Some(Stage::Stopping), _) => (Phase::Stopping, tr("Arrêt en cours…").to_string(), String::new()),
        (Some(Stage::Starting), _) => (Phase::Starting, tr("Démarrage…").to_string(), String::new()),
        (None, Some(status)) => (phase_of(status.state), status.summary(), status.message_text()),
        (None, None) if seen.running => (Phase::Running, tr("Platinum fait passer le son").to_string(), String::new()),
        (None, None) if seen.sonar => (Phase::Gg, tr("SteelSeries GG est ouvert et garde la main sur le son").to_string(), tr("Fermez GG pour démarrer Platinum.").to_string()),
        (None, None) => stopped(seen.status.as_ref(), last_error),
    };
    // Without a microphone (none at all, or none waited for), the daemon looks for one on every
    // attempt by itself: choosing one needs no restart.
    let relays = live.is_none_or(|s| !(matches!(s.state, State::WaitingDevices | State::Running) && s.microphone.is_empty()));
    let microphone = live.map(|s| s.microphone.clone()).filter(|m| !m.is_empty()).unwrap_or_else(|| configured_microphone.to_string());
    DaemonView {
        phase,
        summary,
        detail,
        microphone,
        can_start: stage.is_none() && matches!(phase, Phase::Stopped | Phase::Failed),
        can_stop: stage.is_none() && seen.running,
        restarts_on_mic_change: stage.is_none() && seen.running && relays,
        remote: live.map(|s| s.remote.clone()).unwrap_or_default(),
    }
}

fn phase_of(state: State) -> Phase {
    match state {
        State::Starting => Phase::Starting,
        State::WaitingDevices => Phase::Waiting,
        State::Running => Phase::Running,
        State::Degraded => Phase::Degraded,
        State::Stopping => Phase::Stopping,
        State::Stopped => Phase::Stopped,
        State::Failed => Phase::Failed,
    }
}

/// No daemon: stopped, or failed when the last attempt (the window's or the daemon's own) failed.
fn stopped(status: Option<&Status>, last_error: Option<&str>) -> (Phase, String, String) {
    if let Some(error) = last_error {
        return (Phase::Failed, tr("Platinum n'a pas démarré").into(), error.to_string());
    }
    match status {
        Some(status) if status.state == State::Failed => (Phase::Failed, tr("Platinum s'est arrêté sur une erreur").into(), status.message_text()),
        Some(status) if status.vanished => (Phase::Stopped, status.summary(), String::new()),
        _ => (Phase::Stopped, tr("Platinum est arrêté").into(), String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: State, microphone: &str, message: &str) -> Status {
        Status { state, pid: 1, microphone: microphone.into(), effects: true, message: message.into(), remote: String::new(), worker: 0, replaced: String::new(), updated: String::new(), vanished: false }
    }

    fn seen(status: Option<Status>, running: bool, sonar: bool) -> Seen {
        Seen { status, running, sonar }
    }

    #[test]
    fn a_running_daemon_says_what_it_relays() {
        let view = view(&seen(Some(status(State::Running, "Microphone (Seiren)", "")), true, false), None, None, "Seiren");
        assert_eq!(view.phase, Phase::Running);
        assert_eq!(view.microphone, "Microphone (Seiren)");
        assert!(view.can_stop && !view.can_start && view.restarts_on_mic_change);
        let degraded = super::view(&seen(Some(status(State::Degraded, "M", "Effets du micro indisponibles")), true, false), None, None, "");
        assert_eq!((degraded.phase, degraded.detail.as_str()), (Phase::Degraded, "Effets du micro indisponibles"));
    }

    #[test]
    fn a_remote_session_is_shown_only_while_the_daemon_runs() {
        let mut following = status(State::Running, "M", "");
        following.remote = "Steam Streaming Speakers".into();
        assert_eq!(view(&seen(Some(following.clone()), true, false), None, None, "").remote, "Steam Streaming Speakers");
        // A daemon that is gone follows nothing, whatever its last file says.
        assert_eq!(view(&seen(Some(following), false, false), None, None, "").remote, "");
    }

    #[test]
    fn a_daemon_without_a_microphone_takes_the_new_choice_by_itself() {
        let waiting = view(&seen(Some(status(State::WaitingDevices, "", "Micro pas encore relayé")), true, false), None, None, "");
        assert_eq!(waiting.phase, Phase::Waiting);
        assert!(!waiting.restarts_on_mic_change);
        // No microphone at all (a virtual machine): the sound runs, the microphone stays silent.
        let none = view(&seen(Some(status(State::Running, "", "Aucun micro choisi : le micro virtuel reste muet")), true, false), None, None, "");
        assert_eq!((none.phase, none.summary.as_str()), (Phase::Running, "Son actif, sans micro"));
        assert_eq!(none.detail, "Aucun micro choisi : le micro virtuel reste muet");
        assert!(!none.restarts_on_mic_change);
    }

    #[test]
    fn an_older_daemon_is_seen_by_its_mutex() {
        let view = view(&seen(None, true, false), None, None, "Seiren");
        assert_eq!((view.phase, view.microphone.as_str()), (Phase::Running, "Seiren"));
        assert!(view.can_stop && view.restarts_on_mic_change);
    }

    #[test]
    fn stopped_failed_and_gg_are_told_apart() {
        let stopped = view(&seen(Some(status(State::Stopped, "", "")), false, false), None, None, "");
        assert_eq!(stopped.phase, Phase::Stopped);
        assert!(stopped.can_start && !stopped.can_stop);
        let failed = view(&seen(Some(status(State::Failed, "", "SteelSeries GG est ouvert")), false, false), None, None, "");
        assert_eq!((failed.phase, failed.detail.as_str()), (Phase::Failed, "SteelSeries GG est ouvert"));
        let window = view(&seen(None, false, false), None, Some("platinum-start.exe est introuvable"), "");
        assert_eq!((window.phase, window.detail.as_str()), (Phase::Failed, "platinum-start.exe est introuvable"));
        let gg = view(&seen(None, false, true), None, None, "");
        assert_eq!(gg.phase, Phase::Gg);
        assert!(!gg.can_start && !gg.can_stop);
        // A file that says "running" without the daemon's mutex is a daemon that is gone.
        let gone = view(&seen(Some(status(State::Running, "M", "")), false, false), None, None, "");
        assert_eq!(gone.phase, Phase::Stopped);
    }

    #[test]
    fn a_job_under_way_wins() {
        let starting = view(&seen(Some(status(State::Stopped, "", "")), false, false), Some(Stage::Starting), None, "");
        assert_eq!(starting.phase, Phase::Starting);
        assert!(!starting.can_start && !starting.can_stop);
        let stopping = view(&seen(Some(status(State::Running, "M", "")), true, false), Some(Stage::Stopping), None, "");
        assert_eq!(stopping.phase, Phase::Stopping);
        assert!(!stopping.can_stop && !stopping.restarts_on_mic_change);
    }

    #[test]
    fn the_old_daemons_last_status_is_not_the_new_ones() {
        let now = SystemTime::now();
        let old = Stamp { written: now - Duration::from_millis(600), pid: 40 };
        // The same file, untouched since the launch.
        assert!(!is_newer(old, Some(&old)));
        // Rewritten since, by the new daemon or by the launcher (pid 0), even within the same tick.
        assert!(is_newer(Stamp { written: now, pid: 40 }, Some(&old)));
        assert!(is_newer(Stamp { written: old.written, pid: 0 }, Some(&old)));
        // No file before: whatever is there now is new.
        assert!(is_newer(old, None));
    }

    #[test]
    fn the_log_read_is_the_started_launchers() {
        let launcher = Path::new(r"C:\build\release\platinum-start.exe");
        let files = log_files(launcher);
        assert_eq!(files[0], Path::new(r"C:\build\release\daemon.log"));
        assert!(files[1].ends_with("platinum-daemon.log"));
    }

    #[test]
    fn finished_jobs_are_taken_once() {
        let job: SharedJob = Arc::new(Mutex::new(Some(Job { stage: Stage::Starting, outcome: None })));
        assert_eq!(active_stage(&job), Some(Stage::Starting));
        assert_eq!(take_outcome(&job), None);
        job.lock().unwrap().as_mut().unwrap().outcome = Some(Err("non".into()));
        assert_eq!(active_stage(&job), None);
        assert_eq!(take_outcome(&job), Some(Err("non".into())));
        assert!(current(&job).is_none());
    }

    #[test]
    fn every_phase_has_its_words() {
        for phase in [Phase::Running, Phase::Degraded, Phase::Starting, Phase::Waiting, Phase::Stopping, Phase::Stopped, Phase::Failed, Phase::Gg] {
            assert!(!phase.key().is_empty() && !phase.label().is_empty());
        }
        assert!(Phase::Waiting.carries_sound() && !Phase::Stopped.carries_sound());
    }
}
