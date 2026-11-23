//! Platinum's own configuration: which microphone the daemon relays, which devices become the
//! defaults when it stops, and how the window behaves.
//!
//! One file, `%USERPROFILE%\Platinum\platinum.ini`, next to the installed programs. Not AppData and
//! not the registry: packaged applications (the Claude app among them) redirect both into a private
//! copy that Windows and the other programs never see.
//!
//! The format is plain `key = value` lines with `#` comments, so that the file can be read and
//! fixed by hand. Unknown keys and bad values are reported and skipped, never fatal: a damaged file
//! must not keep the sound from starting.
//!
//! Before this file existed, the daemon's settings were the arguments of the logon command
//! (`platinum-start.exe daemon Seiren --fallback-output AirPods`). While the file is missing they
//! are read from there, in memory only: loading never writes anything.
//!
//! The backup switch's priority lists (see `fallback`) are one line per device, in order:
//! `output_device = <identifier> | <name>`, or `output_excluded = …` for a device never chosen
//! automatically; `input_device` and `input_excluded` for the microphones. The name is the one the
//! device had when last seen, so that an unplugged device can still be shown.
//!
//! Keyboard shortcuts (see `hotkeys`) are one line per bound action,
//! `shortcut_<action> = Ctrl+Alt+Up`; none is bound by default. The overlays' options (see
//! `overlay`) are `overlays`, `overlay_position` and one switch per kind.
//!
//! Streamer mode (see `streamer`): `mode` (classic by default, so that nothing changes until the
//! user switches), the stream mix's device, the personal mix's device for when no channel plays to
//! it, the channels of each mix, and the classic outputs to put back when leaving streamer mode.
//!
//! Remote streaming (see `remote`): `follow_streaming` (on by default, at the user's request), the
//! name fragments of the streaming outputs, and whether the applications playing on one go through
//! Gaming.
//!
//! Preset per game (see `games`): `game_presets`, and GG's Arena 7 rule, `arena_speakers`; both on
//! by default, at the user's request.
//!
//! Language (see `i18n`): `language`, `auto` by default (Windows' display language).
//!
//! The microphone worker's replacement (see `recycle`): `engine_recycle_mb`, 400 by default; it is
//! not in the window, being a limit to leave alone unless the daemon's log says otherwise.
//!
//! The channels' names in Windows (see `names`): `channel_names`, Platinum's by default, at the
//! user's request. A file written before the key existed, by a Platinum whose channels had gone
//! back to SteelSeries' names (`original-names.txt` absent), reads as `steelseries`: that user
//! chose them, and an update must not rename the devices (`names_for_old_file`).
//!
//! Crash reports (see `crash`): `crash_reports`, `off` by default. The reports are written on this
//! PC either way; the window offers to send one only with `ask`.

use crate::autostart;
use crate::crash::Consent;
use crate::daemon::DEFAULT_CHANNELS;
use crate::fallback::PriorityDevice;
use crate::hotkeys::{Action, Hotkey};
use crate::i18n::Choice;
use crate::mixer::STRIPS;
use crate::names::ChannelNames;
use crate::overlay::{self, Position};
use crate::remote;
use crate::streamer::{self, Inclusion, Mode, StreamDevice};
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = "platinum.ini";
/// A microphone worker starts near 40 MB with the effects engine and was seen growing about 4 MB a
/// minute: 400 MB means a replacement about every hour and a half, well before the 738 MB a daemon
/// hosting the engine was once found at.
pub const DEFAULT_ENGINE_RECYCLE_MB: u32 = 400;
/// Every channel the daemon knows how to bring up.
pub const KNOWN_CHANNELS: [&str; 6] = ["game", "chat", "media", "aux", "mic", "stream"];

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// The real microphone relayed into the virtual one: its identifier, its name or part of it.
    /// `None`: none chosen yet, and the virtual microphone stays silent.
    pub microphone: Option<String>,
    /// Output made the default for every role when the daemon stops (`None`: the defaults found
    /// when it started are put back).
    pub fallback_output: Option<String>,
    /// Input made the default when the daemon stops.
    pub fallback_input: Option<String>,
    /// Channels the daemon brings up, from `KNOWN_CHANNELS`.
    pub channels: Vec<String>,
    /// Runs the microphone through the vendor's effects engine.
    pub effects: bool,
    /// Linear microphone volume, 0 to 1.
    pub gain: f32,
    /// Private memory of the microphone worker (which hosts the effects engine), in MB, past which
    /// the daemon replaces it during a silence of the microphone, once it has run an hour
    /// (`recycle`); 0 never does. Read when the daemon starts.
    pub engine_recycle_mb: u32,
    /// The window starts the daemon when it opens and finds it stopped.
    pub start_on_open: bool,
    /// GG's automatic backup device switch: the daemon keeps the output channels on the first
    /// usable device of `output_priority`, and relays the first usable microphone of
    /// `input_priority` while `microphone` is missing. Off by default, as in GG.
    pub auto_switch: bool,
    /// The outputs' priority list, shared by Gaming, Chat, Media and Aux, first to last.
    pub output_priority: Vec<PriorityDevice>,
    /// The microphones' priority list, behind `microphone`.
    pub input_priority: Vec<PriorityDevice>,
    /// The bound keyboard shortcuts, in `Action::ALL` order; empty by default.
    pub shortcuts: Vec<(Action, Hotkey)>,
    /// The on-screen overlays shown when a shortcut changes something.
    pub overlays: overlay::Settings,
    /// Classic mixer or GG's streamer mode.
    pub mode: Mode,
    /// Where the stream mix goes (the Stream virtual device by default).
    pub stream_device: StreamDevice,
    /// The personal mix's device, used when no channel plays to it (otherwise the device the
    /// channels play to is the personal mix's).
    pub personal_device: Option<String>,
    /// The strips in each mix, Gaming to Aux; all of them by default, as GG.
    pub personal_mix: Inclusion,
    pub stream_mix: Inclusion,
    /// The classic outputs, Gaming to Aux, remembered on entering streamer mode and put back on
    /// leaving it; `None` when nothing is remembered.
    pub classic_outputs: Option<[Option<String>; STRIPS]>,
    /// Follow a remote streaming session (Sunshine, Steam Remote Play): while the default output is
    /// a streaming sink, the channels play to it and the applications on it go through Gaming.
    pub follow_streaming: bool,
    /// Name fragments of the outputs a streaming host makes the default during a session.
    pub streaming_sinks: Vec<String>,
    /// During a session, the applications playing on the sink are routed to Gaming.
    pub streaming_route_apps: bool,
    /// GG's QuickSet: while a game linked to a Game preset runs, Gaming uses that preset, and the
    /// preset of before comes back when it exits (`games`).
    pub game_presets: bool,
    /// GG's Arena 7 rule: on a channel that plays to Arena 7 speakers, spatial audio goes to
    /// speaker mode when a preset is applied or the output changes to them.
    pub arena_speakers: bool,
    /// The language of the window, the tray icon and the overlays: Windows' display language (when
    /// Platinum speaks it, else English), or one of `i18n::Language`. The window applies it at once,
    /// the daemon within 2 s (it reads this file on every tick).
    pub language: Choice,
    /// The names the virtual channels carry in Windows: Platinum's ("Gaming (Platinum)") or
    /// SteelSeries'. The daemon applies them when it starts, the window when the user switches.
    pub channel_names: ChannelNames,
    /// Whether the window offers to send a crash report after a crash (never sent without the
    /// user's click); reports are written on this PC either way.
    pub crash_reports: Consent,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            microphone: None,
            fallback_output: None,
            fallback_input: None,
            channels: DEFAULT_CHANNELS.iter().map(|c| c.to_string()).collect(),
            effects: true,
            gain: 1.0,
            engine_recycle_mb: DEFAULT_ENGINE_RECYCLE_MB,
            start_on_open: true,
            auto_switch: false,
            output_priority: Vec::new(),
            input_priority: Vec::new(),
            shortcuts: Vec::new(),
            overlays: overlay::Settings::default(),
            mode: Mode::Classic,
            stream_device: StreamDevice::Virtual,
            personal_device: None,
            personal_mix: streamer::ALL_STRIPS,
            stream_mix: streamer::ALL_STRIPS,
            classic_outputs: None,
            follow_streaming: true,
            streaming_sinks: remote::DEFAULT_SINKS.iter().map(|s| s.to_string()).collect(),
            streaming_route_apps: true,
            game_presets: true,
            arena_speakers: true,
            language: Choice::Auto,
            channel_names: ChannelNames::Platinum,
            crash_reports: Consent::Off,
        }
    }
}

/// Where a loaded configuration came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// `platinum.ini`.
    File,
    /// No file yet: the arguments of the logon command, as written before the file existed.
    LogonCommand,
    /// Neither: the built-in defaults.
    Defaults,
}

/// A configuration, where it came from, and what was wrong in the file (in French, for display).
pub struct Loaded {
    pub config: Config,
    pub origin: Origin,
    pub problems: Vec<String>,
    /// The file exists but could not be read (locked by another program, for instance): `config`
    /// holds defaults, and saving them would replace the user's settings.
    pub unreadable: bool,
}

impl Loaded {
    /// The configuration to change and save, refused while the file exists but cannot be read:
    /// saving then would put the defaults over every other setting the user has. Error in French.
    pub fn for_saving(self) -> Result<Config, String> {
        if self.unreadable {
            return Err(crate::trf!("{file} est illisible : réglage non enregistré, pour ne pas remplacer vos autres réglages", file = FILE_NAME));
        }
        Ok(self.config)
    }
}

/// `%USERPROFILE%\Platinum\platinum.ini`.
pub fn path() -> Result<PathBuf, String> {
    Ok(autostart::install_dir()?.join(FILE_NAME))
}

/// The configuration as it stands: the file, else the logon command, else the defaults.
pub fn load() -> Config {
    load_full().config
}

/// Like `load`, saying where the values came from and what was skipped. Writes nothing.
pub fn load_full() -> Loaded {
    let mut problems = Vec::new();
    let mut unreadable = false;
    if let Ok(file) = path() {
        match read_text(&file) {
            Ok(text) => {
                let (mut config, skipped) = Config::parse(&text);
                if let Some(names) = names_for_old_file(sets_key(&text, "channel_names"), crate::names::originals_exist()) {
                    config.channel_names = names;
                }
                return Loaded { config, origin: Origin::File, problems: skipped, unreadable: false };
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                problems.push(format!("{} est illisible ({error}) : réglages par défaut", file.display()));
                unreadable = true;
            }
        }
    }
    let from_logon = autostart::logon_value().and_then(|command| Config::from_logon_command(&command));
    match from_logon {
        Some(config) if config != Config::default() => Loaded { config, origin: Origin::LogonCommand, problems, unreadable },
        _ => Loaded { config: Config::default(), origin: Origin::Defaults, problems, unreadable },
    }
}

/// Reads one configuration file: the values, and the lines that were skipped.
pub fn load_from(file: &Path) -> std::io::Result<(Config, Vec<String>)> {
    Ok(Config::parse(&read_text(file)?))
}

/// A configuration file's text. Lossy: a file saved by hand in the ANSI code page still gives its
/// ASCII keys and values.
fn read_text(file: &Path) -> std::io::Result<String> {
    Ok(String::from_utf8_lossy(&std::fs::read(file)?).into_owned())
}

/// Whether a file's text sets `key` (a file written before the key existed does not).
fn sets_key(text: &str, key: &str) -> bool {
    text.lines().filter_map(|line| line.trim().trim_start_matches('\u{feff}').split_once('=')).any(|(name, _)| name.trim().eq_ignore_ascii_case(key))
}

/// The channels' names for a file that does not set `channel_names`: Platinum's (the default)
/// unless the channels had gone back to SteelSeries' names, which only an earlier Platinum's
/// `rename --undo` or settings page did (`original-names.txt` absent: `originals` false). `None`
/// when the file sets the key, which then decides.
pub fn names_for_old_file(sets_names: bool, originals: bool) -> Option<ChannelNames> {
    (!sets_names && !originals).then_some(ChannelNames::SteelSeries)
}

/// At the daemon's start: writes `channel_names` into a platinum.ini written before the key
/// existed, with the value `load_full` reads for it, so that the choice no longer hangs on
/// `original-names.txt`. Returns the value written; `None` when there was nothing to do (no file,
/// or the key is there). Error in English or French, for the log.
pub fn settle_channel_names() -> Result<Option<ChannelNames>, String> {
    let file = path()?;
    let text = match read_text(&file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot read {}: {error}", file.display())),
    };
    if sets_key(&text, "channel_names") {
        return Ok(None);
    }
    let config = load_full().for_saving()?;
    config.save()?;
    Ok(Some(config.channel_names))
}

impl Config {
    /// Reads the text of a configuration file. Lines that cannot be used are skipped and listed
    /// (in French); the keys they would have set keep their default.
    pub fn parse(text: &str) -> (Config, Vec<String>) {
        let mut config = Config::default();
        let mut problems = Vec::new();
        for (index, line) in text.trim_start_matches('\u{feff}').lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                problems.push(format!("ligne {} : « {line} » n'est pas de la forme clé = valeur", index + 1));
                continue;
            };
            if let Err(problem) = config.set(key.trim(), unquote(value.trim())) {
                problems.push(format!("ligne {} : {problem}", index + 1));
            }
        }
        (config, problems)
    }

    /// Sets one key from its text as written in the file. The error (in French) says why the value
    /// was refused; the key then keeps its value. A priority list's key adds one device at the end
    /// of its list.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key.to_ascii_lowercase().as_str() {
            "auto_switch" => self.auto_switch = parse_bool(value).ok_or_else(|| format!("auto_switch = {value} : true ou false attendu"))?,
            list @ ("output_device" | "output_excluded" | "input_device" | "input_excluded") => {
                let mut entry = parse_priority(value).ok_or_else(|| format!("{list} = {value} : « identifiant | nom » attendu"))?;
                entry.excluded = list.ends_with("excluded");
                let target = if list.starts_with("output") { &mut self.output_priority } else { &mut self.input_priority };
                if target.iter().any(|e| e.id == entry.id) {
                    return Err(format!("{} est déjà dans la liste : ligne ignorée", entry.id));
                }
                target.push(entry);
            }
            "microphone" => self.microphone = optional(value),
            "fallback_output" => self.fallback_output = optional(value),
            "fallback_input" => self.fallback_input = optional(value),
            "channels" => match parse_channels(value) {
                Ok(channels) if !channels.is_empty() => self.channels = channels,
                Ok(_) => return Err("channels est vide : canaux par défaut".into()),
                Err(unknown) => return Err(format!("canal inconnu « {unknown} » (game, chat, media, aux, mic ou stream)")),
            },
            "effects" => self.effects = parse_bool(value).ok_or_else(|| format!("effects = {value} : true ou false attendu"))?,
            "gain" => self.gain = parse_gain(value).ok_or_else(|| format!("gain = {value} : nombre de 0 à 1 attendu"))?,
            "engine_recycle_mb" => self.engine_recycle_mb = value.trim().parse().map_err(|_| format!("engine_recycle_mb = {value} : nombre entier de Mo attendu (0 pour jamais)"))?,
            "start_on_open" => self.start_on_open = parse_bool(value).ok_or_else(|| format!("start_on_open = {value} : true ou false attendu"))?,
            "overlays" => self.overlays.enabled = parse_bool(value).ok_or_else(|| format!("overlays = {value} : true ou false attendu"))?,
            "overlay_volume" => self.overlays.volume = parse_bool(value).ok_or_else(|| format!("overlay_volume = {value} : true ou false attendu"))?,
            "overlay_mute" => self.overlays.mute = parse_bool(value).ok_or_else(|| format!("overlay_mute = {value} : true ou false attendu"))?,
            "overlay_device" => self.overlays.device = parse_bool(value).ok_or_else(|| format!("overlay_device = {value} : true ou false attendu"))?,
            "overlay_position" => {
                self.overlays.position = Position::parse(value).ok_or_else(|| format!("overlay_position = {value} : top_left, top_right, center_left, center_right, bottom_left ou bottom_right attendu"))?
            }
            "mode" => self.mode = Mode::parse(value).ok_or_else(|| format!("mode = {value} : classic ou streamer attendu"))?,
            "stream_device" => self.stream_device = StreamDevice::parse(value),
            "personal_device" => self.personal_device = optional(value),
            mix @ ("personal_mix" | "stream_mix") => {
                let inclusion = streamer::parse_inclusion(value).map_err(|unknown| format!("{mix} : canal inconnu « {unknown} » (game, chat, media ou aux)"))?;
                if mix == "personal_mix" {
                    self.personal_mix = inclusion;
                } else {
                    self.stream_mix = inclusion;
                }
            }
            "classic_outputs" => {
                self.classic_outputs = match value.trim() {
                    "" => None,
                    text => Some(streamer::parse_outputs(text).ok_or_else(|| format!("classic_outputs = {value} : quatre sorties séparées par des virgules attendues"))?),
                }
            }
            "follow_streaming" => self.follow_streaming = parse_bool(value).ok_or_else(|| format!("follow_streaming = {value} : true ou false attendu"))?,
            "streaming_sinks" => self.streaming_sinks = parse_list(value),
            "streaming_route_apps" => self.streaming_route_apps = parse_bool(value).ok_or_else(|| format!("streaming_route_apps = {value} : true ou false attendu"))?,
            "game_presets" => self.game_presets = parse_bool(value).ok_or_else(|| format!("game_presets = {value} : true ou false attendu"))?,
            "arena_speakers" => self.arena_speakers = parse_bool(value).ok_or_else(|| format!("arena_speakers = {value} : true ou false attendu"))?,
            "language" => self.language = Choice::parse(value).ok_or_else(|| format!("language = {value} : auto, fr, en, es, de, it ou pt attendu"))?,
            "channel_names" => self.channel_names = ChannelNames::parse(value).ok_or_else(|| format!("channel_names = {value} : platinum ou steelseries attendu"))?,
            "crash_reports" => self.crash_reports = Consent::parse(value).ok_or_else(|| format!("crash_reports = {value} : off ou ask attendu"))?,
            other if other.starts_with("shortcut_") => {
                let action = Action::parse_key(&other["shortcut_".len()..]).ok_or_else(|| format!("action de raccourci inconnue « {key} »"))?;
                let hotkey = match value.trim() {
                    "" => None,
                    text => Some(Hotkey::parse(text).map_err(|problem| format!("{key} = {value} : {problem}"))?),
                };
                self.set_shortcut(action, hotkey);
            }
            _ => return Err(format!("clé inconnue « {key} »")),
        }
        Ok(())
    }

    /// The combination bound to an action, if any.
    pub fn shortcut(&self, action: Action) -> Option<Hotkey> {
        self.shortcuts.iter().find(|(a, _)| *a == action).map(|(_, hotkey)| *hotkey)
    }

    /// Binds an action to a combination (`None`: unbinds it), keeping `Action::ALL` order.
    pub fn set_shortcut(&mut self, action: Action, hotkey: Option<Hotkey>) {
        self.shortcuts.retain(|(a, _)| *a != action);
        if let Some(hotkey) = hotkey {
            self.shortcuts.push((action, hotkey));
            self.shortcuts.sort_by_key(|(a, _)| a.index());
        }
    }

    /// The file's text: every key, each with a comment saying what it does.
    pub fn to_text(&self) -> String {
        let text = |value: &Option<String>| value.as_deref().map(single_line).unwrap_or_default();
        format!(
            "\
# Platinum : réglages de la tâche de fond et de la fenêtre.
# Une ligne par réglage, « clé = valeur ». Les lignes qui commencent par # sont ignorées.
# La fenêtre réécrit ce fichier quand on y change un réglage.

# Micro réel relayé vers le micro virtuel : son nom, ou une partie du nom (« Seiren »).
# Vide : aucun micro, le micro virtuel reste muet.
microphone = {}

# Sortie et entrée remises par défaut quand Platinum s'arrête.
# Vides : celles qui l'étaient quand Platinum a démarré.
fallback_output = {}
fallback_input = {}

# Canaux activés, séparés par des virgules : game, chat, media, aux, mic, stream.
channels = {}

# Effets du micro (moteur d'effets de SteelSeries déjà installé) : true ou false.
effects = {}

# Volume du micro, de 0 à 1.
gain = {}

# Mémoire du processus du micro (moteur d'effets), en Mo, au-delà de laquelle il est remplacé
# pendant un silence du micro (coupé une fraction de seconde), au plus tôt une heure après son
# démarrage.
# 0 : jamais. Lu au démarrage de Platinum.
engine_recycle_mb = {}

# Ouvrir la fenêtre démarre Platinum s'il est arrêté : true ou false.
start_on_open = {}

# Changement automatique de périphérique de secours : true ou false.
# Platinum garde les canaux de sortie sur le premier périphérique branché de la liste des sorties,
# et relaie le premier micro branché de la liste des micros tant que le micro choisi est absent.
auto_switch = {}

# Listes de priorité, du premier au dernier : « identifiant | nom », une ligne par périphérique.
# Sorties (partagées par Gaming, Chat, Media et Aux) : output_device ; output_excluded pour un
# périphérique exclu, jamais choisi automatiquement. Micros : input_device et input_excluded.
{}{}
# Superpositions : un petit panneau quand un raccourci change un volume, une coupure du son ou
# le périphérique de sortie. overlays = true ou false ; position : top_left, top_right,
# center_left, center_right, bottom_left ou bottom_right ; puis un interrupteur par type.
overlays = {}
overlay_position = {}
overlay_volume = {}
overlay_mute = {}
overlay_device = {}

# Mode du mixeur : classic (une sortie par canal) ou streamer (un mix personnel et un mix de
# stream, comme le Streamer mode de GG). La fenêtre change ce réglage : passer en streamer ajoute
# stream aux canaux activés, revenir en classic l'enlève.
mode = {}

# Mode streamer. Sortie du mix de stream : vide pour Stream (Platinum), le périphérique virtuel
# qu'enregistrent les logiciels de streaming ; none pour aucune ; sinon l'identifiant d'une sortie.
stream_device = {}
# Sortie du mix personnel quand aucun canal n'y joue (sinon, celle où jouent les canaux).
personal_device = {}
# Canaux de chaque mix, parmi game, chat, media et aux ; gardés d'un passage au mode streamer à
# l'autre.
personal_mix = {}
stream_mix = {}
# Sorties du mode classique (Gaming, Chat, Media, Aux ; vide pour aucune), retenues en passant au
# mode streamer et remises en le quittant.
classic_outputs = {}

# Streaming à distance (Sunshine et Moonlight, Steam Remote Play) : true ou false. Pendant une
# session, l'hôte de streaming prend une sortie virtuelle comme sortie par défaut ; Platinum la suit :
# les canaux y envoient leur son, puis tout revient à la fin de la session.
follow_streaming = {}
# Sorties de streaming reconnues : morceaux de leur nom, séparés par des virgules.
streaming_sinks = {}
# Faire passer par Gaming les applications qui jouent sur la sortie de streaming : true ou false.
streaming_route_apps = {}

# Préréglage par jeu (le QuickSet de GG) : true ou false. Quand un jeu associé à un préréglage de
# Gaming tourne, Gaming prend ce préréglage ; le préréglage d'avant revient quand le jeu se ferme.
# Vos propres associations sont dans game-links.txt, à côté de ce fichier.
game_presets = {}
# Règle Arena 7 de GG : true ou false. Sur un canal qui joue sur les enceintes Arena 7, l'audio
# spatial passe en mode haut-parleur quand un préréglage est appliqué ou que la sortie y passe.
arena_speakers = {}

# Langue de la fenêtre, de l'icône près de l'horloge et des superpositions : auto (celle de
# Windows quand Platinum la parle, sinon l'anglais), fr, en, es, de, it ou pt.
language = {}

# Noms des canaux dans Windows : platinum (« Gaming (Platinum) », « Chat (Platinum) »…) ou
# steelseries (ceux du pilote, « SteelSeries Sonar - Gaming »…). Appliqué au démarrage de
# Platinum ; la fenêtre l'applique tout de suite.
channel_names = {}

# Rapports de plantage : off ou ask. Platinum note toujours ses plantages sur ce PC (dossier
# crashes, à côté de ce fichier) et n'envoie jamais rien de lui-même. ask : après un plantage, la
# fenêtre propose d'envoyer le rapport, anonymisé, par une issue GitHub que vous relisez puis
# envoyez vous-même depuis votre navigateur.
crash_reports = {}

# Raccourcis clavier, une ligne par action : « shortcut_<action> = Ctrl+Alt+Up ».
# Modificateurs : Ctrl, Alt, Shift, Win. Actions : master, game, chat, media, aux ou mic suivi de
# _volume_up, _volume_down ou _mute ; master, game, chat, media ou aux suivi de
# _stream_volume_up, _stream_volume_down ou _stream_mute (mix de stream) ; chatmix_game,
# chatmix_chat ; output_previous, output_next.
# Aucun raccourci par défaut ; la tâche de fond relit ce fichier dans les 2 secondes.
{}",
            text(&self.microphone),
            text(&self.fallback_output),
            text(&self.fallback_input),
            self.channels.join(", "),
            self.effects,
            self.gain,
            self.engine_recycle_mb,
            self.start_on_open,
            self.auto_switch,
            priority_lines("output", &self.output_priority),
            priority_lines("input", &self.input_priority),
            self.overlays.enabled,
            self.overlays.position.key(),
            self.overlays.volume,
            self.overlays.mute,
            self.overlays.device,
            self.mode.key(),
            single_line(&self.stream_device.to_text()),
            text(&self.personal_device),
            streamer::inclusion_text(&self.personal_mix),
            streamer::inclusion_text(&self.stream_mix),
            self.classic_outputs.as_ref().map(|outputs| single_line(&streamer::outputs_text(outputs))).unwrap_or_default(),
            self.follow_streaming,
            self.streaming_sinks.iter().map(|s| single_line(s)).collect::<Vec<_>>().join(", "),
            self.streaming_route_apps,
            self.game_presets,
            self.arena_speakers,
            self.language.key(),
            self.channel_names.key(),
            self.crash_reports.key(),
            self.shortcuts.iter().map(|(action, hotkey)| format!("shortcut_{} = {}\n", action.key(), hotkey.to_text())).collect::<String>(),
        )
    }

    /// Writes the configuration to `platinum.ini`, replacing the whole file at once.
    pub fn save(&self) -> Result<(), String> {
        self.save_to(&path()?)
    }

    pub fn save_to(&self, file: &Path) -> Result<(), String> {
        write_atomically(file, &self.to_text())
    }

    /// Puts the daemon's command-line arguments over this configuration.
    pub fn apply(&mut self, arguments: &Arguments) {
        if let Some(microphone) = &arguments.microphone {
            self.microphone = optional(microphone);
        }
        if let Some(effects) = arguments.effects {
            self.effects = effects;
        }
        if let Some(channels) = &arguments.channels {
            self.channels = channels.clone();
        }
        if let Some(gain) = arguments.gain {
            self.gain = gain;
        }
        if let Some(device) = &arguments.fallback_output {
            self.fallback_output = optional(device);
        }
        if let Some(device) = &arguments.fallback_input {
            self.fallback_input = optional(device);
        }
    }

    /// The configuration a logon command carried in its arguments, as Platinum wrote it before
    /// this file existed (`"…\platinum-start.exe" daemon Seiren --fallback-output AirPods`).
    /// `None` when the command does not start the daemon or its arguments cannot be read.
    pub fn from_logon_command(command: &str) -> Option<Config> {
        let (_, rest) = autostart::split_command(command)?;
        let arguments = autostart::split_arguments(&rest);
        let (first, daemon_args) = arguments.split_first()?;
        if first != "daemon" {
            return None;
        }
        let parsed = Arguments::parse(daemon_args).ok()?;
        let mut config = Config::default();
        config.apply(&parsed);
        Some(config)
    }
}

/// The daemon's command-line arguments. Each one overrides the configuration for one run:
/// `[microphone] [--effects | --no-effects] [--channels <list>] [--gain <0..1>]
/// [--fallback-output <device>] [--fallback-input <device>] [--no-tray]`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Arguments {
    pub microphone: Option<String>,
    pub effects: Option<bool>,
    pub channels: Option<Vec<String>>,
    pub gain: Option<f32>,
    pub fallback_output: Option<String>,
    pub fallback_input: Option<String>,
    /// `--no-tray`: no icon next to the clock. Not part of the configuration.
    pub no_tray: bool,
}

impl Arguments {
    /// Reads the arguments that follow `daemon` (errors in English, like the rest of the command
    /// line).
    pub fn parse<S: AsRef<str>>(args: &[S]) -> Result<Arguments, String> {
        let mut parsed = Arguments::default();
        let mut rest = args.iter().map(AsRef::as_ref);
        while let Some(arg) = rest.next() {
            let mut value = || rest.next().ok_or_else(|| format!("{arg} needs a value"));
            match arg {
                "--effects" => parsed.effects = Some(true),
                "--no-effects" => parsed.effects = Some(false),
                "--no-tray" => parsed.no_tray = true,
                "--channels" => {
                    let list = value()?;
                    let channels = parse_channels(list).map_err(|unknown| format!("unknown channel \"{unknown}\" (expected game, chat, media, aux, mic or stream)"))?;
                    if channels.is_empty() {
                        return Err("--channels needs at least one channel".into());
                    }
                    parsed.channels = Some(channels);
                }
                "--gain" => {
                    let text = value()?;
                    parsed.gain = Some(parse_gain(text).ok_or_else(|| format!("--gain {text}: expected a number from 0 to 1"))?);
                }
                "--fallback-output" => parsed.fallback_output = Some(value()?.to_string()),
                "--fallback-input" => parsed.fallback_input = Some(value()?.to_string()),
                option if option.starts_with("--") => return Err(format!("unknown option {option}")),
                microphone => {
                    if let Some(first) = &parsed.microphone {
                        return Err(format!("the daemon relays one microphone: \"{first}\" or \"{microphone}\"?"));
                    }
                    parsed.microphone = Some(microphone.to_string());
                }
            }
        }
        Ok(parsed)
    }

    /// Whether any argument changes the configuration (`--no-tray` does not).
    pub fn overrides_config(&self) -> bool {
        let Arguments { microphone, effects, channels, gain, fallback_output, fallback_input, no_tray: _ } = self;
        microphone.is_some() || effects.is_some() || channels.is_some() || gain.is_some() || fallback_output.is_some() || fallback_input.is_some()
    }
}

/// Writes `text` to `file` through a temporary file renamed over it, so that a reader never sees
/// half a file. Falls back to a plain write if the rename keeps failing (a reader holding the file
/// open without sharing it, for instance).
pub fn write_atomically(file: &Path, text: &str) -> Result<(), String> {
    if let Some(folder) = file.parent() {
        std::fs::create_dir_all(folder).map_err(|e| format!("cannot create {}: {e}", folder.display()))?;
    }
    let mut temporary = file.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    std::fs::write(&temporary, text).map_err(|e| format!("cannot write {}: {e}", temporary.display()))?;
    for attempt in 0..5 {
        if std::fs::rename(&temporary, file).is_ok() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(20 * (attempt + 1)));
    }
    let _ = std::fs::remove_file(&temporary);
    std::fs::write(file, text).map_err(|e| format!("cannot write {}: {e}", file.display()))
}

/// A device given in the file: empty means none.
fn optional(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// A value without the quotes that may surround it (`"Razer Seiren"`).
fn unquote(value: &str) -> &str {
    value.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(value)
}

/// A value written on one line, whatever it held.
fn single_line(value: &str) -> String {
    value.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().trim().to_string()
}

/// One entry of a priority list as written in the file: `identifier | name` (the name may be
/// missing). Identifiers never hold a `|`; names may.
fn parse_priority(value: &str) -> Option<PriorityDevice> {
    let (id, name) = value.split_once('|').unwrap_or((value, ""));
    let id = id.trim();
    (!id.is_empty()).then(|| PriorityDevice::new(id, name.trim()))
}

/// A priority list's lines (`output_device = …`), one per device, in order.
fn priority_lines(prefix: &str, list: &[PriorityDevice]) -> String {
    list.iter()
        .map(|entry| {
            let kind = if entry.excluded { "excluded" } else { "device" };
            let name = single_line(&entry.name);
            if name.is_empty() { format!("{prefix}_{kind} = {}\n", single_line(&entry.id)) } else { format!("{prefix}_{kind} = {} | {name}\n", single_line(&entry.id)) }
        })
        .collect()
}

/// A comma-separated list of channels; the error is the first unknown name.
fn parse_channels(list: &str) -> Result<Vec<String>, String> {
    let mut channels: Vec<String> = Vec::new();
    for channel in list.split(',').map(|c| c.trim().to_ascii_lowercase()).filter(|c| !c.is_empty()) {
        if !KNOWN_CHANNELS.contains(&channel.as_str()) {
            return Err(channel);
        }
        if !channels.contains(&channel) {
            channels.push(channel);
        }
    }
    Ok(channels)
}

/// A comma-separated list of names, each trimmed; empty entries are dropped (an empty value is an
/// empty list).
fn parse_list(value: &str) -> Vec<String> {
    value.split(',').map(str::trim).filter(|name| !name.is_empty()).map(str::to_string).collect()
}

/// true/false, also in the forms people write by hand (oui/non, yes/no, on/off, 1/0).
fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "oui" | "yes" | "on" | "1" | "vrai" => Some(true),
        "false" | "non" | "no" | "off" | "0" | "faux" => Some(false),
        _ => None,
    }
}

/// A gain from 0 to 1, with a decimal point or a French decimal comma.
fn parse_gain(value: &str) -> Option<f32> {
    let gain: f32 = value.trim().replace(',', ".").parse().ok()?;
    (0.0..=1.0).contains(&gain).then_some(gain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_survive_a_round_trip() {
        let (config, problems) = Config::parse(&Config::default().to_text());
        assert_eq!(config, Config::default());
        assert!(problems.is_empty(), "{problems:?}");
        assert!(config.start_on_open && config.effects && config.microphone.is_none());
        assert_eq!(config.channels, ["game", "chat", "media", "aux", "mic"]);
        // Windows' language until the user picks one; a bad value keeps it.
        assert!(Config::default().to_text().contains("\nlanguage = auto\n"));
        let (config, problems) = Config::parse("language = it\nlanguage = klingon\n");
        assert_eq!((config.language, problems.len()), (crate::i18n::Choice::Fixed(crate::i18n::Language::It), 1));
    }

    #[test]
    fn every_value_survives_a_round_trip() {
        let config = Config {
            microphone: Some("Microphone (Razer Seiren Mini)".into()),
            fallback_output: Some("AirPods".into()),
            fallback_input: Some("Seiren".into()),
            channels: vec!["game".into(), "mic".into()],
            effects: false,
            gain: 0.8,
            engine_recycle_mb: 0,
            start_on_open: false,
            auto_switch: true,
            output_priority: vec![
                PriorityDevice::new("{0.0.0.00000000}.{83e15a44}", "Haut-parleurs (SteelSeries Arena 7)"),
                PriorityDevice { id: "{0.0.0.00000000}.{a1}".into(), name: "PL2792H | NVIDIA".into(), excluded: true },
                PriorityDevice::new("{0.0.0.00000000}.{b2}", ""),
            ],
            input_priority: vec![PriorityDevice::new("{0.0.1.00000000}.{c3}", "Microphone (Razer Seiren Mini)")],
            shortcuts: vec![
                (Action::VolumeUp(crate::hotkeys::Target::Master), Hotkey::parse("Ctrl+Alt+Up").unwrap()),
                (Action::Mute(crate::hotkeys::Target::Mic), Hotkey::parse("Pause").unwrap()),
                (Action::NextOutput, Hotkey::parse("Win+Shift+F12").unwrap()),
            ],
            overlays: overlay::Settings { enabled: false, position: Position::BottomLeft, volume: false, mute: true, device: false },
            mode: Mode::Streamer,
            stream_device: StreamDevice::Device("{0.0.0.00000000}.{d4}".into()),
            personal_device: Some("{0.0.0.00000000}.{83e15a44}".into()),
            personal_mix: [true, true, false, true],
            stream_mix: [true, false, false, false],
            classic_outputs: Some([Some("{0.0.0.00000000}.{83e15a44}".into()), None, Some("{0.0.0.00000000}.{a1}".into()), None]),
            follow_streaming: false,
            streaming_sinks: vec!["Steam Streaming Speakers".into(), "Sunshine Virtual".into()],
            streaming_route_apps: false,
            game_presets: false,
            arena_speakers: false,
            language: crate::i18n::Choice::Fixed(crate::i18n::Language::De),
            channel_names: ChannelNames::SteelSeries,
            crash_reports: Consent::Ask,
        };
        let (read, problems) = Config::parse(&config.to_text());
        assert_eq!(read, config);
        assert!(problems.is_empty(), "{problems:?}");
        // Every channel out of both mixes, and four channels that had no output, survive too.
        let empty = Config { personal_mix: [false; STRIPS], stream_mix: [false; STRIPS], classic_outputs: Some([None, None, None, None]), stream_device: StreamDevice::None, ..Config::default() };
        let (read, problems) = Config::parse(&empty.to_text());
        assert_eq!(read, empty);
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn streamer_mode_is_off_until_the_user_switches() {
        let defaults = Config::default();
        assert_eq!(defaults.mode, Mode::Classic);
        assert_eq!((defaults.stream_device.clone(), defaults.personal_device.clone(), defaults.classic_outputs.clone()), (StreamDevice::Virtual, None, None));
        assert_eq!((defaults.personal_mix, defaults.stream_mix), (streamer::ALL_STRIPS, streamer::ALL_STRIPS));
        assert!(defaults.to_text().contains("\nmode = classic\n"));
        // A file written before streamer mode existed reads as classic.
        let (old, problems) = Config::parse("microphone = Seiren\nchannels = game, chat, media, aux, mic\n");
        assert_eq!(old.mode, Mode::Classic);
        assert!(problems.is_empty());
        let (config, problems) = Config::parse("mode = live\nstream_mix = game, mic\nclassic_outputs = {a}, {b}\nmode = Streamer\nstream_device = none\n");
        assert_eq!((config.mode, config.stream_device), (Mode::Streamer, StreamDevice::None));
        assert_eq!(config.stream_mix, streamer::ALL_STRIPS);
        assert_eq!(config.classic_outputs, None);
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems[0].contains("classic ou streamer") && problems[1].contains("mic") && problems[2].contains("quatre"), "{problems:?}");
    }

    #[test]
    fn remote_streaming_is_followed_by_default() {
        let defaults = Config::default();
        assert!(defaults.follow_streaming && defaults.streaming_route_apps);
        assert_eq!(defaults.streaming_sinks, ["Steam Streaming Speakers"]);
        let text = defaults.to_text();
        assert!(text.contains("\nfollow_streaming = true\n") && text.contains("\nstreaming_sinks = Steam Streaming Speakers\n"), "{text}");
        // A file written before the keys existed follows too: the user asked for it.
        let (old, problems) = Config::parse("microphone = Seiren\nmode = streamer\n");
        assert!(old.follow_streaming && old.streaming_route_apps && problems.is_empty());
        let (config, problems) = Config::parse("follow_streaming = non\nstreaming_sinks = Steam Streaming Speakers , ,Sunshine\nstreaming_route_apps = maybe\n");
        assert!(!config.follow_streaming && config.streaming_route_apps);
        assert_eq!(config.streaming_sinks, ["Steam Streaming Speakers", "Sunshine"]);
        assert_eq!(problems.len(), 1, "{problems:?}");
        // An empty list follows nothing, and stays empty.
        let (none, _) = Config::parse("streaming_sinks =\n");
        assert!(none.streaming_sinks.is_empty());
        assert_eq!(Config::parse(&none.to_text()).0, none);
    }

    #[test]
    fn game_presets_and_the_arena_rule_are_on_by_default() {
        let defaults = Config::default();
        assert!(defaults.game_presets && defaults.arena_speakers);
        let text = defaults.to_text();
        assert!(text.contains("\ngame_presets = true\n") && text.contains("\narena_speakers = true\n"), "{text}");
        // A file written before the keys existed has them on too: the user asked for it.
        let (old, problems) = Config::parse("microphone = Seiren\n");
        assert!(old.game_presets && old.arena_speakers && problems.is_empty());
        let (config, problems) = Config::parse("game_presets = non\narena_speakers = peut-être\n");
        assert!(!config.game_presets && config.arena_speakers);
        assert_eq!(problems.len(), 1, "{problems:?}");
    }

    #[test]
    fn platinum_names_by_default_and_crash_reports_off() {
        let defaults = Config::default();
        assert_eq!((defaults.channel_names, defaults.crash_reports), (ChannelNames::Platinum, Consent::Off));
        let text = defaults.to_text();
        assert!(text.contains("\nchannel_names = platinum\n") && text.contains("\ncrash_reports = off\n"), "{text}");
        let (config, problems) = Config::parse("channel_names = SteelSeries\ncrash_reports = ask\nchannel_names = gg\ncrash_reports = always\n");
        assert_eq!((config.channel_names, config.crash_reports), (ChannelNames::SteelSeries, Consent::Ask));
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("platinum ou steelseries") && problems[1].contains("off ou ask"), "{problems:?}");
    }

    #[test]
    fn an_old_file_keeps_the_names_the_user_went_back_to() {
        // A file from before the key: the names follow original-names.txt, as they did then.
        let old = "microphone = Seiren\nlanguage = auto\n";
        assert!(!sets_key(old, "channel_names"));
        assert_eq!(names_for_old_file(false, true), None);
        assert_eq!(names_for_old_file(false, false), Some(ChannelNames::SteelSeries));
        // The key, once written (by hand too, in any case), decides alone.
        assert!(sets_key("\u{feff}Channel_Names = platinum\n", "channel_names"));
        assert!(!sets_key("# channel_names = steelseries\nchannel_names_old = x\n", "channel_names"));
        assert_eq!(names_for_old_file(true, false), None);
        // What a new install writes has the key: the rule never applies to it.
        assert!(sets_key(&Config::default().to_text(), "channel_names"));
    }

    #[test]
    fn the_microphone_worker_is_replaced_past_400_mb_unless_turned_off() {
        let defaults = Config::default();
        assert_eq!(defaults.engine_recycle_mb, 400);
        assert!(defaults.to_text().contains("\nengine_recycle_mb = 400\n"));
        // A file written before the key existed gets the safety net too.
        let (old, problems) = Config::parse("microphone = Seiren\n");
        assert_eq!((old.engine_recycle_mb, problems.len()), (400, 0));
        let (config, problems) = Config::parse("engine_recycle_mb = 0\nengine_recycle_mb = beaucoup\nengine_recycle_mb = -5\n");
        assert_eq!(config.engine_recycle_mb, 0);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert_eq!(Config::parse(" engine_recycle_mb = 450 ").0.engine_recycle_mb, 450);
    }

    #[test]
    fn shortcuts_are_lines_of_their_own_and_none_is_bound_by_default() {
        let defaults = Config::default();
        assert!(defaults.shortcuts.is_empty());
        assert_eq!(defaults.overlays, overlay::Settings::default());
        assert!(!defaults.to_text().lines().any(|line| line.starts_with("shortcut_")));
        let text = "shortcut_chat_mute = ctrl + maj + m\nshortcut_game_volume_up = Ctrl+Alt+Up\nshortcut_game_volume_up = Ctrl+Alt+PageUp\nshortcut_aux_mute =\nshortcut_dance = Ctrl+D\nshortcut_media_mute = M\noverlay_position = centre\noverlays = non\n";
        let (config, problems) = Config::parse(text);
        // Kept in the window's order; a second line for an action replaces the first.
        let written: Vec<(String, String)> = config.shortcuts.iter().map(|(a, h)| (a.key(), h.to_text())).collect();
        assert_eq!(written, [("game_volume_up".to_string(), "Ctrl+Alt+PageUp".to_string()), ("chat_mute".to_string(), "Ctrl+Shift+M".to_string())]);
        assert_eq!(config.shortcut(Action::Mute(crate::hotkeys::Target::Aux)), None);
        assert!(!config.overlays.enabled);
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems[0].contains("shortcut_dance"), "{problems:?}");
        assert!(problems[1].contains("shortcut_media_mute") && problems[1].contains("Ctrl"), "{problems:?}");
        assert!(problems[2].contains("overlay_position"), "{problems:?}");
        // Unbinding removes the line.
        let mut config = config;
        config.set_shortcut(Action::VolumeUp(crate::hotkeys::Target::Game), None);
        assert_eq!(config.shortcuts.len(), 1);
        assert!(!config.to_text().contains("shortcut_game_volume_up"));
    }

    #[test]
    fn priority_lists_keep_their_order_and_refuse_bad_lines() {
        let text = "auto_switch = oui\noutput_device = {a} | A\noutput_excluded = {b}|B\ninput_device = {m} | Micro\noutput_device = {c}\noutput_device = {a} | twice\noutput_device = | nameless\nauto_switch = maybe\n";
        let (config, problems) = Config::parse(text);
        assert!(config.auto_switch);
        let outputs: Vec<(&str, &str, bool)> = config.output_priority.iter().map(|e| (e.id.as_str(), e.name.as_str(), e.excluded)).collect();
        assert_eq!(outputs, [("{a}", "A", false), ("{b}", "B", true), ("{c}", "", false)]);
        assert_eq!(config.input_priority, [PriorityDevice::new("{m}", "Micro")]);
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems[0].starts_with("ligne 6 :") && problems[0].contains("déjà"), "{problems:?}");
        assert!(problems[1].starts_with("ligne 7 :"), "{problems:?}");
        assert!(problems[2].contains("auto_switch = maybe"), "{problems:?}");
        // Without the keys: switch off, empty lists, as GG ships it.
        let (config, _) = Config::parse("");
        assert!(!config.auto_switch && config.output_priority.is_empty() && config.input_priority.is_empty());
    }

    #[test]
    fn hand_written_files_are_read_leniently() {
        let text = "\u{feff}# comment\n; also a comment\nmicrophone = \"Razer Seiren\"\nEFFECTS = non\ngain = 0,5\nstart_on_open = oui\nfallback_output =\ncolour = blue\nnot a setting\ngain = 7\nchannels = Game, chat, game\neffects = maybe\n";
        let (config, problems) = Config::parse(text);
        assert_eq!(config.microphone.as_deref(), Some("Razer Seiren"));
        // The refused values keep what was set before them.
        assert!(!config.effects);
        assert_eq!(config.gain, 0.5);
        assert!(config.start_on_open);
        assert_eq!(config.fallback_output, None);
        assert_eq!(config.channels, ["game", "chat"]);
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert!(problems[0].starts_with("ligne 8 :") && problems[0].contains("colour"), "{problems:?}");
        assert!(problems[1].starts_with("ligne 9 :"), "{problems:?}");
        assert!(problems[2].contains("gain = 7"), "{problems:?}");
        assert!(problems[3].contains("effects = maybe"), "{problems:?}");
        let (config, problems) = Config::parse("channels = , ,\nchannels = game, speakers\n");
        assert_eq!(config.channels, Config::default().channels);
        assert_eq!(problems.len(), 2, "{problems:?}");
    }

    #[test]
    fn arguments_override_only_what_they_name() {
        let parsed = Arguments::parse(&["Razer Seiren", "--no-effects", "--gain", "0.7", "--fallback-input", "Seiren", "--no-tray"]).unwrap();
        assert!(parsed.no_tray && parsed.overrides_config());
        let mut config = Config { fallback_output: Some("AirPods".into()), ..Config::default() };
        config.apply(&parsed);
        assert_eq!(config.microphone.as_deref(), Some("Razer Seiren"));
        assert!(!config.effects);
        assert_eq!(config.gain, 0.7);
        assert_eq!(config.fallback_output.as_deref(), Some("AirPods"));
        assert_eq!(config.fallback_input.as_deref(), Some("Seiren"));
        assert_eq!(config.channels, Config::default().channels);

        let none = Arguments::parse::<&str>(&[]).unwrap();
        assert!(!none.overrides_config() && !none.no_tray);
        assert!(!Arguments::parse(&["--no-tray"]).unwrap().overrides_config());
        assert_eq!(Arguments::parse(&["--channels", "game,mic"]).unwrap().channels, Some(vec!["game".to_string(), "mic".to_string()]));
    }

    #[test]
    fn bad_arguments_are_refused() {
        for args in [&["a", "b"][..], &["--gain", "2"], &["--gain"], &["--channels", "game,speakers"], &["--channels", ","], &["--loud"]] {
            assert!(Arguments::parse(args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn the_old_logon_command_gives_its_settings() {
        let command = r#""C:\Users\Someone\Platinum\platinum-start.exe" daemon Seiren --fallback-output AirPods --fallback-input Seiren"#;
        let config = Config::from_logon_command(command).unwrap();
        assert_eq!(config.microphone.as_deref(), Some("Seiren"));
        assert_eq!(config.fallback_output.as_deref(), Some("AirPods"));
        assert_eq!(config.fallback_input.as_deref(), Some("Seiren"));
        assert!(config.effects && config.start_on_open);

        let quoted = r#""C:\P\platinum-start.exe" daemon "Razer Seiren" --no-effects --channels game,chat,mic"#;
        let config = Config::from_logon_command(quoted).unwrap();
        assert_eq!(config.microphone.as_deref(), Some("Razer Seiren"));
        assert!(!config.effects);
        assert_eq!(config.channels, ["game", "chat", "mic"]);

        // The command written from now on carries no settings: the file holds them.
        assert_eq!(Config::from_logon_command(r#""C:\P\platinum-start.exe" daemon"#), Some(Config::default()));
        assert_eq!(Config::from_logon_command(r#""C:\P\other.exe" --background"#), None);
        assert_eq!(Config::from_logon_command(r#""C:\P\platinum-start.exe" daemon --gain loud"#), None);
    }

    #[test]
    fn an_unreadable_file_is_never_overwritten() {
        let loaded = Loaded { config: Config::default(), origin: Origin::Defaults, problems: Vec::new(), unreadable: true };
        assert!(loaded.for_saving().unwrap_err().contains("illisible"));
        let loaded = Loaded { config: Config::default(), origin: Origin::Defaults, problems: Vec::new(), unreadable: false };
        assert_eq!(loaded.for_saving().unwrap(), Config::default());
    }

    #[test]
    fn saving_replaces_the_file_whole() {
        let folder = std::env::temp_dir().join(format!("platinum-config-test-{}", std::process::id()));
        let file = folder.join(FILE_NAME);
        let first = Config { microphone: Some("Seiren".into()), ..Config::default() };
        first.save_to(&file).unwrap();
        let second = Config { microphone: Some("Arena 7".into()), gain: 0.25, ..Config::default() };
        second.save_to(&file).unwrap();
        let (read, problems) = load_from(&file).unwrap();
        assert_eq!(read, second);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(!folder.join("platinum.ini.tmp").exists());
        assert_eq!(load_from(&folder.join("missing.ini")).unwrap_err().kind(), std::io::ErrorKind::NotFound);
        let _ = std::fs::remove_dir_all(&folder);
    }
}
