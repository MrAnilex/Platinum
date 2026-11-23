//! Sonar presets: the data model of GG's preset JSON ("data" block), Platinum's own neutral preset,
//! and how a preset becomes setting writes in the audio module, exactly as GG applies it.
//!
//! Field names are GG's, so a preset read from GG's database or from a GG share file is written back
//! the same way. The mapping and the write order come from GG's service (`RenderConfigSettings.Apply`,
//! `CaptureConfigSettings.Apply`), checked against the live module: `analysis/sonar/presets.md`,
//! section 3, and `analysis/sonar/gg-ui-parity.md`, B1 to B20. There is no scaling anywhere: values
//! go into the module as they are, except three enumerations (filter type, loudness, form factor)
//! and a few rules:
//! - AI noise cancelling on forces the classic noise reduction and the impact noise reduction off in
//!   the module; the preset keeps its own values for when AI goes off again (B1);
//! - the automatic noise gate offset is always written as 3 dB, whatever the preset holds (B8);
//! - the Smart Volume loudness sets both compressor thresholds (B4);
//! - the spatial audio state goes to two settings (B5).
//!
//! GG also changes fields from device features. The Arena 7 one is reproduced, from the output's
//! name since Platinum cannot read GG's feature flags: spatial audio on in headphone mode goes to
//! speaker mode (`Preset::arena_speakers`, B7). The others are not (Arena 9 turns virtual surround
//! off; AEC is limited to some devices when AI noise cancelling is switched on).

use crate::apo::{self, Channel, Store};
use crate::eq::{self, Kind};
use crate::json::Json;
use std::collections::HashMap;
use std::fmt;

/// The five kinds of preset, one per channel: GG's "virtual audio device".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    Game,
    /// The Chat channel: the voices heard from Discord and the like.
    ChatRender,
    /// The microphone.
    ChatCapture,
    Media,
    Aux,
}

impl Role {
    pub const ALL: [Role; 5] = [Role::Game, Role::ChatRender, Role::ChatCapture, Role::Media, Role::Aux];

    /// GG's number for it, the `vad` column of its database.
    pub fn vad(self) -> i64 {
        match self {
            Role::Game => 1,
            Role::ChatRender => 2,
            Role::ChatCapture => 3,
            Role::Media => 4,
            Role::Aux => 5,
        }
    }

    pub fn from_vad(vad: i64) -> Option<Role> {
        Role::ALL.into_iter().find(|r| r.vad() == vad)
    }

    /// GG's name for it in its interface and API.
    pub fn api_name(self) -> &'static str {
        match self {
            Role::Game => "game",
            Role::ChatRender => "chatRender",
            Role::ChatCapture => "chatCapture",
            Role::Media => "media",
            Role::Aux => "aux",
        }
    }

    /// The channel whose store the preset is written to.
    pub fn channel(self) -> Channel {
        match self {
            Role::Game => Channel::Game,
            Role::ChatRender => Channel::Chat,
            Role::ChatCapture => Channel::Mic,
            Role::Media => Channel::Media,
            Role::Aux => Channel::Aux,
        }
    }

    pub fn from_channel(channel: Channel) -> Role {
        match channel {
            Channel::Game => Role::Game,
            Channel::Chat => Role::ChatRender,
            Channel::Mic => Role::ChatCapture,
            Channel::Media => Role::Media,
            Channel::Aux => Role::Aux,
        }
    }

    /// Chat and the microphone take capture presets (noise reduction, gate…); the others take
    /// playback presets (boosts, Smart Volume, spatial audio). Chat is an output, but its module
    /// processes it with the capture settings.
    pub fn is_capture(self) -> bool {
        matches!(self, Role::ChatRender | Role::ChatCapture)
    }
}

/// An effect with an on/off switch and one amount: GG's `{enabled, value}`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Toggle {
    pub enabled: bool,
    pub value: f64,
}

/// Smart Volume's loudness, which has no control in GG's window: presets only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Loudness {
    Extreme,
    Balanced,
    Soft,
}

impl Loudness {
    pub fn name(self) -> &'static str {
        match self {
            Loudness::Extreme => "extreme",
            Loudness::Balanced => "balanced",
            Loudness::Soft => "soft",
        }
    }

    pub fn from_name(name: &str) -> Option<Loudness> {
        [Loudness::Extreme, Loudness::Balanced, Loudness::Soft].into_iter().find(|l| l.name().eq_ignore_ascii_case(name))
    }

    /// The compressor threshold GG writes for it (`RenderSettings.LoudnessTypeToLoudnessLevel`).
    pub fn threshold_db(self) -> f64 {
        match self {
            Loudness::Extreme => -9.0,
            Loudness::Balanced => -18.0,
            Loudness::Soft => -27.0,
        }
    }

    /// GG's reverse rule (`LoudnessLevelToLoudnessType`): at most −27 dB is soft, at most −18 dB
    /// balanced, anything higher extreme.
    pub fn from_threshold_db(db: f64) -> Loudness {
        if db <= -27.0 {
            Loudness::Soft
        } else if db <= -18.0 {
            Loudness::Balanced
        } else {
            Loudness::Extreme
        }
    }
}

/// Headphones or speakers, for spatial audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormFactor {
    Headphones,
    Speakers,
}

impl FormFactor {
    pub fn name(self) -> &'static str {
        match self {
            FormFactor::Headphones => "headphones",
            FormFactor::Speakers => "speakers",
        }
    }

    pub fn from_name(name: &str) -> Option<FormFactor> {
        [FormFactor::Headphones, FormFactor::Speakers].into_iter().find(|f| f.name().eq_ignore_ascii_case(name))
    }

    /// The module's number (`kSet_FormFactor`): 0 headphones, 1 speakers.
    pub fn code(self) -> i32 {
        match self {
            FormFactor::Headphones => 0,
            FormFactor::Speakers => 1,
        }
    }

    pub fn from_code(code: i32) -> FormFactor {
        if code == 1 { FormFactor::Speakers } else { FormFactor::Headphones }
    }
}

/// One equalizer filter, with GG's field names.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Filter {
    pub enabled: bool,
    pub q_factor: f64,
    pub frequency: f64,
    pub gain: f64,
    pub kind: Kind,
}

impl Filter {
    /// The filter as the response curve wants it.
    pub fn band(&self) -> eq::Band {
        eq::Band { enabled: self.enabled, frequency: self.frequency, q: self.q_factor, gain: self.gain, kind: self.kind }
    }
}

/// GG's `parametricEQ`: an on/off switch and ten filters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Equalizer {
    pub enabled: bool,
    pub filters: [Filter; eq::BANDS],
}

impl Equalizer {
    pub fn bands(&self) -> Vec<eq::Band> {
        self.filters.iter().map(Filter::band).collect()
    }
}

/// One virtual speaker of spatial audio: its angle in degrees and its gain in dB.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Speaker {
    pub position: f64,
    pub gain: f64,
}

/// GG's names of the eight virtual speakers, in its order (also the order of `Render::speakers`).
pub const SPEAKERS: [&str; 8] = ["frontLeft", "frontRight", "center", "subWoofer", "rearLeft", "rearRight", "sideLeft", "sideRight"];

/// A playback preset (Game, Media, Aux).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Render {
    /// `globalEnableState`: every effect of the channel at once. GG has no control for it.
    pub global_enable: bool,
    pub bass_boost: Toggle,
    pub treble_boost: Toggle,
    pub voice_clarity: Toggle,
    pub smart_volume: Toggle,
    pub loudness: Loudness,
    /// `generalGain`, "Volume boost" in GG's window, in dB.
    pub general_gain: f64,
    pub equalizer: Equalizer,
    pub virtual_surround: bool,
    /// `virtualSurroundChannels`, in the order of [`SPEAKERS`].
    pub speakers: [Speaker; 8],
    /// `reverbGainDB`: GG's "Performance ↔ Immersion" slider.
    pub reverb_gain_db: f64,
    pub form_factor: FormFactor,
}

/// A capture preset (Chat, microphone).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Capture {
    pub global_enable: bool,
    pub noise_reduction: Toggle,
    /// "Compressor" in GG's window.
    pub volume_stabilizer: Toggle,
    pub noise_gate: Toggle,
    /// Only `enabled` reaches the module; GG writes the offset as 3 dB whatever `value` holds.
    pub automatic_noise_gate: Toggle,
    pub equalizer: Equalizer,
    pub impact_noise_reduction: Toggle,
    /// ClearCast AI noise cancelling; `value` is its mix factor.
    pub noise_canceling: Toggle,
    pub acoustic_echo_canceling: bool,
}

/// A preset of either kind.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Preset {
    Render(Render),
    Capture(Capture),
}

/// Name Platinum gives its own neutral preset.
pub const NEUTRAL_NAME: &str = "Neutre (Platinum)";

/// Platinum's own neutral preset for a role: generic values, not taken from any vendor preset.
/// Playback: five flat bells at 35 Hz, 120 Hz, 1 kHz, 6 kHz and 18 kHz plus five spare filters
/// switched off, every effect off, spatial audio off with the usual 7.1 speaker angles. Capture:
/// a flat ten-band octave equalizer (shelves at both ends), every effect off, the noise gate at its
/// lowest threshold; on the microphone, AI noise cancelling on.
pub fn flat(role: Role) -> Preset {
    let filter = |frequency: f64, kind: Kind, enabled: bool| Filter { enabled, q_factor: 0.7071, frequency, gain: 0.0, kind };
    if role.is_capture() {
        let mut filters = [filter(0.0, Kind::Peaking, true); eq::BANDS];
        for (slot, frequency) in filters.iter_mut().zip([31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0]) {
            slot.frequency = frequency;
        }
        filters[0].kind = Kind::LowShelf;
        filters[9].kind = Kind::HighShelf;
        let off = Toggle::default();
        return Preset::Capture(Capture {
            global_enable: true,
            noise_reduction: off,
            volume_stabilizer: off,
            noise_gate: Toggle { enabled: false, value: -60.0 },
            automatic_noise_gate: off,
            equalizer: Equalizer { enabled: true, filters },
            impact_noise_reduction: off,
            noise_canceling: Toggle { enabled: role == Role::ChatCapture, value: 0.9 },
            acoustic_echo_canceling: false,
        });
    }
    let on = [35.0, 120.0, 1000.0, 6000.0, 18000.0].map(|f| filter(f, Kind::Peaking, true));
    let spare = [60.0, 250.0, 500.0, 3000.0, 12000.0].map(|f| filter(f, Kind::Peaking, false));
    let mut filters = [filter(0.0, Kind::Peaking, false); eq::BANDS];
    filters[..5].copy_from_slice(&on);
    filters[5..].copy_from_slice(&spare);
    let angles = [30.0, -30.0, 0.0, 0.0, 150.0, -150.0, 90.0, -90.0];
    Preset::Render(Render {
        global_enable: true,
        bass_boost: Toggle::default(),
        treble_boost: Toggle::default(),
        voice_clarity: Toggle::default(),
        smart_volume: Toggle::default(),
        loudness: Loudness::Balanced,
        general_gain: 0.0,
        equalizer: Equalizer { enabled: true, filters },
        virtual_surround: false,
        speakers: angles.map(|position| Speaker { position, gain: 0.0 }),
        reverb_gain_db: -6.0,
        form_factor: FormFactor::Headphones,
    })
}

/// GG's JSON name of a filter type.
pub fn filter_type_name(kind: Kind) -> &'static str {
    match kind {
        Kind::ByPass => "byPass",
        Kind::LowPass => "lowPass",
        Kind::HighPass => "highPass",
        Kind::BandPassQ => "bandPassPeakQ",
        Kind::BandPass => "bandPassPeak0dB",
        Kind::Notch => "notchFilter",
        Kind::AllPass => "allPass",
        Kind::Peaking => "peakingEQ",
        Kind::LowShelf => "lowShelving",
        Kind::HighShelf => "highShelving",
    }
}

/// The filter type GG means by `name`, ignoring case.
pub fn filter_type_from_name(name: &str) -> Option<Kind> {
    Kind::ALL.into_iter().find(|k| filter_type_name(*k).eq_ignore_ascii_case(name))
}

impl Preset {
    /// Reads GG's "data" JSON. A missing field takes the neutral preset's value, as GG's own
    /// classes keep their defaults; a field of the wrong type is refused. Unknown fields are ignored.
    pub fn from_json(json: &Json, role: Role) -> Result<Preset, String> {
        if json.as_object().is_none() {
            return Err("a preset must be a JSON object".into());
        }
        let fields = Fields { json, path: String::new() };
        match flat(role) {
            Preset::Render(base) => fields.render(&base).map(Preset::Render),
            Preset::Capture(base) => fields.capture(&base).map(Preset::Capture),
        }
    }

    /// Parses JSON text, then reads it as [`Preset::from_json`] does.
    pub fn parse(text: &str, role: Role) -> Result<Preset, String> {
        Preset::from_json(&Json::parse(text)?, role)
    }

    /// GG's "data" JSON, with GG's field order.
    pub fn to_json(&self) -> Json {
        match self {
            Preset::Render(r) => object(vec![
                ("bassBoostState", toggle_json(r.bass_boost)),
                ("trebleBoostState", toggle_json(r.treble_boost)),
                ("voiceClarityState", toggle_json(r.voice_clarity)),
                (
                    "smartVolume",
                    object(vec![("enabled", Json::Bool(r.smart_volume.enabled)), ("volumeLevel", Json::Number(r.smart_volume.value)), ("loudness", Json::String(r.loudness.name().into()))]),
                ),
                ("generalGain", Json::Number(r.general_gain)),
                ("parametricEQ", equalizer_json(&r.equalizer)),
                ("virtualSurroundState", Json::Bool(r.virtual_surround)),
                (
                    "virtualSurroundChannels",
                    Json::Object(SPEAKERS.iter().zip(&r.speakers).map(|(name, s)| (name.to_string(), object(vec![("position", Json::Number(s.position)), ("gain", Json::Number(s.gain))]))).collect()),
                ),
                ("reverbGainDB", Json::Number(r.reverb_gain_db)),
                ("formFactor", Json::String(r.form_factor.name().into())),
                ("globalEnableState", Json::Bool(r.global_enable)),
            ]),
            Preset::Capture(c) => object(vec![
                ("noiseReductionState", toggle_json(c.noise_reduction)),
                ("volumeStabilizerState", toggle_json(c.volume_stabilizer)),
                ("noiseGateState", toggle_json(c.noise_gate)),
                ("automaticNoiseGateState", toggle_json(c.automatic_noise_gate)),
                ("parametricEQ", equalizer_json(&c.equalizer)),
                ("impactNoiseReductionState", toggle_json(c.impact_noise_reduction)),
                ("noiseCancelingState", toggle_json(c.noise_canceling)),
                ("acousticEchoCancelingState", Json::Bool(c.acoustic_echo_canceling)),
                ("globalEnableState", Json::Bool(c.global_enable)),
            ]),
        }
    }

    /// Whether this preset is of the kind `role` takes.
    pub fn fits(&self, role: Role) -> bool {
        matches!(self, Preset::Capture(_)) == role.is_capture()
    }

    /// `globalEnableState`: Platinum's per-channel "Effets" switch. Presets always carry it on;
    /// choosing a preset keeps the channel's own (`with_effects_of`).
    pub fn effects(&self) -> bool {
        match self {
            Preset::Render(r) => r.global_enable,
            Preset::Capture(c) => c.global_enable,
        }
    }

    pub fn set_effects(&mut self, on: bool) {
        match self {
            Preset::Render(r) => r.global_enable = on,
            Preset::Capture(c) => c.global_enable = on,
        }
    }

    /// This preset with the "Effets" switch of `current`, the channel's settings now: a preset
    /// chosen should not undo a switch that belongs to Platinum, not to the preset.
    pub fn with_effects_of(mut self, current: &Preset) -> Preset {
        self.set_effects(current.effects());
        self
    }

    /// GG's Arena 7 rule (`MakeCiaran7Compliant`, B7 in `analysis/sonar/gg-ui-parity.md`), for a
    /// channel that plays to Arena 7 speakers: spatial audio on in headphone mode goes to speaker
    /// mode. GG runs it when a preset is applied and when the speakers appear, and saves the result
    /// into the preset; it never goes back to headphones by itself, and headphones picked by hand
    /// afterwards stay until the next time it runs. Returns whether the preset changed.
    pub fn arena_speakers(&mut self) -> bool {
        match self {
            Preset::Render(r) if r.virtual_surround && r.form_factor == FormFactor::Headphones => {
                r.form_factor = FormFactor::Speakers;
                true
            }
            _ => false,
        }
    }

    pub fn equalizer(&self) -> &Equalizer {
        match self {
            Preset::Render(r) => &r.equalizer,
            Preset::Capture(c) => &c.equalizer,
        }
    }

    pub fn equalizer_mut(&mut self) -> &mut Equalizer {
        match self {
            Preset::Render(r) => &mut r.equalizer,
            Preset::Capture(c) => &mut c.equalizer,
        }
    }

    /// The three boosts of a playback preset, for [`eq::curve_path_with_boosts`]; none for capture.
    pub fn boosts(&self) -> eq::Boosts {
        match self {
            Preset::Render(r) => {
                let boost = |t: Toggle| eq::Boost { enabled: t.enabled, gain: t.value };
                eq::Boosts { bass: boost(r.bass_boost), voice: boost(r.voice_clarity), treble: boost(r.treble_boost) }
            }
            Preset::Capture(_) => eq::Boosts::default(),
        }
    }
}

fn object(members: Vec<(&str, Json)>) -> Json {
    Json::Object(members.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn toggle_json(t: Toggle) -> Json {
    object(vec![("enabled", Json::Bool(t.enabled)), ("value", Json::Number(t.value))])
}

fn equalizer_json(eq: &Equalizer) -> Json {
    let mut members = vec![("enabled".to_string(), Json::Bool(eq.enabled))];
    for (n, f) in eq.filters.iter().enumerate() {
        let filter = object(vec![
            ("enabled", Json::Bool(f.enabled)),
            ("qFactor", Json::Number(f.q_factor)),
            ("frequency", Json::Number(f.frequency)),
            ("gain", Json::Number(f.gain)),
            ("type", Json::String(filter_type_name(f.kind).into())),
        ]);
        members.push((format!("filter{}", n + 1), filter));
    }
    Json::Object(members)
}

/// One object of a preset being read, with its path for error messages.
struct Fields<'a> {
    json: &'a Json,
    path: String,
}

impl<'a> Fields<'a> {
    fn path(&self, key: &str) -> String {
        if self.path.is_empty() { key.to_string() } else { format!("{}.{key}", self.path) }
    }

    /// The nested object `key`, or `None` when it is missing.
    fn child(&self, key: &str) -> Result<Option<Fields<'a>>, String> {
        match self.json.get(key) {
            None | Some(Json::Null) => Ok(None),
            Some(value @ Json::Object(_)) => Ok(Some(Fields { json: value, path: self.path(key) })),
            Some(_) => Err(format!("{} must be an object", self.path(key))),
        }
    }

    fn boolean(&self, key: &str, default: bool) -> Result<bool, String> {
        match self.json.get(key) {
            None | Some(Json::Null) => Ok(default),
            Some(Json::Bool(b)) => Ok(*b),
            Some(_) => Err(format!("{} must be true or false", self.path(key))),
        }
    }

    fn number(&self, key: &str, default: f64) -> Result<f64, String> {
        match self.json.get(key) {
            None | Some(Json::Null) => Ok(default),
            Some(Json::Number(n)) => Ok(*n),
            Some(_) => Err(format!("{} must be a number", self.path(key))),
        }
    }

    /// A string field read through `parse`, which names what is accepted when it fails.
    fn named<T>(&self, key: &str, default: T, parse: impl Fn(&str) -> Option<T>, expected: &str) -> Result<T, String> {
        match self.json.get(key) {
            None | Some(Json::Null) => Ok(default),
            Some(Json::String(s)) => parse(s).ok_or_else(|| format!("{} = \"{s}\": expected {expected}", self.path(key))),
            Some(_) => Err(format!("{} must be text ({expected})", self.path(key))),
        }
    }

    fn toggle(&self, key: &str, default: Toggle) -> Result<Toggle, String> {
        match self.child(key)? {
            None => Ok(default),
            Some(t) => Ok(Toggle { enabled: t.boolean("enabled", default.enabled)?, value: t.number("value", default.value)? }),
        }
    }

    fn equalizer(&self, default: &Equalizer) -> Result<Equalizer, String> {
        let Some(eq) = self.child("parametricEQ")? else {
            return Ok(*default);
        };
        let mut filters = default.filters;
        for (n, slot) in filters.iter_mut().enumerate() {
            if let Some(f) = eq.child(&format!("filter{}", n + 1))? {
                *slot = Filter {
                    enabled: f.boolean("enabled", slot.enabled)?,
                    q_factor: f.number("qFactor", slot.q_factor)?,
                    frequency: f.number("frequency", slot.frequency)?,
                    gain: f.number("gain", slot.gain)?,
                    kind: f.filter_type(slot.kind)?,
                };
            }
        }
        Ok(Equalizer { enabled: eq.boolean("enabled", default.enabled)?, filters })
    }

    /// GG writes the type by name; the module's number (0 to 9) is accepted too.
    fn filter_type(&self, default: Kind) -> Result<Kind, String> {
        match self.json.get("type") {
            Some(Json::Number(n)) if n.fract() == 0.0 && (0.0..=9.0).contains(n) => Ok(Kind::from_code(*n as i32)),
            Some(Json::Number(n)) => Err(format!("{} = {n}: expected a filter type from 0 to 9", self.path("type"))),
            _ => self.named("type", default, filter_type_from_name, "a filter type such as peakingEQ, lowShelving or highPass"),
        }
    }

    fn render(&self, base: &Render) -> Result<Render, String> {
        let (smart_volume, loudness) = match self.child("smartVolume")? {
            None => (base.smart_volume, base.loudness),
            Some(s) => (
                Toggle { enabled: s.boolean("enabled", base.smart_volume.enabled)?, value: s.number("volumeLevel", base.smart_volume.value)? },
                s.named("loudness", base.loudness, Loudness::from_name, "extreme, balanced or soft")?,
            ),
        };
        let mut speakers = base.speakers;
        if let Some(channels) = self.child("virtualSurroundChannels")? {
            for (name, speaker) in SPEAKERS.iter().zip(speakers.iter_mut()) {
                if let Some(s) = channels.child(name)? {
                    *speaker = Speaker { position: s.number("position", speaker.position)?, gain: s.number("gain", speaker.gain)? };
                }
            }
        }
        Ok(Render {
            global_enable: self.boolean("globalEnableState", base.global_enable)?,
            bass_boost: self.toggle("bassBoostState", base.bass_boost)?,
            treble_boost: self.toggle("trebleBoostState", base.treble_boost)?,
            voice_clarity: self.toggle("voiceClarityState", base.voice_clarity)?,
            smart_volume,
            loudness,
            general_gain: self.number("generalGain", base.general_gain)?,
            equalizer: self.equalizer(&base.equalizer)?,
            virtual_surround: self.boolean("virtualSurroundState", base.virtual_surround)?,
            speakers,
            reverb_gain_db: self.number("reverbGainDB", base.reverb_gain_db)?,
            form_factor: self.named("formFactor", base.form_factor, FormFactor::from_name, "headphones or speakers")?,
        })
    }

    fn capture(&self, base: &Capture) -> Result<Capture, String> {
        Ok(Capture {
            global_enable: self.boolean("globalEnableState", base.global_enable)?,
            noise_reduction: self.toggle("noiseReductionState", base.noise_reduction)?,
            volume_stabilizer: self.toggle("volumeStabilizerState", base.volume_stabilizer)?,
            noise_gate: self.toggle("noiseGateState", base.noise_gate)?,
            automatic_noise_gate: self.toggle("automaticNoiseGateState", base.automatic_noise_gate)?,
            equalizer: self.equalizer(&base.equalizer)?,
            impact_noise_reduction: self.toggle("impactNoiseReductionState", base.impact_noise_reduction)?,
            noise_canceling: self.toggle("noiseCancelingState", base.noise_canceling)?,
            acoustic_echo_canceling: self.boolean("acousticEchoCancelingState", base.acoustic_echo_canceling)?,
        })
    }
}

/// A value to write into one setting. Numbers are `f32`, the module's own precision, so that
/// comparing with what the module holds is exact, as GG's comparison is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
    Bool(bool),
    Number(f32),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Bool(b) => write!(f, "{b}"),
            Value::Number(n) => write!(f, "{n}"),
        }
    }
}

/// One setting write: the setting's number in the module, the value, and the preset field it
/// comes from (for display).
#[derive(Clone, Debug, PartialEq)]
pub struct Write {
    pub id: i32,
    pub value: Value,
    pub field: String,
}

/// Settings of the noise gate offset, which GG writes as a constant (B8).
pub const GATE_OFFSET_ID: i32 = 58;
pub const GATE_OFFSET_DB: f32 = 3.0;

/// The settings GG's rules derive from `id`: writing it means they must hold what the rule gives
/// them too (B4: the second compressor threshold follows the first; B5: advanced surround follows
/// spatial audio; B1: AI noise cancelling holds both noise reductions off; B8: the automatic gate
/// comes with its constant offset). A module changed one setting at a time (an older window, the
/// command line) can hold them apart: a change that only writes the differences checks these too.
pub fn linked(id: i32) -> &'static [i32] {
    match id {
        17 => &[23],
        79 => &[203],
        185 => &[63, 161],
        57 => &[GATE_OFFSET_ID],
        _ => &[],
    }
}

/// Gain settings of the eight speakers, in [`SPEAKERS`] order: not in id order, rear before side.
const SPEAKER_GAIN_IDS: [i32; 8] = [195, 196, 197, 198, 201, 202, 199, 200];
/// Angle settings of the eight speakers, in [`SPEAKERS`] order.
const SPEAKER_POSITION_IDS: [i32; 8] = [187, 188, 189, 190, 191, 192, 193, 194];

/// Collects writes in order.
struct Writes(Vec<Write>);

impl Writes {
    fn flag(&mut self, id: i32, value: bool, field: impl Into<String>) {
        self.0.push(Write { id, value: Value::Bool(value), field: field.into() });
    }

    fn number(&mut self, id: i32, value: f64, field: impl Into<String>) {
        self.0.push(Write { id, value: Value::Number(value as f32), field: field.into() });
    }

    /// The equalizer: its switch, then each filter's on/off, Q, frequency, gain and type, in the
    /// order GG writes them (not the settings' order).
    fn equalizer(&mut self, layout: eq::Layout, equalizer: &Equalizer) {
        self.flag(layout.enabled, equalizer.enabled, "parametricEQ.enabled");
        for (n, filter) in equalizer.filters.iter().enumerate() {
            let (on, frequency, q, gain, kind) = layout.band(n);
            let name = |part: &str| format!("parametricEQ.filter{}.{part}", n + 1);
            self.flag(on, filter.enabled, name("enabled"));
            self.number(q, filter.q_factor, name("qFactor"));
            self.number(frequency, filter.frequency, name("frequency"));
            self.number(gain, filter.gain, name("gain"));
            self.number(kind, f64::from(filter.kind.code()), name("type"));
        }
    }
}

/// Every write GG makes for a playback preset (83), in its order (`RenderConfigSettings.Apply`).
fn render_writes(r: &Render) -> Vec<Write> {
    let mut w = Writes(Vec::with_capacity(83));
    w.flag(4, r.global_enable, "globalEnableState");
    w.flag(44, r.bass_boost.enabled, "bassBoostState.enabled");
    w.number(45, r.bass_boost.value, "bassBoostState.value");
    w.number(81, r.reverb_gain_db, "reverbGainDB");
    w.flag(99, r.treble_boost.enabled, "trebleBoostState.enabled");
    w.number(100, r.treble_boost.value, "trebleBoostState.value");
    w.flag(165, r.voice_clarity.enabled, "voiceClarityState.enabled");
    w.number(166, r.voice_clarity.value, "voiceClarityState.value");
    w.flag(6, r.smart_volume.enabled, "smartVolume.enabled");
    w.number(7, r.smart_volume.value, "smartVolume.volumeLevel");
    w.number(17, r.loudness.threshold_db(), "smartVolume.loudness (compressor 1 threshold)");
    w.number(23, r.loudness.threshold_db(), "smartVolume.loudness (compressor 2 threshold)");
    w.number(169, r.general_gain, "generalGain");
    w.number(209, f64::from(r.form_factor.code()), "formFactor");
    w.flag(79, r.virtual_surround, "virtualSurroundState");
    w.flag(203, r.virtual_surround, "virtualSurroundState (advanced surround)");
    for (i, (name, speaker)) in SPEAKERS.iter().zip(&r.speakers).enumerate() {
        w.number(SPEAKER_POSITION_IDS[i], speaker.position, format!("virtualSurroundChannels.{name}.position"));
        w.number(SPEAKER_GAIN_IDS[i], speaker.gain, format!("virtualSurroundChannels.{name}.gain"));
    }
    w.equalizer(eq::PLAYBACK, &r.equalizer);
    w.0
}

/// Every write GG makes for a capture preset (65), in its order (`CaptureConfigSettings.Apply`).
fn capture_writes(c: &Capture) -> Vec<Write> {
    let mut w = Writes(Vec::with_capacity(65));
    let ai = c.noise_canceling.enabled;
    let forced = |field: &str| if ai { format!("{field} (off while AI noise cancelling is on)") } else { field.to_string() };
    w.flag(5, c.global_enable, "globalEnableState");
    w.flag(72, c.volume_stabilizer.enabled, "volumeStabilizerState.enabled");
    w.number(73, c.volume_stabilizer.value, "volumeStabilizerState.value");
    w.flag(56, c.noise_gate.enabled, "noiseGateState.enabled");
    w.flag(63, c.noise_reduction.enabled && !ai, forced("noiseReductionState.enabled"));
    w.flag(57, c.automatic_noise_gate.enabled, "automaticNoiseGateState.enabled");
    w.flag(161, c.impact_noise_reduction.enabled && !ai, forced("impactNoiseReductionState.enabled"));
    w.flag(185, ai, "noiseCancelingState.enabled");
    w.flag(204, c.acoustic_echo_canceling, "acousticEchoCancelingState");
    w.number(GATE_OFFSET_ID, f64::from(GATE_OFFSET_DB), "automatic noise gate offset (always 3 dB)");
    w.number(59, c.noise_gate.value, "noiseGateState.value");
    w.number(64, c.noise_reduction.value, "noiseReductionState.value");
    w.number(162, c.impact_noise_reduction.value, "impactNoiseReductionState.value");
    w.number(186, c.noise_canceling.value, "noiseCancelingState.value");
    w.equalizer(eq::CAPTURE, &c.equalizer);
    w.0
}

/// Every write that applying `preset` makes, in GG's order, before any comparison.
pub fn writes(preset: &Preset) -> Vec<Write> {
    match preset {
        Preset::Render(r) => render_writes(r),
        Preset::Capture(c) => capture_writes(c),
    }
}

/// The writes that turn `previous` into `next` on `role`'s store, in GG's order. With no
/// `previous`, every write; otherwise only the settings whose value changes, so that editing one
/// field writes one setting (or the few a rule ties to it). Comparing setting by setting, not field
/// by field, keeps the rules right: switching AI noise cancelling on writes the noise reduction
/// off, switching it off writes the preset's own noise reduction back.
pub fn plan(previous: Option<&Preset>, next: &Preset, role: Role) -> Result<Vec<Write>, String> {
    if !next.fits(role) {
        return Err(format!("this preset is not for the {} channel", role.api_name()));
    }
    let all = writes(next);
    let Some(previous) = previous.filter(|p| p.fits(role)) else {
        return Ok(all);
    };
    let before: HashMap<i32, Value> = writes(previous).into_iter().map(|w| (w.id, w.value)).collect();
    Ok(all.into_iter().filter(|w| before.get(&w.id) != Some(&w.value)).collect())
}

/// Keeps the writes whose value differs from what the module holds now: GG's own rule
/// (`SettingsUpdater.UpdateSettingValue` reads each setting and writes only on a change, B17).
/// Reads only.
pub fn pending(store: &Store, writes: &[Write]) -> Result<Vec<Write>, String> {
    pending_with(writes, |id| read_setting(store, id))
}

fn pending_with(writes: &[Write], read: impl Fn(i32) -> Result<Value, String>) -> Result<Vec<Write>, String> {
    let mut kept = Vec::new();
    for write in writes {
        if read(write.id)? != write.value {
            kept.push(write.clone());
        }
    }
    Ok(kept)
}

/// One setting of a store as a preset value.
pub fn read_setting(store: &Store, id: i32) -> Result<Value, String> {
    match store.setting(id).and_then(|s| s.value()).map_err(|e| format!("setting {id}: {e}"))? {
        apo::Value::Bool(b) => Ok(Value::Bool(b)),
        apo::Value::Int(n) => Ok(Value::Number(n as f32)),
        apo::Value::Float(x) => Ok(Value::Number(x)),
        other => Err(format!("setting {id} holds {other}, not a number or a switch")),
    }
}

/// Reads what the module holds now back into a preset, for a first start without any preset
/// chosen yet. Two things cannot come back: a noise reduction that AI noise cancelling holds off
/// reads as off, and the automatic gate's `value` (never written) reads as 0.
pub fn from_store(store: &Store, role: Role) -> Result<Preset, String> {
    if store.channel != role.channel() {
        return Err(format!("the {} store does not hold {} presets", store.channel.label(), role.api_name()));
    }
    from_reader(role, |id| read_setting(store, id))
}

fn from_reader(role: Role, read: impl Fn(i32) -> Result<Value, String>) -> Result<Preset, String> {
    let flag = |id: i32| -> Result<bool, String> {
        Ok(match read(id)? {
            Value::Bool(b) => b,
            Value::Number(n) => n != 0.0,
        })
    };
    let number = |id: i32| -> Result<f64, String> {
        Ok(match read(id)? {
            Value::Number(n) => widen(n),
            Value::Bool(b) => f64::from(u8::from(b)),
        })
    };
    let toggle = |on: i32, value: i32| -> Result<Toggle, String> { Ok(Toggle { enabled: flag(on)?, value: number(value)? }) };
    let equalizer = |layout: eq::Layout| -> Result<Equalizer, String> {
        let mut filters = [Filter { enabled: false, q_factor: 0.7071, frequency: 1000.0, gain: 0.0, kind: Kind::Peaking }; eq::BANDS];
        for (n, slot) in filters.iter_mut().enumerate() {
            let (on, frequency, q, gain, kind) = layout.band(n);
            *slot = Filter { enabled: flag(on)?, q_factor: number(q)?, frequency: number(frequency)?, gain: number(gain)?, kind: Kind::from_code(number(kind)? as i32) };
        }
        Ok(Equalizer { enabled: flag(layout.enabled)?, filters })
    };
    if role.is_capture() {
        return Ok(Preset::Capture(Capture {
            global_enable: flag(5)?,
            noise_reduction: toggle(63, 64)?,
            volume_stabilizer: toggle(72, 73)?,
            noise_gate: toggle(56, 59)?,
            automatic_noise_gate: Toggle { enabled: flag(57)?, value: 0.0 },
            equalizer: equalizer(eq::CAPTURE)?,
            impact_noise_reduction: toggle(161, 162)?,
            noise_canceling: toggle(185, 186)?,
            acoustic_echo_canceling: flag(204)?,
        }));
    }
    let mut speakers = [Speaker::default(); 8];
    for (i, speaker) in speakers.iter_mut().enumerate() {
        *speaker = Speaker { position: number(SPEAKER_POSITION_IDS[i])?, gain: number(SPEAKER_GAIN_IDS[i])? };
    }
    Ok(Preset::Render(Render {
        global_enable: flag(4)?,
        bass_boost: toggle(44, 45)?,
        treble_boost: toggle(99, 100)?,
        voice_clarity: toggle(165, 166)?,
        smart_volume: toggle(6, 7)?,
        loudness: Loudness::from_threshold_db(number(17)?),
        general_gain: number(169)?,
        equalizer: equalizer(eq::PLAYBACK)?,
        virtual_surround: flag(79)?,
        speakers,
        reverb_gain_db: number(81)?,
        form_factor: FormFactor::from_code(number(209)? as i32),
    }))
}

/// A module value as the decimal it was written from: 0.4 rather than 0.4000000059604645, so that a
/// preset read back from the module reads like one typed in. The result still converts back to
/// the same `f32`.
fn widen(value: f32) -> f64 {
    match value.to_string().parse::<f64>() {
        Ok(wide) if wide as f32 == value => wide,
        _ => f64::from(value),
    }
}

/// Writes a plan into a store, in order. Each value is checked against the setting's range and
/// refused when outside, as GG does (it does not clamp). Every write is tried even when one fails;
/// the error then lists the ones that failed. The change applies to live audio at once.
pub fn apply(store: &Store, writes: &[Write]) -> Result<usize, String> {
    let mut failures = Vec::new();
    for write in writes {
        let text = match write.value {
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => number_text(n),
        };
        if let Err(error) = store.setting(write.id).and_then(|s| s.set_from_text(&text)) {
            failures.push(format!("{} ({}) = {text}: {error}", write.id, write.field));
        }
    }
    if failures.is_empty() {
        Ok(writes.len())
    } else {
        Err(format!("{} of {} settings were not written: {}", failures.len(), writes.len(), failures.join("; ")))
    }
}

/// Applies a whole preset as GG selects one, and as the window applies a preset chosen: every
/// setting compared with what the module holds, and only those that differ written (B17), in GG's
/// order (B18). A setting that cannot be read is written anyway. Returns how many were written.
pub fn apply_whole(store: &Store, role: Role, next: &Preset) -> Result<usize, String> {
    let all = plan(None, next, role)?;
    let changes = pending(store, &all).unwrap_or(all);
    if changes.is_empty() {
        return Ok(0);
    }
    apply(store, &changes)
}

/// A number as `Setting::set_from_text` reads it: whole numbers without a decimal point, so that
/// whole-number settings (filter type, form factor) accept them.
fn number_text(n: f32) -> String {
    if n.fract() == 0.0 && n.abs() < 1e9 { format!("{}", n as i64) } else { format!("{n}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(writes: &[Write]) -> Vec<i32> {
        writes.iter().map(|w| w.id).collect()
    }

    fn value_of(writes: &[Write], id: i32) -> Option<Value> {
        writes.iter().find(|w| w.id == id).map(|w| w.value)
    }

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
    fn render_writes_follow_ggs_order() {
        let all = plan(None, &flat(Role::Game), Role::Game).unwrap();
        assert_eq!(all.len(), 83);
        let mut expected = vec![4, 44, 45, 81, 99, 100, 165, 166, 6, 7, 17, 23, 169, 209, 79, 203];
        expected.extend([187, 195, 188, 196, 189, 197, 190, 198, 191, 201, 192, 202, 193, 199, 194, 200]);
        expected.push(108);
        for n in 0..10 {
            let base = 109 + 5 * n;
            expected.extend([base, base + 2, base + 1, base + 3, base + 4]);
        }
        assert_eq!(ids(&all), expected);
        let mut unique = ids(&all);
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 83, "no setting is written twice");
    }

    #[test]
    fn capture_writes_follow_ggs_order() {
        let all = plan(None, &flat(Role::ChatCapture), Role::ChatCapture).unwrap();
        assert_eq!(all.len(), 65);
        let mut expected = vec![5, 72, 73, 56, 63, 57, 161, 185, 204, 58, 59, 64, 162, 186, 216];
        for n in 0..10 {
            let base = 217 + 5 * n;
            expected.extend([base, base + 2, base + 1, base + 3, base + 4]);
        }
        assert_eq!(ids(&all), expected);
        // Chat takes the same capture settings, on its own store.
        assert_eq!(ids(&plan(None, &flat(Role::ChatRender), Role::ChatRender).unwrap()), expected);
    }

    #[test]
    fn conversions_follow_gg() {
        let mut r = render(Role::Aux);
        r.loudness = Loudness::Soft;
        r.form_factor = FormFactor::Speakers;
        r.virtual_surround = true;
        r.general_gain = 4.5;
        r.equalizer.filters[0].kind = Kind::HighPass;
        r.equalizer.filters[9].kind = Kind::LowShelf;
        let all = plan(None, &Preset::Render(r), Role::Aux).unwrap();
        assert_eq!(value_of(&all, 17), Some(Value::Number(-27.0)));
        assert_eq!(value_of(&all, 23), Some(Value::Number(-27.0)));
        assert_eq!(value_of(&all, 209), Some(Value::Number(1.0)));
        assert_eq!(value_of(&all, 79), Some(Value::Bool(true)));
        assert_eq!(value_of(&all, 203), Some(Value::Bool(true)));
        assert_eq!(value_of(&all, 169), Some(Value::Number(4.5)));
        assert_eq!(value_of(&all, 113), Some(Value::Number(2.0)));
        assert_eq!(value_of(&all, 158), Some(Value::Number(8.0)));
        assert_eq!(value_of(&all, 51), None, "the volume boost setting is not GG's volume boost");
        assert_eq!(Loudness::Extreme.threshold_db(), -9.0);
        assert_eq!(Loudness::Balanced.threshold_db(), -18.0);
        for (db, loudness) in [(-30.0, Loudness::Soft), (-27.0, Loudness::Soft), (-20.0, Loudness::Balanced), (-18.0, Loudness::Balanced), (-17.9, Loudness::Extreme), (-9.0, Loudness::Extreme)] {
            assert_eq!(Loudness::from_threshold_db(db), loudness, "{db}");
        }
    }

    #[test]
    fn ai_noise_cancelling_holds_the_noise_reductions_off() {
        let mut c = capture(Role::ChatCapture);
        c.noise_reduction = Toggle { enabled: true, value: 0.6 };
        c.impact_noise_reduction = Toggle { enabled: true, value: 0.3 };
        c.noise_canceling.enabled = true;
        c.automatic_noise_gate = Toggle { enabled: true, value: 12.0 };
        let all = plan(None, &Preset::Capture(c), Role::ChatCapture).unwrap();
        assert_eq!(value_of(&all, 63), Some(Value::Bool(false)));
        assert_eq!(value_of(&all, 161), Some(Value::Bool(false)));
        // Their amounts are still written, and the gate offset is GG's constant.
        assert_eq!(value_of(&all, 64), Some(Value::Number(0.6)));
        assert_eq!(value_of(&all, 162), Some(Value::Number(0.3)));
        assert_eq!(value_of(&all, 58), Some(Value::Number(3.0)));
        assert_eq!(value_of(&all, 57), Some(Value::Bool(true)));

        // Switching AI off writes the preset's own noise reductions back, and nothing else.
        let mut off = c;
        off.noise_canceling.enabled = false;
        let diff = plan(Some(&Preset::Capture(c)), &Preset::Capture(off), Role::ChatCapture).unwrap();
        assert_eq!(ids(&diff), [63, 161, 185]);
        assert_eq!(value_of(&diff, 63), Some(Value::Bool(true)));
        // And back on.
        let diff = plan(Some(&Preset::Capture(off)), &Preset::Capture(c), Role::ChatCapture).unwrap();
        assert_eq!(diff.iter().map(|w| (w.id, w.value)).collect::<Vec<_>>(), [(63, Value::Bool(false)), (161, Value::Bool(false)), (185, Value::Bool(true))]);
        // While AI is on, turning the noise reduction on changes nothing in the module.
        let mut wanted = c;
        wanted.noise_reduction.enabled = false;
        assert!(plan(Some(&Preset::Capture(wanted)), &Preset::Capture(c), Role::ChatCapture).unwrap().is_empty());
    }

    #[test]
    fn a_previous_preset_limits_the_plan_to_changes() {
        let before = flat(Role::Media);
        assert!(plan(Some(&before), &before, Role::Media).unwrap().is_empty());
        let mut r = render(Role::Media);
        r.equalizer.filters[2].gain = 4.0;
        r.bass_boost = Toggle { enabled: true, value: 3.0 };
        r.virtual_surround = true;
        r.loudness = Loudness::Extreme;
        let diff = plan(Some(&before), &Preset::Render(r), Role::Media).unwrap();
        assert_eq!(ids(&diff), [44, 45, 17, 23, 79, 203, 122]);
        assert_eq!(diff[6].field, "parametricEQ.filter3.gain");
        // A previous preset of the other kind is ignored: everything is written.
        assert_eq!(plan(Some(&flat(Role::ChatCapture)), &Preset::Render(r), Role::Media).unwrap().len(), 83);
    }

    #[test]
    fn a_preset_goes_only_to_its_kind_of_channel() {
        assert!(plan(None, &flat(Role::Game), Role::ChatRender).is_err());
        assert!(plan(None, &flat(Role::ChatCapture), Role::Aux).is_err());
        assert!(flat(Role::ChatRender).fits(Role::ChatCapture));
        assert!(flat(Role::Game).fits(Role::Media));
    }

    #[test]
    fn the_neutral_presets_are_flat() {
        for role in Role::ALL {
            let preset = flat(role);
            let bands = preset.equalizer().bands();
            for f in [20.0, 100.0, 1000.0, 10_000.0, 20_000.0] {
                assert!(eq::response_db(&bands, f).abs() < 1e-6, "{role:?} at {f} Hz");
            }
            assert!(preset.boosts().bands().is_empty());
        }
        assert!(capture(Role::ChatCapture).noise_canceling.enabled);
        assert!(!capture(Role::ChatRender).noise_canceling.enabled);
        assert_eq!(render(Role::Game).equalizer.filters.iter().filter(|f| f.enabled).count(), 5);
    }

    #[test]
    fn json_round_trips_with_ggs_names_and_order() {
        for role in Role::ALL {
            let preset = flat(role);
            let json = preset.to_json();
            assert_eq!(Preset::from_json(&json, role).unwrap(), preset);
            assert_eq!(Preset::parse(&json.to_pretty(), role).unwrap(), preset);
        }
        let keys = |p: &Preset| -> Vec<String> { p.to_json().as_object().unwrap().iter().map(|(k, _)| k.clone()).collect() };
        assert_eq!(
            keys(&flat(Role::Aux)),
            ["bassBoostState", "trebleBoostState", "voiceClarityState", "smartVolume", "generalGain", "parametricEQ", "virtualSurroundState", "virtualSurroundChannels", "reverbGainDB", "formFactor", "globalEnableState"]
        );
        assert_eq!(
            keys(&flat(Role::ChatRender)),
            ["noiseReductionState", "volumeStabilizerState", "noiseGateState", "automaticNoiseGateState", "parametricEQ", "impactNoiseReductionState", "noiseCancelingState", "acousticEchoCancelingState", "globalEnableState"]
        );
        let text = flat(Role::ChatRender).to_json().to_compact();
        assert!(text.contains(r#""filter1":{"enabled":true,"qFactor":0.7071,"frequency":31,"gain":0,"type":"lowShelving"}"#), "{text}");
    }

    #[test]
    fn reading_json_fills_gaps_and_refuses_wrong_types() {
        let preset = Preset::parse(
            r#"{"bassBoostState":{"enabled":true,"value":3},"smartVolume":{"loudness":"soft"},"formFactor":"speakers",
                "parametricEQ":{"enabled":false,"filter2":{"type":"notchFilter","frequency":440},"filter3":{"type":5}},
                "virtualSurroundChannels":{"rearLeft":{"position":135,"gain":2}},"somethingNew":[1,2]}"#,
            Role::Game,
        )
        .unwrap();
        let Preset::Render(r) = preset else { panic!("a game preset is a playback preset") };
        let base = render(Role::Game);
        assert_eq!(r.bass_boost, Toggle { enabled: true, value: 3.0 });
        assert_eq!(r.loudness, Loudness::Soft);
        assert_eq!(r.form_factor, FormFactor::Speakers);
        assert!(!r.equalizer.enabled);
        assert_eq!((r.equalizer.filters[1].kind, r.equalizer.filters[1].frequency, r.equalizer.filters[1].q_factor), (Kind::Notch, 440.0, 0.7071));
        assert_eq!(r.equalizer.filters[2].kind, Kind::Notch);
        assert_eq!(r.speakers[4], Speaker { position: 135.0, gain: 2.0 });
        assert_eq!(r.speakers[0], base.speakers[0]);
        assert_eq!(r.reverb_gain_db, base.reverb_gain_db);

        for bad in [
            r#"{"generalGain":"loud"}"#,
            r#"{"bassBoostState":true}"#,
            r#"{"parametricEQ":{"filter1":{"type":"wobble"}}}"#,
            r#"{"parametricEQ":{"filter1":{"type":12}}}"#,
            r#"{"smartVolume":{"loudness":"max"}}"#,
            r#"[1]"#,
        ] {
            assert!(Preset::parse(bad, Role::Game).is_err(), "{bad}");
        }
        let error = Preset::parse(r#"{"parametricEQ":{"filter4":{"gain":"x"}}}"#, Role::Aux).unwrap_err();
        assert!(error.contains("parametricEQ.filter4.gain"), "{error}");
        assert!(Preset::parse(r#"{"acousticEchoCancelingState":1}"#, Role::ChatCapture).is_err());
    }

    #[test]
    fn filter_type_names_are_ggs() {
        let names: Vec<&str> = Kind::ALL.iter().map(|k| filter_type_name(*k)).collect();
        assert_eq!(names, ["byPass", "lowPass", "highPass", "bandPassPeakQ", "bandPassPeak0dB", "notchFilter", "allPass", "peakingEQ", "lowShelving", "highShelving"]);
        for kind in Kind::ALL {
            assert_eq!(filter_type_from_name(filter_type_name(kind)), Some(kind));
        }
        assert_eq!(filter_type_from_name("PEAKINGEQ"), Some(Kind::Peaking));
    }

    /// The module as a table of values, filled by a plan.
    fn module_after(preset: &Preset) -> HashMap<i32, Value> {
        writes(preset).into_iter().map(|w| (w.id, w.value)).collect()
    }

    #[test]
    fn reading_the_module_back_gives_the_preset() {
        let mut r = render(Role::Game);
        r.bass_boost = Toggle { enabled: true, value: -2.5 };
        r.loudness = Loudness::Extreme;
        r.form_factor = FormFactor::Speakers;
        r.speakers[6].position = 100.0;
        r.equalizer.filters[0].kind = Kind::HighPass;
        r.equalizer.filters[0].q_factor = 0.7071;
        let preset = Preset::Render(r);
        let module = module_after(&preset);
        let back = from_reader(Role::Game, |id| module.get(&id).copied().ok_or(format!("no {id}"))).unwrap();
        // Values pass through f32, as in the module: compare what they would write.
        assert_eq!(writes(&back), writes(&preset));

        let mut c = capture(Role::ChatCapture);
        c.noise_reduction = Toggle { enabled: true, value: 0.5 };
        c.automatic_noise_gate = Toggle { enabled: true, value: 7.0 };
        let module = module_after(&Preset::Capture(c));
        let back = from_reader(Role::ChatCapture, |id| module.get(&id).copied().ok_or(format!("no {id}"))).unwrap();
        let Preset::Capture(back) = back else { panic!() };
        // AI is on in the neutral microphone preset, so the module holds the noise reduction off.
        assert_eq!(back.noise_reduction, Toggle { enabled: false, value: 0.5 });
        assert_eq!(back.automatic_noise_gate, Toggle { enabled: true, value: 0.0 });
        assert!(plan(Some(&Preset::Capture(back)), &Preset::Capture(c), Role::ChatCapture).unwrap().is_empty());
    }

    #[test]
    fn pending_keeps_what_the_module_does_not_hold() {
        let preset = flat(Role::Aux);
        let all = writes(&preset);
        let mut module = module_after(&preset);
        assert!(pending_with(&all, |id| Ok(module[&id])).unwrap().is_empty());
        module.insert(81, Value::Number(0.16));
        module.insert(108, Value::Bool(false));
        let left = pending_with(&all, |id| Ok(module[&id])).unwrap();
        assert_eq!(ids(&left), [81, 108]);
        assert!(pending_with(&all, |_| Err("unreadable".into())).is_err());
    }

    #[test]
    fn numbers_are_written_as_the_module_reads_them() {
        assert_eq!(number_text(7.0), "7");
        assert_eq!(number_text(-27.0), "-27");
        assert_eq!(number_text(0.7071), "0.7071");
        assert_eq!(number_text(-0.5), "-0.5");
        assert_eq!(number_text(0.7071f64 as f32).parse::<f32>().unwrap(), 0.7071f32);
        assert_eq!(widen(0.4), 0.4);
        assert_eq!(widen(0.7071), 0.7071);
        assert_eq!(widen(-60.0), -60.0);
        assert_eq!(widen(0.9263848) as f32, 0.9263848f32);
    }

    #[test]
    fn arena_speakers_put_spatial_audio_in_speaker_mode_only_when_it_is_on() {
        let mut r = render(Role::Game);
        r.virtual_surround = true;
        let mut on = Preset::Render(r);
        assert!(on.arena_speakers());
        let Preset::Render(after) = on else { panic!() };
        assert_eq!(after.form_factor, FormFactor::Speakers);
        // Once in speaker mode, nothing more changes; spatial audio off, headphones stay.
        assert!(!on.arena_speakers());
        let mut off = flat(Role::Media);
        assert!(!off.arena_speakers());
        assert_eq!(off, flat(Role::Media));
        // Capture presets have no spatial audio.
        let mut mic = flat(Role::ChatCapture);
        assert!(!mic.arena_speakers());
        // The rule's write is the form factor alone.
        let diff = plan(Some(&Preset::Render(r)), &on, Role::Game).unwrap();
        assert_eq!(ids(&diff), [209]);
    }

    #[test]
    fn a_chosen_preset_keeps_the_channels_effects_switch() {
        let mut current = flat(Role::Aux);
        current.set_effects(false);
        let chosen = flat(Role::Aux).with_effects_of(&current);
        assert!(!chosen.effects() && flat(Role::Aux).effects());
    }

    #[test]
    fn roles_match_ggs_numbers_and_platinums_channels() {
        for (vad, role, channel) in [(1, Role::Game, Channel::Game), (2, Role::ChatRender, Channel::Chat), (3, Role::ChatCapture, Channel::Mic), (4, Role::Media, Channel::Media), (5, Role::Aux, Channel::Aux)] {
            assert_eq!(Role::from_vad(vad), Some(role));
            assert_eq!(role.vad(), vad);
            assert_eq!(role.channel(), channel);
            assert_eq!(Role::from_channel(channel), role);
        }
        assert_eq!(Role::from_vad(6), None);
    }
}
