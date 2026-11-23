//! GG's Sonar shortcuts ("Raccourcis"): global key combinations that change a channel's volume or
//! mute, ChatMix, or the output device, from any application.
//!
//! Everything here is pure: the combinations as platinum.ini writes them ("Ctrl+Alt+Up"), the
//! actions, and what each action writes given what the mixer holds (`plan`). The daemon registers
//! the combinations with Windows and carries the writes out (`shortcuts`); the window edits them.
//!
//! The actions are GG's (`EventAction`): per channel Volume +, Volume - and mute, ChatMix towards
//! the game or the chat, and the previous or next output device for every output channel at once
//! (GG's "Changement de tous les périphériques de sortie": Platinum's outputs share one priority
//! list). Each step is GG's: 5 % of volume (`EventActionHandlerService`: ±0.05) and 5 % of the
//! ChatMix balance. Master is GG's classic Master: it rescales the four output channels, keeping
//! their ratios (`mixer::Master`), as the window's Master does.
//!
//! Streamer mode adds GG's Stream variants (`soundstage.actions.gameStream`…): Volume +, Volume -
//! and mute of each output channel's stream mix and of the Stream master. The actions above are the
//! personal mix's in streamer mode (GG's "Personnel"), since the classic volume is the personal
//! one; the microphone has no stream mix in Platinum.

use crate::i18n::{mark, tr};
use crate::mixer::{self, Master, Mix, STRIPS};
use crate::trf;
use crate::overlay::{Content, Glyph, Kind};

/// Windows' modifier flags for `RegisterHotKey`.
pub const MOD_ALT: u32 = 0x0001;
pub const MOD_CONTROL: u32 = 0x0002;
pub const MOD_SHIFT: u32 = 0x0004;
pub const MOD_WIN: u32 = 0x0008;
/// Held down, the combination fires once (mutes and device changes must not flap).
pub const MOD_NOREPEAT: u32 = 0x4000;
const MODIFIERS: u32 = MOD_ALT | MOD_CONTROL | MOD_SHIFT | MOD_WIN;
/// GG's step for a volume and for the ChatMix balance.
pub const STEP: f32 = 0.05;

/// A key combination: Windows' modifier flags and virtual-key code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Hotkey {
    pub modifiers: u32,
    pub key: u32,
}

/// The keys a shortcut may use: virtual-key code, name in platinum.ini, name shown in the window.
const KEYS: &[(u32, &str, &str)] = &[
    (0x08, "Backspace", mark("Retour arrière")),
    (0x09, "Tab", "Tab"),
    (0x0D, "Enter", mark("Entrée")),
    (0x13, "Pause", "Pause"),
    (0x20, "Space", mark("Espace")),
    (0x21, "PageUp", mark("Page préc.")),
    (0x22, "PageDown", mark("Page suiv.")),
    (0x23, "End", mark("Fin")),
    (0x24, "Home", mark("Origine")),
    (0x25, "Left", "←"),
    (0x26, "Up", "↑"),
    (0x27, "Right", "→"),
    (0x28, "Down", "↓"),
    (0x2C, "PrintScreen", mark("Impr. écran")),
    (0x2D, "Insert", mark("Inser")),
    (0x2E, "Delete", mark("Suppr")),
    (0x60, "Numpad0", "Pavé num. 0"),
    (0x61, "Numpad1", "Pavé num. 1"),
    (0x62, "Numpad2", "Pavé num. 2"),
    (0x63, "Numpad3", "Pavé num. 3"),
    (0x64, "Numpad4", "Pavé num. 4"),
    (0x65, "Numpad5", "Pavé num. 5"),
    (0x66, "Numpad6", "Pavé num. 6"),
    (0x67, "Numpad7", "Pavé num. 7"),
    (0x68, "Numpad8", "Pavé num. 8"),
    (0x69, "Numpad9", "Pavé num. 9"),
    (0x6A, "NumpadMultiply", "Pavé num. *"),
    (0x6B, "NumpadAdd", "Pavé num. +"),
    (0x6D, "NumpadSubtract", "Pavé num. -"),
    (0x6E, "NumpadDecimal", "Pavé num. ."),
    (0x6F, "NumpadDivide", "Pavé num. /"),
    (0x91, "ScrollLock", mark("Arrêt défil.")),
    (0xAD, "VolumeMute", mark("Muet")),
    (0xAE, "VolumeDown", mark("Volume -")),
    (0xAF, "VolumeUp", mark("Volume +")),
    (0xB0, "MediaNext", mark("Piste suivante")),
    (0xB1, "MediaPrevious", mark("Piste précédente")),
    (0xB2, "MediaStop", mark("Arrêt")),
    (0xB3, "MediaPlayPause", mark("Lecture/Pause")),
    (0xBA, "Oem1", "OEM 1"),
    (0xBB, "OemPlus", "OEM +"),
    (0xBC, "OemComma", "OEM ,"),
    (0xBD, "OemMinus", "OEM -"),
    (0xBE, "OemPeriod", "OEM ."),
    (0xBF, "Oem2", "OEM 2"),
    (0xC0, "Oem3", "OEM 3"),
    (0xDB, "Oem4", "OEM 4"),
    (0xDC, "Oem5", "OEM 5"),
    (0xDD, "Oem6", "OEM 6"),
    (0xDE, "Oem7", "OEM 7"),
    (0xDF, "Oem8", "OEM 8"),
    (0xE2, "Oem102", "OEM 102"),
];

/// French names people may write by hand in platinum.ini, for keys and modifiers.
const ALIASES: &[(&str, &str)] = &[
    ("control", "ctrl"),
    ("ctl", "ctrl"),
    ("maj", "shift"),
    ("windows", "win"),
    ("meta", "win"),
    ("haut", "up"),
    ("bas", "down"),
    ("gauche", "left"),
    ("droite", "right"),
    ("espace", "space"),
    ("entrée", "enter"),
    ("entree", "enter"),
    ("return", "enter"),
    ("suppr", "delete"),
    ("del", "delete"),
    ("inser", "insert"),
    ("ins", "insert"),
    ("pgup", "pageup"),
    ("pgdn", "pagedown"),
    ("esc", "escape"),
    ("échap", "escape"),
];

/// The platinum.ini name of a key code, if a shortcut may use it.
pub fn key_name(key: u32) -> Option<String> {
    match key {
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(key).map(String::from),
        0x70..=0x87 => Some(format!("F{}", key - 0x6F)),
        _ => KEYS.iter().find(|(code, _, _)| *code == key).map(|(_, name, _)| name.to_string()),
    }
}

/// The key code of a platinum.ini key name (any case).
pub fn key_code(name: &str) -> Option<u32> {
    let lower = name.trim().to_lowercase();
    let lower = ALIASES.iter().find(|(alias, _)| *alias == lower).map_or(lower.clone(), |(_, name)| name.to_string());
    let mut chars = lower.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphanumeric() {
            return Some(c.to_ascii_uppercase() as u32);
        }
    }
    if let Some(number) = lower.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
        return (1..=24).contains(&number).then_some(0x6F + number);
    }
    KEYS.iter().find(|(_, name, _)| name.to_lowercase() == lower).map(|(code, _, _)| *code)
}

/// Whether a key may make a shortcut on its own. A letter, a digit, an arrow or the space bar alone
/// would be taken from every application (no more typing it): those need a modifier. GG allows a
/// single key; the function keys, the media keys and the like are kept for that.
pub fn usable_alone(key: u32) -> bool {
    matches!(key, 0x70..=0x87 | 0x13 | 0x91 | 0x2C | 0xAD..=0xB3 | 0x60..=0x6F)
}

impl Hotkey {
    pub fn new(modifiers: u32, key: u32) -> Hotkey {
        Hotkey { modifiers: modifiers & MODIFIERS, key }
    }

    /// Reads "Ctrl+Alt+Up" (any case, spaces allowed, French names accepted). The error, in
    /// French, says what is wrong.
    pub fn parse(text: &str) -> Result<Hotkey, String> {
        let parts: Vec<&str> = text.split('+').map(str::trim).collect();
        let Some((last, modifiers)) = parts.split_last() else { return Err("raccourci vide".into()) };
        let mut flags = 0;
        for part in modifiers {
            let lower = part.to_lowercase();
            let lower = ALIASES.iter().find(|(alias, _)| *alias == lower).map_or(lower.as_str(), |(_, name)| name);
            flags |= match lower {
                "ctrl" => MOD_CONTROL,
                "alt" => MOD_ALT,
                "shift" => MOD_SHIFT,
                "win" => MOD_WIN,
                "" => return Err(format!("« {text} » : touche manquante entre deux +")),
                _ => return Err(format!("« {part} » n'est pas un modificateur (Ctrl, Alt, Maj ou Win)")),
            };
        }
        let key = key_code(last).ok_or_else(|| format!("touche inconnue « {last} »"))?;
        let hotkey = Hotkey::new(flags, key);
        hotkey.check()?;
        Ok(hotkey)
    }

    /// Whether Windows can take this combination as a shortcut without breaking typing. The error
    /// is in the current language, for the window.
    pub fn check(&self) -> Result<(), String> {
        if key_name(self.key).is_none() {
            return Err(tr("cette touche ne peut pas servir de raccourci").into());
        }
        if self.modifiers & MODIFIERS == 0 && !usable_alone(self.key) {
            return Err(tr("ajoutez Ctrl, Alt ou Maj : cette touche seule ne pourrait plus être tapée nulle part").into());
        }
        Ok(())
    }

    /// As platinum.ini writes it: "Ctrl+Alt+Up".
    pub fn to_text(&self) -> String {
        let mut parts: Vec<String> = self.modifier_names(["Ctrl", "Alt", "Shift", "Win"]);
        parts.push(key_name(self.key).unwrap_or_else(|| format!("0x{:02X}", self.key)));
        parts.join("+")
    }

    /// As the window shows it: "Ctrl + Maj + ↑".
    pub fn display(&self) -> String {
        let mut parts: Vec<String> = self.modifier_names(["Ctrl", "Alt", tr("Maj"), "Win"]);
        let key = match self.key {
            0x30..=0x39 | 0x41..=0x5A | 0x70..=0x87 => key_name(self.key).unwrap_or_default(),
            // The numeric keypad's keys, one text for them all.
            0x60..=0x6F => KEYS.iter().find(|(code, _, _)| *code == self.key).map_or_else(String::new, |(_, _, shown)| trf!("Pavé num. {key}", key = shown.trim_start_matches("Pavé num. "))),
            other => KEYS.iter().find(|(code, _, _)| *code == other).map_or_else(|| format!("0x{other:02X}"), |(_, _, shown)| tr(shown).to_string()),
        };
        parts.push(key);
        parts.join(" + ")
    }

    fn modifier_names(&self, names: [&str; 4]) -> Vec<String> {
        [(MOD_CONTROL, names[0]), (MOD_ALT, names[1]), (MOD_SHIFT, names[2]), (MOD_WIN, names[3])].into_iter().filter(|(flag, _)| self.modifiers & flag != 0).map(|(_, name)| name.to_string()).collect()
    }
}

/// What a shortcut acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    Master,
    Game,
    Chat,
    Media,
    Aux,
    Mic,
}

impl Target {
    pub const ALL: [Target; 6] = [Target::Master, Target::Game, Target::Chat, Target::Media, Target::Aux, Target::Mic];
    /// What has a stream mix: the Master and the four output channels.
    pub const STREAM: [Target; 5] = [Target::Master, Target::Game, Target::Chat, Target::Media, Target::Aux];

    fn key(self) -> &'static str {
        match self {
            Target::Master => "master",
            Target::Game => "game",
            Target::Chat => "chat",
            Target::Media => "media",
            Target::Aux => "aux",
            Target::Mic => "mic",
        }
    }

    /// As the window titles it, in the current language.
    pub fn title(self) -> &'static str {
        match self {
            Target::Master => tr("Master"),
            Target::Game => tr("Gaming"),
            Target::Chat => tr("Chat"),
            Target::Media => tr("Media"),
            Target::Aux => tr("Aux"),
            Target::Mic => tr("Micro"),
        }
    }

    /// Its overlay's icon.
    pub fn glyph(self) -> Glyph {
        match self {
            Target::Master => Glyph::Master,
            Target::Game => Glyph::Game,
            Target::Chat => Glyph::Chat,
            Target::Media => Glyph::Media,
            Target::Aux => Glyph::Aux,
            Target::Mic => Glyph::Mic,
        }
    }

    /// The strip (0 Gaming to 3 Aux) of an output channel.
    pub fn strip(self) -> Option<usize> {
        match self {
            Target::Game => Some(0),
            Target::Chat => Some(1),
            Target::Media => Some(2),
            Target::Aux => Some(3),
            Target::Master | Target::Mic => None,
        }
    }
}

/// One of GG's Sonar actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    VolumeUp(Target),
    VolumeDown(Target),
    Mute(Target),
    /// The stream mix's volume and mute (streamer mode), for `Target::STREAM`.
    StreamVolumeUp(Target),
    StreamVolumeDown(Target),
    StreamMute(Target),
    /// ChatMix towards the game (the chat goes down).
    ChatMixGame,
    /// ChatMix towards the chat (the game goes down).
    ChatMixChat,
    PreviousOutput,
    NextOutput,
}

/// The window's groups of actions, in order, with their titles (GG's French where it has them).
pub const GROUPS: [&str; 8] = [mark("Master"), mark("Gaming"), mark("Chat"), mark("Media"), mark("Aux"), mark("Micro"), mark("ChatMix"), mark("Changement de périphérique")];

/// The heading of the stream mix's actions in a channel's group (GG's `mode.dropdown.stream`).
pub const STREAM_SECTION: &str = mark("Mix de stream");

impl Action {
    /// Every action, in the window's order: within a group, the personal mix's, then the stream
    /// mix's.
    pub const ALL: [Action; 37] = [
        Action::VolumeUp(Target::Master),
        Action::VolumeDown(Target::Master),
        Action::Mute(Target::Master),
        Action::VolumeUp(Target::Game),
        Action::VolumeDown(Target::Game),
        Action::Mute(Target::Game),
        Action::VolumeUp(Target::Chat),
        Action::VolumeDown(Target::Chat),
        Action::Mute(Target::Chat),
        Action::VolumeUp(Target::Media),
        Action::VolumeDown(Target::Media),
        Action::Mute(Target::Media),
        Action::VolumeUp(Target::Aux),
        Action::VolumeDown(Target::Aux),
        Action::Mute(Target::Aux),
        Action::VolumeUp(Target::Mic),
        Action::VolumeDown(Target::Mic),
        Action::Mute(Target::Mic),
        Action::ChatMixGame,
        Action::ChatMixChat,
        Action::PreviousOutput,
        Action::NextOutput,
        Action::StreamVolumeUp(Target::Master),
        Action::StreamVolumeDown(Target::Master),
        Action::StreamMute(Target::Master),
        Action::StreamVolumeUp(Target::Game),
        Action::StreamVolumeDown(Target::Game),
        Action::StreamMute(Target::Game),
        Action::StreamVolumeUp(Target::Chat),
        Action::StreamVolumeDown(Target::Chat),
        Action::StreamMute(Target::Chat),
        Action::StreamVolumeUp(Target::Media),
        Action::StreamVolumeDown(Target::Media),
        Action::StreamMute(Target::Media),
        Action::StreamVolumeUp(Target::Aux),
        Action::StreamVolumeDown(Target::Aux),
        Action::StreamMute(Target::Aux),
    ];

    /// Its name in platinum.ini, after `shortcut_`: "game_volume_up".
    pub fn key(self) -> String {
        match self {
            Action::VolumeUp(target) => format!("{}_volume_up", target.key()),
            Action::VolumeDown(target) => format!("{}_volume_down", target.key()),
            Action::Mute(target) => format!("{}_mute", target.key()),
            Action::StreamVolumeUp(target) => format!("{}_stream_volume_up", target.key()),
            Action::StreamVolumeDown(target) => format!("{}_stream_volume_down", target.key()),
            Action::StreamMute(target) => format!("{}_stream_mute", target.key()),
            Action::ChatMixGame => "chatmix_game".into(),
            Action::ChatMixChat => "chatmix_chat".into(),
            Action::PreviousOutput => "output_previous".into(),
            Action::NextOutput => "output_next".into(),
        }
    }

    pub fn parse_key(text: &str) -> Option<Action> {
        let text = text.trim().to_ascii_lowercase();
        Action::ALL.into_iter().find(|action| action.key() == text)
    }

    /// Its place in `ALL`.
    pub fn index(self) -> usize {
        Action::ALL.iter().position(|a| *a == self).unwrap_or(0)
    }

    /// GG's French label (`soundstage.settings.shortcuts.*`, `soundstage.actions.*`).
    pub fn label(self) -> &'static str {
        match self {
            Action::VolumeUp(_) => tr("Volume +"),
            Action::VolumeDown(_) => tr("Volume -"),
            Action::Mute(_) => tr("Couper son"),
            Action::StreamVolumeUp(_) => tr("Stream : Volume +"),
            Action::StreamVolumeDown(_) => tr("Stream : Volume -"),
            Action::StreamMute(_) => tr("Stream : Couper son"),
            Action::ChatMixGame => tr("ChatMix Jeu"),
            Action::ChatMixChat => tr("ChatMix Chat"),
            Action::PreviousOutput => tr("Périphérique précédent"),
            Action::NextOutput => tr("Périphérique suivant"),
        }
    }

    /// The label under its group's title in the window, where the group (and the stream mix's
    /// heading) says the rest ("Changement de périphérique" › "Suivant").
    pub fn short_label(self) -> &'static str {
        match self {
            Action::PreviousOutput => tr("Précédent"),
            Action::NextOutput => tr("Suivant"),
            Action::StreamVolumeUp(_) => tr("Volume +"),
            Action::StreamVolumeDown(_) => tr("Volume -"),
            Action::StreamMute(_) => tr("Couper son"),
            other => other.label(),
        }
    }

    /// The heading shown above this action in its group: the stream mix's, above its first action.
    pub fn section(self) -> Option<&'static str> {
        matches!(self, Action::StreamVolumeUp(_)).then_some(STREAM_SECTION)
    }

    /// Its group in `GROUPS`.
    pub fn group(self) -> usize {
        match self {
            Action::VolumeUp(target) | Action::VolumeDown(target) | Action::Mute(target) | Action::StreamVolumeUp(target) | Action::StreamVolumeDown(target) | Action::StreamMute(target) => {
                Target::ALL.iter().position(|t| *t == target).unwrap_or(0)
            }
            Action::ChatMixGame | Action::ChatMixChat => 6,
            Action::PreviousOutput | Action::NextOutput => 7,
        }
    }

    /// What a volume or mute action acts on; `None` for ChatMix and the device changes.
    pub fn target(self) -> Option<Target> {
        match self {
            Action::VolumeUp(target) | Action::VolumeDown(target) | Action::Mute(target) | Action::StreamVolumeUp(target) | Action::StreamVolumeDown(target) | Action::StreamMute(target) => Some(target),
            _ => None,
        }
    }

    /// The overlay a stream mix's action shows outside streamer mode, where it changes nothing
    /// heard; `None` for the other actions.
    pub fn outside_streamer_mode(self) -> Option<Content> {
        let target = self.target().filter(|_| self.mix() == Some(Mix::Stream))?;
        let kind = if matches!(self, Action::StreamMute(_)) { Kind::Mute } else { Kind::Volume };
        Some(Content::unavailable(kind, target.glyph(), &mix_title(Mix::Stream, target), tr("Mode streamer inactif")))
    }

    /// The mix a volume or mute action changes; `None` for ChatMix and the device changes.
    pub fn mix(self) -> Option<Mix> {
        match self {
            Action::VolumeUp(_) | Action::VolumeDown(_) | Action::Mute(_) => Some(Mix::Personal),
            Action::StreamVolumeUp(_) | Action::StreamVolumeDown(_) | Action::StreamMute(_) => Some(Mix::Stream),
            _ => None,
        }
    }

    /// A volume or ChatMix step repeats while its keys are held; a mute or a device change fires
    /// once.
    pub fn repeats(self) -> bool {
        matches!(self, Action::VolumeUp(_) | Action::VolumeDown(_) | Action::StreamVolumeUp(_) | Action::StreamVolumeDown(_) | Action::ChatMixGame | Action::ChatMixChat)
    }
}

/// The actions whose combination another action uses too (each of them is flagged in the window;
/// the daemon registers the first one only).
pub fn conflicts(bindings: &[(Action, Hotkey)]) -> Vec<Action> {
    bindings.iter().filter(|(action, hotkey)| bindings.iter().any(|(other, key)| other != action && key == hotkey)).map(|(action, _)| *action).collect()
}

/// What the mixer holds when a shortcut is pressed, as far as the action needs it.
#[derive(Clone, Debug, PartialEq)]
pub struct Mixer {
    /// Gaming, Chat, Media, Aux: volume 0..1 and mute.
    pub volumes: [f32; STRIPS],
    pub mutes: [bool; STRIPS],
    /// ChatMix gains (Game, Chat) and whether ChatMix can be used (Gaming and Chat on one device).
    pub chatmix: (f32, f32),
    pub chatmix_usable: bool,
    /// The Master as last saved (the Stream master in it).
    pub master: Master,
    /// The virtual microphone's Windows volume and mute; `None` while it is not there.
    pub mic: Option<(f32, bool)>,
    /// Gaming, Chat, Media, Aux in the stream mix: volume 0..1 and mute.
    pub stream_volumes: [f32; STRIPS],
    pub stream_mutes: [bool; STRIPS],
}

impl Default for Mixer {
    fn default() -> Mixer {
        Mixer {
            volumes: [1.0; STRIPS],
            mutes: [false; STRIPS],
            chatmix: (1.0, 1.0),
            chatmix_usable: true,
            master: Master::default(),
            mic: None,
            stream_volumes: [1.0; STRIPS],
            stream_mutes: [false; STRIPS],
        }
    }
}

impl Mixer {
    /// One mix's volumes and mutes.
    fn mix(&self, mix: Mix) -> (&[f32; STRIPS], &[bool; STRIPS]) {
        match mix {
            Mix::Personal => (&self.volumes, &self.mutes),
            Mix::Stream => (&self.stream_volumes, &self.stream_mutes),
        }
    }
}

/// One write a shortcut makes.
#[derive(Clone, Debug, PartialEq)]
pub enum Write {
    /// A strip's volume, 0..1 of its setting's range (212, Chat 273).
    Volume(usize, f32),
    /// A strip's mute (213, Chat 274).
    Mute(usize, bool),
    /// A strip's volume and mute in the stream mix (214 and 215, Chat 275 and 276).
    StreamVolume(usize, f32),
    StreamMute(usize, bool),
    /// ChatMix gains: Game (183 on Game's store), Chat (184 on Chat's).
    ChatMix(f32, f32),
    /// The virtual microphone's Windows volume and mute.
    MicLevel(f32),
    MicMute(bool),
}

/// What a shortcut does: the writes in order, the Master to save when it changed, the overlay, and
/// why nothing was done (English, for the daemon's log).
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub writes: Vec<Write>,
    pub master: Option<Master>,
    pub overlay: Option<Content>,
    pub problem: Option<String>,
}

impl Plan {
    fn nothing(problem: &str, overlay: Option<Content>) -> Plan {
        Plan { writes: Vec::new(), master: None, overlay, problem: Some(problem.into()) }
    }
}

/// A volume moved by one step, kept in 0..1 and rounded off float noise (0.30000001).
fn stepped(volume: f32, up: bool) -> f32 {
    let next = volume + if up { STEP } else { -STEP };
    ((next.clamp(0.0, 1.0)) * 10_000.0).round() / 10_000.0
}

/// The overlay's title of an action on `target` in `mix`: "Gaming", or "Gaming · Stream".
fn mix_title(mix: Mix, target: Target) -> String {
    match mix {
        Mix::Personal => target.title().to_string(),
        Mix::Stream => format!("{} · Stream", target.title()),
    }
}

/// A strip's volume or mute write in one mix.
fn volume_write(mix: Mix, index: usize, volume: f32) -> Write {
    match mix {
        Mix::Personal => Write::Volume(index, volume),
        Mix::Stream => Write::StreamVolume(index, volume),
    }
}

fn mute_write(mix: Mix, index: usize, on: bool) -> Write {
    match mix {
        Mix::Personal => Write::Mute(index, on),
        Mix::Stream => Write::StreamMute(index, on),
    }
}

/// A volume step on `target` in `mix`: a channel, or the mix's master, which rescales them all.
fn volume_step(mix: Mix, target: Target, up: bool, mixer: &Mixer, mut master: Master) -> Plan {
    let (volumes, mutes) = mixer.mix(mix);
    let title = mix_title(mix, target);
    match target {
        Target::Master => {
            let wanted = master.with_group(mix, |group| group.set_volume(stepped(group.volume, up)));
            let group = master.group(mix);
            let writes = (0..STRIPS).filter(|&i| (volumes[i] - wanted[i]).abs() > 1e-4).map(|i| volume_write(mix, i, wanted[i])).collect();
            let overlay = Content::volume(Glyph::Master, &title, group.volume, group.muted);
            Plan { writes, master: Some(master), overlay: Some(overlay), problem: None }
        }
        Target::Mic => match (mix, mixer.mic) {
            (Mix::Personal, Some((level, muted))) => {
                let level = stepped(level, up);
                Plan { writes: vec![Write::MicLevel(level)], master: None, overlay: Some(Content::volume(Glyph::Mic, &title, level, muted)), problem: None }
            }
            (Mix::Personal, None) => Plan::nothing("the virtual microphone is not active", Some(Content::unavailable(Kind::Volume, Glyph::Mic, &title, tr("Micro virtuel inactif")))),
            (Mix::Stream, _) => Plan::nothing("the microphone has no stream mix in Platinum", None),
        },
        _ => {
            let index = target.strip().unwrap_or(0);
            let volume = stepped(volumes[index], up);
            master.with_group(mix, |group| group.channel_moved(index, volume, volumes));
            let overlay = Content::volume(target.glyph(), &title, volume, mutes[index]);
            Plan { writes: vec![volume_write(mix, index, volume)], master: Some(master), overlay: Some(overlay), problem: None }
        }
    }
}

/// A mute on `target` in `mix`: a channel, or the mix's master, which mutes them all and gives
/// them their own mutes back.
fn mute_toggle(mix: Mix, target: Target, mixer: &Mixer, mut master: Master) -> Plan {
    let (volumes, mutes) = mixer.mix(mix);
    let title = mix_title(mix, target);
    match target {
        Target::Master => {
            let on = !master.group(mix).muted;
            let wanted = master.with_group(mix, |group| if on { group.mute(mutes) } else { group.unmute() });
            let writes = (0..STRIPS).filter(|&i| mutes[i] != wanted[i]).map(|i| mute_write(mix, i, wanted[i])).collect();
            Plan { writes, master: Some(master), overlay: Some(Content::mute(Glyph::Master, &title, on, master.group(mix).volume)), problem: None }
        }
        Target::Mic => match (mix, mixer.mic) {
            (Mix::Personal, Some((level, muted))) => Plan { writes: vec![Write::MicMute(!muted)], master: None, overlay: Some(Content::mute(Glyph::Mic, &title, !muted, level)), problem: None },
            (Mix::Personal, None) => Plan::nothing("the virtual microphone is not active", Some(Content::unavailable(Kind::Mute, Glyph::Mic, &title, tr("Micro virtuel inactif")))),
            (Mix::Stream, _) => Plan::nothing("the microphone has no stream mix in Platinum", None),
        },
        _ => {
            let index = target.strip().unwrap_or(0);
            let on = !mutes[index];
            // A channel heard again ends its mix's master mute, as in the window.
            if !on {
                master.with_group(mix, |group| group.muted = false);
            }
            let overlay = Content::mute(target.glyph(), &title, on, volumes[index]);
            Plan { writes: vec![mute_write(mix, index, on)], master: Some(master), overlay: Some(overlay), problem: None }
        }
    }
}

/// The writes, Master and overlay of a volume, mute or ChatMix action (a device change is planned
/// by `next_device`). The action's mix master always takes in the channels as they are first, as
/// the window does, so that a change made elsewhere is not undone; the other mix's master is left
/// as saved (its volumes were not read).
pub fn plan(action: Action, mixer: &Mixer) -> Plan {
    let mut master = mixer.master;
    if let Some(mix) = action.mix() {
        let (volumes, mutes) = mixer.mix(mix);
        master.with_group(mix, |group| {
            group.take(volumes);
            group.take_mutes(mutes);
        });
    }
    match action {
        Action::VolumeUp(target) => volume_step(Mix::Personal, target, true, mixer, master),
        Action::VolumeDown(target) => volume_step(Mix::Personal, target, false, mixer, master),
        Action::StreamVolumeUp(target) => volume_step(Mix::Stream, target, true, mixer, master),
        Action::StreamVolumeDown(target) => volume_step(Mix::Stream, target, false, mixer, master),
        Action::Mute(target) => mute_toggle(Mix::Personal, target, mixer, master),
        Action::StreamMute(target) => mute_toggle(Mix::Stream, target, mixer, master),
        Action::ChatMixGame | Action::ChatMixChat => {
            if !mixer.chatmix_usable {
                return Plan::nothing("ChatMix needs Gaming and Chat on the same output", Some(Content::unavailable(Kind::Volume, Glyph::ChatMix, tr("ChatMix"), tr("Gaming et Chat doivent jouer sur la même sortie"))));
            }
            let balance = mixer::chatmix_balance(mixer.chatmix.0, mixer.chatmix.1);
            let delta = if action == Action::ChatMixGame { -STEP } else { STEP };
            let balance = ((balance + delta).clamp(-1.0, 1.0) * 10_000.0).round() / 10_000.0;
            let (game, chat) = mixer::chatmix_gains(balance);
            Plan { writes: vec![Write::ChatMix(game, chat)], master: None, overlay: Some(Content::chatmix(balance, (game, chat))), problem: None }
        }
        Action::PreviousOutput | Action::NextOutput => Plan::nothing("a device change is planned by next_device", None),
    }
}

/// The device a "previous / next device" shortcut moves to: the one after (or before) `current`
/// among `candidates`, round the list; the first (or the last) when `current` is not among them.
/// `None` without any candidate.
pub fn next_device(candidates: &[&str], current: Option<&str>, forward: bool) -> Option<usize> {
    let count = candidates.len();
    if count == 0 {
        return None;
    }
    Some(match current.and_then(|id| candidates.iter().position(|c| *c == id)) {
        Some(index) if forward => (index + 1) % count,
        Some(index) => (index + count - 1) % count,
        None if forward => 0,
        None => count - 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combinations_read_and_write_back() {
        let hotkey = Hotkey::parse("Ctrl+Alt+Up").unwrap();
        assert_eq!(hotkey, Hotkey { modifiers: MOD_CONTROL | MOD_ALT, key: 0x26 });
        assert_eq!(hotkey.to_text(), "Ctrl+Alt+Up");
        assert_eq!(hotkey.display(), "Ctrl + Alt + ↑");
        // Any case, spaces, French names; the order of the modifiers is the file's own.
        assert_eq!(Hotkey::parse(" maj + ctrl + haut ").unwrap().to_text(), "Ctrl+Shift+Up");
        assert_eq!(Hotkey::parse("Ctrl+Shift+m").unwrap().display(), "Ctrl + Maj + M");
        assert_eq!(Hotkey::parse("F13").unwrap(), Hotkey { modifiers: 0, key: 0x7C });
        assert_eq!(Hotkey::parse("Win+Numpad8").unwrap().to_text(), "Win+Numpad8");
        assert_eq!(Hotkey::parse("Alt+Pavé num. 8").map(|h| h.key).ok(), None);
        assert_eq!(Hotkey::parse("Ctrl+Alt+F24").unwrap().key, 0x87);
        assert_eq!(Hotkey::parse("ctrl+alt+7").unwrap().to_text(), "Ctrl+Alt+7");
        assert_eq!(Hotkey::parse("Alt+Suppr").unwrap().to_text(), "Alt+Delete");
        assert_eq!(Hotkey::parse("VolumeUp").unwrap().display(), "Volume +");
        for key in KEYS.iter().map(|(code, _, _)| *code).chain(0x30..=0x39).chain(0x41..=0x5A).chain(0x70..=0x87) {
            let hotkey = Hotkey::new(MOD_CONTROL | MOD_SHIFT, key);
            assert_eq!(Hotkey::parse(&hotkey.to_text()), Ok(hotkey), "{}", hotkey.to_text());
        }
    }

    #[test]
    fn bad_combinations_are_refused_in_french() {
        assert!(Hotkey::parse("").unwrap_err().contains("inconnue"));
        assert!(Hotkey::parse("Ctrl+").unwrap_err().contains("inconnue"));
        assert!(Hotkey::parse("Ctrl++Up").unwrap_err().contains("manquante"));
        assert!(Hotkey::parse("Hyper+Up").unwrap_err().contains("modificateur"));
        assert!(Hotkey::parse("Ctrl+Banana").unwrap_err().contains("inconnue"));
        assert!(Hotkey::parse("Ctrl+F25").is_err());
        // A letter, a digit or an arrow alone would no longer be typed anywhere.
        assert!(Hotkey::parse("M").unwrap_err().contains("Ctrl"));
        assert!(Hotkey::parse("Up").is_err());
        assert!(Hotkey::parse("Space").is_err());
        assert!(Hotkey::parse("Pause").is_ok() && Hotkey::parse("Numpad5").is_ok());
        assert!(Hotkey::new(0, 0x7B).check().is_ok());
        assert!(Hotkey::new(MOD_ALT, 0x5B).check().is_err());
    }

    #[test]
    fn every_action_has_a_unique_name_label_and_group() {
        let mut names: Vec<String> = Action::ALL.iter().map(|a| a.key()).collect();
        for (index, action) in Action::ALL.iter().enumerate() {
            assert_eq!(Action::parse_key(&action.key()), Some(*action));
            assert_eq!(action.index(), index);
            assert!(action.group() < GROUPS.len());
            assert!(!action.label().is_empty());
        }
        names.sort();
        names.dedup();
        assert_eq!(names.len(), Action::ALL.len());
        assert_eq!(Action::VolumeUp(Target::Game).key(), "game_volume_up");
        assert_eq!(Action::Mute(Target::Mic).key(), "mic_mute");
        assert_eq!(Action::NextOutput.key(), "output_next");
        assert!(Action::VolumeDown(Target::Aux).repeats() && Action::ChatMixChat.repeats());
        assert!(!Action::Mute(Target::Chat).repeats() && !Action::NextOutput.repeats());
        assert_eq!(Action::Mute(Target::Mic).group(), 5);
        assert_eq!(Action::PreviousOutput.group(), 7);
        // The stream mix's actions: in their channel's group, after its own, under their heading.
        assert_eq!(Action::StreamVolumeUp(Target::Game).key(), "game_stream_volume_up");
        assert_eq!(Action::StreamMute(Target::Master).key(), "master_stream_mute");
        assert_eq!(Action::StreamMute(Target::Media).group(), 3);
        assert!(Action::StreamVolumeDown(Target::Chat).repeats() && !Action::StreamMute(Target::Chat).repeats());
        assert_eq!(Action::StreamVolumeUp(Target::Aux).section(), Some(STREAM_SECTION));
        assert_eq!(Action::StreamMute(Target::Aux).section(), None);
        assert_eq!(Action::StreamMute(Target::Aux).short_label(), "Couper son");
        for target in Target::STREAM {
            let group: Vec<Action> = Action::ALL.into_iter().filter(|a| a.group() == Action::Mute(target).group()).collect();
            assert_eq!(group, [Action::VolumeUp(target), Action::VolumeDown(target), Action::Mute(target), Action::StreamVolumeUp(target), Action::StreamVolumeDown(target), Action::StreamMute(target)]);
        }
        assert!(!Action::ALL.iter().any(|a| matches!(a, Action::StreamMute(Target::Mic) | Action::StreamVolumeUp(Target::Mic))));
    }

    #[test]
    fn a_combination_used_twice_is_flagged_on_both() {
        let up = Hotkey::parse("Ctrl+Alt+Up").unwrap();
        let down = Hotkey::parse("Ctrl+Alt+Down").unwrap();
        let bindings = [(Action::VolumeUp(Target::Game), up), (Action::VolumeDown(Target::Game), down), (Action::VolumeUp(Target::Chat), up)];
        assert_eq!(conflicts(&bindings), [Action::VolumeUp(Target::Game), Action::VolumeUp(Target::Chat)]);
        assert!(conflicts(&bindings[..2]).is_empty());
    }

    fn mixer() -> Mixer {
        Mixer { volumes: [0.5, 1.0, 0.25, 0.0], mutes: [false, false, true, false], ..Mixer::default() }
    }

    #[test]
    fn a_channel_step_writes_its_volume_and_keeps_the_master_in_step() {
        let plan = plan(Action::VolumeUp(Target::Game), &mixer());
        assert_eq!(plan.writes, [Write::Volume(0, 0.55)]);
        let master = plan.master.unwrap();
        assert_eq!(master.volume, 1.0);
        assert!((master.ratios[0] - 0.55).abs() < 1e-4);
        let overlay = plan.overlay.unwrap();
        assert_eq!((overlay.title.as_str(), overlay.detail.as_str(), overlay.kind), ("Gaming", "55\u{a0}%", Kind::Volume));
        // Clamped at both ends; Chat's volume is its own setting (273), written by strip number.
        assert_eq!(super::plan(Action::VolumeUp(Target::Chat), &mixer()).writes, [Write::Volume(1, 1.0)]);
        assert_eq!(super::plan(Action::VolumeDown(Target::Aux), &mixer()).writes, [Write::Volume(3, 0.0)]);
        // A muted channel's overlay is dim.
        assert!(super::plan(Action::VolumeDown(Target::Media), &mixer()).overlay.unwrap().dimmed);
    }

    #[test]
    fn the_master_rescales_every_channel() {
        let plan = plan(Action::VolumeDown(Target::Master), &mixer());
        // Master 1 → 0.95: each channel keeps its share; Aux at 0 stays and is not written.
        assert_eq!(plan.writes, [Write::Volume(0, 0.475), Write::Volume(1, 0.95), Write::Volume(2, 0.2375)]);
        assert_eq!(plan.master.unwrap().volume, 0.95);
        assert_eq!(plan.overlay.unwrap().detail, "95\u{a0}%");
        // Down to 0 and back up: the ratios bring the channels back.
        let mut state = mixer();
        for _ in 0..25 {
            let step = super::plan(Action::VolumeDown(Target::Master), &state);
            for write in &step.writes {
                if let Write::Volume(i, v) = write {
                    state.volumes[*i] = *v;
                }
            }
            state.master = step.master.unwrap();
        }
        assert_eq!(state.volumes, [0.0; STRIPS]);
        for _ in 0..20 {
            let step = super::plan(Action::VolumeUp(Target::Master), &state);
            for write in &step.writes {
                if let Write::Volume(i, v) = write {
                    state.volumes[*i] = *v;
                }
            }
            state.master = step.master.unwrap();
        }
        assert!(state.volumes.iter().zip([0.5, 1.0, 0.25, 0.0]).all(|(a, b)| (a - b).abs() < 1e-3), "{:?}", state.volumes);
    }

    #[test]
    fn mutes_toggle_and_the_master_gives_the_channels_theirs_back() {
        let plan = plan(Action::Mute(Target::Game), &mixer());
        assert_eq!(plan.writes, [Write::Mute(0, true)]);
        assert_eq!(plan.overlay.as_ref().map(|o| (o.detail.as_str(), o.kind)), Some(("Son coupé", Kind::Mute)));
        assert_eq!(super::plan(Action::Mute(Target::Media), &mixer()).writes, [Write::Mute(2, false)]);
        // Master mute: the three channels heard are muted; Media was muted already.
        let muted = super::plan(Action::Mute(Target::Master), &mixer());
        assert_eq!(muted.writes, [Write::Mute(0, true), Write::Mute(1, true), Write::Mute(3, true)]);
        let master = muted.master.unwrap();
        assert!(master.muted);
        // Unmuting gives Media its own mute back.
        let state = Mixer { mutes: [true; STRIPS], master, ..mixer() };
        let unmuted = super::plan(Action::Mute(Target::Master), &state);
        assert_eq!(unmuted.writes, [Write::Mute(0, false), Write::Mute(1, false), Write::Mute(3, false)]);
        assert_eq!(unmuted.overlay.unwrap().detail, "Son activé");
        // A channel unmuted by hand ends the Master's mute.
        let alone = super::plan(Action::Mute(Target::Chat), &state);
        assert_eq!(alone.writes, [Write::Mute(1, false)]);
        assert!(!alone.master.unwrap().muted);
    }

    #[test]
    fn the_microphone_is_its_windows_volume() {
        let state = Mixer { mic: Some((0.8, false)), ..mixer() };
        assert_eq!(plan(Action::VolumeUp(Target::Mic), &state).writes, [Write::MicLevel(0.85)]);
        assert_eq!(plan(Action::VolumeDown(Target::Mic), &state).master, None);
        let muted = plan(Action::Mute(Target::Mic), &state);
        assert_eq!(muted.writes, [Write::MicMute(true)]);
        assert_eq!(muted.overlay.unwrap().title, "Micro");
        // Without the virtual microphone, nothing is written and the overlay says why.
        let absent = plan(Action::VolumeUp(Target::Mic), &mixer());
        assert!(absent.writes.is_empty() && absent.problem.is_some());
        assert_eq!(absent.overlay.unwrap().line, "Micro virtuel inactif");
    }

    #[test]
    fn stream_actions_change_the_stream_mix_only() {
        let state = Mixer { stream_volumes: [0.5, 1.0, 0.25, 0.0], stream_mutes: [false, true, false, false], ..mixer() };
        let up = plan(Action::StreamVolumeUp(Target::Game), &state);
        assert_eq!(up.writes, [Write::StreamVolume(0, 0.55)]);
        let master = up.master.unwrap();
        assert_eq!((master.volume, master.ratios), (1.0, [1.0; STRIPS]));
        assert!((master.stream.ratios[0] - 0.55).abs() < 1e-4);
        assert_eq!(up.overlay.unwrap().title, "Gaming · Stream");
        // The Stream master rescales the stream volumes, keeping their shares.
        let down = plan(Action::StreamVolumeDown(Target::Master), &state);
        assert_eq!(down.writes, [Write::StreamVolume(0, 0.475), Write::StreamVolume(1, 0.95), Write::StreamVolume(2, 0.2375)]);
        assert_eq!(down.master.unwrap().stream.volume, 0.95);
        assert_eq!(down.overlay.unwrap().title, "Master · Stream");
        // Its mute mutes the channels heard in the stream, and gives Chat its own mute back.
        let muted = plan(Action::StreamMute(Target::Master), &state);
        assert_eq!(muted.writes, [Write::StreamMute(0, true), Write::StreamMute(2, true), Write::StreamMute(3, true)]);
        let master = muted.master.unwrap();
        assert!(master.stream.muted && !master.muted);
        let back = plan(Action::StreamMute(Target::Master), &Mixer { stream_mutes: [true; STRIPS], master, ..state.clone() });
        assert_eq!(back.writes, [Write::StreamMute(0, false), Write::StreamMute(2, false), Write::StreamMute(3, false)]);
        assert_eq!(plan(Action::StreamMute(Target::Chat), &state).writes, [Write::StreamMute(1, false)]);
        // The microphone has no stream mix.
        assert!(plan(Action::StreamMute(Target::Mic), &state).writes.is_empty());
        // A personal action leaves the Stream master as saved: the stream volumes were not read.
        let saved = Master { stream: mixer::Group { volume: 0.3, ..mixer::Group::default() }, ..Master::default() };
        let personal = plan(Action::VolumeUp(Target::Game), &Mixer { master: saved, ..mixer() });
        assert_eq!(personal.master.unwrap().stream, saved.stream);
        assert_eq!(Action::StreamVolumeUp(Target::Aux).mix(), Some(Mix::Stream));
        assert_eq!(Action::ChatMixGame.mix(), None);
        // Outside streamer mode, a stream mix's action only says so.
        let off = Action::StreamMute(Target::Chat).outside_streamer_mode().unwrap();
        assert_eq!((off.title.as_str(), off.line.as_str(), off.kind), ("Chat · Stream", "Mode streamer inactif", Kind::Mute));
        assert_eq!(Action::Mute(Target::Chat).outside_streamer_mode(), None);
        assert_eq!(Action::StreamVolumeUp(Target::Master).target(), Some(Target::Master));
    }

    #[test]
    fn chatmix_steps_along_ggs_curve() {
        let plan = plan(Action::ChatMixChat, &mixer());
        let Write::ChatMix(game, chat) = plan.writes[0] else { panic!("{:?}", plan.writes) };
        assert_eq!(chat, 1.0);
        assert!((game - mixer::chatmix_gains(0.05).0).abs() < 1e-6, "{game}");
        let towards_game = super::plan(Action::ChatMixGame, &mixer());
        assert_eq!(towards_game.writes, [Write::ChatMix(1.0, mixer::chatmix_gains(-0.05).1)]);
        // From a balance read back off the gains, one more step.
        let (g, c) = mixer::chatmix_gains(-0.5);
        let state = Mixer { chatmix: (g, c), ..mixer() };
        let Write::ChatMix(g2, c2) = super::plan(Action::ChatMixGame, &state).writes[0] else { panic!() };
        assert!((mixer::chatmix_balance(g2, c2) + 0.55).abs() < 1e-4);
        // At the end, it stays.
        let state = Mixer { chatmix: mixer::chatmix_gains(1.0), ..mixer() };
        assert_eq!(super::plan(Action::ChatMixChat, &state).writes, [Write::ChatMix(0.0, 1.0)]);
        // Gaming and Chat on two devices: nothing, as GG.
        let split = Mixer { chatmix_usable: false, ..mixer() };
        assert!(super::plan(Action::ChatMixGame, &split).writes.is_empty());
    }

    #[test]
    fn device_changes_go_round_the_list() {
        let list = ["a", "b", "c"];
        assert_eq!(next_device(&list, Some("a"), true), Some(1));
        assert_eq!(next_device(&list, Some("c"), true), Some(0));
        assert_eq!(next_device(&list, Some("a"), false), Some(2));
        assert_eq!(next_device(&list, Some("elsewhere"), true), Some(0));
        assert_eq!(next_device(&list, None, false), Some(2));
        assert_eq!(next_device(&["a"], Some("a"), true), Some(0));
        assert_eq!(next_device(&[], Some("a"), true), None);
        assert!(plan(Action::NextOutput, &mixer()).writes.is_empty());
    }
}
