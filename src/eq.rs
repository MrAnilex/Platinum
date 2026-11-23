//! The channels' parametric equalizer: where its settings sit in the audio module's store, and the
//! response curve its ten filters draw, for display.
//!
//! Each filter has five settings in a row: on/off, frequency (Hz), Q, gain (dB) and type. The curve
//! is computed with the usual second-order filter formulas (R. Bristow-Johnson's cookbook) at
//! 48 kHz; it shows what the settings ask for, not a measurement of the module.
//!
//! Playback channels also have three fixed boosts (bass, voice, treble) outside the ten filters.
//! GG draws them on the same graph as three more peaking filters; [`curve_path_with_boosts`] does
//! the same.

use std::f64::consts::PI;

/// Number of filters in each equalizer.
pub const BANDS: usize = 10;

/// Where an equalizer's settings start in a channel's store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// The equalizer's own on/off setting.
    pub enabled: i32,
    /// The first filter's on/off setting; filter n (from 0) starts at `first + 5 n`.
    pub first: i32,
}

/// The playback equalizer (Game, Media, Aux).
pub const PLAYBACK: Layout = Layout { enabled: 108, first: 109 };
/// The capture-side equalizer, used by Chat and by the microphone.
pub const CAPTURE: Layout = Layout { enabled: 216, first: 217 };

impl Layout {
    /// The settings of filter `band` (from 0): (on/off, frequency, Q, gain, type).
    pub fn band(self, band: usize) -> (i32, i32, i32, i32, i32) {
        let base = self.first + 5 * band as i32;
        (base, base + 1, base + 2, base + 3, base + 4)
    }
}

/// Filter shapes, in the order of the number the module stores for them.
///
/// The module stores GG's own `FilterType` numbering unchanged: the service converts the preset's
/// type names with a ten-way switch that returns 0 to 9 in this order, and every filter of the five
/// presets selected on the test machine reads back with the number this order predicts
/// (`analysis/sonar/presets.md`, section 2). The module's unused defaults, 2 for the first filter
/// and 1 for the last, are thus a high-pass and a low-pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// 0: the filter is skipped; drawn flat.
    ByPass,
    /// 1: cuts above the frequency.
    LowPass,
    /// 2: cuts below the frequency.
    HighPass,
    /// 3: band-pass whose peak gain equals Q (the cookbook's "constant skirt gain").
    BandPassQ,
    /// 4: band-pass with a 0 dB peak.
    BandPass,
    /// 5: removes a narrow band around the frequency.
    Notch,
    /// 6: all-pass: only the phase changes, so the magnitude is flat.
    AllPass,
    /// 7: bell around the frequency.
    Peaking,
    /// 8: lifts or lowers everything below the frequency.
    LowShelf,
    /// 9: lifts or lowers everything above the frequency.
    HighShelf,
}

impl Kind {
    /// Every kind, indexed by its number in the module.
    pub const ALL: [Kind; 10] = [
        Kind::ByPass,
        Kind::LowPass,
        Kind::HighPass,
        Kind::BandPassQ,
        Kind::BandPass,
        Kind::Notch,
        Kind::AllPass,
        Kind::Peaking,
        Kind::LowShelf,
        Kind::HighShelf,
    ];

    /// The kind the module means by `code`. The module's range is 0 to 9 and the service refuses
    /// anything else, so another number cannot come from GG; it is drawn flat, as a bypass.
    pub fn from_code(code: i32) -> Kind {
        usize::try_from(code).ok().and_then(|i| Kind::ALL.get(i).copied()).unwrap_or(Kind::ByPass)
    }

    /// The number the module stores for this kind.
    pub fn code(self) -> i32 {
        Kind::ALL.iter().position(|k| *k == self).unwrap_or(0) as i32
    }

    /// Whether the gain setting changes the shape (it does not for cuts, band-passes and
    /// all-passes).
    pub fn uses_gain(self) -> bool {
        matches!(self, Kind::Peaking | Kind::LowShelf | Kind::HighShelf)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Band {
    pub enabled: bool,
    pub frequency: f64,
    pub q: f64,
    pub gain: f64,
    pub kind: Kind,
}

const SAMPLE_RATE: f64 = 48_000.0;

/// Second-order filter coefficients (b0, b1, b2, a0, a1, a2).
fn coefficients(band: &Band) -> [f64; 6] {
    let w0 = 2.0 * PI * band.frequency.clamp(10.0, SAMPLE_RATE * 0.49) / SAMPLE_RATE;
    let (sin, cos) = w0.sin_cos();
    let q = band.q.max(0.05);
    let alpha = sin / (2.0 * q);
    let a = 10f64.powf(band.gain / 40.0);
    let root = a.sqrt();
    match band.kind {
        Kind::ByPass => [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
        Kind::Peaking => [1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a],
        Kind::LowShelf => [
            a * ((a + 1.0) - (a - 1.0) * cos + 2.0 * root * alpha),
            2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
            a * ((a + 1.0) - (a - 1.0) * cos - 2.0 * root * alpha),
            (a + 1.0) + (a - 1.0) * cos + 2.0 * root * alpha,
            -2.0 * ((a - 1.0) + (a + 1.0) * cos),
            (a + 1.0) + (a - 1.0) * cos - 2.0 * root * alpha,
        ],
        Kind::HighShelf => [
            a * ((a + 1.0) + (a - 1.0) * cos + 2.0 * root * alpha),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
            a * ((a + 1.0) + (a - 1.0) * cos - 2.0 * root * alpha),
            (a + 1.0) - (a - 1.0) * cos + 2.0 * root * alpha,
            2.0 * ((a - 1.0) - (a + 1.0) * cos),
            (a + 1.0) - (a - 1.0) * cos - 2.0 * root * alpha,
        ],
        Kind::LowPass => [(1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha],
        Kind::HighPass => [(1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha],
        Kind::Notch => [1.0, -2.0 * cos, 1.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha],
        // sin / 2 = Q·alpha: the peak reaches Q instead of 1.
        Kind::BandPassQ => [sin / 2.0, 0.0, -sin / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha],
        Kind::BandPass => [alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha],
        Kind::AllPass => [1.0 - alpha, -2.0 * cos, 1.0 + alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha],
    }
}

/// Gain in dB of one filter at `frequency`.
fn band_db(band: &Band, frequency: f64) -> f64 {
    let [b0, b1, b2, a0, a1, a2] = coefficients(band);
    let w = 2.0 * PI * frequency / SAMPLE_RATE;
    // H(e^jw) = (b0 + b1 e^-jw + b2 e^-2jw) / (a0 + a1 e^-jw + a2 e^-2jw)
    let (c1, s1, c2, s2) = (w.cos(), w.sin(), (2.0 * w).cos(), (2.0 * w).sin());
    let (nr, ni) = (b0 + b1 * c1 + b2 * c2, -(b1 * s1 + b2 * s2));
    let (dr, di) = (a0 + a1 * c1 + a2 * c2, -(a1 * s1 + a2 * s2));
    let power = (nr * nr + ni * ni) / (dr * dr + di * di).max(1e-30);
    10.0 * power.max(1e-30).log10()
}

/// Gain in dB of the whole equalizer at `frequency`: the sum of the filters that are on.
pub fn response_db(bands: &[Band], frequency: f64) -> f64 {
    bands.iter().filter(|b| b.enabled).map(|b| band_db(b, frequency)).sum()
}

/// One of the three boosts of a playback channel: whether it is on and its gain in dB.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Boost {
    pub enabled: bool,
    pub gain: f64,
}

/// The bass, voice and treble boosts of a playback channel (settings 44/45, 165/166 and 99/100).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Boosts {
    pub bass: Boost,
    pub voice: Boost,
    pub treble: Boost,
}

/// Frequency (Hz) and Q at which GG draws each boost: the same as the module's own fixed boost
/// frequencies (`kSet_BassBoostFrequencyHz` 80, `kSet_MidBoostFrequencyHz` 2000 with a bandwidth of
/// 0.5, `kSet_TrebleBoostFrequencyHz` 8000) and the Q of GG's graph markers.
pub const BASS_BOOST: (f64, f64) = (80.0, 0.707);
pub const VOICE_BOOST: (f64, f64) = (2_000.0, 0.5);
pub const TREBLE_BOOST: (f64, f64) = (8_000.0, 0.707);

impl Boosts {
    /// The boosts that are on, as peaking filters, for the curve and for drawing their markers
    /// (at `frequency_to_x(band.frequency)` and `band.gain`).
    pub fn bands(&self) -> Vec<Band> {
        [(self.bass, BASS_BOOST), (self.voice, VOICE_BOOST), (self.treble, TREBLE_BOOST)]
            .into_iter()
            .filter(|(boost, _)| boost.enabled)
            .map(|(boost, (frequency, q))| Band { enabled: true, frequency, q, gain: boost.gain, kind: Kind::Peaking })
            .collect()
    }
}

/// Lowest and highest frequency the graph shows.
pub const GRAPH_LOW: f64 = 20.0;
pub const GRAPH_HIGH: f64 = 20_000.0;

/// Position of a frequency across the graph, from 0 (20 Hz) to 1 (20 kHz), on a log scale.
pub fn frequency_to_x(frequency: f64) -> f64 {
    (frequency.clamp(GRAPH_LOW, GRAPH_HIGH) / GRAPH_LOW).log10() / (GRAPH_HIGH / GRAPH_LOW).log10()
}

pub fn x_to_frequency(x: f64) -> f64 {
    GRAPH_LOW * (GRAPH_HIGH / GRAPH_LOW).powf(x.clamp(0.0, 1.0))
}

/// The curve as SVG path commands in a `width` × `height` box, where the vertical middle is 0 dB
/// and the top and bottom are +`range` and -`range` dB.
pub fn curve_path(bands: &[Band], width: f64, height: f64, range: f64) -> String {
    const POINTS: usize = 160;
    let mut path = String::new();
    for i in 0..=POINTS {
        let x = i as f64 / POINTS as f64;
        let db = response_db(bands, x_to_frequency(x)).clamp(-range * 1.2, range * 1.2);
        let y = height / 2.0 - db / range * (height / 2.0);
        path.push_str(&format!("{}{:.1} {:.1} ", if i == 0 { 'M' } else { 'L' }, x * width, y));
    }
    path.trim_end().to_string()
}

/// The curve of a playback channel: its ten filters plus the boosts that are on, as GG draws it.
pub fn curve_path_with_boosts(bands: &[Band], boosts: &Boosts, width: f64, height: f64, range: f64) -> String {
    let mut all = bands.to_vec();
    all.extend(boosts.bands());
    curve_path(&all, width, height, range)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peak(frequency: f64, gain: f64) -> Band {
        Band { enabled: true, frequency, q: 1.0, gain, kind: Kind::Peaking }
    }

    fn shaped(kind: Kind, frequency: f64, q: f64, gain: f64) -> Band {
        Band { enabled: true, frequency, q, gain, kind }
    }

    #[test]
    fn kinds_follow_the_modules_numbering() {
        let expected = [
            (0, Kind::ByPass),
            (1, Kind::LowPass),
            (2, Kind::HighPass),
            (3, Kind::BandPassQ),
            (4, Kind::BandPass),
            (5, Kind::Notch),
            (6, Kind::AllPass),
            (7, Kind::Peaking),
            (8, Kind::LowShelf),
            (9, Kind::HighShelf),
        ];
        for (code, kind) in expected {
            assert_eq!(Kind::from_code(code), kind, "code {code}");
            assert_eq!(kind.code(), code, "{kind:?}");
        }
        assert_eq!(Kind::from_code(-1), Kind::ByPass);
        assert_eq!(Kind::from_code(10), Kind::ByPass);
    }

    #[test]
    fn a_peaking_filter_reaches_its_gain_at_its_frequency() {
        let band = peak(1000.0, 6.0);
        assert!((response_db(&[band], 1000.0) - 6.0).abs() < 0.05);
        assert!(response_db(&[band], 50.0).abs() < 0.3);
        assert!(response_db(&[Band { enabled: false, ..band }], 1000.0).abs() < 1e-9);
    }

    #[test]
    fn shelves_lift_their_side_only() {
        let low = shaped(Kind::LowShelf, 100.0, 0.707, 6.0);
        let high = Band { kind: Kind::HighShelf, frequency: 8000.0, ..low };
        assert!((response_db(&[low], 25.0) - 6.0).abs() < 0.3);
        assert!(response_db(&[low], 5000.0).abs() < 0.3);
        assert!((response_db(&[high], 19000.0) - 6.0).abs() < 0.5);
        assert!(response_db(&[high], 100.0).abs() < 0.3);
    }

    #[test]
    fn code_2_cuts_the_low_end_and_code_1_the_high_end() {
        // The microphone preset's first filter: a 40 Hz high-pass, stored as 2.
        let high_pass = shaped(Kind::from_code(2), 40.0, 0.7071, 0.0);
        assert!(response_db(&[high_pass], 10.0) < -20.0);
        assert!((response_db(&[high_pass], 40.0) + 3.0).abs() < 0.2);
        assert!(response_db(&[high_pass], 1000.0).abs() < 0.1);
        // The module's default last filter: a 16 kHz low-pass, stored as 1.
        let low_pass = shaped(Kind::from_code(1), 16_000.0, 0.7071, 0.0);
        assert!(response_db(&[low_pass], 1000.0).abs() < 0.1);
        assert!(response_db(&[low_pass], 23_000.0) < -6.0);
    }

    #[test]
    fn notch_band_pass_all_pass_and_bypass_have_their_own_shapes() {
        let notch = shaped(Kind::Notch, 1000.0, 2.0, 12.0);
        assert!(response_db(&[notch], 1000.0) < -40.0);
        assert!(response_db(&[notch], 100.0).abs() < 0.2);

        let band_pass = shaped(Kind::BandPass, 1000.0, 2.0, 12.0);
        assert!(response_db(&[band_pass], 1000.0).abs() < 0.05);
        assert!(response_db(&[band_pass], 100.0) < -20.0);

        // Peak gain equals Q: 20·log10(4) ≈ 12 dB.
        let band_pass_q = shaped(Kind::BandPassQ, 1000.0, 4.0, 0.0);
        assert!((response_db(&[band_pass_q], 1000.0) - 20.0 * 4f64.log10()).abs() < 0.05);

        for kind in [Kind::AllPass, Kind::ByPass] {
            let band = shaped(kind, 1000.0, 2.0, 12.0);
            for f in [20.0, 300.0, 1000.0, 5000.0, 20_000.0] {
                assert!(response_db(&[band], f).abs() < 1e-6, "{kind:?} at {f} Hz");
            }
        }
        assert!(!Kind::Notch.uses_gain() && !Kind::AllPass.uses_gain() && Kind::HighShelf.uses_gain());
    }

    #[test]
    fn boosts_are_drawn_only_when_on() {
        let off = Boosts::default();
        assert!(off.bands().is_empty());
        let boosts = Boosts { bass: Boost { enabled: true, gain: 6.0 }, voice: Boost { enabled: false, gain: 9.0 }, treble: Boost { enabled: true, gain: -4.0 } };
        let bands = boosts.bands();
        assert_eq!(bands.len(), 2);
        assert_eq!((bands[0].frequency, bands[0].q, bands[0].gain), (80.0, 0.707, 6.0));
        assert_eq!((bands[1].frequency, bands[1].q, bands[1].gain), (8000.0, 0.707, -4.0));
        assert!((response_db(&bands, 80.0) - 6.0).abs() < 0.1);
        assert!((response_db(&bands, 8000.0) + 4.0).abs() < 0.1);

        let flat = [peak(1000.0, 0.0)];
        assert_eq!(curve_path_with_boosts(&flat, &off, 1000.0, 200.0, 12.0), curve_path(&flat, 1000.0, 200.0, 12.0));
        assert_ne!(curve_path_with_boosts(&flat, &boosts, 1000.0, 200.0, 12.0), curve_path(&flat, 1000.0, 200.0, 12.0));
        let voice = Boosts { voice: Boost { enabled: true, gain: 3.0 }, ..off }.bands();
        assert_eq!((voice[0].frequency, voice[0].q), (2000.0, 0.5));
    }

    #[test]
    fn the_frequency_scale_round_trips() {
        for f in [20.0, 100.0, 1000.0, 12_345.0, 20_000.0] {
            assert!((x_to_frequency(frequency_to_x(f)) - f).abs() / f < 1e-9);
        }
        assert_eq!(PLAYBACK.band(9), (154, 155, 156, 157, 158));
        assert_eq!(CAPTURE.band(9), (262, 263, 264, 265, 266));
    }

    #[test]
    fn the_curve_is_a_path_across_the_box() {
        let path = curve_path(&[peak(1000.0, 0.0)], 1000.0, 200.0, 12.0);
        assert!(path.starts_with("M0.0 100.0 "), "{path}");
        assert!(path.ends_with("L1000.0 100.0"), "{path}");
    }
}
