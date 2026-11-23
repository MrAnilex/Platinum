//! When the daemon replaces the microphone worker, to give back the memory the vendor's effects
//! engine gathers, and how soon it starts a worker again after one ended on its own.
//!
//! The engine (`fxprobe::MicEffects`) runs in a worker process of its own (`worker`). Its private
//! memory was measured growing about 4 MB a minute while echo cancellation had references (205 to
//! 208), and staying flat while they were empty (17–18/11/2026). Rebuilding the engine inside the
//! same process gave almost nothing back ("effects engine recycled at 300 MB, now 296 MB"): the
//! memory is held in the DLL's global state, and the DLL can never be unloaded. Only ending the
//! process returns it. So once the worker's private memory passes `engine_recycle_mb` and it has
//! run for an hour, the daemon starts a new worker, lets it get ready, and hands the microphone
//! over during a silence of the user: about 0.1 to 0.2 s of the microphone, echo cancellation kept.
//!
//! Small decisions, kept apart from Windows so that they can be tested: `Replacement` says when to
//! replace the worker, `Restarts` how soon to start one again after it ended on its own (crashed,
//! killed, stuck), and `QuietClock`, on the worker's side, how long the microphone has been quiet.

use std::time::{Duration, Instant};

/// A worker younger than this is never replaced: it costs a fraction of a second of the microphone
/// each time, and at the growth measured an hour is far below any limit worth having.
pub const MIN_AGE: Duration = Duration::from_secs(60 * 60);
/// How long the microphone must stay quiet before the worker is replaced.
pub const QUIET_FOR: Duration = Duration::from_secs(2);
/// The worker is replaced after waiting this long for a silence, quiet or not: a microphone that
/// never goes quiet (a noisy room, a long call) must not let the memory grow forever.
pub const LONGEST_WAIT: Duration = Duration::from_secs(30 * 60);
/// The wait after a replacement that failed (the new worker could not get ready), doubled after
/// each failure in a row up to `LONGEST_BACKOFF`.
pub const FIRST_BACKOFF: Duration = Duration::from_secs(15 * 60);
pub const LONGEST_BACKOFF: Duration = Duration::from_secs(4 * 60 * 60);
/// Peak level (linear) under which the microphone counts as quiet: -40 dBFS, well above a quiet
/// room (peaks of 0.0001 to 0.0016 were measured on the user's microphone between words) and well
/// below speech (0.07 to 0.2).
pub const QUIET_LEVEL: f32 = 0.01;

/// Why the worker is replaced now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Moment {
    /// The microphone stayed quiet for `QUIET_FOR`.
    Quiet,
    /// It did not within `LONGEST_WAIT` of the worker being due.
    WaitedTooLong,
}

/// What to do with the worker after a look at its memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Not due: under the limit, too young, limit off, or backing off after a failure.
    Keep,
    /// Due, waiting for a silence; `first` on the look that found it due (said once in the log).
    Wait { first: bool },
    /// Replace it now.
    Replace(Moment),
}

/// When to replace the microphone worker, from its private memory, its age and the microphone's
/// silences.
pub struct Replacement {
    /// The limit in MB; 0 turns replacements off.
    threshold_mb: u32,
    /// Since when the worker has been due, waiting for a silence.
    due_since: Option<Instant>,
    /// No replacement before this, after one failed.
    not_before: Option<Instant>,
    /// The wait after the next failure.
    backoff: Duration,
}

impl Replacement {
    pub fn new(threshold_mb: u32) -> Replacement {
        Replacement { threshold_mb, due_since: None, not_before: None, backoff: FIRST_BACKOFF }
    }

    pub fn threshold_mb(&self) -> u32 {
        self.threshold_mb
    }

    /// One look at the worker: `age` since it started, `private_mb` its private memory, `quiet`
    /// how long the microphone has been quiet (zero when unknown, which never counts as a silence).
    pub fn decide(&mut self, now: Instant, age: Duration, private_mb: f64, quiet: Duration) -> Decision {
        let backing_off = self.not_before.is_some_and(|until| now < until);
        let due = self.threshold_mb > 0 && private_mb >= f64::from(self.threshold_mb) && age >= MIN_AGE && !backing_off;
        if !due {
            // The memory fell back, or the worker changed: the wait starts afresh next time.
            self.due_since = None;
            return Decision::Keep;
        }
        let first = self.due_since.is_none();
        let since = *self.due_since.get_or_insert(now);
        if quiet >= QUIET_FOR {
            Decision::Replace(Moment::Quiet)
        } else if now.saturating_duration_since(since) >= LONGEST_WAIT {
            Decision::Replace(Moment::WaitedTooLong)
        } else {
            Decision::Wait { first }
        }
    }

    /// How long the worker has been waiting for a silence.
    pub fn waited(&self, now: Instant) -> Duration {
        self.due_since.map_or(Duration::ZERO, |since| now.saturating_duration_since(since))
    }

    /// The worker was replaced: the next one starts with a clean slate.
    pub fn replaced(&mut self) {
        self.due_since = None;
        self.not_before = None;
        self.backoff = FIRST_BACKOFF;
    }

    /// The new worker could not take over at `now`: the old one goes on, and no replacement is
    /// tried for a while, twice as long after each failure in a row, up to `LONGEST_BACKOFF`.
    /// Returns that wait.
    pub fn failed(&mut self, now: Instant) -> Duration {
        let wait = self.backoff;
        self.not_before = Some(now + wait);
        self.backoff = (self.backoff * 2).min(LONGEST_BACKOFF);
        self.due_since = None;
        wait
    }
}

/// A worker that ran this long before it ended on its own ends a streak of failures: the next one
/// starts at once.
pub const STEADY: Duration = Duration::from_secs(30);
/// The longest wait before starting a worker again within a streak.
pub const LONGEST_RESTART_DELAY: Duration = Duration::from_secs(30);
/// Worker ends in a row, with the effects engine, after which the daemon relays the microphone
/// without effects for the rest of its run: a raw microphone beats one that keeps coming and going.
pub const CRASHES_BEFORE_RAW: u32 = 3;

/// How soon to start the worker again after it ended on its own.
#[derive(Default)]
pub struct Restarts {
    /// Ends in a row, each after a short run.
    streak: u32,
}

impl Restarts {
    /// The worker ended on its own after running `ran`. Returns how long to wait before starting
    /// another: at once for the first end of a streak, then 1, 2, 4… s up to
    /// `LONGEST_RESTART_DELAY` while they keep ending quickly. A worker that ran `STEADY` or more
    /// starts a new streak.
    pub fn ended(&mut self, ran: Duration) -> Duration {
        if ran >= STEADY {
            self.streak = 0;
        }
        self.streak += 1;
        match self.streak {
            1 => Duration::ZERO,
            n => Duration::from_secs(1u64 << (n - 2).min(5)).min(LONGEST_RESTART_DELAY),
        }
    }

    /// How many ends in a row, so far (0: none since the last steady run).
    pub fn streak(&self) -> u32 {
        self.streak
    }

    /// A worker has been running steadily: the streak is over. Returns how many ends it counted.
    pub fn steady(&mut self) -> u32 {
        std::mem::take(&mut self.streak)
    }
}

/// On the worker's side: how long the microphone has stayed under `QUIET_LEVEL`, from its peak
/// levels as the relay reads them.
#[derive(Default)]
pub struct QuietClock {
    quiet_since: Option<Instant>,
}

impl QuietClock {
    /// The microphone's peak level over the audio read up to `now`.
    pub fn add(&mut self, now: Instant, peak: f32) {
        // A level that is not a number counts as sound: better to wait than to cut speech.
        if peak.is_nan() || peak >= QUIET_LEVEL {
            self.quiet_since = None;
        } else if self.quiet_since.is_none() {
            self.quiet_since = Some(now);
        }
    }

    /// How long the microphone has been quiet at `now` (zero before any quiet reading).
    pub fn quiet_for(&self, now: Instant) -> Duration {
        self.quiet_since.map_or(Duration::ZERO, |since| now.saturating_duration_since(since))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, seconds: f64) -> Instant {
        start + Duration::from_secs_f64(seconds)
    }

    const HOUR: Duration = MIN_AGE;
    const LOUD: Duration = Duration::ZERO;
    const SILENT: Duration = Duration::from_secs(3);

    #[test]
    fn a_worker_is_replaced_over_the_limit_once_an_hour_old_and_at_a_silence() {
        let start = Instant::now();
        let mut policy = Replacement::new(400);
        // Under the limit, or too young: kept, whatever the silence.
        assert_eq!(policy.decide(start, HOUR * 2, 399.9, SILENT), Decision::Keep);
        assert_eq!(policy.decide(start, HOUR - Duration::from_secs(1), 520.0, SILENT), Decision::Keep);
        // Due: said once, then it waits while the user speaks…
        assert_eq!(policy.decide(start, HOUR, 400.0, LOUD), Decision::Wait { first: true });
        assert_eq!(policy.decide(at(start, 2.0), HOUR, 405.0, Duration::from_millis(1999)), Decision::Wait { first: false });
        // …and goes at the first two quiet seconds.
        assert_eq!(policy.decide(at(start, 4.0), HOUR, 410.0, QUIET_FOR), Decision::Replace(Moment::Quiet));
        assert_eq!(policy.waited(at(start, 4.0)), Duration::from_secs(4));
        // Zero turns it off.
        assert_eq!(Replacement::new(0).decide(start, HOUR * 10, 10_000.0, SILENT), Decision::Keep);
    }

    #[test]
    fn half_an_hour_without_silence_replaces_it_anyway() {
        let start = Instant::now();
        let mut policy = Replacement::new(400);
        assert_eq!(policy.decide(start, HOUR, 450.0, LOUD), Decision::Wait { first: true });
        assert_eq!(policy.decide(at(start, 1799.0), HOUR, 560.0, LOUD), Decision::Wait { first: false });
        assert_eq!(policy.decide(at(start, 1800.0), HOUR, 560.0, LOUD), Decision::Replace(Moment::WaitedTooLong));
        // Memory that falls back under the limit ends the wait; the next one starts afresh.
        let mut policy = Replacement::new(400);
        assert_eq!(policy.decide(start, HOUR, 450.0, LOUD), Decision::Wait { first: true });
        assert_eq!(policy.decide(at(start, 900.0), HOUR, 300.0, LOUD), Decision::Keep);
        assert_eq!(policy.decide(at(start, 1000.0), HOUR, 450.0, LOUD), Decision::Wait { first: true });
        assert_eq!(policy.decide(at(start, 2799.0), HOUR, 450.0, LOUD), Decision::Wait { first: false });
        assert_eq!(policy.decide(at(start, 2800.0), HOUR, 450.0, LOUD), Decision::Replace(Moment::WaitedTooLong));
    }

    #[test]
    fn failed_replacements_back_off_twice_as_long_up_to_four_hours() {
        let start = Instant::now();
        let mut policy = Replacement::new(400);
        assert_eq!(policy.failed(start), FIRST_BACKOFF);
        assert_eq!(policy.decide(at(start, 60.0), HOUR, 500.0, SILENT), Decision::Keep);
        assert_eq!(policy.decide(start + FIRST_BACKOFF - Duration::from_secs(1), HOUR, 500.0, SILENT), Decision::Keep);
        assert_eq!(policy.decide(start + FIRST_BACKOFF, HOUR, 500.0, SILENT), Decision::Replace(Moment::Quiet));
        let waits: Vec<u64> = (0..5).map(|_| policy.failed(start).as_secs() / 60).collect();
        assert_eq!(waits, [30, 60, 120, 240, 240]);
        // One that works brings the first wait back.
        policy.replaced();
        assert_eq!(policy.decide(start, HOUR, 500.0, SILENT), Decision::Replace(Moment::Quiet));
        assert_eq!(policy.failed(start), FIRST_BACKOFF);
    }

    #[test]
    fn a_worker_that_keeps_ending_is_started_again_less_and_less_often() {
        let mut restarts = Restarts::default();
        let quick = Duration::from_secs(2);
        let delays: Vec<u64> = (0..8).map(|_| restarts.ended(quick).as_secs()).collect();
        assert_eq!(delays, [0, 1, 2, 4, 8, 16, 30, 30]);
        assert_eq!(restarts.streak(), 8);
        // A worker that ran steadily before it ended starts a new streak: at once again.
        assert_eq!(restarts.ended(STEADY), Duration::ZERO);
        assert_eq!(restarts.streak(), 1);
        assert_eq!(restarts.steady(), 1);
        assert_eq!(restarts.ended(quick), Duration::ZERO);
    }

    #[test]
    fn the_microphone_is_quiet_after_readings_under_minus_40_dbfs() {
        let start = Instant::now();
        let mut clock = QuietClock::default();
        assert_eq!(clock.quiet_for(start), Duration::ZERO);
        clock.add(start, 0.15);
        assert_eq!(clock.quiet_for(at(start, 1.0)), Duration::ZERO);
        clock.add(at(start, 1.0), 0.002);
        clock.add(at(start, 2.0), 0.009);
        assert_eq!(clock.quiet_for(at(start, 3.5)), Duration::from_millis(2500));
        // Speech, or a level that is not a number, breaks the quiet.
        clock.add(at(start, 4.0), 0.08);
        assert_eq!(clock.quiet_for(at(start, 5.0)), Duration::ZERO);
        clock.add(at(start, 5.0), 0.0);
        clock.add(at(start, 6.0), f32::NAN);
        assert_eq!(clock.quiet_for(at(start, 7.0)), Duration::ZERO);
    }
}
