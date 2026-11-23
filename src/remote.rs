//! Following a remote streaming session: Sunshine (played with Moonlight) and Steam Remote Play.
//!
//! When a session starts, the streaming host makes a virtual output the Windows default for every
//! role ("Haut-parleurs (Steam Streaming Speakers)", installed by Steam and used by both hosts) and
//! records what plays on it. Whenever the default changes during the session it puts it back, and
//! at the end it restores the previous one. Meanwhile the applications that follow the default play
//! straight into that output, around Platinum (no channel volume, no effects), and Platinum's
//! channels keep playing to the real speakers, in a room nobody is in. Making a channel the default
//! again would only start a fight the host wins every time.
//!
//! So Platinum follows the session instead. While the default output is a streaming sink (its name
//! holds one of `streaming_sinks`), the channels' real output moves to the sink, and the
//! applications playing on the sink are routed to Gaming, so that they go through Platinum on their
//! way to the remote device. Once the default is something else, everything goes back. What is
//! changed is written down before it is changed (`Saved`, in remote-streaming.txt next to the
//! configuration): a session that ends, a daemon that stops and a daemon that crashed in the middle
//! all put back the same things.
//!
//! Platinum never undoes what someone did during the session: a channel put on another output, or
//! an application routed elsewhere, keeps that choice; only what is still as Platinum left it goes
//! back. An application with a route of its own is never routed, and neither are the programs that
//! record the sink.
//!
//! Known limit: when the Moonlight client asks to play the sound on the host PC, Sunshine leaves the
//! default output alone, and nothing here happens.
//!
//! Everything here is pure except reading and writing the state file; the daemon's mixer thread
//! looks at the devices and writes the settings.

use crate::config::Config;
use crate::devices::{Endpoint, Flow};
use crate::mixer::STRIPS;
use crate::streamer;
use std::path::{Path, PathBuf};

/// The state file, in the install folder next to platinum.ini.
pub const FILE_NAME: &str = "remote-streaming.txt";
/// The sinks known by default: Steam's virtual speakers, which Sunshine and Steam Remote Play use.
pub const DEFAULT_SINKS: [&str; 1] = ["Steam Streaming Speakers"];
/// How many looks in a row without the sink as the default end a session. The host puts the sink
/// back within a moment of any change (the daemon's own start makes Gaming the default), so a single
/// look at something else proves nothing.
pub const LEAVE_AFTER: u32 = 2;
/// Programs never routed to Gaming: the streaming hosts, whose session on the sink is their
/// recording of it (Steam Remote Play records from steam.exe), Windows' audio engine, and
/// Platinum's own programs.
pub const NEVER_ROUTED: [&str; 7] = ["sunshine.exe", "steam.exe", "audiodg.exe", "platinum.exe", "platinum.old.exe", "platinum-ui.exe", "platinum-start.exe"];

/// Whether an output's name holds one of the sink fragments, ignoring case (a blank fragment
/// matches nothing).
pub fn is_sink_name(name: &str, sinks: &[String]) -> bool {
    let name = name.to_lowercase();
    sinks.iter().map(|fragment| fragment.trim().to_lowercase()).any(|fragment| !fragment.is_empty() && name.contains(&fragment))
}

/// The sink the default output (`default`, its identifier) is, among `endpoints`: an output whose
/// name holds a fragment, and not one of Platinum's channels, which would then play to itself.
pub fn sink<'a>(default: &str, endpoints: &'a [Endpoint], sinks: &[String]) -> Option<&'a Endpoint> {
    endpoints.iter().find(|e| e.id == default && e.flow == Flow::Render && !crate::fallback::is_virtual(e) && is_sink_name(&e.name, sinks))
}

/// How the window and the overlays name a sink: the device in parentheses ("Steam Streaming
/// Speakers"), else the whole name.
pub fn short_name(name: &str) -> &str {
    match (name.find('('), name.rfind(')')) {
        (Some(open), Some(close)) if close > open + 1 => name[open + 1..close].trim(),
        _ => name.trim(),
    }
}

/// What the default output is, as the daemon sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Look {
    /// It could not be read (Windows' audio service restarting): nothing is decided.
    Unknown,
    /// A streaming sink, by endpoint identifier.
    Sink(String),
    /// Anything else, or no default output at all.
    Other,
}

/// What the daemon does after a look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Nothing,
    /// A session began: write down, move the outputs to the sink, route the applications.
    Enter,
    /// The session goes on: route the applications that started playing on the sink since.
    Follow,
    /// The session is over, or following was turned off: put everything back.
    Leave,
}

/// The memory from one look to the next: how many looks in a row missed the sink.
#[derive(Debug, Default)]
pub struct Tracker {
    away: u32,
}

impl Tracker {
    /// The step for a look. `enabled` is `follow_streaming`; `following` the sink of the session
    /// followed, if any. A session is left at once when following is turned off, and after
    /// `LEAVE_AFTER` looks in a row without its sink otherwise; a look that could not read the
    /// default counts for nothing.
    pub fn decide(&mut self, enabled: bool, look: &Look, following: Option<&str>) -> Step {
        let Some(sink) = following else {
            self.away = 0;
            return if enabled && matches!(look, Look::Sink(_)) { Step::Enter } else { Step::Nothing };
        };
        if !enabled {
            self.away = 0;
            return Step::Leave;
        }
        match look {
            Look::Unknown => Step::Nothing,
            Look::Sink(id) if id == sink => {
                self.away = 0;
                Step::Follow
            }
            Look::Sink(_) | Look::Other => {
                self.away += 1;
                if self.away >= LEAVE_AFTER {
                    self.away = 0;
                    Step::Leave
                } else {
                    Step::Nothing
                }
            }
        }
    }
}

/// What Platinum changed for a session, written down before it changes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Saved {
    /// The sink's endpoint identifier and name.
    pub sink: String,
    pub sink_name: String,
    /// The channels' outputs when the session was found, Gaming to Aux (`None`: no output). Those
    /// that played elsewhere were moved to the sink, and go back.
    pub before: [Option<String>; STRIPS],
    /// The device the applications are routed to: Gaming's playback side.
    pub target: String,
    /// The applications Platinum routed there, by executable, in order.
    pub apps: Vec<String>,
    /// When the session was found, local time.
    pub since: String,
}

impl Saved {
    /// What Platinum is about to change for a session on `sink`, given where the channels play now
    /// and Gaming's playback side.
    pub fn new(sink: &Endpoint, before: [Option<String>; STRIPS], target: &str, since: &str) -> Saved {
        Saved { sink: sink.id.clone(), sink_name: sink.name.clone(), before, target: target.to_string(), apps: Vec::new(), since: since.to_string() }
    }

    /// The strips moved to the sink: those that played to another device.
    pub fn moved(&self) -> Vec<usize> {
        (0..STRIPS).filter(|&strip| self.before[strip].as_deref().is_some_and(|id| id != self.sink)).collect()
    }

    /// The first device the channels played to before the session: where a strip that had none of
    /// its own goes back.
    fn fallback(&self) -> Option<&str> {
        self.before.iter().flatten().map(String::as_str).find(|id| *id != self.sink)
    }

    /// The outputs to write back at the end, as (strip, device), given where the strips play now. A
    /// strip goes back only while it still plays to the sink: one moved elsewhere during the session
    /// keeps that choice, and one that played to the sink before the session stays there. A strip
    /// put on the sink during the session (a channel added to the personal mix in streamer mode)
    /// goes to the first device the channels played to: the sink plays to nowhere afterwards.
    pub fn restore(&self, now: &[Option<String>]) -> Vec<(usize, String)> {
        (0..STRIPS)
            .filter_map(|strip| {
                if now.get(strip)?.as_deref() != Some(self.sink.as_str()) {
                    return None;
                }
                let device = match self.before[strip].as_deref() {
                    Some(id) if id == self.sink => return None,
                    Some(id) => id,
                    None => self.fallback()?,
                };
                Some((strip, device.to_string()))
            })
            .collect()
    }

    /// Whether Platinum routed this application during the session.
    pub fn routed(&self, exe: &str) -> bool {
        self.apps.iter().any(|app| app.eq_ignore_ascii_case(exe))
    }

    /// Takes the sink out of platinum.ini at the end of the session. Streamer mode switched in the
    /// window during the session remembers the sink as the personal mix's device or as a classic
    /// output, which would play to nowhere afterwards: each goes back to the device of before.
    /// Returns whether something changed.
    pub fn repair(&self, config: &mut Config) -> bool {
        let sink = Some(self.sink.as_str());
        let mut changed = false;
        if config.personal_device.as_deref() == sink {
            config.personal_device = self.fallback().map(str::to_string);
            changed = true;
        }
        if let Some(outputs) = config.classic_outputs.as_mut() {
            for (strip, output) in outputs.iter_mut().enumerate() {
                if output.as_deref() == sink {
                    *output = self.before[strip].clone().filter(|id| *id != self.sink).or_else(|| self.fallback().map(str::to_string));
                    changed = true;
                }
            }
        }
        changed
    }

    /// The file's text.
    pub fn to_text(&self) -> String {
        let mut text = format!(
            "# Platinum suit une session de streaming à distance : ce qu'il a changé, pour le remettre\n# à la fin. Écrit et supprimé par la tâche de fond.\nsink = {}\nsink_name = {}\nbefore = {}\ntarget = {}\nsince = {}\n",
            single_line(&self.sink),
            single_line(&self.sink_name),
            single_line(&streamer::outputs_text(&self.before)),
            single_line(&self.target),
            single_line(&self.since),
        );
        for app in &self.apps {
            text.push_str(&format!("app = {}\n", single_line(app)));
        }
        text
    }

    /// Reads the file's text; `None` without a sink or the channels' outputs (then nothing can be
    /// put back).
    pub fn parse(text: &str) -> Option<Saved> {
        let (mut sink, mut sink_name, mut before, mut target, mut since, mut apps) = (String::new(), String::new(), None, String::new(), String::new(), Vec::new());
        for line in text.trim_start_matches('\u{feff}').lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim();
            match key.trim() {
                "sink" => sink = value.to_string(),
                "sink_name" => sink_name = value.to_string(),
                "before" => before = streamer::parse_outputs(value),
                "target" => target = value.to_string(),
                "since" => since = value.to_string(),
                "app" if !value.is_empty() => apps.push(value.to_string()),
                _ => {}
            }
        }
        if sink.is_empty() {
            return None;
        }
        Some(Saved { sink, sink_name, before: before?, target, apps, since })
    }
}

/// The applications to consider routing, from the sessions on the sink as (process, executable,
/// playing): those with a playing session, each once with all its processes there; never the system
/// sounds (process 0), the daemon itself (`me`) or `NEVER_ROUTED`.
pub fn candidates(sessions: &[(u32, String, bool)], me: u32) -> Vec<(String, Vec<u32>)> {
    let mut found: Vec<(String, Vec<u32>)> = Vec::new();
    for (_, exe, _) in sessions.iter().filter(|(pid, exe, playing)| *playing && *pid != 0 && *pid != me && !NEVER_ROUTED.iter().any(|own| exe.eq_ignore_ascii_case(own))) {
        if found.iter().any(|(known, _)| known.eq_ignore_ascii_case(exe)) {
            continue;
        }
        let pids = sessions.iter().filter(|(pid, other, _)| other.eq_ignore_ascii_case(exe) && *pid != 0).map(|(pid, _, _)| *pid).collect();
        found.push((exe.clone(), pids));
    }
    found
}

/// Whether an application found playing on the sink is routed to Gaming, given the route of each of
/// its processes there: only one that follows the default output. A route of its own, to the sink
/// itself included, is someone's choice; and one Platinum routed already that still plays there
/// opens the device by itself, so routing it again would change nothing.
pub fn should_route(saved: &Saved, exe: &str, routes: &[Option<String>]) -> bool {
    !saved.routed(exe) && routes.iter().all(Option::is_none)
}

/// Whether an application Platinum routed goes back to the default output at the end: only while
/// its route is still the one Platinum gave it.
pub fn still_ours(route: Option<&str>, target: &str) -> bool {
    !target.is_empty() && route == Some(target)
}

/// `%USERPROFILE%\Platinum\remote-streaming.txt`.
pub fn path() -> Result<PathBuf, String> {
    Ok(crate::autostart::install_dir()?.join(FILE_NAME))
}

/// The session written down, if any: `Ok(None)` without a file, an error when it cannot be read.
pub fn load() -> Result<Option<Saved>, String> {
    load_from(&path()?)
}

pub fn load_from(file: &Path) -> Result<Option<Saved>, String> {
    match std::fs::read(file) {
        Ok(bytes) => Saved::parse(&String::from_utf8_lossy(&bytes)).map(Some).ok_or_else(|| format!("{} holds no session that can be put back", file.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot read {}: {error}", file.display())),
    }
}

/// Writes the session down, replacing the whole file at once.
pub fn save(saved: &Saved) -> Result<(), String> {
    save_to(&path()?, saved)
}

pub fn save_to(file: &Path, saved: &Saved) -> Result<(), String> {
    crate::config::write_atomically(file, &saved.to_text())
}

/// Removes the file once everything is back (a missing file is fine).
pub fn remove() -> Result<(), String> {
    remove_at(&path()?)
}

pub fn remove_at(file: &Path) -> Result<(), String> {
    match std::fs::remove_file(file) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot remove {}: {error}", file.display())),
    }
}

/// A value written on one line.
fn single_line(value: &str) -> String {
    value.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::State;

    const SINK: &str = "{0.0.0.00000000}.{9d58a5fb}";

    fn endpoint(id: &str, name: &str, flow: Flow, role: Option<u32>) -> Endpoint {
        Endpoint { id: id.into(), name: name.into(), flow, state: State::Active, virtual_role: role }
    }

    fn sinks() -> Vec<String> {
        DEFAULT_SINKS.iter().map(|s| s.to_string()).collect()
    }

    fn some(id: &str) -> Option<String> {
        Some(id.to_string())
    }

    /// A session found with Gaming and Chat on the Arena 7, Media without an output and Aux on the
    /// AirPods.
    fn session() -> Saved {
        let sink = endpoint(SINK, "Haut-parleurs (Steam Streaming Speakers)", Flow::Render, None);
        Saved::new(&sink, [some("{arena}"), some("{arena}"), None, some("{airpods}")], "{gaming}", "20261117-210000")
    }

    #[test]
    fn the_sink_is_found_by_its_name_among_the_real_outputs() {
        let endpoints = [
            endpoint(SINK, "Haut-parleurs (Steam Streaming Speakers)", Flow::Render, None),
            endpoint("{jack}", "Prise jack AUX interne (Steam Streaming Speakers)", Flow::Capture, None),
            endpoint("{game}", "Gaming (Steam Streaming Speakers)", Flow::Render, Some(2)),
            endpoint("{arena}", "Haut-parleurs (SteelSeries Arena 7)", Flow::Render, None),
        ];
        assert_eq!(sink(SINK, &endpoints, &sinks()).map(|e| e.id.as_str()), Some(SINK));
        // An input, one of Platinum's channels (it would play to itself) and another output are not.
        assert!(sink("{jack}", &endpoints, &sinks()).is_none());
        assert!(sink("{game}", &endpoints, &sinks()).is_none());
        assert!(sink("{arena}", &endpoints, &sinks()).is_none());
        assert!(is_sink_name("HAUT-PARLEURS (steam streaming speakers)", &sinks()));
        assert!(!is_sink_name("Haut-parleurs (Steam Streaming Speakers)", &[" ".to_string()]));
        assert!(!is_sink_name("Haut-parleurs (Steam Streaming Speakers)", &[]));
        assert_eq!(short_name("Haut-parleurs (Steam Streaming Speakers)"), "Steam Streaming Speakers");
        assert_eq!(short_name("Sunshine sink"), "Sunshine sink");
        assert_eq!(short_name("Odd ()"), "Odd ()");
    }

    #[test]
    fn a_session_is_entered_followed_and_left_after_two_looks_without_its_sink() {
        let mut tracker = Tracker::default();
        let sink = Look::Sink(SINK.into());
        assert_eq!(tracker.decide(true, &Look::Other, None), Step::Nothing);
        assert_eq!(tracker.decide(false, &sink, None), Step::Nothing);
        assert_eq!(tracker.decide(true, &sink, None), Step::Enter);
        assert_eq!(tracker.decide(true, &sink, Some(SINK)), Step::Follow);
        // The daemon's start makes Gaming the default for a moment, and the host puts the sink back.
        assert_eq!(tracker.decide(true, &Look::Other, Some(SINK)), Step::Nothing);
        assert_eq!(tracker.decide(true, &sink, Some(SINK)), Step::Follow);
        // An unreadable default counts for nothing, either way.
        assert_eq!(tracker.decide(true, &Look::Other, Some(SINK)), Step::Nothing);
        assert_eq!(tracker.decide(true, &Look::Unknown, Some(SINK)), Step::Nothing);
        assert_eq!(tracker.decide(true, &Look::Other, Some(SINK)), Step::Leave);
        // Another sink is the end of this session; the next look enters the new one.
        assert_eq!(tracker.decide(true, &Look::Sink("{other}".into()), Some(SINK)), Step::Nothing);
        assert_eq!(tracker.decide(true, &Look::Sink("{other}".into()), Some(SINK)), Step::Leave);
        // Turned off: left at once, then nothing more.
        assert_eq!(tracker.decide(false, &sink, Some(SINK)), Step::Leave);
        assert_eq!(tracker.decide(false, &sink, None), Step::Nothing);
    }

    #[test]
    fn only_the_outputs_still_on_the_sink_go_back() {
        let saved = session();
        assert_eq!(saved.moved(), [0, 1, 3]);
        // Gaming and Aux still on the sink go back; Chat was moved to the screen during the session
        // and keeps it; Media had no output and has none.
        let now = [some(SINK), some("{screen}"), None, some(SINK)];
        assert_eq!(saved.restore(&now), [(0, "{arena}".to_string()), (3, "{airpods}".to_string())]);
        // Media added to the personal mix during the session goes to the first device of before.
        let added = [some(SINK), some(SINK), some(SINK), some(SINK)];
        assert_eq!(saved.restore(&added)[2], (2, "{arena}".to_string()));
        // A channel on the sink before the session was not moved, and stays.
        let mut on_sink = session();
        on_sink.before[0] = some(SINK);
        assert_eq!(on_sink.moved(), [1, 3]);
        assert!(!on_sink.restore(&added).iter().any(|(strip, _)| *strip == 0));
        // Nothing played anywhere before: nowhere to go back to, nothing written.
        let mut silent = session();
        silent.before = [None, None, None, None];
        assert!(silent.moved().is_empty() && silent.restore(&added).is_empty());
        // Unreadable now (fewer values): nothing for the missing strips.
        assert_eq!(saved.restore(&[some(SINK)]), [(0, "{arena}".to_string())]);
    }

    #[test]
    fn applications_following_the_default_are_routed_once_and_given_back_only_if_still_ours() {
        let sessions = vec![
            (0, String::new(), true),
            (10, "sunshine.exe".to_string(), true),
            (11, "Steam.exe".to_string(), true),
            (20, "game.exe".to_string(), true),
            (21, "chrome.exe".to_string(), false),
            (22, "chrome.exe".to_string(), true),
            (23, "idle.exe".to_string(), false),
            (99, "platinum.exe".to_string(), true),
            (42, "me.exe".to_string(), true),
        ];
        let found = candidates(&sessions, 42);
        assert_eq!(found, [("game.exe".to_string(), vec![20]), ("chrome.exe".to_string(), vec![21, 22])]);
        let mut saved = session();
        assert!(should_route(&saved, "game.exe", &[None]));
        // A route of its own, even to the sink, is left alone.
        assert!(!should_route(&saved, "chrome.exe", &[None, some("{media}")]));
        assert!(!should_route(&saved, "game.exe", &[some(SINK)]));
        // Routed once and still on the sink: it opens the device itself.
        saved.apps.push("Game.exe".into());
        assert!(!should_route(&saved, "game.exe", &[None]));
        assert!(still_ours(Some("{gaming}"), "{gaming}"));
        assert!(!still_ours(Some("{media}"), "{gaming}") && !still_ours(None, "{gaming}") && !still_ours(None, ""));
    }

    #[test]
    fn platinum_ini_forgets_the_sink_at_the_end() {
        let saved = session();
        let mut config = Config { personal_device: some(SINK), classic_outputs: Some([some(SINK), some("{screen}"), some(SINK), None]), ..Config::default() };
        assert!(saved.repair(&mut config));
        assert_eq!(config.personal_device.as_deref(), Some("{arena}"));
        // Each classic output goes back to its own device of before, else the first one.
        assert_eq!(config.classic_outputs, Some([some("{arena}"), some("{screen}"), some("{arena}"), None]));
        assert!(!saved.repair(&mut config));
        assert!(!saved.repair(&mut Config::default()));
    }

    #[test]
    fn the_state_file_survives_a_round_trip() {
        let mut saved = session();
        saved.apps = vec!["game.exe".into(), "chrome.exe".into()];
        assert_eq!(Saved::parse(&saved.to_text()), Some(saved.clone()));
        assert_eq!(Saved::parse("before = , , , \n"), None);
        assert_eq!(Saved::parse("sink = {a}\nbefore = {b}\n"), None);
        let folder = std::env::temp_dir().join(format!("platinum-remote-test-{}", std::process::id()));
        let file = folder.join(FILE_NAME);
        assert_eq!(load_from(&file), Ok(None));
        save_to(&file, &saved).unwrap();
        assert_eq!(load_from(&file), Ok(Some(saved)));
        std::fs::write(&file, "garbage").unwrap();
        assert!(load_from(&file).unwrap_err().contains("no session"));
        remove_at(&file).unwrap();
        remove_at(&file).unwrap();
        assert!(!file.exists());
        let _ = std::fs::remove_dir_all(&folder);
    }
}
