//! GG's streamer mode ("Streamer mode", `PUT /mode/stream`): two mixes instead of one.
//!
//! In the classic mixer each output channel plays to a real device of its own, through its first
//! stream redirection (163/164; Chat 269/270). In streamer mode every channel feeds two mixes, each
//! with one device for all the channels: the personal mix (what the user hears, GG's "monitoring"),
//! still through the first redirection, and the stream mix (what the audience hears) through the
//! second one (210/211; Chat 271/272), by default into the Stream virtual device that streaming
//! software records. Each channel has a volume and a mute in each mix (`mixer::Mix`).
//!
//! A channel is in a mix when its redirection names the mix's device, and out of it when the
//! device is empty (GG's `StreamRedirectionMonitoring/Streaming.SetTargetDeviceId`:
//! `IsEnabled ? device : String.Empty`). The choice of channels is kept in platinum.ini
//! (`personal_mix`, `stream_mix`), so that leaving streamer mode and coming back finds it again.
//!
//! GG keeps each mode's devices apart. Platinum does the same: entering streamer mode remembers
//! the classic outputs (`classic_outputs`), and leaving it puts them back, except a device that is
//! gone, replaced by the personal mix's device. Leaving also takes every channel out of the stream
//! mix, so that nothing is sent to it in classic mode.
//!
//! Everything here is pure except `mix_device` and `set_mix_device`, which read and write a store.
//! The window carries a switch out; the daemon only keeps the Stream virtual device enabled while
//! the configuration asks for the `stream` channel.

use crate::apo::{Store, Value};
use crate::devices::{Endpoint, Flow};
use crate::mixer::{Mix, STRIPS};
use crate::outputs::{CHAT_OUTPUT, RENDER_OUTPUT};

/// The strips' names in platinum.ini, Gaming to Aux.
pub const STRIP_KEYS: [&str; STRIPS] = ["game", "chat", "media", "aux"];
/// The stream mix's (switch, device) of Gaming, Media and Aux: the second redirection.
pub const RENDER_STREAM_OUTPUT: (i32, i32) = (210, 211);
/// The same for Chat, under the capture numbering.
pub const CHAT_STREAM_OUTPUT: (i32, i32) = (271, 272);
/// The channel of the Stream virtual device, as `session` and `config` name it.
pub const STREAM_CHANNEL: &str = "stream";

/// The mixer's mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// One output per channel, one volume: the mixer as Platinum always had it.
    #[default]
    Classic,
    /// A personal mix and a stream mix.
    Streamer,
}

impl Mode {
    /// Its name in platinum.ini.
    pub fn key(self) -> &'static str {
        match self {
            Mode::Classic => "classic",
            Mode::Streamer => "streamer",
        }
    }

    /// Reads platinum.ini's value, in English or French.
    pub fn parse(text: &str) -> Option<Mode> {
        match text.trim().to_lowercase().as_str() {
            "classic" | "classique" => Some(Mode::Classic),
            "streamer" | "stream" => Some(Mode::Streamer),
            _ => None,
        }
    }
}

/// Where the stream mix goes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum StreamDevice {
    /// The Stream virtual device ("Stream (Platinum)"), GG's default: streaming software records
    /// its recording side.
    #[default]
    Virtual,
    /// Nowhere: the stream mix is not played.
    None,
    /// A real output, by its endpoint identifier.
    Device(String),
}

impl StreamDevice {
    /// platinum.ini's value: empty for the virtual device, `none`, or an identifier.
    pub fn parse(text: &str) -> StreamDevice {
        match text.trim() {
            "" => StreamDevice::Virtual,
            none if none.eq_ignore_ascii_case("none") || none.eq_ignore_ascii_case("aucune") => StreamDevice::None,
            id => StreamDevice::Device(id.to_string()),
        }
    }

    pub fn to_text(&self) -> String {
        match self {
            StreamDevice::Virtual => String::new(),
            StreamDevice::None => "none".into(),
            StreamDevice::Device(id) => id.clone(),
        }
    }

    /// The endpoint identifier the stream mix plays to, among `endpoints`; `None` for nowhere, or
    /// while the Stream virtual device does not exist. A real device is named even when it is
    /// unplugged: the module keeps it and plays again when it comes back.
    pub fn resolve(&self, endpoints: &[Endpoint]) -> Option<String> {
        match self {
            StreamDevice::Virtual => virtual_stream(endpoints).map(|e| e.id.clone()),
            StreamDevice::None => None,
            StreamDevice::Device(id) => Some(id.clone()),
        }
    }
}

/// The playback side of the Stream virtual device (driver role 4), whatever its state.
pub fn virtual_stream(endpoints: &[Endpoint]) -> Option<&Endpoint> {
    endpoints.iter().find(|e| e.flow == Flow::Render && crate::session::virtual_kind(e).as_deref() == Some("stream"))
}

/// Which strips a mix holds, Gaming to Aux.
pub type Inclusion = [bool; STRIPS];

/// Every strip, GG's default for both mixes.
pub const ALL_STRIPS: Inclusion = [true; STRIPS];

/// A mix's strips as platinum.ini writes them: "game, chat, media, aux".
pub fn inclusion_text(inclusion: &Inclusion) -> String {
    STRIP_KEYS.iter().zip(inclusion).filter(|(_, on)| **on).map(|(key, _)| *key).collect::<Vec<_>>().join(", ")
}

/// Reads a mix's strips; the error is the first name that is not a strip.
pub fn parse_inclusion(text: &str) -> Result<Inclusion, String> {
    let mut inclusion = [false; STRIPS];
    for name in text.split(',').map(|n| n.trim().to_ascii_lowercase()).filter(|n| !n.is_empty()) {
        let name = if name == "gaming" { "game".to_string() } else { name };
        let index = STRIP_KEYS.iter().position(|key| *key == name).ok_or(name)?;
        inclusion[index] = true;
    }
    Ok(inclusion)
}

/// The classic outputs as platinum.ini keeps them: four identifiers, empty for no output.
pub fn outputs_text(outputs: &[Option<String>; STRIPS]) -> String {
    outputs.iter().map(|o| o.as_deref().unwrap_or("")).collect::<Vec<_>>().join(", ")
}

/// Reads the classic outputs: exactly four values, Gaming to Aux.
pub fn parse_outputs(text: &str) -> Option<[Option<String>; STRIPS]> {
    let values: Vec<Option<String>> = text.split(',').map(|v| Some(v.trim()).filter(|v| !v.is_empty()).map(str::to_string)).collect();
    values.try_into().ok()
}

/// The channels the daemon brings up, with the Stream virtual device added or taken away.
pub fn with_stream_channel(channels: &[String], on: bool) -> Vec<String> {
    let mut list: Vec<String> = channels.iter().filter(|c| c.as_str() != STREAM_CHANNEL).cloned().collect();
    if on {
        list.push(STREAM_CHANNEL.to_string());
    }
    list
}

/// A channel's (switch, device) in one mix; `None` for the microphone.
pub fn mix_settings(channel: crate::apo::Channel, mix: Mix) -> Option<(i32, i32)> {
    use crate::apo::Channel;
    match (channel, mix) {
        (Channel::Game | Channel::Media | Channel::Aux, Mix::Personal) => Some(RENDER_OUTPUT),
        (Channel::Game | Channel::Media | Channel::Aux, Mix::Stream) => Some(RENDER_STREAM_OUTPUT),
        (Channel::Chat, Mix::Personal) => Some(CHAT_OUTPUT),
        (Channel::Chat, Mix::Stream) => Some(CHAT_STREAM_OUTPUT),
        (Channel::Mic, _) => None,
    }
}

/// The device a channel plays to in a mix: `None` when its redirection is off or names nothing
/// (it is out of the mix), or cannot be read.
pub fn mix_device(store: &Store, mix: Mix) -> Option<String> {
    let (switch, device) = mix_settings(store.channel, mix)?;
    if !matches!(store.setting(switch).and_then(|s| s.value()), Ok(Value::Bool(true))) {
        return None;
    }
    match store.setting(device).and_then(|s| s.value()) {
        Ok(Value::Text(id)) if !id.is_empty() => Some(id),
        _ => None,
    }
}

/// Puts a channel in a mix on `device`, or takes it out (`None`), as GG does: the device, or an
/// empty one, then the switch on, so that the channel never plays to a device it is leaving.
pub fn set_mix_device(store: &Store, mix: Mix, device: Option<&str>) -> Result<(), String> {
    let (switch, id) = mix_settings(store.channel, mix).ok_or("ce canal n'a pas de sortie")?;
    let write = |setting: i32, value: &str| store.setting(setting).and_then(|s| s.set_from_text(value)).map_err(|e| e.to_string());
    write(id, device.unwrap_or(""))?;
    if device.is_some() {
        write(switch, "true")?;
    }
    Ok(())
}

/// One routing write: a strip's place in a mix (`None`: out of it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub strip: usize,
    pub mix: Mix,
    pub device: Option<String>,
}

/// The personal mix's device: the one the channels play to (the first, should they differ), else
/// the one remembered for when no channel is in the mix.
pub fn personal_device(routed: &[Option<String>], remembered: Option<&str>) -> Option<String> {
    routed.iter().flatten().next().cloned().or_else(|| remembered.filter(|id| !id.is_empty()).map(str::to_string))
}

/// What streamer mode writes for the two mixes: each strip on the mix's device when it is in the
/// mix, out of it otherwise (and out of a mix that has no device).
pub fn routes(personal: Option<&str>, stream: Option<&str>, personal_mix: &Inclusion, stream_mix: &Inclusion) -> Vec<Route> {
    let mut writes = Vec::new();
    for strip in 0..STRIPS {
        writes.push(Route { strip, mix: Mix::Personal, device: personal.filter(|_| personal_mix[strip]).map(str::to_string) });
        writes.push(Route { strip, mix: Mix::Stream, device: stream.filter(|_| stream_mix[strip]).map(str::to_string) });
    }
    writes
}

/// The routes that change one mix only (its device, or the strips it holds).
pub fn mix_routes(mix: Mix, device: Option<&str>, inclusion: &Inclusion) -> Vec<Route> {
    (0..STRIPS).map(|strip| Route { strip, mix, device: device.filter(|_| inclusion[strip]).map(str::to_string) }).collect()
}

/// The classic outputs to put back when leaving streamer mode: each strip's own, as remembered,
/// while that device is still there (`present`); the personal mix's device instead of one that is
/// gone, or for every strip when nothing was remembered. A strip that had no output keeps none.
pub fn classic_outputs(saved: Option<&[Option<String>; STRIPS]>, personal: Option<&str>, present: &[&str]) -> [Option<String>; STRIPS] {
    std::array::from_fn(|strip| match saved {
        None => personal.map(str::to_string),
        Some(saved) => match saved[strip].as_deref() {
            None => None,
            Some(id) if present.contains(&id) => Some(id.to_string()),
            Some(id) => Some(personal.unwrap_or(id).to_string()),
        },
    })
}

/// Whether "Écouter le stream" can be used (GG's `IsStreamMonitoringActivable`): the stream mix
/// plays somewhere, and somewhere else than the personal mix.
pub fn can_listen(personal: Option<&str>, stream: Option<&str>) -> bool {
    match (personal, stream) {
        (Some(personal), Some(stream)) => personal != stream,
        _ => false,
    }
}

/// Whether the channels name the Stream virtual device.
pub fn has_stream_channel(channels: &[String]) -> bool {
    channels.iter().any(|c| c == STREAM_CHANNEL)
}

/// Whether the daemon should keep the Stream virtual device enabled: streamer mode asks for it
/// whatever the stream mix's device is, so that streaming software always finds its recording
/// side (the window also writes `stream` into the channels, for the next start); the channels may
/// also name it by hand.
pub fn wants_stream_channel(mode: Mode, channels: &[String]) -> bool {
    mode == Mode::Streamer || has_stream_channel(channels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apo::Channel;
    use crate::devices::State;

    fn endpoint(id: &str, name: &str, flow: Flow, role: Option<u32>) -> Endpoint {
        Endpoint { id: id.into(), name: name.into(), flow, state: State::Disabled, virtual_role: role }
    }

    #[test]
    fn modes_and_devices_read_what_they_write() {
        for mode in [Mode::Classic, Mode::Streamer] {
            assert_eq!(Mode::parse(mode.key()), Some(mode));
        }
        assert_eq!(Mode::parse(" Classique "), Some(Mode::Classic));
        assert_eq!(Mode::parse("loud"), None);
        assert_eq!(Mode::default(), Mode::Classic);
        for device in [StreamDevice::Virtual, StreamDevice::None, StreamDevice::Device("{0.0.0.00000000}.{a}".into())] {
            assert_eq!(StreamDevice::parse(&device.to_text()), device);
        }
        assert_eq!(StreamDevice::parse("Aucune"), StreamDevice::None);
    }

    #[test]
    fn the_stream_mix_finds_the_virtual_device_by_its_role() {
        let endpoints = [
            endpoint("{cap}", "Stream (Platinum)", Flow::Capture, Some(4)),
            endpoint("{game}", "Gaming (Platinum)", Flow::Render, Some(2)),
            endpoint("{stream}", "Stream (Platinum)", Flow::Render, Some(4)),
        ];
        assert_eq!(StreamDevice::Virtual.resolve(&endpoints).as_deref(), Some("{stream}"));
        assert_eq!(StreamDevice::None.resolve(&endpoints), None);
        assert_eq!(StreamDevice::Device("{gone}".into()).resolve(&endpoints).as_deref(), Some("{gone}"));
        assert_eq!(StreamDevice::Virtual.resolve(&endpoints[..2]), None);
    }

    #[test]
    fn mixes_and_outputs_are_lines_of_platinum_ini() {
        assert_eq!(inclusion_text(&ALL_STRIPS), "game, chat, media, aux");
        assert_eq!(inclusion_text(&[false; STRIPS]), "");
        assert_eq!(parse_inclusion("Gaming, aux, game"), Ok([true, false, false, true]));
        assert_eq!(parse_inclusion(""), Ok([false; STRIPS]));
        assert_eq!(parse_inclusion("game, mic"), Err("mic".to_string()));
        let outputs = [Some("{a}".to_string()), None, Some("{b}".to_string()), Some("{a}".to_string())];
        assert_eq!(parse_outputs(&outputs_text(&outputs)), Some(outputs));
        assert_eq!(parse_outputs(", , , "), Some([None, None, None, None]));
        assert_eq!(parse_outputs("{a}, {b}"), None);
        let channels: Vec<String> = ["game", "chat", "mic"].iter().map(|c| c.to_string()).collect();
        let on = with_stream_channel(&channels, true);
        assert!(has_stream_channel(&on) && !has_stream_channel(&channels));
        assert!(wants_stream_channel(Mode::Classic, &on) && wants_stream_channel(Mode::Streamer, &channels));
        assert!(!wants_stream_channel(Mode::Classic, &channels));
        assert_eq!(with_stream_channel(&on, true), on);
        assert_eq!(with_stream_channel(&on, false), channels);
    }

    #[test]
    fn each_mix_has_its_own_redirection() {
        assert_eq!(mix_settings(Channel::Game, Mix::Personal), Some((163, 164)));
        assert_eq!(mix_settings(Channel::Media, Mix::Stream), Some((210, 211)));
        assert_eq!(mix_settings(Channel::Chat, Mix::Personal), Some((269, 270)));
        assert_eq!(mix_settings(Channel::Chat, Mix::Stream), Some((271, 272)));
        assert_eq!(mix_settings(Channel::Mic, Mix::Stream), None);
    }

    #[test]
    fn a_channel_out_of_a_mix_names_no_device() {
        let writes = routes(Some("{airpods}"), Some("{stream}"), &[true, true, false, true], &[true, false, true, true]);
        let device = |strip: usize, mix: Mix| writes.iter().find(|r| r.strip == strip && r.mix == mix).and_then(|r| r.device.clone());
        assert_eq!(writes.len(), 2 * STRIPS);
        assert_eq!(device(0, Mix::Personal).as_deref(), Some("{airpods}"));
        assert_eq!(device(2, Mix::Personal), None);
        assert_eq!(device(1, Mix::Stream), None);
        assert_eq!(device(3, Mix::Stream).as_deref(), Some("{stream}"));
        // A mix without a device takes every channel out.
        assert!(routes(None, Some("{stream}"), &ALL_STRIPS, &ALL_STRIPS).iter().filter(|r| r.mix == Mix::Personal).all(|r| r.device.is_none()));
        let stream_only = mix_routes(Mix::Stream, Some("{stream}"), &[false, true, false, false]);
        assert_eq!(stream_only.iter().map(|r| r.device.is_some()).collect::<Vec<_>>(), [false, true, false, false]);
        assert!(stream_only.iter().all(|r| r.mix == Mix::Stream));
    }

    #[test]
    fn the_personal_mix_plays_where_the_channels_play() {
        let routed = [None, Some("{arena}".to_string()), Some("{airpods}".to_string()), None];
        assert_eq!(personal_device(&routed, Some("{old}")).as_deref(), Some("{arena}"));
        assert_eq!(personal_device(&[None, None], Some("{old}")).as_deref(), Some("{old}"));
        assert_eq!(personal_device(&[None, None], Some("")), None);
    }

    #[test]
    fn leaving_streamer_mode_gives_the_classic_outputs_back() {
        let saved = [Some("{airpods}".to_string()), Some("{arena}".to_string()), None, Some("{gone}".to_string())];
        let back = classic_outputs(Some(&saved), Some("{arena}"), &["{airpods}", "{arena}"]);
        // Gaming and Chat as they were, Media had no output, Aux's device is gone: the personal one.
        assert_eq!(back, [Some("{airpods}".to_string()), Some("{arena}".to_string()), None, Some("{arena}".to_string())]);
        // Nothing remembered: every channel on the personal mix's device.
        assert_eq!(classic_outputs(None, Some("{arena}"), &[]), std::array::from_fn(|_| Some("{arena}".to_string())));
        // Without a personal device either, a device that is gone is kept (the module plays again
        // when it comes back).
        assert_eq!(classic_outputs(Some(&saved), None, &[])[3].as_deref(), Some("{gone}"));
    }

    #[test]
    fn listening_to_the_stream_needs_two_devices() {
        assert!(can_listen(Some("{airpods}"), Some("{stream}")));
        assert!(!can_listen(Some("{airpods}"), Some("{airpods}")));
        assert!(!can_listen(Some("{airpods}"), None));
        assert!(!can_listen(None, Some("{stream}")));
    }
}
