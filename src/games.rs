//! Preset per game: GG's QuickSet, done by Platinum itself.
//!
//! GG's library links most of its Game presets to games (`game_ids`, 321 of 353 Game presets on GG
//! 123): comma-separated "<launcher>_<id>" entries, the launcher being a number of GG's game
//! detection (1 Steam application id, 3 GOG product id, 4 Battle.net, 7 Riot, 8 Ubisoft Connect id,
//! 9 Epic catalog item or application name, 10 Microsoft Store package family name, 16 EA app
//! GUID, -1 a display name, others a single title each). GG core detects the games and Sonar
//! selects the preset of the game's "loadout". Platinum detects the games itself (`gamescan`: Steam's
//! running application, the package of each process, the install folders Epic, GOG and Ubisoft
//! Connect record) and the user can link any Game preset to a program himself ("Associer au jeu en
//! cours…"), kept in `game-links.txt` next to the configuration. His links win over GG's.
//!
//! While a linked game runs, the daemon puts its preset on Gaming, exactly as the window applies a
//! preset (with the user's edits of it, the channel's "Effets" switch kept, GG's order, only the
//! settings that change); when the game exits, the preset chosen before comes back, with the
//! user's edits of it. If several linked games run, the one started last wins. Choosing a preset in
//! the window meanwhile ends it: the user's choice stays, and the game's exit changes nothing.
//!
//! What the daemon changed is written down first, in `game-preset.txt` (`Saved`), which only the
//! daemon writes: a game that exits, a daemon that stops and a daemon that crashed in the middle
//! all put back the same preset. The window reads it to show the game's preset as Gaming's current
//! one ("Activé par <jeu>"), and says that the user ended it in its own state.json
//! (`presetfiles::State::game_ended`): neither program ever writes the other's file.
//!
//! Everything here is pure except reading and writing these two files.

use crate::json::Json;
use crate::library::Library;
use crate::preset::{Preset, Role};
use crate::presetfiles::PresetRef;
use std::path::{Path, PathBuf};

/// The override file, in the install folder next to platinum.ini.
pub const STATE_FILE: &str = "game-preset.txt";
/// The user's own links.
pub const LINKS_FILE: &str = "game-links.txt";
/// How many looks in a row without the game end its override: a launcher that restarts its game,
/// or Steam clearing its running application for a moment, is not an exit.
pub const LEAVE_AFTER: u32 = 2;

/// GG's launcher numbers in `game_ids`, those Platinum detects.
pub mod launcher {
    pub const STEAM: i32 = 1;
    pub const GOG: i32 = 3;
    pub const UBISOFT: i32 = 8;
    pub const EPIC: i32 = 9;
    pub const STORE: i32 = 10;
}

/// One entry of GG's `game_ids`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GameId {
    pub launcher: i32,
    pub id: String,
}

impl GameId {
    /// "<launcher>_<id>": the launcher a number (-1 included), the id the rest, which may hold "_"
    /// itself ("13_World_of_Tanks_EU"). Blanks around either are trimmed; anything else is `None`.
    pub fn parse(entry: &str) -> Option<GameId> {
        let (launcher, id) = entry.trim().split_once('_')?;
        let launcher = launcher.trim().parse::<i32>().ok()?;
        let id = id.trim();
        (!id.is_empty()).then(|| GameId { launcher, id: id.to_string() })
    }
}

/// GG's `game_ids` column: comma-separated entries, sometimes with a blank after the comma.
pub fn parse_game_ids(text: &str) -> Vec<GameId> {
    text.split(',').filter_map(GameId::parse).collect()
}

/// A game as Platinum finds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GameKey {
    Steam(u32),
    /// An Epic Games manifest: catalog item, application name and main game's application name
    /// (GG names Epic games by either of the first two).
    Epic { catalog: String, app: String, main: String },
    /// A Microsoft Store package, by family name.
    Store(String),
    Gog(String),
    Ubisoft(String),
    /// A program the user linked, by its executable's file name.
    Exe(String),
}

impl GameKey {
    /// One line that names the game from one look to the next: "steam:1671210", "epic:<catalog>",
    /// "store:<family>", "exe:deltarune.exe" (lowercase where Windows ignores case).
    pub fn text(&self) -> String {
        match self {
            GameKey::Steam(id) => format!("steam:{id}"),
            GameKey::Epic { catalog, app, .. } => format!("epic:{}", if catalog.is_empty() { app.to_lowercase() } else { catalog.to_lowercase() }),
            GameKey::Store(family) => format!("store:{}", family.to_lowercase()),
            GameKey::Gog(id) => format!("gog:{id}"),
            GameKey::Ubisoft(id) => format!("ubisoft:{id}"),
            GameKey::Exe(exe) => format!("exe:{}", exe.to_lowercase()),
        }
    }

    /// Where it comes from, for the window and the command line.
    pub fn source(&self) -> &'static str {
        match self {
            GameKey::Steam(_) => "Steam",
            GameKey::Epic { .. } => "Epic Games",
            GameKey::Store(_) => "Microsoft Store",
            GameKey::Gog(_) => "GOG",
            GameKey::Ubisoft(_) => "Ubisoft Connect",
            GameKey::Exe(_) => "programme",
        }
    }

    /// Whether one of GG's `game_ids` entries names this game.
    pub fn matches(&self, id: &GameId) -> bool {
        let same = |a: &str| !a.is_empty() && a.eq_ignore_ascii_case(&id.id);
        match (self, id.launcher) {
            (GameKey::Steam(app), launcher::STEAM) => id.id.parse::<u32>() == Ok(*app),
            (GameKey::Epic { catalog, app, main }, launcher::EPIC) => same(catalog) || same(app) || same(main),
            (GameKey::Store(family), launcher::STORE) => same(family),
            (GameKey::Gog(product), launcher::GOG) => same(product),
            (GameKey::Ubisoft(install), launcher::UBISOFT) => same(install),
            _ => false,
        }
    }
}

/// A game running now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Running {
    pub key: GameKey,
    /// Its name, for the log and the window ("DELTARUNE").
    pub name: String,
    /// When it started, in Windows' file time (100 ns since 1601): its first process's creation,
    /// or for Steam's running application, when the daemon first saw it.
    pub started: u64,
}

/// A preset of GG's library that names games.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GgLink {
    /// The preset, as `PresetRef::key` names it ("gg:<id>").
    pub preset: String,
    pub name: String,
    pub games: Vec<GameId>,
}

/// GG's links, Game presets only, in the order of GG's browser (the user's own GG presets first,
/// then by name): when several presets name the same game, the first one is chosen.
pub fn gg_links(library: &Library) -> Vec<GgLink> {
    library
        .configs(Role::Game)
        .into_iter()
        .filter_map(|config| {
            let games: Vec<GameId> = config.game_ids.iter().filter_map(|entry| GameId::parse(entry)).collect();
            (!games.is_empty()).then(|| GgLink { preset: PresetRef::Gg(config.id.clone()).key(), name: config.name.clone(), games })
        })
        .collect()
}

/// A program the user linked to a Game preset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserLink {
    /// The program's executable file name ("DELTARUNE.exe"), matched ignoring case.
    pub exe: String,
    /// How it was named when linked: its window's title, else the executable's name.
    pub title: String,
    /// The preset (`PresetRef::key`) and its name when linked.
    pub preset: String,
    pub preset_name: String,
}

/// The user's links as the file holds them: one section per program, named by its executable.
pub fn links_to_text(links: &[UserLink]) -> String {
    let mut text = String::from(
        "# Platinum : les préréglages de Gaming que vous avez associés à vos jeux (menu « … » d'un\n# préréglage de Gaming, « Associer au jeu en cours… »). Quand le programme tourne, Gaming prend\n# le préréglage ; vos associations passent avant celles de GG. Écrit par la fenêtre.\n",
    );
    for link in links {
        text.push_str(&format!("\n[{}]\ntitle = {}\npreset = {}\npreset_name = {}\n", single_line(&link.exe), single_line(&link.title), single_line(&link.preset), single_line(&link.preset_name)));
    }
    text
}

/// Reads the links file's text; a section without a preset is skipped, and a program named twice
/// keeps its last section.
pub fn parse_links(text: &str) -> Vec<UserLink> {
    let mut links: Vec<UserLink> = Vec::new();
    let mut current: Option<UserLink> = None;
    let keep = |link: Option<UserLink>, links: &mut Vec<UserLink>| {
        if let Some(link) = link.filter(|l| !l.exe.is_empty() && PresetRef::parse(&l.preset) != PresetRef::Custom) {
            set_link(links, link);
        }
    };
    for line in text.trim_start_matches('\u{feff}').lines().map(str::trim) {
        if let Some(exe) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            keep(current.take(), &mut links);
            current = Some(UserLink { exe: exe.trim().to_string(), title: String::new(), preset: String::new(), preset_name: String::new() });
            continue;
        }
        let (Some(link), Some((key, value))) = (current.as_mut(), line.split_once('=')) else { continue };
        let value = value.trim().to_string();
        match key.trim() {
            "title" => link.title = value,
            "preset" => link.preset = value,
            "preset_name" => link.preset_name = value,
            _ => {}
        }
    }
    keep(current, &mut links);
    links
}

/// Adds a link, replacing the one of the same program: a program has one preset.
pub fn set_link(links: &mut Vec<UserLink>, link: UserLink) {
    links.retain(|l| !l.exe.eq_ignore_ascii_case(&link.exe));
    links.push(link);
}

/// Removes a program's link; returns whether there was one.
pub fn remove_link(links: &mut Vec<UserLink>, exe: &str) -> bool {
    let before = links.len();
    links.retain(|l| !l.exe.eq_ignore_ascii_case(exe));
    links.len() != before
}

/// A preset renamed or deleted: its links follow it, or go with it.
pub fn relink(links: &mut Vec<UserLink>, old: &str, new: Option<(&str, &str)>) -> bool {
    let mut changed = false;
    match new {
        Some((key, name)) => {
            for link in links.iter_mut().filter(|l| l.preset == old) {
                link.preset = key.to_string();
                link.preset_name = name.to_string();
                changed = true;
            }
        }
        None => {
            let before = links.len();
            links.retain(|l| l.preset != old);
            changed = links.len() != before;
        }
    }
    changed
}

/// The preset Gaming should use now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub preset: String,
    pub preset_name: String,
    /// The game, by name and by `GameKey::text`.
    pub game: String,
    pub game_key: String,
    /// It comes from a link of the user's, not from GG's library.
    pub by_user: bool,
}

/// The preset for the games running now, if any. A program the user linked comes first (a game he
/// linked himself wins over what GG says of it, and of any other game); among games of the same
/// rank, the one started last wins; a game several of GG's presets name takes the first of them in
/// `gg` order.
pub fn choose(running: &[Running], gg: &[GgLink], user: &[UserLink]) -> Option<Choice> {
    let latest = |candidates: Vec<(u64, Choice)>| {
        let mut best: Option<(u64, Choice)> = None;
        for (started, choice) in candidates {
            if best.as_ref().is_none_or(|(at, _)| started > *at) {
                best = Some((started, choice));
            }
        }
        best.map(|(_, choice)| choice)
    };
    let mine: Vec<(u64, Choice)> = running
        .iter()
        .filter_map(|game| {
            let GameKey::Exe(exe) = &game.key else { return None };
            let link = user.iter().find(|l| l.exe.eq_ignore_ascii_case(exe))?;
            let name = if link.title.is_empty() { game.name.clone() } else { link.title.clone() };
            Some((game.started, Choice { preset: link.preset.clone(), preset_name: link.preset_name.clone(), game: name, game_key: game.key.text(), by_user: true }))
        })
        .collect();
    if let Some(choice) = latest(mine) {
        return Some(choice);
    }
    let ggs: Vec<(u64, Choice)> = running
        .iter()
        .filter_map(|game| {
            let link = gg.iter().find(|l| l.games.iter().any(|id| game.key.matches(id)))?;
            Some((game.started, Choice { preset: link.preset.clone(), preset_name: link.name.clone(), game: game.name.clone(), game_key: game.key.text(), by_user: false }))
        })
        .collect();
    latest(ggs)
}

/// What the daemon changed on Gaming for a game, written down before it changes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Saved {
    /// The game whose preset Gaming uses, by name and by `GameKey::text`.
    pub game: String,
    pub game_key: String,
    /// The preset put on Gaming (`PresetRef::key`) and its name.
    pub preset: String,
    pub preset_name: String,
    /// The preset Gaming used before (`PresetRef::key`, "custom" for settings of no preset) and its
    /// name: what comes back when the game exits.
    pub restore: String,
    pub restore_name: String,
    /// Gaming's settings before, as the module held them: what comes back when the preset of
    /// before cannot be found (settings of no preset, a preset deleted meanwhile).
    pub before: Option<Preset>,
    /// When the game's preset was put on Gaming, local time.
    pub since: String,
}

impl Saved {
    /// What names this override for the window's "ended by the user" mark: when it began, and
    /// which game.
    pub fn id(&self) -> String {
        format!("{} {}", self.since, self.game_key)
    }

    /// The file's text.
    pub fn to_text(&self) -> String {
        format!(
            "# Platinum a mis le préréglage d'un jeu sur Gaming : ce qu'il a changé, pour le remettre\n# quand le jeu se ferme. Écrit et supprimé par la tâche de fond.\ngame = {}\ngame_key = {}\npreset = {}\npreset_name = {}\nrestore = {}\nrestore_name = {}\nsince = {}\nbefore = {}\n",
            single_line(&self.game),
            single_line(&self.game_key),
            single_line(&self.preset),
            single_line(&self.preset_name),
            single_line(&self.restore),
            single_line(&self.restore_name),
            single_line(&self.since),
            self.before.map(|p| p.to_json().to_compact()).unwrap_or_default(),
        )
    }

    /// Reads the file's text; `None` without a game or a preset (nothing could be put back).
    pub fn parse(text: &str) -> Option<Saved> {
        let mut saved = Saved { game: String::new(), game_key: String::new(), preset: String::new(), preset_name: String::new(), restore: String::new(), restore_name: String::new(), before: None, since: String::new() };
        for line in text.trim_start_matches('\u{feff}').lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim().to_string();
            match key.trim() {
                "game" => saved.game = value,
                "game_key" => saved.game_key = value,
                "preset" => saved.preset = value,
                "preset_name" => saved.preset_name = value,
                "restore" => saved.restore = value,
                "restore_name" => saved.restore_name = value,
                "since" => saved.since = value,
                "before" => saved.before = Preset::parse(&value, Role::Game).ok(),
                _ => {}
            }
        }
        (!saved.game_key.is_empty() && !saved.preset.is_empty()).then_some(saved)
    }
}

/// What the daemon does after a look.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Nothing,
    /// A linked game is running and no override is under way: write down, apply its preset.
    Enter(Choice),
    /// Another game wins now (started later, or the preset linked to it changed): its preset goes
    /// on Gaming, and the preset of before stays the one that comes back.
    Switch(Choice),
    /// The game exited, or the feature was turned off: the preset of before comes back.
    Leave,
    /// An override the user ended in the window is over: its file goes, nothing is applied (his
    /// choice stays).
    Drop,
}

/// The memory from one look to the next: how many looks in a row missed the game.
#[derive(Debug, Default)]
pub struct Tracker {
    away: u32,
}

impl Tracker {
    /// The step for a look. `enabled` is `game_presets`; `choice` what `choose` says now; `saved`
    /// the override under way; `dismissed` whether the user ended it in the window. A game that
    /// exits is left after `LEAVE_AFTER` looks without it; another game that starts takes over at
    /// once. An override the user ended stays quiet while its game runs, so that the game is not
    /// put back over his choice, and goes once it no longer runs.
    pub fn decide(&mut self, enabled: bool, choice: Option<&Choice>, saved: Option<&Saved>, dismissed: bool) -> Step {
        let Some(saved) = saved else {
            self.away = 0;
            return match choice {
                Some(choice) if enabled => Step::Enter(choice.clone()),
                _ => Step::Nothing,
            };
        };
        let same_game = choice.is_some_and(|c| c.game_key == saved.game_key);
        if dismissed {
            if enabled && same_game {
                self.away = 0;
                return Step::Nothing;
            }
            // Another game takes over at once (from the user's choice, which is then what comes
            // back); without any, the game must really be gone.
            if !enabled || choice.is_some() {
                self.away = 0;
                return Step::Drop;
            }
            return self.missed(Step::Drop);
        }
        if !enabled {
            self.away = 0;
            return Step::Leave;
        }
        match choice {
            Some(choice) if same_game && choice.preset == saved.preset => {
                self.away = 0;
                Step::Nothing
            }
            Some(choice) => {
                self.away = 0;
                Step::Switch(choice.clone())
            }
            None => self.missed(Step::Leave),
        }
    }

    /// One more look without the game: `step` once there were `LEAVE_AFTER` in a row.
    fn missed(&mut self, step: Step) -> Step {
        self.away += 1;
        if self.away >= LEAVE_AFTER {
            self.away = 0;
            step
        } else {
            Step::Nothing
        }
    }
}

/// A game installed on this machine, from what its launcher records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    pub key: GameKey,
    pub name: String,
    /// Its install folder, for the launchers that say it (Steam's library manifests, Epic, GOG,
    /// Ubisoft Connect): a process running from inside it is the game. Empty otherwise.
    pub folder: String,
}

/// For each of GG's presets that names a game installed here, those games ("DELTARUNE (Steam)"),
/// in `gg` order: the window's "Par jeu" filter.
pub fn installed_links(installed: &[Installed], gg: &[GgLink]) -> Vec<(String, Vec<String>)> {
    gg.iter()
        .filter_map(|link| {
            let games: Vec<String> = installed.iter().filter(|game| link.games.iter().any(|id| game.key.matches(id))).map(|game| format!("{} ({})", game.name, game.key.source())).collect();
            (!games.is_empty()).then(|| (link.preset.clone(), games))
        })
        .collect()
}

/// A running process, as the scanner keeps it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    /// Its executable's file name.
    pub exe: String,
    /// Its executable's full path; empty when it could not be read (a protected process).
    pub path: String,
    /// Its package's family name, for a packaged application; empty otherwise.
    pub family: String,
    /// When it started, in Windows' file time.
    pub created: u64,
}

/// Whether `path` is inside `folder`, ignoring case and the kind of slash.
pub fn is_under(path: &str, folder: &str) -> bool {
    let normal = |text: &str| text.replace('/', "\\").trim_end_matches('\\').to_lowercase();
    let (path, folder) = (normal(path), normal(folder));
    !folder.is_empty() && path.len() > folder.len() && path.starts_with(&folder) && path.as_bytes()[folder.len()] == b'\\'
}

/// The games running among `processes`: an installed game with a folder, while a process runs from
/// inside it; a Microsoft Store package named in `families` (lowercase); a program the user linked.
/// Each game once, started when its first process did.
pub fn running_games(processes: &[Process], installs: &[Installed], families: &[String], user: &[UserLink]) -> Vec<Running> {
    let earliest = |matching: &mut dyn Iterator<Item = &Process>| matching.map(|p| p.created).min();
    let mut found: Vec<Running> = Vec::new();
    for install in installs.iter().filter(|i| !i.folder.is_empty()) {
        if let Some(started) = earliest(&mut processes.iter().filter(|p| is_under(&p.path, &install.folder))) {
            found.push(Running { key: install.key.clone(), name: install.name.clone(), started });
        }
    }
    let mut seen_families: Vec<String> = Vec::new();
    for process in processes.iter().filter(|p| !p.family.is_empty()) {
        let family = process.family.to_lowercase();
        if seen_families.contains(&family) || !families.contains(&family) {
            continue;
        }
        let started = earliest(&mut processes.iter().filter(|p| p.family.eq_ignore_ascii_case(&family))).unwrap_or(process.created);
        found.push(Running { key: GameKey::Store(process.family.clone()), name: program_name(&process.exe), started });
        seen_families.push(family);
    }
    for link in user {
        if let Some(started) = earliest(&mut processes.iter().filter(|p| p.exe.eq_ignore_ascii_case(&link.exe))) {
            let name = if link.title.is_empty() { program_name(&link.exe) } else { link.title.clone() };
            found.push(Running { key: GameKey::Exe(link.exe.to_lowercase()), name, started });
        }
    }
    found
}

/// Each game once: a Steam game Steam started is seen both as Steam's running application and by
/// its folder. The earliest start is kept, which is the more exact one (a process's creation rather
/// than when the daemon first read `RunningAppID`).
pub fn once_each(mut running: Vec<Running>) -> Vec<Running> {
    running.sort_by_key(|game| game.started);
    let mut kept: Vec<Running> = Vec::new();
    for game in running {
        if !kept.iter().any(|known| known.key == game.key) {
            kept.push(game);
        }
    }
    kept
}

/// The Microsoft Store families GG's links name, lowercase: the only packages worth matching.
pub fn store_families(gg: &[GgLink]) -> Vec<String> {
    let mut families: Vec<String> = gg.iter().flat_map(|l| l.games.iter()).filter(|id| id.launcher == launcher::STORE).map(|id| id.id.to_lowercase()).collect();
    families.sort();
    families.dedup();
    families
}

/// A program's name without ".exe".
pub fn program_name(exe: &str) -> String {
    let lower = exe.to_ascii_lowercase();
    if lower.ends_with(".exe") { exe[..exe.len() - 4].to_string() } else { exe.to_string() }
}

/// A packaged application's family name from its full name
/// ("Microsoft.MinecraftUWP_1.21.11201.0_x64__8wekyb3d8bbwe" gives
/// "Microsoft.MinecraftUWP_8wekyb3d8bbwe"): its name and its publisher's id, the first and last of
/// the five fields. Package names hold no "_".
pub fn family_name(full: &str) -> Option<String> {
    let parts: Vec<&str> = full.trim().split('_').collect();
    match parts.as_slice() {
        [name, _version, _architecture, _resource, publisher] if !name.is_empty() && !publisher.is_empty() => Some(format!("{name}_{publisher}")),
        _ => None,
    }
}

/// An Epic Games manifest (`…\EpicGamesLauncher\Data\Manifests\*.item`) as an installed game.
pub fn parse_epic_manifest(text: &str) -> Option<Installed> {
    let json = Json::parse(text.trim_start_matches('\u{feff}')).ok()?;
    let field = |key: &str| json.get(key).and_then(Json::as_str).unwrap_or("").trim().to_string();
    let (catalog, app, main, folder) = (field("CatalogItemId"), field("AppName"), field("MainGameAppName"), field("InstallLocation"));
    if catalog.is_empty() && app.is_empty() {
        return None;
    }
    let name = match field("DisplayName") {
        name if !name.is_empty() => name,
        _ => app.clone(),
    };
    Some(Installed { key: GameKey::Epic { catalog, app, main }, name, folder })
}

/// `%USERPROFILE%\Platinum\game-preset.txt`.
pub fn state_path() -> Result<PathBuf, String> {
    Ok(crate::autostart::install_dir()?.join(STATE_FILE))
}

/// `%USERPROFILE%\Platinum\game-links.txt`.
pub fn links_path() -> Result<PathBuf, String> {
    Ok(crate::autostart::install_dir()?.join(LINKS_FILE))
}

/// The override written down, if any: `Ok(None)` without a file, an error when it cannot be read.
pub fn load() -> Result<Option<Saved>, String> {
    load_from(&state_path()?)
}

pub fn load_from(file: &Path) -> Result<Option<Saved>, String> {
    match std::fs::read(file) {
        Ok(bytes) => Saved::parse(&String::from_utf8_lossy(&bytes)).map(Some).ok_or_else(|| format!("{} holds no game preset that can be put back", file.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot read {}: {error}", file.display())),
    }
}

/// Writes the override down, replacing the whole file at once.
pub fn save(saved: &Saved) -> Result<(), String> {
    save_to(&state_path()?, saved)
}

pub fn save_to(file: &Path, saved: &Saved) -> Result<(), String> {
    crate::config::write_atomically(file, &saved.to_text())
}

/// Removes the override file once Gaming is back (a missing file is fine).
pub fn remove() -> Result<(), String> {
    remove_at(&state_path()?)
}

pub fn remove_at(file: &Path) -> Result<(), String> {
    match std::fs::remove_file(file) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot remove {}: {error}", file.display())),
    }
}

/// The user's links (none without a file).
pub fn load_links() -> Vec<UserLink> {
    links_path().ok().and_then(|file| std::fs::read(file).ok()).map(|bytes| parse_links(&String::from_utf8_lossy(&bytes))).unwrap_or_default()
}

pub fn save_links(links: &[UserLink]) -> Result<(), String> {
    crate::config::write_atomically(&links_path()?, &links_to_text(links))
}

/// A value written on one line.
fn single_line(value: &str) -> String {
    value.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preset::flat;

    fn id(text: &str) -> GameId {
        GameId::parse(text).unwrap()
    }

    fn link(preset: &str, name: &str, ids: &[&str]) -> GgLink {
        GgLink { preset: format!("gg:{preset}"), name: name.into(), games: ids.iter().map(|i| id(i)).collect() }
    }

    fn user(exe: &str, preset: &str) -> UserLink {
        UserLink { exe: exe.into(), title: program_name(exe).to_uppercase(), preset: format!("own:{preset}"), preset_name: preset.into() }
    }

    fn running(key: GameKey, name: &str, started: u64) -> Running {
        Running { key, name: name.into(), started }
    }

    fn library() -> Vec<GgLink> {
        vec![
            link("deltarune", "Deltarune", &["1_1671210", " 1_1690940"]),
            link("fortnite-pro", "Fortnite Pro Preset", &["9_09e442f830a341f698b4da42abd98c9b", "9_Fortnite", "10_436609B6.FORTNITECLIENT_9NCXWBGMMV7M8"]),
            link("fortnite-sky", "Fortnite by SKY", &["9_Fortnite"]),
            link("minecraft", "Minecraft", &["10_MICROSOFT.MINECRAFTUWP_8WEKYB3D8BBWE", "11_Minecraft"]),
            link("witcher", "The Witcher 3", &["3_1207664643"]),
            link("siege", "Rainbow Six Siege", &["8_635", "1_359550"]),
        ]
    }

    #[test]
    fn game_ids_are_read_as_gg_writes_them() {
        assert_eq!(parse_game_ids("7_valorant,20_VALORANT, 20_Valorant2001715"), [id("7_valorant"), id("20_VALORANT"), id("20_Valorant2001715")]);
        assert_eq!(id("13_World_of_Tanks_EU"), GameId { launcher: 13, id: "World_of_Tanks_EU".into() });
        assert_eq!(id("-1_Grand Theft Auto V Enhanced"), GameId { launcher: -1, id: "Grand Theft Auto V Enhanced".into() });
        assert_eq!(id(" 16_{D7FBF176-382D-484E-863A-DFD1124A2A1C} ").launcher, 16);
        // No launcher number, nothing after it, or an empty column: skipped.
        for bad in ["valorant", "x_1", "1_", " ", ""] {
            assert_eq!(GameId::parse(bad), None, "{bad}");
        }
        assert!(parse_game_ids("").is_empty());
        assert_eq!(parse_game_ids("1_570,,junk, 9_Fortnite").len(), 2);
    }

    #[test]
    fn each_launcher_matches_its_own_ids() {
        let steam = GameKey::Steam(1671210);
        assert!(steam.matches(&id("1_1671210")) && !steam.matches(&id("1_1690940")));
        // A Steam number under another launcher is not the same game.
        assert!(!steam.matches(&id("3_1671210")));
        let epic = GameKey::Epic { catalog: "4fe75bbc5a674f4f9b356b5c90567da5".into(), app: "Fortnite".into(), main: String::new() };
        assert!(epic.matches(&id("9_fortnite")) && epic.matches(&id("9_4FE75BBC5A674F4F9B356B5C90567DA5")));
        assert!(!epic.matches(&id("9_Fortnite_Studio")) && !epic.matches(&id("1_Fortnite")));
        // An empty field matches nothing.
        let bare = GameKey::Epic { catalog: String::new(), app: "Pikuniku".into(), main: String::new() };
        assert!(!bare.matches(&GameId { launcher: launcher::EPIC, id: String::new() }) && bare.matches(&id("9_pikuniku")));
        let store = GameKey::Store("Microsoft.MinecraftUWP_8wekyb3d8bbwe".into());
        assert!(store.matches(&id("10_MICROSOFT.MINECRAFTUWP_8WEKYB3D8BBWE")) && !store.matches(&id("11_Minecraft")));
        assert!(GameKey::Gog("1207664643".into()).matches(&id("3_1207664643")));
        assert!(GameKey::Ubisoft("635".into()).matches(&id("8_635")) && !GameKey::Ubisoft("635".into()).matches(&id("1_635")));
        assert!(!GameKey::Exe("deltarune.exe".into()).matches(&id("-1_deltarune.exe")));
        assert_eq!(GameKey::Store("Microsoft.MinecraftUWP_8wekyb3d8bbwe".into()).text(), "store:microsoft.minecraftuwp_8wekyb3d8bbwe");
        assert_eq!(epic.text(), "epic:4fe75bbc5a674f4f9b356b5c90567da5");
        assert_eq!(GameKey::Exe("DELTARUNE.exe".into()).text(), "exe:deltarune.exe");
    }

    #[test]
    fn the_game_started_last_wins_and_the_users_links_come_first() {
        let gg = library();
        let deltarune = running(GameKey::Steam(1671210), "DELTARUNE", 100);
        let minecraft = running(GameKey::Store("Microsoft.MinecraftUWP_8wekyb3d8bbwe".into()), "Minecraft.Windows", 200);
        assert_eq!(choose(&[], &gg, &[]), None);
        let one = choose(&[deltarune.clone()], &gg, &[]).unwrap();
        assert_eq!((one.preset.as_str(), one.preset_name.as_str(), one.game.as_str(), one.game_key.as_str(), one.by_user), ("gg:deltarune", "Deltarune", "DELTARUNE", "steam:1671210", false));
        // Two linked games: the one started last.
        assert_eq!(choose(&[deltarune.clone(), minecraft.clone()], &gg, &[]).unwrap().preset, "gg:minecraft");
        assert_eq!(choose(&[minecraft.clone(), deltarune.clone()], &gg, &[]).unwrap().preset, "gg:minecraft");
        // A game no preset names is no candidate.
        let other = running(GameKey::Steam(730), "Counter-Strike 2", 300);
        assert_eq!(choose(&[deltarune.clone(), other], &gg, &[]).unwrap().preset, "gg:deltarune");
        // Several presets name Fortnite: the first in GG's order.
        let fortnite = running(GameKey::Epic { catalog: "4fe75bbc".into(), app: "Fortnite".into(), main: String::new() }, "Fortnite", 50);
        assert_eq!(choose(&[fortnite], &gg, &[]).unwrap().preset, "gg:fortnite-pro");
        // A program the user linked wins over GG's links, even started earlier.
        let mine = [user("DELTARUNE.exe", "Mon Deltarune")];
        let program = running(GameKey::Exe("deltarune.exe".into()), "DELTARUNE", 90);
        let chosen = choose(&[deltarune, program, minecraft], &gg, &mine).unwrap();
        assert_eq!((chosen.preset.as_str(), chosen.game_key.as_str(), chosen.by_user), ("own:Mon Deltarune", "exe:deltarune.exe", true));
        // A program running without a link of the user's is no candidate.
        assert_eq!(choose(&[running(GameKey::Exe("notepad.exe".into()), "notepad", 999)], &gg, &mine), None);
    }

    fn saved(game_key: &str, preset: &str) -> Saved {
        Saved {
            game: "DELTARUNE".into(),
            game_key: game_key.into(),
            preset: preset.into(),
            preset_name: "Deltarune".into(),
            restore: "own:Soir".into(),
            restore_name: "Soir".into(),
            before: Some(flat(Role::Game)),
            since: "20261117-213000".into(),
        }
    }

    fn choice(game_key: &str, preset: &str) -> Choice {
        Choice { preset: preset.into(), preset_name: "P".into(), game: "G".into(), game_key: game_key.into(), by_user: false }
    }

    #[test]
    fn a_game_is_entered_followed_switched_and_left_after_two_looks_without_it() {
        let mut tracker = Tracker::default();
        let deltarune = choice("steam:1671210", "gg:deltarune");
        let under_way = saved("steam:1671210", "gg:deltarune");
        assert_eq!(tracker.decide(true, None, None, false), Step::Nothing);
        assert_eq!(tracker.decide(false, Some(&deltarune), None, false), Step::Nothing);
        assert_eq!(tracker.decide(true, Some(&deltarune), None, false), Step::Enter(deltarune.clone()));
        assert_eq!(tracker.decide(true, Some(&deltarune), Some(&under_way), false), Step::Nothing);
        // Gone for one look (a launcher restarting it): nothing yet.
        assert_eq!(tracker.decide(true, None, Some(&under_way), false), Step::Nothing);
        assert_eq!(tracker.decide(true, Some(&deltarune), Some(&under_way), false), Step::Nothing);
        assert_eq!(tracker.decide(true, None, Some(&under_way), false), Step::Nothing);
        assert_eq!(tracker.decide(true, None, Some(&under_way), false), Step::Leave);
        // Another game started later takes over at once; so does a new link of the same game.
        let minecraft = choice("store:minecraft", "gg:minecraft");
        assert_eq!(tracker.decide(true, Some(&minecraft), Some(&under_way), false), Step::Switch(minecraft));
        let relinked = choice("steam:1671210", "own:Mon Deltarune");
        assert_eq!(tracker.decide(true, Some(&relinked), Some(&under_way), false), Step::Switch(relinked));
        // Turned off: the preset of before comes back at once.
        assert_eq!(tracker.decide(false, Some(&deltarune), Some(&under_way), false), Step::Leave);
    }

    #[test]
    fn an_override_left_by_a_crash_goes_back_once_its_game_is_gone() {
        // At the next start the file is there and the game no longer runs: two looks, then back.
        let mut tracker = Tracker::default();
        let left = saved("steam:1671210", "gg:deltarune");
        assert_eq!(tracker.decide(true, None, Some(&left), false), Step::Nothing);
        assert_eq!(tracker.decide(true, None, Some(&left), false), Step::Leave);
        // Still running: followed on, nothing applied again.
        assert_eq!(tracker.decide(true, Some(&choice("steam:1671210", "gg:deltarune")), Some(&left), false), Step::Nothing);
    }

    #[test]
    fn a_manual_choice_ends_the_override_until_the_next_game_starts() {
        let mut tracker = Tracker::default();
        let deltarune = choice("steam:1671210", "gg:deltarune");
        let ended = saved("steam:1671210", "gg:deltarune");
        // While its game runs, the game is not put back over the user's choice.
        assert_eq!(tracker.decide(true, Some(&deltarune), Some(&ended), true), Step::Nothing);
        // Its game gone (two looks), the file goes without applying anything.
        assert_eq!(tracker.decide(true, None, Some(&ended), true), Step::Nothing);
        assert_eq!(tracker.decide(true, None, Some(&ended), true), Step::Drop);
        // Another game starting ends it at once, and is entered at the next look.
        let minecraft = choice("store:minecraft", "gg:minecraft");
        assert_eq!(tracker.decide(true, Some(&minecraft), Some(&ended), true), Step::Drop);
        assert_eq!(tracker.decide(true, Some(&minecraft), None, false), Step::Enter(minecraft));
        assert_eq!(tracker.decide(false, Some(&deltarune), Some(&ended), true), Step::Drop);
    }

    #[test]
    fn the_override_file_survives_a_round_trip() {
        let mut written = saved("steam:1671210", "gg:7d1c");
        assert_eq!(Saved::parse(&written.to_text()), Some(written.clone()));
        written.before = None;
        written.restore = "custom".into();
        assert_eq!(Saved::parse(&written.to_text()), Some(written.clone()));
        assert_eq!(written.id(), "20261117-213000 steam:1671210");
        assert_eq!(Saved::parse("game = X\n"), None);
        // Settings that cannot be read are lost, not the rest.
        let damaged = Saved::parse("game_key = steam:1\npreset = flat\nbefore = {\"generalGain\":\"loud\"}\n").unwrap();
        assert_eq!((damaged.before, damaged.preset.as_str()), (None, "flat"));
        let folder = std::env::temp_dir().join(format!("platinum-games-test-{}", std::process::id()));
        let file = folder.join(STATE_FILE);
        assert_eq!(load_from(&file), Ok(None));
        save_to(&file, &written).unwrap();
        assert_eq!(load_from(&file), Ok(Some(written)));
        std::fs::write(&file, "garbage").unwrap();
        assert!(load_from(&file).unwrap_err().contains("no game preset"));
        remove_at(&file).unwrap();
        remove_at(&file).unwrap();
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn the_users_links_survive_a_round_trip_and_follow_their_presets() {
        let mut links = vec![user("DELTARUNE.exe", "Soir"), user("hl2.exe", "Nuit")];
        links[0].title = "DELTARUNE | Chapitre 3".into();
        assert_eq!(parse_links(&links_to_text(&links)), links);
        // A program linked again keeps one link, the new one.
        set_link(&mut links, user("deltarune.EXE", "Jour"));
        assert_eq!(links.len(), 2);
        assert_eq!(links[1].preset, "own:Jour");
        assert!(relink(&mut links, "own:Jour", Some(("own:Matin", "Matin"))));
        assert_eq!((links[1].preset.as_str(), links[1].preset_name.as_str()), ("own:Matin", "Matin"));
        assert!(relink(&mut links, "own:Nuit", None) && links.len() == 1);
        assert!(remove_link(&mut links, "DELTARUNE.exe") && links.is_empty() && !remove_link(&mut links, "x.exe"));
        // Hand-written: a section without a preset, or before any section, is skipped.
        let text = "preset = flat\n[a.exe]\ntitle = A\n[b.exe]\npreset = gg:1\n[c.exe]\npreset = wobble\n";
        let read = parse_links(text);
        assert_eq!(read.len(), 1);
        assert_eq!((read[0].exe.as_str(), read[0].preset.as_str()), ("b.exe", "gg:1"));
    }

    #[test]
    fn a_steam_game_seen_twice_counts_once_with_its_earliest_start() {
        let game = |key: GameKey, started: u64| Running { key, name: "DELTARUNE".into(), started };
        // Steam's running application (seen by the daemon at 900) and the same game by its folder
        // (process created at 700), plus another game.
        let kept = once_each(vec![game(GameKey::Steam(1671210), 900), game(GameKey::Exe("other.exe".into()), 800), game(GameKey::Steam(1671210), 700)]);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept.iter().find(|g| g.key == GameKey::Steam(1671210)).map(|g| g.started), Some(700));
    }

    #[test]
    fn running_games_are_found_by_folder_package_and_program() {
        let process = |pid: u32, exe: &str, path: &str, family: &str, created: u64| Process { pid, exe: exe.into(), path: path.into(), family: family.into(), created };
        let processes = [
            process(10, "FortniteLauncher.exe", r"D:\Fortnite\FortniteGame\Binaries\Win64\FortniteLauncher.exe", "", 500),
            process(11, "FortniteClient-Win64-Shipping.exe", r"d:/fortnite/FortniteGame/Binaries/Win64/FortniteClient-Win64-Shipping.exe", "", 400),
            process(12, "Fortnite2.exe", r"D:\Fortnite2\x.exe", "", 300),
            process(20, "Minecraft.Windows.exe", r"C:\Program Files\WindowsApps\Microsoft.MinecraftUWP_1.21.0_x64__8wekyb3d8bbwe\Minecraft.Windows.exe", "Microsoft.MinecraftUWP_8wekyb3d8bbwe", 700),
            process(21, "SearchHost.exe", r"C:\Windows\SystemApps\x\SearchHost.exe", "MicrosoftWindows.Client.CBS_cw5n1h2txyewy", 10),
            process(30, "DELTARUNE.exe", r"C:\Games\DELTARUNE.exe", "", 900),
            process(31, "notepad.exe", "", "", 50),
        ];
        let fortnite = Installed { key: GameKey::Epic { catalog: "4fe7".into(), app: "Fortnite".into(), main: String::new() }, name: "Fortnite".into(), folder: r"D:\Fortnite\".into() };
        let steam = Installed { key: GameKey::Steam(1671210), name: "DELTARUNE".into(), folder: String::new() };
        let families = store_families(&library());
        assert_eq!(families, ["436609b6.fortniteclient_9ncxwbgmmv7m8", "microsoft.minecraftuwp_8wekyb3d8bbwe"]);
        let found = running_games(&processes, &[fortnite, steam], &families, &[user("deltarune.exe", "Soir")]);
        let summary: Vec<(String, u64)> = found.iter().map(|r| (r.key.text(), r.started)).collect();
        // Fortnite started with its first process; D:\Fortnite2 is another folder; the search
        // package is named by no link.
        assert_eq!(summary, [("epic:4fe7".to_string(), 400), ("store:microsoft.minecraftuwp_8wekyb3d8bbwe".to_string(), 700), ("exe:deltarune.exe".to_string(), 900)]);
        assert_eq!(found[1].name, "Minecraft.Windows");
        assert_eq!(found[2].name, "DELTARUNE");
        assert!(is_under(r"C:\A\b.exe", r"c:\a") && !is_under(r"C:\AB\b.exe", r"C:\A") && !is_under(r"C:\A", r"C:\A") && !is_under(r"C:\A\b.exe", ""));
    }

    #[test]
    fn launcher_records_are_read_as_their_launchers_write_them() {
        assert_eq!(family_name("Microsoft.MinecraftUWP_1.21.11201.0_x64__8wekyb3d8bbwe").as_deref(), Some("Microsoft.MinecraftUWP_8wekyb3d8bbwe"));
        assert_eq!(family_name("436609B6.FortniteClient_1.0.0.0_x64_~_9ncxwbgmmv7m8").as_deref(), Some("436609B6.FortniteClient_9ncxwbgmmv7m8"));
        assert_eq!(family_name("not_a_package"), None);
        let manifest = r#"{"FormatVersion":0,"DisplayName":"Fortnite","InstallLocation":"D:\\Fortnite","LaunchExecutable":"FortniteGame/Binaries/Win64/FortniteBootstrapper.exe","CatalogNamespace":"fn","CatalogItemId":"4fe75bbc5a674f4f9b356b5c90567da5","AppName":"Fortnite","MainGameAppName":""}"#;
        let game = parse_epic_manifest(manifest).unwrap();
        assert_eq!((game.name.as_str(), game.folder.as_str()), ("Fortnite", r"D:\Fortnite"));
        assert!(game.key.matches(&id("9_Fortnite")) && game.key.matches(&id("9_4fe75bbc5a674f4f9b356b5c90567da5")));
        assert_eq!(parse_epic_manifest(r#"{"DisplayName":"Nothing"}"#), None);
        assert_eq!(parse_epic_manifest("{"), None);
        // The window's "Par jeu": the presets naming a game installed here.
        let installed = [game, Installed { key: GameKey::Steam(1671210), name: "DELTARUNE".into(), folder: String::new() }];
        let labels = installed_links(&installed, &library());
        assert_eq!(labels, [
            ("gg:deltarune".to_string(), vec!["DELTARUNE (Steam)".to_string()]),
            ("gg:fortnite-pro".to_string(), vec!["Fortnite (Epic Games)".to_string()]),
            ("gg:fortnite-sky".to_string(), vec!["Fortnite (Epic Games)".to_string()]),
        ]);
        assert_eq!(program_name("DELTARUNE.EXE"), "DELTARUNE");
        assert_eq!(program_name("launcher"), "launcher");
    }
}
