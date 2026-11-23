//! Platform-independent signal helpers for the microphone relay: channel mapping, a variable-ratio
//! resampler that absorbs the clock drift between two devices, and the controller that steers it.
//!
//! Nothing here talks to the operating system, so it is tested without any audio device and can be
//! reused as is on other platforms.

/// Largest sample magnitude in `samples`.
pub fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0, |max, s| max.max(s.abs()))
}

/// Appends `input` (interleaved, `cin` channels) to `output` as `cout` channels, scaled by `gain`.
///
/// Same layout: copied. Mono source: sent to the first two channels. Mono destination: the average
/// of the source channels. Otherwise the channels both sides have are copied and the rest is silent.
pub fn remap(input: &[f32], cin: usize, cout: usize, gain: f32, output: &mut Vec<f32>) {
    output.reserve(input.len() / cin * cout);
    for frame in input.chunks_exact(cin) {
        if cin == cout {
            output.extend(frame.iter().map(|s| s * gain));
        } else if cout == 1 {
            output.push(frame.iter().sum::<f32>() / cin as f32 * gain);
        } else {
            for channel in 0..cout {
                let sample = if cin == 1 {
                    if channel < 2 { frame[0] } else { 0.0 }
                } else {
                    frame.get(channel).copied().unwrap_or(0.0)
                };
                output.push(sample * gain);
            }
        }
    }
}

/// Resampler for ratios very close to 1, used to follow the slow drift between two device clocks.
///
/// It interpolates with a four-point cubic (Catmull-Rom), which is transparent for the fraction of
/// a percent it has to absorb. It is not meant for real rate conversion (44.1 to 48 kHz): that is
/// left to the operating system's converter. At a ratio of exactly 1 the output is the input,
/// delayed by two frames.
pub struct Resampler {
    channels: usize,
    /// The last three input frames, interleaved.
    history: Vec<f32>,
    /// Where the next output frame falls, in input frames past the middle history frame.
    phase: f64,
}

impl Resampler {
    pub fn new(channels: usize) -> Resampler {
        Resampler { channels, history: vec![0.0; 3 * channels], phase: 0.0 }
    }

    /// Appends to `output` the frames made from `input`, one input frame being worth `ratio` output frames.
    pub fn process(&mut self, input: &[f32], ratio: f64, output: &mut Vec<f32>) {
        let channels = self.channels;
        let frames = input.len() / channels;
        if frames == 0 {
            return;
        }
        // Frame `i` of the three history frames followed by the new input.
        let sample = |i: usize, channel: usize| if i < 3 { self.history[i * channels + channel] } else { input[(i - 3) * channels + channel] };

        let step = 1.0 / ratio;
        let end = (frames + 1) as f64;
        let mut position = 1.0 + self.phase;
        while position < end {
            let index = position as usize;
            let t = (position - index as f64) as f32;
            for channel in 0..channels {
                let (y0, y1, y2, y3) = (sample(index - 1, channel), sample(index, channel), sample(index + 1, channel), sample(index + 2, channel));
                output.push(y1 + 0.5 * t * (y2 - y0 + t * (2.0 * y0 - 5.0 * y1 + 4.0 * y2 - y3 + t * (3.0 * (y1 - y2) + y3 - y0))));
            }
            position += step;
        }
        self.phase = position - end;

        let mut history = Vec::with_capacity(3 * channels);
        for i in frames..frames + 3 {
            for channel in 0..channels {
                history.push(sample(i, channel));
            }
        }
        self.history = history;
    }
}

/// Cuts a stream of interleaved frames into blocks of a fixed size, for an effects engine that
/// works period by period. Frames that do not fill a block yet wait for the next push.
pub struct Framer {
    channels: usize,
    block: usize,
    pending: Vec<f32>,
}

impl Framer {
    pub fn new(channels: usize, block: usize) -> Framer {
        Framer { channels, block, pending: Vec::with_capacity(2 * block * channels) }
    }

    pub fn push(&mut self, samples: &[f32]) {
        self.pending.extend_from_slice(samples);
    }

    /// Appends `frames` frames of silence.
    pub fn push_silence(&mut self, frames: usize) {
        self.pending.resize(self.pending.len() + frames * self.channels, 0.0);
    }

    /// Hands each complete block to `f`, oldest first, and keeps the remainder.
    pub fn drain(&mut self, mut f: impl FnMut(&[f32])) {
        let size = self.block * self.channels;
        let complete = self.pending.len() / size * size;
        for block in self.pending[..complete].chunks_exact(size) {
            f(block);
        }
        self.pending.drain(..complete);
    }

    /// Frames waiting for their block to fill.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn pending_frames(&self) -> usize {
        self.pending.len() / self.channels
    }
}

/// How far the resampling ratio may move away from 1: half a percent, about a twelfth of a semitone.
pub const MAX_CORRECTION: f64 = 0.005;
/// Seconds over which the measured queue length is averaged.
const SMOOTHING: f64 = 0.5;
/// Seconds after start during which the loop reacts quickly, to find the drift before it can empty
/// or fill the queue.
const ACQUISITION: f64 = 30.0;
/// Loop speeds as (response, reset) in seconds: how long the proportional action takes to absorb
/// an error, and how long the integral action takes to learn the drift. Reset is four times
/// response, which settles without overshoot.
const FAST: (f64, f64) = (2.0, 8.0);
const SLOW: (f64, f64) = (10.0, 40.0);

/// Keeps the amount of audio queued at the destination near a target by nudging the resampling ratio.
///
/// The source and the destination each count time on their own clock, so one always runs slightly
/// faster. Without correction the queue would slowly drain (drop-outs) or grow (delay). This is a
/// proportional-integral loop on the queue length: the proportional part reacts to the error, the
/// integral part converges on the drift itself. It is quick for the first half minute, then slow.
///
/// The queue is measured right after each write, and the destination empties it in whole blocks,
/// so the measurement wanders by up to one block as the two clocks slide past each other. The loop
/// follows that wander: the real queue moves by as much, which is harmless as long as the target
/// leaves more than a block in reserve.
pub struct DriftControl {
    rate: f64,
    target: f64,
    smoothed: Option<f64>,
    /// Seconds of audio seen so far.
    elapsed: f64,
    /// How much faster the source runs than the destination, as learned so far.
    learned: f64,
    correction: f64,
}

impl DriftControl {
    /// `rate` in frames per second, `target` in frames queued right after a write.
    pub fn new(rate: u32, target: usize) -> DriftControl {
        DriftControl { rate: f64::from(rate), target: target as f64, smoothed: None, elapsed: 0.0, learned: 0.0, correction: 0.0 }
    }

    /// Takes the queue length measured right after a write and the number of input frames consumed
    /// since the previous call (they are the loop's clock). Returns the ratio to use next.
    pub fn update(&mut self, queued: f64, frames: usize) -> f64 {
        let dt = frames as f64 / self.rate;
        if dt <= 0.0 {
            return 1.0 + self.correction;
        }
        self.elapsed += dt;
        let smoothed = match self.smoothed {
            None => queued,
            Some(previous) => previous + (queued - previous) * (1.0 - (-dt / SMOOTHING).exp()),
        };
        self.smoothed = Some(smoothed);

        // Error in seconds of audio: positive when too much is queued.
        let error = (smoothed - self.target) / self.rate;
        let (response, reset) = if self.elapsed < ACQUISITION { FAST } else { SLOW };
        self.learned = (self.learned + error * dt / (response * reset)).clamp(-MAX_CORRECTION, MAX_CORRECTION);
        self.correction = (-error / response - self.learned).clamp(-MAX_CORRECTION, MAX_CORRECTION);
        1.0 + self.correction
    }

    /// Call after the queue was changed abruptly (silence inserted, frames dropped): the next
    /// measurement replaces the average instead of being blended into it. What was learned about
    /// the drift is kept.
    pub fn rebase(&mut self) {
        self.smoothed = None;
    }

    /// Current correction in parts per million; negative when the source clock runs fast.
    pub fn correction_ppm(&self) -> f64 {
        // Adding zero turns a negative zero into a plain one, so that it prints as "+0".
        self.correction * 1e6 + 0.0
    }
}

/// What to do before queueing new frames at the destination.
#[derive(Debug, PartialEq, Eq)]
pub enum Correction {
    None,
    /// Queue this many frames of silence first: the queue is about to run dry, or already has.
    Prime(usize),
    /// Discard this many of the oldest new frames: too much is queued, the audio would be late.
    Drop(usize),
}

/// Decides how to bring the queue back to `target` when `pending` new frames are about to follow
/// the `queued` ones and the total would be under `low` or over `high`.
pub fn queue_plan(queued: usize, pending: usize, target: usize, low: usize, high: usize) -> Correction {
    let after = queued + pending;
    if after < low {
        Correction::Prime(target - after)
    } else if after > high {
        Correction::Drop(pending.min(after - target))
    } else {
        Correction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remap_keeps_or_adapts_the_layout() {
        let mut out = Vec::new();
        remap(&[1.0, 2.0, 3.0, 4.0], 2, 2, 0.5, &mut out);
        assert_eq!(out, [0.5, 1.0, 1.5, 2.0]);

        out.clear();
        remap(&[1.0, 2.0], 1, 2, 1.0, &mut out);
        assert_eq!(out, [1.0, 1.0, 2.0, 2.0]);

        out.clear();
        remap(&[1.0], 1, 4, 1.0, &mut out);
        assert_eq!(out, [1.0, 1.0, 0.0, 0.0]);

        out.clear();
        remap(&[1.0, 3.0], 2, 1, 1.0, &mut out);
        assert_eq!(out, [2.0]);

        out.clear();
        remap(&[1.0, 2.0, 3.0, 4.0], 4, 2, 1.0, &mut out);
        assert_eq!(out, [1.0, 2.0]);

        out.clear();
        remap(&[1.0, 2.0], 2, 4, 1.0, &mut out);
        assert_eq!(out, [1.0, 2.0, 0.0, 0.0]);
    }

    #[test]
    fn framer_cuts_fixed_blocks_in_order() {
        let input: Vec<f32> = (0..2 * 1100).map(|i| i as f32).collect();
        let mut framer = Framer::new(2, 480);
        let mut blocks: Vec<Vec<f32>> = Vec::new();
        // Pushes of 100, 380 and 620 frames: the first two make one block, the third one more and 140 left.
        for range in [0..200, 200..960, 960..2200] {
            framer.push(&input[range]);
            framer.drain(|block| blocks.push(block.to_vec()));
        }
        assert_eq!(blocks.len(), 2);
        assert!(blocks.iter().all(|block| block.len() == 960));
        assert_eq!(blocks.concat(), input[..1920]);
        assert_eq!(framer.pending_frames(), 140);
        framer.push_silence(340);
        framer.drain(|block| blocks.push(block.to_vec()));
        assert_eq!(blocks.len(), 3);
        assert_eq!(&blocks[2][..280], &input[1920..]);
        assert!(blocks[2][280..].iter().all(|s| *s == 0.0));
        assert_eq!(framer.pending_frames(), 0);
    }

    #[test]
    fn peak_is_the_largest_magnitude() {
        assert_eq!(peak(&[0.1, -0.7, 0.3]), 0.7);
        assert_eq!(peak(&[]), 0.0);
    }

    #[test]
    fn resampler_is_transparent_at_ratio_one() {
        let input: Vec<f32> = (0..200).map(|i| i as f32).collect();
        let mut resampler = Resampler::new(2);
        let mut out = Vec::new();
        // Uneven blocks, including one shorter than the history.
        for block in [&input[..60], &input[60..62], &input[62..]] {
            resampler.process(block, 1.0, &mut out);
        }
        assert_eq!(out.len(), 200);
        assert_eq!(&out[..4], &[0.0; 4]);
        assert_eq!(&out[4..], &input[..196]);
    }

    #[test]
    fn resampler_produces_the_requested_amount() {
        for ratio in [0.995, 0.9999, 1.0, 1.0001, 1.005] {
            let mut resampler = Resampler::new(1);
            let block = vec![0.0f32; 480];
            let mut out = Vec::new();
            for _ in 0..1000 {
                resampler.process(&block, ratio, &mut out);
            }
            let expected = 480_000.0 * ratio;
            // The first frame comes out at once, so the count can be one more than the plain product.
            assert!((out.len() as f64 - expected).abs() < 2.0, "ratio {ratio}: {} frames, expected {expected}", out.len());
        }
    }

    #[test]
    fn resampler_keeps_a_tone_clean() {
        let (rate, frequency) = (48_000.0f64, 1_000.0f64);
        let tone = |position: f64| (2.0 * std::f64::consts::PI * frequency * position / rate).sin();
        let input: Vec<f32> = (0..48_000).map(|i| tone(f64::from(i)) as f32).collect();
        for ratio in [0.9995, 1.0005, 1.005] {
            let mut resampler = Resampler::new(1);
            let mut out = Vec::new();
            for block in input.chunks(480) {
                resampler.process(block, ratio, &mut out);
            }
            let step = 1.0 / ratio;
            let worst = out.iter().enumerate().skip(16).map(|(k, sample)| (f64::from(*sample) - tone(k as f64 * step - 2.0)).abs()).fold(0.0, f64::max);
            println!("tone at ratio {ratio}: worst error {worst:.6}");
            assert!(worst < 1e-3, "ratio {ratio}: worst error {worst}");
        }
    }

    const BLOCK: usize = 480;
    const TARGET: usize = 3 * BLOCK;
    const HIGH: usize = TARGET + 4 * BLOCK;

    #[test]
    fn queue_plan_only_acts_outside_the_limits() {
        assert_eq!(queue_plan(960, 480, TARGET, BLOCK, HIGH), Correction::None);
        // Exactly one block queued still satisfies the next pull.
        assert_eq!(queue_plan(0, 480, TARGET, BLOCK, HIGH), Correction::None);
        assert_eq!(queue_plan(0, 479, TARGET, BLOCK, HIGH), Correction::Prime(961));
        // The first write uses the target as its lower limit.
        assert_eq!(queue_plan(0, 480, TARGET, TARGET, HIGH), Correction::Prime(960));
        assert_eq!(queue_plan(1000, 4800, TARGET, BLOCK, HIGH), Correction::Drop(4360));
        // Never drops more than the new frames.
        assert_eq!(queue_plan(4000, 480, TARGET, BLOCK, HIGH), Correction::Drop(480));
    }

    struct Outcome {
        /// Pulls that found less than a block, after the queue was first filled.
        underruns: usize,
        primes: usize,
        drops: usize,
        /// Extremes of the queue right after a write, over the whole run.
        lowest: usize,
        highest: usize,
        /// The same over the second half of the run, with the mean correction in ppm.
        settled_lowest: usize,
        settled_highest: usize,
        mean_correction: f64,
    }

    /// Simulates the relay between two clocks for `seconds`. The source delivers 10 ms blocks on
    /// its own clock, which runs `drift` faster than the destination's; the destination removes
    /// 10 ms blocks on its clock. The queue is measured right after each write, as the relay does.
    /// With `stall_at`, the relay stops being scheduled for 100 ms at that time.
    fn simulate(drift: f64, seconds: f64, stall_at: Option<f64>) -> Outcome {
        let (rate, period) = (48_000u32, 0.01f64);
        let write_period = period / (1.0 + drift);
        let mut control = DriftControl::new(rate, TARGET);
        let (mut queue, mut carry, mut ratio, mut primed) = (0usize, 0.0f64, 1.0f64, false);
        let (mut next_write, mut next_pull) = (0.0f64, 0.0037f64);
        let mut backlog = 0usize; // blocks captured during a stall, delivered together afterwards
        let mut stalled_until = None;
        let mut outcome = Outcome { underruns: 0, primes: 0, drops: 0, lowest: usize::MAX, highest: 0, settled_lowest: usize::MAX, settled_highest: 0, mean_correction: 0.0 };
        let mut samples = 0usize;

        while next_write.min(next_pull) < seconds {
            if next_pull <= next_write {
                if primed {
                    if queue < BLOCK {
                        outcome.underruns += 1;
                        queue = 0;
                    } else {
                        queue -= BLOCK;
                    }
                }
                next_pull += period;
                continue;
            }
            let now = next_write;
            next_write += write_period;
            if let Some(at) = stall_at {
                if stalled_until.is_none() && now >= at {
                    stalled_until = Some(at + 0.1);
                }
            }
            backlog += 1;
            if stalled_until.is_some_and(|until| now < until) {
                continue;
            }
            let frames_in = backlog * BLOCK;
            backlog = 0;
            carry += frames_in as f64 * ratio;
            let mut pending = carry as usize;
            carry -= pending as f64;
            match queue_plan(queue, pending, TARGET, if primed { BLOCK } else { TARGET }, HIGH) {
                Correction::None => {}
                Correction::Prime(silence) => {
                    queue += silence;
                    outcome.primes += 1;
                    control.rebase();
                }
                Correction::Drop(frames) => {
                    pending -= frames;
                    outcome.drops += 1;
                    control.rebase();
                }
            }
            primed = true;
            queue += pending;
            ratio = control.update(queue as f64, frames_in);
            outcome.lowest = outcome.lowest.min(queue);
            outcome.highest = outcome.highest.max(queue);
            if now >= seconds / 2.0 {
                outcome.settled_lowest = outcome.settled_lowest.min(queue);
                outcome.settled_highest = outcome.settled_highest.max(queue);
                outcome.mean_correction += control.correction_ppm();
                samples += 1;
            }
        }
        outcome.mean_correction /= samples as f64;
        outcome
    }

    #[test]
    fn drift_is_absorbed_without_dropouts() {
        for drift_ppm in [0.0, 5.0, -5.0, 20.0, -20.0, 150.0, -150.0, 1000.0, -1000.0, 3000.0, -3000.0, 4000.0, -4000.0] {
            let outcome = simulate(drift_ppm * 1e-6, 1200.0, None);
            let label = format!(
                "drift {drift_ppm:>6} ppm: queue {}..{} frames ({}..{} once settled), mean correction {:>8.1} ppm, {} primes, {} drops, {} underruns",
                outcome.lowest, outcome.highest, outcome.settled_lowest, outcome.settled_highest, outcome.mean_correction, outcome.primes, outcome.drops, outcome.underruns
            );
            println!("{label}");
            // The first write fills the queue; nothing else is ever needed.
            assert_eq!((outcome.primes, outcome.drops, outcome.underruns), (1, 0, 0), "{label}");
            // The queue always keeps a block and a half and stays under the drop limit, start-up included.
            assert!(outcome.lowest >= BLOCK * 3 / 2 && outcome.highest < HIGH, "{label}");
            // Once settled it stays within a block and a quarter of the target: the wander described on DriftControl.
            assert!(outcome.settled_lowest >= TARGET - BLOCK * 5 / 4 && outcome.settled_highest <= TARGET + BLOCK * 5 / 4, "{label}");
            // The correction settles on the opposite of the drift. With a small drift the wander
            // takes minutes to average out, hence the fixed part of the tolerance.
            assert!((outcome.mean_correction + drift_ppm).abs() <= 30.0f64.max(drift_ppm.abs() * 0.05), "{label}");
        }
    }

    #[test]
    fn a_stall_costs_one_correction_and_then_recovers() {
        let outcome = simulate(100e-6, 600.0, Some(200.0));
        println!("stall: queue {}..{} frames once settled, {} primes, {} drops, {} underruns", outcome.settled_lowest, outcome.settled_highest, outcome.primes, outcome.drops, outcome.underruns);
        // The start-up fill, then one correction for the ten blocks that arrive together after the stall.
        assert_eq!(outcome.primes + outcome.drops, 2);
        // The destination ran dry during the 100 ms stall: at most ten empty pulls.
        assert!(outcome.underruns <= 10);
        assert!(outcome.settled_lowest >= TARGET - BLOCK * 5 / 4 && outcome.settled_highest <= TARGET + BLOCK * 5 / 4);
    }
}
