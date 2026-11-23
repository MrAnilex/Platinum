//! The window's engine: reads the channels' settings, the devices, the applications and the daemon,
//! and carries out what the window asks for. It runs on a thread of its own because the audio
//! module's objects want COM in multithreaded mode, which the window's thread cannot have.
//!
//! Only the user's actions write. A snapshot reads; opening the window reads (and may start the
//! daemon, when the configuration asks for it). In read-only mode (the layout check,
//! `PLATINUM_UI_SNAPSHOT`) every command is counted and dropped: nothing is written, started or
//! stopped.
//!
//! Every snapshot carries the number of commands handled before it was taken, so that the window
//! can tell a snapshot that already includes the user's last move from one that would undo it on
//! screen for a moment.
//!
//! The effects are presets: each channel has a current preset (`catalog::Presets`, saved in
//! state.json), and every change of an effect card becomes the writes that turn the current preset
//! into the new one (`preset::plan`, then `preset::apply`), so that GG's rules hold on every edit.
//! A preset chosen, reset or imported is compared with what the module holds instead, setting by
//! setting, as GG does (B17): a module changed behind Platinum's back is then set right. Each
//! snapshot reads the module back into the current preset, so that the window never shows what the
//! module does not hold.
//!
//! Streamer mode (`platinum::streamer`) is switched here, on the user's word only: entering it
//! remembers the classic outputs and puts every channel in the mixes platinum.ini names, leaving it
//! takes them out of the stream mix and gives them their classic outputs back. The Stream master is
//! the Master's twin over the stream volumes, saved in window.ini with it.
//!
//! The preset per game is the daemon's (`platinum::games`): each snapshot reads its file, and while
//! a game's preset is on Gaming, Gaming's current preset is that one (edits go to it); choosing
//! another preset ends it, which state.json records. The window writes the user's links of
//! presets to games (game-links.txt). GG's Arena 7 rule runs on every preset applied whole and
//! after every change of output made here.

use crate::background::{self, DaemonView, Kind, SharedJob};
use crate::catalog::{Catalog, Presets};
use crate::persist::Edited;
use crate::presets::{self, Edit, PresetRef, ROLES};
use crate::settings::{self, Change, DeviceMenu, StartSettings};
use platinum::apo::{Channel, Store, Stores, Value};
use platinum::devices::{Devices, Endpoint, Flow, Role, State};
use platinum::fallback::{self, PriorityDevice};
use platinum::hotkeys::{Action, Hotkey};
use platinum::i18n::{mark, tr};
use platinum::mixer::{self, CHAT_VOLUME, CHATMIX_CHAT, CHATMIX_GAME, Master, Mix, RENDER_VOLUME, STRIPS, chatmix_reason};
use platinum::overlay;
use platinum::outputs::{self, CHAT_OUTPUT, RENDER_OUTPUT, StreamReference};
use platinum::preset::{self, Preset};
use platinum::streamer::{self, Inclusion, Mode, StreamDevice};
use platinum::wasapi::EndpointVolume;
use platinum::{autostart, config, daemon, guard, names, routing, session, status, trf};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub enum Command {
    SetVolume(String, f32),
    SetMuted(String, bool),
    /// Channel, then the real output's identifier (`None`: no output).
    SetOutput(String, Option<String>),
    /// GG's classic Master: rescales the four output channels, keeping their ratios.
    SetMasterVolume(f32),
    SetMasterMuted(bool),
    /// The same real output for every output channel.
    SetMasterOutput(Option<String>),
    /// Windows volume of the real output the channels play to (GG's "level" box).
    SetDeviceLevel(f32),
    /// Windows volume and mute of the virtual microphone, as applications record it.
    SetMicVolume(f32),
    SetMicMuted(bool),
    /// -1 (Game side: Chat turned down) .. 0 (both) .. 1 (Chat side: Game turned down).
    SetChatMix(f32),
    /// A change of an effect card (the "Effets" switch included) on a channel's current preset.
    Edit(String, Edit),
    /// Applies a preset to a channel (only the settings that change are written).
    SelectPreset(String, PresetRef),
    /// Back to the chosen preset's own settings (GG's "Réinitialiser").
    ResetPreset(String),
    /// Saves the channel's settings into the user's preset it uses.
    SavePreset(String),
    /// Saves the channel's settings as a new preset of the user's, then uses it.
    SavePresetAs(String, String),
    RenamePreset(String, String),
    DeletePreset(String),
    ToggleFavorite(String, PresetRef),
    /// A preset file read by the window (its file name without extension, then its text): added
    /// to the user's presets, then applied.
    ImportPreset(String, String, String),
    /// Application, destination (0: the default device, 1 to 4: Gaming, Chat, Media, Aux).
    Route(String, usize),
    /// A recording application, destination (0: the default microphone, 1: the virtual one).
    RouteCapture(String, usize),
    /// Puts the microphone's echo cancellation back on the outputs the channels play to.
    SyncEcho,
    /// A favourite moved to another place of the bar: channel, preset, new place.
    MoveFavorite(String, PresetRef, usize),
    /// The zone labels above a channel's equalizer: a voice's (true) or GG's default ones.
    SetVoiceZones(String, bool),
    StartDaemon,
    StopDaemon,
    /// The real microphone the daemon relays, by endpoint identifier: saved in the configuration,
    /// then the daemon restarts if it relays one already.
    ChooseMicrophone(String),
    /// GG's automatic backup device switch on or off (an empty output list is first seeded with
    /// the devices the channels play to, so that turning it on moves nothing).
    SetAutoSwitch(bool),
    /// A priority list as the settings page left it: the outputs' (`Flow::Render`) or the
    /// microphones'.
    SetPriority(Flow, Vec<PriorityDevice>),
    /// An option of platinum.ini changed on the settings page (`settings::Change`).
    Set(Change),
    SetStartWithWindows(bool),
    /// The channels' names in Windows: Platinum's (`true`) or those they had before
    /// (`platinum::names`).
    SetChannelNames(bool),
    /// Stops the daemon and starts it again, for the options it reads only when it starts; asked
    /// by the user once he was told that the sound cuts for a few seconds.
    RestartDaemon,
    /// A keyboard shortcut bound to a combination, or unbound (`None`); the daemon reads
    /// platinum.ini again within two seconds.
    SetShortcut(Action, Option<Hotkey>),
    /// The on-screen overlays' options.
    SetOverlays(overlay::Settings),
    /// The classic mixer or GG's streamer mode (see `platinum::streamer`).
    SetMode(Mode),
    /// A channel's volume and mute in the stream mix.
    SetStreamVolume(String, f32),
    SetStreamMuted(String, bool),
    /// The Stream master: rescales the stream volumes, keeping their ratios; its mute.
    SetStreamMasterVolume(f32),
    SetStreamMasterMuted(bool),
    /// A channel in or out of a mix (streamer mode; remembered in platinum.ini in any mode).
    SetInclusion(String, Mix, bool),
    /// The personal mix's device (`None`: none), for every channel in it.
    SetPersonalDevice(Option<String>),
    /// The stream mix's device, for every channel in it.
    SetStreamDevice(StreamDevice),
    /// Windows volume of the stream mix's device (GG's level box next to it).
    SetStreamDeviceLevel(f32),
    /// A preset of Gaming's linked to a program (executable, then the title shown for it), in
    /// game-links.txt; the daemon reads it within two seconds.
    LinkGame(PresetRef, String, String),
    /// A program's link removed, by executable.
    UnlinkGame(String),
    /// Removes one notification.
    Dismiss(u64),
    /// A notification from the window itself (an export that failed): what, then why.
    Notify(String, String),
}

pub struct ChannelDef {
    pub key: &'static str,
    pub title: &'static str,
    pub icon: &'static str,
    channel: Channel,
    volume: Option<i32>,
    muted: Option<i32>,
    /// (routing enabled, device identifier)
    output: Option<(i32, i32)>,
    /// The channel kind of its virtual device, as `session::virtual_kind` names it.
    pub meter: &'static str,
}

pub static CHANNELS: [ChannelDef; 5] = [
    ChannelDef { key: "game", title: mark("Gaming"), icon: "game", channel: Channel::Game, volume: Some(RENDER_VOLUME.0), muted: Some(RENDER_VOLUME.1), output: Some(RENDER_OUTPUT), meter: "gaming" },
    ChannelDef { key: "chat", title: mark("Chat"), icon: "chat", channel: Channel::Chat, volume: Some(CHAT_VOLUME.0), muted: Some(CHAT_VOLUME.1), output: Some(CHAT_OUTPUT), meter: "chat" },
    ChannelDef { key: "media", title: mark("Media"), icon: "media", channel: Channel::Media, volume: Some(RENDER_VOLUME.0), muted: Some(RENDER_VOLUME.1), output: Some(RENDER_OUTPUT), meter: "media" },
    ChannelDef { key: "aux", title: mark("Aux"), icon: "aux", channel: Channel::Aux, volume: Some(RENDER_VOLUME.0), muted: Some(RENDER_VOLUME.1), output: Some(RENDER_OUTPUT), meter: "aux" },
    ChannelDef { key: "mic", title: mark("Micro"), icon: "mic", channel: Channel::Mic, volume: None, muted: None, output: None, meter: "microphone" },
];

/// Position of the microphone in `CHANNELS`.
pub const MIC: usize = 4;
/// Programs that are not applications to route: Platinum's own, Windows' audio engine, Sunshine
/// (its session on the streaming output is its recording of it, see `platinum::remote`), and the
/// Windows processes GG leaves out of its list too (`AUDIO_PROCESS_EXCLUSION_LIST`: the sound
/// control panel's test tone, Settings, Search), plus Windows 11's search and shell hosts.
const NOT_APPLICATIONS: [&str; 11] = [
    "platinum.exe",
    "platinum.old.exe",
    "platinum-ui.exe",
    "platinum-start.exe",
    "audiodg.exe",
    "sunshine.exe",
    "rundll32.exe",
    "systemsettings.exe",
    "searchapp.exe",
    "searchhost.exe",
    "shellexperiencehost.exe",
];
/// Applications that open their audio device themselves and may not follow a route: GG's list
/// (`deviceRouting.infoTooltip` is shown on them).
const OWN_DEVICE_APPS: [&str; 4] = ["discord.exe", "ms-teams.exe", "msteams.exe", "itunes.exe"];
/// How long after a route an application's sound should have moved: Windows moves a stream that
/// follows the default device within a second; past three, it is not going to.
const ROUTE_SETTLE: Duration = Duration::from_secs(3);
/// Past this, an application that played nothing since its route is not checked any more.
const ROUTE_WATCH: Duration = Duration::from_secs(12);
/// The virtual microphone's place among the window's drop targets (after Gaming to Aux).
pub const MIC_TARGET: i32 = 5;
/// At most this many notifications are kept.
const NOTICES: usize = 5;

pub fn find(key: &str) -> Option<&'static ChannelDef> {
    CHANNELS.iter().find(|c| c.key == key)
}

fn position(key: &str) -> Option<usize> {
    CHANNELS.iter().position(|c| c.key == key)
}

fn index_of(key: &str) -> Result<usize, String> {
    position(key).ok_or_else(|| format!("canal inconnu : {key}"))
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChannelData {
    /// Its store could be read. When false, every other field is a placeholder and the window says
    /// the settings are unreadable instead of showing them.
    pub readable: bool,
    /// 0..1 of the volume setting's range: the classic volume, which is also the personal mix's.
    pub volume: f32,
    pub muted: bool,
    /// The stream mix's volume (0..1) and mute, for the output channels.
    pub stream_volume: f32,
    pub stream_muted: bool,
    /// Index in `Snapshot::outputs` (0: no output).
    pub output_index: i32,
    /// That output is plugged in and enabled (a channel can name a device that is unplugged): the
    /// test sounds are only offered when it is, and the microphone test listens on Chat's.
    pub output_present: bool,
    /// The channel's current preset, as the module holds it; `None` while unreadable. Its
    /// `globalEnableState` is the channel's "Effets" switch.
    pub preset: Option<Preset>,
    /// The preset it comes from, and that preset's name when chosen.
    pub selection: PresetRef,
    pub selection_name: String,
    /// What the module holds in the classic and impact noise reductions' switches (capture only).
    /// AI noise cancelling should hold both off (B1), but a module changed one setting at a time
    /// can hold them on: the card shows what is heard.
    pub held_reductions: (bool, bool),
    /// Plays to SteelSeries Arena 7 speakers: GG's speaker rule for spatial audio applies.
    pub arena: bool,
    /// The user's edits of the channel's presets (see `catalog::Presets::edited`).
    pub edited: Edited,
    /// Gaming only: the game whose preset the daemon put on the channel ("Activé par <jeu>");
    /// empty when none.
    pub game: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppData {
    pub exe: String,
    pub playing: bool,
    /// Records from a microphone (listed under the Micro strip or the Master), instead of playing.
    pub capture: bool,
    /// 0: plays straight to a real device (listed under the Master); 1 to 4: through Gaming,
    /// Chat, Media, Aux; `MIC_TARGET`: records the virtual microphone.
    pub channel_index: i32,
    /// It opens its device itself (GG's list): a route may not reach its sound.
    pub own_device: bool,
    /// Its sound did not follow the last route the window made.
    pub refused: bool,
    /// Has no device of its own in Windows: it plays to the default device.
    pub follows_default: bool,
    /// The real device, for an application that goes around the channels.
    pub device: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MasterData {
    /// Index in `Snapshot::outputs` of the device the channels play to; -1 when they play to
    /// several (the Master then names none and leaves the devices' levels alone).
    pub output_index: i32,
    /// The group fader and its mute (Platinum's own state).
    pub fader: Master,
    /// Windows volume of that device; `None` without one.
    pub device_level: Option<f32>,
    pub device_muted: bool,
}

/// A real microphone the daemon could relay.
#[derive(Clone, Debug, PartialEq)]
pub struct Input {
    /// The endpoint's identifier (for a chosen microphone that is missing: what the configuration
    /// names).
    pub id: String,
    pub name: String,
    /// The microphone of a Bluetooth headset: the daemon refuses it (opening it would switch the
    /// headset to its hands-free profile and spoil the music).
    pub hands_free: bool,
    /// Plugged in and enabled (a chosen microphone that is missing is listed too).
    pub present: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MicData {
    pub inputs: Vec<Input>,
    /// Index in `inputs` of the microphone relayed or chosen; -1 when none is.
    pub current: i32,
    /// Windows volume of the virtual microphone; `None` while it is not there (daemon stopped).
    pub volume: Option<f32>,
    pub muted: bool,
}

/// Streamer mode, as the mixer shows it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamerData {
    pub mode: Mode,
    /// Index in `Snapshot::outputs` of the personal mix's device (0: none).
    pub personal_index: i32,
    /// The stream mix's device menu: "Aucune sortie", the Stream virtual device, then the real
    /// outputs (and the chosen one, when it is unplugged); `choices` says what each line is.
    pub outputs: Vec<String>,
    pub choices: Vec<StreamDevice>,
    /// Index in `outputs` of the stream mix's device.
    pub stream_index: i32,
    /// The channels each mix holds, Gaming to Aux, as platinum.ini keeps them.
    pub personal_mix: Inclusion,
    pub stream_mix: Inclusion,
    /// Windows volume and mute of the stream mix's device (GG's level box next to it); `None`
    /// outside streamer mode or without a device.
    pub level: Option<(f32, bool)>,
}

/// The backup device switch and its priority lists, for the device manager.
#[derive(Clone, Debug, Default)]
pub struct PriorityData {
    pub auto_switch: bool,
    /// The lists as saved in platinum.ini: identifiers, last names seen, exclusions.
    pub output_list: Vec<PriorityDevice>,
    pub input_list: Vec<PriorityDevice>,
    /// The devices the lists can hold, as Windows lists them now (plugged in or not): the real
    /// outputs and microphones, without the Bluetooth hands-free endpoints, which are never chosen.
    pub output_devices: Vec<Endpoint>,
    pub input_devices: Vec<Endpoint>,
    /// The Bluetooth hands-free endpoints, left out of the devices above.
    pub hands_free: Vec<String>,
    /// The devices in use: those the channels play to, the microphone relayed (else chosen).
    pub output_current: Vec<String>,
    pub input_current: Option<String>,
}

/// The daemon and the install folder, for the settings page's "À propos".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct About {
    /// The running daemon's process, as status.txt names it; `None` while none runs.
    pub pid: Option<u32>,
    /// Its private memory and working set, in bytes, read from the window (`guard::process_memory`).
    pub memory: Option<(usize, usize)>,
    /// The microphone worker, which hosts the relay and the effects engine (status.txt's `worker`),
    /// and its memory read the same way; `None` while none runs.
    pub worker_pid: Option<u32>,
    pub worker_memory: Option<(usize, usize)>,
    /// When the daemon last wrote status.txt (its own stamp, "YYYYMMDD-HHMMSS").
    pub updated: String,
    /// The daemon's log, when one is there (next to the running daemon, else in the install folder).
    pub log: Option<std::path::PathBuf>,
    /// The install folder (platinum.ini, status.txt, the logs), when it exists.
    pub folder: Option<std::path::PathBuf>,
}

/// Something the user should know, kept until dismissed.
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub id: u64,
    /// What did not happen, in French.
    pub text: String,
    /// Why, as the system said it (may be technical).
    pub detail: String,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// How many commands the engine had handled when it took this snapshot.
    pub handled: u64,
    pub channels: Vec<ChannelData>,
    /// "Aucune sortie", then the real outputs; `output_ids` holds their identifiers.
    pub outputs: Vec<String>,
    pub output_ids: Vec<Option<String>>,
    pub master: MasterData,
    pub chatmix: f32,
    pub apps: Vec<AppData>,
    pub mic: MicData,
    pub daemon: DaemonView,
    /// platinum.ini as read for this snapshot: the settings page's options, the shortcuts, GG's
    /// Arena 7 rule…
    pub settings: config::Config,
    /// The options changed since the running daemon started, which wait for its next start.
    pub pending_restart: Vec<settings::Setting>,
    /// The channels are to carry Platinum's names in Windows (`channel_names` in platinum.ini).
    pub platinum_names: bool,
    /// The fallback devices' menus (the output, the input made the default when the daemon stops).
    pub fallback_outputs: DeviceMenu,
    pub fallback_inputs: DeviceMenu,
    /// What the "À propos" section shows of the daemon and the install folder.
    pub about: About,
    /// The user's links of Gaming's presets to programs (game-links.txt).
    pub game_links: Vec<platinum::games::UserLink>,
    pub start_with_windows: bool,
    /// Platinum is installed (the logon start can be set).
    pub installed: bool,
    /// Task Manager keeps Platinum from starting with Windows: its Run value is there, disabled.
    pub startup_blocked: bool,
    /// Echo cancellation is on and listens to other outputs than those the channels play to.
    pub echo_stale: bool,
    /// Why ChatMix cannot be used, in GG's words; empty when it can (Gaming and Chat play to the
    /// same device).
    pub chatmix_reason: String,
    /// The channel the default output is (1 to 4), 0 for a real device: where an application sent
    /// back to the default device plays.
    pub default_channel: i32,
    /// The default recording device is the virtual microphone.
    pub default_capture_mic: bool,
    pub priority: PriorityData,
    pub streamer: StreamerData,
    pub notices: Vec<Notice>,
    /// Every channel's presets, shared: rebuilt only when they change.
    pub catalog: Arc<Catalog>,
}

/// Reads one store's settings, keeping the numeric ranges, which never change.
struct Reader<'e> {
    store: &'e Store<'static>,
    index: usize,
    ranges: &'e mut HashMap<(usize, i32), Option<(f32, f32)>>,
}

impl Reader<'_> {
    fn value(&self, id: i32) -> Option<Value> {
        self.store.setting(id).and_then(|s| s.value()).ok()
    }

    fn flag(&self, id: i32) -> bool {
        matches!(self.value(id), Some(Value::Bool(true)))
    }

    fn text(&self, id: i32) -> Option<String> {
        match self.value(id) {
            Some(Value::Text(text)) => Some(text),
            _ => None,
        }
    }

    /// The range of a numeric setting, read once; `None` for settings that are not numbers.
    fn range(&mut self, id: i32) -> Option<(f32, f32)> {
        if let Some(range) = self.ranges.get(&(self.index, id)) {
            return *range;
        }
        let range = self.store.setting(id).and_then(|s| s.number()).ok()?.map(|(_, minimum, maximum)| (minimum, maximum));
        self.ranges.insert((self.index, id), range);
        range
    }

    /// A numeric setting as (value, minimum, maximum).
    fn number(&mut self, id: i32) -> Option<(f32, f32, f32)> {
        let (minimum, maximum) = self.range(id)?;
        let value = match self.value(id)? {
            Value::Float(v) => v,
            Value::Int(v) => v as f32,
            _ => return None,
        };
        Some((value, minimum, maximum))
    }

    /// A numeric setting as 0..1 of its range.
    fn fraction(&mut self, id: i32) -> Option<f32> {
        let (value, minimum, maximum) = self.number(id)?;
        Some(if maximum > minimum { ((value - minimum) / (maximum - minimum)).clamp(0.0, 1.0) } else { value })
    }
}

/// Reads one channel's volume, mute and output (its preset is read apart); also returns the real
/// output it plays to.
fn read_channel(reader: &mut Reader, def: &ChannelDef) -> (ChannelData, Option<String>) {
    let stream = mixer::mix_volume_settings(def.channel, Mix::Stream);
    let data = ChannelData {
        volume: def.volume.and_then(|id| reader.fraction(id)).unwrap_or(1.0),
        muted: def.muted.is_some_and(|id| reader.flag(id)),
        stream_volume: stream.and_then(|(id, _)| reader.fraction(id)).unwrap_or(1.0),
        stream_muted: stream.is_some_and(|(_, id)| reader.flag(id)),
        ..ChannelData::default()
    };
    let output = def.output.filter(|(enabled, _)| reader.flag(*enabled)).and_then(|(_, device)| reader.text(device)).filter(|id| !id.is_empty());
    (data, output)
}

struct Engine {
    /// The settings API and each channel's settings store, opened once for the life of the
    /// window: opening one costs the vendor's DLL memory that closing it does not all give back.
    stores: Stores,
    ranges: HashMap<(usize, i32), Option<(f32, f32)>>,
    devices: Option<Devices>,
    /// The device list as last read.
    endpoints: Vec<Endpoint>,
    /// The device behind each entry of the last output list (None: no output).
    output_ids: Vec<Option<String>>,
    /// The real output the channels play to, shared with the meters thread.
    master_device: Arc<Mutex<Option<String>>>,
    /// The stream mix's device in streamer mode (`None` otherwise), shared with the meters thread.
    stream_device: Arc<Mutex<Option<String>>>,
    /// Layout check only (`PLATINUM_UI_STREAMER=1`): the mixer is shown in streamer mode whatever
    /// platinum.ini says.
    force_streamer: bool,
    master: Master,
    /// The Master changed by the user and not saved yet.
    master_dirty: bool,
    /// When window.ini was last read or written here: the daemon rewrites it when a keyboard
    /// shortcut moves the Master, and the window then reads it again.
    master_stamp: Option<std::time::SystemTime>,
    /// Bluetooth hands-free microphones, by endpoint (a property read, kept).
    hands_free: HashMap<String, bool>,
    /// The channels' presets, state.json and the presets on offer.
    presets: Presets,
    job: SharedJob,
    /// Why the last start asked from the window failed, until the daemon runs again.
    last_error: Option<String>,
    /// The job under way was asked for by the user (a start, a microphone change), not by opening
    /// the window: when it ends, the echo cancellation is set on the outputs heard.
    job_by_user: bool,
    /// A microphone chosen while a start was under way: the daemon may have read the previous one,
    /// so it restarts for this one once the start is over.
    pending_microphone: Option<String>,
    /// The options the running daemon read when it started, by its process (`StartSettings`):
    /// taken when the window first sees that process, so that the settings page can say what
    /// waits for its next start.
    applied: Option<(u32, StartSettings)>,
    /// Which channels play to Arena 7 speakers, as last read.
    arena: [bool; 5],
    /// GG's Arena 7 rule is on (platinum.ini's `arena_speakers`, as the last snapshot read it).
    arena_speakers: bool,
    /// Applications routed lately, to check that their sound followed: executable, channel, when.
    routes_sent: Vec<(String, i32, Instant)>,
    /// Applications whose sound did not follow their last route.
    refused: Vec<String>,
    notices: Vec<Notice>,
    next_notice: u64,
    handled: u64,
    read_only: bool,
}

fn text<E: ToString>(error: E) -> String {
    error.to_string()
}

fn on_off(on: bool) -> &'static str {
    if on { "true" } else { "false" }
}

/// What a command was meant to do, said when it fails.
fn context(command: &Command) -> String {
    let title = |key: &str| find(key).map_or(key.to_string(), |c| tr(c.title).to_string());
    match command {
        Command::SetVolume(key, _) => trf!("Volume de {channel} non réglé", channel = title(key)),
        Command::SetMuted(key, _) => trf!("Sourdine de {channel} non changée", channel = title(key)),
        Command::SetOutput(key, _) => trf!("Sortie de {channel} non changée", channel = title(key)),
        Command::SetMasterVolume(_) => tr("Master non réglé").into(),
        Command::SetMasterMuted(_) => tr("Sourdine du Master non changée").into(),
        Command::SetMasterOutput(_) => tr("Sortie des canaux non changée").into(),
        Command::SetDeviceLevel(_) => tr("Niveau du périphérique non réglé").into(),
        Command::SetMicVolume(_) => tr("Volume du micro virtuel non réglé").into(),
        Command::SetMicMuted(_) => tr("Sourdine du micro virtuel non changée").into(),
        Command::SetChatMix(_) => tr("ChatMix non réglé").into(),
        Command::Edit(key, Edit::Set(presets::Field::GlobalEnable, _)) => trf!("Effets de {channel} non changés", channel = title(key)),
        Command::Edit(key, Edit::Band(..)) => trf!("Égaliseur de {channel} non réglé", channel = title(key)),
        Command::Edit(key, _) => trf!("Réglage de {channel} non écrit", channel = title(key)),
        Command::SelectPreset(key, _) => trf!("Préréglage de {channel} non appliqué", channel = title(key)),
        Command::ResetPreset(key) => trf!("Préréglage de {channel} non réinitialisé", channel = title(key)),
        Command::SavePreset(_) | Command::SavePresetAs(..) => tr("Préréglage non enregistré").into(),
        Command::RenamePreset(..) => tr("Préréglage non renommé").into(),
        Command::DeletePreset(_) => tr("Préréglage non supprimé").into(),
        Command::ToggleFavorite(..) => tr("Favoris non changés").into(),
        Command::ImportPreset(..) => tr("Préréglage non importé").into(),
        Command::Route(exe, _) | Command::RouteCapture(exe, _) => trf!("{app} non transféré", app = exe),
        Command::SyncEcho => tr("Annulation d'écho non mise à jour").into(),
        Command::MoveFavorite(..) => tr("Favoris non changés").into(),
        Command::SetVoiceZones(..) => tr("Zones de l'égaliseur non changées").into(),
        Command::StartDaemon => tr("Platinum n'a pas démarré").into(),
        Command::StopDaemon => tr("Platinum ne s'est pas arrêté").into(),
        Command::ChooseMicrophone(_) => tr("Micro non changé").into(),
        Command::SetAutoSwitch(_) => tr("Changement automatique de périphérique de secours non enregistré").into(),
        Command::SetPriority(..) => tr("Liste de priorité non enregistrée").into(),
        Command::Set(change) => change.failed(),
        Command::SetStartWithWindows(_) => tr("Démarrage avec Windows non changé").into(),
        Command::SetChannelNames(_) => tr("Noms des canaux non changés").into(),
        Command::RestartDaemon => tr("Platinum n'a pas redémarré").into(),
        Command::SetShortcut(..) => tr("Raccourci non enregistré").into(),
        Command::SetOverlays(_) => tr("Options des superpositions non enregistrées").into(),
        Command::SetMode(Mode::Streamer) => tr("Mode streamer non activé").into(),
        Command::SetMode(Mode::Classic) => tr("Mode streamer non désactivé").into(),
        Command::SetStreamVolume(key, _) => trf!("Volume de {channel} dans le mix de stream non réglé", channel = title(key)),
        Command::SetStreamMuted(key, _) => trf!("Sourdine de {channel} dans le mix de stream non changée", channel = title(key)),
        Command::SetStreamMasterVolume(_) => tr("Master du mix de stream non réglé").into(),
        Command::SetStreamMasterMuted(_) => tr("Sourdine du mix de stream non changée").into(),
        Command::SetInclusion(key, Mix::Personal, _) => trf!("{channel} non ajouté au mix personnel ni retiré", channel = title(key)),
        Command::SetInclusion(key, Mix::Stream, _) => trf!("{channel} non ajouté au mix de stream ni retiré", channel = title(key)),
        Command::SetPersonalDevice(_) => tr("Sortie du mix personnel non changée").into(),
        Command::SetStreamDevice(_) => tr("Sortie du mix de stream non changée").into(),
        Command::SetStreamDeviceLevel(_) => tr("Niveau de la sortie du mix de stream non réglé").into(),
        Command::LinkGame(..) => tr("Préréglage non associé au jeu").into(),
        Command::UnlinkGame(_) => tr("Association au jeu non retirée").into(),
        Command::Dismiss(_) | Command::Notify(..) => String::new(),
    }
}

impl Engine {
    fn new(meter_devices: MeterDevices, read_only: bool) -> Engine {
        let mut engine = Engine {
            stores: Stores::load_default(),
            ranges: HashMap::new(),
            devices: None,
            endpoints: Vec::new(),
            output_ids: Vec::new(),
            master_device: meter_devices.master,
            stream_device: meter_devices.stream,
            force_streamer: read_only && std::env::var("PLATINUM_UI_STREAMER").is_ok_and(|v| v == "1"),
            master: mixer::load_master(),
            master_dirty: false,
            master_stamp: mixer::master_stamp(),
            hands_free: HashMap::new(),
            presets: Presets::load(),
            job: SharedJob::default(),
            last_error: None,
            job_by_user: false,
            pending_microphone: None,
            applied: None,
            arena: [false; 5],
            arena_speakers: config::load().arena_speakers,
            routes_sent: Vec::new(),
            refused: Vec::new(),
            notices: Vec::new(),
            next_notice: 1,
            handled: 0,
            read_only,
        };
        if let Some(error) = engine.stores.api_error().map(str::to_string) {
            engine.notify(tr("Les réglages du module audio de SteelSeries sont indisponibles"), error);
        }
        for problem in config::load_full().problems {
            engine.notify(tr("Une ligne de platinum.ini a été ignorée"), problem);
        }
        engine.notify_preset_problems();
        engine
    }

    fn notify_preset_problems(&mut self) {
        for (text, detail) in std::mem::take(&mut self.presets.problems) {
            self.notify(text, detail);
        }
    }

    fn notify(&mut self, text: impl Into<String>, detail: impl Into<String>) {
        let (text, detail) = (text.into(), detail.into());
        if self.notices.iter().any(|n| n.text == text && n.detail == detail) {
            return;
        }
        self.notices.push(Notice { id: self.next_notice, text, detail });
        self.next_notice += 1;
        if self.notices.len() > NOTICES {
            self.notices.remove(0);
        }
    }

    /// Opening the window starts the daemon when the configuration asks for it and nothing else
    /// holds the devices.
    fn start_on_open(&mut self) {
        if self.read_only || daemon::is_running() || daemon::sonar_running() || !config::load().start_on_open {
            return;
        }
        background::begin(&self.job, Kind::Start);
    }

    /// The device list, reopening the device API when Windows' audio service has restarted.
    fn refresh_endpoints(&mut self) {
        if let Some(list) = self.devices.as_ref().and_then(|d| d.list().ok()) {
            self.endpoints = list;
            return;
        }
        self.devices = Devices::open().ok();
        self.endpoints = self.devices.as_ref().and_then(|d| d.list().ok()).unwrap_or_default();
    }

    fn handle(&mut self, command: Command) {
        self.handled += 1;
        if self.read_only {
            return;
        }
        let what = context(&command);
        // A change of the presets themselves is rare and must survive the window closing at once:
        // state.json is saved right away, not with the knobs' changes.
        let structural = matches!(
            command,
            Command::SelectPreset(..) | Command::ResetPreset(_) | Command::SavePreset(_) | Command::SavePresetAs(..) | Command::RenamePreset(..) | Command::DeletePreset(_) | Command::ToggleFavorite(..) | Command::MoveFavorite(..) | Command::ImportPreset(..)
        );
        if let Err(error) = self.carry_out(command) {
            self.notify(what, error);
        }
        if structural {
            self.save_presets();
        }
    }

    fn carry_out(&mut self, command: Command) -> Result<(), String> {
        match command {
            Command::SetVolume(key, value) => self.set_volume(&key, value),
            Command::SetMuted(key, on) => self.set_muted(&key, on),
            Command::SetOutput(key, device) => {
                self.set_output(&key, device.as_deref())?;
                self.after_outputs_changed();
                if let Some(device) = &device {
                    self.put_first(device);
                }
                Ok(())
            }
            Command::SetMasterOutput(device) => {
                for def in CHANNELS.iter().filter(|c| c.output.is_some()) {
                    self.set_output(def.key, device.as_deref())?;
                }
                self.after_outputs_changed();
                if let Some(device) = &device {
                    self.put_first(device);
                }
                Ok(())
            }
            Command::SetAutoSwitch(on) => self.set_auto_switch(on),
            Command::SetPriority(flow, list) => {
                let mut settings = config::load_full().for_saving()?;
                match flow {
                    Flow::Render => settings.output_priority = list,
                    Flow::Capture => settings.input_priority = list,
                }
                settings.save()
            }
            Command::SetMasterVolume(volume) => self.set_master_volume(volume),
            Command::SetMasterMuted(on) => self.set_master_muted(on),
            Command::SetDeviceLevel(level) => {
                let id = self.master_device.lock().map_err(text)?.clone().ok_or(tr("aucune sortie réelle n'est choisie"))?;
                self.endpoint_volume(&id)?.set_level(level).map_err(text)
            }
            Command::SetMicVolume(level) => self.microphone_volume()?.set_level(level).map_err(text),
            Command::SetMicMuted(on) => self.microphone_volume()?.set_muted(on).map_err(text),
            Command::SetChatMix(balance) => {
                let (game, chat) = mixer::chatmix_gains(balance);
                self.write("game", CHATMIX_GAME, &game.to_string()).and_then(|_| self.write("chat", CHATMIX_CHAT, &chat.to_string()))
            }
            Command::Edit(key, edit) => self.edit_preset(&key, edit),
            Command::SelectPreset(key, chosen) => self.select_preset(&key, &chosen),
            Command::ResetPreset(key) => self.reset_preset(&key),
            Command::SavePreset(key) => self.presets.save_own(index_of(&key)?),
            Command::SavePresetAs(key, name) => self.save_preset_as(&key, &name),
            Command::RenamePreset(key, name) => self.presets.rename_own(index_of(&key)?, &name),
            Command::DeletePreset(key) => self.presets.delete_own(index_of(&key)?),
            Command::ToggleFavorite(key, chosen) => self.presets.toggle_favorite(index_of(&key)?, &chosen),
            Command::MoveFavorite(key, chosen, to) => self.presets.move_favorite(index_of(&key)?, &chosen, to),
            Command::ImportPreset(key, stem, text) => self.import_preset(&key, &stem, &text),
            Command::Route(exe, index) => self.route(&exe, index),
            Command::RouteCapture(exe, index) => self.route_capture(&exe, index),
            Command::SyncEcho => self.sync_echo_references(),
            Command::SetVoiceZones(key, on) => {
                let index = index_of(&key)?;
                if let Some(slot) = self.master.voice_zones.get_mut(index) {
                    *slot = on;
                    self.master_dirty = true;
                }
                Ok(())
            }
            Command::StartDaemon => self.start_daemon(),
            Command::StopDaemon => {
                if background::begin(&self.job, Kind::Stop) { Ok(()) } else { Err(tr("une autre opération sur Platinum est en cours").into()) }
            }
            Command::ChooseMicrophone(name) => self.choose_microphone(&name),
            Command::Set(change) => self.change_setting(&change),
            Command::SetChannelNames(platinum) => {
                // The choice first: the daemon applies it at its next start.
                let mut settings = config::load_full().for_saving()?;
                settings.channel_names = if platinum { names::ChannelNames::Platinum } else { names::ChannelNames::SteelSeries };
                settings.save()?;
                if self.devices.is_none() {
                    self.refresh_endpoints();
                }
                let devices = self.devices.as_ref().ok_or(tr("périphériques audio de Windows indisponibles"))?;
                names::choose(devices, platinum)
            }
            Command::RestartDaemon => self.restart_daemon(),
            Command::SetShortcut(action, hotkey) => {
                let mut settings = config::load_full().for_saving()?;
                settings.set_shortcut(action, hotkey);
                settings.save()
            }
            Command::SetOverlays(overlays) => {
                let mut settings = config::load_full().for_saving()?;
                settings.overlays = overlays;
                settings.save()
            }
            Command::SetMode(mode) => self.set_mode(mode),
            Command::LinkGame(chosen, exe, title) => self.presets.link_game(&chosen, &exe, &title),
            Command::UnlinkGame(exe) => self.presets.unlink_game(&exe),
            Command::SetStreamVolume(key, value) => self.set_stream_volume(&key, value),
            Command::SetStreamMuted(key, on) => self.set_stream_muted(&key, on),
            Command::SetStreamMasterVolume(volume) => self.set_stream_master_volume(volume),
            Command::SetStreamMasterMuted(on) => self.set_stream_master_muted(on),
            Command::SetInclusion(key, mix, on) => self.set_inclusion(&key, mix, on),
            Command::SetPersonalDevice(device) => self.set_personal_device(device),
            Command::SetStreamDevice(device) => self.set_stream_device(device),
            Command::SetStreamDeviceLevel(level) => {
                let id = self.stream_device.lock().map_err(text)?.clone().ok_or(tr("le mix de stream n'a pas de sortie"))?;
                self.endpoint_volume(&id)?.set_level(level).map_err(text)
            }
            Command::SetStartWithWindows(on) => {
                // The settings still held by an old logon command (microphone, fallbacks) would be
                // lost with it: they go into the file first.
                let loaded = config::load_full();
                if loaded.origin == config::Origin::LogonCommand {
                    loaded.for_saving()?.save()?;
                }
                if on {
                    autostart::register()?;
                    // Task Manager's own switch wins over the Run value: say so, or the switch
                    // would turn itself off again without a word.
                    if !autostart::startup_approved() {
                        return Err(tr(STARTUP_BLOCKED).into());
                    }
                    Ok(())
                } else {
                    autostart::disable().map_err(|e| trf!("impossible de retirer le démarrage avec Windows ({error})", error = e))
                }
            }
            Command::Dismiss(id) => {
                self.notices.retain(|n| n.id != id);
                Ok(())
            }
            Command::Notify(text, detail) => {
                self.notify(text, detail);
                Ok(())
            }
        }
    }

    /// A channel's store, opened now if it was not (at most once every 30 s after a failure).
    fn store(&mut self, key: &str) -> Result<&Store<'static>, String> {
        if let Some(error) = self.stores.api_error() {
            return Err(error.to_string());
        }
        let def = find(key).ok_or_else(|| format!("canal inconnu : {key}"))?;
        self.stores.get(def.channel).ok_or_else(|| trf!("réglages du canal {channel} illisibles", channel = tr(def.title)))
    }

    fn reader(&mut self, key: &str) -> Result<Reader<'_>, String> {
        self.store(key)?;
        let index = position(key).ok_or_else(|| format!("canal inconnu : {key}"))?;
        let store = self.stores.opened(CHANNELS[index].channel).ok_or(tr("réglages illisibles"))?;
        Ok(Reader { store, index, ranges: &mut self.ranges })
    }

    fn write(&mut self, key: &str, id: i32, value: &str) -> Result<(), String> {
        let store = self.store(key)?;
        store.setting(id).and_then(|s| s.set_from_text(value)).map_err(text)
    }

    fn write_flag(&mut self, key: &str, pick: fn(&ChannelDef) -> Option<i32>, on: bool) -> Result<(), String> {
        let id = find(key).and_then(pick).ok_or(tr("réglage absent pour ce canal"))?;
        self.write(key, id, on_off(on))
    }

    /// Writes a channel's volume, 0..1 of its range.
    fn write_volume(&mut self, key: &str, fraction: f32) -> Result<(), String> {
        let id = find(key).and_then(|c| c.volume).ok_or(tr("ce canal n'a pas de volume"))?;
        let (minimum, maximum) = self.reader(key)?.range(id).ok_or(tr("le volume n'est pas un nombre"))?;
        let value = (minimum + fraction.clamp(0.0, 1.0) * (maximum - minimum)).clamp(minimum, maximum);
        self.write(key, id, &value.to_string())
    }

    /// The four output channels' volumes, 0..1, as the stores hold them now.
    fn strip_volumes(&mut self) -> Result<[f32; STRIPS], String> {
        self.mix_volumes(Mix::Personal)
    }

    fn strip_mutes(&mut self) -> Result<[bool; STRIPS], String> {
        self.mix_mutes(Mix::Personal)
    }

    /// The four output channels' volumes in one mix, 0..1, as the stores hold them now.
    fn mix_volumes(&mut self, mix: Mix) -> Result<[f32; STRIPS], String> {
        let mut volumes = [0.0; STRIPS];
        for (volume, def) in volumes.iter_mut().zip(&CHANNELS) {
            let (id, _) = mixer::mix_volume_settings(def.channel, mix).ok_or(tr("ce canal n'a pas de volume"))?;
            *volume = self.reader(def.key)?.fraction(id).ok_or_else(|| trf!("volume de {channel} illisible", channel = tr(def.title)))?;
        }
        Ok(volumes)
    }

    fn mix_mutes(&mut self, mix: Mix) -> Result<[bool; STRIPS], String> {
        let mut mutes = [false; STRIPS];
        for (muted, def) in mutes.iter_mut().zip(&CHANNELS) {
            let (_, id) = mixer::mix_volume_settings(def.channel, mix).ok_or(tr("ce canal n'a pas de sourdine"))?;
            *muted = self.reader(def.key)?.flag(id);
        }
        Ok(mutes)
    }

    /// Writes a channel's volume in the stream mix, 0..1 of its range.
    fn write_stream_volume(&mut self, key: &str, fraction: f32) -> Result<(), String> {
        let def = find(key).ok_or_else(|| format!("canal inconnu : {key}"))?;
        let (id, _) = mixer::mix_volume_settings(def.channel, Mix::Stream).ok_or(tr("ce canal n'a pas de mix de stream"))?;
        let (minimum, maximum) = self.reader(key)?.range(id).ok_or(tr("le volume n'est pas un nombre"))?;
        let value = (minimum + fraction.clamp(0.0, 1.0) * (maximum - minimum)).clamp(minimum, maximum);
        self.write(key, id, &value.to_string())
    }

    fn write_stream_muted(&mut self, key: &str, on: bool) -> Result<(), String> {
        let def = find(key).ok_or_else(|| format!("canal inconnu : {key}"))?;
        let (_, id) = mixer::mix_volume_settings(def.channel, Mix::Stream).ok_or(tr("ce canal n'a pas de mix de stream"))?;
        self.write(key, id, on_off(on))
    }

    /// A channel's stream volume: the Stream master follows it, as the Master follows the
    /// personal volumes (raised when the channel goes above it).
    fn set_stream_volume(&mut self, key: &str, value: f32) -> Result<(), String> {
        self.write_stream_volume(key, value)?;
        if let Some(index) = position(key).filter(|&i| i < STRIPS) {
            let volumes = self.mix_volumes(Mix::Stream)?;
            self.master.stream.channel_moved(index, value, &volumes);
            self.master_dirty = true;
        }
        Ok(())
    }

    fn set_stream_muted(&mut self, key: &str, on: bool) -> Result<(), String> {
        self.write_stream_muted(key, on)?;
        if !on && self.master.stream.muted {
            self.master.stream.muted = false;
            self.master_dirty = true;
        }
        Ok(())
    }

    /// GG's Stream master (`SetStreamerMasterVolume`): every stream volume keeps its share.
    fn set_stream_master_volume(&mut self, volume: f32) -> Result<(), String> {
        let volumes = self.mix_volumes(Mix::Stream)?;
        self.master.stream.take(&volumes);
        let wanted = self.master.stream.set_volume(volume);
        self.master_dirty = true;
        for ((def, now), target) in CHANNELS.iter().zip(volumes).zip(wanted) {
            if (now - target).abs() > 1e-4 {
                self.write_stream_volume(def.key, target)?;
            }
        }
        Ok(())
    }

    /// The Stream master's mute (`SetStreamerMasterMute`): mutes the four channels in the stream
    /// mix, then gives each its own mute back.
    fn set_stream_master_muted(&mut self, on: bool) -> Result<(), String> {
        let mutes = self.mix_mutes(Mix::Stream)?;
        let wanted = if on { self.master.stream.mute(&mutes) } else { self.master.stream.unmute() };
        self.master_dirty = true;
        for ((def, now), target) in CHANNELS.iter().zip(mutes).zip(wanted) {
            if now != target {
                self.write_stream_muted(def.key, target)?;
            }
        }
        Ok(())
    }

    /// The devices the four strips play to (their personal mix in streamer mode).
    fn routed_strips(&mut self) -> Vec<Option<String>> {
        CHANNELS.iter().take(STRIPS).map(|def| self.routed_output(def.key)).collect()
    }

    /// The real outputs plugged in now, by identifier.
    fn present_outputs(&self) -> Vec<&str> {
        self.endpoints.iter().filter(|e| e.flow == Flow::Render && e.virtual_role.is_none() && e.state == State::Active).map(|e| e.id.as_str()).collect()
    }

    /// Puts strips in or out of the mixes (see `streamer::Route`).
    fn write_routes(&mut self, routes: &[streamer::Route]) -> Result<(), String> {
        for route in routes {
            let store = self.store(CHANNELS[route.strip].key)?;
            streamer::set_mix_device(store, route.mix, route.device.as_deref())?;
        }
        Ok(())
    }

    /// Switches between the classic mixer and streamer mode (`platinum::streamer`). Entering:
    /// the classic outputs are remembered, every channel joins the mixes platinum.ini says (the
    /// personal mix on the device the channels played to, the stream mix on the Stream virtual
    /// device by default), and the daemon is asked for the Stream device. Leaving: every channel
    /// leaves the stream mix and gets its classic output back, and the Stream device is let go.
    /// The echo cancellation follows in both cases.
    fn set_mode(&mut self, mode: Mode) -> Result<(), String> {
        let mut settings = config::load_full().for_saving()?;
        if settings.mode == mode {
            return Ok(());
        }
        self.refresh_endpoints();
        let routed = self.routed_strips();
        let personal = streamer::personal_device(&routed, settings.personal_device.as_deref());
        match mode {
            Mode::Streamer => {
                let stream = settings.stream_device.resolve(&self.endpoints);
                settings.classic_outputs = Some(std::array::from_fn(|strip| routed[strip].clone()));
                self.write_routes(&streamer::routes(personal.as_deref(), stream.as_deref(), &settings.personal_mix, &settings.stream_mix))?;
            }
            Mode::Classic => {
                let back = streamer::classic_outputs(settings.classic_outputs.as_ref(), personal.as_deref(), &self.present_outputs());
                self.write_routes(&streamer::mix_routes(Mix::Stream, None, &streamer::ALL_STRIPS))?;
                for (def, device) in CHANNELS.iter().zip(back) {
                    self.set_output(def.key, device.as_deref())?;
                }
                settings.classic_outputs = None;
            }
        }
        settings.personal_device = personal;
        settings.mode = mode;
        settings.channels = streamer::with_stream_channel(&settings.channels, mode == Mode::Streamer);
        settings.save()?;
        self.after_outputs_changed();
        Ok(())
    }

    /// A channel in or out of a mix. The choice is kept in platinum.ini in any mode; in streamer
    /// mode the channel's redirection names the mix's device, or none (GG's rule, B14).
    fn set_inclusion(&mut self, key: &str, mix: Mix, on: bool) -> Result<(), String> {
        let index = index_of(key)?;
        if index >= STRIPS {
            return Err(tr("seuls Gaming, Chat, Media et Aux vont dans les mix").into());
        }
        let mut settings = config::load_full().for_saving()?;
        match mix {
            Mix::Personal => settings.personal_mix[index] = on,
            Mix::Stream => settings.stream_mix[index] = on,
        }
        settings.save()?;
        if settings.mode != Mode::Streamer {
            return Ok(());
        }
        let device = match mix {
            Mix::Personal => {
                let routed = self.routed_strips();
                streamer::personal_device(&routed, settings.personal_device.as_deref())
            }
            Mix::Stream => {
                if self.endpoints.is_empty() {
                    self.refresh_endpoints();
                }
                settings.stream_device.resolve(&self.endpoints)
            }
        };
        streamer::set_mix_device(self.store(key)?, mix, device.as_deref().filter(|_| on))?;
        match mix {
            Mix::Personal => self.after_outputs_changed(),
            Mix::Stream => {
                if let Err(error) = self.sync_echo_references() {
                    self.notify(tr("Annulation d'écho non mise à jour"), error);
                }
            }
        }
        Ok(())
    }

    /// The personal mix's device, for every channel in it (streamer mode).
    fn set_personal_device(&mut self, device: Option<String>) -> Result<(), String> {
        let mut settings = config::load_full().for_saving()?;
        if settings.mode != Mode::Streamer {
            return Err(tr("le mode streamer n'est pas actif").into());
        }
        settings.personal_device = device.clone();
        settings.save()?;
        self.write_routes(&streamer::mix_routes(Mix::Personal, device.as_deref(), &settings.personal_mix))?;
        self.after_outputs_changed();
        if let Some(device) = &device {
            self.put_first(device);
        }
        Ok(())
    }

    /// The stream mix's device, for every channel in it (in streamer mode; otherwise only
    /// remembered).
    fn set_stream_device(&mut self, device: StreamDevice) -> Result<(), String> {
        let mut settings = config::load_full().for_saving()?;
        settings.stream_device = device;
        settings.save()?;
        if settings.mode != Mode::Streamer {
            return Ok(());
        }
        self.refresh_endpoints();
        let resolved = settings.stream_device.resolve(&self.endpoints);
        self.write_routes(&streamer::mix_routes(Mix::Stream, resolved.as_deref(), &settings.stream_mix))?;
        self.sync_echo_references()
    }

    fn set_volume(&mut self, key: &str, value: f32) -> Result<(), String> {
        self.write_volume(key, value)?;
        if let Some(index) = position(key).filter(|&i| i < STRIPS) {
            let volumes = self.strip_volumes()?;
            self.master.channel_moved(index, value, &volumes);
            self.master_dirty = true;
        }
        Ok(())
    }

    fn set_muted(&mut self, key: &str, on: bool) -> Result<(), String> {
        self.write_flag(key, |c| c.muted, on)?;
        if !on && position(key).is_some_and(|i| i < STRIPS) && self.master.muted {
            self.master.muted = false;
            self.master_dirty = true;
        }
        Ok(())
    }

    /// GG's classic Master: every channel keeps its share of the Master.
    fn set_master_volume(&mut self, volume: f32) -> Result<(), String> {
        let volumes = self.strip_volumes()?;
        self.master.take(&volumes);
        let wanted = self.master.set_volume(volume);
        self.master_dirty = true;
        for ((def, now), target) in CHANNELS.iter().zip(volumes).zip(wanted) {
            if (now - target).abs() > 1e-4 {
                self.write_volume(def.key, target)?;
            }
        }
        Ok(())
    }

    /// The Master's mute mutes the four channels, then gives each its own mute back.
    fn set_master_muted(&mut self, on: bool) -> Result<(), String> {
        let mutes = self.strip_mutes()?;
        let wanted = if on { self.master.mute(&mutes) } else { self.master.unmute() };
        self.master_dirty = true;
        for ((def, now), target) in CHANNELS.iter().zip(mutes).zip(wanted) {
            if now != target {
                self.write_flag(def.key, |c| c.muted, target)?;
            }
        }
        Ok(())
    }

    /// Saves the Master when the user changed it (never in read-only mode).
    fn save_master(&mut self) {
        if !self.master_dirty || self.read_only {
            return;
        }
        self.master_dirty = false;
        if let Err(error) = mixer::save_master(&self.master) {
            self.notify(tr("La position du Master n'a pas été enregistrée"), error);
        }
        self.master_stamp = mixer::master_stamp();
    }

    /// Reads the Master again when window.ini changed behind the window (a keyboard shortcut moved
    /// it), unless the user moved it here and it is not saved yet.
    fn reload_master(&mut self) {
        let stamp = mixer::master_stamp();
        if self.master_dirty || stamp == self.master_stamp {
            return;
        }
        self.master_stamp = stamp;
        self.master = mixer::load_master();
    }

    /// Saves state.json when the user changed a preset (never in read-only mode).
    fn save_presets(&mut self) {
        if self.read_only {
            return;
        }
        if let Err(error) = self.presets.save() {
            self.notify(tr("Les préréglages des canaux n'ont pas été enregistrés"), error);
        }
    }

    /// Writes what turns a channel's current preset into `next`, GG's rules included (the Arena 7
    /// speaker rule too: on a preset applied whole, as GG's `MakeCiaran7Compliant`, or on spatial
    /// audio switched on), and returns what the channel then holds with the outcome. When a write
    /// fails, the module is read back instead, so that the window shows what it really holds.
    fn write_preset(&mut self, index: usize, next: Preset, diff: Diff) -> (Preset, Result<(), String>) {
        let (current, outcome, _) = self.write_preset_ruled(index, next, diff);
        (current, outcome)
    }

    /// `write_preset`, also saying whether GG's Arena 7 rule changed what was asked: a preset
    /// applied whole then keeps speaker mode in the user's edits of it, as GG saves it into the
    /// preset.
    fn write_preset_ruled(&mut self, index: usize, mut next: Preset, diff: Diff) -> (Preset, Result<(), String>, bool) {
        let role = ROLES[index];
        let Some(previous) = self.presets.current(index).copied() else { return (next, Err(tr("réglages du canal illisibles").into()), false) };
        let ruled = presets::arena_rule(&previous, &mut next, self.arena[index] && self.arena_speakers, diff == Diff::Apply);
        let writes = match preset::plan((diff == Diff::Edit).then_some(&previous), &next, role) {
            Ok(writes) => writes,
            Err(error) => return (previous, Err(error), false),
        };
        let (current, outcome) = self.write_planned(index, previous, next, writes, diff);
        (current, outcome, ruled)
    }

    fn write_planned(&mut self, index: usize, previous: Preset, next: Preset, writes: Vec<preset::Write>, diff: Diff) -> (Preset, Result<(), String>) {
        let role = ROLES[index];
        if writes.is_empty() {
            return (next, Ok(()));
        }
        if let Err(error) = self.store(CHANNELS[index].key) {
            return (previous, Err(error));
        }
        let Some(store) = self.stores.opened(CHANNELS[index].channel) else { return (previous, Err(tr("réglages du canal illisibles").into())) };
        let (current, outcome) = write_into(store, role, &next, writes, diff);
        if index == MIC && outcome.is_ok() && echo_switched_on(&previous, &current) {
            if let Err(error) = self.sync_echo_references() {
                self.notify(tr("Annulation d'écho non mise à jour"), error);
            }
        }
        (current, outcome)
    }

    fn edit_preset(&mut self, key: &str, edit: Edit) -> Result<(), String> {
        let index = index_of(key)?;
        let mut next = *self.presets.current(index).ok_or(tr("réglages du canal illisibles"))?;
        if !presets::apply(&mut next, edit) {
            return Ok(());
        }
        let (current, outcome) = self.write_preset(index, next, Diff::Edit);
        self.presets.edited(index, current);
        outcome
    }

    /// Applies a preset (with the user's edits of it, if any): every setting that differs from the
    /// module is written; the "Effets" switch stays.
    fn select_preset(&mut self, key: &str, chosen: &PresetRef) -> Result<(), String> {
        let index = index_of(key)?;
        let entry = self.presets.catalog.entry(index, chosen).cloned().ok_or(tr("ce préréglage n'existe plus"))?;
        // Nothing is chosen on a channel whose settings cannot be read: it could not be applied.
        let current = self.presets.current(index).ok_or(tr("réglages du canal illisibles"))?;
        let next = presets::keeping_effects(self.presets.data_for(index, &entry), Some(current));
        let (current, outcome, ruled) = self.write_preset_ruled(index, next, Diff::Apply);
        // On Gaming, a game's preset put there by the daemon ends here (`Presets::select`).
        self.presets.select(index, &entry);
        if ruled && outcome.is_ok() {
            self.presets.edited(index, current);
        } else {
            self.presets.set_current(index, current);
        }
        outcome
    }

    /// Back to the chosen preset's own settings (its factory version for a GG preset); the edits
    /// kept for it are dropped (but for speaker mode, when GG's Arena 7 rule puts it back).
    fn reset_preset(&mut self, key: &str) -> Result<(), String> {
        let index = index_of(key)?;
        let entry = self.presets.selected_entry(index).cloned().ok_or(tr("aucun préréglage à réinitialiser : ces réglages ne viennent d'aucun préréglage"))?;
        self.presets.forget_edits(index);
        let next = presets::keeping_effects(entry.reference, self.presets.current(index));
        let (current, outcome, ruled) = self.write_preset_ruled(index, next, Diff::Apply);
        if ruled && outcome.is_ok() {
            self.presets.edited(index, current);
        } else {
            self.presets.set_current(index, current);
        }
        self.presets.dirty = true;
        outcome
    }

    /// GG's Arena 7 rule after an output change: each surround channel (Gaming, Media, Aux) that
    /// now plays to Arena 7 speakers and did not before, with spatial audio in headphone mode, goes
    /// to speaker mode, kept as an edit of its preset, as GG saves it. A channel that was on the
    /// speakers already keeps the mode the user picked.
    fn arena_after_output_change(&mut self) -> Result<(), String> {
        self.refresh_endpoints();
        let before = self.arena;
        for index in 0..STRIPS {
            let output = self.routed_output(CHANNELS[index].key);
            self.arena[index] = output.is_some_and(|id| self.endpoints.iter().any(|e| e.id == id && outputs::is_arena(e)));
        }
        if !self.arena_speakers {
            return Ok(());
        }
        let surround: Vec<usize> = (0..STRIPS).filter(|&i| !ROLES[i].is_capture() && self.arena[i] && !before[i]).collect();
        for index in surround {
            let Some(mut next) = self.presets.current(index).copied() else { continue };
            if !next.arena_speakers() {
                continue;
            }
            let (current, outcome) = self.write_preset(index, next, Diff::Edit);
            self.presets.edited(index, current);
            outcome?;
        }
        Ok(())
    }

    /// Saves the channel's settings under a new name; the channel then uses that preset (its
    /// settings do not change, so nothing is written to the module).
    fn save_preset_as(&mut self, key: &str, name: &str) -> Result<(), String> {
        let index = index_of(key)?;
        let data = *self.presets.current(index).ok_or(tr("réglages du canal illisibles"))?;
        let entry = self.presets.add_own(index, name, data)?;
        self.presets.select(index, &entry);
        Ok(())
    }

    /// Adds a preset file to the user's presets, under a name no other preset of the channel uses,
    /// then applies it.
    fn import_preset(&mut self, key: &str, stem: &str, text: &str) -> Result<(), String> {
        let index = index_of(key)?;
        let (name, data) = crate::persist::import_text(text, ROLES[index])?;
        let shelf = self.presets.catalog.shelf(index).ok_or(tr("canal inconnu"))?;
        let name = shelf.unique_name(name.as_deref().unwrap_or(stem));
        let entry = self.presets.add_own(index, &name, data)?;
        self.select_preset(key, &entry.key)
    }

    fn set_output(&mut self, key: &str, device: Option<&str>) -> Result<(), String> {
        outputs::set_output(self.store(key)?, device)
    }

    /// The real output a channel plays to, `None` when it has none or cannot be read.
    fn routed_output(&mut self, key: &str) -> Option<String> {
        self.store(key).ok().and_then(outputs::routed)
    }

    /// With the backup switch on, a device the user chose for the channels goes first in the
    /// output list: the daemon would otherwise move them back within two seconds. The output did
    /// change: a failure here is a notice of its own.
    fn put_first(&mut self, id: &str) {
        let loaded = config::load_full();
        if !loaded.config.auto_switch {
            return;
        }
        let name = self.endpoints.iter().find(|e| e.id == id).map(|e| e.name.clone()).unwrap_or_default();
        let outcome = loaded.for_saving().and_then(|mut settings| if fallback::promote(&mut settings.output_priority, id, &name) { settings.save() } else { Ok(()) });
        if let Err(error) = outcome {
            self.notify(tr("La liste de priorité des sorties n'a pas été mise à jour"), error);
        }
    }

    /// Turns the backup switch on or off. Turned on with an empty output list, the list starts
    /// with the devices the channels play to now, so that nothing moves until the user orders it.
    fn set_auto_switch(&mut self, on: bool) -> Result<(), String> {
        let mut settings = config::load_full().for_saving()?;
        settings.auto_switch = on;
        if on && settings.output_priority.is_empty() {
            self.refresh_endpoints();
            let routed: Vec<Option<String>> = CHANNELS.iter().take(STRIPS).map(|def| self.routed_output(def.key)).collect();
            for id in outputs::distinct(&routed) {
                let name = self.endpoints.iter().find(|e| e.id == *id).map_or("", |e| e.name.as_str());
                fallback::add(&mut settings.output_priority, id, name);
            }
        }
        settings.save()
    }

    /// What follows a change of output, as in GG: the echo cancellation listens to the new
    /// outputs, and ChatMix goes back to the middle once Gaming and Chat no longer share a device.
    /// The output itself did change: a failure here is its own notice.
    fn after_outputs_changed(&mut self) {
        if let Err(error) = self.sync_echo_references() {
            self.notify(tr("Annulation d'écho non mise à jour"), error);
        }
        if let Err(error) = self.reset_split_chatmix() {
            self.notify(tr("ChatMix non remis au milieu"), error);
        }
        if let Err(error) = self.arena_after_output_change() {
            self.notify(tr("Mode haut-parleur non appliqué pour l'Arena 7"), error);
        }
    }

    /// GG's `DisableChatmix`: with Gaming and Chat on different devices (or one without any),
    /// ChatMix cannot mix them, and a balance left aside would keep one channel turned down on its
    /// own device for no visible reason. Both gains go back to 1.
    fn reset_split_chatmix(&mut self) -> Result<(), String> {
        let (game, chat) = (self.routed_output("game"), self.routed_output("chat"));
        if chatmix_reason(game.as_deref(), chat.as_deref()).is_empty() {
            return Ok(());
        }
        for (key, id) in [("game", CHATMIX_GAME), ("chat", CHATMIX_CHAT)] {
            let held = self.reader(key)?.number(id).map(|(value, _, _)| value);
            if held.is_some_and(|value| (value - 1.0).abs() > 1e-4) {
                self.write(key, id, "1")?;
            }
        }
        Ok(())
    }

    /// Keeps the microphone's echo cancellation listening to the outputs the user hears, as GG
    /// does whenever an output changes (see `outputs::echo_references`). A channel that cannot be
    /// read is left out rather than stopping the others. Writes only what differs.
    fn sync_echo_references(&mut self) -> Result<(), String> {
        let routed = self.routed_strips();
        let settings = config::load();
        if self.endpoints.is_empty() {
            self.refresh_endpoints();
        }
        let stream_device = settings.stream_device.resolve(&self.endpoints);
        let stream = StreamReference::of(settings.mode, &settings.channels, stream_device.as_deref());
        outputs::sync_echo(self.store("mic")?, &routed, stream).map(|_| ())
    }

    fn endpoint_volume(&mut self, id: &str) -> Result<EndpointVolume, String> {
        if self.devices.is_none() {
            self.refresh_endpoints();
        }
        let devices = self.devices.as_ref().ok_or(tr("périphériques audio indisponibles"))?;
        EndpointVolume::activate(devices, id).map_err(text)
    }

    /// The recording side of the virtual microphone (what Discord and the others record).
    fn microphone_endpoint(&self) -> Option<&Endpoint> {
        self.endpoints.iter().find(|e| e.flow == Flow::Capture && e.state == State::Active && session::virtual_kind(e).as_deref() == Some("microphone"))
    }

    /// The virtual microphone's Windows volume. The device list the last snapshot read is used
    /// first: the fader sends a command per pointer move, and listing every endpoint costs
    /// milliseconds. The list is read again only when the microphone is missing from it or its
    /// volume cannot be opened (the daemon restarted it).
    fn microphone_volume(&mut self) -> Result<EndpointVolume, String> {
        if let Some(id) = self.microphone_endpoint().map(|e| e.id.clone()) {
            if let Ok(volume) = self.endpoint_volume(&id) {
                return Ok(volume);
            }
        }
        self.refresh_endpoints();
        let id = self.microphone_endpoint().map(|e| e.id.clone()).ok_or(tr("le micro virtuel n'est pas actif : démarrez Platinum"))?;
        self.endpoint_volume(&id)
    }

    fn route(&mut self, exe: &str, index: usize) -> Result<(), String> {
        let target = match index {
            0 => None,
            1..=4 => Some(&CHANNELS[index - 1]),
            _ => return Err(tr("destination inconnue").into()),
        };
        self.refresh_endpoints();
        let destination = match target {
            Some(def) => {
                let endpoint = session::channel_output(&self.endpoints, def.key).ok_or_else(|| trf!("le canal {channel} n'existe pas", channel = tr(def.title)))?;
                if endpoint.state != State::Active {
                    return Err(trf!("le canal {channel} n'est pas actif : démarrez Platinum", channel = tr(def.title)));
                }
                Some(endpoint.id.clone())
            }
            None => None,
        };
        set_route(exe, Flow::Render, destination.as_deref())?;
        // Its sound is checked in the next snapshots: an application that opened its device
        // itself does not follow, and the window then says so.
        self.refused.retain(|e| !e.eq_ignore_ascii_case(exe));
        self.routes_sent.retain(|(e, _, _)| !e.eq_ignore_ascii_case(exe));
        let channel = target.map_or(self.default_channel(), |_| index as i32);
        self.routes_sent.push((exe.to_string(), channel, Instant::now()));
        Ok(())
    }

    /// A recording application to the virtual microphone (1), or back to the default one (0).
    fn route_capture(&mut self, exe: &str, index: usize) -> Result<(), String> {
        let destination = match index {
            0 => None,
            1 => {
                self.refresh_endpoints();
                Some(self.microphone_endpoint().map(|e| e.id.clone()).ok_or(tr("le micro virtuel n'est pas actif : démarrez Platinum"))?)
            }
            _ => return Err(tr("destination inconnue").into()),
        };
        set_route(exe, Flow::Capture, destination.as_deref())
    }

    /// The channel (1 to 4) the default output is, 0 when it is a real device.
    fn default_channel(&self) -> i32 {
        let default = self.devices.as_ref().and_then(|d| d.default(Flow::Render, Role::Console).ok().flatten());
        default.and_then(|id| self.endpoints.iter().find(|e| e.id == id)).map_or(0, channel_of)
    }

    fn start_daemon(&mut self) -> Result<(), String> {
        self.last_error = None;
        if daemon::sonar_running() {
            return Err(tr("SteelSeries GG est ouvert : fermez GG pour démarrer Platinum").into());
        }
        self.follow_job();
        if !background::begin(&self.job, Kind::Start) {
            return Err(tr("une autre opération sur Platinum est en cours").into());
        }
        self.job_by_user = true;
        Ok(())
    }

    /// Stops the daemon and starts it again (the settings page's "Redémarrer Platinum", once
    /// confirmed): it then reads the options it reads only when it starts.
    fn restart_daemon(&mut self) -> Result<(), String> {
        self.last_error = None;
        if daemon::sonar_running() {
            return Err(tr("SteelSeries GG est ouvert : fermez GG pour démarrer Platinum").into());
        }
        // A job that is over still holds its place until its outcome is taken.
        self.follow_job();
        if !background::begin(&self.job, Kind::Restart) {
            return Err(tr("une autre opération sur Platinum est en cours").into());
        }
        self.job_by_user = true;
        Ok(())
    }

    /// Saves an option of the settings page in platinum.ini (the whole file is rewritten, its
    /// comments with it). An old logon command still carrying that option as an argument would
    /// win over the file at the next logon: it is replaced by the one that leaves the settings to
    /// the file, as for the microphone.
    fn change_setting(&mut self, change: &Change) -> Result<(), String> {
        let mut settings = config::load_full().for_saving()?;
        change.apply(&mut settings);
        settings.save()?;
        if change.is_daemon_argument() && autostart::logon_overrides_config() {
            if let Err(error) = autostart::register() {
                self.notify(tr("Le démarrage avec Windows garde les anciens réglages du micro"), error);
            }
        }
        Ok(())
    }

    /// Saves the microphone in the configuration, then restarts the daemon if it relays one.
    fn choose_microphone(&mut self, id: &str) -> Result<(), String> {
        self.refresh_endpoints();
        let captures: Vec<Endpoint> = self.endpoints.iter().filter(|e| e.flow == Flow::Capture).cloned().collect();
        let endpoint = captures.iter().find(|e| e.id == id).ok_or(tr("ce micro n'est plus branché"))?;
        if self.devices.as_ref().is_some_and(|d| d.is_hands_free(id)) {
            return Err(format!("{} est le micro d'un casque Bluetooth : l'ouvrir ferait passer le casque en mode mains libres, Platinum ne l'utilise pas", endpoint.name));
        }
        let name = endpoint.name.clone();
        let mut settings = config::load_full().for_saving()?;
        settings.microphone = Some(settings::device_spec(&captures, endpoint));
        settings.save()?;
        // An old logon command still carries a microphone, which would win over the file at the
        // next logon: replace it by the one that leaves the settings to the file (a `--no-tray`
        // alone is no setting, and stays).
        if autostart::logon_overrides_config() {
            if let Err(error) = autostart::register() {
                self.notify(tr("Le démarrage avec Windows garde l'ancien micro"), error);
            }
        }
        // A start under way may have read the previous microphone already: the daemon restarts
        // for this one once it is over (`follow_job`).
        if background::active_stage(&self.job).is_some() {
            self.pending_microphone = Some(name);
            return Ok(());
        }
        // A job that is over still holds its place until its outcome is taken.
        self.follow_job();
        let seen = background::Seen::now();
        let view = background::view(&seen, None, None, &name);
        if view.restarts_on_mic_change {
            if !background::begin(&self.job, Kind::Restart) {
                return Err(tr("une autre opération sur Platinum est en cours : le micro servira au prochain démarrage").into());
            }
            self.job_by_user = true;
        }
        Ok(())
    }

    fn snapshot(&mut self) -> Snapshot {
        self.refresh_endpoints();
        let endpoints = std::mem::take(&mut self.endpoints);
        let mut snapshot = Snapshot { handled: self.handled, ..Snapshot::default() };
        let loaded = config::load_full().config;

        // Outputs: no output, then every real output that is plugged in.
        let real: Vec<&Endpoint> = endpoints.iter().filter(|e| e.flow == Flow::Render && e.virtual_role.is_none() && e.state == State::Active).collect();
        self.output_ids = std::iter::once(None).chain(real.iter().map(|e| Some(e.id.clone()))).collect();
        snapshot.outputs = std::iter::once(tr("Aucune sortie").to_string()).chain(real.iter().map(|e| e.name.clone())).collect();

        self.stores.open_all();
        if self.presets.refresh_library() {
            self.notify_preset_problems();
        }
        self.presets.refresh_installed();
        // A game's preset the daemon put on Gaming (its own file, read only here).
        self.presets.set_game(platinum::games::load().ok().flatten());
        self.arena_speakers = loaded.arena_speakers;
        let mut routed: Vec<Option<String>> = Vec::new();
        let mut gains = (1.0, 1.0);
        for (index, def) in CHANNELS.iter().enumerate() {
            let selection = self.presets.selection(index);
            let selection_name = self.presets.selection_name(index);
            let module = self.stores.opened(def.channel).map(|store| preset::from_store(store, ROLES[index]));
            let (Some(store), Some(Ok(module))) = (self.stores.opened(def.channel), module) else {
                snapshot.channels.push(ChannelData { selection, selection_name, ..ChannelData::default() });
                routed.push(None);
                continue;
            };
            let mut reader = Reader { store, index, ranges: &mut self.ranges };
            let (mut data, output) = read_channel(&mut reader, def);
            match def.key {
                "game" => gains.0 = reader.number(CHATMIX_GAME).map_or(1.0, |(v, _, _)| v),
                "chat" => gains.1 = reader.number(CHATMIX_CHAT).map_or(1.0, |(v, _, _)| v),
                _ => {}
            }
            if let Preset::Capture(held) = &module {
                data.held_reductions = (held.noise_reduction.enabled, held.impact_noise_reduction.enabled);
            }
            // The module read back: the current preset keeps only what the module cannot show. On
            // the first start, that is the preset GG had selected, as the catalog seeded it.
            let model = self.presets.current(index).copied().or_else(|| self.presets.selected_entry(index).map(|entry| self.presets.data_for(index, entry)));
            let current = match model {
                Some(model) => presets::reconcile(&model, module),
                None => module,
            };
            self.presets.set_current(index, current);
            data.readable = true;
            data.preset = Some(current);
            data.selection = selection;
            data.selection_name = selection_name;
            data.edited = self.presets.state.edited[index].clone();
            if index == 0 {
                data.game = self.presets.game_override().map(|saved| saved.game.clone()).unwrap_or_default();
            }
            snapshot.channels.push(data);
            routed.push(output);
        }
        for (index, output) in routed.iter().enumerate() {
            let arena = output.as_deref().is_some_and(|id| endpoints.iter().any(|e| e.id == id && outputs::is_arena(e)));
            self.arena[index] = arena;
            snapshot.channels[index].arena = arena;
        }
        let strips_read = snapshot.channels.iter().take(STRIPS).all(|c| c.readable);
        snapshot.echo_stale = strips_read && self.echo_is_stale(&routed[..STRIPS], &loaded, &endpoints);
        snapshot.chatmix_reason = chatmix_reason(routed[0].as_deref(), routed[1].as_deref()).to_string();
        for (data, output) in snapshot.channels.iter_mut().zip(&routed) {
            if let Some(id) = output {
                data.output_index = output_index(&mut self.output_ids, &mut snapshot.outputs, &endpoints, id);
                data.output_present = real.iter().any(|e| e.id == *id);
            }
        }
        snapshot.output_ids = self.output_ids.clone();
        snapshot.chatmix = mixer::chatmix_balance(gains.0, gains.1);

        // Master: the device the channels play to, when they share one (its level box and meter
        // would otherwise follow one device and say nothing of the others); the fader takes in the
        // volumes as they are (in memory only: a snapshot writes nothing).
        let heard = outputs::distinct(&routed[..STRIPS]);
        let master_id = (heard.len() == 1).then(|| heard[0].clone());
        if let Ok(mut shared) = self.master_device.lock() {
            shared.clone_from(&master_id);
        }
        let strips = &snapshot.channels[..STRIPS];
        snapshot.master.output_index = if heard.len() > 1 { -1 } else { strips.iter().map(|c| c.output_index).find(|&i| i > 0).unwrap_or(0) };
        self.reload_master();
        if strips.iter().all(|c| c.readable) {
            let volumes: [f32; STRIPS] = std::array::from_fn(|i| strips[i].volume);
            let mutes: [bool; STRIPS] = std::array::from_fn(|i| strips[i].muted);
            self.master.take(&volumes);
            self.master.take_mutes(&mutes);
            let stream_volumes: [f32; STRIPS] = std::array::from_fn(|i| strips[i].stream_volume);
            let stream_mutes: [bool; STRIPS] = std::array::from_fn(|i| strips[i].stream_muted);
            self.master.stream.take(&stream_volumes);
            self.master.stream.take_mutes(&stream_mutes);
        }
        snapshot.master.fader = self.master;
        if let Some(volume) = master_id.as_deref().and_then(|id| self.endpoint_volume(id).ok()) {
            snapshot.master.device_level = volume.level().ok();
            snapshot.master.device_muted = volume.muted().unwrap_or(false);
        }
        snapshot.streamer = self.streamer_data(&loaded, &routed[..STRIPS], &endpoints, &mut snapshot.outputs);
        // The personal mix's device may have been added to the outputs (unplugged).
        snapshot.output_ids = self.output_ids.clone();

        snapshot.apps = self.applications(&endpoints, Flow::Render);
        snapshot.apps.extend(self.applications(&endpoints, Flow::Capture));
        self.check_routes(&snapshot.apps);
        self.endpoints = endpoints;
        snapshot.default_channel = self.default_channel();
        snapshot.default_capture_mic = self.devices.as_ref().and_then(|d| d.default(Flow::Capture, Role::Console).ok().flatten()).is_some_and(|id| self.microphone_endpoint().is_some_and(|e| e.id == id));

        self.follow_job();
        let seen = background::Seen::now();
        snapshot.mic = self.microphone_data(loaded.microphone.as_deref(), &seen);
        let relayed = seen.status.as_ref().filter(|s| s.state.is_live() && !s.vanished && !s.microphone.is_empty()).map(|s| s.microphone.clone());
        snapshot.priority = self.priority_data(&loaded, &routed[..STRIPS], relayed.as_deref(), &snapshot.mic);
        let chosen = usize::try_from(snapshot.mic.current).ok().and_then(|i| snapshot.mic.inputs.get(i)).map(|m| m.name.clone()).or_else(|| loaded.microphone.clone()).unwrap_or_default();
        snapshot.daemon = background::view(&seen, background::active_stage(&self.job), self.last_error.as_deref(), &chosen);
        if snapshot.daemon.phase.carries_sound() {
            self.last_error = None;
        }
        let live = seen.status.as_ref().filter(|s| s.state.is_live() && !s.vanished && seen.running);
        snapshot.pending_restart = self.pending_restart(live, &loaded);
        snapshot.about = self.about(live, seen.status.as_ref());
        snapshot.fallback_outputs = settings::device_menu(&self.endpoints, Flow::Render, &snapshot.priority.hands_free, loaded.fallback_output.as_deref());
        snapshot.fallback_inputs = settings::device_menu(&self.endpoints, Flow::Capture, &snapshot.priority.hands_free, loaded.fallback_input.as_deref());
        snapshot.platinum_names = loaded.channel_names == names::ChannelNames::Platinum;
        snapshot.game_links = self.presets.links.clone();
        snapshot.start_with_windows = autostart::starts_at_logon();
        snapshot.startup_blocked = autostart::logon_value().is_some() && !autostart::startup_approved();
        snapshot.installed = autostart::install_dir().is_ok_and(|folder| folder.join("platinum-start.exe").exists());
        snapshot.notices = self.notices.clone();
        snapshot.catalog = self.presets.catalog.clone();
        snapshot.settings = loaded;
        snapshot
    }

    /// What waits for the daemon's next start: the options it reads only then, compared with what
    /// it took. What it took is set when the window first sees its process (a daemon the window
    /// started, or restarted, read the file as it is then).
    fn pending_restart(&mut self, live: Option<&status::Status>, loaded: &config::Config) -> Vec<settings::Setting> {
        self.applied = match (live, self.applied.take()) {
            (Some(status), Some((pid, applied))) if pid == status.pid => Some((pid, applied)),
            (Some(status), _) => Some((status.pid, StartSettings::of_running(loaded, status))),
            (None, _) => None,
        };
        self.applied.as_ref().map(|(_, applied)| settings::pending(applied, &StartSettings::of(loaded))).unwrap_or_default()
    }

    /// The running daemon's process and memory, its microphone worker's, when the daemon last wrote
    /// status.txt (`last`, running or not), and where its log and the install folder are. Opens the
    /// two processes for reading only (`guard::process_memory`).
    fn about(&self, live: Option<&status::Status>, last: Option<&status::Status>) -> About {
        let folder = autostart::install_dir().ok().filter(|folder| folder.is_dir());
        let pid = live.map(|s| s.pid).filter(|&pid| pid != 0);
        let program = pid.and_then(status::process_path);
        let log = settings::log_candidates(program.as_deref(), folder.as_deref()).into_iter().find(|file| file.is_file());
        let worker_pid = live.map(|s| s.worker).filter(|&pid| pid != 0);
        About { pid, memory: pid.and_then(guard::process_memory), worker_pid, worker_memory: worker_pid.and_then(guard::process_memory), updated: last.map(|s| s.updated.clone()).unwrap_or_default(), log, folder }
    }

    /// Streamer mode as the mixer shows it: the personal mix's device (where the channels play,
    /// else the one remembered), the stream mix's menu and device, and the channels of each mix.
    /// Also tells the meters thread which device the Stream master's meter reads.
    fn streamer_data(&mut self, settings: &config::Config, routed: &[Option<String>], endpoints: &[Endpoint], outputs: &mut Vec<String>) -> StreamerData {
        let mode = if self.force_streamer { Mode::Streamer } else { settings.mode };
        let personal = streamer::personal_device(routed, settings.personal_device.as_deref());
        let personal_index = personal.as_deref().map_or(0, |id| output_index(&mut self.output_ids, outputs, endpoints, id));
        let virtual_name = streamer::virtual_stream(endpoints).map_or_else(|| "Stream (Platinum) (absent)".to_string(), |e| e.name.clone());
        let mut names = vec![tr("Aucune sortie").to_string(), virtual_name];
        let mut choices = vec![StreamDevice::None, StreamDevice::Virtual];
        for endpoint in endpoints.iter().filter(|e| e.flow == Flow::Render && e.virtual_role.is_none() && e.state == State::Active) {
            names.push(endpoint.name.clone());
            choices.push(StreamDevice::Device(endpoint.id.clone()));
        }
        let stream_index = match choices.iter().position(|c| *c == settings.stream_device) {
            Some(index) => index,
            // A real device that is unplugged: listed, as the outputs list one.
            None => {
                let id = settings.stream_device.to_text();
                let name = endpoints.iter().find(|e| e.id == id).map_or_else(|| id.clone(), |e| e.name.clone());
                names.push(format!("{name} (absent)"));
                choices.push(settings.stream_device.clone());
                choices.len() - 1
            }
        };
        let metered = (mode == Mode::Streamer).then(|| settings.stream_device.resolve(endpoints)).flatten();
        if let Ok(mut shared) = self.stream_device.lock() {
            shared.clone_from(&metered);
        }
        let level = metered.as_deref().and_then(|id| self.endpoint_volume(id).ok()).and_then(|volume| Some((volume.level().ok()?, volume.muted().unwrap_or(false))));
        StreamerData { mode, personal_index, outputs: names, choices, stream_index: stream_index as i32, personal_mix: settings.personal_mix, stream_mix: settings.stream_mix, level }
    }

    /// Whether an endpoint is a Bluetooth headset's hands-free side (a property read, kept).
    fn hands_free(&mut self, id: &str) -> bool {
        if let Some(known) = self.hands_free.get(id) {
            return *known;
        }
        let Some(devices) = &self.devices else { return false };
        let found = devices.is_hands_free(id);
        self.hands_free.insert(id.to_string(), found);
        found
    }

    /// The device manager's data: the lists, the devices they can hold, those in use (the
    /// microphone the daemon relays by its name, else the one chosen).
    fn priority_data(&mut self, loaded: &config::Config, routed: &[Option<String>], relayed: Option<&str>, mic: &MicData) -> PriorityData {
        let mut data = PriorityData {
            auto_switch: loaded.auto_switch,
            output_list: loaded.output_priority.clone(),
            input_list: loaded.input_priority.clone(),
            output_current: outputs::distinct(routed).into_iter().cloned().collect(),
            ..PriorityData::default()
        };
        let candidates: Vec<Endpoint> = self.endpoints.iter().filter(|e| !fallback::is_virtual(e)).cloned().collect();
        for endpoint in candidates {
            if self.hands_free(&endpoint.id) {
                data.hands_free.push(endpoint.id);
                continue;
            }
            match endpoint.flow {
                Flow::Render => data.output_devices.push(endpoint),
                Flow::Capture => data.input_devices.push(endpoint),
            }
        }
        let chosen = usize::try_from(mic.current).ok().and_then(|i| mic.inputs.get(i)).filter(|m| m.present).map(|m| m.id.clone());
        data.input_current = relayed.and_then(|name| data.input_devices.iter().find(|e| e.name == name)).map(|e| e.id.clone()).or(chosen);
        data
    }

    /// Whether the microphone's echo cancellation is on and listens to other outputs than those
    /// the channels play to. Reads only: the window offers to set it right.
    fn echo_is_stale(&mut self, routed: &[Option<String>], settings: &config::Config, endpoints: &[Endpoint]) -> bool {
        let stream_device = settings.stream_device.resolve(endpoints);
        let stream = StreamReference::of(settings.mode, &settings.channels, stream_device.as_deref());
        self.store("mic").is_ok_and(|mic| outputs::echo_stale(mic, routed, stream))
    }

    /// Takes in a finished daemon job: a failure is kept, shown and notified. After a start or a
    /// microphone change the user asked for, the echo cancellation is set on the outputs heard
    /// (the user's action is what writes), and a microphone chosen during a start is taken in.
    fn follow_job(&mut self) {
        let by_user = std::mem::take(&mut self.job_by_user);
        match background::take_outcome(&self.job) {
            Some(Err(error)) => {
                self.notify(tr("Platinum : opération non aboutie"), error.clone());
                self.last_error = Some(error);
                self.pending_microphone = None;
            }
            Some(Ok(())) => {
                if by_user && daemon::is_running() {
                    if let Err(error) = self.sync_echo_references() {
                        self.notify(tr("Annulation d'écho non mise à jour"), error);
                    }
                }
                self.restart_for_pending_microphone();
            }
            // Still under way: it keeps its origin.
            None => self.job_by_user = by_user,
        }
    }

    /// A microphone chosen while the daemon was starting: restart it when it relays another one.
    fn restart_for_pending_microphone(&mut self) {
        let Some(name) = self.pending_microphone.take() else { return };
        let seen = background::Seen::now();
        let relayed = seen.status.as_ref().filter(|s| s.state.is_live() && !s.vanished).map(|s| s.microphone.clone()).unwrap_or_default();
        if relayed == name {
            return;
        }
        let view = background::view(&seen, None, None, &name);
        if view.restarts_on_mic_change && background::begin(&self.job, Kind::Restart) {
            self.job_by_user = true;
        }
    }

    /// Applications routed lately whose sound did not move: they open their device themselves
    /// (GG warns about them too). Each is said once, and marked on its pill until routed again.
    fn check_routes(&mut self, apps: &[AppData]) {
        let mut refused = Vec::new();
        self.routes_sent.retain(|(exe, channel, when)| {
            if when.elapsed() < ROUTE_SETTLE {
                return true;
            }
            let app = apps.iter().find(|a| !a.capture && a.exe.eq_ignore_ascii_case(exe));
            match app {
                Some(app) if app.playing && app.channel_index != *channel => {
                    refused.push(exe.clone());
                    false
                }
                Some(app) if app.playing => false,
                _ => when.elapsed() < ROUTE_WATCH,
            }
        });
        for exe in refused {
            let name = exe.strip_suffix(".exe").or_else(|| exe.strip_suffix(".EXE")).unwrap_or(&exe).to_string();
            self.notify(trf!("{app} n'a pas suivi le transfert", app = name), trf!("{app} n'a pas autorisé Platinum à modifier ses paramètres audio. Ajustez les paramètres audio directement dans {app}.", app = name));
            self.refused.push(exe);
        }
    }

    /// The real microphones, the one chosen, and the virtual microphone's Windows volume.
    fn microphone_data(&mut self, configured: Option<&str>, seen: &background::Seen) -> MicData {
        let mut data = MicData { current: -1, ..MicData::default() };
        // Every capture endpoint, as the daemon searches them for the configured microphone.
        let captures: Vec<Endpoint> = self.endpoints.iter().filter(|e| e.flow == Flow::Capture).cloned().collect();
        for endpoint in captures.iter().filter(|e| e.state == State::Active && e.virtual_role.is_none()) {
            let hands_free = match (self.hands_free.get(&endpoint.id), &self.devices) {
                (Some(known), _) => *known,
                (None, Some(devices)) => *self.hands_free.entry(endpoint.id.clone()).or_insert_with(|| devices.is_hands_free(&endpoint.id)),
                (None, None) => false,
            };
            data.inputs.push(Input { id: endpoint.id.clone(), name: endpoint.name.clone(), hands_free, present: true });
        }
        // The user's choice first; without one, what a running daemon says it relays.
        let wanted = match configured {
            Some(spec) => match platinum::devices::find_device(&captures, spec) {
                Ok(endpoint) => Some((endpoint.id.clone(), endpoint.name.clone())),
                Err(_) => Some((spec.to_string(), spec.to_string())),
            },
            None => seen.status.as_ref().filter(|s| s.state.is_live() && !s.vanished && !s.microphone.is_empty()).map(|s| {
                let id = captures.iter().find(|e| e.name == s.microphone).map_or_else(|| s.microphone.clone(), |e| e.id.clone());
                (id, s.microphone.clone())
            }),
        };
        if let Some((id, name)) = wanted {
            data.current = match data.inputs.iter().position(|m| m.id == id) {
                Some(index) => index as i32,
                None => {
                    data.inputs.push(Input { id, name: format!("{name} (absent)"), hands_free: false, present: false });
                    (data.inputs.len() - 1) as i32
                }
            };
        }
        if let Some(id) = self.microphone_endpoint().map(|e| e.id.clone()) {
            if let Ok(volume) = self.endpoint_volume(&id) {
                data.volume = volume.level().ok();
                data.muted = volume.muted().unwrap_or(false);
            }
        }
        data
    }

    /// The applications that play to an output (`Flow::Render`) or record from a microphone
    /// (`Flow::Capture`), each with the channel it actually goes through. An application with no
    /// device of its own follows the default device, which is a channel while the daemon runs
    /// (Gaming, or Chat for communications; the virtual microphone for recording); it is listed
    /// there.
    fn applications(&self, endpoints: &[Endpoint], flow: Flow) -> Vec<AppData> {
        let Some(devices) = &self.devices else { return Vec::new() };
        let processes = routing::all_processes().unwrap_or_default();
        let routing = routing::Routing::open().ok();
        let me = std::process::id();
        let default = devices.default(flow, Role::Console).ok().flatten();
        // Every session: application, process, endpoint, playing.
        let mut sessions: Vec<(&str, u32, &Endpoint, bool)> = Vec::new();
        for endpoint in endpoints.iter().filter(|e| e.flow == flow && e.state == State::Active) {
            // The virtual microphone's playback side only carries Platinum's own relay.
            if flow == Flow::Render && session::virtual_kind(endpoint).as_deref() == Some("microphone") {
                continue;
            }
            for s in devices.sessions(&endpoint.id).unwrap_or_default() {
                if s.pid == 0 || s.pid == me {
                    continue;
                }
                let Some((_, exe)) = processes.iter().find(|(pid, _)| *pid == s.pid) else { continue };
                if NOT_APPLICATIONS.iter().any(|own| exe.eq_ignore_ascii_case(own)) {
                    continue;
                }
                sessions.push((exe.as_str(), s.pid, endpoint, s.active));
            }
        }
        let mut apps: Vec<AppData> = Vec::new();
        for (exe, pid, _, _) in &sessions {
            if apps.iter().any(|a| a.exe.eq_ignore_ascii_case(exe)) {
                continue;
            }
            let own: Vec<&(&str, u32, &Endpoint, bool)> = sessions.iter().filter(|s| s.0.eq_ignore_ascii_case(exe)).collect();
            let routed = routing.as_ref().and_then(|r| r.get(*pid, flow).ok().flatten());
            // Where it is heard, most telling first, playing sessions before silent ones so that an
            // application with sessions on several devices stays in one place from one snapshot to
            // the next: the device Windows routes it to, when its sound is there; a device it opened
            // by itself (Discord's voice on Chat while its notification sounds go elsewhere); then
            // the default device. A route alone places nothing: an application that opens its device
            // itself does not follow it, and its pill would show a route that is not heard.
            let routed_to = routed.as_deref().and_then(|id| endpoints.iter().find(|e| e.id == id));
            let has_session = |endpoint: &Endpoint, playing: bool| own.iter().any(|s| s.2.id == endpoint.id && (!playing || s.3));
            let chosen = |playing: bool| own.iter().find(|s| (!playing || s.3) && Some(&s.2.id) != default.as_ref()).map(|s| s.2);
            let on_default = |playing: bool| own.iter().find(|s| (!playing || s.3) && Some(&s.2.id) == default.as_ref()).map(|s| s.2);
            let routed_heard = |playing: bool| routed_to.filter(|e| has_session(e, playing));
            let found = routed_heard(true).or_else(|| chosen(true)).or_else(|| on_default(true)).or_else(|| routed_heard(false)).or_else(|| chosen(false)).or_else(|| on_default(false));
            let Some(endpoint) = found else { continue };
            let channel_index = channel_of(endpoint);
            apps.push(AppData {
                exe: exe.to_string(),
                playing: own.iter().any(|s| s.3),
                capture: flow == Flow::Capture,
                channel_index,
                own_device: OWN_DEVICE_APPS.iter().any(|name| exe.eq_ignore_ascii_case(name)),
                refused: flow == Flow::Render && self.refused.iter().any(|e| e.eq_ignore_ascii_case(exe)),
                follows_default: routed.is_none(),
                device: if channel_index == 0 { endpoint.name.clone() } else { String::new() },
            });
        }
        apps.sort_by_key(|a| a.exe.to_lowercase());
        apps
    }

    /// How long to wait for the next snapshot when nothing is asked: shorter while the daemon
    /// starts or stops, so that the window follows it closely.
    fn idle_period(&self) -> Duration {
        if background::current(&self.job).is_some() { Duration::from_millis(700) } else { Duration::from_secs(2) }
    }
}

/// The position of a device in the output list, adding it when it is not plugged in.
fn output_index(ids: &mut Vec<Option<String>>, names: &mut Vec<String>, endpoints: &[Endpoint], id: &str) -> i32 {
    if let Some(index) = ids.iter().position(|o| o.as_deref() == Some(id)) {
        return index as i32;
    }
    let name = endpoints.iter().find(|e| e.id == id).map_or(id.to_string(), |e| e.name.clone());
    ids.push(Some(id.to_string()));
    names.push(format!("{name} (absent)"));
    (ids.len() - 1) as i32
}

/// The notice and the reason shown when Task Manager keeps Platinum from starting with Windows.
pub const STARTUP_BLOCKED: &str = mark("Platinum est désactivé dans le Gestionnaire des tâches (Applications de démarrage) : activez-le là pour qu'il démarre avec Windows");

/// How a change of the presets finds what to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Diff {
    /// A knob or a switch: what differs from the current preset (a knob sends dozens of changes a
    /// second), plus the settings GG's rules tie to them, checked against the module.
    Edit,
    /// A preset chosen, reset or imported: every setting compared with what the module holds,
    /// GG's own rule (B17), so that a module changed behind Platinum's back is set right.
    Apply,
}

/// Writes `writes` (every setting of `next` for `Diff::Apply`, the changes for `Diff::Edit`) into
/// a store, keeping only what the module does not hold already, and returns what the channel then
/// holds with the outcome. Reads the module back only after a failure: a knob being turned writes
/// dozens of times a second.
fn write_into(store: &Store, role: preset::Role, next: &Preset, writes: Vec<preset::Write>, diff: Diff) -> (Preset, Result<(), String>) {
    let writes = match diff {
        // A setting that cannot be read is written anyway.
        Diff::Apply => preset::pending(store, &writes).unwrap_or(writes),
        Diff::Edit => {
            let linked = linked_writes(store, next, &writes);
            writes.into_iter().chain(linked).collect()
        }
    };
    if writes.is_empty() {
        return (*next, Ok(()));
    }
    let outcome = preset::apply(store, &writes).map(|_| ());
    let current = match outcome.is_err().then(|| preset::from_store(store, role).ok()).flatten() {
        Some(module) => presets::reconcile(next, module),
        None => *next,
    };
    (current, outcome)
}

/// The settings GG's rules tie to those in `writes` (`preset::linked`) that the module does not
/// hold as `next` wants: a module changed one setting at a time (an older window, the command line)
/// is set right by the next edit of the setting they follow. Nothing when they cannot be read.
fn linked_writes(store: &Store, next: &Preset, writes: &[preset::Write]) -> Vec<preset::Write> {
    let ids: Vec<i32> = writes.iter().flat_map(|w| preset::linked(w.id).iter().copied()).filter(|id| !writes.iter().any(|w| w.id == *id)).collect();
    if ids.is_empty() {
        return Vec::new();
    }
    let wanted: Vec<preset::Write> = preset::writes(next).into_iter().filter(|w| ids.contains(&w.id)).collect();
    preset::pending(store, &wanted).unwrap_or_default()
}

/// Whether a change of the microphone's preset switched echo cancellation or AI noise cancelling
/// on: the hosted engine then listens to its references, which must name the outputs heard.
fn echo_switched_on(before: &Preset, after: &Preset) -> bool {
    match (before, after) {
        (Preset::Capture(b), Preset::Capture(a)) => (!b.acoustic_echo_canceling && a.acoustic_echo_canceling) || (!b.noise_canceling.enabled && a.noise_canceling.enabled),
        _ => false,
    }
}

/// The window's place for an endpoint: 1 to 4 for the channels Gaming to Aux, `MIC_TARGET` for
/// the virtual microphone, 0 for a real device.
fn channel_of(endpoint: &Endpoint) -> i32 {
    match session::virtual_kind(endpoint).as_deref() {
        Some("gaming") => 1,
        Some("chat") => 2,
        Some("media") => 3,
        Some("aux") => 4,
        Some("microphone") if endpoint.flow == Flow::Capture => MIC_TARGET,
        _ => 0,
    }
}

/// Routes every process of an application to `destination` (`None`: the default device).
fn set_route(exe: &str, flow: Flow, destination: Option<&str>) -> Result<(), String> {
    let routing = routing::Routing::open().map_err(text)?;
    let processes = routing::processes(exe).map_err(text)?;
    let accepted = processes.iter().filter(|(pid, _)| routing.set(*pid, flow, destination).is_ok()).count();
    if accepted == 0 { Err(trf!("Windows a refusé de rediriger {app}", app = exe)) } else { Ok(()) }
}

/// A snapshot comes at least this long after the last command: a fader being dragged sends dozens
/// a second, and a snapshot taken mid-drag would be thrown away by the window (it would undo the
/// moves that follow it).
const SPACING: Duration = Duration::from_millis(500);

/// The devices the meters thread reads besides the channels, set by the engine: the real output
/// the channels play to (the Master's meter), and the stream mix's device in streamer mode (the
/// Stream master's meter).
#[derive(Clone, Default)]
pub struct MeterDevices {
    pub master: Arc<Mutex<Option<String>>>,
    pub stream: Arc<Mutex<Option<String>>>,
}

/// The engine loop: carries out commands as they come, and sends a fresh snapshot once they stop
/// coming, and every two seconds otherwise. The Master's position is saved at the same moments,
/// not on every move.
pub fn run(commands: Receiver<Command>, meter_devices: MeterDevices, read_only: bool, publish: impl Fn(Snapshot)) {
    let mut engine = Engine::new(meter_devices, read_only);
    engine.start_on_open();
    publish(engine.snapshot());
    let mut idle = Instant::now() + engine.idle_period();
    let mut last_command: Option<Instant> = None;
    loop {
        let due = last_command.map_or(idle, |last| last + SPACING);
        match commands.recv_timeout(due.saturating_duration_since(Instant::now())) {
            Ok(command) => {
                engine.handle(command);
                while let Ok(more) = commands.try_recv() {
                    engine.handle(more);
                }
                last_command = Some(Instant::now());
            }
            Err(RecvTimeoutError::Timeout) => {
                engine.save_master();
                engine.save_presets();
                publish(engine.snapshot());
                last_command = None;
                idle = Instant::now() + engine.idle_period();
            }
            // The window closed (every command it sent has been carried out by now): save, then
            // let a restart under way reach its start, or the user would be left without Platinum.
            Err(RecvTimeoutError::Disconnected) => {
                engine.save_master();
                engine.save_presets();
                // Its end is taken in as usual (a pending microphone may restart the daemon once
                // more, and is waited for too).
                background::wait_idle(&engine.job);
                engine.follow_job();
                background::wait_idle(&engine.job);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_outputs_are_listed_once() {
        let mut ids = vec![None, Some("a".to_string())];
        let mut names = vec!["Aucune sortie".to_string(), "A".to_string()];
        assert_eq!(output_index(&mut ids, &mut names, &[], "a"), 1);
        assert_eq!(output_index(&mut ids, &mut names, &[], "b"), 2);
        assert_eq!(output_index(&mut ids, &mut names, &[], "b"), 2);
        assert_eq!(names, ["Aucune sortie", "A", "b (absent)"]);
    }

    #[test]
    fn echo_turned_on_is_told_apart() {
        let before = platinum::preset::flat(platinum::preset::Role::ChatCapture);
        let mut after = before;
        presets::apply(&mut after, Edit::Set(presets::Field::EchoCanceling, 1.0));
        assert!(echo_switched_on(&before, &after) && !echo_switched_on(&after, &before));
    }

    #[test]
    fn every_command_says_what_failed_in_french() {
        assert_eq!(context(&Command::SetVolume("game".into(), 0.5)), "Volume de Gaming non réglé");
        assert_eq!(context(&Command::Route("spotify.exe".into(), 4)), "spotify.exe non transféré");
        assert!(context(&Command::StartDaemon).contains("Platinum"));
        assert_eq!(context(&Command::SetStreamVolume("chat".into(), 0.5)), "Volume de Chat dans le mix de stream non réglé");
        assert_eq!(context(&Command::SetMode(Mode::Streamer)), "Mode streamer non activé");
        assert!(context(&Command::SetInclusion("aux".into(), Mix::Stream, true)).contains("mix de stream"));
    }

    #[test]
    fn the_settings_pages_commands_say_what_failed() {
        assert_eq!(context(&Command::Set(Change::Gain(0.5))), "« Volume du micro » non enregistré");
        assert_eq!(context(&Command::SetChannelNames(false)), "Noms des canaux non changés");
        assert_eq!(context(&Command::RestartDaemon), "Platinum n'a pas redémarré");
    }

    #[test]
    fn every_channel_takes_its_kind_of_preset() {
        for (def, role) in CHANNELS.iter().zip(ROLES) {
            assert_eq!(platinum::preset::Role::from_channel(def.channel), role, "{}", def.key);
        }
    }

    #[test]
    fn the_strips_come_first_and_the_microphone_last() {
        assert_eq!(CHANNELS[MIC].key, "mic");
        assert!(CHANNELS[..STRIPS].iter().all(|c| c.volume.is_some() && c.muted.is_some() && c.output.is_some()));
    }
}
