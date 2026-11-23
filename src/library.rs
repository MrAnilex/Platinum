//! GG's preset library, read from GG's own database on this machine, or rebuilt from GG's
//! database migrations when GG is not installed.
//!
//! Platinum ships no vendor preset: when GG is installed, its presets are read at run time from
//! its SQLite database, read-only (`sqlite.rs`), and nothing is ever written back. The layout is in
//! `analysis/sonar/presets.md`, section 1: table `configs` (one row per preset, the preset itself
//! in the JSON column `data`, its factory version in `default_data`) and table `selected_config`
//! (the preset each channel uses in GG). Without GG, the same two tables come from replaying the
//! SQL migrations Platinum's setup took from GG's installer (`migrations.rs`), which is what GG
//! itself does on a fresh install.

use crate::json::Json;
use crate::migrations;
use crate::preset::{Preset, Role};
use crate::sqlite::{Database, RowRef};
use std::fmt;
use std::path::{Path, PathBuf};

/// Where the library was read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// GG's database.
    Database,
    /// GG's database migrations (no GG on this machine).
    Migrations,
}

/// The columns of a preset row, from GG's database or from the migrations.
trait Columns {
    fn text(&self, column: &str) -> Option<&str>;
    fn integer(&self, column: &str) -> Option<i64>;
}

impl Columns for RowRef<'_> {
    fn text(&self, column: &str) -> Option<&str> {
        RowRef::text(self, column)
    }

    fn integer(&self, column: &str) -> Option<i64> {
        RowRef::integer(self, column)
    }
}

impl Columns for migrations::Row {
    fn text(&self, column: &str) -> Option<&str> {
        migrations::Row::text(self, column)
    }

    fn integer(&self, column: &str) -> Option<i64> {
        migrations::Row::integer(self, column)
    }
}

/// Where GG keeps its Sonar database.
pub const DEFAULT_PATH: &str = r"C:\ProgramData\SteelSeries\GG\apps\sonar\db\database.db";

/// Why the library cannot be read.
#[derive(Clone, Debug, PartialEq)]
pub enum Error {
    /// No database at that path: GG is not installed, or not on this machine. The window shows
    /// its own French sentence for this case.
    Missing(PathBuf),
    /// The file is there but cannot be read as GG's database.
    Unreadable(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Missing(path) => write!(f, "GG's preset database is not at {} (GG does not seem to be installed)", path.display()),
            Error::Unreadable(message) => write!(f, "GG's preset database cannot be read: {message}"),
        }
    }
}

/// One preset of GG's library ("config" in GG's code).
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// GG's identifier, a GUID.
    pub id: String,
    pub name: String,
    pub role: Role,
    /// True for SteelSeries' own presets (GG's "SteelSeries" tab), false for the ones the user
    /// made in GG ("My presets").
    pub is_preset: bool,
    /// The preset as GG applies it, with the user's edits if any.
    pub data: Preset,
    /// The factory version, used by GG's "Reset".
    pub default_data: Preset,
    pub is_favorite: bool,
    /// Place among the channel's favorites, 0 to 8; -1 when not a favorite.
    pub favorite_position: i32,
    /// The GG version that brought the preset, while GG still shows it as "Nouveau".
    pub release_version: Option<String>,
    /// Games the preset is meant for, as GG's game detection names them ("1_1172470" is a Steam
    /// application id); only game presets have some.
    pub game_ids: Vec<String>,
    pub schema_version: i64,
}

impl Config {
    /// Whether GG marks it "Nouveau".
    pub fn is_new(&self) -> bool {
        self.release_version.is_some()
    }

    /// Whether the user changed it in GG after it was installed.
    pub fn is_edited(&self) -> bool {
        self.data != self.default_data
    }
}

/// The whole library.
#[derive(Clone, Debug, PartialEq)]
pub struct Library {
    pub path: PathBuf,
    pub configs: Vec<Config>,
    /// The preset each channel uses in GG, by id.
    pub selected: Vec<(Role, String)>,
    /// Rows that could not be read, with the reason (none on GG 123).
    pub skipped: Vec<String>,
    /// The database's change counter when read: a later read with the same counter is the same.
    pub change_counter: u32,
    pub origin: Origin,
}

/// What `Library::open_default` reads: GG's database when it exists, else the first folder of
/// migrations found (Platinum's copy, then GG's own), else `None`.
pub fn default_source() -> Option<(PathBuf, Origin)> {
    let database = PathBuf::from(DEFAULT_PATH);
    if database.is_file() {
        return Some((database, Origin::Database));
    }
    crate::sonarfiles::migrations_folders().into_iter().find(|folder| folder.is_dir()).map(|folder| (folder, Origin::Migrations))
}

/// The file or folder whose date tells when the library changed: the source `open_default` reads,
/// or GG's database path while there is none.
pub fn source_path() -> PathBuf {
    default_source().map(|(path, _)| path).unwrap_or_else(|| PathBuf::from(DEFAULT_PATH))
}

impl Library {
    /// Reads GG's database at its usual place, or without one, rebuilds the library from GG's
    /// migrations (`default_source`).
    pub fn open_default() -> Result<Library, Error> {
        match default_source() {
            Some((path, Origin::Database)) => Library::open(&path),
            Some((folder, Origin::Migrations)) => Library::from_migrations(&folder),
            None => Err(Error::Missing(PathBuf::from(DEFAULT_PATH))),
        }
    }

    /// Rebuilds the library from a folder of GG's database migrations. Statements the replay could
    /// not follow are listed in `skipped`, as rows that cannot be read are.
    pub fn from_migrations(folder: &Path) -> Result<Library, Error> {
        let files = migrations::read_folder(folder).map_err(Error::Unreadable)?;
        Ok(Library::from_replay(&migrations::replay(&files), folder))
    }

    /// The library left by a replay of the migrations.
    pub fn from_replay(replay: &migrations::Replay, path: &Path) -> Library {
        let mut configs = Vec::new();
        let mut skipped: Vec<String> = replay.ignored.iter().map(|why| format!("migration statement not followed: {why}")).collect();
        for row in &replay.configs {
            // GG's service reads presets with a lenient parser, and the first migrations wrote
            // relaxed JSON (bare keys): read them the same way.
            let mut row = row.clone();
            let role = row.integer("vad").and_then(Role::from_vad);
            for column in ["data", "default_data"] {
                if let Some(text) = row.text(column).map(migrations::strict_json) {
                    let text = role.map_or(text.clone(), |role| upgrade_legacy_equalizer(&text, role));
                    row.0.insert(column.into(), migrations::Value::Text(text));
                }
            }
            let row = &row;
            match read_config(row) {
                Ok(mut config) => {
                    if !keeps_new_tag(config.release_version.as_deref(), replay.sonar_version) {
                        config.release_version = None;
                    }
                    configs.push(upgrade_schema(config));
                }
                Err(why) => skipped.push(format!("{} ({}): {why}", row.text("name").unwrap_or("?"), row.text("id").unwrap_or("?"))),
            }
        }
        let selected = replay.selected.iter().filter_map(|(vad, id)| Some((Role::from_vad(*vad)?, id.clone()))).collect();
        Library { path: path.to_path_buf(), configs, selected, skipped, change_counter: 0, origin: Origin::Migrations }
    }

    pub fn open(path: &Path) -> Result<Library, Error> {
        if !path.is_file() {
            return Err(Error::Missing(path.to_path_buf()));
        }
        let database = Database::open(path).map_err(Error::Unreadable)?;
        Library::from_database(&database, path)
    }

    /// Reads the library out of a database already in memory.
    pub fn from_database(database: &Database, path: &Path) -> Result<Library, Error> {
        let configs_table = database.table("configs").map_err(Error::Unreadable)?;
        let mut configs = Vec::new();
        let mut skipped = Vec::new();
        for row in configs_table.rows() {
            match read_config(&row) {
                Ok(config) => configs.push(config),
                Err(why) => skipped.push(format!("{} ({}): {why}", row.text("name").unwrap_or("?"), row.text("id").unwrap_or("?"))),
            }
        }
        let mut selected = Vec::new();
        for row in database.table("selected_config").map_err(Error::Unreadable)?.rows() {
            if let (Some(role), Some(id)) = (row.integer("vad").and_then(Role::from_vad), row.text("config_id")) {
                selected.push((role, id.to_string()));
            }
        }
        Ok(Library { path: path.to_path_buf(), configs, selected, skipped, change_counter: database.change_counter(), origin: Origin::Database })
    }

    /// The presets of one channel, in the order of GG's browser: by name, the user's own first,
    /// and for Game the music and movie presets after the game ones.
    pub fn configs(&self, role: Role) -> Vec<&Config> {
        let mut list: Vec<&Config> = self.configs.iter().filter(|c| c.role == role).collect();
        list.sort_by_key(|c| c.name.to_uppercase());
        list.sort_by_key(|c| c.is_preset);
        if role == Role::Game {
            list.sort_by_key(|c| c.is_preset && is_music_or_movie(&c.name));
        }
        list
    }

    /// The preset GG uses for a channel.
    pub fn selected(&self, role: Role) -> Option<&Config> {
        let (_, id) = self.selected.iter().find(|(r, _)| *r == role)?;
        self.by_id(id)
    }

    pub fn by_id(&self, id: &str) -> Option<&Config> {
        self.configs.iter().find(|c| c.id.eq_ignore_ascii_case(id))
    }

    /// A channel's preset by its exact name, ignoring case (GG's names are unique per channel).
    pub fn find(&self, role: Role, name: &str) -> Option<&Config> {
        let wanted = name.to_lowercase();
        self.configs.iter().find(|c| c.role == role && c.name.to_lowercase() == wanted)
    }

    /// GG's search: the presets whose name contains `query`, ignoring case, in browser order.
    pub fn search(&self, role: Role, query: &str) -> Vec<&Config> {
        let query = query.to_lowercase();
        self.configs(role).into_iter().filter(|c| c.name.to_lowercase().contains(&query)).collect()
    }

    /// A channel's favorites by their position (GG allows nine).
    pub fn favorites(&self, role: Role) -> Vec<&Config> {
        let mut list: Vec<&Config> = self.configs.iter().filter(|c| c.role == role && c.is_favorite).collect();
        list.sort_by_key(|c| c.favorite_position);
        list
    }
}

/// GG sorts the game channel's "Music: …" and "Movie: …" presets after the game titles.
fn is_music_or_movie(name: &str) -> bool {
    let name = name.to_lowercase();
    name.contains("music") || name.contains("movie")
}

/// GG's "New" tag rule (`ConfigsService.CleanOldConfigReleaseVersion`, run at every start of the
/// service): a preset keeps its `release_version` only when it has the major version of the Sonar
/// it runs with and one of its last two minor versions; anything unreadable loses it.
pub fn keeps_new_tag(release: Option<&str>, sonar: Option<(u64, u64, u64)>) -> bool {
    let (Some(release), Some((major, minor, _))) = (release, sonar) else { return false };
    let mut parts = release.trim().split('.').map(|p| p.parse::<u64>().ok());
    match (parts.next().flatten(), parts.next().flatten()) {
        (Some(m), Some(n)) => m == major && n <= minor && n + 2 > minor,
        _ => false,
    }
}

/// The schema of microphone presets GG 123 works with; game, chat, media and aux are at 5.
const CAPTURE_SCHEMA: i64 = 6;

/// What GG's service does to an older preset when it starts (`ConfigsService.UpgradeConfigs`),
/// for presets rebuilt from the migrations, which never went through it. Microphone presets
/// older than schema 6 come out with ClearCast AI noise cancelling on, in the preset and in its
/// factory version: the 8 such presets of GG 123's migrations differ from GG's database by exactly
/// that and nothing else (found by comparing them, not read in GG's code). Other channels' older
/// presets already match GG's database as they are.
fn upgrade_schema(mut config: Config) -> Config {
    if config.role != Role::ChatCapture || config.schema_version >= CAPTURE_SCHEMA {
        return config;
    }
    for preset in [&mut config.data, &mut config.default_data] {
        if let Preset::Capture(capture) = preset {
            capture.noise_canceling.enabled = true;
        }
    }
    config.schema_version = CAPTURE_SCHEMA;
    config
}

/// The legacy equalizer's bands (`LegacyAudioSettings.Equalizer`), in order.
const LEGACY_BANDS: [&str; 10] = ["31Hz", "62Hz", "125Hz", "250Hz", "500Hz", "1000Hz", "2000Hz", "4000Hz", "8000Hz", "16000Hz"];

/// GG's upgrade of a chat or microphone preset written with the first schema's fixed-band
/// equalizer (`"equalizer": {"enabled", "31Hz" … "16000Hz"}`): the same gains on a ten-band octave
/// parametric equalizer with shelves at both ends, which is exactly the layout of Platinum's
/// neutral capture preset. Found by comparing the 8 such presets of GG 123's migrations with GG's
/// database (equal after this), not read in GG's code. Other texts come back unchanged.
fn upgrade_legacy_equalizer(text: &str, role: Role) -> String {
    let Ok(Json::Object(mut members)) = Json::parse(text) else { return text.to_string() };
    if !role.is_capture() || members.iter().any(|(key, _)| key == "parametricEQ") {
        return text.to_string();
    }
    let Some(at) = members.iter().position(|(key, _)| key == "equalizer") else { return text.to_string() };
    let legacy = members.remove(at).1;
    let Some(Json::Object(mut equalizer)) = crate::preset::flat(role).to_json().get("parametricEQ").cloned() else { return text.to_string() };
    for (key, value) in equalizer.iter_mut() {
        if key == "enabled" {
            *value = Json::Bool(legacy.get("enabled").and_then(Json::as_bool).unwrap_or(true));
        } else if let Some(band) = key.strip_prefix("filter").and_then(|n| n.parse::<usize>().ok()).and_then(|n| LEGACY_BANDS.get(n.wrapping_sub(1))) {
            let gain = legacy.get(band).and_then(Json::as_f64).unwrap_or(0.0);
            if let Json::Object(filter) = value {
                for (name, field) in filter.iter_mut() {
                    if name == "gain" {
                        *field = Json::Number(gain);
                    }
                }
            }
        }
    }
    members.push(("parametricEQ".into(), Json::Object(equalizer)));
    Json::Object(members).to_compact()
}

fn read_config(row: &impl Columns) -> Result<Config, String> {
    let vad = row.integer("vad").ok_or("no channel number")?;
    let role = Role::from_vad(vad).ok_or_else(|| format!("unknown channel number {vad}"))?;
    let preset = |column: &str| -> Result<Preset, String> {
        let text = row.text(column).ok_or_else(|| format!("no {column}"))?;
        Preset::from_json(&Json::parse(text)?, role).map_err(|e| format!("{column}: {e}"))
    };
    let data = preset("data")?;
    // An empty factory copy would make every preset look edited: fall back to the preset itself.
    // `{}` is the column's default in GG's migrations, for presets that never got one.
    let default_data = match row.text("default_data") {
        Some(text) if !text.trim().is_empty() && text.trim() != "{}" => preset("default_data")?,
        _ => data,
    };
    Ok(Config {
        id: row.text("id").ok_or("no id")?.to_string(),
        name: row.text("name").ok_or("no name")?.to_string(),
        role,
        is_preset: row.integer("is_preset").unwrap_or(0) != 0,
        data,
        default_data,
        is_favorite: row.integer("is_favorite").unwrap_or(0) != 0,
        favorite_position: row.integer("favorite_position").and_then(|p| i32::try_from(p).ok()).unwrap_or(-1),
        release_version: row.text("release_version").map(str::to_string).filter(|v| !v.is_empty()),
        game_ids: row.text("game_ids").unwrap_or("").split(',').map(str::trim).filter(|g| !g.is_empty()).map(str::to_string).collect(),
        schema_version: row.integer("schema_version").unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preset::flat;

    fn config(name: &str, role: Role, is_preset: bool) -> Config {
        Config {
            id: format!("id-{name}"),
            name: name.into(),
            role,
            is_preset,
            data: flat(role),
            default_data: flat(role),
            is_favorite: false,
            favorite_position: -1,
            release_version: None,
            game_ids: Vec::new(),
            schema_version: 5,
        }
    }

    fn library(configs: Vec<Config>) -> Library {
        Library { path: PathBuf::new(), configs, selected: vec![(Role::Game, "id-beta".into())], skipped: Vec::new(), change_counter: 0, origin: Origin::Database }
    }

    /// GG's migrations, synthetic: a table, two factory presets, a user preset, renames, game
    /// links, a factory refresh and the seeded selection.
    #[test]
    fn the_library_rebuilds_from_migrations() {
        let game = flat(Role::Game).to_json().to_compact();
        let louder = match flat(Role::Game) {
            Preset::Render(render) => Preset::Render(crate::preset::Render { general_gain: 3.0, ..render }),
            other => other,
        };
        let louder_json = louder.to_json().to_compact();
        let mic = flat(Role::ChatCapture).to_json().to_compact();
        let files = vec![
            ("sonar-0.2.0\\20210922091324_squash.sql".to_string(), format!("-- +goose Up\nCREATE TABLE IF NOT EXISTS configs (id TEXT NOT NULL UNIQUE, name TEXT NOT NULL, vad INTEGER NOT NULL, data TEXT NOT NULL, schema_version INTEGER NOT NULL, created_at DATETIME NOT NULL, updated_at DATETIME NOT NULL, PRIMARY KEY(id));\nINSERT INTO configs (id,name,vad,data,schema_version,created_at,updated_at) VALUES ('flat-game','Flat',1,'{game}',5,DATETIME('now'),DATETIME('now')), ('user-game','Flat',1,'{game}',5,DATETIME('now'),DATETIME('now'));\n-- +goose Down\nDROP TABLE configs;\n")),
            ("sonar-1.4.0\\20220601000000_squash.sql".to_string(), format!("-- +goose Up\nALTER TABLE configs ADD is_preset INTEGER DEFAULT 0;\nALTER TABLE configs ADD default_data DEFAULT \"{{}}\";\nALTER TABLE configs ADD game_ids TEXT NOT NULL DEFAULT '';\nALTER TABLE configs ADD release_version TEXT DEFAULT NULL;\nUPDATE configs SET is_preset = 1, default_data = data WHERE id = 'flat-game';\nUPDATE configs SET name = 'Custom' WHERE id='user-game' and name='Flat';\nINSERT INTO configs (id,name,vad,data,schema_version,created_at,updated_at,is_preset,default_data,default_data_last_update, image, game_ids, release_version) VALUES ('deltarune','Deltarune',1,'{game}',5,DATETIME('now'),DATETIME('now'),1,'{game}',DATETIME('now'),'sonar.svg','1_1671210','1.74.0'), ('mic','Broadcast',3,'{mic}',6,DATETIME('now'),DATETIME('now'),1,'{mic}',DATETIME('now'),'sonar.svg','',NULL);\nINSERT INTO selected_config (vad, config_id) VALUES (1,'user-game');\nUPDATE configs SET game_ids = '1_1671210, 7_x' WHERE configs.id = 'deltarune';\n-- +goose Down\n")),
            ("sonar-1.75.0\\20250724112952_squash.sql".to_string(), format!("-- +goose Up\nWITH const AS ( SELECT 'deltarune' AS id, '{louder_json}' AS default_data ), compare_user_default_data AS ( SELECT valid_configs.id, CASE WHEN SUM(json_extract(data, default_data_json.fullkey) != value) = 0 THEN 1 ELSE 0 END AS IS_EQUAL FROM ( SELECT id, data FROM configs WHERE json_valid(data) = 1 ) AS valid_configs JOIN json_tree(default_data) AS default_data_json WHERE valid_configs.id = (SELECT id FROM const) GROUP BY valid_configs.id ) UPDATE configs SET default_data = (SELECT default_data from const), data = CASE WHEN (SELECT IS_EQUAL from compare_user_default_data) = 1 THEN (SELECT default_data from const) ELSE configs.data END WHERE configs.id = (SELECT id FROM const);\n")),
        ];
        let library = Library::from_replay(&migrations::replay(&files), Path::new("migrations"));
        assert!(library.skipped.is_empty(), "{:?}", library.skipped);
        assert_eq!(library.origin, Origin::Migrations);
        let names = |role| library.configs(role).iter().map(|c| c.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(Role::Game), ["Custom", "Deltarune", "Flat"]);
        assert_eq!(names(Role::ChatCapture), ["Broadcast"]);
        let deltarune = library.find(Role::Game, "deltarune").unwrap();
        assert!(deltarune.is_preset && deltarune.is_new());
        assert_eq!(deltarune.game_ids, ["1_1671210", "7_x"]);
        assert_eq!(deltarune.data, louder);
        assert!(!deltarune.is_edited());
        let custom = library.find(Role::Game, "Custom").unwrap();
        // `{}`, the column's default, is no factory version: the preset stands for itself.
        assert!(!custom.is_preset && !custom.is_edited());
        assert_eq!(library.selected(Role::Game).map(|c| c.name.as_str()), Some("Custom"));
        assert_eq!(library.favorites(Role::Game).len(), 0);
    }

    #[test]
    fn the_browser_order_is_ggs() {
        let lib = library(vec![
            config("zulu", Role::Game, true),
            config("Music: Rock", Role::Game, true),
            config("alpha", Role::Game, true),
            config("Mine", Role::Game, false),
            config("Movie: Night", Role::Game, true),
            config("beta", Role::Game, true),
            config("Music: Jazz", Role::Media, true),
            config("Flat", Role::Media, true),
        ]);
        let names = |role| lib.configs(role).iter().map(|c| c.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(Role::Game), ["Mine", "alpha", "beta", "zulu", "Movie: Night", "Music: Rock"]);
        // Only the game channel moves music and movie presets to the end.
        assert_eq!(names(Role::Media), ["Flat", "Music: Jazz"]);
        assert_eq!(lib.selected(Role::Game).map(|c| c.name.as_str()), Some("beta"));
        assert_eq!(lib.selected(Role::Aux), None);
        assert_eq!(lib.find(Role::Game, "ZULU").map(|c| c.id.as_str()), Some("id-zulu"));
        assert_eq!(lib.find(Role::Media, "zulu"), None);
        assert_eq!(lib.search(Role::Game, "MU").len(), 1);
    }

    #[test]
    fn favorites_come_by_position() {
        let mut a = config("a", Role::ChatCapture, true);
        let mut b = config("b", Role::ChatCapture, true);
        (a.is_favorite, a.favorite_position) = (true, 1);
        (b.is_favorite, b.favorite_position) = (true, 0);
        let lib = library(vec![a, b, config("c", Role::ChatCapture, true)]);
        let names: Vec<&str> = lib.favorites(Role::ChatCapture).iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["b", "a"]);
    }

    #[test]
    fn a_missing_database_says_so() {
        let error = Library::open(Path::new(r"C:\no\such\folder\database.db")).unwrap_err();
        assert!(matches!(error, Error::Missing(_)));
        assert!(error.to_string().contains("not at"));
    }

    #[test]
    fn only_the_last_two_versions_stay_new() {
        let sonar = Some((1, 106, 0));
        assert!(keeps_new_tag(Some("1.106.0"), sonar));
        assert!(keeps_new_tag(Some("1.105.0"), sonar));
        assert!(!keeps_new_tag(Some("1.104.0"), sonar));
        assert!(!keeps_new_tag(Some("1.107.0"), sonar));
        assert!(!keeps_new_tag(Some("2.106.0"), sonar));
        assert!(!keeps_new_tag(Some("new"), sonar));
        assert!(!keeps_new_tag(None, sonar));
        assert!(!keeps_new_tag(Some("1.106.0"), None));
    }

    #[test]
    fn first_schema_presets_are_upgraded_as_gg_does() {
        let legacy = r#"{"globalEnableState":true,"noiseGateState":{"enabled":false,"value":-60},"equalizer":{"enabled":true,"31Hz":-6,"62Hz":1,"125Hz":2,"250Hz":0,"500Hz":0,"1000Hz":-1.5,"2000Hz":0,"4000Hz":0,"8000Hz":0,"16000Hz":5}}"#;
        let upgraded = upgrade_legacy_equalizer(legacy, Role::ChatCapture);
        let json = Json::parse(&upgraded).unwrap();
        assert!(json.get("equalizer").is_none());
        let gain = |n: usize| json.get("parametricEQ").and_then(|eq| eq.get(&format!("filter{n}"))).and_then(|f| f.get("gain")).and_then(Json::as_f64);
        assert_eq!((gain(1), gain(2), gain(6), gain(10)), (Some(-6.0), Some(1.0), Some(-1.5), Some(5.0)));
        // A playback channel, or a preset already in the current schema, is left alone.
        assert_eq!(upgrade_legacy_equalizer(legacy, Role::Game), legacy);
        let current = flat(Role::ChatCapture).to_json().to_compact();
        assert_eq!(upgrade_legacy_equalizer(&current, Role::ChatCapture), current);
        let mut config = config("old", Role::ChatCapture, true);
        config.schema_version = 1;
        if let Preset::Capture(capture) = &mut config.data {
            capture.noise_canceling.enabled = false;
        }
        let config = upgrade_schema(config);
        assert_eq!(config.schema_version, CAPTURE_SCHEMA);
        assert!(matches!(config.data, Preset::Capture(c) if c.noise_canceling.enabled));
    }

    /// When GG is installed here, its migrations replayed must give the factory presets of its
    /// database: same names, channels, games and factory versions. Skipped without GG.
    #[test]
    fn migrations_match_ggs_database_when_present() {
        let folder = crate::sonarfiles::gg_folder().join(crate::sonarfiles::MIGRATIONS);
        let (Ok(database), true) = (Library::open(Path::new(DEFAULT_PATH)), folder.is_dir()) else { return };
        let replayed = Library::from_migrations(&folder).unwrap();
        assert!(replayed.skipped.is_empty(), "{:#?}", replayed.skipped);
        let mut differences = Vec::new();
        let mut compared = 0;
        for config in database.configs.iter().filter(|c| c.is_preset) {
            compared += 1;
            let Some(rebuilt) = replayed.by_id(&config.id) else {
                differences.push(format!("{} ({}): missing from the replay", config.name, config.id));
                continue;
            };
            if (rebuilt.name.as_str(), rebuilt.role, &rebuilt.game_ids, rebuilt.default_data) != (config.name.as_str(), config.role, &config.game_ids, config.default_data) {
                differences.push(format!("{} ({}): differs", config.name, config.id));
            }
            // GG's data is the user's: equal to the replay unless the user edited the preset.
            if !config.is_edited() && rebuilt.data != config.data {
                differences.push(format!("{} ({}): data differs", config.name, config.id));
            }
        }
        let extra: Vec<&str> = replayed.configs.iter().filter(|c| c.is_preset && database.by_id(&c.id).is_none()).map(|c| c.name.as_str()).collect();
        eprintln!("{compared} factory presets compared, {} differences, {} only in the replay: {extra:?}", differences.len(), extra.len());
        assert!(differences.is_empty(), "{differences:#?}");
        assert!(extra.is_empty());
        for role in Role::ALL {
            assert!(replayed.selected(role).is_some(), "no selection for {role:?}");
        }
    }

    /// Reads GG's own library when it is installed: `cargo test --release -- --ignored`.
    #[test]
    #[ignore]
    fn reads_ggs_library_when_present() {
        let library = match Library::open_default() {
            Ok(library) => library,
            Err(Error::Missing(_)) => return,
            Err(error) => panic!("{error}"),
        };
        for role in Role::ALL {
            let configs = library.configs(role);
            eprintln!(
                "{:<12} {:>3} presets ({} library, {} user, {} edited, {} new, {} with games, {} favorites), selected: {}",
                role.api_name(),
                configs.len(),
                configs.iter().filter(|c| c.is_preset).count(),
                configs.iter().filter(|c| !c.is_preset).count(),
                configs.iter().filter(|c| c.is_edited()).count(),
                configs.iter().filter(|c| c.is_new()).count(),
                configs.iter().filter(|c| !c.game_ids.is_empty()).count(),
                library.favorites(role).len(),
                library.selected(role).map_or("(none)", |c| c.name.as_str())
            );
            assert!(configs.iter().all(|c| c.data.fits(role)));
        }
        eprintln!("skipped rows: {:?}", library.skipped);
        assert!(library.skipped.is_empty());
    }
}
