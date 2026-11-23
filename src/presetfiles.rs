//! Platinum's own preset files, in %USERPROFILE%\Platinum:
//! - `state.json`: the preset each channel uses and its current settings (Platinum is the only
//!   writer of the settings, so it keeps its own copy, as GG keeps its database), each preset's
//!   edits (GG saves an edit into the preset itself; Platinum keeps the edited copy here, never in
//!   GG's database), the favourites and the GG presets whose "Nouveau" tag the user has already
//!   seen;
//! - `presets\<n>-<channel>\*.json`: the user's own presets, one file each, in the shape of GG's
//!   own preset objects (`name`, `virtualAudioDevice`, `schemaVersion`, `data`), which is also the
//!   format of Export and Import.
//!
//! The window writes both; the daemon only reads them, when it puts a game's preset on Gaming and
//! the preset of before back afterwards (`games`): it needs the channel's chosen preset and the
//! user's edits of each preset, exactly as the window keeps them. That is why this lives in the
//! library rather than in the window.
//!
//! Every write goes through a temporary file renamed over the old one. Nothing here touches the
//! audio module, and GG's database is never written.

use crate::i18n::{mark, tr};
use crate::json::Json;
use crate::trf;
use crate::library::Library;
use crate::preset::{self, Preset, Role};
use std::path::{Path, PathBuf};

pub const STATE_FILE: &str = "state.json";
pub const PRESETS_FOLDER: &str = "presets";
/// GG allows nine favourites per channel ("Favoris (n/9)").
pub const FAVORITES: usize = 9;

/// The preset kind of each channel, in the window's channel order (Gaming, Chat, Media, Aux,
/// Micro): the order of state.json's lists.
pub const ROLES: [Role; 5] = [Role::Game, Role::ChatRender, Role::Media, Role::Aux, Role::ChatCapture];

/// The window's name for the channel a preset kind belongs to.
pub fn role_title(role: Role) -> &'static str {
    match role {
        Role::Game => "Gaming",
        Role::ChatRender => "Chat",
        Role::ChatCapture => mark("Micro"),
        Role::Media => "Media",
        Role::Aux => "Aux",
    }
}

/// Which preset a channel uses.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PresetRef {
    /// Settings that come from no preset: what the module held on the first start, or a preset
    /// that has since been deleted. Shown as "Personnalisé".
    #[default]
    Custom,
    /// Platinum's own neutral preset.
    Flat,
    /// A preset of GG's database, by GG's identifier.
    Gg(String),
    /// One of the user's own Platinum presets, by name.
    Own(String),
}

impl PresetRef {
    /// How the window and state.json name it: `custom`, `flat`, `gg:<id>`, `own:<name>`.
    pub fn key(&self) -> String {
        match self {
            PresetRef::Custom => "custom".into(),
            PresetRef::Flat => "flat".into(),
            PresetRef::Gg(id) => format!("gg:{id}"),
            PresetRef::Own(name) => format!("own:{name}"),
        }
    }

    /// The reverse of [`PresetRef::key`]; anything else is `Custom`.
    pub fn parse(key: &str) -> PresetRef {
        if key == "flat" {
            PresetRef::Flat
        } else if let Some(id) = key.strip_prefix("gg:").filter(|id| !id.is_empty()) {
            PresetRef::Gg(id.to_string())
        } else if let Some(name) = key.strip_prefix("own:").filter(|name| !name.is_empty()) {
            PresetRef::Own(name.to_string())
        } else {
            PresetRef::Custom
        }
    }
}

/// One channel in state.json.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChannelState {
    pub selection: PresetRef,
    /// The chosen preset's name, kept for when it cannot be found any more.
    pub name: String,
    /// The channel's current settings; `None` until the module was read once.
    pub data: Option<Preset>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    /// In the window's channel order (`ROLES`).
    pub channels: [ChannelState; 5],
    pub favorites: [Vec<PresetRef>; 5],
    /// GG's own favourites were copied in once; after that the list is Platinum's.
    pub favorites_seeded: bool,
    /// GG presets the user chose since GG marked them "Nouveau": the tag no longer shows.
    pub seen_new: Vec<String>,
    /// Each channel's presets as the user edited them, by preset: choosing one again brings its
    /// edits back, as GG does (B20), until "Réinitialiser".
    pub edited: [Edited; 5],
    /// The game preset the user ended by choosing a preset in the window while a game had put its
    /// own on Gaming (`games::Saved::id`); empty when none. The daemon then stops managing that
    /// game: it neither puts its preset back nor the one of before. The window writes it in its own
    /// file, so that neither program ever writes the other's.
    pub game_ended: String,
}

/// The edited copies of one channel's presets.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Edited(pub Vec<(PresetRef, Preset)>);

impl Edited {
    pub fn get(&self, key: &PresetRef) -> Option<&Preset> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, data)| data)
    }

    pub fn set(&mut self, key: &PresetRef, data: Preset) {
        match self.0.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = data,
            None => self.0.push((key.clone(), data)),
        }
    }

    /// Removes a copy; returns whether there was one.
    pub fn remove(&mut self, key: &PresetRef) -> bool {
        let before = self.0.len();
        self.0.retain(|(k, _)| k != key);
        self.0.len() != before
    }

    /// A preset renamed: its copy follows.
    pub fn rename(&mut self, old: &PresetRef, new: &PresetRef) {
        for (key, _) in self.0.iter_mut().filter(|(k, _)| k == old) {
            *key = new.clone();
        }
    }
}

impl State {
    /// The file's text: JSON, channels by GG's names.
    pub fn to_text(&self) -> String {
        let channels = ROLES.iter().zip(&self.channels).map(|(role, channel)| {
            let mut members = vec![("selected".to_string(), Json::String(channel.selection.key())), ("name".to_string(), Json::String(channel.name.clone()))];
            if let Some(data) = &channel.data {
                members.push(("data".to_string(), data.to_json()));
            }
            (role.api_name().to_string(), Json::Object(members))
        });
        let favorites = ROLES.iter().zip(&self.favorites).map(|(role, list)| (role.api_name().to_string(), Json::Array(list.iter().map(|r| Json::String(r.key())).collect())));
        let edited = ROLES.iter().zip(&self.edited).map(|(role, copies)| (role.api_name().to_string(), Json::Object(copies.0.iter().map(|(key, data)| (key.key(), data.to_json())).collect())));
        let mut members = vec![
            ("version".to_string(), Json::Number(1.0)),
            ("channels".to_string(), Json::Object(channels.collect())),
            ("favorites".to_string(), Json::Object(favorites.collect())),
            ("favoritesSeeded".to_string(), Json::Bool(self.favorites_seeded)),
            ("seenNew".to_string(), Json::Array(self.seen_new.iter().map(|id| Json::String(id.clone())).collect())),
            ("edited".to_string(), Json::Object(edited.collect())),
        ];
        if !self.game_ended.is_empty() {
            members.push(("gamePresetEnded".to_string(), Json::String(self.game_ended.clone())));
        }
        Json::Object(members).to_pretty() + "\n"
    }

    /// Reads the file's text. What is missing or unreadable keeps its default, so that a damaged
    /// file loses only the part that is damaged.
    pub fn parse(text: &str) -> State {
        let mut state = State::default();
        let Ok(root) = Json::parse(text) else { return state };
        for (index, role) in ROLES.iter().enumerate() {
            if let Some(channel) = root.get("channels").and_then(|c| c.get(role.api_name())) {
                let slot = &mut state.channels[index];
                slot.selection = PresetRef::parse(channel.get("selected").and_then(Json::as_str).unwrap_or(""));
                slot.name = channel.get("name").and_then(Json::as_str).unwrap_or("").to_string();
                slot.data = channel.get("data").and_then(|data| Preset::from_json(data, *role).ok());
            }
            if let Some(Json::Object(copies)) = root.get("edited").and_then(|e| e.get(role.api_name())) {
                for (key, data) in copies {
                    let key = PresetRef::parse(key);
                    if let (false, Ok(data)) = (key == PresetRef::Custom, Preset::from_json(data, *role)) {
                        state.edited[index].set(&key, data);
                    }
                }
            }
            if let Some(list) = root.get("favorites").and_then(|f| f.get(role.api_name())).and_then(Json::as_array) {
                for key in list.iter().filter_map(Json::as_str).map(PresetRef::parse) {
                    if key != PresetRef::Custom && !state.favorites[index].contains(&key) && state.favorites[index].len() < FAVORITES {
                        state.favorites[index].push(key);
                    }
                }
            }
        }
        state.favorites_seeded = root.get("favoritesSeeded").and_then(Json::as_bool).unwrap_or(false);
        state.seen_new = root.get("seenNew").and_then(Json::as_array).unwrap_or(&[]).iter().filter_map(Json::as_str).map(str::to_string).collect();
        state.game_ended = root.get("gamePresetEnded").and_then(Json::as_str).unwrap_or("").to_string();
        state
    }
}

/// `%USERPROFILE%\Platinum\state.json`.
pub fn state_path() -> Result<PathBuf, String> {
    Ok(crate::autostart::install_dir()?.join(STATE_FILE))
}

/// The state as last saved; the defaults without a file (first start).
pub fn load_state() -> State {
    state_path().ok().and_then(|file| std::fs::read(file).ok()).map(|bytes| State::parse(&String::from_utf8_lossy(&bytes))).unwrap_or_default()
}

pub fn save_state(state: &State) -> Result<(), String> {
    crate::config::write_atomically(&state_path()?, &state.to_text())
}

/// The settings choosing `key` on the channel at `index` (in `ROLES` order) applies, as the
/// window applies them, with the preset's name: the user's edits of it kept in `state` when there
/// are some, else the preset itself (GG's library, the user's own presets, or the neutral one).
/// `None` for settings that come from no preset, and for a preset that no longer exists (deleted,
/// or GG's library unreadable).
pub fn resolve(key: &PresetRef, index: usize, state: &State, library: Option<&Library>, own: &[OwnPreset]) -> Option<(String, Preset)> {
    let role = *ROLES.get(index)?;
    let (name, data) = match key {
        PresetRef::Custom => return None,
        PresetRef::Flat => (preset::NEUTRAL_NAME.to_string(), preset::flat(role)),
        PresetRef::Gg(id) => library?.by_id(id).filter(|config| config.role == role).map(|config| (config.name.clone(), config.data))?,
        PresetRef::Own(name) => own.iter().find(|p| p.name == *name).map(|p| (p.name.clone(), p.data))?,
    };
    let data = state.edited.get(index).and_then(|copies| copies.get(key)).copied().unwrap_or(data);
    data.fits(role).then_some((name, data))
}

/// One of the user's own presets.
#[derive(Clone, Debug, PartialEq)]
pub struct OwnPreset {
    pub name: String,
    pub file: PathBuf,
    pub data: Preset,
}

/// `%USERPROFILE%\Platinum\presets`.
pub fn presets_root() -> Result<PathBuf, String> {
    Ok(crate::autostart::install_dir()?.join(PRESETS_FOLDER))
}

/// A channel's folder: GG's number and name for it ("1-game", "5-aux"). Not the bare name:
/// Windows reserves "aux" as a device name and refuses a folder called so.
pub fn folder_name(role: Role) -> String {
    format!("{}-{}", role.vad(), role.api_name())
}

/// GG's schema version for the preset objects of a channel (`schema_version` in its database).
fn schema_version(role: Role) -> f64 {
    if role == Role::ChatCapture { 6.0 } else { 5.0 }
}

/// A preset as a file: GG's preset object, without the fields only GG's database means anything
/// for (identifier, dates, image).
pub fn export_text(name: &str, role: Role, data: &Preset) -> String {
    let mut data = *data;
    // Presets always carry the channel's effects on; the switch is Platinum's, not the preset's.
    data.set_effects(true);
    let object = Json::Object(vec![
        ("name".to_string(), Json::String(name.to_string())),
        ("virtualAudioDevice".to_string(), Json::String(role.api_name().to_string())),
        ("schemaVersion".to_string(), Json::Number(schema_version(role))),
        ("data".to_string(), data.to_json()),
    ]);
    object.to_pretty() + "\n"
}

/// Reads a preset file: GG's preset object (`data` as an object, or as JSON text as in GG's
/// database), or a bare `data` block. Returns the name it carries, if any. A preset made for a
/// channel of the other kind (a microphone preset for Gaming) is refused; one of the same kind
/// (Media for Aux) is accepted, as the settings are the same. A bare block names no channel: its
/// kind is told from its keys, so that a Gaming block imported on Micro is refused too, instead of
/// leaving every capture effect at its neutral value.
pub fn import_text(text: &str, role: Role) -> Result<(Option<String>, Preset), String> {
    let json = Json::parse(text).map_err(|e| trf!("ce fichier n'est pas du JSON lisible ({error})", error = e))?;
    let inner;
    let (name, made_for, data) = match json.get("data") {
        Some(data @ Json::Object(_)) => (json.get("name"), made_for(&json), data),
        Some(Json::String(text)) => {
            inner = Json::parse(text).map_err(|e| trf!("le bloc « data » n'est pas du JSON lisible ({error})", error = e))?;
            (json.get("name"), made_for(&json), &inner)
        }
        _ if json.get("parametricEQ").is_some() => (None, None, &json),
        _ => return Err(tr("ce fichier ne contient pas de préréglage Sonar (ni « data » ni « parametricEQ »)").into()),
    };
    if let Some(other) = made_for.filter(|other| other.is_capture() != role.is_capture()) {
        return Err(trf!("ce préréglage est fait pour le canal {other}, pas pour le canal {channel}", other = tr(role_title(other)), channel = tr(role_title(role))));
    }
    if made_for.is_none() && kind_of_keys(data).is_some_and(|capture| capture != role.is_capture()) {
        let error = if role.is_capture() {
            trf!("ce préréglage est fait pour un canal de lecture (Gaming, Media, Aux), pas pour le canal {channel}", channel = tr(role_title(role)))
        } else {
            trf!("ce préréglage est fait pour un canal de capture (Chat, Micro), pas pour le canal {channel}", channel = tr(role_title(role)))
        };
        return Err(error);
    }
    let preset = Preset::from_json(data, role).map_err(|e| trf!("réglages non valides ({error})", error = e))?;
    let name = name.and_then(Json::as_str).map(str::trim).filter(|n| !n.is_empty()).map(str::to_string);
    Ok((name, preset))
}

/// Keys only a playback preset has, and keys only a capture preset has.
const PLAYBACK_KEYS: [&str; 9] = ["bassBoostState", "trebleBoostState", "voiceClarityState", "smartVolume", "generalGain", "virtualSurroundState", "virtualSurroundChannels", "reverbGainDB", "formFactor"];
const CAPTURE_KEYS: [&str; 7] = ["noiseReductionState", "volumeStabilizerState", "noiseGateState", "automaticNoiseGateState", "impactNoiseReductionState", "noiseCancelingState", "acousticEchoCancelingState"];

/// The kind of preset a bare `data` block is, from its keys: `Some(true)` for capture,
/// `Some(false)` for playback, `None` when they do not tell (only the equalizer, or both kinds).
fn kind_of_keys(data: &Json) -> Option<bool> {
    let has = |keys: &[&str]| keys.iter().any(|key| data.get(key).is_some());
    match (has(&PLAYBACK_KEYS), has(&CAPTURE_KEYS)) {
        (true, false) => Some(false),
        (false, true) => Some(true),
        _ => None,
    }
}

/// The channel a preset object says it is for: GG's API name, or its database number.
fn made_for(json: &Json) -> Option<Role> {
    if let Some(api) = json.get("virtualAudioDevice").and_then(Json::as_str) {
        return Role::ALL.into_iter().find(|r| r.api_name().eq_ignore_ascii_case(api));
    }
    json.get("vad").and_then(Json::as_f64).and_then(|vad| Role::from_vad(vad as i64))
}

/// The user's own presets of a channel, by name, with the files that could not be read.
pub fn load_own(root: &Path, role: Role) -> (Vec<OwnPreset>, Vec<String>) {
    let mut presets: Vec<OwnPreset> = Vec::new();
    let mut problems = Vec::new();
    let Ok(listing) = std::fs::read_dir(root.join(folder_name(role))) else { return (presets, problems) };
    let mut files: Vec<PathBuf> = listing.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("json"))).collect();
    files.sort();
    for file in files {
        let read = std::fs::read(&file).map_err(|e| e.to_string()).and_then(|bytes| import_text(&String::from_utf8_lossy(&bytes), role));
        match read {
            Ok((name, data)) => {
                let name = name.unwrap_or_else(|| file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
                // Two files with the same name inside: the first one wins.
                if !presets.iter().any(|p| p.name.eq_ignore_ascii_case(&name)) {
                    presets.push(OwnPreset { name, file, data });
                }
            }
            Err(error) => problems.push(format!("{} : {error}", file.display())),
        }
    }
    presets.sort_by_key(|p| p.name.to_uppercase());
    (presets, problems)
}

/// Writes one of the user's presets. `replacing` is its current file when it is saved again or
/// renamed: the new file is written first, the old one removed after.
pub fn write_own(root: &Path, role: Role, name: &str, data: &Preset, replacing: Option<&Path>) -> Result<PathBuf, String> {
    let folder = root.join(folder_name(role));
    let wanted = file_name(name);
    let mut file = folder.join(format!("{wanted}.json"));
    let mut n = 2;
    while file.exists() && Some(file.as_path()) != replacing {
        file = folder.join(format!("{wanted} ({n}).json"));
        n += 1;
    }
    crate::config::write_atomically(&file, &export_text(name, role, data))?;
    if let Some(old) = replacing.filter(|old| *old != file) {
        std::fs::remove_file(old).map_err(|e| format!("cannot remove {}: {e}", old.display()))?;
    }
    Ok(file)
}

pub fn delete_own(file: &Path) -> Result<(), String> {
    std::fs::remove_file(file).map_err(|e| format!("cannot remove {}: {e}", file.display()))
}

/// A preset's name as a file name: characters Windows refuses become "_", and the names Windows
/// reserves for devices get a "_" in front.
pub fn file_name(name: &str) -> String {
    let mut text: String = name.chars().map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c }).take(80).collect();
    while text.ends_with(['.', ' ']) {
        text.pop();
    }
    let text = text.trim_start().to_string();
    let stem = text.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") || ((stem.starts_with("COM") || stem.starts_with("LPT")) && stem.len() == 4 && stem.as_bytes()[3].is_ascii_digit());
    match () {
        _ if text.is_empty() => "préréglage".into(),
        _ if reserved => format!("_{text}"),
        _ => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preset::flat;

    fn scratch(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("platinum-persist-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        folder
    }

    #[test]
    fn references_round_trip_through_their_keys() {
        for r in [PresetRef::Custom, PresetRef::Flat, PresetRef::Gg("e6979db3-3e00".into()), PresetRef::Own("Mon réglage: soir".into())] {
            assert_eq!(PresetRef::parse(&r.key()), r);
        }
        assert_eq!(PresetRef::parse("gg:"), PresetRef::Custom);
        assert_eq!(PresetRef::parse("wobble"), PresetRef::Custom);
    }

    #[test]
    fn the_state_survives_a_round_trip() {
        let mut state = State::default();
        state.channels[0] = ChannelState { selection: PresetRef::Gg("abc".into()), name: "Deltarune".into(), data: Some(flat(Role::Game)) };
        state.channels[4] = ChannelState { selection: PresetRef::Own("Soir".into()), name: "Soir".into(), data: Some(flat(Role::ChatCapture)) };
        state.favorites[4] = vec![PresetRef::Flat, PresetRef::Gg("x".into())];
        state.favorites_seeded = true;
        state.seen_new = vec!["n1".into()];
        let mut edited = flat(Role::Game);
        if let Preset::Render(r) = &mut edited {
            r.general_gain = 4.5;
        }
        state.edited[0].set(&PresetRef::Gg("abc".into()), edited);
        state.edited[0].set(&PresetRef::Flat, flat(Role::Game));
        let text = state.to_text();
        assert!(text.contains("\"chatCapture\"") && text.contains("\"selected\": \"gg:abc\""), "{text}");
        assert!(!text.contains("gamePresetEnded"), "written only once a game preset was ended");
        assert_eq!(State::parse(&text), state);
        state.game_ended = "20261117-213000 steam:1671210".into();
        assert_eq!(State::parse(&state.to_text()), state);
    }

    #[test]
    fn a_damaged_state_loses_only_what_is_damaged() {
        let text = r#"{"channels":{"game":{"selected":"flat","data":{"generalGain":"loud"}},"aux":{"selected":"own:A"}},
                      "favorites":{"media":["flat","flat","custom","gg:1"]},"seenNew":[3,"z"],"gamePresetEnded":7}"#;
        let state = State::parse(text);
        assert_eq!(state.channels[0].selection, PresetRef::Flat);
        assert_eq!(state.channels[0].data, None);
        assert_eq!(state.channels[3].selection, PresetRef::Own("A".into()));
        assert_eq!(state.favorites[2], [PresetRef::Flat, PresetRef::Gg("1".into())]);
        assert_eq!(state.seen_new, ["z"]);
        assert!(!state.favorites_seeded && state.game_ended.is_empty());
        assert_eq!(State::parse("not json"), State::default());
    }

    #[test]
    fn a_preset_resolves_with_the_users_edits_as_the_window_applies_it() {
        let mut state = State::default();
        let mine = OwnPreset { name: "Soir".into(), file: "Soir.json".into(), data: flat(Role::Game) };
        let mut edited = flat(Role::Game);
        if let Preset::Render(r) = &mut edited {
            r.general_gain = 6.0;
        }
        // Without edits, the preset itself; with them, the edited copy.
        assert_eq!(resolve(&PresetRef::Own("Soir".into()), 0, &state, None, std::slice::from_ref(&mine)), Some(("Soir".to_string(), flat(Role::Game))));
        state.edited[0].set(&PresetRef::Own("Soir".into()), edited);
        assert_eq!(resolve(&PresetRef::Own("Soir".into()), 0, &state, None, std::slice::from_ref(&mine)), Some(("Soir".to_string(), edited)));
        assert_eq!(resolve(&PresetRef::Flat, 4, &state, None, &[]).map(|(_, data)| data), Some(flat(Role::ChatCapture)));
        // Settings of no preset, a preset gone, GG's library missing: nothing to apply.
        assert_eq!(resolve(&PresetRef::Custom, 0, &state, None, &[]), None);
        assert_eq!(resolve(&PresetRef::Own("Gone".into()), 0, &state, None, &[mine]), None);
        assert_eq!(resolve(&PresetRef::Gg("abc".into()), 0, &state, None, &[]), None);
        assert_eq!(resolve(&PresetRef::Flat, 9, &state, None, &[]), None);
    }

    #[test]
    fn exported_presets_import_back_and_refuse_the_other_kind() {
        let preset = flat(Role::Media);
        let text = export_text("Soirée", Role::Media, &preset);
        assert!(text.contains("\"virtualAudioDevice\": \"media\"") && text.contains("\"schemaVersion\": 5"), "{text}");
        assert_eq!(import_text(&text, Role::Aux).unwrap(), (Some("Soirée".into()), preset));
        let error = import_text(&text, Role::ChatCapture).unwrap_err();
        assert!(error.contains("Media") && error.contains("Micro"), "{error}");
        // A bare data block, and GG's database row with data as text.
        let bare = preset.to_json().to_compact();
        assert_eq!(import_text(&bare, Role::Game).unwrap(), (None, preset));
        // A bare block of the other kind is told by its keys and refused, both ways.
        assert!(import_text(&bare, Role::ChatCapture).unwrap_err().contains("lecture"));
        let capture = flat(Role::ChatCapture).to_json().to_compact();
        assert!(import_text(&capture, Role::Aux).unwrap_err().contains("capture"));
        assert!(import_text(&capture, Role::ChatRender).is_ok());
        // Only an equalizer says nothing about its kind: accepted.
        assert!(import_text(r#"{"parametricEQ":{"enabled":true}}"#, Role::ChatCapture).is_ok());
        let row = format!("{{\"name\":\"Row\",\"vad\":4,\"data\":{}}}", Json::String(bare).to_compact());
        assert_eq!(import_text(&row, Role::Game).unwrap().0.as_deref(), Some("Row"));
        assert!(import_text("{\"foo\":1}", Role::Game).is_err());
        assert!(import_text("[", Role::Game).is_err());
    }

    #[test]
    fn own_presets_are_written_listed_renamed_and_deleted() {
        let root = scratch("own");
        let a = write_own(&root, Role::Aux, "Nuit: calme", &flat(Role::Aux), None).unwrap();
        assert!(a.ends_with("5-aux/Nuit_ calme.json") || a.ends_with("5-aux\\Nuit_ calme.json"), "{}", a.display());
        // Same file name, another preset: a second file.
        let b = write_own(&root, Role::Aux, "Nuit/ calme", &flat(Role::Aux), None).unwrap();
        assert_ne!(a, b);
        let (list, problems) = load_own(&root, Role::Aux);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(list.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["Nuit/ calme", "Nuit: calme"]);
        let renamed = write_own(&root, Role::Aux, "Jour", &flat(Role::Aux), Some(&a)).unwrap();
        assert!(!a.exists() && renamed.exists());
        delete_own(&b).unwrap();
        std::fs::write(root.join("5-aux").join("broken.json"), "{").unwrap();
        let (list, problems) = load_own(&root, Role::Aux);
        assert_eq!(list.len(), 1);
        assert_eq!(problems.len(), 1);
        assert!(load_own(&root, Role::Game).0.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn file_names_avoid_what_windows_refuses() {
        assert_eq!(file_name("a<b>c:d"), "a_b_c_d");
        assert_eq!(file_name("aux"), "_aux");
        assert_eq!(file_name("COM3.txt"), "_COM3.txt");
        assert_eq!(file_name("Comète"), "Comète");
        assert_eq!(file_name("fin. "), "fin");
        assert_eq!(file_name("   "), "préréglage");
        assert_eq!(folder_name(Role::Aux), "5-aux");
    }
}
