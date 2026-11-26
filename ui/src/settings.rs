//! The settings page ("Paramètres"): every option of Platinum in one place, by section, instead of
//! switches scattered around the window.
//!
//! The options stay where they always were: platinum.ini (`platinum::config`, rewritten whole by
//! the engine, comment header included), the Run value (`platinum::autostart`), the channels' names
//! (`platinum::names`), game-links.txt (`platinum::games`). This module holds what the page works
//! out without Windows: when each option takes effect (`Setting::applies`), the changes it makes
//! to platinum.ini (`Change`, applied the same way to the window's copy and to the file), which of
//! them wait for the daemon's next start (`pending`), the fallback devices' menus, and the texts of
//! the "À propos" section.
//!
//! When an option takes effect was read in the daemon: its mixer thread reads platinum.ini on
//! every 2 s tick (backup switch and lists, mode, remote streaming, preset per game, Arena 7 rule)
//! and game-links.txt when it changes; its shortcuts thread reads the file again when it changes;
//! the microphone, its effects, its volume and the fallback devices are read once, when the daemon
//! starts. The window itself reads `start_on_open` when it opens, Windows reads the Run value at
//! logon, and the channels' names are written at once (and `channel_names` saved, which the daemon
//! applies at its next start).
//!
//! The "Rapports de plantage" section lists the crash reports of `platinum::crash`; this module
//! words them (date, program, reason) and says where an issue would go.

use platinum::config::Config;
use platinum::crash::{self, Consent, Summary};
use platinum::devices::{self, Endpoint, Flow, State};
use platinum::games::{self, UserLink};
use platinum::i18n::{self, Choice, Language, mark, tr};
use platinum::trf;
use platinum::status::{State as DaemonState, Status};
use std::path::{Path, PathBuf};

/// The settings page's number among the window's pages (0 the mixer, 1 to 5 the channel tabs).
pub const PAGE: i32 = 6;

/// The sections Rust opens by number (ui/backend.slint's `Section` has them all, in the
/// navigation's order), and how many there are.
pub const OUTPUTS: i32 = 2;
pub const SHORTCUTS: i32 = 6;
pub const CRASHES: i32 = 7;
pub const SECTIONS: i32 = 9;

/// The options whose note says when they take effect, in ui/backend.slint's `SettingId` order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setting {
    StartWithWindows,
    StartOnOpen,
    ChannelNames,
    Microphone,
    Effects,
    Gain,
    FallbackInput,
    AutoSwitch,
    FallbackOutput,
    Streamer,
    FollowStreaming,
    RouteApps,
    StreamingSinks,
    GamePresets,
    ArenaSpeakers,
    Shortcuts,
    Language,
    CrashReports,
}

/// When a change of an option takes effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applies {
    /// Written where it acts (the devices' names), or read again by the daemon within 2 s.
    Now,
    /// Read by the daemon when it starts: the running daemon keeps the value it started with.
    NextStart,
    /// Read by Windows when the user's session opens (the Run value).
    NextLogon,
    /// Read by the window when it opens.
    NextWindow,
    /// Changing it restarts the daemon, once the user confirmed it (the microphone).
    Restart,
    /// Used when a preset is applied or an output changes (GG's Arena 7 rule).
    NextPreset,
    /// Applied by the window at once, and by the daemon within 2 s (it reads platinum.ini on every
    /// tick): the language.
    WindowNowDaemonSoon,
}

impl Applies {
    /// The note under the option, in the current language.
    pub fn note(self) -> &'static str {
        match self {
            Applies::Now => tr("S'applique tout de suite."),
            Applies::NextStart => tr("S'applique au prochain démarrage de Platinum."),
            Applies::NextLogon => tr("S'applique à la prochaine ouverture de session Windows."),
            Applies::NextWindow => tr("S'applique à la prochaine ouverture de cette fenêtre."),
            Applies::Restart => tr("Si Platinum relaie déjà un micro, il redémarre pour ouvrir le nouveau, après confirmation : le son et le micro sont coupés quelques secondes."),
            Applies::NextPreset => tr("S'applique au prochain préréglage appliqué ou changement de sortie."),
            Applies::WindowNowDaemonSoon => tr("S'applique tout de suite à cette fenêtre. L'icône près de l'horloge et les superpositions suivent dans les 2 secondes quand Platinum tourne (ou à son prochain démarrage)."),
        }
    }
}

impl Setting {
    pub const ALL: [Setting; 18] = [
        Setting::StartWithWindows,
        Setting::StartOnOpen,
        Setting::ChannelNames,
        Setting::Microphone,
        Setting::Effects,
        Setting::Gain,
        Setting::FallbackInput,
        Setting::AutoSwitch,
        Setting::FallbackOutput,
        Setting::Streamer,
        Setting::FollowStreaming,
        Setting::RouteApps,
        Setting::StreamingSinks,
        Setting::GamePresets,
        Setting::ArenaSpeakers,
        Setting::Shortcuts,
        Setting::Language,
        Setting::CrashReports,
    ];

    /// The option with this number in `SettingId`.
    pub fn from_index(index: i32) -> Option<Setting> {
        usize::try_from(index).ok().and_then(|i| Setting::ALL.get(i)).copied()
    }

    /// When a change takes effect, as the daemon and the window read the option.
    pub fn applies(self) -> Applies {
        match self {
            Setting::StartWithWindows => Applies::NextLogon,
            Setting::StartOnOpen => Applies::NextWindow,
            Setting::Microphone => Applies::Restart,
            Setting::Effects | Setting::Gain | Setting::FallbackInput | Setting::FallbackOutput => Applies::NextStart,
            Setting::ArenaSpeakers => Applies::NextPreset,
            Setting::Language => Applies::WindowNowDaemonSoon,
            Setting::ChannelNames | Setting::AutoSwitch | Setting::Streamer | Setting::FollowStreaming | Setting::RouteApps | Setting::StreamingSinks | Setting::GamePresets | Setting::Shortcuts | Setting::CrashReports => Applies::Now,
        }
    }

    /// How the page names it (in the restart notice, lower-cased), in the current language.
    pub fn label(self) -> &'static str {
        match self {
            Setting::StartWithWindows => tr("Démarrer avec Windows"),
            Setting::StartOnOpen => tr("Démarrer à l'ouverture de la fenêtre"),
            Setting::ChannelNames => tr("Noms des canaux"),
            Setting::Microphone => tr("Micro relayé"),
            Setting::Effects => tr("Effets du micro"),
            Setting::Gain => tr("Volume du micro"),
            Setting::FallbackInput => tr("Entrée remise à l'arrêt"),
            Setting::AutoSwitch => tr("Bascule de secours"),
            Setting::FallbackOutput => tr("Sortie remise à l'arrêt"),
            Setting::Streamer => tr("Mode streamer"),
            Setting::FollowStreaming => tr("Suivre le streaming à distance"),
            Setting::RouteApps => tr("Applications par Platinum"),
            Setting::StreamingSinks => tr("Sorties de streaming reconnues"),
            Setting::GamePresets => tr("Préréglage par jeu"),
            Setting::ArenaSpeakers => tr("Règle Arena 7"),
            Setting::Shortcuts => tr("Raccourcis et superpositions"),
            Setting::Language => tr("Langue"),
            Setting::CrashReports => tr("Proposer d'envoyer les rapports de plantage"),
        }
    }
}

/// Every option's note, in `Setting::ALL` order, for the page.
pub fn notes() -> Vec<&'static str> {
    Setting::ALL.iter().map(|setting| setting.applies().note()).collect()
}

/// A change of platinum.ini made on the page. The window applies it to its copy (shown at once)
/// and the engine to the file, with the same function, as the effect cards do with presets.
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    StartOnOpen(bool),
    Effects(bool),
    /// The relayed microphone's linear volume, 0 to 1 (kept to the percent shown).
    Gain(f32),
    /// The output made the default when the daemon stops, as platinum.ini names it (`None`: the
    /// one found when it started).
    FallbackOutput(Option<String>),
    FallbackInput(Option<String>),
    FollowStreaming(bool),
    RouteApps(bool),
    /// The streaming outputs' name fragments, as typed (comma-separated).
    StreamingSinks(String),
    GamePresets(bool),
    ArenaSpeakers(bool),
    Language(Choice),
    CrashReports(Consent),
}

impl Change {
    /// The on/off option of the page with this `Setting`; `None` for the others (the Run value,
    /// the names, the backup switch and streamer mode have their own paths).
    pub fn switch(setting: Setting, on: bool) -> Option<Change> {
        Some(match setting {
            Setting::StartOnOpen => Change::StartOnOpen(on),
            Setting::Effects => Change::Effects(on),
            Setting::FollowStreaming => Change::FollowStreaming(on),
            Setting::RouteApps => Change::RouteApps(on),
            Setting::GamePresets => Change::GamePresets(on),
            Setting::ArenaSpeakers => Change::ArenaSpeakers(on),
            Setting::CrashReports => Change::CrashReports(if on { Consent::Ask } else { Consent::Off }),
            _ => return None,
        })
    }

    pub fn setting(&self) -> Setting {
        match self {
            Change::StartOnOpen(_) => Setting::StartOnOpen,
            Change::Effects(_) => Setting::Effects,
            Change::Gain(_) => Setting::Gain,
            Change::FallbackOutput(_) => Setting::FallbackOutput,
            Change::FallbackInput(_) => Setting::FallbackInput,
            Change::FollowStreaming(_) => Setting::FollowStreaming,
            Change::RouteApps(_) => Setting::RouteApps,
            Change::StreamingSinks(_) => Setting::StreamingSinks,
            Change::GamePresets(_) => Setting::GamePresets,
            Change::ArenaSpeakers(_) => Setting::ArenaSpeakers,
            Change::Language(_) => Setting::Language,
            Change::CrashReports(_) => Setting::CrashReports,
        }
    }

    /// Puts the change in a configuration. The streaming outputs are read as the file reads them.
    pub fn apply(&self, config: &mut Config) {
        match self {
            Change::StartOnOpen(on) => config.start_on_open = *on,
            Change::Effects(on) => config.effects = *on,
            Change::Gain(gain) => config.gain = percent(*gain),
            Change::FallbackOutput(device) => config.fallback_output = device.clone().filter(|d| !d.trim().is_empty()),
            Change::FallbackInput(device) => config.fallback_input = device.clone().filter(|d| !d.trim().is_empty()),
            Change::FollowStreaming(on) => config.follow_streaming = *on,
            Change::RouteApps(on) => config.streaming_route_apps = *on,
            // A list of names: the file's own reading never refuses one.
            Change::StreamingSinks(text) => {
                let _ = config.set("streaming_sinks", text);
            }
            Change::GamePresets(on) => config.game_presets = *on,
            Change::ArenaSpeakers(on) => config.arena_speakers = *on,
            Change::Language(choice) => config.language = *choice,
            Change::CrashReports(consent) => config.crash_reports = *consent,
        }
    }

    /// An option an old logon command may still carry as an argument (`--no-effects`, `--gain`,
    /// `--fallback-output`…), which would win over the file at the next logon.
    pub fn is_daemon_argument(&self) -> bool {
        matches!(self, Change::Effects(_) | Change::Gain(_) | Change::FallbackOutput(_) | Change::FallbackInput(_))
    }

    /// What the notification says when it could not be saved.
    pub fn failed(&self) -> String {
        trf!("« {option} » non enregistré", option = self.setting().label())
    }
}

/// The language menu: Windows' language first ("Automatique (Deutsch)", or English when Platinum
/// does not speak Windows' language), then every language in itself, `Language::ALL`'s order.
pub fn language_menu(windows: Option<Language>) -> Vec<String> {
    let automatic = trf!("Automatique ({language})", language = windows.unwrap_or(Language::En).name());
    std::iter::once(automatic).chain(Language::ALL.iter().map(|language| language.name().to_string())).collect()
}

/// A choice's line in the language menu.
pub fn language_index(choice: Choice) -> i32 {
    match choice {
        Choice::Auto => 0,
        Choice::Fixed(language) => Language::ALL.iter().position(|l| *l == language).map_or(0, |i| i as i32 + 1),
    }
}

/// The choice on a line of the language menu; `None` past its end.
pub fn language_choice(index: i32) -> Option<Choice> {
    match index {
        0 => Some(Choice::Auto),
        _ => usize::try_from(index - 1).ok().and_then(|i| Language::ALL.get(i)).map(|l| Choice::Fixed(*l)),
    }
}

/// A volume kept to the percent the page shows, within 0 to 1.
fn percent(gain: f32) -> f32 {
    (gain.clamp(0.0, 1.0) * 100.0).round() / 100.0
}

/// The options the daemon reads only when it starts, as one daemon took them.
#[derive(Clone, Debug, PartialEq)]
pub struct StartSettings {
    pub effects: bool,
    pub gain: f32,
    pub fallback_output: Option<String>,
    pub fallback_input: Option<String>,
}

impl StartSettings {
    pub fn of(config: &Config) -> StartSettings {
        StartSettings { effects: config.effects, gain: config.gain, fallback_output: config.fallback_output.clone(), fallback_input: config.fallback_input.clone() }
    }

    /// What a daemon first seen running took: the configuration as it is (it read the file when
    /// it started, and only the window writes these keys), except the effects, which its status
    /// tells for sure while it relays a microphone (relaying without effects that were asked for
    /// is "degraded").
    pub fn of_running(config: &Config, status: &Status) -> StartSettings {
        let effects = match status.state {
            DaemonState::Running if !status.microphone.is_empty() => status.effects,
            DaemonState::Degraded => true,
            _ => config.effects,
        };
        StartSettings { effects, ..StartSettings::of(config) }
    }

    /// Whether the option differs between the two (false for options read at another time).
    fn differs(&self, other: &StartSettings, setting: Setting) -> bool {
        match setting {
            Setting::Effects => self.effects != other.effects,
            Setting::Gain => (percent(self.gain) - percent(other.gain)).abs() > 0.001,
            Setting::FallbackOutput => self.fallback_output != other.fallback_output,
            Setting::FallbackInput => self.fallback_input != other.fallback_input,
            _ => false,
        }
    }
}

/// The options changed since the running daemon started (`applied`): they wait for its next start.
pub fn pending(applied: &StartSettings, now: &StartSettings) -> Vec<Setting> {
    Setting::ALL.into_iter().filter(|setting| setting.applies() == Applies::NextStart && applied.differs(now, *setting)).collect()
}

/// The notice above the page while some options wait for the daemon's next start; empty when none
/// does.
pub fn pending_sentence(pending: &[Setting]) -> String {
    let names: Vec<String> = pending.iter().enumerate().map(|(i, setting)| if i == 0 { setting.label().to_string() } else { i18n::in_sentence(setting.label()) }).collect();
    match names.as_slice() {
        [] => String::new(),
        [one] => trf!("{option} : s'appliquera au prochain démarrage de Platinum.", option = one),
        [first @ .., last] => trf!("{options} : s'appliqueront au prochain démarrage de Platinum.", options = trf!("{list} et {last}", list = first.join(", "), last = last)),
    }
}

/// What the fallback devices' menus say for "none": the device that was the default when the
/// daemon started is put back.
pub const BEFORE_PLATINUM: &str = mark("Celle d'avant Platinum");

/// A fallback device's menu: "Celle d'avant Platinum", the devices that can be made the default
/// now, then the one platinum.ini names when it is not among them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeviceMenu {
    pub labels: Vec<String>,
    /// What choosing each line writes in platinum.ini.
    pub specs: Vec<Option<String>>,
    pub current: i32,
}

/// The menu of the fallback device of one direction. `endpoints` are every device Windows lists,
/// `hands_free` the Bluetooth headsets' hands-free sides (never offered: making one the default
/// would switch the headset to its hands-free profile as soon as an application opens it), and
/// `configured` what platinum.ini holds, looked up as the daemon does.
pub fn device_menu(endpoints: &[Endpoint], flow: Flow, hands_free: &[String], configured: Option<&str>) -> DeviceMenu {
    let candidates: Vec<Endpoint> = endpoints.iter().filter(|e| e.flow == flow).cloned().collect();
    let offered: Vec<&Endpoint> = candidates.iter().filter(|e| e.virtual_role.is_none() && e.state == State::Active && !hands_free.contains(&e.id)).collect();
    let mut menu = DeviceMenu { labels: vec![tr(BEFORE_PLATINUM).to_string()], specs: vec![None], current: 0 };
    for endpoint in &offered {
        menu.labels.push(endpoint.name.clone());
        menu.specs.push(Some(device_spec(&candidates, endpoint)));
    }
    let Some(spec) = configured.map(str::trim).filter(|s| !s.is_empty()) else { return menu };
    let found = devices::find_device(&candidates, spec).ok();
    match found.and_then(|device| offered.iter().position(|e| e.id == device.id)) {
        Some(index) => menu.current = index as i32 + 1,
        None => {
            menu.labels.push(trf!("{device} (absent)", device = found.map_or(spec, |device| device.name.as_str())));
            menu.specs.push(Some(spec.to_string()));
            menu.current = menu.labels.len() as i32 - 1;
        }
    }
    menu
}

/// How platinum.ini names a device: by its name, which stays readable, when the daemon's search
/// finds exactly this device by it; otherwise by its identifier (two devices share the name, or
/// one's name contains the other's). `candidates` are the devices of its direction.
pub fn device_spec(candidates: &[Endpoint], endpoint: &Endpoint) -> String {
    match devices::find_device(candidates, &endpoint.name) {
        Ok(found) if found.id == endpoint.id => endpoint.name.clone(),
        _ => endpoint.id.clone(),
    }
}

/// One of the user's links as the "Jeux" section lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkLine {
    pub exe: String,
    pub title: String,
    /// The preset's name now (`preset_name` finds it by key), else as it was named when linked.
    pub preset: String,
}

/// The user's links (game-links.txt), by title.
pub fn link_lines(links: &[UserLink], preset_name: impl Fn(&str) -> Option<String>) -> Vec<LinkLine> {
    let mut lines: Vec<LinkLine> = links
        .iter()
        .map(|link| LinkLine {
            exe: link.exe.clone(),
            title: if link.title.is_empty() { games::program_name(&link.exe) } else { link.title.clone() },
            preset: preset_name(&link.preset).unwrap_or_else(|| if link.preset_name.is_empty() { tr("Préréglage introuvable").to_string() } else { link.preset_name.clone() }),
        })
        .collect();
    lines.sort_by_key(|line| line.title.to_lowercase());
    lines
}

/// status.txt's stamp ("20261117-174426") as the page shows it ("17/11/2026 à 17:44:26" in
/// French, the date in the current language's order); any other text as it is.
pub fn stamp_text(stamp: &str) -> String {
    match stamp.split_once('-') {
        Some((date, time)) if date.len() == 8 && time.len() == 6 && date.bytes().chain(time.bytes()).all(|b| b.is_ascii_digit()) => {
            let (year, month, day) = (&date[..4], &date[4..6], &date[6..8]);
            let date = match i18n::language() {
                Language::En => format!("{year}-{month}-{day}"),
                Language::De => format!("{day}.{month}.{year}"),
                _ => format!("{day}/{month}/{year}"),
            };
            trf!("{date} à {time}", date = date, time = format!("{}:{}:{}", &time[..2], &time[2..4], &time[4..6]))
        }
        _ => stamp.to_string(),
    }
}

/// An amount of memory in megabytes, with the current language's decimal mark and unit ("39,2 Mo"
/// in French).
pub fn megabytes_text(bytes: usize) -> String {
    trf!("{amount} Mo", amount = i18n::decimal(format!("{:.1}", bytes as f64 / (1024.0 * 1024.0))))
}

/// The daemon's memory as the page shows it: private memory first (what the hourly lines of
/// daemon.log follow), then the working set.
pub fn memory_text(memory: Option<(usize, usize)>) -> String {
    match memory {
        Some((private, working_set)) => trf!("{private} privés · {resident} en mémoire vive", private = megabytes_text(private), resident = megabytes_text(working_set)),
        None => "—".to_string(),
    }
}

/// The microphone engine's memory, next to the daemon's: the worker process that hosts the relay
/// and the vendor's effects engine, which the daemon replaces once it has grown. "—" while none
/// runs (no microphone relayed, or relayed inside the daemon).
pub fn engine_memory_text(pid: Option<u32>, memory: Option<(usize, usize)>) -> String {
    match (pid, memory) {
        (Some(pid), Some(memory)) => trf!("{memory} (processus {pid})", memory = memory_text(Some(memory)), pid = pid),
        _ => "—".to_string(),
    }
}

/// A crash report as the "Rapports de plantage" section lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrashLine {
    /// The report's file name, which identifies it.
    pub name: String,
    pub date: String,
    pub program: String,
    pub reason: String,
}

/// A program of Platinum as the window names it, in the current language.
pub fn program_label(program: &str) -> String {
    match program {
        crash::CLI => tr("Ligne de commande").to_string(),
        crash::DAEMON => tr("Tâche de fond").to_string(),
        crash::WORKER => tr("Moteur du micro").to_string(),
        crash::WINDOW => tr("Fenêtre").to_string(),
        crash::LAUNCHER => tr("Lanceur").to_string(),
        crash::SETUP => tr("Installateur").to_string(),
        crash::SETUP_ADMIN => tr("Installateur (partie administrateur)").to_string(),
        other => other.to_string(),
    }
}

/// The reports, newest first, as the section lists them; the reason is the report's own line
/// (technical, in English), cut to what one line shows.
pub fn crash_lines(reports: &[Summary]) -> Vec<CrashLine> {
    reports
        .iter()
        .map(|report| {
            let reason = if report.reason.is_empty() { tr("Raison inconnue").to_string() } else { report.reason.clone() };
            CrashLine { name: report.name.clone(), date: stamp_text(&report.stamp), program: program_label(&report.program), reason: if reason.chars().count() > 160 { reason.chars().take(159).collect::<String>() + "…" } else { reason } }
        })
        .collect()
}

/// The question the window asks after a crash (`crash_reports = ask`).
pub fn crash_question(report: &Summary) -> String {
    trf!("Un plantage de Platinum ({program}) a été enregistré sur ce PC le {date}. Voulez-vous envoyer le rapport pour aider à corriger le problème ? Vous le lirez d'abord, anonymisé, et c'est vous qui l'enverrez depuis GitHub.", program = program_label(&report.program), date = stamp_text(&report.stamp))
}

/// Where an issue would go, or why none can be opened (this build names no repository).
pub fn repository_sentence(repository: Option<&str>) -> String {
    match repository {
        Some(repo) => trf!("« Ouvrir sur GitHub » ouvre une nouvelle issue sur github.com/{repo}, préremplie avec le rapport anonymisé : rien ne part tant que vous ne l'envoyez pas vous-même.", repo = repo),
        None => tr("Le dépôt GitHub de Platinum n'est pas encore défini dans cette version : l'envoi par GitHub est indisponible. Vous pouvez lire et copier les rapports anonymisés.").to_string(),
    }
}

/// What the send dialog says once "Ouvrir sur GitHub" was clicked.
pub fn sent_note(truncated: bool) -> &'static str {
    if truncated {
        tr("Le rapport était trop long pour un lien : il a été coupé, et le texte complet anonymisé est dans le presse-papiers. Collez-le dans l'issue avant de l'envoyer.")
    } else {
        tr("La page de GitHub s'ouvre dans votre navigateur : relisez l'issue, puis envoyez-la vous-même si vous le voulez.")
    }
}

/// Where the daemon's log may be, most likely first: next to the running daemon's program (the
/// launcher writes it next to itself, and both sit in one folder), then in the install folder.
pub fn log_candidates(daemon_program: Option<&Path>, install: Option<&Path>) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    for folder in [daemon_program.and_then(Path::parent), install].into_iter().flatten() {
        let file = folder.join("daemon.log");
        if !files.contains(&file) {
            files.push(file);
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use platinum::games::remove_link;

    #[test]
    fn only_what_the_daemon_reads_at_start_waits_for_a_restart() {
        let next_start: Vec<Setting> = Setting::ALL.into_iter().filter(|s| s.applies() == Applies::NextStart).collect();
        assert_eq!(next_start, [Setting::Effects, Setting::Gain, Setting::FallbackInput, Setting::FallbackOutput]);
        // Read every 2 s by the daemon, or by Windows and the window at their own time.
        for setting in [Setting::AutoSwitch, Setting::Streamer, Setting::FollowStreaming, Setting::RouteApps, Setting::StreamingSinks, Setting::GamePresets, Setting::Shortcuts, Setting::ChannelNames, Setting::CrashReports] {
            assert_eq!(setting.applies(), Applies::Now, "{setting:?}");
        }
        assert_eq!(Setting::StartWithWindows.applies(), Applies::NextLogon);
        assert_eq!(Setting::StartOnOpen.applies(), Applies::NextWindow);
        assert_eq!(Setting::Microphone.applies(), Applies::Restart);
        assert!(Applies::Restart.note().contains("quelques secondes"));
        assert_eq!(Applies::NextStart.note(), "S'applique au prochain démarrage de Platinum.");
        // One note per option, in the page's order.
        assert_eq!(notes().len(), Setting::ALL.len());
        assert!(Setting::ALL.iter().enumerate().all(|(i, s)| Setting::from_index(i as i32) == Some(*s)));
        assert_eq!(Setting::from_index(-1), None);
        assert_eq!(Setting::from_index(Setting::ALL.len() as i32), None);
    }

    #[test]
    fn changes_wait_only_while_they_differ_from_what_the_daemon_took() {
        let config = Config { effects: true, gain: 0.8, fallback_output: Some("AirPods".into()), ..Config::default() };
        let applied = StartSettings::of(&config);
        assert!(pending(&applied, &applied).is_empty());
        let mut now = config.clone();
        Change::Effects(false).apply(&mut now);
        Change::Gain(0.5).apply(&mut now);
        Change::FollowStreaming(false).apply(&mut now);
        let waiting = pending(&applied, &StartSettings::of(&now));
        assert_eq!(waiting, [Setting::Effects, Setting::Gain]);
        assert_eq!(pending_sentence(&waiting), "Effets du micro et volume du micro : s'appliqueront au prochain démarrage de Platinum.");
        assert_eq!(pending_sentence(&[Setting::FallbackOutput]), "Sortie remise à l'arrêt : s'appliquera au prochain démarrage de Platinum.");
        // Put back as it was: nothing waits any more.
        Change::Effects(true).apply(&mut now);
        Change::Gain(0.8).apply(&mut now);
        assert!(pending(&applied, &StartSettings::of(&now)).is_empty());
        assert_eq!(pending_sentence(&[]), "");
        Change::FallbackOutput(None).apply(&mut now);
        Change::FallbackInput(Some("Seiren".into())).apply(&mut now);
        let waiting = pending(&applied, &StartSettings::of(&now));
        assert_eq!(waiting, [Setting::FallbackInput, Setting::FallbackOutput]);
        assert!(pending_sentence(&[Setting::Effects, Setting::Gain, Setting::FallbackOutput]).starts_with("Effets du micro, volume du micro et sortie remise à l'arrêt : "));
    }

    fn status(state: DaemonState, microphone: &str, effects: bool) -> Status {
        Status { state, pid: 7, microphone: microphone.into(), effects, message: String::new(), remote: String::new(), worker: 0, replaced: String::new(), audio_frozen: String::new(), updated: String::new(), vanished: false }
    }

    #[test]
    fn a_running_daemon_tells_its_effects_by_its_status() {
        let config = Config { effects: true, ..Config::default() };
        // Started without effects, then the file changed: the status says so.
        assert!(!StartSettings::of_running(&config, &status(DaemonState::Running, "Seiren", false)).effects);
        // Effects asked for, refused by the engine: it took them.
        let off = Config { effects: false, ..Config::default() };
        assert!(StartSettings::of_running(&off, &status(DaemonState::Degraded, "Seiren", false)).effects);
        // Relaying nothing, its status says nothing of them: the file is all there is.
        assert!(StartSettings::of_running(&config, &status(DaemonState::WaitingDevices, "", false)).effects);
        assert!(!StartSettings::of_running(&off, &status(DaemonState::Running, "", false)).effects);
    }

    #[test]
    fn every_key_the_page_writes_survives_the_file() {
        let changes = [
            Change::StartOnOpen(false),
            Change::Effects(false),
            Change::Gain(0.4237),
            Change::FallbackOutput(Some("Haut-parleurs (SteelSeries Arena 7)".into())),
            Change::FallbackInput(Some("{0.0.1.00000000}.{c3}".into())),
            Change::FollowStreaming(false),
            Change::RouteApps(false),
            Change::StreamingSinks(" Steam Streaming Speakers, , Sunshine Virtual ".into()),
            Change::GamePresets(false),
            Change::ArenaSpeakers(false),
            Change::CrashReports(Consent::Ask),
        ];
        let before = Config { microphone: Some("Seiren".into()), auto_switch: true, ..Config::default() };
        let mut config = before.clone();
        for change in &changes {
            let mut alone = before.clone();
            change.apply(&mut alone);
            assert_ne!(alone, before, "{change:?} changes nothing");
            change.apply(&mut config);
        }
        let (read, problems) = Config::parse(&config.to_text());
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(read, config);
        assert_eq!(read.gain, 0.42);
        assert_eq!(read.streaming_sinks, ["Steam Streaming Speakers", "Sunshine Virtual"]);
        assert_eq!(read.fallback_output.as_deref(), Some("Haut-parleurs (SteelSeries Arena 7)"));
        // What the page does not touch stays.
        assert_eq!((read.microphone.as_deref(), read.auto_switch), (Some("Seiren"), true));
        // Back to the defaults; "none" is no line value at all.
        let mut back = read.clone();
        for change in [Change::StartOnOpen(true), Change::Effects(true), Change::Gain(1.0), Change::FallbackOutput(None), Change::FallbackInput(Some(" ".into())), Change::FollowStreaming(true), Change::RouteApps(true), Change::StreamingSinks("Steam Streaming Speakers".into()), Change::GamePresets(true), Change::ArenaSpeakers(true), Change::CrashReports(Consent::Off)] {
            change.apply(&mut back);
        }
        let expected = Config { microphone: Some("Seiren".into()), auto_switch: true, ..Config::default() };
        assert_eq!(back, expected);
        assert!(back.to_text().contains("\nfallback_output = \n") && back.to_text().contains("\nfallback_input = \n"));
    }

    #[test]
    fn the_language_menu_offers_windows_language_then_each_one() {
        let menu = language_menu(Some(Language::De));
        assert_eq!(menu, ["Automatique (Deutsch)", "Français", "English", "Español", "Deutsch", "Italiano", "Português"]);
        // Windows in a language Platinum does not speak: English.
        assert_eq!(language_menu(None)[0], "Automatique (English)");
        for index in 0..menu.len() as i32 {
            let choice = language_choice(index).unwrap();
            assert_eq!(language_index(choice), index);
        }
        assert_eq!(language_choice(7), None);
        assert_eq!(language_choice(-1), None);
        assert_eq!(language_choice(2), Some(Choice::Fixed(Language::En)));
        // The window applies it at once, the daemon on its next tick.
        assert_eq!(Setting::Language.applies(), Applies::WindowNowDaemonSoon);
        let mut config = Config::default();
        Change::Language(Choice::Fixed(Language::Pt)).apply(&mut config);
        assert_eq!(Config::parse(&config.to_text()).0.language, Choice::Fixed(Language::Pt));
        assert!(!Change::Language(Choice::Auto).is_daemon_argument());
    }

    #[test]
    fn switches_and_daemon_arguments_are_told_apart() {
        assert_eq!(Change::switch(Setting::Effects, false), Some(Change::Effects(false)));
        assert_eq!(Change::switch(Setting::ArenaSpeakers, true), Some(Change::ArenaSpeakers(true)));
        for setting in [Setting::StartWithWindows, Setting::ChannelNames, Setting::AutoSwitch, Setting::Streamer, Setting::Microphone] {
            assert_eq!(Change::switch(setting, true), None, "{setting:?}");
        }
        for setting in Setting::ALL {
            if let Some(change) = Change::switch(setting, true) {
                assert_eq!(change.setting(), setting);
            }
        }
        assert!(Change::Gain(0.5).is_daemon_argument() && Change::FallbackInput(None).is_daemon_argument());
        assert!(!Change::GamePresets(true).is_daemon_argument() && !Change::StartOnOpen(true).is_daemon_argument());
        assert_eq!(Change::Effects(true).failed(), "« Effets du micro » non enregistré");
    }

    fn endpoint(id: &str, name: &str, flow: Flow, state: State) -> Endpoint {
        Endpoint { id: id.into(), name: name.into(), flow, state, virtual_role: None }
    }

    #[test]
    fn the_fallback_menus_offer_what_can_be_the_default() {
        let mut gaming = endpoint("{gaming}", "Gaming (Platinum)", Flow::Render, State::Active);
        gaming.virtual_role = Some(2);
        let endpoints = [
            endpoint("{arena}", "Haut-parleurs (SteelSeries Arena 7)", Flow::Render, State::Active),
            endpoint("{airpods}", "Casque (AirPods Pro)", Flow::Render, State::Unplugged),
            endpoint("{screen-a}", "Écran", Flow::Render, State::Active),
            endpoint("{screen-b}", "Écran", Flow::Render, State::Active),
            endpoint("{seiren}", "Microphone (Razer Seiren Mini)", Flow::Capture, State::Active),
            endpoint("{airpods-mic}", "Casque (AirPods Pro)", Flow::Capture, State::Active),
            gaming,
        ];
        // A device named in the file by a part of its name, unplugged now: kept, marked absent.
        let outputs = device_menu(&endpoints, Flow::Render, &[], Some("AirPods"));
        assert_eq!(outputs.labels, ["Celle d'avant Platinum", "Haut-parleurs (SteelSeries Arena 7)", "Écran", "Écran", "Casque (AirPods Pro) (absent)"]);
        assert_eq!(outputs.current, 4);
        // Two devices share a name: they are saved by identifier.
        assert_eq!(outputs.specs[1..4], [Some("Haut-parleurs (SteelSeries Arena 7)".to_string()), Some("{screen-a}".into()), Some("{screen-b}".into())]);
        assert_eq!(outputs.specs[4].as_deref(), Some("AirPods"));
        // A device named by a part of its name and plugged in is that line.
        assert_eq!(device_menu(&endpoints, Flow::Render, &[], Some("Arena")).current, 1);
        assert_eq!(device_menu(&endpoints, Flow::Render, &[], None).current, 0);
        // A Bluetooth headset's microphone is never offered.
        let inputs = device_menu(&endpoints, Flow::Capture, &["{airpods-mic}".to_string()], Some("Seiren"));
        assert_eq!((inputs.labels.as_slice(), inputs.current), (["Celle d'avant Platinum".to_string(), "Microphone (Razer Seiren Mini)".to_string()].as_slice(), 1));
        // A device nothing matches any more.
        let gone = device_menu(&endpoints, Flow::Capture, &[], Some("Yeti"));
        assert_eq!((gone.labels.last().map(String::as_str), gone.current), (Some("Yeti (absent)"), 3));
    }

    #[test]
    fn a_microphone_is_saved_by_name_unless_the_name_is_ambiguous() {
        let captures = [endpoint("{a}", "Microphone (Seiren Mini)", Flow::Capture, State::Active), endpoint("{b}", "Microphone (Arena 7)", Flow::Capture, State::Active), endpoint("{c}", "Microphone (Arena 7)", Flow::Capture, State::Active)];
        assert_eq!(device_spec(&captures, &captures[0]), "Microphone (Seiren Mini)");
        assert_eq!(device_spec(&captures, &captures[2]), "{c}");
    }

    #[test]
    fn links_are_listed_by_title_and_go_when_dissociated() {
        let mut links = vec![
            UserLink { exe: "hl2.exe".into(), title: "Half-Life 2".into(), preset: "own:Soir".into(), preset_name: "Soir".into() },
            UserLink { exe: "DELTARUNE.exe".into(), title: String::new(), preset: "flat".into(), preset_name: "Neutre".into() },
            UserLink { exe: "old.exe".into(), title: "Ancien".into(), preset: "own:Parti".into(), preset_name: String::new() },
        ];
        let name = |key: &str| (key == "flat").then(|| "Neutre (Platinum)".to_string());
        let lines = link_lines(&links, name);
        let shown: Vec<(&str, &str)> = lines.iter().map(|l| (l.title.as_str(), l.preset.as_str())).collect();
        assert_eq!(shown, [("Ancien", "Préréglage introuvable"), ("DELTARUNE", "Neutre (Platinum)"), ("Half-Life 2", "Soir")]);
        assert!(remove_link(&mut links, "deltarune.EXE"));
        let left: Vec<String> = link_lines(&links, name).into_iter().map(|l| l.exe).collect();
        assert_eq!(left, ["old.exe", "hl2.exe"]);
    }

    #[test]
    fn crash_reports_are_listed_and_offered_in_words() {
        let report = Summary { file: PathBuf::from("x/20261122-153000-mic-worker.txt"), name: "20261122-153000-mic-worker.txt".into(), stamp: "20261122-153000".into(), program: "mic-worker".into(), reason: "access violation (0xC0000005) in Sonar.APO.dll+0x1A2B".into() };
        let odd = Summary { program: "other-tool".into(), reason: String::new(), stamp: "hier".into(), ..report.clone() };
        let lines = crash_lines(&[report.clone(), odd]);
        assert_eq!(lines[0], CrashLine { name: report.name.clone(), date: "22/11/2026 à 15:30:00".into(), program: "Moteur du micro".into(), reason: report.reason.clone() });
        assert_eq!((lines[1].program.as_str(), lines[1].reason.as_str(), lines[1].date.as_str()), ("other-tool", "Raison inconnue", "hier"));
        assert!(crash_question(&report).starts_with("Un plantage de Platinum (Moteur du micro) a été enregistré sur ce PC le 22/11/2026 à 15:30:00."));
        for program in [crash::CLI, crash::DAEMON, crash::WORKER, crash::WINDOW, crash::LAUNCHER, crash::SETUP, crash::SETUP_ADMIN] {
            assert_ne!(program_label(program), program, "{program}");
        }
        // Without a repository the page says so, and never shows a link that leads nowhere.
        assert!(repository_sentence(None).contains("pas encore défini"));
        assert!(repository_sentence(Some("someone/platinum")).contains("github.com/someone/platinum"));
        assert!(sent_note(true).contains("presse-papiers") && !sent_note(false).contains("presse-papiers"));
        // The switch writes the consent; on means "ask".
        assert_eq!(Change::switch(Setting::CrashReports, true), Some(Change::CrashReports(Consent::Ask)));
        let mut config = Config::default();
        Change::CrashReports(Consent::Ask).apply(&mut config);
        assert_eq!(Config::parse(&config.to_text()).0.crash_reports, Consent::Ask);
    }

    #[test]
    fn the_about_section_speaks_french() {
        assert_eq!(stamp_text("20261117-174426"), "17/11/2026 à 17:44:26");
        assert_eq!(stamp_text(""), "");
        assert_eq!(stamp_text("hier"), "hier");
        assert_eq!(stamp_text("2026111-1744266"), "2026111-1744266");
        assert_eq!(megabytes_text(41_104_179), "39,2 Mo");
        assert_eq!(memory_text(Some((41_104_179, 52 * 1024 * 1024))), "39,2 Mo privés · 52,0 Mo en mémoire vive");
        assert_eq!(memory_text(None), "—");
        // The microphone engine's process, when one runs and its memory could be read.
        assert_eq!(engine_memory_text(Some(8124), Some((222_298_112, 240 * 1024 * 1024))), "212,0 Mo privés · 240,0 Mo en mémoire vive (processus 8124)");
        assert_eq!(engine_memory_text(Some(8124), None), "—");
        assert_eq!(engine_memory_text(None, None), "—");
        let logs = log_candidates(Some(Path::new(r"C:\Users\U\Platinum\platinum.exe")), Some(Path::new(r"C:\Users\U\Platinum")));
        assert_eq!(logs, [PathBuf::from(r"C:\Users\U\Platinum\daemon.log")]);
        let logs = log_candidates(Some(Path::new(r"C:\build\release\platinum.exe")), Some(Path::new(r"C:\Users\U\Platinum")));
        assert_eq!(logs, [PathBuf::from(r"C:\build\release\daemon.log"), PathBuf::from(r"C:\Users\U\Platinum\daemon.log")]);
        assert_eq!(log_candidates(None, None), Vec::<PathBuf>::new());
    }
}
