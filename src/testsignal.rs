//! Synthetic test signals and simple measurements, to check offline what an effects engine does to
//! a known input. Nothing here touches an audio device, and nothing depends on the platform.

use std::f64::consts::PI;

/// Deterministic white noise, uniform in -1..1: the same seed always gives the same noise.
pub struct Noise(u64);

impl Noise {
    pub fn new(seed: u64) -> Noise {
        Noise(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next(&mut self) -> f64 {
        // xorshift64*: small, fast and good enough for test noise.
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let value = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11;
        value as f64 / (1u64 << 52) as f64 - 1.0
    }
}

pub fn db_to_linear(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

pub fn linear_to_db(value: f64) -> f64 {
    if value > 0.0 { 20.0 * value.log10() } else { f64::NEG_INFINITY }
}

/// What one segment of a test signal contains. Levels are RMS, in dB relative to full scale.
#[derive(Clone, Copy, Debug)]
pub enum Part {
    Silence,
    /// Steady white noise.
    Noise { db: f64 },
    /// A pure tone.
    Tone { hz: f64, db: f64 },
    /// A synthetic voice: a buzzing source at a speaking pitch, shaped into changing vowels, in
    /// syllables of about a quarter of a second. Not real speech, but closer to it than a tone.
    Voice { db: f64 },
    /// The same voice with steady white noise underneath.
    VoiceInNoise { voice_db: f64, noise_db: f64 },
    /// One click every half second, for measuring the delay through an engine.
    Clicks { db: f64 },
}

/// Channels of a spatial test, in Windows' 7.1 order: front left, front right, centre, bass, back
/// left, back right, side left, side right.
pub const SPATIAL_CHANNELS: usize = 8;
/// Windows' speaker mask of that layout (front pair, centre, bass, back pair, side pair).
pub const SPATIAL_MASK: u32 = 0x63F;
/// The speakers a spatial test steps through, by place in that order: clockwise from the front
/// left (front left, centre, front right, side right, back right, back left, side left). The bass
/// channel is left out: low sounds do not tell where they come from.
pub const SPATIAL_ROUND: [usize; 7] = [0, 2, 1, 7, 5, 4, 6];
/// Each speaker gets three short bursts of noise (broadband, so the ear can place them), then a
/// rest before the next speaker.
const SPATIAL_BURSTS: usize = 3;
const SPATIAL_BURST_SECONDS: f64 = 0.12;
const SPATIAL_GAP_SECONDS: f64 = 0.06;
const SPATIAL_REST_SECONDS: f64 = 0.35;

/// A spatial test: bursts of noise on one speaker of a 7.1 layout at a time, round the room
/// (`SPATIAL_ROUND`), the bursts at `db` RMS. Interleaved, `SPATIAL_CHANNELS` channels in Windows'
/// order; every other channel is silent at each moment, and the bass channel always is. About six
/// seconds. Generated, not recorded: it stands in for GG's spatial test sound when GG is absent.
pub fn spatial_round(rate: u32, db: f64) -> Vec<f32> {
    let rate_f = f64::from(rate);
    let frames = |seconds: f64| (seconds * rate_f).round() as usize;
    let (burst, gap, rest) = (frames(SPATIAL_BURST_SECONDS), frames(SPATIAL_GAP_SECONDS), frames(SPATIAL_REST_SECONDS));
    // 5 ms raised-cosine edges, so that no burst starts or stops with a click.
    let edge = frames(0.005).max(1);
    let mut noise = Noise::new(17);
    // Uniform noise in -1..1 has an RMS of 1/sqrt(3).
    let amplitude = 3f64.sqrt() * db_to_linear(db);
    let mut out = Vec::with_capacity(SPATIAL_ROUND.len() * SPATIAL_CHANNELS * (SPATIAL_BURSTS * (burst + gap) + rest));
    let silence = |out: &mut Vec<f32>, count: usize| out.resize(out.len() + count * SPATIAL_CHANNELS, 0.0);
    for speaker in SPATIAL_ROUND {
        for index in 0..SPATIAL_BURSTS {
            for i in 0..burst {
                let from_edge = i.min(burst - 1 - i);
                let envelope = if from_edge < edge { (PI * from_edge as f64 / edge as f64 / 2.0).sin().powi(2) } else { 1.0 };
                let mut frame = [0.0f32; SPATIAL_CHANNELS];
                frame[speaker] = (noise.next() * amplitude * envelope).clamp(-1.0, 1.0) as f32;
                out.extend_from_slice(&frame);
            }
            if index + 1 < SPATIAL_BURSTS {
                silence(&mut out, gap);
            }
        }
        silence(&mut out, rest);
    }
    out
}

#[derive(Clone, Copy, Debug)]
pub struct Segment {
    pub name: &'static str,
    pub part: Part,
    pub seconds: f64,
}

/// Formant frequencies (Hz) of a few vowels, roughly as spoken by an adult.
const VOWELS: [[f64; 3]; 4] = [[730.0, 1090.0, 2440.0], [270.0, 2290.0, 3010.0], [300.0, 870.0, 2240.0], [530.0, 1840.0, 2480.0]];

/// One sample of the synthetic voice at time `t` seconds, before level scaling.
fn voice_sample(t: f64) -> f64 {
    let syllable = 0.25;
    let index = (t / syllable) as usize;
    let phase = t / syllable - index as f64;
    // Each syllable: a smooth rise and fall over 80 % of its length, then a short gap.
    let envelope = if phase < 0.8 { (PI * phase / 0.8).sin().powi(2) } else { 0.0 };
    if envelope == 0.0 {
        return 0.0;
    }
    let formants = VOWELS[index % VOWELS.len()];
    // A pitch around 120 Hz that drifts slowly, as a speaking voice does.
    let pitch = 120.0 * (1.0 + 0.06 * (2.0 * PI * 0.7 * t).sin());
    let pitch_phase = 120.0 * t - 0.06 * 120.0 / (2.0 * PI * 0.7) * ((2.0 * PI * 0.7 * t).cos() - 1.0);
    let mut sample = 0.0;
    let mut harmonic = 1;
    while (harmonic as f64) * pitch < 4000.0 {
        let frequency = harmonic as f64 * pitch;
        // Harmonics near a formant are louder: three resonance bumps, 100 Hz wide.
        let weight: f64 = formants.iter().map(|f| (-((frequency - f) / 100.0).powi(2)).exp()).sum::<f64>() + 0.05;
        sample += weight * (2.0 * PI * harmonic as f64 * pitch_phase).sin() / (harmonic as f64).sqrt();
        harmonic += 1;
    }
    envelope * sample
}

/// RMS of the unscaled synthetic voice over its active parts, measured once.
fn voice_rms() -> f64 {
    let rate = 16_000.0;
    let (mut sum, mut count) = (0.0, 0usize);
    for i in 0..(rate as usize * 2) {
        let sample = voice_sample(i as f64 / rate);
        sum += sample * sample;
        count += 1;
    }
    (sum / count as f64).sqrt()
}

/// Renders the segments one after the other as interleaved samples, the same signal on every channel.
#[cfg_attr(not(test), allow(dead_code))]
pub fn render(segments: &[Segment], rate: u32, channels: usize, seed: u64) -> Vec<f32> {
    render_counting_clips(segments, rate, channels, seed).0
}

/// Like `render`, also counting the frames that had to be clipped to -1..1: a correct test signal
/// has none.
pub fn render_counting_clips(segments: &[Segment], rate: u32, channels: usize, seed: u64) -> (Vec<f32>, usize) {
    let mut clipped = 0;
    let rate_f = f64::from(rate);
    let mut noise = Noise::new(seed);
    let voice_scale = 1.0 / voice_rms();
    // Uniform noise in -1..1 has an RMS of 1/sqrt(3).
    let noise_scale = 3f64.sqrt();
    let mut out = Vec::new();
    for segment in segments {
        let frames = (segment.seconds * rate_f).round() as usize;
        for i in 0..frames {
            let t = i as f64 / rate_f;
            let sample = match segment.part {
                Part::Silence => 0.0,
                Part::Noise { db } => noise.next() * noise_scale * db_to_linear(db),
                Part::Tone { hz, db } => (2.0 * PI * hz * t).sin() * 2f64.sqrt() * db_to_linear(db),
                Part::Voice { db } => voice_sample(t) * voice_scale * db_to_linear(db),
                Part::VoiceInNoise { voice_db, noise_db } => voice_sample(t) * voice_scale * db_to_linear(voice_db) + noise.next() * noise_scale * db_to_linear(noise_db),
                Part::Clicks { db } => {
                    if i % (rate as usize / 2) == rate as usize / 4 { db_to_linear(db) } else { 0.0 }
                }
            };
            if sample.abs() > 1.0 {
                clipped += 1;
            }
            let sample = sample.clamp(-1.0, 1.0) as f32;
            out.extend(std::iter::repeat_n(sample, channels));
        }
    }
    (out, clipped)
}

/// Where each segment starts and ends, in frames.
pub fn bounds(segments: &[Segment], rate: u32) -> Vec<(usize, usize)> {
    let mut start = 0;
    segments
        .iter()
        .map(|segment| {
            let frames = (segment.seconds * f64::from(rate)).round() as usize;
            let range = (start, start + frames);
            start += frames;
            range
        })
        .collect()
}

/// RMS over frames `start..end`, all channels together.
pub fn rms(samples: &[f32], channels: usize, start: usize, end: usize) -> f64 {
    let end = end.min(samples.len() / channels);
    if end <= start {
        return 0.0;
    }
    let slice = &samples[start * channels..end * channels];
    (slice.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / slice.len() as f64).sqrt()
}

/// RMS amplitude of the component at `hz` in channel 0 over frames `start..end` (Goertzel).
pub fn tone_rms(samples: &[f32], channels: usize, rate: u32, hz: f64, start: usize, end: usize) -> f64 {
    let end = end.min(samples.len() / channels);
    if end <= start {
        return 0.0;
    }
    let coefficient = 2.0 * (2.0 * PI * hz / f64::from(rate)).cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for frame in start..end {
        let s0 = f64::from(samples[frame * channels]) + coefficient * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coefficient * s1 * s2;
    let n = (end - start) as f64;
    // Amplitude of a sinusoid is 2 * sqrt(power) / n; its RMS is that over sqrt(2).
    2.0 * power.max(0.0).sqrt() / n / 2f64.sqrt()
}

/// RMS of each consecutive window of `window` frames, all channels together.
pub fn window_levels(samples: &[f32], channels: usize, window: usize) -> Vec<f64> {
    let frames = samples.len() / channels;
    (0..frames / window).map(|i| rms(samples, channels, i * window, (i + 1) * window)).collect()
}

/// The value below which `fraction` of the levels fall (0.1 gives the quiet moments: the noise
/// floor; 0.9 gives the loud ones: speech, when someone talks).
pub fn percentile(levels: &[f64], fraction: f64) -> f64 {
    if levels.is_empty() {
        return 0.0;
    }
    let mut sorted = levels.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[((sorted.len() - 1) as f64 * fraction.clamp(0.0, 1.0)).round() as usize]
}

/// The delay in frames, up to `max_lag`, at which channel 0 of `output` best matches channel 0 of
/// `input` over frames `start..end`, with the normalised correlation at that delay (1 = identical
/// shape, 0 = unrelated).
pub fn delay(input: &[f32], output: &[f32], channels: usize, start: usize, end: usize, max_lag: usize) -> (usize, f64) {
    let available = (output.len() / channels).saturating_sub(max_lag);
    let end = end.min(input.len() / channels).min(available);
    if end <= start {
        return (0, 0.0);
    }
    let x = |i: usize| f64::from(input[i * channels]);
    let y = |i: usize| f64::from(output[i * channels]);
    let energy_x: f64 = (start..end).map(|i| x(i).powi(2)).sum();
    let mut best = (0, 0.0);
    for lag in 0..=max_lag {
        let (mut cross, mut energy_y) = (0.0, 0.0);
        for i in start..end {
            cross += x(i) * y(i + lag);
            energy_y += y(i + lag).powi(2);
        }
        let correlation = if energy_x > 0.0 && energy_y > 0.0 { cross / (energy_x * energy_y).sqrt() } else { 0.0 };
        if correlation > best.1 {
            best = (lag, correlation);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    #[test]
    fn noise_is_repeatable_and_centred() {
        let (mut a, mut b) = (Noise::new(7), Noise::new(7));
        let values: Vec<f64> = (0..100_000).map(|_| a.next()).collect();
        assert!(values.iter().zip((0..100_000).map(|_| b.next())).all(|(x, y)| *x == y));
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        let rms = (values.iter().map(|v| v * v).sum::<f64>() / values.len() as f64).sqrt();
        assert!(mean.abs() < 0.01, "mean {mean}");
        assert!((rms - 1.0 / 3f64.sqrt()).abs() < 0.01, "rms {rms}");
        assert!(values.iter().all(|v| (-1.0..1.0).contains(v)));
    }

    #[test]
    fn the_spatial_test_goes_round_the_speakers_one_at_a_time() {
        let signal = spatial_round(RATE, -18.0);
        let frames = signal.len() / SPATIAL_CHANNELS;
        assert_eq!(signal.len() % SPATIAL_CHANNELS, 0);
        let seconds = frames as f64 / f64::from(RATE);
        assert!((5.0..7.0).contains(&seconds), "{seconds} s");
        // Never two speakers at once, never the bass channel.
        assert!(signal.chunks_exact(SPATIAL_CHANNELS).all(|frame| frame.iter().filter(|s| **s != 0.0).count() <= 1 && frame[3] == 0.0));
        // The speakers sound in the round's order, each in its own seventh of the test.
        let span = frames / SPATIAL_ROUND.len();
        for (step, speaker) in SPATIAL_ROUND.iter().enumerate() {
            let part = &signal[step * span * SPATIAL_CHANNELS..(step + 1) * span * SPATIAL_CHANNELS];
            let loudest = part.chunks_exact(SPATIAL_CHANNELS).map(|frame| frame.iter().enumerate().filter(|(_, s)| **s != 0.0).map(|(c, _)| c).next()).flatten().next();
            assert_eq!(loudest, Some(*speaker), "step {step}");
        }
        // The bursts have the level asked for, and nothing clips.
        let burst = (SPATIAL_BURST_SECONDS * f64::from(RATE)) as usize;
        let level = linear_to_db(rms(&signal, SPATIAL_CHANNELS, burst / 4, burst * 3 / 4) * (SPATIAL_CHANNELS as f64).sqrt());
        assert!((level + 18.0).abs() < 0.5, "burst level {level}");
        assert!(signal.iter().all(|s| s.abs() < 1.0));
    }

    #[test]
    fn segments_have_the_requested_levels() {
        let segments = [
            Segment { name: "silence", part: Part::Silence, seconds: 0.5 },
            Segment { name: "noise", part: Part::Noise { db: -30.0 }, seconds: 1.0 },
            Segment { name: "tone", part: Part::Tone { hz: 1000.0, db: -20.0 }, seconds: 1.0 },
            Segment { name: "voice", part: Part::Voice { db: -20.0 }, seconds: 2.0 },
        ];
        let signal = render(&segments, RATE, 2, 1);
        let spans = bounds(&segments, RATE);
        assert_eq!(signal.len(), 2 * spans.last().unwrap().1);
        let level = |index: usize| linear_to_db(rms(&signal, 2, spans[index].0, spans[index].1));
        assert_eq!(rms(&signal, 2, spans[0].0, spans[0].1), 0.0);
        assert!((level(1) + 30.0).abs() < 0.2, "noise {}", level(1));
        assert!((level(2) + 20.0).abs() < 0.05, "tone {}", level(2));
        assert!((level(3) + 20.0).abs() < 1.0, "voice {}", level(3));
        // The tone measured by Goertzel matches its RMS; the noise has almost nothing at 1 kHz.
        let tone = linear_to_db(tone_rms(&signal, 2, RATE, 1000.0, spans[2].0, spans[2].1));
        assert!((tone + 20.0).abs() < 0.05, "goertzel {tone}");
        let noise_at_tone = linear_to_db(tone_rms(&signal, 2, RATE, 1000.0, spans[1].0, spans[1].1));
        assert!(noise_at_tone < -60.0, "noise at 1 kHz {noise_at_tone}");
    }

    #[test]
    fn percentiles_separate_quiet_and_loud_moments() {
        // 2 s of quiet noise then 1 s of loud tone: the 10th percentile sees the noise, the 90th the tone.
        let segments = [
            Segment { name: "quiet", part: Part::Noise { db: -60.0 }, seconds: 2.0 },
            Segment { name: "loud", part: Part::Tone { hz: 440.0, db: -20.0 }, seconds: 1.0 },
        ];
        let signal = render(&segments, RATE, 2, 5);
        let levels = window_levels(&signal, 2, 4_800);
        assert_eq!(levels.len(), 30);
        assert!((linear_to_db(percentile(&levels, 0.1)) + 60.0).abs() < 1.0);
        assert!((linear_to_db(percentile(&levels, 0.9)) + 20.0).abs() < 0.2);
        assert_eq!(percentile(&[], 0.5), 0.0);
    }

    #[test]
    fn delay_finds_a_known_shift() {
        let segments = [Segment { name: "voice", part: Part::VoiceInNoise { voice_db: -20.0, noise_db: -40.0 }, seconds: 1.0 }];
        let input = render(&segments, RATE, 1, 3);
        let shift = 731;
        let mut output = vec![0.0f32; shift];
        output.extend(input.iter().map(|s| s * 0.5));
        let (lag, correlation) = delay(&input, &output, 1, 0, 24_000, 2_000);
        assert_eq!(lag, shift);
        assert!(correlation > 0.999, "correlation {correlation}");
    }
}

/// Writes interleaved 32-bit float samples as a WAV file (WAVE_FORMAT_IEEE_FLOAT).
pub fn write_wav(path: &std::path::Path, samples: &[f32], channels: u16, rate: u32) -> std::io::Result<()> {
    let data_bytes = (samples.len() * 4) as u32;
    let mut bytes = Vec::with_capacity(44 + samples.len() * 4);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&rate.to_le_bytes());
    bytes.extend_from_slice(&(rate * u32::from(channels) * 4).to_le_bytes());
    bytes.extend_from_slice(&(channels * 4).to_le_bytes());
    bytes.extend_from_slice(&32u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_bytes.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    if let Some(folder) = path.parent() {
        std::fs::create_dir_all(folder)?;
    }
    std::fs::write(path, bytes)
}
