//! Where the output channels play, as their settings stores hold it, and the microphone's echo
//! cancellation references that must follow.
//!
//! Each output channel sends its sound to a real device through its stream redirection: a switch
//! and a device identifier in its store (163 and 164; Chat keeps its routing under the capture
//! numbers, 269 and 270). The window's menus and the daemon's backup switch both write them here,
//! the same way.
//!
//! The microphone's effects engine takes the sound of the outputs the user hears out of the
//! microphone (echo cancellation). It listens to the devices named in the microphone's store
//! (`kSet_AEC…DeviceId`): GG writes the device the user hears into the first and clears the last
//! two whenever an output changes; the second is its stream mix, cleared while Platinum does not run
//! the stream channel (the engine would listen to a disabled device). Outside streamer mode,
//! Platinum puts its other channels' devices, if any, in the last two. In streamer mode (see
//! `streamer`) the channels share the personal mix's device, and the second reference names the
//! stream mix's.

use crate::apo::{Channel, Store, Value};
use crate::devices::{Endpoint, Flow};

/// The output channels, in the mixer's order, with their French titles.
pub const CHANNELS: [(Channel, &str); 4] = [(Channel::Game, "Gaming"), (Channel::Chat, "Chat"), (Channel::Media, "Media"), (Channel::Aux, "Aux")];
/// The channels with spatial audio, those GG's Arena 7 rule looks at (`_virtualSurroundValidVADs`).
pub const SURROUND: [Channel; 3] = [Channel::Game, Channel::Media, Channel::Aux];
/// How Windows names Arena 7 speakers ("Haut-parleurs (SteelSeries Arena 7)"). GG knows them by a
/// device feature Platinum cannot read; the name is what Windows shows.
pub const ARENA_NAME: &str = "SteelSeries Arena 7";
/// Spatial audio's switch and its headphones or speakers mode (0 headphones, 1 speakers).
const SURROUND_SWITCH: i32 = 79;
const FORM_FACTOR: i32 = 209;

/// Whether an output is Arena 7 speakers: a real output (not one of Platinum's channels) whose name
/// holds `ARENA_NAME`, ignoring case.
pub fn is_arena(endpoint: &Endpoint) -> bool {
    endpoint.flow == Flow::Render && endpoint.virtual_role.is_none() && endpoint.name.to_lowercase().contains(&ARENA_NAME.to_lowercase())
}

/// Whether a channel plays to Arena 7 speakers (its personal mix's device in streamer mode, which
/// is the same setting).
pub fn plays_to_arena(store: &Store, endpoints: &[Endpoint]) -> bool {
    routed(store).is_some_and(|id| endpoints.iter().any(|e| e.id == id && is_arena(e)))
}

/// GG's Arena 7 rule (B7) on a surround channel that plays to Arena 7 speakers, after its output
/// changed to them: spatial audio on in headphone mode goes to speaker mode. Writes the form factor
/// alone; returns whether it did. Never goes back to headphones.
pub fn arena_rule(store: &Store, endpoints: &[Endpoint]) -> Result<bool, String> {
    if !SURROUND.contains(&store.channel) || !plays_to_arena(store, endpoints) {
        return Ok(false);
    }
    let surround = crate::preset::read_setting(store, SURROUND_SWITCH)?;
    let form = crate::preset::read_setting(store, FORM_FACTOR)?;
    if surround != crate::preset::Value::Bool(true) || form != crate::preset::Value::Number(0.0) {
        return Ok(false);
    }
    store.setting(FORM_FACTOR).and_then(|s| s.set_from_text("1")).map_err(|e| format!("setting {FORM_FACTOR}: {e}"))?;
    Ok(true)
}
/// (routing switch, device identifier) of Gaming, Media and Aux.
pub const RENDER_OUTPUT: (i32, i32) = (163, 164);
/// The same for Chat, under the capture numbering.
pub const CHAT_OUTPUT: (i32, i32) = (269, 270);
/// Echo cancellation references on the microphone's store: the device heard, GG's stream mix, and
/// two more.
pub const AEC_HEARD: i32 = 205;
pub const AEC_STREAM: i32 = 206;
pub const AEC_OTHERS: [i32; 2] = [207, 208];
/// The microphone's echo cancellation switch: the engine listens to its references only with it.
pub const AEC_SWITCH: i32 = 204;

/// The routing settings of an output channel; `None` for the microphone.
pub fn output_settings(channel: Channel) -> Option<(i32, i32)> {
    match channel {
        Channel::Game | Channel::Media | Channel::Aux => Some(RENDER_OUTPUT),
        Channel::Chat => Some(CHAT_OUTPUT),
        Channel::Mic => None,
    }
}

/// The real output a channel plays to: `None` when it is not routed, routed to nothing, or its
/// settings cannot be read.
pub fn routed(store: &Store) -> Option<String> {
    let (enabled, device) = output_settings(store.channel)?;
    let on = matches!(store.setting(enabled).and_then(|s| s.value()), Ok(Value::Bool(true)));
    if !on {
        return None;
    }
    match store.setting(device).and_then(|s| s.value()) {
        Ok(Value::Text(id)) if !id.is_empty() => Some(id),
        _ => None,
    }
}

/// Routes a channel to a real output (`None`: to nothing), as the window's menus always have: the
/// device first, then the switch, so that the channel never plays to a device it is leaving.
pub fn set_output(store: &Store, device: Option<&str>) -> Result<(), String> {
    let (enabled, id) = output_settings(store.channel).ok_or("ce canal n'a pas de sortie")?;
    let write = |setting: i32, value: &str| store.setting(setting).and_then(|s| s.set_from_text(value)).map_err(|e| e.to_string());
    match device {
        Some(device) => {
            write(id, device)?;
            write(enabled, "true")
        }
        None => write(enabled, "false"),
    }
}

/// Each device once, in order.
pub fn distinct(routed: &[Option<String>]) -> Vec<&String> {
    let mut heard: Vec<&String> = Vec::new();
    for id in routed.iter().flatten() {
        if !heard.contains(&id) {
            heard.push(id);
        }
    }
    heard
}

/// What the second echo cancellation reference, GG's stream mix, should hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamReference<'a> {
    /// Nothing: Platinum does not run the stream channel (the engine would listen to a disabled
    /// device).
    Cleared,
    /// Whatever it holds: the stream channel runs outside streamer mode (asked for by hand in
    /// platinum.ini's channels).
    LeftAlone,
    /// Streamer mode: the stream mix's device (empty when it has none), as GG's
    /// `SetAECStreamingDeviceId` writes it; the last two references are then cleared (B3).
    Mix(&'a str),
}

impl<'a> StreamReference<'a> {
    /// The reference for the mixer's mode, the channels the daemon brings up, and the stream mix's
    /// device (resolved, see `streamer::StreamDevice::resolve`).
    pub fn of(mode: crate::streamer::Mode, channels: &[String], stream_device: Option<&'a str>) -> StreamReference<'a> {
        match mode {
            crate::streamer::Mode::Streamer => StreamReference::Mix(stream_device.unwrap_or("")),
            crate::streamer::Mode::Classic if crate::streamer::has_stream_channel(channels) => StreamReference::LeftAlone,
            crate::streamer::Mode::Classic => StreamReference::Cleared,
        }
    }
}

/// The echo cancellation references the microphone should hold for the outputs the channels play
/// to (Gaming, Chat, Media, Aux, in order; `None` for one that has no output or cannot be read): the
/// first device heard in the first reference, as GG writes it, and any other in the last two
/// (outside streamer mode, where they can differ). The second reference is GG's stream mix (see
/// `StreamReference`).
pub fn echo_references(routed: &[Option<String>], stream: StreamReference) -> Vec<(i32, String)> {
    let heard = distinct(routed);
    let at = |index: usize| heard.get(index).map(|id| id.to_string()).unwrap_or_default();
    match stream {
        StreamReference::Mix(device) => vec![(AEC_HEARD, at(0)), (AEC_STREAM, device.to_string()), (AEC_OTHERS[0], String::new()), (AEC_OTHERS[1], String::new())],
        StreamReference::Cleared => vec![(AEC_HEARD, at(0)), (AEC_STREAM, String::new()), (AEC_OTHERS[0], at(1)), (AEC_OTHERS[1], at(2))],
        StreamReference::LeftAlone => vec![(AEC_HEARD, at(0)), (AEC_OTHERS[0], at(1)), (AEC_OTHERS[1], at(2))],
    }
}

/// A text setting of a store; empty when it cannot be read.
fn text(store: &Store, id: i32) -> String {
    match store.setting(id).and_then(|s| s.value()) {
        Ok(Value::Text(text)) => text,
        _ => String::new(),
    }
}

/// Points the microphone's echo cancellation at the outputs heard (see `echo_references`),
/// writing only the references that differ. Returns how many were written.
pub fn sync_echo(mic: &Store, routed: &[Option<String>], stream: StreamReference) -> Result<usize, String> {
    let mut written = 0;
    for (id, device) in echo_references(routed, stream) {
        if text(mic, id) != device {
            mic.setting(id).and_then(|s| s.set_from_text(&device)).map_err(|e| e.to_string())?;
            written += 1;
        }
    }
    Ok(written)
}

/// The four echo cancellation references as the microphone's store holds them now, 205 to 208 in
/// order (empty for one that names nothing). Fails if one cannot be read: the caller then keeps
/// what it knew rather than taking an unreadable reference for an empty one.
pub fn references_held(mic: &Store) -> Result<Vec<String>, String> {
    [AEC_HEARD, AEC_STREAM, AEC_OTHERS[0], AEC_OTHERS[1]]
        .into_iter()
        .map(|id| match mic.setting(id).and_then(|s| s.value()) {
            Ok(Value::Text(text)) => Ok(text),
            Ok(other) => Err(format!("setting {id} holds {other}, not a device")),
            Err(error) => Err(format!("setting {id}: {error}")),
        })
        .collect()
}

/// Whether echo cancellation is on and listens to other outputs than those the channels play to.
/// Reads only.
pub fn echo_stale(mic: &Store, routed: &[Option<String>], stream: StreamReference) -> bool {
    let on = matches!(mic.setting(AEC_SWITCH).and_then(|s| s.value()), Ok(Value::Bool(true)));
    on && echo_references(routed, stream).iter().any(|(id, device)| text(mic, *id) != *device)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_echo_cancellation_listens_to_every_device_heard() {
        let routed = [Some("{arena}".to_string()), Some("{arena}".to_string()), None, Some("{airpods}".to_string())];
        assert_eq!(echo_references(&routed, StreamReference::Cleared), [(205, "{arena}".to_string()), (206, String::new()), (207, "{airpods}".to_string()), (208, String::new())]);
        // With the stream channel run by hand, its reference is left alone.
        assert!(!echo_references(&routed, StreamReference::LeftAlone).iter().any(|(id, _)| *id == 206));
        assert!(echo_references(&[None, None, None, None], StreamReference::Cleared).iter().all(|(_, device)| device.is_empty()));
        assert_eq!(distinct(&routed), [&"{arena}".to_string(), &"{airpods}".to_string()]);
    }

    #[test]
    fn streamer_mode_points_the_echo_cancellation_at_both_mixes() {
        use crate::streamer::Mode;
        let routed = [Some("{airpods}".to_string()), None, Some("{airpods}".to_string()), None];
        // GG's rule (B3): the personal mix's device, the stream mix's, and the last two cleared.
        let wanted = echo_references(&routed, StreamReference::Mix("{stream}"));
        assert_eq!(wanted, [(205, "{airpods}".to_string()), (206, "{stream}".to_string()), (207, String::new()), (208, String::new())]);
        let channels: Vec<String> = ["game", "mic", "stream"].iter().map(|c| c.to_string()).collect();
        assert_eq!(StreamReference::of(Mode::Streamer, &channels, None), StreamReference::Mix(""));
        assert_eq!(StreamReference::of(Mode::Classic, &channels, Some("{stream}")), StreamReference::LeftAlone);
        assert_eq!(StreamReference::of(Mode::Classic, &channels[..2], Some("{stream}")), StreamReference::Cleared);
    }

    #[test]
    fn arena_speakers_are_known_by_their_name_among_the_real_outputs() {
        use crate::devices::State;
        let endpoint = |name: &str, flow: Flow, role: Option<u32>| Endpoint { id: "{a}".into(), name: name.into(), flow, state: State::Active, virtual_role: role };
        assert!(is_arena(&endpoint("Haut-parleurs (SteelSeries Arena 7)", Flow::Render, None)));
        assert!(is_arena(&endpoint("Speakers (steelseries arena 7)", Flow::Render, None)));
        // Its microphone, one of Platinum's channels named after it, and the Arena 9 are not.
        assert!(!is_arena(&endpoint("Microphone (SteelSeries Arena 7)", Flow::Capture, None)));
        assert!(!is_arena(&endpoint("Gaming (SteelSeries Arena 7)", Flow::Render, Some(2))));
        assert!(!is_arena(&endpoint("Haut-parleurs (SteelSeries Arena 9)", Flow::Render, None)));
        assert_eq!(SURROUND, [Channel::Game, Channel::Media, Channel::Aux]);
    }

    #[test]
    fn only_the_output_channels_have_routing() {
        assert_eq!(output_settings(Channel::Game), Some((163, 164)));
        assert_eq!(output_settings(Channel::Chat), Some((269, 270)));
        assert_eq!(output_settings(Channel::Mic), None);
        assert!(CHANNELS.iter().all(|(channel, _)| output_settings(*channel).is_some()));
    }
}
