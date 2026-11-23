//! What the window offers for each channel, and the engine's bookkeeping of it.
//!
//! A channel's presets come from three places, in the order of GG's browser: Platinum's neutral
//! preset, always there; the user's own Platinum presets (files, see `persist.rs`); and, when GG
//! is installed, GG's library read from its database, read-only, at run time (Platinum ships none
//! of it). GG's own user-made presets ("Custom") come with the library and sit with "Mes
//! préréglages", as GG files them under "My presets".
//!
//! The engine owns a [`Presets`]: state.json, the library, the user's presets, and the
//! [`Catalog`] it publishes to the window (shared, rebuilt only when something changes).
//!
//! Gaming's presets also carry the games they are linked to (GG's QuickSet, done by the daemon:
//! `platinum::games`): those of GG's links whose game is installed here, and the programs the user
//! linked himself (game-links.txt, which the window writes and the daemon reads). While a game's
//! preset is on Gaming, the daemon says so in its own file; Gaming's current preset is then that
//! one, and choosing another in the window ends it (the mark goes into state.json).

use crate::persist::{self, FAVORITES, OwnPreset, State};
use crate::presets::{self, PresetRef, ROLES};
use platinum::games::{self, Installed, Saved, UserLink};
use platinum::i18n::tr;
use platinum::trf;
use platinum::library::{self, Library};
use platinum::preset::{self, Preset};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// How often the games installed here are looked at again (for the "Par jeu" filter).
const INSTALLED_PERIOD: Duration = Duration::from_secs(60);

/// Where a preset comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Neutral,
    Own,
    GgLibrary,
    /// Made by the user in GG.
    GgUser,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Neutral => "Platinum",
            Source::Own => tr("Mes préréglages"),
            Source::GgLibrary => tr("Bibliothèque GG"),
            Source::GgUser => tr("Créé dans GG"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub key: PresetRef,
    pub name: String,
    pub source: Source,
    /// GG marks it "Nouveau" and the user has not chosen it since.
    pub is_new: bool,
    /// What choosing it applies (for a GG preset, with the user's edits made in GG).
    pub data: Preset,
    /// What "Réinitialiser" goes back to, and what "modifié" is measured against (for a GG preset,
    /// its factory version, as GG's own Reset).
    pub reference: Preset,
    /// Gaming only: the games it is linked to, as the "Par jeu" filter shows them ("DELTARUNE
    /// (Steam)" for GG's links to an installed game, "Associé : DELTARUNE" for the user's).
    pub games: Vec<String>,
}

/// One channel's presets.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Shelf {
    pub entries: Vec<Entry>,
    /// At most nine, each one of `entries`.
    pub favorites: Vec<PresetRef>,
}

impl Shelf {
    pub fn entry(&self, key: &PresetRef) -> Option<&Entry> {
        self.entries.iter().find(|e| &e.key == key)
    }

    pub fn is_favorite(&self, key: &PresetRef) -> bool {
        self.favorites.contains(key)
    }

    /// Why `name` cannot name a new preset of this channel (or the one being renamed), in French;
    /// `None` when it can. Names are unique per channel, ignoring case, as in GG.
    pub fn name_problem(&self, name: &str, renaming: Option<&str>) -> Option<&'static str> {
        let name = name.trim();
        if name.is_empty() {
            return Some(tr("Donnez un nom au préréglage."));
        }
        if name.chars().count() > NAME_LIMIT {
            return Some(tr("Nom trop long (60 caractères au plus)."));
        }
        let taken = self.entries.iter().any(|e| e.name.eq_ignore_ascii_case(name) && renaming.is_none_or(|old| !old.eq_ignore_ascii_case(&e.name)));
        taken.then_some(tr("Nom déjà utilisé"))
    }

    /// `base`, or `base 2`, `base 3`… : the first that no preset of the channel uses.
    pub fn unique_name(&self, base: &str) -> String {
        let base: String = base.trim().chars().take(NAME_LIMIT - 4).collect();
        let base = if base.is_empty() { tr("Mon préréglage").to_string() } else { base };
        let free = |name: &str| !self.entries.iter().any(|e| e.name.eq_ignore_ascii_case(name));
        if free(&base) {
            return base;
        }
        (2..).map(|n| format!("{base} {n}")).find(|name| free(name)).unwrap_or(base)
    }
}

/// Longest preset name accepted.
pub const NAME_LIMIT: usize = 60;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Catalog {
    /// In the engine's channel order.
    pub shelves: Vec<Shelf>,
    /// Why GG's library is not offered, in French; empty when it is.
    pub note: String,
}

impl Catalog {
    pub fn shelf(&self, channel: usize) -> Option<&Shelf> {
        self.shelves.get(channel)
    }

    pub fn entry(&self, channel: usize, key: &PresetRef) -> Option<&Entry> {
        self.shelf(channel).and_then(|s| s.entry(key))
    }
}

/// The browser's filters, GG's categories; "Par jeu" stands for GG's QuickSet (Gaming only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    All,
    Library,
    Favorites,
    Mine,
    New,
    /// Presets linked to a game: GG's links to a game installed here, and the user's own.
    Games,
}

impl Filter {
    pub const ALL: [Filter; 6] = [Filter::All, Filter::Library, Filter::Favorites, Filter::Mine, Filter::New, Filter::Games];

    pub fn from_index(index: i32) -> Filter {
        usize::try_from(index).ok().and_then(|i| Filter::ALL.get(i).copied()).unwrap_or(Filter::All)
    }

    fn admits(self, entry: &Entry, shelf: &Shelf) -> bool {
        match self {
            Filter::All => true,
            Filter::Library => entry.source == Source::GgLibrary,
            Filter::Favorites => shelf.is_favorite(&entry.key),
            Filter::Mine => matches!(entry.source, Source::Own | Source::GgUser),
            Filter::New => entry.is_new,
            Filter::Games => !entry.games.is_empty(),
        }
    }
}

/// The presets the browser shows for a filter and a search (GG's: the name contains it, ignoring
/// case), in browser order.
pub fn browse<'a>(shelf: &'a Shelf, filter: Filter, query: &str) -> Vec<&'a Entry> {
    let query = query.trim().to_lowercase();
    shelf.entries.iter().filter(|e| filter.admits(e, shelf) && e.name.to_lowercase().contains(&query)).collect()
}

/// How many presets each filter holds, in `Filter::ALL` order.
pub fn counts(shelf: &Shelf) -> [usize; 6] {
    Filter::ALL.map(|filter| shelf.entries.iter().filter(|e| filter.admits(e, shelf)).count())
}

/// The games each of Gaming's presets is linked to, by preset key: the user's links first
/// ("Associé : DELTARUNE"), then GG's links to games installed here (`gg`, from
/// `games::installed_links`).
pub fn game_labels(gg: &[(String, Vec<String>)], links: &[UserLink]) -> Vec<(String, Vec<String>)> {
    let mut labels: Vec<(String, Vec<String>)> = Vec::new();
    let mut add = |key: &str, label: String| match labels.iter_mut().find(|(k, _)| k == key) {
        Some((_, list)) => list.push(label),
        None => labels.push((key.to_string(), vec![label])),
    };
    for link in links {
        add(&link.preset, trf!("Associé : {game}", game = if link.title.is_empty() { games::program_name(&link.exe) } else { link.title.clone() }));
    }
    for (key, games) in gg {
        for game in games {
            add(key, game.clone());
        }
    }
    labels
}

/// Builds every channel's shelf. `labels` are Gaming's game links (`game_labels`).
pub fn build(state: &State, library: Option<&Library>, own: &[Vec<OwnPreset>], note: String, labels: &[(String, Vec<String>)]) -> Catalog {
    let shelves = ROLES
        .iter()
        .enumerate()
        .map(|(index, role)| {
            let flat = preset::flat(*role);
            let mut entries = vec![Entry { key: PresetRef::Flat, name: preset::NEUTRAL_NAME.into(), source: Source::Neutral, is_new: false, data: flat, reference: flat, games: Vec::new() }];
            for mine in own.get(index).into_iter().flatten() {
                entries.push(Entry { key: PresetRef::Own(mine.name.clone()), name: mine.name.clone(), source: Source::Own, is_new: false, data: mine.data, reference: mine.data, games: Vec::new() });
            }
            for config in library.map(|l| l.configs(*role)).unwrap_or_default() {
                entries.push(Entry {
                    key: PresetRef::Gg(config.id.clone()),
                    name: config.name.clone(),
                    source: if config.is_preset { Source::GgLibrary } else { Source::GgUser },
                    is_new: config.is_new() && !state.seen_new.contains(&config.id),
                    data: config.data,
                    reference: config.default_data,
                    games: Vec::new(),
                });
            }
            if *role == preset::Role::Game {
                for entry in &mut entries {
                    let key = entry.key.key();
                    if let Some((_, games)) = labels.iter().find(|(k, _)| *k == key) {
                        entry.games = games.clone();
                    }
                }
            }
            let favorites = state.favorites[index].iter().filter(|key| entries.iter().any(|e| &e.key == *key)).take(FAVORITES).cloned().collect();
            Shelf { entries, favorites }
        })
        .collect();
    Catalog { shelves, note }
}

/// The modification time of GG's database, to read it again only when GG changed it.
fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// The engine's presets: state.json, GG's library, the user's own presets, and the catalog built
/// from them.
pub struct Presets {
    pub state: State,
    library: Option<Library>,
    library_time: Option<SystemTime>,
    note: String,
    own: Vec<Vec<OwnPreset>>,
    /// The user's links of Gaming's presets to programs (game-links.txt).
    pub links: Vec<UserLink>,
    /// The games installed here, when they were read, and GG's links to them by preset key.
    installed: Vec<Installed>,
    installed_read: Option<Instant>,
    installed_links: Vec<(String, Vec<String>)>,
    /// The game preset the daemon put on Gaming, as its file says (`None`: none).
    game: Option<Saved>,
    pub catalog: Arc<Catalog>,
    /// Changed by the user and not saved yet.
    pub dirty: bool,
    /// What could not be read, for the engine's notifications: (what, why).
    pub problems: Vec<(String, String)>,
}

impl Presets {
    /// Reads everything; writes nothing.
    pub fn load() -> Presets {
        let mut presets = Presets {
            state: persist::load_state(),
            library: None,
            library_time: None,
            note: String::new(),
            own: Vec::new(),
            links: games::load_links(),
            installed: Vec::new(),
            installed_read: None,
            installed_links: Vec::new(),
            game: games::load().ok().flatten(),
            catalog: Arc::default(),
            dirty: false,
            problems: Vec::new(),
        };
        presets.read_library();
        presets.read_own();
        presets.read_installed();
        // GG's favourites become Platinum's once; after that the list is Platinum's own.
        if let (false, Some(library)) = (presets.state.favorites_seeded, &presets.library) {
            for (index, role) in ROLES.iter().enumerate() {
                presets.state.favorites[index] = library.favorites(*role).iter().take(FAVORITES).map(|c| PresetRef::Gg(c.id.clone())).collect();
            }
            presets.state.favorites_seeded = true;
        }
        presets.seed_selection();
        presets.rebuild();
        presets
    }

    /// On the first start (a channel whose settings were never read), the preset GG had selected
    /// on each channel is the one shown, as the module holds it: the user finds GG's choice again,
    /// with "Modifié" only where the module really differs. In memory only (nothing is marked to be
    /// saved), so that opening the window, or the layout check, writes nothing.
    fn seed_selection(&mut self) {
        let Some(library) = &self.library else { return };
        for (index, role) in ROLES.iter().enumerate() {
            let slot = &mut self.state.channels[index];
            if slot.data.is_some() || slot.selection != PresetRef::Custom {
                continue;
            }
            if let Some(config) = library.selected(*role) {
                slot.selection = PresetRef::Gg(config.id.clone());
                slot.name = config.name.clone();
            }
        }
    }

    /// GG's database, or without GG, the library rebuilt from GG's migrations (`library::default_source`).
    fn read_library(&mut self) {
        self.library_time = modified(&library::source_path());
        match Library::open_default() {
            Ok(library) => {
                self.library = Some(library);
                self.note.clear();
            }
            Err(library::Error::Missing(_)) => {
                self.library = None;
                self.note = tr("La base de préréglages de GG est introuvable : seuls le préréglage neutre et vos préréglages sont proposés.").into();
            }
            Err(library::Error::Unreadable(why)) => {
                // Keep what was read before, if anything: GG may be writing the file right now.
                self.note = tr("La base de préréglages de GG n'a pas pu être lue.").into();
                self.problems.push((tr("La bibliothèque de préréglages de GG est illisible").into(), why));
            }
        }
    }

    fn read_own(&mut self) {
        let Ok(root) = persist::presets_root() else { return };
        self.own = ROLES
            .iter()
            .map(|role| {
                let (list, problems) = persist::load_own(&root, *role);
                self.problems.extend(problems.into_iter().map(|why| (tr("Un de vos préréglages n'a pas pu être lu").to_string(), why)));
                list
            })
            .collect();
    }

    /// Reads GG's database again when its file changed (GG ran in between), or the migrations
    /// when their folder did. Returns whether it did.
    pub fn refresh_library(&mut self) -> bool {
        if modified(&library::source_path()) == self.library_time {
            return false;
        }
        self.read_library();
        self.link_installed();
        self.rebuild();
        true
    }

    /// The games installed here (what their launchers record), and GG's links to them.
    fn read_installed(&mut self) {
        self.installed = platinum::gamescan::installed();
        self.installed_read = Some(Instant::now());
        self.link_installed();
    }

    fn link_installed(&mut self) {
        let gg = self.library.as_ref().map(games::gg_links).unwrap_or_default();
        self.installed_links = games::installed_links(&self.installed, &gg);
    }

    /// Looks at the games installed again now and then (a game installed while the window is open
    /// joins "Par jeu"). Returns whether the catalog changed.
    pub fn refresh_installed(&mut self) -> bool {
        if self.installed_read.is_some_and(|at| at.elapsed() < INSTALLED_PERIOD) {
            return false;
        }
        let before = self.installed_links.clone();
        self.read_installed();
        if self.installed_links == before {
            return false;
        }
        self.rebuild();
        true
    }

    fn rebuild(&mut self) {
        let labels = game_labels(&self.installed_links, &self.links);
        self.catalog = Arc::new(build(&self.state, self.library.as_ref(), &self.own, self.note.clone(), &labels));
    }

    /// The daemon's override file as read now: a game's preset on Gaming, or none.
    pub fn set_game(&mut self, game: Option<Saved>) {
        self.game = game;
    }

    /// The game whose preset is on Gaming, unless the user ended it by choosing another preset.
    pub fn game_override(&self) -> Option<&Saved> {
        self.game.as_ref().filter(|saved| saved.id() != self.state.game_ended)
    }

    /// While a game's preset is on Gaming and the user changes Gaming's preset in the window, the
    /// game's override ends: state.json says so (the daemon then puts nothing back when the game
    /// exits), and Gaming's selection becomes the game's preset, which the module holds, before
    /// the user's change is made on it.
    fn end_game_override(&mut self, channel: usize) {
        if channel != 0 {
            return;
        }
        let Some(saved) = self.game_override().cloned() else { return };
        self.state.game_ended = saved.id();
        let slot = &mut self.state.channels[0];
        slot.selection = PresetRef::parse(&saved.preset);
        slot.name = saved.preset_name;
        self.dirty = true;
    }

    /// Links a program to a preset of Gaming's (replacing the program's previous link), and saves
    /// game-links.txt; the daemon reads it within two seconds.
    pub fn link_game(&mut self, key: &PresetRef, exe: &str, title: &str) -> Result<(), String> {
        let entry = self.catalog.entry(0, key).cloned().ok_or(tr("ce préréglage n'existe plus"))?;
        if *key == PresetRef::Custom {
            return Err(tr("seul un préréglage peut être associé à un jeu").into());
        }
        let mut links = self.links.clone();
        games::set_link(&mut links, UserLink { exe: exe.to_string(), title: title.to_string(), preset: key.key(), preset_name: entry.name });
        games::save_links(&links)?;
        self.links = links;
        self.rebuild();
        Ok(())
    }

    /// Removes a program's link, and saves game-links.txt.
    pub fn unlink_game(&mut self, exe: &str) -> Result<(), String> {
        let mut links = self.links.clone();
        if !games::remove_link(&mut links, exe) {
            return Ok(());
        }
        games::save_links(&links)?;
        self.links = links;
        self.rebuild();
        Ok(())
    }

    /// A preset of Gaming's renamed or deleted: its links follow, or go.
    fn relink(&mut self, channel: usize, old: &PresetRef, new: Option<&PresetRef>) -> Result<(), String> {
        if channel != 0 {
            return Ok(());
        }
        let mut links = self.links.clone();
        let target = new.map(|key| (key.key(), match key {
            PresetRef::Own(name) => name.clone(),
            _ => String::new(),
        }));
        if games::relink(&mut links, &old.key(), target.as_ref().map(|(key, name)| (key.as_str(), name.as_str()))) {
            games::save_links(&links)?;
            self.links = links;
        }
        Ok(())
    }

    pub fn current(&self, channel: usize) -> Option<&Preset> {
        self.state.channels.get(channel).and_then(|c| c.data.as_ref())
    }

    /// The module read back: not a change of the user's, so nothing is saved for it.
    pub fn set_current(&mut self, channel: usize, data: Preset) {
        if let Some(slot) = self.state.channels.get_mut(channel) {
            slot.data = Some(data);
        }
    }

    /// An edit of the user's. It is kept with the preset the channel uses, as GG saves an edit into
    /// the preset itself: choosing that preset again later brings it back. A copy that holds the
    /// preset's own settings is not kept.
    pub fn edited(&mut self, channel: usize, data: Preset) {
        self.set_current(channel, data);
        if let Some(entry) = self.selected_entry(channel).cloned() {
            if let Some(copies) = self.state.edited.get_mut(channel) {
                if presets::same_content(&data, &entry.data) {
                    copies.remove(&entry.key);
                } else {
                    copies.set(&entry.key, data);
                }
            }
        }
        self.dirty = true;
    }

    /// What choosing `entry` applies: the user's edits of it when there are any, else its own
    /// settings.
    pub fn data_for(&self, channel: usize, entry: &Entry) -> Preset {
        self.state.edited.get(channel).and_then(|copies| copies.get(&entry.key)).copied().unwrap_or(entry.data)
    }

    /// "Réinitialiser": the edits kept for the channel's preset are dropped.
    pub fn forget_edits(&mut self, channel: usize) {
        let key = self.selection(channel);
        if self.state.edited.get_mut(channel).is_some_and(|copies| copies.remove(&key)) {
            self.dirty = true;
        }
    }

    /// The preset a channel uses: on Gaming, a game's while the daemon has put it there.
    pub fn selection(&self, channel: usize) -> PresetRef {
        if let (0, Some(saved)) = (channel, self.game_override()) {
            return PresetRef::parse(&saved.preset);
        }
        self.state.channels.get(channel).map(|c| c.selection.clone()).unwrap_or_default()
    }

    /// The name of the preset a channel uses, kept for when it cannot be found any more.
    pub fn selection_name(&self, channel: usize) -> String {
        if let (0, Some(saved)) = (channel, self.game_override()) {
            return saved.preset_name.clone();
        }
        self.state.channels.get(channel).map(|c| c.name.clone()).unwrap_or_default()
    }

    /// The entry a channel uses, when there is one.
    pub fn selected_entry(&self, channel: usize) -> Option<&Entry> {
        self.catalog.entry(channel, &self.selection(channel))
    }

    /// Records the choice of a preset; a GG preset chosen loses its "Nouveau" tag. On Gaming, a
    /// game's preset put there by the daemon ends with it.
    pub fn select(&mut self, channel: usize, entry: &Entry) {
        self.end_game_override(channel);
        if let Some(slot) = self.state.channels.get_mut(channel) {
            slot.selection = entry.key.clone();
            slot.name = entry.name.clone();
        }
        if let (true, PresetRef::Gg(id)) = (entry.is_new, &entry.key) {
            self.state.seen_new.push(id.clone());
            self.rebuild();
        }
        self.dirty = true;
    }

    /// Moves a favourite to another place in the bar, as GG's `SetFavoritePosition` does: taken
    /// out, then put back at `to` (the others shift).
    pub fn move_favorite(&mut self, channel: usize, key: &PresetRef, to: usize) -> Result<(), String> {
        let list = self.state.favorites.get_mut(channel).ok_or(tr("canal inconnu"))?;
        if !move_in(list, key, to) {
            return Ok(());
        }
        self.dirty = true;
        self.rebuild();
        Ok(())
    }

    pub fn toggle_favorite(&mut self, channel: usize, key: &PresetRef) -> Result<(), String> {
        let list = self.state.favorites.get_mut(channel).ok_or(tr("canal inconnu"))?;
        if let Some(index) = list.iter().position(|k| k == key) {
            list.remove(index);
        } else if list.len() >= FAVORITES {
            return Err(tr("Nombre maximum de favoris atteint").into());
        } else {
            list.push(key.clone());
        }
        self.dirty = true;
        self.rebuild();
        Ok(())
    }

    fn own_index(&self, channel: usize, name: &str) -> Option<usize> {
        self.own.get(channel)?.iter().position(|p| p.name == name)
    }

    fn checked_name(&self, channel: usize, name: &str, renaming: Option<&str>) -> Result<String, String> {
        let shelf = self.catalog.shelf(channel).ok_or(tr("canal inconnu"))?;
        match shelf.name_problem(name, renaming) {
            Some(problem) => Err(problem.to_string()),
            None => Ok(name.trim().to_string()),
        }
    }

    /// Adds a preset of the user's holding `data`, and returns its entry.
    pub fn add_own(&mut self, channel: usize, name: &str, data: Preset) -> Result<Entry, String> {
        let name = self.checked_name(channel, name, None)?;
        let role = *ROLES.get(channel).ok_or(tr("canal inconnu"))?;
        let file = persist::write_own(&persist::presets_root()?, role, &name, &data, None)?;
        if self.own.len() < ROLES.len() {
            self.own.resize(ROLES.len(), Vec::new());
        }
        self.own[channel].push(OwnPreset { name: name.clone(), file, data });
        self.own[channel].sort_by_key(|p| p.name.to_uppercase());
        self.rebuild();
        self.catalog.entry(channel, &PresetRef::Own(name)).cloned().ok_or_else(|| tr("préréglage enregistré mais introuvable").into())
    }

    /// Saves the channel's current settings into the user's preset it uses.
    pub fn save_own(&mut self, channel: usize) -> Result<(), String> {
        let PresetRef::Own(name) = self.selection(channel) else { return Err(tr("seuls vos propres préréglages s'enregistrent").into()) };
        let data = *self.current(channel).ok_or(tr("réglages du canal inconnus"))?;
        let index = self.own_index(channel, &name).ok_or(tr("ce préréglage n'existe plus"))?;
        let role = ROLES[channel];
        let file = self.own[channel][index].file.clone();
        let written = persist::write_own(&persist::presets_root()?, role, &name, &data, Some(&file))?;
        self.own[channel][index] = OwnPreset { name: name.clone(), file: written, data };
        // The file now holds the edits: no copy is needed any more.
        if self.state.edited[channel].remove(&PresetRef::Own(name)) {
            self.dirty = true;
        }
        self.rebuild();
        Ok(())
    }

    /// Renames the user's preset the channel uses; the favourites, its links to games and the
    /// other channels follow.
    pub fn rename_own(&mut self, channel: usize, new_name: &str) -> Result<(), String> {
        self.end_game_override(channel);
        let PresetRef::Own(old) = self.selection(channel) else { return Err(tr("seuls vos propres préréglages se renomment").into()) };
        let new_name = self.checked_name(channel, new_name, Some(&old))?;
        let index = self.own_index(channel, &old).ok_or(tr("ce préréglage n'existe plus"))?;
        let preset = self.own[channel][index].clone();
        let written = persist::write_own(&persist::presets_root()?, ROLES[channel], &new_name, &preset.data, Some(&preset.file))?;
        self.own[channel][index] = OwnPreset { name: new_name.clone(), file: written, data: preset.data };
        self.own[channel].sort_by_key(|p| p.name.to_uppercase());
        let (old_key, new_key) = (PresetRef::Own(old), PresetRef::Own(new_name.clone()));
        for key in self.state.favorites[channel].iter_mut().filter(|k| **k == old_key) {
            *key = new_key.clone();
        }
        self.state.edited[channel].rename(&old_key, &new_key);
        self.state.channels[channel].selection = new_key.clone();
        self.state.channels[channel].name = new_name;
        self.dirty = true;
        let relinked = self.relink(channel, &old_key, Some(&new_key));
        self.rebuild();
        relinked.map_err(|error| trf!("préréglage renommé, mais ses associations à des jeux n'ont pas suivi ({error})", error = error))
    }

    /// Deletes the user's preset the channel uses; the channel keeps its settings, as
    /// "Personnalisé", and its links to games go with it.
    pub fn delete_own(&mut self, channel: usize) -> Result<(), String> {
        self.end_game_override(channel);
        let PresetRef::Own(name) = self.selection(channel) else { return Err(tr("seuls vos propres préréglages se suppriment").into()) };
        let index = self.own_index(channel, &name).ok_or(tr("ce préréglage n'existe plus"))?;
        persist::delete_own(&self.own[channel][index].file)?;
        self.own[channel].remove(index);
        let key = PresetRef::Own(name);
        self.state.favorites[channel].retain(|k| *k != key);
        self.state.edited[channel].remove(&key);
        self.state.channels[channel].selection = PresetRef::Custom;
        self.dirty = true;
        let relinked = self.relink(channel, &key, None);
        self.rebuild();
        relinked.map_err(|error| trf!("préréglage supprimé, mais ses associations à des jeux restent ({error})", error = error))
    }

    /// Saves state.json when the user changed something (never called in read-only mode).
    pub fn save(&mut self) -> Result<(), String> {
        if !self.dirty {
            return Ok(());
        }
        self.dirty = false;
        persist::save_state(&self.state)
    }
}

/// Moves `key` to place `to` in `list` (clamped to its end); returns whether the list changed.
pub fn move_in(list: &mut Vec<PresetRef>, key: &PresetRef, to: usize) -> bool {
    let Some(from) = list.iter().position(|k| k == key) else { return false };
    let to = to.min(list.len() - 1);
    if from == to {
        return false;
    }
    let moved = list.remove(from);
    list.insert(to, moved);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use platinum::preset::{Role, flat};

    #[test]
    fn a_favourite_moves_and_the_others_shift() {
        let key = |n: &str| PresetRef::Own(n.into());
        let mut list = vec![key("a"), key("b"), key("c"), key("d")];
        assert!(move_in(&mut list, &key("d"), 0));
        assert_eq!(list, [key("d"), key("a"), key("b"), key("c")]);
        assert!(move_in(&mut list, &key("d"), 99));
        assert_eq!(list, [key("a"), key("b"), key("c"), key("d")]);
        assert!(!move_in(&mut list, &key("b"), 1));
        assert!(!move_in(&mut list, &key("z"), 0));
    }

    fn entry(name: &str, source: Source, is_new: bool) -> Entry {
        let key = match source {
            Source::Own => PresetRef::Own(name.into()),
            Source::Neutral => PresetRef::Flat,
            _ => PresetRef::Gg(format!("id-{name}")),
        };
        Entry { key, name: name.into(), source, is_new, data: flat(Role::Game), reference: flat(Role::Game), games: Vec::new() }
    }

    fn shelf() -> Shelf {
        let mut apex = entry("Apex Legends", Source::GgLibrary, true);
        apex.games = vec!["Apex Legends (Steam)".into()];
        let entries = vec![entry(preset::NEUTRAL_NAME, Source::Neutral, false), entry("Soir", Source::Own, false), entry("Custom", Source::GgUser, false), apex, entry("Music: Rock", Source::GgLibrary, false)];
        Shelf { favorites: vec![PresetRef::Gg("id-Music: Rock".into())], entries }
    }

    #[test]
    fn filters_and_search_follow_ggs_categories() {
        let shelf = shelf();
        assert_eq!(counts(&shelf), [5, 2, 1, 2, 1, 1]);
        let names = |filter, query| browse(&shelf, filter, query).iter().map(|e| e.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(Filter::Mine, ""), ["Soir", "Custom"]);
        assert_eq!(names(Filter::All, "MUSIC"), ["Music: Rock"]);
        assert_eq!(names(Filter::Library, "e"), ["Apex Legends"]);
        assert_eq!(names(Filter::Games, ""), ["Apex Legends"]);
        assert!(names(Filter::New, "rock").is_empty());
        assert_eq!(Filter::from_index(5), Filter::Games);
        assert_eq!(Filter::from_index(9), Filter::All);
    }

    #[test]
    fn gamings_presets_carry_their_games_the_users_links_first() {
        let gg = vec![("gg:deltarune".to_string(), vec!["DELTARUNE (Steam)".to_string()])];
        let links = vec![
            UserLink { exe: "DELTARUNE.exe".into(), title: "DELTARUNE".into(), preset: "gg:deltarune".into(), preset_name: "Deltarune".into() },
            UserLink { exe: "hl2.exe".into(), title: String::new(), preset: "own:Soir".into(), preset_name: "Soir".into() },
        ];
        let labels = game_labels(&gg, &links);
        assert_eq!(labels, [
            ("gg:deltarune".to_string(), vec!["Associé : DELTARUNE".to_string(), "DELTARUNE (Steam)".to_string()]),
            ("own:Soir".to_string(), vec!["Associé : hl2".to_string()]),
        ]);
        // Only Gaming's shelf carries them.
        let own = vec![vec![OwnPreset { name: "Soir".into(), file: "Soir.json".into(), data: flat(Role::Game) }], Vec::new(), vec![OwnPreset { name: "Soir".into(), file: "Soir.json".into(), data: flat(Role::Media) }], Vec::new(), Vec::new()];
        let catalog = build(&State::default(), None, &own, String::new(), &labels);
        assert_eq!(catalog.entry(0, &PresetRef::Own("Soir".into())).map(|e| e.games.clone()), Some(vec!["Associé : hl2".to_string()]));
        assert!(catalog.entry(2, &PresetRef::Own("Soir".into())).is_some_and(|e| e.games.is_empty()));
        assert_eq!(counts(catalog.shelf(0).unwrap())[5], 1);
    }

    #[test]
    fn a_games_preset_is_gamings_until_the_user_chooses_another() {
        let mut presets = Presets {
            state: State::default(),
            library: None,
            library_time: None,
            note: String::new(),
            own: Vec::new(),
            links: Vec::new(),
            installed: Vec::new(),
            installed_read: Some(Instant::now()),
            installed_links: Vec::new(),
            game: None,
            catalog: Arc::default(),
            dirty: false,
            problems: Vec::new(),
        };
        presets.rebuild();
        presets.state.channels[0].selection = PresetRef::Own("Soir".into());
        presets.state.channels[0].name = "Soir".into();
        let saved = Saved {
            game: "DELTARUNE".into(),
            game_key: "steam:1671210".into(),
            preset: "flat".into(),
            preset_name: preset::NEUTRAL_NAME.into(),
            restore: "own:Soir".into(),
            restore_name: "Soir".into(),
            before: None,
            since: "20261117-213000".into(),
        };
        presets.set_game(Some(saved.clone()));
        // Gaming shows the game's preset; the other channels and state.json's own choice do not
        // change (the daemon puts that one back when the game exits).
        assert_eq!((presets.selection(0), presets.selection_name(0)), (PresetRef::Flat, preset::NEUTRAL_NAME.to_string()));
        assert_eq!(presets.selection(1), PresetRef::Custom);
        assert_eq!(presets.state.channels[0].selection, PresetRef::Own("Soir".into()));
        assert!(presets.selected_entry(0).is_some_and(|e| e.key == PresetRef::Flat));
        // Choosing a preset ends it: state.json says so, and the choice is the user's.
        let flat_entry = presets.catalog.entry(0, &PresetRef::Flat).cloned().unwrap();
        presets.select(0, &flat_entry);
        assert_eq!(presets.state.game_ended, saved.id());
        assert!(presets.game_override().is_none() && presets.dirty);
        assert_eq!(presets.selection(0), PresetRef::Flat);
        // A later game (another override) shows again.
        presets.set_game(Some(Saved { since: "20261117-220000".into(), ..saved }));
        assert!(presets.game_override().is_some());
    }

    #[test]
    fn names_are_unique_per_channel() {
        let shelf = shelf();
        assert_eq!(shelf.name_problem("  ", None), Some("Donnez un nom au préréglage."));
        assert_eq!(shelf.name_problem("custom", None), Some("Nom déjà utilisé"));
        assert_eq!(shelf.name_problem("SOIR", Some("Soir")), None, "renaming to itself, other case");
        assert_eq!(shelf.name_problem(&"x".repeat(61), None), Some("Nom trop long (60 caractères au plus)."));
        assert_eq!(shelf.unique_name("Soir"), "Soir 2");
        assert_eq!(shelf.unique_name("Matin"), "Matin");
        assert_eq!(shelf.unique_name(""), "Mon préréglage");
    }

    #[test]
    fn the_catalog_lists_the_neutral_preset_first_and_keeps_known_favourites() {
        let mut state = State::default();
        state.favorites[1] = vec![PresetRef::Own("Gone".into()), PresetRef::Flat, PresetRef::Own("Mine".into())];
        let own = vec![Vec::new(), vec![OwnPreset { name: "Mine".into(), file: "Mine.json".into(), data: flat(Role::ChatRender) }], Vec::new(), Vec::new(), Vec::new()];
        let catalog = build(&state, None, &own, "note".into(), &[]);
        assert_eq!(catalog.shelves.len(), 5);
        let chat = catalog.shelf(1).unwrap();
        assert_eq!(chat.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), [preset::NEUTRAL_NAME, "Mine"]);
        assert_eq!(chat.favorites, [PresetRef::Flat, PresetRef::Own("Mine".into())]);
        assert!(catalog.entry(4, &PresetRef::Flat).is_some_and(|e| e.data == flat(Role::ChatCapture)));
        assert_eq!(catalog.note, "note");
    }
}
