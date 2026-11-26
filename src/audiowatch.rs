//! Whether Windows' audio service still answers.
//!
//! On 26/11/2026 the service (Audiosrv) froze for minutes right after a Bluetooth headset, the
//! channels' real output, came back. Every call into it waited, the keep-alive thread's among them,
//! and the daemon's stop waited behind that thread. Under GG alone the same freeze had happened about
//! twenty times since August, each time after the same kind of reconnection; only ending the
//! service brought the sound back.
//!
//! A probe thread asks the service one question every `PERIOD`: the shared-mode format of the
//! virtual Gaming channel, else of the default output (`IAudioClient::GetMixFormat`, the call that
//! stayed stuck). It opens no stream and plays nothing; an error is an answer too. The daemon's
//! watcher looks at the question under way twice a second (`Freeze`): one left unanswered for
//! `FROZEN_LOOKS` looks means the service is frozen. The probe thread is never joined, so that a
//! question that never comes back cannot hold the daemon.

use crate::devices::{Devices, Flow, Role, State};
use crate::wasapi::AudioClient;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How often the probe asks the service.
pub const PERIOD: Duration = Duration::from_secs(5);
/// How many of the watcher's looks (one every 500 ms) a question may stay unanswered before the
/// service counts as frozen: 10 s. Counting looks rather than measuring time keeps a question under
/// way while the machine slept from passing for a frozen service.
pub const FROZEN_LOOKS: u32 = 20;
/// The virtual Gaming channel (`Endpoint::virtual_role`): a device of the virtual driver, always
/// there while the channels are up, and never a Bluetooth headset caught in its reconnection.
const GAME_ROLE: u32 = 2;

static STARTED: AtomicBool = AtomicBool::new(false);
/// The number the next question gets (from 1).
static NEXT: AtomicU64 = AtomicU64::new(1);
/// The question under way (its number); 0 between questions.
static ASKING: AtomicU64 = AtomicU64::new(0);
/// When the question under way was asked, in milliseconds after `EPOCH`.
static ASKED_AT: AtomicU64 = AtomicU64::new(0);
static EPOCH: OnceLock<Instant> = OnceLock::new();

fn now_ms() -> u64 {
    u64::try_from(EPOCH.get_or_init(Instant::now).elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Starts the probe thread, once per process; false when it could not start.
pub fn start() -> bool {
    if STARTED.swap(true, Ordering::SeqCst) {
        return true;
    }
    let started = std::thread::Builder::new().name("audio-probe".into()).spawn(probe).is_ok();
    if !started {
        STARTED.store(false, Ordering::SeqCst);
    }
    started
}

/// The question under way (0: none), for `Freeze::look`.
pub fn asking() -> u64 {
    ASKING.load(Ordering::SeqCst)
}

/// How long the question under way has waited; `None` between questions. For a thread that cannot
/// look twice a second, as the daemon's stop deadline (the watcher has ended by then).
pub fn stuck_for() -> Option<Duration> {
    (asking() != 0).then(|| Duration::from_millis(now_ms().saturating_sub(ASKED_AT.load(Ordering::SeqCst))))
}

fn probe() {
    let mut probe = Probe::default();
    // The daemon brings the channels up meanwhile: the first question goes to Gaming.
    std::thread::sleep(PERIOD);
    loop {
        ASKED_AT.store(now_ms(), Ordering::SeqCst);
        ASKING.store(NEXT.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
        probe.ask();
        ASKING.store(0, Ordering::SeqCst);
        std::thread::sleep(PERIOD);
    }
}

/// What the probe keeps from one question to the next: reopened after a failure, since the
/// service may have restarted under it.
#[derive(Default)]
struct Probe {
    devices: Option<Devices>,
    /// The output asked about: its identifier, and whether it is the virtual Gaming channel (kept
    /// then; any other is looked for again at every question, since the channels come up after
    /// the probe starts and the default output may be a headset).
    target: Option<(String, bool)>,
}

impl Probe {
    fn ask(&mut self) {
        if self.try_ask().is_err() {
            self.devices = None;
            self.target = None;
        }
    }

    fn try_ask(&mut self) -> Result<(), String> {
        if self.devices.is_none() {
            self.devices = Some(Devices::open().map_err(|e| e.to_string())?);
        }
        let Some(devices) = &self.devices else { return Err("no audio devices".into()) };
        if !self.target.as_ref().is_some_and(|(_, game)| *game) {
            self.target = target(devices);
        }
        let Some((id, _)) = &self.target else { return Err("no output to ask about".into()) };
        let client = AudioClient::activate(devices, id).map_err(|e| e.to_string())?;
        client.mix_format(devices.ole()).map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// The output asked about, and whether it is the virtual Gaming channel: that channel when it is
/// active, else the default output.
fn target(devices: &Devices) -> Option<(String, bool)> {
    let listed = devices.list().unwrap_or_default();
    listed
        .iter()
        .find(|e| e.flow == Flow::Render && e.state == State::Active && e.virtual_role == Some(GAME_ROLE))
        .map(|e| (e.id.clone(), true))
        .or_else(|| devices.default(Flow::Render, Role::Console).ok().flatten().map(|id| (id, false)))
}

/// What a look of the watcher found new.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// The question under way has gone unanswered for `FROZEN_LOOKS` looks.
    Froze,
    /// The question that had frozen came back (an error included), after this many looks.
    Answered { looks: u32 },
}

/// The watcher's view of the probe, one look every 500 ms.
#[derive(Debug, Default)]
pub struct Freeze {
    /// The question seen under way at the last look (0: none).
    question: u64,
    /// How many looks in a row have seen it under way.
    looks: u32,
    frozen: bool,
}

impl Freeze {
    /// One look, given the question under way now (`asking()`).
    pub fn look(&mut self, asking: u64) -> Option<Change> {
        if asking != 0 && asking == self.question {
            self.looks += 1;
            if !self.frozen && self.looks >= FROZEN_LOOKS {
                self.frozen = true;
                return Some(Change::Froze);
            }
            return None;
        }
        let looks = self.looks;
        self.question = asking;
        self.looks = u32::from(asking != 0);
        std::mem::take(&mut self.frozen).then_some(Change::Answered { looks })
    }

    /// Whether the service counts as frozen now.
    pub fn frozen(&self) -> bool {
        self.frozen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_answers_never_freeze() {
        let mut freeze = Freeze::default();
        // A question caught under way at one look and gone at the next, again and again.
        for question in 1..200u64 {
            assert_eq!(freeze.look(question), None);
            assert_eq!(freeze.look(0), None);
        }
        // Questions that follow each other without a pause between two looks.
        for question in 500..600u64 {
            assert_eq!(freeze.look(question), None);
        }
        assert!(!freeze.frozen());
    }

    #[test]
    fn a_question_left_unanswered_freezes_then_thaws() {
        let mut freeze = Freeze::default();
        for _ in 1..FROZEN_LOOKS {
            assert_eq!(freeze.look(7), None);
        }
        assert_eq!(freeze.look(7), Some(Change::Froze));
        assert!(freeze.frozen());
        // Said once, however long it lasts.
        for _ in 0..100 {
            assert_eq!(freeze.look(7), None);
        }
        // The answer (or the error when the service is ended) comes back.
        assert_eq!(freeze.look(0), Some(Change::Answered { looks: FROZEN_LOOKS + 100 }));
        assert!(!freeze.frozen());
        assert_eq!(freeze.look(0), None);
        // The next question already under way at the look that sees the thaw counts afresh.
        let mut freeze = Freeze::default();
        for _ in 0..FROZEN_LOOKS {
            freeze.look(3);
        }
        assert_eq!(freeze.look(4), Some(Change::Answered { looks: FROZEN_LOOKS }));
        // That look was the new question's first.
        for _ in 2..FROZEN_LOOKS {
            assert_eq!(freeze.look(4), None);
        }
        assert_eq!(freeze.look(4), Some(Change::Froze));
    }
}
