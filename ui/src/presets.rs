//! The channels' presets as the window edits them: which preset a channel uses, the edits its
//! effect cards make, GG's rules that tie fields together, and how a changed preset is told from
//! the one it came from.
//!
//! These are pure functions, shared by the window (which shows an edit at once) and by the engine
//! (which turns the same edit into setting writes with `preset::plan` and `preset::apply`, so that
//! GG's apply rules run on every edit, as in GG where each edit saves the whole preset: B20 in
//! `analysis/sonar/gg-ui-parity.md`).
//!
//! Ranges are those of GG's own controls (sections 9 to 11 of the same file), not the module's,
//! which are wider: GG's sliders never send more than ±12 dB to a boost, for instance.

use platinum::eq::Kind;
use platinum::i18n::{mark, tr};
use platinum::json::Json;
use platinum::preset::{Capture, Filter, FormFactor, Loudness, Preset, Render, SPEAKERS};

/// Which preset a channel uses, and the channels' order: shared with the daemon, which reads
/// state.json too (`platinum::presetfiles`).
pub use platinum::presetfiles::{PresetRef, ROLES};

/// A field an effect card changes. The numbers are what app.slint sends (its `Field` global
/// repeats them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    /// `globalEnableState`, Platinum's per-channel "Effets" switch (no control in GG).
    GlobalEnable = 1,
    Equalizer = 2,
    Bass = 10,
    Voice = 11,
    Treble = 12,
    Spatial = 13,
    /// 1 for speakers, 0 for headphones.
    Speakers = 14,
    /// Slider position 0..1, from Performance (-12 dB) to Immersion (0 dB).
    Reverb = 15,
    /// Slider position 0..1, shown in percent: 1 is the nearest (0 dB on every speaker).
    Distance = 16,
    VolumeBoost = 17,
    SmartVolume = 18,
    SmartLevel = 19,
    /// 0 extreme, 1 balanced, 2 soft.
    Loudness = 20,
    Ai = 30,
    /// Slider position 0..1 on GG's decibel-shaped scale.
    AiAmount = 31,
    EchoCanceling = 32,
    NoiseReduction = 33,
    NoiseReductionAmount = 34,
    Impact = 35,
    ImpactAmount = 36,
    Gate = 37,
    GateThreshold = 38,
    AutoGate = 39,
    Compressor = 40,
    CompressorLevel = 41,
}

impl Field {
    const ALL: [Field; 25] = [
        Field::GlobalEnable,
        Field::Equalizer,
        Field::Bass,
        Field::Voice,
        Field::Treble,
        Field::Spatial,
        Field::Speakers,
        Field::Reverb,
        Field::Distance,
        Field::VolumeBoost,
        Field::SmartVolume,
        Field::SmartLevel,
        Field::Loudness,
        Field::Ai,
        Field::AiAmount,
        Field::EchoCanceling,
        Field::NoiseReduction,
        Field::NoiseReductionAmount,
        Field::Impact,
        Field::ImpactAmount,
        Field::Gate,
        Field::GateThreshold,
        Field::AutoGate,
        Field::Compressor,
        Field::CompressorLevel,
    ];

    pub fn from_code(code: i32) -> Option<Field> {
        Field::ALL.into_iter().find(|f| *f as i32 == code)
    }
}

/// One change made from the window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Edit {
    /// A switch (0 or 1) or an amount, in the control's own scale (see [`Field`]).
    Set(Field, f64),
    /// A change to one equalizer filter, by its place (from 0).
    Band(usize, BandEdit),
    /// A speaker of the spatial audio wheel, by its place in `preset::SPEAKERS`, to an angle in
    /// degrees (positive to the left).
    Speaker(usize, f64),
    /// GG's card "Réinitialiser": the card's amounts copied from `reference` (the chosen preset's
    /// own settings), its switches kept as they are.
    ResetCard(CardKind, Preset),
}

/// An effect card, for its "Réinitialiser". The numbers are what app.slint sends (its `CardKind`
/// global repeats them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardKind {
    Equalizer = 1,
    Spatial = 2,
    VolumeBoost = 3,
    SmartVolume = 4,
    Ai = 5,
    NoiseReduction = 6,
    NoiseGate = 7,
    Compressor = 8,
}

impl CardKind {
    const ALL: [CardKind; 8] = [CardKind::Equalizer, CardKind::Spatial, CardKind::VolumeBoost, CardKind::SmartVolume, CardKind::Ai, CardKind::NoiseReduction, CardKind::NoiseGate, CardKind::Compressor];

    pub fn from_code(code: i32) -> Option<CardKind> {
        CardKind::ALL.into_iter().find(|c| *c as i32 == code)
    }
}

/// What the equalizer's graph and band editor change in one filter (GG's `ParametricEqualizer` and
/// `BandMarkerTooltip`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BandEdit {
    /// Dragged on the graph: frequency in Hz and gain in dB. Moving a filter that was off turns it
    /// on.
    Move(f64, f64),
    /// Turned on by the graph's "+", at a frequency and gain. Unlike GG, a filter whose type does
    /// not use its gain (a cut, as the module's own unused filters are) becomes a bell: the "+"
    /// shows a point to lift or lower, not a cut of everything below or above it.
    Add(f64, f64),
    /// Typed in the band editor; the filter stays on or off as it was, as in GG.
    Frequency(f64),
    Gain(f64),
    /// From the side handles, the wheel or the editor.
    Q(f64),
    /// One of the six types GG's menu offers ([`FILTER_TYPES`]); any other is refused.
    Kind(Kind),
    /// The editor's switch; the right-click "Supprimer" turns the filter off.
    Enabled(bool),
    /// The right-click "Réinitialiser": the filter as the chosen preset has it.
    Reset(Filter),
}

/// The filter types GG's band editor offers, in its order (`N5` in GG's renderer), with French
/// names: GG's own French table leaves them in English capitals ("PEAKINGEQ"). The module knows
/// four more (bypass, two band-passes, all-pass) that only a preset can bring.
pub const FILTER_TYPES: [(Kind, &str); 6] = [
    (Kind::LowPass, mark("Passe-bas")),
    (Kind::HighPass, mark("Passe-haut")),
    (Kind::Notch, mark("Coupe-bande")),
    (Kind::Peaking, mark("Égaliseur en cloche")),
    (Kind::LowShelf, mark("Plateau grave")),
    (Kind::HighShelf, mark("Plateau aigu")),
];

/// The name of any filter type in the current language, for one the menu does not offer too.
pub fn filter_type_title(kind: Kind) -> &'static str {
    match FILTER_TYPES.iter().find(|(k, _)| *k == kind) {
        Some((_, name)) => tr(name),
        None => match kind {
            Kind::ByPass => tr("Sans effet"),
            Kind::BandPassQ | Kind::BandPass => tr("Passe-bande"),
            _ => tr("Passe-tout"),
        },
    }
}

/// The ranges of GG's equalizer controls: frequency (Hz), gain (dB) and Q. The module accepts
/// gains up to ±36 dB, but GG's graph and fields never send more than ±12.
pub const BAND_FREQUENCY: (f64, f64) = (20.0, 20_000.0);
pub const BAND_GAIN: (f64, f64) = (-12.0, 12.0);
pub const BAND_Q: (f64, f64) = (0.5, 10.0);

/// The value a band editor field is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BandValue {
    Gain = 0,
    Frequency = 1,
    Q = 2,
}

impl BandValue {
    pub fn from_code(code: i32) -> Option<BandValue> {
        [BandValue::Gain, BandValue::Frequency, BandValue::Q].into_iter().find(|v| *v as i32 == code)
    }

    /// What was typed in the field, as the edit it asks for; `None` when it is not a number.
    /// Typing as `read_number` reads it: a decimal comma or point, spaces, and the units ("dB",
    /// "Hz", "kHz", "1,2k").
    pub fn parse(self, text: &str) -> Option<BandEdit> {
        let value = read_number(text, self == BandValue::Frequency)?;
        Some(match self {
            BandValue::Gain => BandEdit::Gain(value),
            BandValue::Frequency => BandEdit::Frequency(value),
            BandValue::Q => BandEdit::Q(value),
        })
    }
}

/// A number as people type it, whatever their language: a decimal comma or point, spaces (thousands
/// separators included), a leading "+" or a typographic minus, and a unit after it ("%", "dB",
/// "Hz"; "k" for thousands when `kilo`, as in "1,2 kHz"). `None` when it is not a number.
pub fn read_number(text: &str, kilo: bool) -> Option<f64> {
    let mut text = text.trim().to_lowercase().replace(',', ".").replace('\u{2212}', "-");
    text.retain(|c| !c.is_whitespace());
    for unit in ["db", "hz", "%"] {
        if let Some(rest) = text.strip_suffix(unit) {
            text = rest.to_string();
        }
    }
    let mut scale = 1.0;
    if kilo {
        if let Some(rest) = text.strip_suffix('k') {
            text = rest.to_string();
            scale = 1000.0;
        }
    }
    Some(text.parse::<f64>().ok().filter(|v| v.is_finite())? * scale)
}

/// A value typed under a knob or a fader (the window's ValueEdit), as the control takes it: read
/// by `read_number`, as a percentage when `percent` ("40" is 0.40 on a 0..1 control), rounded to
/// `step` and kept within `minimum..=maximum`, so that it lands where a drag could have put it.
/// `None` when it is not a number: the control keeps its value.
pub fn typed_value(text: &str, minimum: f64, maximum: f64, step: f64, percent: bool) -> Option<f64> {
    let value = read_number(text, false)?;
    let value = if percent { value / 100.0 } else { value };
    let stepped = if step > 0.0 { round_to(value, step) } else { value };
    Some(stepped.clamp(minimum.min(maximum), minimum.max(maximum)))
}

/// The first filter that is off, which the graph's "+" turns on (GG: `findIndex(e => !e.enabled)`).
pub fn first_free_band(preset: &Preset) -> Option<usize> {
    preset.equalizer().filters.iter().position(|f| !f.enabled)
}

/// GG's angle limits for each speaker of the wheel, in `SPEAKERS` order (`G8` in GG's renderer);
/// the sub-woofer is not on the wheel.
const SPEAKER_LIMITS: [(f64, f64); 8] = [(15.0, 60.0), (-60.0, -15.0), (-15.0, 15.0), (-180.0, 180.0), (120.0, 180.0), (-180.0, -120.0), (60.0, 120.0), (-120.0, -60.0)];
/// Where the speakers stand in a standard 7.1 layout, for a wheel reset without a better default.
pub const STANDARD_ANGLES: [f64; 8] = [30.0, -30.0, 0.0, 0.0, 150.0, -150.0, 90.0, -90.0];
/// The speakers drawn on the wheel, with GG's French names, by their place in `SPEAKERS`.
pub const WHEEL: [(usize, &str); 7] = [(2, mark("Centre")), (0, mark("Avant gauche")), (1, mark("Avant droit")), (6, mark("Latéral gauche")), (7, mark("Latéral droit")), (4, mark("Arrière gauche")), (5, mark("Arrière droit"))];

/// GG's limits for a speaker's angle.
pub fn speaker_limits(index: usize) -> (f64, f64) {
    SPEAKER_LIMITS.get(index).copied().unwrap_or((-180.0, 180.0))
}

/// An angle brought within a speaker's limits. The pointer's angle is read on the side nearest
/// to the speaker's sector first, so that a rear speaker dragged past the back (180° to -180°)
/// stops at the back instead of jumping to the other end of its sector.
pub fn clamp_angle(index: usize, angle: f64) -> f64 {
    let (low, high) = speaker_limits(index);
    let middle = (low + high) / 2.0;
    let nearest = [angle - 360.0, angle, angle + 360.0].into_iter().min_by(|a, b| (a - middle).abs().total_cmp(&(b - middle).abs())).unwrap_or(angle);
    nearest.clamp(low, high).round()
}

/// GG's AI noise cancelling slider: the mix factor v (0..1) is shown at 10^(3v − 3) of the
/// slider's length, a decibel scale from -60 dB to 0 dB (`hne`, `yne` in GG's renderer).
pub fn ai_position(value: f64) -> f64 {
    10f64.powf(3.0 * value.clamp(0.0, 1.0) - 3.0)
}

/// The mix factor for a position on that slider, to GG's step of 0.0001.
pub fn ai_value(position: f64) -> f64 {
    round_to(1.0 + position.clamp(0.001, 1.0).log10() / 3.0, 1e-4).clamp(0.0, 1.0)
}

/// Performance (-12 dB) to Immersion (0 dB), as a slider position.
pub fn reverb_position(db: f64) -> f64 {
    ((db + 12.0) / 12.0).clamp(0.0, 1.0)
}

/// GG's distance slider: one gain of 0 to 12 dB on every speaker, shown as 100 % (0 dB) down to
/// 0 % (12 dB): `setChannelsDistance` in GG's renderer.
pub fn distance_position(render: &Render) -> f64 {
    (1.0 - render.speakers[0].gain / 12.0).clamp(0.0, 1.0)
}

pub fn loudness_index(loudness: Loudness) -> i32 {
    match loudness {
        Loudness::Extreme => 0,
        Loudness::Balanced => 1,
        Loudness::Soft => 2,
    }
}

/// `value` to the nearest multiple of `step` (0.1, 0.01…). Scaling by the inverse of the step and
/// dividing back, rather than multiplying by the step, gives the double nearest to the decimal
/// (0.3, not 0.30000000000000004), which is what state.json and preset files then hold.
fn round_to(value: f64, step: f64) -> f64 {
    let scale = 1.0 / step;
    (value * scale).round() / scale
}

/// A decibel amount from a control, to 0.1 dB within GG's range.
fn decibels(value: f64, low: f64, high: f64) -> f64 {
    round_to(value.clamp(low, high), 0.1)
}

/// A level from a control, 0..1 to the hundredth.
fn level(value: f64) -> f64 {
    round_to(value.clamp(0.0, 1.0), 0.01)
}

fn on(value: f64) -> bool {
    value >= 0.5
}

/// Applies an edit, with GG's rules; returns whether the preset changed.
pub fn apply(preset: &mut Preset, edit: Edit) -> bool {
    let before = *preset;
    match edit {
        Edit::Set(Field::GlobalEnable, value) => match preset {
            Preset::Render(r) => r.global_enable = on(value),
            Preset::Capture(c) => c.global_enable = on(value),
        },
        Edit::Set(Field::Equalizer, value) => {
            preset.equalizer_mut().enabled = on(value);
            // The boosts have no switch of their own in GG: theirs follow the equalizer's (B9).
            if let Preset::Render(r) = preset {
                for boost in [&mut r.bass_boost, &mut r.voice_clarity, &mut r.treble_boost] {
                    boost.enabled = on(value);
                }
            }
        }
        Edit::Set(field, value) => match preset {
            Preset::Render(r) => render_field(r, field, value),
            Preset::Capture(c) => capture_field(c, field, value),
        },
        Edit::Band(index, change) => {
            if let Some(filter) = preset.equalizer_mut().filters.get_mut(index) {
                band_edit(filter, change);
            }
        }
        Edit::Speaker(index, angle) => {
            if let (Preset::Render(r), Some(_)) = (&mut *preset, SPEAKERS.get(index)) {
                r.speakers[index].position = clamp_angle(index, angle);
            }
        }
        Edit::ResetCard(card, reference) => reset_card(preset, card, &reference),
    }
    *preset != before
}

/// Copies one card's amounts from `reference`, as GG's card reset copies its `resetSettingNames`
/// from the preset's `defaultData` and puts the card's switch back as it was. The boosts' switches
/// follow the equalizer's, so they stay too.
fn reset_card(preset: &mut Preset, card: CardKind, reference: &Preset) {
    match (preset, reference) {
        (Preset::Render(r), Preset::Render(from)) => match card {
            CardKind::Equalizer => {
                r.equalizer.filters = from.equalizer.filters;
                r.bass_boost.value = from.bass_boost.value;
                r.voice_clarity.value = from.voice_clarity.value;
                r.treble_boost.value = from.treble_boost.value;
            }
            CardKind::Spatial => {
                r.speakers = from.speakers;
                r.reverb_gain_db = from.reverb_gain_db;
                r.form_factor = from.form_factor;
            }
            CardKind::VolumeBoost => r.general_gain = from.general_gain,
            CardKind::SmartVolume => {
                r.smart_volume.value = from.smart_volume.value;
                r.loudness = from.loudness;
            }
            _ => {}
        },
        (Preset::Capture(c), Preset::Capture(from)) => match card {
            CardKind::Equalizer => c.equalizer.filters = from.equalizer.filters,
            CardKind::Ai => c.noise_canceling.value = from.noise_canceling.value,
            CardKind::NoiseReduction => {
                c.noise_reduction.value = from.noise_reduction.value;
                c.impact_noise_reduction.value = from.impact_noise_reduction.value;
            }
            CardKind::NoiseGate => c.noise_gate.value = from.noise_gate.value,
            CardKind::Compressor => c.volume_stabilizer.value = from.volume_stabilizer.value,
            _ => {}
        },
        _ => {}
    }
}

/// GG's Arena 7 rule (`MakeCiaran7Compliant` and its window's `Kee`, B7 in
/// `analysis/sonar/gg-ui-parity.md`), on a channel that plays to Arena 7 speakers: a preset applied
/// whole (`whole`: chosen, reset, imported) with spatial audio on goes to speaker mode, as GG does
/// when it applies one; an edit that switches spatial audio on does too. GG does not lock the
/// choice: headphones picked afterwards stay, until the next preset or output change. Returns
/// whether it changed `next`.
pub fn arena_rule(before: &Preset, next: &mut Preset, arena: bool, whole: bool) -> bool {
    if !arena {
        return false;
    }
    if whole {
        return next.arena_speakers();
    }
    match (before, next) {
        (Preset::Render(old), Preset::Render(new)) if !old.virtual_surround && new.virtual_surround && new.form_factor != FormFactor::Speakers => {
            new.form_factor = FormFactor::Speakers;
            true
        }
        _ => false,
    }
}

/// The classic and impact noise reductions' switches as the module holds them once `c` is
/// written: off while AI noise cancelling is on (B1).
pub fn expected_reductions(c: &Capture) -> (bool, bool) {
    let ai = c.noise_canceling.enabled;
    (c.noise_reduction.enabled && !ai, c.impact_noise_reduction.enabled && !ai)
}

/// One filter changed from the graph or the band editor, within GG's ranges and to its steps
/// (0.1 Hz, 0.1 dB, Q to the thousandth, as its editor shows it).
fn band_edit(filter: &mut Filter, change: BandEdit) {
    let frequency = |f: f64| round_to(f.clamp(BAND_FREQUENCY.0, BAND_FREQUENCY.1), 0.1);
    match change {
        BandEdit::Move(f, gain) => {
            filter.frequency = frequency(f);
            filter.gain = decibels(gain, BAND_GAIN.0, BAND_GAIN.1);
            filter.enabled = true;
        }
        BandEdit::Add(f, gain) => {
            band_edit(filter, BandEdit::Move(f, gain));
            if !filter.kind.uses_gain() {
                filter.kind = Kind::Peaking;
            }
        }
        BandEdit::Frequency(f) => filter.frequency = frequency(f),
        BandEdit::Gain(gain) => filter.gain = decibels(gain, BAND_GAIN.0, BAND_GAIN.1),
        BandEdit::Q(q) => filter.q_factor = round_to(q.clamp(BAND_Q.0, BAND_Q.1), 0.001),
        BandEdit::Kind(kind) => {
            if FILTER_TYPES.iter().any(|(k, _)| *k == kind) {
                filter.kind = kind;
            }
        }
        BandEdit::Enabled(on) => filter.enabled = on,
        BandEdit::Reset(original) => *filter = original,
    }
}

fn render_field(r: &mut Render, field: Field, value: f64) {
    match field {
        // Writing a boost also writes its switch as the equalizer's (GG's updateBoostEffectValue).
        Field::Bass => r.bass_boost = boost(value, r.equalizer.enabled),
        Field::Voice => r.voice_clarity = boost(value, r.equalizer.enabled),
        Field::Treble => r.treble_boost = boost(value, r.equalizer.enabled),
        Field::Spatial => r.virtual_surround = on(value),
        Field::Speakers => r.form_factor = if on(value) { FormFactor::Speakers } else { FormFactor::Headphones },
        Field::Reverb => r.reverb_gain_db = decibels(-12.0 + 12.0 * value, -12.0, 0.0),
        Field::Distance => {
            let gain = decibels(12.0 * (1.0 - value), 0.0, 12.0);
            r.speakers.iter_mut().for_each(|speaker| speaker.gain = gain);
        }
        Field::VolumeBoost => r.general_gain = decibels(value, 0.0, 12.0),
        Field::SmartVolume => r.smart_volume.enabled = on(value),
        Field::SmartLevel => r.smart_volume.value = level(value),
        Field::Loudness => {
            r.loudness = match value.round() as i32 {
                0 => Loudness::Extreme,
                2 => Loudness::Soft,
                _ => Loudness::Balanced,
            }
        }
        _ => {}
    }
}

fn boost(value: f64, equalizer: bool) -> platinum::preset::Toggle {
    platinum::preset::Toggle { enabled: equalizer, value: decibels(value, -12.0, 12.0) }
}

fn capture_field(c: &mut Capture, field: Field, value: f64) {
    match field {
        Field::Ai => c.noise_canceling.enabled = on(value),
        Field::AiAmount => c.noise_canceling.value = ai_value(value),
        Field::EchoCanceling => c.acoustic_echo_canceling = on(value),
        Field::NoiseReduction => c.noise_reduction.enabled = on(value),
        Field::NoiseReductionAmount => c.noise_reduction.value = level(value),
        Field::Impact => c.impact_noise_reduction.enabled = on(value),
        Field::ImpactAmount => c.impact_noise_reduction.value = level(value),
        Field::Gate => c.noise_gate.enabled = on(value),
        Field::GateThreshold => c.noise_gate.value = decibels(value, -60.0, -10.0),
        Field::AutoGate => c.automatic_noise_gate.enabled = on(value),
        Field::Compressor => c.volume_stabilizer.enabled = on(value),
        Field::CompressorLevel => c.volume_stabilizer.value = level(value),
        _ => {}
    }
}

/// `data` with the channel's "Effets" switch kept as it is: presets always carry it on, and it
/// is a Platinum switch that choosing a preset should not undo.
pub fn keeping_effects(data: Preset, current: Option<&Preset>) -> Preset {
    match current {
        Some(current) => data.with_effects_of(current),
        None => data,
    }
}

/// One value of a preset, at the module's precision.
#[derive(Debug, PartialEq)]
enum Leaf {
    Bool(bool),
    Number(f32),
    Text(String),
}

/// Every value of a preset by its JSON path, numbers at the module's precision, without the
/// "Effets" switch.
fn leaves(preset: &Preset) -> Vec<(String, Leaf)> {
    fn walk(json: &Json, path: &str, out: &mut Vec<(String, Leaf)>) {
        match json {
            Json::Object(members) => {
                for (key, value) in members.iter().filter(|(key, _)| key != "globalEnableState") {
                    walk(value, &format!("{path}.{key}"), out);
                }
            }
            Json::Bool(b) => out.push((path.to_string(), Leaf::Bool(*b))),
            Json::Number(n) => out.push((path.to_string(), Leaf::Number(*n as f32))),
            Json::String(s) => out.push((path.to_string(), Leaf::Text(s.clone()))),
            Json::Null | Json::Array(_) => {}
        }
    }
    let mut out = Vec::new();
    walk(&preset.to_json(), "", &mut out);
    out
}

/// Whether two presets hold the same settings, as the module would hold them (a value that only
/// differs past 32-bit precision is the same) and leaving the "Effets" switch aside: what "modifié"
/// is measured with. Unlike comparing the writes, a noise reduction switched on while AI noise
/// cancelling holds it off counts as a change.
pub fn same_content(a: &Preset, b: &Preset) -> bool {
    leaves(a) == leaves(b)
}

/// The channel's preset after reading the module back: what the module holds, keeping from
/// `model` what the module cannot show (a noise reduction that AI noise cancelling holds off, the
/// automatic gate's unused value) and `model` itself when the module holds exactly it.
pub fn reconcile(model: &Preset, module: Preset) -> Preset {
    let mut result = module;
    if let (Preset::Capture(kept), Preset::Capture(read)) = (model, &mut result) {
        if read.noise_canceling.enabled {
            read.noise_reduction.enabled = kept.noise_reduction.enabled;
            read.impact_noise_reduction.enabled = kept.impact_noise_reduction.enabled;
        }
        read.automatic_noise_gate.value = kept.automatic_noise_gate.value;
    }
    if same_kind(model, &result) && same_content(model, &result) && effects(model) == effects(&result) { *model } else { result }
}

/// Whether two presets are of the same kind (playback or capture).
fn same_kind(a: &Preset, b: &Preset) -> bool {
    matches!((a, b), (Preset::Render(_), Preset::Render(_)) | (Preset::Capture(_), Preset::Capture(_)))
}

/// The "Effets" switch (`globalEnableState`).
pub fn effects(preset: &Preset) -> bool {
    preset.effects()
}

#[cfg(test)]
mod tests {
    use super::*;
    use platinum::preset::{Role, flat, plan};

    fn render(role: Role) -> Render {
        match flat(role) {
            Preset::Render(r) => r,
            Preset::Capture(_) => unreachable!(),
        }
    }

    fn capture(role: Role) -> Capture {
        match flat(role) {
            Preset::Capture(c) => c,
            Preset::Render(_) => unreachable!(),
        }
    }

    #[test]
    fn field_codes_are_stable() {
        for field in Field::ALL {
            assert_eq!(Field::from_code(field as i32), Some(field));
        }
        assert_eq!(Field::from_code(99), None);
        assert_eq!(Field::Distance as i32, 16);
        assert_eq!(Field::CompressorLevel as i32, 41);
    }

    #[test]
    fn boosts_follow_the_equalizer_switch() {
        let mut preset = flat(Role::Game);
        assert!(apply(&mut preset, Edit::Set(Field::Bass, 4.04)));
        let Preset::Render(r) = preset else { panic!() };
        // The equalizer is on in the neutral preset: the boost goes on with its value.
        assert_eq!((r.bass_boost.enabled, r.bass_boost.value), (true, 4.0));
        assert!(apply(&mut preset, Edit::Set(Field::Equalizer, 0.0)));
        let Preset::Render(r) = preset else { panic!() };
        assert!(!r.equalizer.enabled && !r.bass_boost.enabled && !r.voice_clarity.enabled && !r.treble_boost.enabled);
        // A boost moved while the equalizer is off stays off, as in GG.
        apply(&mut preset, Edit::Set(Field::Treble, 30.0));
        let Preset::Render(r) = preset else { panic!() };
        assert_eq!((r.treble_boost.enabled, r.treble_boost.value), (false, 12.0));
    }

    #[test]
    fn spatial_controls_use_ggs_scales() {
        let mut preset = flat(Role::Media);
        apply(&mut preset, Edit::Set(Field::Distance, 0.25));
        apply(&mut preset, Edit::Set(Field::Reverb, 0.0));
        apply(&mut preset, Edit::Set(Field::Speakers, 1.0));
        apply(&mut preset, Edit::Set(Field::Spatial, 1.0));
        let Preset::Render(r) = preset else { panic!() };
        assert!(r.speakers.iter().all(|s| s.gain == 9.0), "{:?}", r.speakers);
        assert_eq!(r.reverb_gain_db, -12.0);
        assert_eq!(r.form_factor, FormFactor::Speakers);
        assert!((distance_position(&r) - 0.25).abs() < 1e-9);
        // Spatial audio on writes both of its settings.
        let writes = plan(Some(&flat(Role::Media)), &preset, Role::Media).unwrap();
        assert!(writes.iter().any(|w| w.id == 79) && writes.iter().any(|w| w.id == 203) && writes.iter().any(|w| w.id == 209));
        assert_eq!(reverb_position(-6.0), 0.5);
    }

    #[test]
    fn speakers_stay_within_ggs_limits() {
        assert_eq!(clamp_angle(0, 80.0), 60.0);
        assert_eq!(clamp_angle(2, -40.0), -15.0);
        // Rear left dragged past the back stops at the back, rear right likewise.
        assert_eq!(clamp_angle(4, -170.0), 180.0);
        assert_eq!(clamp_angle(5, 170.0), -180.0);
        assert_eq!(clamp_angle(5, -140.4), -140.0);
        let mut preset = flat(Role::Aux);
        apply(&mut preset, Edit::Speaker(6, 200.0));
        let Preset::Render(r) = preset else { panic!() };
        assert_eq!(r.speakers[6].position, 120.0);
        assert!(!apply(&mut flat(Role::ChatRender), Edit::Speaker(0, 40.0)), "capture presets have no speakers");
    }

    #[test]
    fn the_ai_slider_uses_ggs_decibel_scale() {
        assert!((ai_position(0.9) - 10f64.powf(-0.3)).abs() < 1e-12);
        assert!((ai_position(1.0) - 1.0).abs() < 1e-12);
        for value in [0.0, 0.3, 0.5, 0.9, 0.926, 1.0] {
            assert!((ai_value(ai_position(value)) - value).abs() < 1e-4, "{value}");
        }
        assert_eq!(ai_value(0.0), 0.0);
        let mut preset = flat(Role::ChatCapture);
        apply(&mut preset, Edit::Set(Field::AiAmount, 0.5));
        let Preset::Capture(c) = preset else { panic!() };
        assert!((c.noise_canceling.value - 0.8997).abs() < 1e-9, "{}", c.noise_canceling.value);
    }

    #[test]
    fn capture_edits_keep_ggs_rules_in_the_writes() {
        let before = flat(Role::ChatCapture);
        let mut after = before;
        // AI is on in the neutral microphone preset: the noise reduction stays off in the module.
        apply(&mut after, Edit::Set(Field::NoiseReduction, 1.0));
        assert!(plan(Some(&before), &after, Role::ChatCapture).unwrap().is_empty());
        assert!(!same_content(&before, &after), "the preset itself changed");
        // Turning AI off now writes the noise reduction on.
        let mut off = after;
        apply(&mut off, Edit::Set(Field::Ai, 0.0));
        let ids: Vec<i32> = plan(Some(&after), &off, Role::ChatCapture).unwrap().iter().map(|w| w.id).collect();
        assert_eq!(ids, [63, 185]);
        apply(&mut off, Edit::Set(Field::GateThreshold, -70.0));
        let Preset::Capture(c) = off else { panic!() };
        assert_eq!(c.noise_gate.value, -60.0);
    }

    #[test]
    fn a_dragged_band_turns_on_and_stays_on_the_graph() {
        let mut preset = flat(Role::Game);
        assert!(apply(&mut preset, Edit::Band(7, BandEdit::Move(25_000.0, 20.0))));
        let filter = preset.equalizer().filters[7];
        assert_eq!((filter.enabled, filter.frequency, filter.gain), (true, 20_000.0, 12.0));
        assert!(!apply(&mut preset, Edit::Band(12, BandEdit::Move(100.0, 0.0))));
    }

    #[test]
    fn the_band_editor_keeps_ggs_ranges_and_types() {
        let mut preset = flat(Role::ChatCapture);
        // Typed values leave the filter's switch alone.
        apply(&mut preset, Edit::Band(2, BandEdit::Enabled(false)));
        apply(&mut preset, Edit::Band(2, BandEdit::Gain(-30.0)));
        apply(&mut preset, Edit::Band(2, BandEdit::Frequency(12.0)));
        apply(&mut preset, Edit::Band(2, BandEdit::Q(0.70714)));
        let filter = preset.equalizer().filters[2];
        assert_eq!((filter.enabled, filter.gain, filter.frequency, filter.q_factor), (false, -12.0, 20.0, 0.707));
        apply(&mut preset, Edit::Band(2, BandEdit::Q(40.0)));
        assert_eq!(preset.equalizer().filters[2].q_factor, 10.0);
        // Only GG's six types can be chosen.
        assert!(apply(&mut preset, Edit::Band(2, BandEdit::Kind(Kind::HighPass))));
        assert!(!apply(&mut preset, Edit::Band(2, BandEdit::Kind(Kind::AllPass))));
        assert_eq!(preset.equalizer().filters[2].kind, Kind::HighPass);
        // Reset puts the whole filter back.
        let original = flat(Role::ChatCapture).equalizer().filters[2];
        assert!(apply(&mut preset, Edit::Band(2, BandEdit::Reset(original))));
        assert_eq!(preset.equalizer().filters[2], original);
        assert_eq!(FILTER_TYPES.map(|(k, _)| k.code()), [1, 2, 5, 7, 8, 9]);
        assert_eq!(filter_type_title(Kind::BandPass), "Passe-bande");
    }

    #[test]
    fn values_typed_under_a_knob_or_a_fader_land_on_the_control() {
        // A percentage knob or fader (0..1, 1 % steps): with or without its unit.
        for text in ["40", "40 %", "40%", " 40,0 ", "+40"] {
            assert_eq!(typed_value(text, 0.0, 1.0, 0.01, true), Some(0.4), "{text}");
        }
        assert_eq!(typed_value("150", 0.0, 1.0, 0.01, true), Some(1.0));
        assert_eq!(typed_value("-5 %", 0.0, 1.0, 0.01, true), Some(0.0));
        assert_eq!(typed_value("33,333", 0.0, 1.0, 0.01, true), Some(0.33));
        // A decibel knob (-12..12, 0.1 dB steps): comma or point, unit or not, a typographic minus.
        for text in ["-3,5", "-3.5", "-3.5 dB", "-3,5dB", "\u{2212}3,5 dB", "-3,47"] {
            assert_eq!(typed_value(text, -12.0, 12.0, 0.1, false), Some(-3.5), "{text}");
        }
        assert_eq!(typed_value("+4", -12.0, 12.0, 0.1, false), Some(4.0));
        assert_eq!(typed_value("20", -12.0, 12.0, 0.1, false), Some(12.0));
        assert_eq!(typed_value("0,04", -12.0, 12.0, 0.1, false), Some(0.0));
        // The noise gate's threshold, -60..-10 dB.
        assert_eq!(typed_value("-5", -60.0, -10.0, 0.1, false), Some(-10.0));
        assert_eq!(typed_value("-72 dB", -60.0, -10.0, 0.1, false), Some(-60.0));
        // Not a number: nothing is written.
        for text in ["", "abc", "dB", "4,5,6", "inf", "NaN", "1e999"] {
            assert_eq!(typed_value(text, -12.0, 12.0, 0.1, false), None, "{text}");
        }
        assert_eq!(read_number("1,25 kHz", true), Some(1250.0));
        assert_eq!(read_number("1,25 kHz", false), None);
    }

    #[test]
    fn typed_values_accept_french_writing() {
        let parse = |value: BandValue, text: &str| value.parse(text);
        assert_eq!(parse(BandValue::Gain, " +3,5 dB "), Some(BandEdit::Gain(3.5)));
        assert_eq!(parse(BandValue::Gain, "-4"), Some(BandEdit::Gain(-4.0)));
        assert_eq!(parse(BandValue::Frequency, "1,25 kHz"), Some(BandEdit::Frequency(1250.0)));
        assert_eq!(parse(BandValue::Frequency, "2k"), Some(BandEdit::Frequency(2000.0)));
        assert_eq!(parse(BandValue::Frequency, "1 000 Hz"), Some(BandEdit::Frequency(1000.0)));
        assert_eq!(parse(BandValue::Q, "0.707"), Some(BandEdit::Q(0.707)));
        assert_eq!(parse(BandValue::Q, "large"), None);
        assert_eq!(parse(BandValue::Q, ""), None);
        assert_eq!(parse(BandValue::Gain, "inf"), None);
        assert_eq!(BandValue::from_code(1), Some(BandValue::Frequency));
        assert_eq!(BandValue::from_code(3), None);
    }

    #[test]
    fn the_plus_turns_on_the_first_band_that_is_off() {
        // The neutral playback preset has its last five filters off.
        let mut preset = flat(Role::Media);
        assert_eq!(first_free_band(&preset), Some(5));
        // A free filter left as a cut (the module's default last filter is a low-pass) comes back
        // as a bell; a shelf stays a shelf.
        preset.equalizer_mut().filters[5].kind = Kind::LowPass;
        preset.equalizer_mut().filters[6].kind = Kind::HighShelf;
        apply(&mut preset, Edit::Band(5, BandEdit::Add(3000.0, 4.0)));
        apply(&mut preset, Edit::Band(6, BandEdit::Add(9000.0, -2.0)));
        let [added, shelf] = [preset.equalizer().filters[5], preset.equalizer().filters[6]];
        assert_eq!((added.enabled, added.kind, added.frequency, added.gain), (true, Kind::Peaking, 3000.0, 4.0));
        assert_eq!((shelf.enabled, shelf.kind), (true, Kind::HighShelf));
        for index in 0..10 {
            apply(&mut preset, Edit::Band(index, BandEdit::Enabled(true)));
        }
        assert_eq!(first_free_band(&preset), None);
        // Values rounded to their step stay short decimals in the files.
        apply(&mut preset, Edit::Band(0, BandEdit::Move(1234.5678, 0.3333)));
        let filter = preset.equalizer().filters[0];
        assert_eq!((filter.frequency.to_string(), filter.gain.to_string()), ("1234.6".to_string(), "0.3".to_string()));
    }

    #[test]
    fn a_card_reset_copies_its_amounts_and_keeps_its_switch() {
        let reference = flat(Role::Game);
        let mut preset = reference;
        apply(&mut preset, Edit::Set(Field::Bass, 6.0));
        apply(&mut preset, Edit::Band(0, BandEdit::Move(500.0, 4.0)));
        apply(&mut preset, Edit::Set(Field::Spatial, 1.0));
        apply(&mut preset, Edit::Set(Field::Reverb, 0.0));
        assert!(apply(&mut preset, Edit::ResetCard(CardKind::Equalizer, reference)));
        let Preset::Render(r) = preset else { panic!() };
        // The filters and the boost's amount come back; the boost's switch and spatial audio stay.
        assert_eq!(r.equalizer.filters, render(Role::Game).equalizer.filters);
        assert_eq!((r.bass_boost.value, r.bass_boost.enabled), (0.0, true));
        assert!(r.virtual_surround && r.reverb_gain_db == -12.0);
        // A playback card on a capture preset changes nothing.
        assert!(!apply(&mut flat(Role::ChatCapture), Edit::ResetCard(CardKind::Spatial, reference)));
        assert_eq!(CardKind::from_code(8), Some(CardKind::Compressor));
        assert_eq!(CardKind::from_code(0), None);
    }

    #[test]
    fn spatial_audio_switched_on_with_arena_speakers_goes_to_speaker_mode() {
        let before = flat(Role::Game);
        let mut next = before;
        apply(&mut next, Edit::Set(Field::Spatial, 1.0));
        let mut elsewhere = next;
        assert!(arena_rule(&before, &mut next, true, false));
        assert!(!arena_rule(&before, &mut elsewhere, false, false));
        let (Preset::Render(arena), Preset::Render(other)) = (next, elsewhere) else { panic!() };
        assert_eq!((arena.form_factor, other.form_factor), (FormFactor::Speakers, FormFactor::Headphones));
        // Already on: headphones picked afterwards stay, as in GG.
        let mut headphones = next;
        apply(&mut headphones, Edit::Set(Field::Speakers, 0.0));
        let picked = headphones;
        assert!(!arena_rule(&next, &mut headphones, true, false));
        assert_eq!(headphones, picked);
        // A preset applied whole with spatial audio on in headphone mode goes to speakers, as GG's
        // MakeCiaran7Compliant; one without spatial audio is left alone.
        let mut chosen = picked;
        assert!(arena_rule(&next, &mut chosen, true, true));
        let Preset::Render(chosen) = chosen else { panic!() };
        assert_eq!(chosen.form_factor, FormFactor::Speakers);
        let mut plain = flat(Role::Aux);
        assert!(!arena_rule(&next, &mut plain, true, true));
        assert!(!arena_rule(&next, &mut Preset::Render(chosen), false, true));
    }

    #[test]
    fn choosing_a_preset_keeps_the_effects_switch() {
        let mut current = flat(Role::Aux);
        apply(&mut current, Edit::Set(Field::GlobalEnable, 0.0));
        let chosen = keeping_effects(flat(Role::Aux), Some(&current));
        assert!(!effects(&chosen));
        assert!(effects(&keeping_effects(flat(Role::Aux), None)));
        // The switch is not part of what "modifié" measures.
        assert!(same_content(&chosen, &flat(Role::Aux)));
    }

    #[test]
    fn reading_the_module_back_keeps_what_it_cannot_show() {
        let mut model = capture(Role::ChatCapture);
        model.noise_reduction.enabled = true;
        model.automatic_noise_gate.value = 7.0;
        // The module holds the noise reduction off while AI is on, and no gate value.
        let mut module = model;
        module.noise_reduction.enabled = false;
        module.automatic_noise_gate.value = 0.0;
        assert_eq!(reconcile(&Preset::Capture(model), Preset::Capture(module)), Preset::Capture(model));
        // A value changed elsewhere comes in.
        module.volume_stabilizer.value = 0.4000000059604645;
        let Preset::Capture(read) = reconcile(&Preset::Capture(model), Preset::Capture(module)) else { panic!() };
        assert_eq!(read.volume_stabilizer.value, 0.4000000059604645);
        assert!(read.noise_reduction.enabled);
        // Past 32-bit precision, the model is kept as it is.
        let mut close = render(Role::Game);
        close.reverb_gain_db = -6.0000001;
        assert_eq!(reconcile(&flat(Role::Game), Preset::Render(close)), flat(Role::Game));
        // The effects switch read back counts.
        let mut off = render(Role::Game);
        off.global_enable = false;
        assert_eq!(reconcile(&flat(Role::Game), Preset::Render(off)), Preset::Render(off));
    }
}
