//! The mixer as GG runs it, shared by the window and the daemon's keyboard shortcuts: where each
//! output channel keeps its volume and mute, the classic Master, and ChatMix.
//!
//! GG's classic Master has no setting of its own in the audio module: it is a group fader that
//! rescales the channels' volumes, `new = old / old master × new master`
//! (`SonarDeviceVolumeService.SetClassicMasterVolume`), and its mute mutes the channels and gives
//! them back their own mutes afterwards. Its position therefore lives in
//! `%USERPROFILE%\Platinum\window.ini`, written when the user moves the Master in the window or with
//! a shortcut (the daemon then rewrites it, and the window reads it again).
//!
//! ChatMix follows `ChatMixService.SetChatmix`: the balance b in -1..1 becomes
//! s = sign(b)·2|b|/(|b|+1), Game gain = 1 - s and Chat gain = 1 + s, each kept in 0..1; left (-1)
//! is the Game side and turns Chat down, right (+1) turns Game down.
//!
//! In streamer mode (`streamer`) every output channel has two volumes: the personal mix's, which is
//! the one the classic mixer has always used (212, Chat 273), and the stream mix's (214, Chat 275),
//! each with its mute. GG's streamer masters are the same group fader over each (`SetStreamerMasterVolume`):
//! the classic Master is therefore also the Personal master, and `Master::stream` is the Stream
//! master, saved in window.ini next to it.

use crate::apo::{Channel, Store, Value};
use crate::i18n::tr;
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = "window.ini";
/// The output channels under the Master: Gaming, Chat, Media, Aux.
pub const STRIPS: usize = 4;
/// The strips' channels, in the mixer's order.
pub const STRIP_CHANNELS: [Channel; STRIPS] = [Channel::Game, Channel::Chat, Channel::Media, Channel::Aux];
/// Every channel with an equalizer: the four strips, then the microphone.
pub const CHANNEL_COUNT: usize = 5;
/// Volumes closer than this are the same volume (the module stores 32-bit floats).
const SAME: f32 = 0.002;
/// Volume and mute of Gaming, Media and Aux.
pub const RENDER_VOLUME: (i32, i32) = (212, 213);
/// The same for Chat, under the capture numbering.
pub const CHAT_VOLUME: (i32, i32) = (273, 274);
/// The stream mix's volume and mute of Gaming, Media and Aux (the second redirection).
pub const RENDER_STREAM_VOLUME: (i32, i32) = (214, 215);
/// The same for Chat.
pub const CHAT_STREAM_VOLUME: (i32, i32) = (275, 276);
/// ChatMix gains: on the Game channel's store and on the Chat channel's store.
pub const CHATMIX_GAME: i32 = 183;
pub const CHATMIX_CHAT: i32 = 184;

/// The two mixes of GG's streamer mode: what the user hears (Personal, GG's "monitoring") and what
/// the audience hears (Stream). Outside streamer mode only the personal mix is used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mix {
    Personal,
    Stream,
}

impl Mix {
    pub const ALL: [Mix; 2] = [Mix::Personal, Mix::Stream];
}

/// The (volume, mute) settings of an output channel; `None` for the microphone, whose volume is
/// the Windows volume of the virtual microphone.
pub fn volume_settings(channel: Channel) -> Option<(i32, i32)> {
    mix_volume_settings(channel, Mix::Personal)
}

/// The (volume, mute) settings of an output channel in one mix; `None` for the microphone.
pub fn mix_volume_settings(channel: Channel, mix: Mix) -> Option<(i32, i32)> {
    match (channel, mix) {
        (Channel::Game | Channel::Media | Channel::Aux, Mix::Personal) => Some(RENDER_VOLUME),
        (Channel::Game | Channel::Media | Channel::Aux, Mix::Stream) => Some(RENDER_STREAM_VOLUME),
        (Channel::Chat, Mix::Personal) => Some(CHAT_VOLUME),
        (Channel::Chat, Mix::Stream) => Some(CHAT_STREAM_VOLUME),
        (Channel::Mic, _) => None,
    }
}

/// GG's group fader: it rescales the channels' volumes and keeps their shares
/// (`SonarDeviceVolumeService`); a channel moved above it raises it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Group {
    /// The group fader, 0..1.
    pub volume: f32,
    /// Each channel's volume as a share of the fader: channel = ratio × fader. Kept so that a fader
    /// brought down to 0 can bring every channel back where it was.
    pub ratios: [f32; STRIPS],
    pub muted: bool,
    /// The channels' own mutes before the fader muted them all, given back when it unmutes.
    pub saved_mutes: [bool; STRIPS],
}

impl Default for Group {
    fn default() -> Group {
        Group { volume: 1.0, ratios: [1.0; STRIPS], muted: false, saved_mutes: [false; STRIPS] }
    }
}

impl Group {
    /// The channel volumes for the fader at its current position.
    pub fn volumes(&self) -> [f32; STRIPS] {
        self.ratios.map(|ratio| (ratio * self.volume).clamp(0.0, 1.0))
    }


    /// Moves the fader: returns the channel volumes that keep their ratios.
    pub fn set_volume(&mut self, volume: f32) -> [f32; STRIPS] {
        self.volume = volume.clamp(0.0, 1.0);
        self.volumes()
    }

    /// A channel moved on its own. Its ratio follows; moved above the fader, it raises the fader
    /// to its level, as GG's streamer master does, and the other ratios follow.
    pub fn channel_moved(&mut self, index: usize, volume: f32, volumes: &[f32; STRIPS]) {
        let mut volumes = *volumes;
        if let Some(slot) = volumes.get_mut(index) {
            *slot = volume.clamp(0.0, 1.0);
        }
        self.take(&volumes);
    }

    /// Takes in the volumes the channels hold now (a preset, GG or the command line may have
    /// changed one): ratios that no longer match are worked out again, and a channel above the
    /// fader raises it. Returns whether anything changed. Writes nothing anywhere.
    pub fn take(&mut self, volumes: &[f32; STRIPS]) -> bool {
        let before = *self;
        let loudest = volumes.iter().copied().fold(0.0_f32, f32::max);
        if loudest > self.volume + SAME {
            self.volume = loudest.min(1.0);
        }
        if self.volume > SAME {
            for (ratio, volume) in self.ratios.iter_mut().zip(volumes) {
                if (*ratio * self.volume - volume).abs() > SAME {
                    *ratio = (volume / self.volume).clamp(0.0, 1.0);
                }
            }
        }
        *self != before
    }

    /// Mutes every channel: remembers their own mutes, returns the mutes to write.
    pub fn mute(&mut self, mutes: &[bool; STRIPS]) -> [bool; STRIPS] {
        if !self.muted {
            self.saved_mutes = *mutes;
        }
        self.muted = true;
        [true; STRIPS]
    }

    /// Unmutes the fader: returns the channels' own mutes, as they were before.
    pub fn unmute(&mut self) -> [bool; STRIPS] {
        self.muted = false;
        self.saved_mutes
    }

    /// Takes in the channels' mutes: a channel heard again (unmuted by hand, or by another program)
    /// ends the fader's mute. Returns whether that changed it.
    pub fn take_mutes(&mut self, mutes: &[bool; STRIPS]) -> bool {
        if self.muted && mutes.iter().any(|muted| !muted) {
            self.muted = false;
            return true;
        }
        false
    }
}

/// window.ini: the classic Master (also streamer mode's Personal master: both rescale the personal
/// volumes, 212 and 273), the Stream master, and the equalizers' zone labels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Master {
    /// The group fader, 0..1.
    pub volume: f32,
    /// Each channel's volume as a share of the Master: channel = ratio × Master. Kept so that a
    /// Master brought down to 0 can bring every channel back where it was.
    pub ratios: [f32; STRIPS],
    pub muted: bool,
    /// The channels' own mutes before the Master muted them all, given back when it unmutes.
    pub saved_mutes: [bool; STRIPS],
    /// Not the Master's, but the window's too: each channel's equalizer shows a voice's zone labels
    /// (true) or GG's default ones (false, GG's choice on every channel until the user picks).
    pub voice_zones: [bool; CHANNEL_COUNT],
    /// Streamer mode's Stream master, over the stream mix's volumes (214 and 275).
    pub stream: Group,
}

impl Default for Master {
    fn default() -> Master {
        Master { volume: 1.0, ratios: [1.0; STRIPS], muted: false, saved_mutes: [false; STRIPS], voice_zones: [false; CHANNEL_COUNT], stream: Group::default() }
    }
}

impl Master {
    /// The Master's own fader (the personal mix's) as a group.
    pub fn personal(&self) -> Group {
        Group { volume: self.volume, ratios: self.ratios, muted: self.muted, saved_mutes: self.saved_mutes }
    }

    /// One mix's group fader.
    pub fn group(&self, mix: Mix) -> Group {
        match mix {
            Mix::Personal => self.personal(),
            Mix::Stream => self.stream,
        }
    }

    /// Runs `change` on one mix's group fader and keeps what it did.
    pub fn with_group<R>(&mut self, mix: Mix, change: impl FnOnce(&mut Group) -> R) -> R {
        match mix {
            Mix::Stream => change(&mut self.stream),
            Mix::Personal => {
                let mut group = self.personal();
                let result = change(&mut group);
                (self.volume, self.ratios, self.muted, self.saved_mutes) = (group.volume, group.ratios, group.muted, group.saved_mutes);
                result
            }
        }
    }

    /// The channel volumes for the Master at its current position.
    pub fn volumes(&self) -> [f32; STRIPS] {
        self.personal().volumes()
    }

    /// Moves the Master: returns the channel volumes that keep their ratios.
    pub fn set_volume(&mut self, volume: f32) -> [f32; STRIPS] {
        self.with_group(Mix::Personal, |group| group.set_volume(volume))
    }

    /// A channel moved on its own (see `Group::channel_moved`).
    pub fn channel_moved(&mut self, index: usize, volume: f32, volumes: &[f32; STRIPS]) {
        self.with_group(Mix::Personal, |group| group.channel_moved(index, volume, volumes));
    }

    /// Takes in the volumes the channels hold now (see `Group::take`).
    pub fn take(&mut self, volumes: &[f32; STRIPS]) -> bool {
        self.with_group(Mix::Personal, |group| group.take(volumes))
    }

    /// Mutes every channel: remembers their own mutes, returns the mutes to write.
    pub fn mute(&mut self, mutes: &[bool; STRIPS]) -> [bool; STRIPS] {
        self.with_group(Mix::Personal, |group| group.mute(mutes))
    }

    /// Unmutes the Master: returns the channels' own mutes, as they were before.
    pub fn unmute(&mut self) -> [bool; STRIPS] {
        self.with_group(Mix::Personal, Group::unmute)
    }

    /// Takes in the channels' mutes (see `Group::take_mutes`).
    pub fn take_mutes(&mut self, mutes: &[bool; STRIPS]) -> bool {
        self.with_group(Mix::Personal, |group| group.take_mutes(mutes))
    }

    /// The file's text.
    pub fn to_text(&self) -> String {
        let list = |values: &[String]| values.join(", ");
        format!(
            "# Platinum : état du Master, réécrit par la fenêtre ou par un raccourci clavier quand il change.\n\
             # Master = volume général (aussi celui du mix personnel en mode streamer) ; ratios = part de chaque\n\
             # canal (Gaming, Chat, Media, Aux). stream_* : le Master du mix de stream, en mode streamer.\n\
             # voice_zones = zones de l'égaliseur d'une voix (true) ou par défaut, par canal (Gaming à Micro).\n\
             master = {}\nratios = {}\nmaster_muted = {}\nsaved_mutes = {}\nvoice_zones = {}\n\
             stream_master = {}\nstream_ratios = {}\nstream_master_muted = {}\nstream_saved_mutes = {}\n",
            self.volume,
            list(&self.ratios.map(|r| r.to_string())),
            self.muted,
            list(&self.saved_mutes.map(|m| m.to_string())),
            list(&self.voice_zones.map(|z| z.to_string())),
            self.stream.volume,
            list(&self.stream.ratios.map(|r| r.to_string())),
            self.stream.muted,
            list(&self.stream.saved_mutes.map(|m| m.to_string())),
        )
    }

    /// Reads the file's text; what is missing or unreadable keeps its default.
    pub fn parse(text: &str) -> Master {
        let mut master = Master::default();
        for line in text.trim_start_matches('\u{feff}').lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim();
            let key = key.trim();
            let (mix, field) = match key.strip_prefix("stream_") {
                Some(field) => (Mix::Stream, field),
                None => (Mix::Personal, key),
            };
            match field {
                "master" => {
                    if let Some(volume) = number(value) {
                        master.with_group(mix, |group| group.volume = volume);
                    }
                }
                "ratios" => {
                    if let Some(ratios) = list(value, number) {
                        master.with_group(mix, |group| group.ratios = ratios);
                    }
                }
                "master_muted" => master.with_group(mix, |group| group.muted = value == "true"),
                "saved_mutes" => {
                    if let Some(mutes) = list(value, |v| Some(v == "true")) {
                        master.with_group(mix, |group| group.saved_mutes = mutes);
                    }
                }
                "voice_zones" if mix == Mix::Personal => {
                    if let Some(zones) = list(value, |v| Some(v == "true")) {
                        master.voice_zones = zones;
                    }
                }
                _ => {}
            }
        }
        master
    }
}

fn number(text: &str) -> Option<f32> {
    text.trim().parse::<f32>().ok().filter(|v| v.is_finite()).map(|v| v.clamp(0.0, 1.0))
}

/// Exactly `N` comma-separated values.
fn list<T: Copy + Default, const N: usize>(text: &str, read: impl Fn(&str) -> Option<T>) -> Option<[T; N]> {
    let values: Vec<T> = text.split(',').map(|v| read(v.trim())).collect::<Option<_>>()?;
    values.try_into().ok()
}

/// `%USERPROFILE%\Platinum\window.ini`.
pub fn master_path() -> Result<PathBuf, String> {
    Ok(crate::autostart::install_dir()?.join(FILE_NAME))
}

/// The Master as last saved; the defaults (Master at 100 %, nothing muted) without a file.
pub fn load_master() -> Master {
    master_path().ok().and_then(|file| load_master_from(&file)).unwrap_or_default()
}

pub fn load_master_from(file: &Path) -> Option<Master> {
    let bytes = std::fs::read(file).ok()?;
    Some(Master::parse(&String::from_utf8_lossy(&bytes)))
}

/// When window.ini was last written (by the window or the daemon); `None` without a file. The
/// window reads the Master again when this changes, so that a shortcut's move shows up.
pub fn master_stamp() -> Option<std::time::SystemTime> {
    master_path().ok().and_then(|file| std::fs::metadata(file).ok()).and_then(|meta| meta.modified().ok())
}

pub fn save_master(master: &Master) -> Result<(), String> {
    save_master_to(&master_path()?, master)
}

pub fn save_master_to(file: &Path, master: &Master) -> Result<(), String> {
    crate::config::write_atomically(file, &master.to_text())
}

/// ChatMix: the Game and Chat gains for a balance in -1..1 (GG's curve).
pub fn chatmix_gains(balance: f32) -> (f32, f32) {
    let b = if balance.is_finite() { balance.clamp(-1.0, 1.0) } else { 0.0 };
    let scaled = if b == 0.0 { 0.0 } else { b.signum() * 2.0 * b.abs() / (b.abs() + 1.0) };
    ((1.0 - scaled).clamp(0.0, 1.0), (1.0 + scaled).clamp(0.0, 1.0))
}

/// ChatMix read back: the balance the two gains stand for (GG's `ToLinearBalance` and `ToLinear`).
pub fn chatmix_balance(game: f32, chat: f32) -> f32 {
    let scaled = if game < 1.0 { 1.0 - game } else { chat.min(1.0) - 1.0 };
    let scaled = scaled.clamp(-1.0, 1.0);
    scaled / (2.0 - scaled.abs())
}

/// Why ChatMix cannot mix Gaming and Chat, in GG's words (`chatmix.differentDeviceSelected`,
/// `chatmix.noDeviceSelected`); empty when they play to the same device.
pub fn chatmix_reason(game: Option<&str>, chat: Option<&str>) -> &'static str {
    match (game, chat) {
        (Some(game), Some(chat)) if game == chat => "",
        (Some(_), Some(_)) => tr("Veuillez sélectionner le même périphérique de lecture pour Game et Chat afin d'utiliser le ChatMix"),
        _ => tr("Veuillez sélectionner un Périphérique de lecture pour utiliser ChatMix"),
    }
}

/// A numeric setting as (value, minimum, maximum).
fn setting_number(store: &Store, id: i32) -> Result<(f32, f32, f32), String> {
    store.setting(id).and_then(|s| s.number()).map_err(|e| e.to_string())?.ok_or_else(|| format!("setting {id} is not a number"))
}

/// An output channel's volume, 0..1 of its setting's range.
pub fn read_volume(store: &Store) -> Result<f32, String> {
    read_mix_volume(store, Mix::Personal)
}

/// Writes an output channel's volume, 0..1 of its setting's range, as the window does.
pub fn write_volume(store: &Store, fraction: f32) -> Result<(), String> {
    write_mix_volume(store, Mix::Personal, fraction)
}

pub fn read_muted(store: &Store) -> Result<bool, String> {
    read_mix_muted(store, Mix::Personal)
}

pub fn write_muted(store: &Store, on: bool) -> Result<(), String> {
    write_mix_muted(store, Mix::Personal, on)
}

/// An output channel's volume in one mix, 0..1 of its setting's range.
pub fn read_mix_volume(store: &Store, mix: Mix) -> Result<f32, String> {
    let (volume, _) = mix_volume_settings(store.channel, mix).ok_or("this channel has no volume setting")?;
    let (value, minimum, maximum) = setting_number(store, volume)?;
    Ok(if maximum > minimum { ((value - minimum) / (maximum - minimum)).clamp(0.0, 1.0) } else { value })
}

/// Writes an output channel's volume in one mix, 0..1 of its setting's range.
pub fn write_mix_volume(store: &Store, mix: Mix, fraction: f32) -> Result<(), String> {
    let (volume, _) = mix_volume_settings(store.channel, mix).ok_or("this channel has no volume setting")?;
    let (_, minimum, maximum) = setting_number(store, volume)?;
    let value = (minimum + fraction.clamp(0.0, 1.0) * (maximum - minimum)).clamp(minimum, maximum);
    store.setting(volume).and_then(|s| s.set_from_text(&value.to_string())).map_err(|e| e.to_string())
}

pub fn read_mix_muted(store: &Store, mix: Mix) -> Result<bool, String> {
    let (_, mute) = mix_volume_settings(store.channel, mix).ok_or("this channel has no mute setting")?;
    match store.setting(mute).and_then(|s| s.value()).map_err(|e| e.to_string())? {
        Value::Bool(on) => Ok(on),
        other => Err(format!("unexpected mute value {other}")),
    }
}

pub fn write_mix_muted(store: &Store, mix: Mix, on: bool) -> Result<(), String> {
    let (_, mute) = mix_volume_settings(store.channel, mix).ok_or("this channel has no mute setting")?;
    store.setting(mute).and_then(|s| s.set_from_text(if on { "true" } else { "false" })).map_err(|e| e.to_string())
}

/// One ChatMix gain as the store holds it (1, both channels heard, when it cannot be read).
pub fn read_gain(store: &Store, id: i32) -> f32 {
    setting_number(store, id).map_or(1.0, |(value, _, _)| value)
}

/// Writes one ChatMix gain.
pub fn write_gain(store: &Store, id: i32, gain: f32) -> Result<(), String> {
    store.setting(id).and_then(|s| s.set_from_text(&gain.clamp(0.0, 1.0).to_string())).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn chatmix_follows_ggs_curve_and_sides() {
        assert_eq!(chatmix_gains(0.0), (1.0, 1.0));
        // Right (+1) is the Chat side: Game goes down, (1-b)/(1+b).
        let (game, chat) = chatmix_gains(0.5);
        assert!(close(game, 1.0 / 3.0) && chat == 1.0, "{game} {chat}");
        let (game, chat) = chatmix_gains(-0.5);
        assert!(game == 1.0 && close(chat, 1.0 / 3.0), "{game} {chat}");
        assert_eq!(chatmix_gains(1.0), (0.0, 1.0));
        assert_eq!(chatmix_gains(-1.0), (1.0, 0.0));
        assert_eq!(chatmix_gains(f32::NAN), (1.0, 1.0));
        for step in -20..=20 {
            let b = step as f32 / 20.0;
            let (game, chat) = chatmix_gains(b);
            assert!(close(chatmix_balance(game, chat), b), "{b}");
        }
    }

    #[test]
    fn chatmix_needs_gaming_and_chat_on_one_device() {
        assert_eq!(chatmix_reason(Some("a"), Some("a")), "");
        assert!(chatmix_reason(Some("a"), Some("b")).contains("même périphérique"));
        assert!(chatmix_reason(None, Some("b")).contains("Périphérique de lecture"));
    }

    #[test]
    fn the_master_keeps_the_ratios() {
        let mut master = Master::default();
        assert!(master.take(&[0.25, 1.0, 0.5, 1.0]));
        assert_eq!((master.volume, master.ratios), (1.0, [0.25, 1.0, 0.5, 1.0]));
        let volumes = master.set_volume(0.5);
        assert_eq!(volumes, [0.125, 0.5, 0.25, 0.5]);
        // Down to 0 and back: every channel comes back where it was.
        assert_eq!(master.set_volume(0.0), [0.0; STRIPS]);
        assert!(!master.take(&[0.0; STRIPS]));
        assert_eq!(master.set_volume(1.0), [0.25, 1.0, 0.5, 1.0]);
    }

    #[test]
    fn a_channel_above_the_master_raises_it() {
        let mut master = Master { volume: 0.5, ratios: [1.0, 0.5, 0.5, 0.5], ..Master::default() };
        let volumes = master.volumes();
        master.channel_moved(1, 0.8, &volumes);
        assert!(close(master.volume, 0.8));
        assert!(close(master.ratios[0], 0.5 / 0.8) && close(master.ratios[1], 1.0), "{:?}", master.ratios);
        // Below the Master, only its own ratio changes.
        master.channel_moved(2, 0.4, &master.volumes());
        assert!(close(master.volume, 0.8) && close(master.ratios[2], 0.5));
        // A change made elsewhere is taken in the same way, without writing anything.
        assert!(master.take(&[0.5, 0.8, 0.4, 0.1]));
        assert!(close(master.ratios[3], 0.125));
        assert!(!master.take(&master.volumes()));
    }

    #[test]
    fn the_master_mute_gives_the_channels_their_own_mutes_back() {
        let mut master = Master::default();
        assert_eq!(master.mute(&[false, true, false, false]), [true; STRIPS]);
        // Muting again keeps what was saved the first time.
        master.mute(&[true; STRIPS]);
        assert_eq!(master.unmute(), [false, true, false, false]);
        master.mute(&[false; STRIPS]);
        assert!(!master.take_mutes(&[true; STRIPS]));
        assert!(master.take_mutes(&[true, false, true, true]) && !master.muted);
    }

    #[test]
    fn the_stream_master_is_a_group_of_its_own() {
        let mut master = Master::default();
        // The stream mix's fader moves without touching the Master's.
        master.with_group(Mix::Stream, |group| group.take(&[0.5, 1.0, 0.25, 0.0]));
        assert_eq!(master.with_group(Mix::Stream, |group| group.set_volume(0.5)), [0.25, 0.5, 0.125, 0.0]);
        assert_eq!((master.volume, master.ratios), (1.0, [1.0; STRIPS]));
        assert_eq!(master.group(Mix::Stream).volume, 0.5);
        // And the Master's own moves leave the stream's alone.
        master.set_volume(0.2);
        assert_eq!(master.group(Mix::Personal).volume, 0.2);
        assert_eq!(master.stream.volume, 0.5);
        master.with_group(Mix::Stream, |group| group.mute(&[false; STRIPS]));
        assert!(master.stream.muted && !master.muted);
        assert_eq!(mix_volume_settings(Channel::Aux, Mix::Stream), Some((214, 215)));
        assert_eq!(mix_volume_settings(Channel::Chat, Mix::Stream), Some((275, 276)));
        assert_eq!(mix_volume_settings(Channel::Mic, Mix::Stream), None);
    }

    #[test]
    fn the_file_survives_a_round_trip() {
        let stream = Group { volume: 0.7, ratios: [1.0, 0.5, 0.25, 0.125], muted: true, saved_mutes: [true, false, false, true] };
        let master = Master { volume: 0.4, ratios: [0.25, 1.0, 0.5, 0.75], muted: true, saved_mutes: [false, true, false, true], voice_zones: [false, true, false, false, true], stream };
        assert_eq!(Master::parse(&master.to_text()), master);
        // A window.ini written before streamer mode keeps its Master and gets the default stream one.
        let old = "master = 0.4\nratios = 0.25, 1, 0.5, 0.75\nmaster_muted = false\nsaved_mutes = false, false, false, false\nvoice_zones = false, false, false, false, false\n";
        assert_eq!(Master::parse(old), Master { volume: 0.4, ratios: [0.25, 1.0, 0.5, 0.75], ..Master::default() });
        assert_eq!(Master::parse("master = 7\nratios = 1, 2\nmaster_muted = maybe\n"), Master { volume: 1.0, ..Master::default() });
        let folder = std::env::temp_dir().join(format!("platinum-window-test-{}", std::process::id()));
        let file = folder.join(FILE_NAME);
        save_master_to(&file, &master).unwrap();
        assert_eq!(load_master_from(&file), Some(master));
        assert_eq!(load_master_from(&folder.join("missing.ini")), None);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn every_output_channel_has_its_volume_and_mute() {
        assert_eq!(volume_settings(Channel::Game), Some((212, 213)));
        assert_eq!(volume_settings(Channel::Chat), Some((273, 274)));
        assert_eq!(volume_settings(Channel::Mic), None);
        assert!(STRIP_CHANNELS.iter().zip(crate::outputs::CHANNELS).all(|(a, (b, _))| *a == b));
    }
}
