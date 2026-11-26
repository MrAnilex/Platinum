//! Platinum's window. The interface itself is described in ui/app.slint (and the files it imports);
//! this file connects it to the engine (src/engine.rs), which runs on its own thread, and to the
//! level meters (src/meters.rs).
//!
//! How the state flows:
//! - every user action goes to the engine as a `Command` and changes what is shown at once
//!   (optimistically), so that the control answers before the engine has written anything. An
//!   effect card's change is applied to the window's copy of the channel's preset with the same
//!   function the engine uses (`presets::apply`), then every view of that channel is redrawn from
//!   it;
//! - the engine sends a `Snapshot` after each burst of commands and every two seconds. A snapshot
//!   says how many commands the engine had handled when it was taken: one taken before the user's
//!   last action would undo that action on screen, so it is dropped, and the next one is used;
//! - the models are created once and updated row by row, comparing values: replacing a model would
//!   destroy every element drawn from it, a knob or a point being dragged included. A write that
//!   failed is corrected by the next snapshot, which holds what the stores really hold.

#![windows_subsystem = "windows"]

mod background;
mod catalog;
mod clipboard;
mod engine;
mod filedialog;
mod keys;
mod meters;
mod persist;
mod presets;
mod priority;
mod settings;
mod single;
mod sounds;
mod tester;

use background::{DaemonView, Phase};
use catalog::{Catalog, Entry, Filter, Shelf, Source};
use engine::{AppData, CHANNELS, ChannelData, ChannelDef, Command, MIC, MIC_TARGET, MeterDevices, Snapshot};
use persist::FAVORITES;
use platinum::devices::{Endpoint, Flow};
use platinum::eq;
use platinum::fallback::{self, PriorityDevice};
use platinum::games::{self, UserLink};
use platinum::hotkeys::{self, Action, Hotkey, GROUPS};
use platinum::overlay::{self, Position};
use platinum::preset::{Capture, FormFactor, Preset, Render, flat};
use presets::{BandEdit, BandValue, CardKind, Edit, Field, PresetRef, ROLES};
use settings::Change;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use platinum::mixer::{Master, Mix, STRIPS};
use platinum::streamer::Mode;
use sounds::MicPhase;
use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};
use std::time::{Duration, Instant};
use platinum::crash::{self, Consent};
use platinum::i18n::{self, Choice, Language, tr};
use platinum::trf;

slint::include_modules!();

/// The window's catalogs (its .slint files' texts and its Rust texts), French being the source.
/// The .slint texts are also bundled by Slint from the same files (build.rs).
const WINDOW_CATALOGS: i18n::Sources = &[
    (Language::En, include_str!("../lang/en/LC_MESSAGES/platinum-ui.po")),
    (Language::Es, include_str!("../lang/es/LC_MESSAGES/platinum-ui.po")),
    (Language::De, include_str!("../lang/de/LC_MESSAGES/platinum-ui.po")),
    (Language::It, include_str!("../lang/it/LC_MESSAGES/platinum-ui.po")),
    (Language::Pt, include_str!("../lang/pt/LC_MESSAGES/platinum-ui.po")),
];

/// The equalizer graph's box and range, as eq.slint draws it.
const CURVE_WIDTH: f64 = 1000.0;
const CURVE_HEIGHT: f64 = 240.0;
const CURVE_RANGE: f64 = 12.0;
/// How many steps across the graph the curve is sampled at for the "+" that follows it: eq.slint
/// reads the sample under the pointer.
const CURVE_SAMPLES: usize = 160;
/// At most this many notifications are shown at once (the newest); closing one shows the next.
const NOTICES_SHOWN: usize = 3;
/// A dragged application whose pointer has not moved for this long lost its pointer (the drop never
/// came): the drag is cleared, so that the mixer does not stay frozen.
const DRAG_TIMEOUT: Duration = Duration::from_secs(10);
/// Short names of the wheel's speakers, in `presets::WHEEL` order (translated where shown).
const SPEAKER_SHORT: [&str; 7] = [i18n::mark("C"), i18n::mark("AG"), i18n::mark("AD"), i18n::mark("LG"), i18n::mark("LD"), i18n::mark("ArG"), i18n::mark("ArD")];

/// The models the window shows, created once and then updated row by row.
struct Models {
    columns: Rc<VecModel<ColumnView>>,
    /// The applications of each strip (Gaming, Chat, Media, Aux).
    column_apps: Vec<Rc<VecModel<AppPill>>>,
    /// The applications that record the virtual microphone (the Micro strip).
    mic_apps: Rc<VecModel<AppPill>>,
    /// The favourites in each strip's preset menu (Gaming, Chat, Media, Aux, then Micro).
    column_favorites: Vec<Rc<VecModel<PresetTile>>>,
    master_apps: Rc<VecModel<AppPill>>,
    outputs: Rc<VecModel<SharedString>>,
    /// Streamer mode: the stream mix's device menu.
    stream_outputs: Rc<VecModel<SharedString>>,
    levels: Rc<VecModel<f32>>,
    mic_inputs: Rc<VecModel<SharedString>>,
    mic_hands_free: Rc<VecModel<bool>>,
    notices: Rc<VecModel<NoticeView>>,
    /// The open tab's equalizer, boost markers, speakers and favourite slots, shared by every tab.
    bands: Rc<VecModel<BandView>>,
    markers: Rc<VecModel<MarkerView>>,
    curve_samples: Rc<VecModel<f32>>,
    speakers: Rc<VecModel<SpeakerView>>,
    favorites: Rc<VecModel<PresetTile>>,
    browse_tiles: Rc<VecModel<PresetTile>>,
    browse_counts: Rc<VecModel<i32>>,
    /// The priority lists of the settings page, and the devices each can still take.
    output_priority: Rc<VecModel<PriorityRow>>,
    input_priority: Rc<VecModel<PriorityRow>>,
    output_addable: Rc<VecModel<SharedString>>,
    input_addable: Rc<VecModel<SharedString>>,
    /// The shortcuts section's rows, one model per group (`hotkeys::GROUPS`).
    shortcut_rows: Vec<Rc<VecModel<ShortcutRow>>>,
    /// The settings page's fallback device menus, and the user's links of presets to games.
    fallback_outputs: Rc<VecModel<SharedString>>,
    fallback_inputs: Rc<VecModel<SharedString>>,
    game_links: Rc<VecModel<LinkRow>>,
    /// The crash reports of this PC, newest first.
    crash_rows: Rc<VecModel<CrashRow>>,
    /// The open channel tab's test sounds.
    test_samples: Rc<VecModel<TestSample>>,
    /// The programs "Associer au jeu en cours…" offers.
    link_apps: Rc<VecModel<RunningApp>>,
}

/// The preset browser while it is open.
struct Browse {
    channel: usize,
    filter: Filter,
    query: String,
}

/// What the window keeps between snapshots.
struct Ui {
    models: Models,
    /// The last snapshot shown, with the user's changes applied since (presets, equalizer points).
    /// The menus on screen list its outputs, microphones and presets, so a choice is read against
    /// it.
    last: Snapshot,
    /// The Master as last shown, to work out on screen what the engine will write.
    master: Master,
    browse: Option<Browse>,
    /// The microphone picked in the Micro strip while its confirmation is on screen, by identifier:
    /// the list may change under the dialog (a device plugged in), its place in it with it.
    asked_microphone: Option<String>,
    /// What the test sounds' thread last said (what plays, the microphone test), with the notes
    /// the window adds itself when a button cannot work.
    test: tester::Status,
    /// Why the open channel tab's test sounds cannot be played (no output); `None` when they can.
    /// Worked out at each snapshot and tab change, not at every progress step.
    sample_problem: Option<String>,
    /// The note that GG's sounds are missing and generated ones play instead, for the open tab.
    sample_note: Option<String>,
    /// While "Associer au jeu en cours…" is open: the preset it links, and the programs offered
    /// (executable, title), in the dialog's order.
    linking: Option<(PresetRef, Vec<(String, String)>)>,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
    /// Where commands go, and how many the window has sent.
    static COMMANDS: RefCell<Option<Sender<Command>>> = const { RefCell::new(None) };
    static SENT: Cell<u64> = const { Cell::new(0) };
    /// Layout check only: the states drawn on top of the real ones (see `preview`).
    static PREVIEW: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// Layout check: nothing may be written, opened or started, file dialogs included.
    static READ_ONLY: Cell<bool> = const { Cell::new(false) };
    /// The test sounds' thread (see `tester`), started by the first test button pressed; dropping
    /// it when the window closes stops whatever plays or records.
    static TESTER: RefCell<Option<Sender<tester::Request>>> = const { RefCell::new(None) };
    /// A missing prerequisite (`platinum::doctor::banner`: no Sonar driver, no VC++ runtime…), as a
    /// French catalog key and a tone; it takes the daemon's banner line while it lasts.
    static PREREQUISITE: RefCell<Option<(&'static str, i32)>> = const { RefCell::new(None) };
    /// The daemon says Windows' audio service no longer answers (`watch_audio_service`). Read by
    /// the window itself from status.txt: the engine's thread may be stuck in a call to that
    /// service, and its snapshots with it.
    static AUDIO_FROZEN: Cell<bool> = const { Cell::new(false) };
    /// The restart of Windows' audio service asked from the banner (`repair_audio`).
    static REPAIR: RefCell<Repair> = const { RefCell::new(Repair::Idle) };
    /// The timer of `watch_audio_service`, kept for the life of the window.
    static AUDIO_TIMER: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };
    /// The window, for the full repaints the window procedure asks for (`full_repaint_requested`).
    static WINDOW: RefCell<slint::Weak<App>> = RefCell::new(slint::Weak::default());
    /// The crash report in the send dialog, anonymised: what "Copier le texte" and "Ouvrir sur
    /// GitHub" use.
    static CRASH_TEXT: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Makes the whole window dirty for the software renderer, so that the next frame repaints and
/// copies every pixel (the full-size rectangle under everything changes colour by one step).
fn repaint_everything(app: &App) {
    app.set_full_repaint(!app.get_full_repaint());
}

/// Set while a full repaint asked by the window procedure waits for the event loop: several
/// messages in a row make one repaint.
static REPAINT_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Called from the window procedure (`platinum::repaint::watch`) when Windows needs the window
/// drawn again in full: posts the repaint to the event loop rather than touching Slint there.
fn full_repaint_requested() {
    if REPAINT_PENDING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let _ = slint::invoke_from_event_loop(|| {
        REPAINT_PENDING.store(false, std::sync::atomic::Ordering::SeqCst);
        if let Some(app) = WINDOW.with(|slot| slot.borrow().upgrade()) {
            repaint_everything(&app);
        }
    });
}

/// Watches the window procedure once the native window exists (it is created by the event
/// loop): tried every 250 ms, for ten seconds at most.
fn watch_repaints(app: &App) {
    WINDOW.with(|slot| *slot.borrow_mut() = app.as_weak());
    let timer = Rc::new(slint::Timer::default());
    let tries = Rc::new(Cell::new(0));
    // The callback holds the timer, so it lives on after this function returns (a few bytes,
    // kept once it stops).
    let held = timer.clone();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(250), move || {
        tries.set(tries.get() + 1);
        if platinum::repaint::watch(full_repaint_requested) || tries.get() >= 40 {
            held.stop();
        }
    });
}

/// Looks for missing prerequisites on a thread of its own (device and registry reads, a few tens
/// of milliseconds), then shows the most important one in the banner line.
fn check_prerequisites(app: &App) {
    let weak = app.as_weak();
    let _ = std::thread::Builder::new().name("prérequis".into()).spawn(move || {
        let found = platinum::doctor::Findings::gather();
        let missing = platinum::doctor::banner(&found).map(|(text, level)| (text, if level == platinum::doctor::Level::Problem { 2 } else { 1 }));
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = weak.upgrade() {
                set_prerequisite(&app, missing);
            }
        });
    });
}

fn set_prerequisite(app: &App, missing: Option<(&'static str, i32)>) {
    PREREQUISITE.with(|slot| *slot.borrow_mut() = missing);
    refresh_daemon(app);
}

/// Draws the daemon's state again from the last snapshot, with what the window adds to it.
fn refresh_daemon(app: &App) {
    with_ui(|ui| app.set_daemon(daemon_state(&ui.last.daemon)));
}

/// Where the restart of Windows' audio service asked from the banner stands.
#[derive(Clone, Debug, PartialEq)]
enum Repair {
    Idle,
    /// Windows' elevation prompt, the restart, then Platinum's own restart.
    Running,
    /// Why it did not work out, and when that was known.
    Failed(String, Instant),
}

/// How long a failed restart stays said once the service answers again.
const REPAIR_FAILURE_SHOWN: Duration = Duration::from_secs(20);
/// Set while `repair_audio`'s thread runs: the window closing waits for it (`shut_down`), so that
/// Platinum is not left stopped between the stop and the start of its restart.
static REPAIRING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// The longest that thread can take: Windows' prompt answered, the service's restart
/// (`audioservice::run_elevated` waits 45 s at most), then Platinum's.
const REPAIR_LIMIT: Duration = Duration::from_secs(90);

/// The banner line about Windows' audio service, and its tone, when there is one to show: it goes
/// before everything else, since no sound plays meanwhile.
fn audio_banner(frozen: bool, repair: &Repair, now: Instant) -> Option<(String, i32)> {
    match repair {
        Repair::Running => Some((tr("Relance du son de Windows… Windows demande l'accord administrateur.").into(), 0)),
        Repair::Failed(error, at) if frozen || now.duration_since(*at) < REPAIR_FAILURE_SHOWN => Some((trf!("Relance du son de Windows non aboutie : {error}", error = error), 2)),
        _ if frozen => Some((tr("Le service Audio de Windows ne répond plus : le son de tous les programmes est figé.").into(), 2)),
        _ => None,
    }
}

/// Reads status.txt every second, on the window's thread, for the daemon's word on Windows' audio
/// service; draws the banner again when it changes, and forgets an old failure of the restart.
fn watch_audio_service(app: &App) {
    let weak = app.as_weak();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), move || {
        let frozen = platinum::status::read().is_some_and(|status| status.audio_frozen());
        let mut changed = AUDIO_FROZEN.with(|cell| cell.replace(frozen)) != frozen;
        REPAIR.with(|repair| {
            let mut repair = repair.borrow_mut();
            if matches!(&*repair, Repair::Failed(_, at) if !frozen && at.elapsed() >= REPAIR_FAILURE_SHOWN) {
                *repair = Repair::Idle;
                changed = true;
            }
        });
        if changed {
            if let Some(app) = weak.upgrade() {
                refresh_daemon(&app);
            }
        }
    });
    AUDIO_TIMER.with(|slot| *slot.borrow_mut() = Some(timer));
}

/// The banner's "Relancer le son de Windows": Windows' audio service is restarted as administrator
/// (`platinum audio-restart`, after Windows' prompt), then Platinum, whose streams went with the
/// old service. On a thread of its own, outside the engine.
fn repair_audio(app: &App) {
    if READ_ONLY.with(Cell::get) || REPAIR.with(|repair| *repair.borrow() == Repair::Running) {
        return;
    }
    REPAIR.with(|repair| *repair.borrow_mut() = Repair::Running);
    refresh_daemon(app);
    let weak = app.as_weak();
    REPAIRING.store(true, std::sync::atomic::Ordering::SeqCst);
    let spawned = std::thread::Builder::new().name("relance du son".into()).spawn(move || {
        let outcome = platinum::audioservice::run_elevated().and_then(|()| background::restart_now());
        REPAIRING.store(false, std::sync::atomic::Ordering::SeqCst);
        let _ = slint::invoke_from_event_loop(move || finish_repair(&weak, outcome));
    });
    if let Err(error) = spawned {
        REPAIRING.store(false, std::sync::atomic::Ordering::SeqCst);
        finish_repair(&app.as_weak(), Err(error.to_string()));
    }
}

fn finish_repair(weak: &slint::Weak<App>, outcome: Result<(), String>) {
    REPAIR.with(|repair| *repair.borrow_mut() = outcome.err().map_or(Repair::Idle, |error| Repair::Failed(error, Instant::now())));
    if let Some(app) = weak.upgrade() {
        refresh_daemon(&app);
    }
}

fn with_ui<R>(work: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    UI.with(|ui| ui.borrow_mut().as_mut().map(work))
}

/// Sends a command to the engine, counting it (see `apply`). On the window's thread only.
fn send(command: Command) {
    SENT.with(|sent| sent.set(sent.get() + 1));
    COMMANDS.with(|commands| {
        if let Some(commands) = commands.borrow().as_ref() {
            let _ = commands.send(command);
        }
    });
}

fn shared<T: Clone + 'static>(model: &Rc<VecModel<T>>) -> ModelRc<T> {
    ModelRc::from(model.clone())
}

/// Brings a model to `rows`, touching only the rows that differ: the elements drawn from the other
/// rows keep their state (a press, a hover, a drag).
fn sync<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    let before = model.row_count();
    for (index, row) in rows.iter().enumerate() {
        if index >= before {
            model.push(row.clone());
        } else if model.row_data(index).as_ref() != Some(row) {
            model.set_row_data(index, row.clone());
        }
    }
    while model.row_count() > rows.len() {
        model.remove(model.row_count() - 1);
    }
}

/// Changes the rows that match, in place.
fn edit_rows<T: Clone + 'static>(model: &VecModel<T>, matches: impl Fn(&T) -> bool, change: impl Fn(&mut T)) {
    for index in 0..model.row_count() {
        if let Some(mut row) = model.row_data(index).filter(|row| matches(row)) {
            change(&mut row);
            model.set_row_data(index, row);
        }
    }
}

fn channel_index(key: &str) -> Option<usize> {
    CHANNELS.iter().position(|c| c.key == key)
}

/// How a recording application's pill is named for the window: apart from the same application's
/// playing pill (Discord plays and records).
const CAPTURE_KEY: &str = "capture:";

fn pill(app: &AppData) -> AppPill {
    let name = if app.exe.to_ascii_lowercase().ends_with(".exe") { &app.exe[..app.exe.len() - 4] } else { app.exe.as_str() };
    let key = if app.capture { format!("{CAPTURE_KEY}{}", app.exe) } else { app.exe.clone() };
    AppPill { key: key.into(), exe: app.exe.as_str().into(), name: name.into(), playing: app.playing, device: app.device.as_str().into(), capture: app.capture, hint: app.own_device, refused: app.refused }
}

/// The playing applications of a strip (0: the Master's list), or the recording ones.
fn pills(snapshot: &Snapshot, channel_index: i32, capture: bool) -> Vec<AppPill> {
    snapshot.apps.iter().filter(|a| a.capture == capture && a.channel_index == channel_index).map(pill).collect()
}

/// The Master's list: what plays straight to a real device, then what records a real microphone.
fn master_pills(snapshot: &Snapshot) -> Vec<AppPill> {
    let mut list = pills(snapshot, 0, false);
    list.extend(pills(snapshot, 0, true));
    list
}

/// What dropping an application on the Master does, said before it is dropped: it follows the
/// default device, which is a channel while Platinum runs.
fn master_drop_hint(snapshot: &Snapshot) -> String {
    match usize::try_from(snapshot.default_channel - 1).ok().and_then(|i| CHANNELS.get(i).filter(|_| i < STRIPS)) {
        Some(def) => trf!("Déposer ici : suit la sortie par défaut de Windows, aujourd'hui {channel}", channel = tr(def.title)),
        None => tr("Déposer ici : sortie par défaut de Windows").into(),
    }
}

/// The name shown for the preset a channel uses.
fn preset_name(catalog: &Catalog, index: usize, data: &ChannelData) -> String {
    match catalog.entry(index, &data.selection) {
        Some(entry) => entry.name.clone(),
        None if data.selection == PresetRef::Custom || data.selection_name.is_empty() => tr("Personnalisé").into(),
        None => data.selection_name.clone(),
    }
}

fn column(def: &ChannelDef, index: usize, snapshot: &Snapshot, apps: ModelRc<AppPill>, favorites: ModelRc<PresetTile>) -> ColumnView {
    let data = snapshot.channels.get(index).cloned().unwrap_or_default();
    let streamer = &snapshot.streamer;
    ColumnView {
        key: def.key.into(),
        title: tr(def.title).to_uppercase().into(),
        icon: def.icon.into(),
        readable: data.readable,
        volume: data.volume,
        muted: data.muted,
        stream_volume: data.stream_volume,
        stream_muted: data.stream_muted,
        in_personal: streamer.personal_mix.get(index).copied().unwrap_or(false),
        in_stream: streamer.stream_mix.get(index).copied().unwrap_or(false),
        processing: data.preset.as_ref().is_some_and(presets::effects),
        output_index: data.output_index,
        apps,
        preset: preset_name(&snapshot.catalog, index, &data).into(),
        favorites,
    }
}

/// A preset as a tile; `games`: its second line says the games it is linked to (the browser's
/// "Par jeu") instead of where it comes from.
fn tile(entry: &Entry, shelf: &Shelf, selection: &PresetRef, games: bool) -> PresetTile {
    let source = if games && !entry.games.is_empty() { entry.games.join(", ") } else { entry.source.label().to_string() };
    PresetTile {
        key: entry.key.key().into(),
        name: entry.name.as_str().into(),
        source: source.into(),
        is_new: entry.is_new,
        favorite: shelf.is_favorite(&entry.key),
        selected: &entry.key == selection,
    }
}

/// A channel's favourites, in their order.
fn favorite_tiles(catalog: &Catalog, index: usize, selection: &PresetRef) -> Vec<PresetTile> {
    let Some(shelf) = catalog.shelf(index) else { return Vec::new() };
    shelf.favorites.iter().filter_map(|key| shelf.entry(key)).map(|e| tile(e, shelf, selection, false)).collect()
}

/// How a program the user linked is named in the "…" menu.
fn link_title(link: &UserLink) -> String {
    if link.title.is_empty() { games::program_name(&link.exe) } else { link.title.clone() }
}

/// The tab's current preset; `links` are the user's links of Gaming's presets to programs.
fn header(catalog: &Catalog, index: usize, data: &ChannelData, links: &[UserLink]) -> PresetHeader {
    let shelf = catalog.shelf(index);
    let entry = shelf.and_then(|s| s.entry(&data.selection));
    let modified = match (entry, &data.preset) {
        (Some(entry), Some(current)) => !presets::same_content(current, &entry.reference),
        _ => false,
    };
    let favorite = entry.zip(shelf).is_some_and(|(e, s)| s.is_favorite(&e.key));
    let full = shelf.is_some_and(|s| s.favorites.len() >= FAVORITES);
    let own = entry.is_some_and(|e| e.source == Source::Own);
    let source = match entry {
        Some(entry) => entry.source.label(),
        None if data.selection == PresetRef::Custom => tr("Réglages actuels du canal"),
        None => tr("Préréglage introuvable"),
    };
    PresetHeader {
        key: data.selection.key().into(),
        name: preset_name(catalog, index, data).into(),
        source: source.into(),
        modified,
        is_new: entry.is_some_and(|e| e.is_new),
        favorite,
        can_favorite: entry.is_some() && (favorite || !full),
        can_reset: entry.is_some() && modified,
        own,
        can_save: own && modified,
        ready: data.readable,
        game: if index == 0 { data.game.as_str().into() } else { SharedString::default() },
        can_link: index == 0 && entry.is_some(),
        links: ModelRc::new(VecModel::from(if index == 0 {
            let key = data.selection.key();
            links.iter().filter(|l| l.preset == key).map(|l| GameLinkView { exe: l.exe.as_str().into(), title: link_title(l).into() }).collect::<Vec<_>>()
        } else {
            Vec::new()
        })),
    }
}

/// Programs that are no game to link: Windows' shell and Platinum's own.
const NOT_GAMES: [&str; 16] = [
    "explorer.exe",
    "applicationframehost.exe",
    "textinputhost.exe",
    "shellexperiencehost.exe",
    "startmenuexperiencehost.exe",
    "searchhost.exe",
    "searchapp.exe",
    "systemsettings.exe",
    "lockapp.exe",
    "dwm.exe",
    "rundll32.exe",
    "audiodg.exe",
    "platinum.exe",
    "platinum.old.exe",
    "platinum-ui.exe",
    "platinum-start.exe",
];

/// The programs "Associer au jeu en cours…" offers: those with a window on screen (named by its
/// title), then those with sound (named by their executable), each executable once, by title.
/// Returns (executable, title, detail): the detail names the executable and the preset it is
/// linked to already, if any.
fn program_rows(windows: &[(String, String)], sounding: &[String], links: &[UserLink]) -> Vec<(String, String, String)> {
    let mut rows: Vec<(String, String)> = Vec::new();
    let candidates = windows.iter().cloned().chain(sounding.iter().map(|exe| (exe.clone(), games::program_name(exe))));
    for (exe, title) in candidates {
        if exe.is_empty() || NOT_GAMES.iter().any(|own| exe.eq_ignore_ascii_case(own)) || rows.iter().any(|(known, _)| known.eq_ignore_ascii_case(&exe)) {
            continue;
        }
        rows.push((exe, title));
    }
    rows.sort_by_key(|(_, title)| title.to_lowercase());
    rows.into_iter()
        .map(|(exe, title)| {
            let detail = match links.iter().find(|l| l.exe.eq_ignore_ascii_case(&exe)) {
                Some(link) => trf!("{exe} · associé à « {preset} »", exe = exe, preset = link.preset_name),
                None => exe.clone(),
            };
            (exe, title, detail)
        })
        .collect()
}

/// A filter as the graph draws it and its editor shows it.
fn band_view(band: &eq::Band) -> BandView {
    let kind = presets::FILTER_TYPES.iter().position(|(k, _)| *k == band.kind);
    BandView {
        enabled: band.enabled,
        x: eq::frequency_to_x(band.frequency) as f32,
        gain: band.gain as f32,
        frequency: band.frequency as f32,
        q: band.q as f32,
        kind: kind.map_or(-1, |k| k as i32),
        kind_name: presets::filter_type_title(band.kind).into(),
        gain_text: gain_text(band.gain).into(),
        frequency_text: frequency_text(band.frequency).into(),
        q_text: i18n::decimal(short_number(band.q, 3, 1)).into(),
    }
}

/// The bass, voice and treble boosts of a playback preset, marked as GG marks them: at their
/// frequency, at their gain when on and at 0 dB when off; none for a capture preset.
fn marker_views(preset: &Preset) -> Vec<MarkerView> {
    let Preset::Render(r) = preset else { return Vec::new() };
    // The first letter of each knob's label (Basses, Voix, Aigus), in the current language.
    let boosts = [(r.bass_boost, eq::BASS_BOOST.0, tr("B")), (r.voice_clarity, eq::VOICE_BOOST.0, tr("V")), (r.treble_boost, eq::TREBLE_BOOST.0, tr("A"))];
    boosts
        .into_iter()
        .map(|(boost, frequency, letter)| MarkerView {
            x: eq::frequency_to_x(frequency) as f32,
            gain: if boost.enabled { boost.value as f32 } else { 0.0 },
            on: boost.enabled && r.equalizer.enabled,
            letter: letter.into(),
        })
        .collect()
}

/// `value` with at most `most` decimals and at least `least`: 0.707, 1.0, 31.3.
fn short_number(value: f64, most: usize, least: usize) -> String {
    let text = format!("{value:.most$}");
    let Some(point) = text.find('.') else { return text };
    let keep = text.trim_end_matches('0').len().max(point + 1 + least);
    let text = &text[..keep];
    text.strip_suffix('.').unwrap_or(text).to_string()
}

/// "62 Hz", "31.3 Hz", "1.25 kHz", "16 kHz": GG's editor shows hertz below 1 kHz and kilohertz
/// from there. With a decimal comma in the languages that write one ("1,25 kHz").
fn frequency_text(frequency: f64) -> String {
    let text = if frequency >= 1000.0 { format!("{} kHz", short_number(frequency / 1000.0, 2, 0)) } else { format!("{} Hz", short_number(frequency, 1, 0)) };
    i18n::decimal(text)
}

/// "+3.5 dB", "0.0 dB", "-4.0 dB" (with a decimal comma in the languages that write one).
fn gain_text(gain: f64) -> String {
    let gain = (gain * 10.0).round() / 10.0;
    i18n::decimal(if gain == 0.0 { "0.0 dB".into() } else { format!("{gain:+.1} dB") })
}

/// The gain of the curve drawn from `drawn` at `CURVE_SAMPLES + 1` points across the graph.
fn curve_samples(drawn: &[eq::Band]) -> Vec<f32> {
    (0..=CURVE_SAMPLES).map(|i| eq::response_db(drawn, eq::x_to_frequency(i as f64 / CURVE_SAMPLES as f64)) as f32).collect()
}

fn render_view(r: &Render) -> RenderView {
    let lit = |on: bool| on && r.equalizer.enabled;
    // A boost whose switch is off while the equalizer is on is not heard: its knob shows 0 dB,
    // as its marker and the curve do, so that the first move starts from what is heard (the
    // stored value stays in the preset). With the equalizer off, the knobs show what comes back.
    let boost = |t: platinum::preset::Toggle| if r.equalizer.enabled && !t.enabled { 0.0 } else { t.value as f32 };
    RenderView {
        eq_on: r.equalizer.enabled,
        bass: boost(r.bass_boost),
        voice: boost(r.voice_clarity),
        treble: boost(r.treble_boost),
        bass_on: lit(r.bass_boost.enabled),
        voice_on: lit(r.voice_clarity.enabled),
        treble_on: lit(r.treble_boost.enabled),
        spatial_on: r.virtual_surround,
        speakers_mode: r.form_factor == FormFactor::Speakers,
        reverb: presets::reverb_position(r.reverb_gain_db) as f32,
        distance: presets::distance_position(r) as f32,
        boost: r.general_gain as f32,
        smart_on: r.smart_volume.enabled,
        smart_level: r.smart_volume.value as f32,
        loudness: presets::loudness_index(r.loudness),
    }
}

/// The capture cards; `held` is what the module holds in the two noise reductions' switches.
fn capture_view(c: &Capture, microphone: bool, held: (bool, bool)) -> CaptureView {
    let ai = c.noise_canceling.enabled;
    CaptureView {
        eq_on: c.equalizer.enabled,
        ai_on: ai,
        ai: presets::ai_position(c.noise_canceling.value) as f32,
        has_echo: microphone,
        echo_on: c.acoustic_echo_canceling,
        // As the module holds them, what is heard: AI noise cancelling should hold both off (B1),
        // and a module changed one setting at a time may not.
        nr_on: held.0,
        nr: c.noise_reduction.value as f32,
        impact_on: held.1,
        reductions_stuck: ai && (held.0 || held.1),
        impact: c.impact_noise_reduction.value as f32,
        gate_on: c.noise_gate.enabled,
        gate: c.noise_gate.value as f32,
        auto_gate: c.automatic_noise_gate.enabled,
        comp_on: c.volume_stabilizer.enabled,
        comp: c.volume_stabilizer.value as f32,
    }
}

fn speaker_views(r: &Render) -> Vec<SpeakerView> {
    presets::WHEEL
        .iter()
        .zip(SPEAKER_SHORT)
        .map(|((index, name), short)| SpeakerView { index: *index as i32, name: tr(name).into(), short: tr(short).into(), angle: r.speakers[*index].position as f32 })
        .collect()
}

/// "actif" -> "Actif", for the header.
fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
}

/// The line above the mixer while the sound does not go through Platinum, or goes to a remote
/// streaming session, and its tone (0 information, 1 warning, 2 error); empty otherwise.
fn banner(view: &DaemonView) -> (String, i32) {
    let with_detail = |sentence: &str, text: &str| if view.detail.is_empty() { tr(sentence).to_string() } else { trf!("{text} — {detail}", text = tr(text), detail = view.detail) };
    match view.phase {
        Phase::Running if !view.remote.is_empty() => (remote_sentence(&view.remote), 0),
        Phase::Running => (String::new(), 0),
        Phase::Starting => (tr("Démarrage de Platinum…").into(), 0),
        Phase::Stopping => (tr("Arrêt de Platinum…").into(), 0),
        Phase::Stopped => (tr("Le son ne passe pas par Platinum.").into(), 1),
        Phase::Failed => (with_detail(i18n::mark("Le son ne passe pas par Platinum."), i18n::mark("Le son ne passe pas par Platinum")), 2),
        Phase::Gg => (tr("SteelSeries GG est ouvert et garde la main sur le son : fermez GG pour démarrer Platinum.").into(), 0),
        Phase::Waiting if view.detail.is_empty() => (tr("Platinum attend ses périphériques audio.").into(), 1),
        Phase::Waiting => (view.detail.clone(), 1),
        Phase::Degraded => (with_detail(i18n::mark("Le micro passe sans les effets de Platinum."), i18n::mark("Le micro passe sans les effets de Platinum")), 1),
    }
}

/// What the window says while the daemon follows a remote streaming session.
fn remote_sentence(sink: &str) -> String {
    trf!("Streaming à distance : le son de Platinum part vers {output}", output = sink)
}

fn daemon_state(view: &DaemonView) -> DaemonState {
    let frozen = AUDIO_FROZEN.with(Cell::get);
    let repairing = REPAIR.with(|repair| *repair.borrow() == Repair::Running);
    // Windows' frozen audio service stops every sound: it goes first. Then a missing prerequisite,
    // which explains the rest.
    let audio = REPAIR.with(|repair| audio_banner(frozen, &repair.borrow(), Instant::now()));
    let (banner, banner_tone) = match (audio, PREREQUISITE.with(|slot| *slot.borrow())) {
        (Some(line), _) => line,
        (None, Some((text, tone))) => (tr(text).to_string(), tone),
        (None, None) => banner(view),
    };
    // A running daemon's message only says what it is doing: no warning box for it.
    let detail = if view.phase == Phase::Running { String::new() } else { view.detail.clone() };
    DaemonState {
        phase: view.phase.key().into(),
        label: capitalized(view.phase.label()).into(),
        summary: view.summary.as_str().into(),
        detail: detail.into(),
        microphone: view.microphone.as_str().into(),
        // A freeze said by a daemon that is gone may have ended since: "Démarrer" stays offered
        // beside the service's restart. Nothing else starts or stops Platinum during that restart,
        // which stops and starts it itself.
        can_start: view.can_start && !repairing,
        can_stop: view.can_stop && !repairing,
        restarts_on_mic_change: view.restarts_on_mic_change,
        banner: banner.into(),
        banner_tone,
        audio_frozen: frozen && !repairing,
        repairing_audio: repairing,
        remote: if view.remote.is_empty() { SharedString::new() } else { trf!("{sentence}. Tout revient sur vos sorties à la fin de la session.", sentence = remote_sentence(&view.remote)).into() },
    }
}

/// The daemon as shown right after the user asked to start or stop it, before the engine says.
fn daemon_under_way(app: &App, phase: Phase) {
    let mut view = DaemonView { phase, microphone: app.get_daemon().microphone.to_string(), ..DaemonView::default() };
    view.summary = if phase == Phase::Starting { tr("Démarrage…").into() } else { tr("Arrêt en cours…").into() };
    app.set_daemon(daemon_state(&view));
}

fn show_mixer(app: &App, models: &Models, snapshot: &Snapshot) {
    // Moving pills while one is dragged could destroy it under the pointer: the lists wait.
    if !app.get_dragging() {
        for (index, apps) in models.column_apps.iter().enumerate() {
            sync(apps, pills(snapshot, index as i32 + 1, false));
        }
        sync(&models.master_apps, master_pills(snapshot));
        sync(&models.mic_apps, pills(snapshot, MIC_TARGET, true));
        // A pill that went away under the pointer leaves no explanation behind.
        let hint = app.global::<Hint>();
        let key = hint.get_key();
        if !key.is_empty() && !snapshot.apps.iter().map(pill).any(|p| p.key == key) {
            hint.set_text(SharedString::default());
            hint.set_key(SharedString::default());
        }
    }
    app.set_master_drop_hint(master_drop_hint(snapshot).into());
    let columns = CHANNELS.iter().take(STRIPS).enumerate().map(|(index, def)| column(def, index, snapshot, shared(&models.column_apps[index]), shared(&models.column_favorites[index]))).collect();
    sync(&models.columns, columns);
    let master = &snapshot.master;
    app.set_master_ready(snapshot.channels.iter().take(STRIPS).all(|c| c.readable));
    app.set_master_volume(master.fader.volume);
    app.set_master_muted(master.fader.muted);
    app.set_master_output_index(master.output_index);
    app.set_master_level_known(master.device_level.is_some());
    app.set_master_level(master.device_level.unwrap_or(0.0));
    app.set_master_device_muted(master.device_muted);
    app.set_chatmix(snapshot.chatmix);
    app.set_chatmix_ready(snapshot.channels.iter().take(2).all(|c| c.readable) && snapshot.chatmix_reason.is_empty());
    app.set_chatmix_reason(snapshot.chatmix_reason.as_str().into());
    show_streamer(app, models, snapshot);
}

/// Streamer mode: the Stream master, the mixes' devices and the stream mix's device level (the
/// strips' own stream volumes and mixes are in their columns).
fn show_streamer(app: &App, models: &Models, snapshot: &Snapshot) {
    let streamer = &snapshot.streamer;
    app.set_streamer(streamer.mode == Mode::Streamer);
    app.set_stream_master_volume(snapshot.master.fader.stream.volume);
    app.set_stream_master_muted(snapshot.master.fader.stream.muted);
    app.set_personal_output_index(streamer.personal_index);
    sync(&models.stream_outputs, streamer.outputs.iter().map(SharedString::from).collect());
    app.set_stream_output_index(streamer.stream_index);
    app.set_stream_level_known(streamer.level.is_some());
    app.set_stream_level(streamer.level.map_or(0.0, |(level, _)| level));
    app.set_stream_device_muted(streamer.level.is_some_and(|(_, muted)| muted));
}

/// Every strip's preset name, "Effets" switch and favourites, from `snapshot` (the rest of the
/// strips is left as it is).
fn show_strip_presets(app: &App, models: &Models, snapshot: &Snapshot) {
    for (index, def) in CHANNELS.iter().enumerate() {
        let Some(data) = snapshot.channels.get(index) else { continue };
        let name: SharedString = preset_name(&snapshot.catalog, index, data).into();
        let processing = data.preset.as_ref().is_some_and(presets::effects);
        if let Some(favorites) = models.column_favorites.get(index) {
            sync(favorites, favorite_tiles(&snapshot.catalog, index, &data.selection));
        }
        if index == MIC {
            let mut mic = app.get_mic();
            if mic.preset != name || mic.processing != processing {
                mic.preset = name;
                mic.processing = processing;
                app.set_mic(mic);
            }
        } else {
            edit_rows(&models.columns, |c| c.key == def.key && (c.preset != name || c.processing != processing), |c| {
                c.preset = name.clone();
                c.processing = processing;
            });
        }
    }
}

fn show_microphone(app: &App, models: &Models, snapshot: &Snapshot) {
    app.set_mic(column(&CHANNELS[MIC], MIC, snapshot, ModelRc::default(), shared(&models.column_favorites[MIC])));
    let mic = &snapshot.mic;
    let names = mic.inputs.iter().map(|m| if m.hands_free { trf!("{device} — Bluetooth, refusé", device = m.name) } else { m.name.clone() }.into()).collect();
    sync(&models.mic_inputs, names);
    sync(&models.mic_hands_free, mic.inputs.iter().map(|m| m.hands_free).collect());
    app.set_mic_current(mic.current);
    app.set_mic_volume_known(mic.volume.is_some());
    app.set_mic_volume(mic.volume.unwrap_or(0.0));
    app.set_mic_muted(mic.muted);
}

/// The equalizer, the boost markers and the effect cards of a channel's preset.
fn show_effects(app: &App, models: &Models, data: &ChannelData, preset: &Preset, microphone: bool) {
    let bands = preset.equalizer().bands();
    let boosts = preset.boosts();
    // What the curve is drawn from: the ten filters (those that are off count for nothing) and the
    // boosts that are on.
    let drawn: Vec<eq::Band> = bands.iter().copied().chain(boosts.bands()).collect();
    sync(&models.bands, bands.iter().map(band_view).collect());
    sync(&models.markers, marker_views(preset));
    app.set_eq_curve(eq::curve_path_with_boosts(&bands, &boosts, CURVE_WIDTH, CURVE_HEIGHT, CURVE_RANGE).into());
    sync(&models.curve_samples, curve_samples(&drawn));
    app.global::<EqEditor>().set_can_add(presets::first_free_band(preset).is_some());
    match preset {
        Preset::Render(r) => {
            app.set_render(render_view(r));
            sync(&models.speakers, speaker_views(r));
        }
        Preset::Capture(c) => app.set_capture(capture_view(c, microphone, data.held_reductions)),
    }
}

/// The open channel tab: header, current preset, favourites, equalizer and effect cards.
fn show_tab(app: &App, models: &Models, snapshot: &Snapshot) {
    let page = app.get_page();
    let Some(index) = usize::try_from(page - 1).ok().filter(|&i| i < CHANNELS.len()) else { return };
    let def = &CHANNELS[index];
    let data = snapshot.channels.get(index).cloned().unwrap_or_default();
    app.set_channel_key(def.key.into());
    app.set_channel_readable(data.readable);
    app.set_channel_processing(data.preset.as_ref().is_some_and(presets::effects));
    app.set_channel_output_index(data.output_index);
    app.set_channel_capture(ROLES[index].is_capture());
    app.set_eq_voice_zones(snapshot.master.fader.voice_zones.get(index).copied().unwrap_or(false));
    app.set_preset(header(&snapshot.catalog, index, &data, &snapshot.game_links));
    let mut slots = favorite_tiles(&snapshot.catalog, index, &data.selection);
    app.set_favorite_count(slots.len() as i32);
    slots.resize(FAVORITES, PresetTile::default());
    sync(&models.favorites, slots);
    if let Some(preset) = &data.preset {
        show_effects(app, models, &data, preset, index == MIC);
    }
}

/// The browser's tiles and counts, when it is open.
fn show_browse(app: &App, models: &Models, snapshot: &Snapshot, browse: Option<&Browse>) {
    let Some(browse) = browse else { return };
    let Some(shelf) = snapshot.catalog.shelf(browse.channel) else { return };
    let selection = snapshot.channels.get(browse.channel).map(|c| c.selection.clone()).unwrap_or_default();
    let games = browse.filter == Filter::Games;
    sync(&models.browse_tiles, catalog::browse(shelf, browse.filter, &browse.query).into_iter().map(|e| tile(e, shelf, &selection, games)).collect());
    sync(&models.browse_counts, catalog::counts(shelf).iter().map(|&n| n as i32).collect());
    // "Par jeu" is Gaming's: GG's QuickSet links Game presets only.
    app.set_browse_games(ROLES[browse.channel] == platinum::preset::Role::Game);
    app.set_browse_key(CHANNELS[browse.channel].key.into());
    app.set_browse_title(tr(CHANNELS[browse.channel].title).to_uppercase().into());
    app.set_browse_filter(Filter::ALL.iter().position(|f| *f == browse.filter).unwrap_or(0) as i32);
    app.set_browse_full(shelf.favorites.len() >= FAVORITES);
    app.set_browse_note(snapshot.catalog.note.as_str().into());
}

fn priority_row(row: priority::Row) -> PriorityRow {
    PriorityRow { name: row.name.into(), state: row.state.into(), present: row.present, excluded: row.excluded, current: row.current }
}

/// The backup switch, both priority lists and their "Ajouter" menus, for the settings page (kept
/// up to date whether it is open or not: they are small).
fn show_devices(app: &App, models: &Models, snapshot: &Snapshot) {
    let data = &snapshot.priority;
    let input_current: Vec<String> = data.input_current.iter().cloned().collect();
    sync(&models.output_priority, priority::rows(&data.output_list, &data.output_devices, &data.hands_free, &data.output_current).into_iter().map(priority_row).collect());
    sync(&models.input_priority, priority::rows(&data.input_list, &data.input_devices, &data.hands_free, &input_current).into_iter().map(priority_row).collect());
    let labels = |list: &[PriorityDevice], devices: &[Endpoint], flow: Flow| priority::addable(list, devices, flow).into_iter().map(|d| SharedString::from(priority::addable_label(d))).collect();
    sync(&models.output_addable, labels(&data.output_list, &data.output_devices, Flow::Render));
    sync(&models.input_addable, labels(&data.input_list, &data.input_devices, Flow::Capture));
    app.set_auto_switch(data.auto_switch);
    let chosen = usize::try_from(snapshot.mic.current).ok().and_then(|i| snapshot.mic.inputs.get(i)).map(|m| m.name.clone()).unwrap_or_default();
    app.set_preferred_microphone(chosen.into());
}

/// Edits a priority list (0 the outputs, 1 the microphones) in the window's copy and shows it at
/// once, then sends the whole list to be saved. Each entry whose device is listed now takes its
/// current name, so that platinum.ini remembers the last name seen.
fn edit_priority(app: &App, list: i32, change: impl FnOnce(&mut Vec<PriorityDevice>, &[Endpoint], Flow) -> bool) {
    let flow = if list == 0 { Flow::Render } else { Flow::Capture };
    let edited = with_ui(|ui| {
        let data = &mut ui.last.priority;
        let (entries, devices) = match flow {
            Flow::Render => (&mut data.output_list, &data.output_devices),
            Flow::Capture => (&mut data.input_list, &data.input_devices),
        };
        if !change(entries, devices, flow) {
            return None;
        }
        fallback::refresh_names(entries, devices);
        let edited = entries.clone();
        show_devices(app, &ui.models, &ui.last);
        Some(edited)
    })
    .flatten();
    if let Some(entries) = edited {
        send(Command::SetPriority(flow, entries));
    }
}

fn connect_devices(app: &App) {
    let backend = app.global::<Backend>();

    // Shown at once as the engine saves it: an empty output list starts with the devices the
    // channels play to (`Engine::set_auto_switch`).
    let weak = app.as_weak();
    backend.on_set_auto_switch(move |on| {
        if let Some(app) = weak.upgrade() {
            with_ui(|ui| {
                let data = &mut ui.last.priority;
                data.auto_switch = on;
                if on && data.output_list.is_empty() {
                    for id in &data.output_current {
                        let name = data.output_devices.iter().find(|d| d.id == *id).map_or("", |d| d.name.as_str());
                        fallback::add(&mut data.output_list, id, name);
                    }
                }
                show_devices(&app, &ui.models, &ui.last);
            });
        }
        send(Command::SetAutoSwitch(on));
    });

    let weak = app.as_weak();
    backend.on_priority_move(move |list, index, delta| {
        if let Some(app) = weak.upgrade() {
            edit_priority(&app, list, |entries, _, _| fallback::move_entry(entries, index.max(0) as usize, delta as isize));
        }
    });

    let weak = app.as_weak();
    backend.on_priority_exclude(move |list, index, on| {
        if let Some(app) = weak.upgrade() {
            edit_priority(&app, list, |entries, _, _| fallback::set_excluded(entries, index.max(0) as usize, on));
        }
    });

    let weak = app.as_weak();
    backend.on_priority_remove(move |list, index| {
        if let Some(app) = weak.upgrade() {
            edit_priority(&app, list, |entries, _, _| fallback::remove(entries, index.max(0) as usize));
        }
    });

    // The menu lists `priority::addable` of the window's copy, which is what is read here.
    let weak = app.as_weak();
    backend.on_priority_add(move |list, index| {
        if let Some(app) = weak.upgrade() {
            edit_priority(&app, list, |entries, devices, flow| {
                let picked = priority::addable(entries, devices, flow).get(index.max(0) as usize).map(|d| (d.id.clone(), d.name.clone()));
                picked.is_some_and(|(id, name)| fallback::add(entries, &id, &name))
            });
        }
    });
}

/// The icon of each shortcuts group, in `hotkeys::GROUPS` order.
const GROUP_ICONS: [&str; 8] = ["master", "game", "chat", "media", "aux", "mic", "mixer", "speaker"];

/// Each group's rows, from the bindings: every action, its combination as shown, and whether
/// another action uses it too.
fn shortcut_rows(bindings: &[(Action, Hotkey)]) -> Vec<Vec<ShortcutRow>> {
    let conflicts = hotkeys::conflicts(bindings);
    let mut groups: Vec<Vec<ShortcutRow>> = GROUPS.iter().map(|_| Vec::new()).collect();
    for action in Action::ALL {
        let keys = bindings.iter().find(|(a, _)| *a == action).map(|(_, hotkey)| hotkey.display()).unwrap_or_default();
        let section = action.section().map(|section| tr(section).to_uppercase()).unwrap_or_default();
        let row = ShortcutRow { index: action.index() as i32, label: action.short_label().into(), keys: keys.into(), conflict: conflicts.contains(&action), section: section.into() };
        groups[action.group()].push(row);
    }
    groups
}

/// The shortcuts section: the rows and the overlays' options (kept up to date whether it is open
/// or not: they are small).
fn show_shortcuts(app: &App, models: &Models, snapshot: &Snapshot) {
    let bindings = &snapshot.settings.shortcuts;
    for (model, rows) in models.shortcut_rows.iter().zip(shortcut_rows(bindings)) {
        sync(model, rows);
    }
    app.set_shortcut_conflicts(!hotkeys::conflicts(bindings).is_empty());
    let overlays = snapshot.settings.overlays;
    app.set_overlays_on(overlays.enabled);
    app.set_overlay_position(overlays.position.index() as i32);
    app.set_overlay_volume(overlays.volume);
    app.set_overlay_mute(overlays.mute);
    app.set_overlay_device(overlays.device);
}

/// Binds (or unbinds) an action in the window's copy, shows it at once and sends it to be saved.
/// A combination another action uses already is kept, flagged, and said.
fn set_shortcut(app: &App, index: i32, hotkey: Option<Hotkey>) {
    let Some(action) = usize::try_from(index).ok().and_then(|i| Action::ALL.get(i)).copied() else { return };
    let note = with_ui(|ui| {
        let shortcuts = &mut ui.last.settings.shortcuts;
        shortcuts.retain(|(a, _)| *a != action);
        if let Some(hotkey) = hotkey {
            shortcuts.push((action, hotkey));
            shortcuts.sort_by_key(|(a, _)| a.index());
        }
        show_shortcuts(app, &ui.models, &ui.last);
        let other = hotkey.and_then(|hotkey| ui.last.settings.shortcuts.iter().find(|(a, h)| *a != action && *h == hotkey).map(|(a, _)| *a));
        other.map(|other| trf!("Raccourci déjà assigné : {keys} sert aussi à « {group} · {action} ».", keys = hotkey.map(|h| h.display()).unwrap_or_default(), group = tr(GROUPS[other.group()]), action = other.label()))
    })
    .flatten();
    app.set_shortcut_note(note.unwrap_or_default().into());
    send(Command::SetShortcut(action, hotkey));
}

/// Changes the overlays' options in the window's copy, shows them and sends them to be saved.
fn edit_overlays(app: &App, change: impl FnOnce(&mut overlay::Settings)) {
    let settings = with_ui(|ui| {
        change(&mut ui.last.settings.overlays);
        show_shortcuts(app, &ui.models, &ui.last);
        ui.last.settings.overlays
    });
    if let Some(settings) = settings {
        send(Command::SetOverlays(settings));
    }
}

fn connect_shortcuts(app: &App) {
    let backend = app.global::<Backend>();

    let weak = app.as_weak();
    backend.on_shortcut_capture(move |_| {
        if let Some(app) = weak.upgrade() {
            app.set_shortcut_note(SharedString::default());
        }
    });

    // The key the user pressed: a modifier alone keeps the field waiting; a combination that
    // cannot be one says why and keeps it waiting too.
    let weak = app.as_weak();
    backend.on_shortcut_pressed(move |index, text, control, alt, shift, meta| {
        let Some(app) = weak.upgrade() else { return false };
        match keys::combination(&text, control, alt, shift, meta) {
            None => false,
            Some(Err(why)) => {
                app.set_shortcut_note(why.into());
                false
            }
            Some(Ok(hotkey)) => {
                set_shortcut(&app, index, Some(hotkey));
                true
            }
        }
    });

    let weak = app.as_weak();
    backend.on_clear_shortcut(move |index| {
        if let Some(app) = weak.upgrade() {
            set_shortcut(&app, index, None);
        }
    });

    let weak = app.as_weak();
    backend.on_set_overlays(move |on| {
        if let Some(app) = weak.upgrade() {
            edit_overlays(&app, |settings| settings.enabled = on);
        }
    });

    let weak = app.as_weak();
    backend.on_set_overlay_kind(move |kind, on| {
        if let Some(app) = weak.upgrade() {
            edit_overlays(&app, |settings| match kind {
                0 => settings.volume = on,
                1 => settings.mute = on,
                _ => settings.device = on,
            });
        }
    });

    let weak = app.as_weak();
    backend.on_set_overlay_position(move |index| {
        if let (Some(app), Some(position)) = (weak.upgrade(), usize::try_from(index).ok().and_then(|i| Position::ALL.get(i)).copied()) {
            edit_overlays(&app, |settings| settings.position = position);
        }
    });

    // A sample overlay where the options put it, from the window itself (the daemon may be
    // stopped); never in the layout check.
    backend.on_preview_overlay(move || {
        if READ_ONLY.with(Cell::get) {
            return;
        }
        let Some((position, level)) = with_ui(|ui| (ui.last.settings.overlays.position, ui.master.volume)) else { return };
        let _ = std::thread::Builder::new().name("aperçu superposition".into()).spawn(move || {
            let _ = overlay::preview(&overlay::Content::volume(overlay::Glyph::Master, tr("Master"), level, false), position, overlay::HOLD);
        });
    });
}

/// The streaming outputs' list as the settings page's field shows it.
fn sinks_text(sinks: &[String]) -> String {
    sinks.join(", ")
}

/// The "À propos" section: the version, the install folder, the daemon's state, process, memory
/// and last word in status.txt, and the memory of its microphone engine (the worker process).
fn about_view(snapshot: &Snapshot) -> AboutView {
    let about = &snapshot.about;
    let daemon = &snapshot.daemon;
    let folder = match &about.folder {
        Some(folder) => folder.display().to_string(),
        None => platinum::autostart::install_dir().map(|folder| trf!("{folder} (absent : Platinum n'est pas installé)", folder = folder.display())).unwrap_or_default(),
    };
    let summary = if daemon.summary.is_empty() { String::new() } else { format!(" — {}", daemon.summary) };
    AboutView {
        version: env!("CARGO_PKG_VERSION").into(),
        folder: folder.into(),
        can_open_folder: about.folder.is_some(),
        log: about.log.as_ref().map(|file| file.display().to_string()).unwrap_or_else(|| tr("Aucun journal pour l'instant").into()).into(),
        can_open_log: about.log.is_some(),
        state: format!("{}{summary}", capitalized(daemon.phase.label())).into(),
        pid: about.pid.map_or_else(|| "—".to_string(), |pid| pid.to_string()).into(),
        memory: settings::memory_text(about.memory).into(),
        engine_memory: settings::engine_memory_text(about.worker_pid, about.worker_memory).into(),
        updated: if about.updated.is_empty() { "—".into() } else { settings::stamp_text(&about.updated).into() },
    }
}

/// What the settings page shows, from `snapshot` (kept up to date whether it is open or not: it is
/// small). The streaming outputs' field is the user's while he types: it is filled when its
/// page or a section opens (`prepare_settings`), not here.
fn show_settings(app: &App, models: &Models, snapshot: &Snapshot) {
    let settings = &snapshot.settings;
    app.set_start_on_open(settings.start_on_open);
    app.set_start_with_windows(snapshot.start_with_windows);
    app.set_startup_blocked(snapshot.startup_blocked);
    app.set_installed(snapshot.installed);
    app.set_platinum_names(snapshot.platinum_names);
    app.set_language_index(settings::language_index(settings.language));
    app.set_mic_effects(settings.effects);
    app.set_mic_gain(settings.gain);
    sync(&models.fallback_outputs, snapshot.fallback_outputs.labels.iter().map(SharedString::from).collect());
    app.set_fallback_output_index(snapshot.fallback_outputs.current);
    sync(&models.fallback_inputs, snapshot.fallback_inputs.labels.iter().map(SharedString::from).collect());
    app.set_fallback_input_index(snapshot.fallback_inputs.current);
    app.set_follow_streaming(settings.follow_streaming);
    app.set_route_apps(settings.streaming_route_apps);
    app.set_sinks_saved(sinks_text(&settings.streaming_sinks).into());
    app.set_game_presets(settings.game_presets);
    app.set_arena_speakers(settings.arena_speakers);
    app.set_crash_ask(settings.crash_reports == Consent::Ask);
    let lines = settings::link_lines(&snapshot.game_links, |key| snapshot.catalog.entry(0, &PresetRef::parse(key)).map(|entry| entry.name.clone()));
    sync(&models.game_links, lines.into_iter().map(|line| LinkRow { exe: line.exe.into(), title: line.title.into(), preset: line.preset.into() }).collect());
    app.set_pending_restart(settings::pending_sentence(&snapshot.pending_restart).into());
    app.set_about(about_view(snapshot));
}

/// Opens the settings page at a section: from its button, its navigation, or a "Gestionnaire de
/// périphériques" line of the mixer's menus.
fn open_settings(app: &App, section: i32) {
    let section = section.clamp(0, settings::SECTIONS - 1);
    // Another section starts at its top.
    if section != app.get_settings_section() {
        app.set_settings_scroll(0.0);
    }
    app.set_settings_section(section);
    if section == settings::CRASHES {
        refresh_crashes(app);
    }
    open_page(app, settings::PAGE);
}

/// The crash reports' section: the reports, where an issue would go (or why none can be opened),
/// and the folder.
fn show_crashes(app: &App, reports: &[crash::Summary]) {
    let rows: Vec<CrashRow> = settings::crash_lines(reports).into_iter().map(|line| CrashRow { name: line.name.into(), date: line.date.into(), program: line.program.into(), reason: line.reason.into() }).collect();
    with_ui(|ui| sync(&ui.models.crash_rows, rows));
    let repository = crash::repository();
    app.set_crash_repository(settings::repository_sentence(repository).into());
    app.set_crash_repository_known(repository.is_some());
    app.set_crash_folder(crash::folder().map(|folder| folder.display().to_string()).unwrap_or_default().into());
}

/// The crash reports of this PC, read again (the layout check keeps its samples).
fn refresh_crashes(app: &App) {
    if !PREVIEW.with(|items| items.borrow().iter().any(|item| item.starts_with("crash"))) {
        show_crashes(app, &crash::list());
    }
}

/// After a crash, with `crash_reports = ask`: the question, once (the newest report not asked about
/// yet; the older ones are taken as asked about with it). Never in the layout check.
fn offer_crash_report(app: &App) {
    if config_consent() != Consent::Ask {
        return;
    }
    let reports = crash::list();
    let Some(report) = crash::to_offer(&reports, &crash::offered()) else { return };
    app.set_crash_question(settings::crash_question(report).into());
    app.set_crash_question_name(report.name.clone().into());
    app.set_asking(10);
    let names: Vec<String> = reports.iter().map(|r| r.name.clone()).collect();
    let _ = crash::mark_offered(&names);
}

fn config_consent() -> Consent {
    platinum::config::load().crash_reports
}

/// A report of the list, by file name: a file of the reports' folder and nothing else.
fn report_file(name: &str) -> Option<std::path::PathBuf> {
    if name.is_empty() || name.contains(['\\', '/', ':']) || !name.ends_with(".txt") {
        return None;
    }
    crash::folder().ok().map(|folder| folder.join(name)).filter(|file| file.is_file())
}

/// The audio devices whose names a report must not carry: those Windows lists and platinum.ini
/// remembers, as the last snapshot holds them.
fn device_names() -> Vec<crash::DeviceName> {
    with_ui(|ui| {
        let devices: Vec<Endpoint> = ui.last.priority.output_devices.iter().chain(ui.last.priority.input_devices.iter()).cloned().collect();
        crash::device_names(&devices, &ui.last.settings)
    })
    .unwrap_or_default()
}

/// Opens the send dialog on a report, anonymised (`crash::anonymize`).
fn send_crash_report(app: &App, name: &str) {
    let text = if READ_ONLY.with(Cell::get) {
        sample_crash_report()
    } else {
        match report_file(name).map(|file| crash::read(&file)) {
            Some(Ok(text)) => text,
            Some(Err(error)) => {
                send(Command::Notify(tr("Rapport de plantage illisible").into(), error));
                return;
            }
            None => return,
        }
    };
    let mut devices = device_names();
    if READ_ONLY.with(Cell::get) {
        // The sample's own headset, which this PC does not have.
        devices.push(crash::DeviceName { name: "Casque (AirPods de Jean)".into(), input: false });
    }
    let anonymized = crash::anonymize(&text, &crash::Identity::of_this_pc(devices));
    CRASH_TEXT.with(|slot| *slot.borrow_mut() = anonymized.clone());
    app.set_crash_text(anonymized.into());
    app.set_crash_note(SharedString::default());
    app.set_crash_sending(true);
}

fn connect_crashes(app: &App) {
    let backend = app.global::<Backend>();

    backend.on_crash_view(|name| {
        if let Some(file) = report_file(&name) {
            shell_open(file.into_os_string());
        }
    });

    let weak = app.as_weak();
    backend.on_crash_send(move |name| {
        if let Some(app) = weak.upgrade() {
            send_crash_report(&app, &name);
        }
    });

    let weak = app.as_weak();
    backend.on_crash_copy(move || {
        let Some(app) = weak.upgrade() else { return };
        if READ_ONLY.with(Cell::get) {
            return;
        }
        let text = CRASH_TEXT.with(|slot| slot.borrow().clone());
        let note = match clipboard::set_text(&text) {
            Ok(()) => tr("Le rapport anonymisé est dans le presse-papiers.").to_string(),
            Err(error) => trf!("Copie impossible : {error}", error = error),
        };
        app.set_crash_note(note.into());
    });

    // The browser does the sending, once the user clicks there: Platinum only opens the page.
    let weak = app.as_weak();
    backend.on_crash_github(move || {
        let (Some(app), Some(repository)) = (weak.upgrade(), crash::repository()) else { return };
        if READ_ONLY.with(Cell::get) {
            return;
        }
        let text = CRASH_TEXT.with(|slot| slot.borrow().clone());
        let link = crash::issue_link(repository, &text);
        let mut note = settings::sent_note(link.truncated).to_string();
        if link.truncated {
            if let Err(error) = clipboard::set_text(&text) {
                note = trf!("Le rapport était trop long pour un lien et n'a pas pu être copié ({error}) : utilisez « Copier le texte », puis collez-le dans l'issue.", error = error);
            }
        }
        app.set_crash_note(note.into());
        shell_open(link.url.into());
    });

    backend.on_crash_open_folder(|| {
        if let Ok(folder) = crash::folder() {
            if folder.is_dir() {
                shell_open(folder.into_os_string());
            }
        }
    });

    let weak = app.as_weak();
    backend.on_crash_clear(move || {
        let Some(app) = weak.upgrade() else { return };
        if READ_ONLY.with(Cell::get) {
            return;
        }
        if let Err(error) = crash::clear() {
            send(Command::Notify(tr("Rapports de plantage non effacés").into(), error));
        }
        refresh_crashes(&app);
    });

    let weak = app.as_weak();
    backend.on_crash_never_ask(move || {
        if let Some(app) = weak.upgrade() {
            change_setting(&app, Change::CrashReports(Consent::Off));
        }
    });
}

/// The layout check's sample report (a crash in the vendor's DLL, seen by the daemon).
fn sample_crash_report() -> String {
    let profile = std::env::var("USERPROFILE").unwrap_or_else(|_| r"C:\Users\Someone".into());
    format!(
        "Platinum crash report\nprogram: mic-worker\nversion: {}\nwindows: 10.0.26200\ntime: 2026-11-22 15:30:00\npid: 8124\nthread: 9120\nreason: access violation (0xC0000005) in Sonar.APO.dll+0x1A2B3\nrecorded by: daemon\n\ndetails:\n  exception address 0x7FFA1C21A2B3, flags 0x0\n  attempted to read address 0x18\n\nstack (module+offset):\n  Sonar.APO.dll+0x1A2B3\n  Sonar.APO.dll+0x9F10\n  platinum.exe+0x4C21E\n  KERNEL32.DLL+0x2DDA7\n  ntdll.dll+0x7EE4C\n\ndaemon.log, last 4 lines:\n  [20261122-152958] mic worker 8124: relaying Microphone (Razer Seiren Mini) into Microphone (Platinum) through the effects engine\n  [20261122-152959] keep-alive: stream opened on Casque (AirPods de Jean)\n  [20261122-152959] memory (hourly): private 61.2 MB, working set 70.4 MB, 612 handles, 24 threads, window open, 1 keep-alive stream; mic worker 8124: private 212.0 MB\n  [20261122-153000] settings read from {profile}\\Platinum\\platinum.ini\n\nminidump:\n  20261122-153000-mic-worker.dmp, kept on this PC only (never part of what is sent)\n",
        env!("CARGO_PKG_VERSION")
    )
}

/// The layout check's sample reports.
fn sample_crashes() -> Vec<crash::Summary> {
    let sample = |stamp: &str, program: &str, reason: &str| crash::Summary { file: std::path::PathBuf::new(), name: format!("{stamp}-{program}.txt"), stamp: stamp.into(), program: program.into(), reason: reason.into() };
    vec![
        sample("20261122-153000", crash::WORKER, "access violation (0xC0000005) in Sonar.APO.dll+0x1A2B3"),
        sample("20261121-091244", crash::WINDOW, "panic: called `Option::unwrap()` on a `None` value (at ui\\src\\engine.rs:812:40)"),
        sample("20261119-224501", crash::DAEMON, "panic: index out of bounds: the len is 4 but the index is 4 (at src\\mixer.rs:212:17)"),
    ]
}

/// The settings page as it opens, or changes section: no shortcut field waits for keys, and the
/// streaming outputs' field starts from what platinum.ini holds.
fn prepare_settings(app: &App) {
    app.set_shortcut_note(SharedString::default());
    with_ui(|ui| {
        app.set_sinks_text(sinks_text(&ui.last.settings.streaming_sinks).into());
        show_settings(app, &ui.models, &ui.last);
        show_devices(app, &ui.models, &ui.last);
        show_shortcuts(app, &ui.models, &ui.last);
    });
}

/// An option of platinum.ini changed on the settings page: changed in the window's copy with the
/// function the engine saves it with, shown at once, then sent.
fn change_setting(app: &App, change: Change) {
    with_ui(|ui| {
        change.apply(&mut ui.last.settings);
        show_settings(app, &ui.models, &ui.last);
    });
    send(Command::Set(change));
}

fn connect_settings(app: &App) {
    let backend = app.global::<Backend>();

    // The language: the window speaks it at once, then platinum.ini keeps it (the daemon reads it
    // there within 2 s).
    let weak = app.as_weak();
    backend.on_set_language(move |index| {
        let (Some(app), Some(choice)) = (weak.upgrade(), settings::language_choice(index)) else { return };
        with_ui(|ui| Change::Language(choice).apply(&mut ui.last.settings));
        speak(&app, choice.resolve());
        send(Command::Set(Change::Language(choice)));
    });

    let weak = app.as_weak();
    backend.on_open_settings(move |section| {
        if let Some(app) = weak.upgrade() {
            open_settings(&app, section);
        }
    });

    // The on/off options of platinum.ini, by their `SettingId`.
    let weak = app.as_weak();
    backend.on_set_switch(move |id, on| {
        let change = settings::Setting::from_index(id).and_then(|setting| Change::switch(setting, on));
        if let (Some(app), Some(change)) = (weak.upgrade(), change) {
            // Turned on, the offer is for the crashes to come: the reports already here are
            // listed on the page, and never asked about.
            if change == Change::CrashReports(Consent::Ask) && !READ_ONLY.with(Cell::get) {
                let names: Vec<String> = crash::list().into_iter().map(|report| report.name).collect();
                let _ = crash::mark_offered(&names);
            }
            change_setting(&app, change);
        }
    });

    // Sent when the slider is let go, not at every step of a drag.
    let weak = app.as_weak();
    backend.on_set_gain(move |gain| {
        if let Some(app) = weak.upgrade() {
            change_setting(&app, Change::Gain(gain));
        }
    });

    // 0: the output, 1: the input; the menus list the last snapshot's devices, read here.
    let weak = app.as_weak();
    backend.on_set_fallback(move |list, index| {
        let Some(app) = weak.upgrade() else { return };
        let picked = with_ui(|ui| {
            let menu = if list == 0 { &mut ui.last.fallback_outputs } else { &mut ui.last.fallback_inputs };
            let spec = menu.specs.get(usize::try_from(index).ok()?)?.clone();
            menu.current = index;
            Some(spec)
        })
        .flatten();
        if let Some(spec) = picked {
            change_setting(&app, if list == 0 { Change::FallbackOutput(spec) } else { Change::FallbackInput(spec) });
        }
    });

    let weak = app.as_weak();
    backend.on_set_channel_names(move |platinum| {
        if let Some(app) = weak.upgrade() {
            with_ui(|ui| {
                ui.last.platinum_names = platinum;
                show_settings(&app, &ui.models, &ui.last);
            });
        }
        send(Command::SetChannelNames(platinum));
    });

    // The field then shows the list as the file holds it.
    let weak = app.as_weak();
    backend.on_set_streaming_sinks(move |text| {
        let Some(app) = weak.upgrade() else { return };
        change_setting(&app, Change::StreamingSinks(text.to_string()));
        if let Some(saved) = with_ui(|ui| sinks_text(&ui.last.settings.streaming_sinks)) {
            app.set_sinks_text(saved.into());
        }
    });

    backend.on_open_log(|| {
        if let Some(file) = with_ui(|ui| ui.last.about.log.clone()).flatten() {
            shell_open(file.into_os_string());
        }
    });

    backend.on_open_folder(|| {
        if let Some(folder) = with_ui(|ui| ui.last.about.folder.clone()).flatten() {
            shell_open(folder.into_os_string());
        }
    });
}

fn show_daemon(app: &App, models: &Models, snapshot: &Snapshot) {
    app.set_daemon(daemon_state(&snapshot.daemon));
    app.set_echo_stale(snapshot.echo_stale);
    let newest = snapshot.notices.iter().rev().take(NOTICES_SHOWN).rev();
    sync(&models.notices, newest.map(|n| NoticeView { id: n.id as i32, text: n.text.as_str().into(), detail: n.detail.as_str().into() }).collect());
}

/// A channel plays to a real output that is plugged in: its test sounds can be heard.
fn channel_has_output(snapshot: &Snapshot, index: usize) -> bool {
    snapshot.channels.get(index).is_some_and(|c| c.readable && c.output_index > 0 && c.output_present)
}

/// The Chat channel's real output, when it has one plugged in: where the microphone test is heard
/// (the device Chat is redirected to, settings 269 and 270, as the last snapshot read them).
fn chat_output(snapshot: &Snapshot) -> Option<String> {
    let index = channel_index("chat")?;
    let chat = snapshot.channels.get(index).filter(|_| channel_has_output(snapshot, index))?;
    snapshot.output_ids.get(usize::try_from(chat.output_index).ok()?).cloned().flatten()
}

/// Whether a microphone is chosen (and plugged in), and whether the daemon relays one now: only
/// then does the virtual microphone carry anything to record.
fn microphone_state(snapshot: &Snapshot) -> (bool, bool) {
    let chosen = usize::try_from(snapshot.mic.current).ok().and_then(|i| snapshot.mic.inputs.get(i)).is_some_and(|m| m.present);
    let relayed = matches!(snapshot.daemon.phase, Phase::Running | Phase::Degraded);
    (chosen, relayed)
}

/// What a microphone test button asks the test sounds' thread for (0 "Enregistrer" or
/// "Arrêter", 1 "Écouter" or "Pause"), or why it cannot work, in GG's words.
fn mic_request(test: &tester::Status, snapshot: &Snapshot, button: i32) -> Result<tester::Request, &'static str> {
    match (button, test.mic) {
        (0, MicPhase::Recording) | (1, MicPhase::Playing) => Ok(tester::Request::Stop),
        (0, _) => {
            let (chosen, relayed) = microphone_state(snapshot);
            sounds::record_problem(chosen, relayed).map_or(Ok(tester::Request::Record), Err)
        }
        (_, phase) => {
            let output = chat_output(snapshot);
            match sounds::listen_problem(matches!(phase, MicPhase::Recorded | MicPhase::Playing), output.as_deref()) {
                Some(why) => Err(why),
                None => Ok(tester::Request::Listen(output.unwrap_or_default())),
            }
        }
    }
}

/// Works out again why the open channel tab's test sounds cannot be played (no output), and
/// whether GG's files are missing (looked for now), in which case generated sounds play.
fn refresh_sample_problem(ui: &mut Ui, page: i32) {
    let index = usize::try_from(page - 1).ok().filter(|&i| i < STRIPS);
    ui.sample_problem = index.and_then(|index| sounds::sample_problem(channel_has_output(&ui.last, index)));
    if !PREVIEW.with(|items| items.borrow().iter().any(|item| item == "test-missing")) {
        ui.sample_note = index.and_then(|index| sounds::missing_note(&sounds::missing(&sounds::sample_folder(), sounds::samples_for(CHANNELS[index].key))));
    }
}

/// The header's "Tester" row of the open tab: the channel's test sounds, or the microphone test.
/// A note the test sounds' thread left for this tab is shown unless the buttons are dimmed (the
/// reason they are wins).
fn show_tests(app: &App, ui: &Ui) {
    let Some(index) = usize::try_from(app.get_page() - 1).ok().filter(|&i| i < CHANNELS.len()) else { return };
    let key = CHANNELS[index].key;
    let test = &ui.test;
    let note_here = test.note.as_ref().filter(|(tab, _)| tab == key).map(|(_, text)| text.clone());
    if index == MIC {
        let (chosen, relayed) = microphone_state(&ui.last);
        let record = sounds::record_problem(chosen, relayed);
        let resting = matches!(test.mic, MicPhase::Idle | MicPhase::Recorded);
        let note = note_here.or_else(|| record.filter(|_| resting).map(str::to_string));
        app.set_mic_test(MicTest { phase: test.mic.code(), level: test.level, time: sounds::mic_time(test.mic, test.position, test.recorded).into(), can_record: record.is_none() });
        // The notes are French keys (sounds.rs) or were translated when written.
        app.set_test_note(tr(&note.unwrap_or_default()).into());
        return;
    }
    let playing = |sample: &sounds::Sample| test.playing.as_ref().is_some_and(|(tab, s)| tab == key && s == sample);
    sync(&ui.models.test_samples, sounds::samples_for(key).iter().map(|sample| TestSample { label: sample.label().into(), playing: playing(sample) }).collect());
    app.set_test_ready(ui.sample_problem.is_none());
    app.set_test_note(tr(&ui.sample_problem.clone().or(note_here).or_else(|| ui.sample_note.clone()).unwrap_or_default()).into());
}

/// Hands a request to the test sounds' thread, started by the first one. Never in the layout
/// check: nothing may be played or recorded there.
fn test_request(app: &App, request: tester::Request) {
    if READ_ONLY.with(Cell::get) {
        return;
    }
    TESTER.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            let (sender, receiver) = channel();
            let weak = app.as_weak();
            let spawned = std::thread::Builder::new().name("sons de test".into()).spawn(move || {
                tester::run(receiver, move |status| {
                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(app) = weak.upgrade() {
                            with_ui(|ui| {
                                ui.test = status;
                                show_tests(&app, ui);
                            });
                        }
                    });
                });
            });
            if spawned.is_ok() {
                *slot = Some(sender);
            }
        }
        if let Some(sender) = slot.as_ref() {
            let _ = sender.send(request);
        }
    });
}

fn connect_tests(app: &App) {
    let backend = app.global::<Backend>();

    // The sound playing stops; any other starts from the beginning (one at a time).
    let weak = app.as_weak();
    backend.on_play_sample(move |key, index| {
        let Some(app) = weak.upgrade() else { return };
        let Some(sample) = usize::try_from(index).ok().and_then(|i| sounds::samples_for(&key).get(i)).copied() else { return };
        let playing = with_ui(|ui| ui.test.playing.as_ref().is_some_and(|(tab, s)| tab.as_str() == key.as_str() && *s == sample)).unwrap_or(false);
        test_request(&app, if playing { tester::Request::Stop } else { tester::Request::Play(key.to_string(), sample) });
    });

    let weak = app.as_weak();
    backend.on_mic_test(move |button| {
        let Some(app) = weak.upgrade() else { return };
        let Some(asked) = with_ui(|ui| mic_request(&ui.test, &ui.last, button)) else { return };
        match asked {
            Ok(request) => test_request(&app, request),
            Err(why) => {
                with_ui(|ui| {
                    ui.test.note = Some((tester::MIC_KEY.to_string(), why.to_string()));
                    show_tests(&app, ui);
                });
            }
        }
    });
}

fn apply(app: &App, snapshot: Snapshot) {
    // Taken before the engine handled the user's last action: it would undo it on screen.
    if snapshot.handled < SENT.with(Cell::get) {
        return;
    }
    with_ui(|ui| {
        sync(&ui.models.outputs, snapshot.outputs.iter().map(SharedString::from).collect());
        show_mixer(app, &ui.models, &snapshot);
        show_microphone(app, &ui.models, &snapshot);
        show_strip_presets(app, &ui.models, &snapshot);
        show_tab(app, &ui.models, &snapshot);
        show_browse(app, &ui.models, &snapshot, ui.browse.as_ref());
        show_devices(app, &ui.models, &snapshot);
        show_shortcuts(app, &ui.models, &snapshot);
        show_settings(app, &ui.models, &snapshot);
        show_daemon(app, &ui.models, &snapshot);
        ui.master = snapshot.master.fader;
        ui.last = snapshot;
        refresh_sample_problem(ui, app.get_page());
        show_tests(app, ui);
    });
    if PREVIEW.with(|items| !items.borrow().is_empty()) {
        preview(app);
    }
}

fn open_page(app: &App, page: i32) {
    // The band editor belongs to the tab it was opened on.
    if page != app.get_page() {
        app.global::<EqEditor>().set_selected(-1);
    }
    // A shortcut field waiting for keys stops waiting once its section is left.
    app.set_shortcut_capturing(-1);
    app.set_page(page);
    if page == settings::PAGE {
        prepare_settings(app);
        return;
    }
    with_ui(|ui| {
        show_tab(app, &ui.models, &ui.last);
        refresh_sample_problem(ui, page);
        show_tests(app, ui);
    });
}

/// Redraws what shows a channel's preset (its strip, its tab, the browser) after a change made in
/// the window.
fn preset_changed(app: &App, ui: &Ui) {
    show_strip_presets(app, &ui.models, &ui.last);
    show_tab(app, &ui.models, &ui.last);
    show_browse(app, &ui.models, &ui.last, ui.browse.as_ref());
}

/// The window's copy of a channel takes `next` as the engine will write it: GG's Arena 7 rule
/// (`arena_rule`: platinum.ini's `arena_speakers`), then the noise reductions the module will hold
/// (`all`: a preset applied whole writes them; an edit, only when AI noise cancelling or their own
/// switch changes).
fn take_preset(channel: &mut ChannelData, mut next: Preset, all: bool, arena_rule: bool) {
    let Some(before) = channel.preset else { return };
    presets::arena_rule(&before, &mut next, channel.arena && arena_rule, all);
    if let (Preset::Capture(old), Preset::Capture(new)) = (&before, &next) {
        let changes = old.noise_canceling.enabled != new.noise_canceling.enabled || presets::expected_reductions(old) != presets::expected_reductions(new);
        if all || changes {
            channel.held_reductions = presets::expected_reductions(new);
        }
    }
    channel.preset = Some(next);
}

/// An edit kept with the preset it was made on, as the engine keeps it (`Presets::edited`).
fn keep_edit(channel: &mut ChannelData, catalog: &Catalog, index: usize) {
    let (Some(entry), Some(data)) = (catalog.entry(index, &channel.selection), channel.preset) else { return };
    if presets::same_content(&data, &entry.data) {
        channel.edited.remove(&entry.key);
    } else {
        channel.edited.set(&entry.key, data);
    }
}

/// Applies an edit to the window's copy of a channel's preset and shows it, then sends it.
fn edit(app: &App, key: &str, change: Edit) {
    let Some(index) = channel_index(key) else { return };
    let changed = with_ui(|ui| {
        let rule = ui.last.settings.arena_speakers;
        let Some(channel) = ui.last.channels.get_mut(index) else { return false };
        let Some(mut next) = channel.preset else { return false };
        if !presets::apply(&mut next, change) {
            return false;
        }
        take_preset(channel, next, false, rule);
        keep_edit(channel, &ui.last.catalog, index);
        preset_changed(app, ui);
        true
    });
    if changed == Some(true) {
        send(Command::Edit(key.into(), change));
    }
}

/// Shows a preset chosen at once, with the user's edits of it (the engine applies it), then sends
/// the choice.
fn select_preset(app: &App, key: &str, preset_key: &str) {
    let (Some(index), chosen) = (channel_index(key), PresetRef::parse(preset_key)) else { return };
    let ready = with_ui(|ui| {
        let Some(entry) = ui.last.catalog.entry(index, &chosen).cloned() else { return false };
        let rule = ui.last.settings.arena_speakers;
        let Some(channel) = ui.last.channels.get_mut(index).filter(|c| c.preset.is_some()) else { return false };
        let data = channel.edited.get(&entry.key).copied().unwrap_or(entry.data);
        let next = presets::keeping_effects(data, channel.preset.as_ref());
        take_preset(channel, next, true, rule);
        channel.selection = chosen.clone();
        channel.selection_name = entry.name;
        // A game's preset on Gaming ends with a choice made here (the engine records it).
        channel.game.clear();
        preset_changed(app, ui);
        true
    });
    if ready == Some(true) {
        send(Command::SelectPreset(key.into(), chosen));
    }
}

/// Moves a favourite at once (in the window's copy of the presets), then sends it.
fn move_favorite(app: &App, key: &str, preset_key: &str, to: i32) {
    let (Some(index), chosen, Ok(to)) = (channel_index(key), PresetRef::parse(preset_key), usize::try_from(to)) else { return };
    let moved = with_ui(|ui| {
        let Some(shelf) = Arc::make_mut(&mut ui.last.catalog).shelves.get_mut(index) else { return false };
        let moved = catalog::move_in(&mut shelf.favorites, &chosen, to);
        if moved {
            preset_changed(app, ui);
        }
        moved
    });
    if moved == Some(true) {
        send(Command::MoveFavorite(key.into(), chosen, to));
    }
}

/// Stars or unstars a preset at once (in the window's copy of the presets), then sends it.
fn toggle_favorite(app: &App, key: &str, preset_key: &str) {
    let (Some(index), chosen) = (channel_index(key), PresetRef::parse(preset_key)) else { return };
    let ready = with_ui(|ui| {
        let Some(shelf) = Arc::make_mut(&mut ui.last.catalog).shelves.get_mut(index) else { return false };
        if let Some(position) = shelf.favorites.iter().position(|k| *k == chosen) {
            shelf.favorites.remove(position);
        } else if shelf.favorites.len() < FAVORITES && shelf.entry(&chosen).is_some() {
            shelf.favorites.push(chosen.clone());
        } else {
            return false;
        }
        preset_changed(app, ui);
        true
    });
    if ready == Some(true) {
        send(Command::ToggleFavorite(key.into(), chosen));
    }
}

fn open_browse(app: &App, key: &str) {
    let Some(channel) = channel_index(key) else { return };
    with_ui(|ui| {
        ui.browse = Some(Browse { channel, filter: Filter::All, query: String::new() });
        show_browse(app, &ui.models, &ui.last, ui.browse.as_ref());
    });
    app.set_browse_open(true);
}

/// Opens the name dialog with a name to start from: a free one for a new preset or a copy, the
/// current one for a rename.
fn ask_name(app: &App, key: &str, purpose: i32) {
    let Some(index) = channel_index(key) else { return };
    let initial = with_ui(|ui| {
        let data = ui.last.channels.get(index).cloned().unwrap_or_default();
        let name = preset_name(&ui.last.catalog, index, &data);
        let shelf = ui.last.catalog.shelf(index).cloned().unwrap_or_default();
        match purpose {
            3 => name,
            2 if data.selection != PresetRef::Custom => shelf.unique_name(&name),
            _ => shelf.unique_name(tr("Mon préréglage")),
        }
    });
    app.set_naming_key(key.into());
    app.set_naming_initial(initial.unwrap_or_default().into());
    app.set_naming(purpose);
}

/// Why a name typed in the name dialog cannot be used ("" when it can). Called while Slint
/// evaluates a binding: the window's state may be borrowed then, and the name is accepted for now.
fn name_problem(key: &str, purpose: i32, name: &str) -> String {
    let Some(index) = channel_index(key) else { return String::new() };
    UI.with(|ui| {
        let Ok(ui) = ui.try_borrow() else { return String::new() };
        let Some(ui) = ui.as_ref() else { return String::new() };
        let Some(shelf) = ui.last.catalog.shelf(index) else { return String::new() };
        let renaming = match (purpose, ui.last.channels.get(index).map(|c| &c.selection)) {
            (3, Some(PresetRef::Own(old))) => Some(old.as_str()),
            _ => None,
        };
        shelf.name_problem(name, renaming).unwrap_or("").to_string()
    })
}

/// Export and Import: Windows' own dialogs, on a thread of their own; the file read comes back to
/// the window's thread, then to the engine. Never in the layout check.
fn file_command(key: &str, command: &str) {
    let Some(index) = channel_index(key) else { return };
    if READ_ONLY.with(Cell::get) {
        return;
    }
    let owner = filedialog::owner();
    let title = tr(CHANNELS[index].title);
    match command {
        "export" => {
            let Some((name, text)) = with_ui(|ui| {
                let data = ui.last.channels.get(index)?;
                let name = preset_name(&ui.last.catalog, index, data);
                Some((name.clone(), persist::export_text(&name, ROLES[index], data.preset.as_ref()?)))
            })
            .flatten() else {
                return;
            };
            let suggested = persist::file_name(&format!("{title} - {name}.json"));
            filedialog::save(owner, trf!("Exporter le préréglage de {channel}", channel = title), suggested, text, |outcome| {
                if let Some(Err(error)) = outcome {
                    let _ = slint::invoke_from_event_loop(move || send(Command::Notify(tr("Préréglage non exporté").into(), error)));
                }
            });
        }
        "import" => {
            let key = key.to_string();
            filedialog::open(owner, trf!("Importer un préréglage pour {channel}", channel = title), move |outcome| {
                let command = match outcome {
                    Some(Ok((path, text))) => Command::ImportPreset(key, path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(), text),
                    Some(Err(error)) => Command::Notify(tr("Préréglage non importé").into(), error),
                    None => return,
                };
                let _ = slint::invoke_from_event_loop(move || send(command));
            });
        }
        _ => {}
    }
}

/// "Réinitialiser": the chosen preset's own settings at once (its edits dropped), then the engine
/// writes them.
fn reset_preset(app: &App, key: &str) {
    let Some(index) = channel_index(key) else { return };
    with_ui(|ui| {
        let Some(entry) = ui.last.catalog.entry(index, &ui.last.channels.get(index).map(|c| c.selection.clone()).unwrap_or_default()).cloned() else { return };
        let rule = ui.last.settings.arena_speakers;
        if let Some(channel) = ui.last.channels.get_mut(index).filter(|c| c.preset.is_some()) {
            channel.edited.remove(&entry.key);
            let next = presets::keeping_effects(entry.reference, channel.preset.as_ref());
            take_preset(channel, next, true, rule);
            preset_changed(app, ui);
        }
    });
    send(Command::ResetPreset(key.into()));
}

/// A card's "Réinitialiser": its amounts from the chosen preset's own settings (the neutral
/// preset's for settings that come from no preset), as an edit.
fn reset_card(app: &App, key: &str, card: CardKind) {
    let Some(index) = channel_index(key) else { return };
    let reference = with_ui(|ui| {
        let selection = ui.last.channels.get(index).map(|c| c.selection.clone()).unwrap_or_default();
        ui.last.catalog.entry(index, &selection).map_or_else(|| flat(ROLES[index]), |e| e.reference)
    });
    if let Some(reference) = reference {
        edit(app, key, Edit::ResetCard(card, reference));
    }
}

/// An equalizer filter as the chosen preset has it (GG's band "Réinitialiser" copies it from the
/// preset's `defaultData`): the neutral preset's for settings that come from no preset.
fn reference_filter(ui: &Ui, index: usize, band: usize) -> Option<platinum::preset::Filter> {
    let selection = ui.last.channels.get(index).map(|c| c.selection.clone()).unwrap_or_default();
    let reference = ui.last.catalog.entry(index, &selection).map_or_else(|| flat(ROLES[index]), |e| e.reference);
    reference.equalizer().filters.get(band).copied()
}

/// The graph's "+": turns on the first filter that is off, where the pointer is on the curve;
/// returns it, for the graph to open its editor, or -1 when all ten are on.
fn add_band(app: &App, key: &str, x: f32, gain: f32) -> i32 {
    let Some(index) = channel_index(key) else { return -1 };
    let free = with_ui(|ui| ui.last.channels.get(index).and_then(|c| c.preset.as_ref()).and_then(presets::first_free_band)).flatten();
    let Some(band) = free else { return -1 };
    edit(app, key, Edit::Band(band, BandEdit::Add(eq::x_to_frequency(f64::from(x)), f64::from(gain))));
    band as i32
}

/// A speaker put back where the chosen preset has it (or a standard 7.1 layout).
fn speaker_reset(app: &App, key: &str, speaker: usize) {
    let Some(index) = channel_index(key) else { return };
    let angle = with_ui(|ui| {
        let selection = ui.last.channels.get(index).map(|c| c.selection.clone()).unwrap_or_default();
        match ui.last.catalog.entry(index, &selection).map(|e| e.reference) {
            Some(Preset::Render(r)) => r.speakers.get(speaker).map(|s| s.position),
            _ => None,
        }
    })
    .flatten()
    .or_else(|| presets::STANDARD_ANGLES.get(speaker).copied());
    if let Some(angle) = angle {
        edit(app, key, Edit::Speaker(speaker, angle));
    }
}

/// Shows a strip change at once, before the engine confirms it (the microphone's strip included).
fn edit_column(app: &App, models: &Models, key: &str, change: impl Fn(&mut ColumnView)) {
    edit_rows(&models.columns, |c| c.key == key, &change);
    if key == CHANNELS[MIC].key {
        let mut mic = app.get_mic();
        change(&mut mic);
        app.set_mic(mic);
    }
}

/// Where a pill dropped on `target` will be listed once the engine has routed it: an application
/// sent back to the default device (the Master, 0) plays through the channel that device is while
/// Platinum runs, and a recording one records the virtual microphone.
fn landing(snapshot: &Snapshot, target: i32, capture: bool) -> i32 {
    match (target, capture) {
        (0, false) => snapshot.default_channel,
        (0, true) if snapshot.default_capture_mic => MIC_TARGET,
        _ => target,
    }
}

/// Moves a pill at once to where it will be listed (0: the Master's list, `MIC_TARGET`: the
/// Micro strip's).
fn move_pill(models: &Models, snapshot: &Snapshot, key: &str, target: i32) {
    let lists: Vec<&Rc<VecModel<AppPill>>> = std::iter::once(&models.master_apps).chain(models.column_apps.iter()).chain(std::iter::once(&models.mic_apps)).collect();
    let mut moved = None;
    for list in &lists {
        if let Some(index) = (0..list.row_count()).find(|&i| list.row_data(i).is_some_and(|p| p.key == key)) {
            moved = list.row_data(index);
            list.remove(index);
        }
    }
    let Some(mut pill) = moved else { return };
    let place = landing(snapshot, target, pill.capture);
    let Some(destination) = usize::try_from(place).ok().and_then(|i| lists.get(i)) else { return };
    // Its real device no longer applies once it goes through a channel.
    if place > 0 {
        pill.device = SharedString::default();
    }
    // A new route: the warning of the last one no longer applies.
    pill.refused = false;
    destination.push(pill);
}

fn connect_mixer(app: &App) {
    let backend = app.global::<Backend>();

    let weak = app.as_weak();
    backend.on_open_page(move |page| {
        if let Some(app) = weak.upgrade() {
            open_page(&app, page);
        }
    });

    let weak = app.as_weak();
    backend.on_set_volume(move |key, value| {
        if let Some(app) = weak.upgrade() {
            with_ui(|ui| {
                edit_column(&app, &ui.models, &key, |c| c.volume = value);
                // A channel raised above the Master raises it, as the engine will do.
                if let Some(index) = channel_index(&key).filter(|&i| i < STRIPS) {
                    let volumes: [f32; STRIPS] = std::array::from_fn(|i| ui.models.columns.row_data(i).map_or(0.0, |c| c.volume));
                    ui.master.channel_moved(index, value, &volumes);
                    app.set_master_volume(ui.master.volume);
                }
            });
        }
        send(Command::SetVolume(key.into(), value));
    });

    let weak = app.as_weak();
    backend.on_set_muted(move |key, on| {
        if let Some(app) = weak.upgrade() {
            with_ui(|ui| {
                edit_column(&app, &ui.models, &key, |c| c.muted = on);
                if !on && ui.master.muted && key.as_str() != CHANNELS[MIC].key {
                    ui.master.muted = false;
                    app.set_master_muted(false);
                }
            });
        }
        send(Command::SetMuted(key.into(), on));
    });

    let weak = app.as_weak();
    backend.on_set_output(move |key, index| {
        let Some(app) = weak.upgrade() else { return };
        let Some(device) = with_ui(|ui| ui.last.output_ids.get(index.max(0) as usize).cloned()).flatten() else { return };
        with_ui(|ui| edit_column(&app, &ui.models, &key, |c| c.output_index = index));
        if app.get_channel_key() == key {
            app.set_channel_output_index(index);
        }
        send(Command::SetOutput(key.into(), device));
    });

    let weak = app.as_weak();
    backend.on_set_master_output(move |index| {
        let Some(app) = weak.upgrade() else { return };
        let Some(device) = with_ui(|ui| ui.last.output_ids.get(index.max(0) as usize).cloned()).flatten() else { return };
        app.set_master_output_index(index);
        with_ui(|ui| edit_rows(&ui.models.columns, |_| true, |c| c.output_index = index));
        if CHANNELS.iter().take(STRIPS).any(|c| app.get_channel_key() == c.key) {
            app.set_channel_output_index(index);
        }
        send(Command::SetMasterOutput(device));
    });

    let weak = app.as_weak();
    backend.on_set_master_volume(move |value| {
        if let Some(app) = weak.upgrade() {
            app.set_master_volume(value);
            with_ui(|ui| {
                let volumes = ui.master.set_volume(value);
                for (index, volume) in volumes.into_iter().enumerate() {
                    edit_rows(&ui.models.columns, |c| c.key == CHANNELS[index].key, |c| c.volume = volume);
                }
            });
        }
        send(Command::SetMasterVolume(value));
    });

    let weak = app.as_weak();
    backend.on_set_master_muted(move |on| {
        if let Some(app) = weak.upgrade() {
            app.set_master_muted(on);
            with_ui(|ui| {
                let now: [bool; STRIPS] = std::array::from_fn(|i| ui.models.columns.row_data(i).is_some_and(|c| c.muted));
                let mutes = if on { ui.master.mute(&now) } else { ui.master.unmute() };
                for (index, muted) in mutes.into_iter().enumerate() {
                    edit_rows(&ui.models.columns, |c| c.key == CHANNELS[index].key, |c| c.muted = muted);
                }
            });
        }
        send(Command::SetMasterMuted(on));
    });

    let weak = app.as_weak();
    backend.on_set_device_level(move |value| {
        if let Some(app) = weak.upgrade() {
            app.set_master_level(value);
        }
        send(Command::SetDeviceLevel(value));
    });

    let weak = app.as_weak();
    backend.on_set_mic_volume(move |value| {
        if let Some(app) = weak.upgrade() {
            app.set_mic_volume(value);
        }
        send(Command::SetMicVolume(value));
    });

    let weak = app.as_weak();
    backend.on_set_mic_muted(move |on| {
        if let Some(app) = weak.upgrade() {
            app.set_mic_muted(on);
        }
        send(Command::SetMicMuted(on));
    });

    let weak = app.as_weak();
    backend.on_choose_microphone(move |index| {
        let Some(app) = weak.upgrade() else { return };
        let Some(id) = with_ui(|ui| ui.last.mic.inputs.get(index.max(0) as usize).map(|m| m.id.clone())).flatten() else { return };
        app.set_mic_current(index);
        send(Command::ChooseMicrophone(id));
    });

    // A microphone picked for the confirmation: resolved now, while the list on screen is the one
    // the user picked from.
    backend.on_microphone_picked(move |index| {
        with_ui(|ui| ui.asked_microphone = ui.last.mic.inputs.get(index.max(0) as usize).map(|m| m.id.clone()));
    });

    let weak = app.as_weak();
    backend.on_confirm_microphone(move || {
        let Some(id) = with_ui(|ui| ui.asked_microphone.take()).flatten() else { return };
        if let Some(app) = weak.upgrade() {
            if let Some(index) = with_ui(|ui| ui.last.mic.inputs.iter().position(|m| m.id == id)).flatten() {
                app.set_mic_current(index as i32);
            }
        }
        send(Command::ChooseMicrophone(id));
    });

    let weak = app.as_weak();
    backend.on_set_chatmix(move |value| {
        if let Some(app) = weak.upgrade() {
            app.set_chatmix(value);
        }
        send(Command::SetChatMix(value));
    });

    backend.on_route_app(move |key, target| {
        with_ui(|ui| move_pill(&ui.models, &ui.last, &key, target));
        send(match key.strip_prefix(CAPTURE_KEY) {
            Some(exe) => Command::RouteCapture(exe.into(), usize::from(target == MIC_TARGET)),
            None => Command::Route(key.into(), target.max(0) as usize),
        });
    });

    backend.on_sync_echo(|| send(Command::SyncEcho));

    backend.on_open_app_volume(|| shell_open("ms-settings:apps-volume".into()));

    connect_streamer(app);
}

/// The stream mix's volumes of the strips, as shown.
fn stream_volumes(models: &Models) -> [f32; STRIPS] {
    std::array::from_fn(|i| models.columns.row_data(i).map_or(0.0, |c| c.stream_volume))
}

/// Streamer mode's controls: each change is shown at once, as the engine will make it (the Stream
/// master follows its channels as the Master does), then sent.
fn connect_streamer(app: &App) {
    let backend = app.global::<Backend>();

    // Asked once the user confirmed it (the dialog says what changes).
    let weak = app.as_weak();
    backend.on_set_streamer(move |on| {
        if let Some(app) = weak.upgrade() {
            app.set_streamer(on);
        }
        send(Command::SetMode(if on { Mode::Streamer } else { Mode::Classic }));
    });

    let weak = app.as_weak();
    backend.on_set_stream_volume(move |key, value| {
        if let Some(app) = weak.upgrade() {
            with_ui(|ui| {
                edit_column(&app, &ui.models, &key, |c| c.stream_volume = value);
                if let Some(index) = channel_index(&key).filter(|&i| i < STRIPS) {
                    let volumes = stream_volumes(&ui.models);
                    ui.master.stream.channel_moved(index, value, &volumes);
                    app.set_stream_master_volume(ui.master.stream.volume);
                }
            });
        }
        send(Command::SetStreamVolume(key.into(), value));
    });

    let weak = app.as_weak();
    backend.on_set_stream_muted(move |key, on| {
        if let Some(app) = weak.upgrade() {
            with_ui(|ui| {
                edit_column(&app, &ui.models, &key, |c| c.stream_muted = on);
                if !on && ui.master.stream.muted {
                    ui.master.stream.muted = false;
                    app.set_stream_master_muted(false);
                }
            });
        }
        send(Command::SetStreamMuted(key.into(), on));
    });

    let weak = app.as_weak();
    backend.on_set_stream_master_volume(move |value| {
        if let Some(app) = weak.upgrade() {
            app.set_stream_master_volume(value);
            with_ui(|ui| {
                let volumes = ui.master.stream.set_volume(value);
                for (index, volume) in volumes.into_iter().enumerate() {
                    edit_rows(&ui.models.columns, |c| c.key == CHANNELS[index].key, |c| c.stream_volume = volume);
                }
            });
        }
        send(Command::SetStreamMasterVolume(value));
    });

    let weak = app.as_weak();
    backend.on_set_stream_master_muted(move |on| {
        if let Some(app) = weak.upgrade() {
            app.set_stream_master_muted(on);
            with_ui(|ui| {
                let now: [bool; STRIPS] = std::array::from_fn(|i| ui.models.columns.row_data(i).is_some_and(|c| c.stream_muted));
                let mutes = if on { ui.master.stream.mute(&now) } else { ui.master.stream.unmute() };
                for (index, muted) in mutes.into_iter().enumerate() {
                    edit_rows(&ui.models.columns, |c| c.key == CHANNELS[index].key, |c| c.stream_muted = muted);
                }
            });
        }
        send(Command::SetStreamMasterMuted(on));
    });

    // 0: the personal mix, 1: the stream mix.
    let weak = app.as_weak();
    backend.on_set_inclusion(move |key, mix, on| {
        let mix = if mix == 0 { Mix::Personal } else { Mix::Stream };
        if let Some(app) = weak.upgrade() {
            with_ui(|ui| {
                edit_column(&app, &ui.models, &key, |c| match mix {
                    Mix::Personal => c.in_personal = on,
                    Mix::Stream => c.in_stream = on,
                });
                if let Some(index) = channel_index(&key).filter(|&i| i < STRIPS) {
                    let inclusion = match mix {
                        Mix::Personal => &mut ui.last.streamer.personal_mix,
                        Mix::Stream => &mut ui.last.streamer.stream_mix,
                    };
                    inclusion[index] = on;
                }
            });
        }
        send(Command::SetInclusion(key.into(), mix, on));
    });

    // The menus list the last snapshot's devices, which is what is read here.
    let weak = app.as_weak();
    backend.on_set_personal_output(move |index| {
        let Some(app) = weak.upgrade() else { return };
        let Some(device) = with_ui(|ui| ui.last.output_ids.get(index.max(0) as usize).cloned()).flatten() else { return };
        app.set_personal_output_index(index);
        send(Command::SetPersonalDevice(device));
    });

    let weak = app.as_weak();
    backend.on_set_stream_output(move |index| {
        let Some(app) = weak.upgrade() else { return };
        let Some(choice) = with_ui(|ui| ui.last.streamer.choices.get(index.max(0) as usize).cloned()).flatten() else { return };
        app.set_stream_output_index(index);
        send(Command::SetStreamDevice(choice));
    });

    let weak = app.as_weak();
    backend.on_set_stream_level(move |value| {
        if let Some(app) = weak.upgrade() {
            app.set_stream_level(value);
        }
        send(Command::SetStreamDeviceLevel(value));
    });
}

/// Opens a file, a folder or a page of Windows' Settings as the Explorer would (Windows' own
/// per-application sound settings for GG's "App to device routing" button, the daemon's log and the
/// install folder for the settings page), on a thread of its own: Windows may take a moment. Never
/// in the layout check, which opens nothing.
fn shell_open(target: std::ffi::OsString) {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteW(window: *mut std::ffi::c_void, verb: *const u16, file: *const u16, parameters: *const u16, folder: *const u16, show: i32) -> isize;
    }
    const SW_SHOWNORMAL: i32 = 1;
    if READ_ONLY.with(Cell::get) {
        return;
    }
    let _ = std::thread::Builder::new().name("ouverture".into()).spawn(move || {
        let verb: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
        let file: Vec<u16> = target.encode_wide().chain(std::iter::once(0)).collect();
        unsafe {
            ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL);
        }
    });
}

fn connect_effects(app: &App) {
    let backend = app.global::<Backend>();

    // A value typed under a knob or a fader; the control writes it as a drag would.
    backend.on_typed_value(|text, minimum, maximum, step, percent| match presets::typed_value(&text, minimum.into(), maximum.into(), step.into(), percent) {
        Some(value) => TypedValue { ok: true, value: value as f32 },
        None => TypedValue { ok: false, value: 0.0 },
    });

    let weak = app.as_weak();
    backend.on_edit(move |key, code, value| {
        if let (Some(app), Some(field)) = (weak.upgrade(), Field::from_code(code)) {
            edit(&app, &key, Edit::Set(field, f64::from(value)));
        }
    });

    connect_equalizer(app);

    let weak = app.as_weak();
    backend.on_speaker_moved(move |key, speaker, angle| {
        if let Some(app) = weak.upgrade() {
            edit(&app, &key, Edit::Speaker(speaker.max(0) as usize, f64::from(angle)));
        }
    });

    let weak = app.as_weak();
    backend.on_speaker_reset(move |key, speaker| {
        if let Some(app) = weak.upgrade() {
            speaker_reset(&app, &key, speaker.max(0) as usize);
        }
    });

    let weak = app.as_weak();
    backend.on_reset_card(move |key, card| {
        if let (Some(app), Some(card)) = (weak.upgrade(), CardKind::from_code(card)) {
            reset_card(&app, &key, card);
        }
    });

    let weak = app.as_weak();
    backend.on_set_voice_zones(move |key, on| {
        let Some(index) = channel_index(&key) else { return };
        if let Some(app) = weak.upgrade() {
            app.set_eq_voice_zones(on);
        }
        with_ui(|ui| {
            if let Some(slot) = ui.last.master.fader.voice_zones.get_mut(index) {
                *slot = on;
            }
        });
        send(Command::SetVoiceZones(key.into(), on));
    });
}

/// The equalizer graph and its band editor: each change is an edit of the channel's preset, like
/// any effect card's.
fn connect_equalizer(app: &App) {
    let editor = app.global::<EqEditor>();
    let band_edit = |app: &App, key: &str, band: i32, change: BandEdit| {
        if let Ok(band) = usize::try_from(band) {
            edit(app, key, Edit::Band(band, change));
        }
    };

    let weak = app.as_weak();
    editor.on_moved(move |key, band, x, gain| {
        if let Some(app) = weak.upgrade() {
            band_edit(&app, &key, band, BandEdit::Move(eq::x_to_frequency(f64::from(x)), f64::from(gain)));
        }
    });

    let weak = app.as_weak();
    editor.on_q_changed(move |key, band, q| {
        if let Some(app) = weak.upgrade() {
            band_edit(&app, &key, band, BandEdit::Q(f64::from(q)));
        }
    });

    let weak = app.as_weak();
    editor.on_typed(move |key, band, value, text| {
        let (Some(app), Some(change)) = (weak.upgrade(), BandValue::from_code(value).and_then(|v| v.parse(&text))) else { return };
        band_edit(&app, &key, band, change);
    });

    let weak = app.as_weak();
    editor.on_kind_chosen(move |key, band, kind| {
        let (Some(app), Some((kind, _))) = (weak.upgrade(), usize::try_from(kind).ok().and_then(|k| presets::FILTER_TYPES.get(k))) else { return };
        band_edit(&app, &key, band, BandEdit::Kind(*kind));
    });

    let weak = app.as_weak();
    editor.on_enabled_changed(move |key, band, on| {
        if let Some(app) = weak.upgrade() {
            band_edit(&app, &key, band, BandEdit::Enabled(on));
        }
    });

    let weak = app.as_weak();
    editor.on_reset(move |key, band| {
        let (Some(app), Some(index), Ok(slot)) = (weak.upgrade(), channel_index(&key), usize::try_from(band)) else { return };
        if let Some(filter) = with_ui(|ui| reference_filter(ui, index, slot)).flatten() {
            band_edit(&app, &key, band, BandEdit::Reset(filter));
        }
    });

    let weak = app.as_weak();
    editor.on_add(move |key, x, gain| weak.upgrade().map_or(-1, |app| add_band(&app, &key, x, gain)));
}

fn connect_presets(app: &App) {
    let backend = app.global::<Backend>();

    let weak = app.as_weak();
    backend.on_select_preset(move |key, preset| {
        if let Some(app) = weak.upgrade() {
            select_preset(&app, &key, &preset);
        }
    });

    let weak = app.as_weak();
    backend.on_toggle_favorite(move |key, preset| {
        if let Some(app) = weak.upgrade() {
            toggle_favorite(&app, &key, &preset);
        }
    });

    let weak = app.as_weak();
    backend.on_move_favorite(move |key, preset, to| {
        if let Some(app) = weak.upgrade() {
            move_favorite(&app, &key, &preset, to);
        }
    });

    let weak = app.as_weak();
    backend.on_open_browse(move |key| {
        if let Some(app) = weak.upgrade() {
            open_browse(&app, &key);
        }
    });

    let weak = app.as_weak();
    backend.on_close_browse(move || {
        with_ui(|ui| ui.browse = None);
        if let Some(app) = weak.upgrade() {
            app.set_browse_open(false);
        }
    });

    let weak = app.as_weak();
    backend.on_browse_filter(move |index| {
        let Some(app) = weak.upgrade() else { return };
        with_ui(|ui| {
            if let Some(browse) = ui.browse.as_mut() {
                browse.filter = Filter::from_index(index);
            }
            show_browse(&app, &ui.models, &ui.last, ui.browse.as_ref());
        });
    });

    let weak = app.as_weak();
    backend.on_browse_search(move |text| {
        let Some(app) = weak.upgrade() else { return };
        with_ui(|ui| {
            if let Some(browse) = ui.browse.as_mut() {
                browse.query = text.to_string();
            }
            show_browse(&app, &ui.models, &ui.last, ui.browse.as_ref());
        });
    });

    let weak = app.as_weak();
    backend.on_preset_command(move |key, command| match command.as_str() {
        "reset" => {
            if let Some(app) = weak.upgrade() {
                reset_preset(&app, &key);
            }
        }
        "save" => send(Command::SavePreset(key.into())),
        other => file_command(&key, other),
    });

    let weak = app.as_weak();
    backend.on_ask_name(move |key, purpose| {
        if let Some(app) = weak.upgrade() {
            ask_name(&app, &key, purpose);
        }
    });

    backend.on_name_problem(|key, purpose, name| name_problem(&key, purpose, &name).into());

    backend.on_name_chosen(|key, purpose, name| {
        let name = name.trim().to_string();
        send(if purpose == 3 { Command::RenamePreset(key.into(), name) } else { Command::SavePresetAs(key.into(), name) });
    });

    backend.on_delete_preset(|key| send(Command::DeletePreset(key.into())));

    let weak = app.as_weak();
    backend.on_ask_link(move |key| {
        if let Some(app) = weak.upgrade() {
            ask_link(&app, &key);
        }
    });

    let weak = app.as_weak();
    backend.on_link_game(move |index| {
        if let Some(app) = weak.upgrade() {
            link_game(&app, index);
        }
    });

    let weak = app.as_weak();
    backend.on_unlink_game(move |exe| {
        if let Some(app) = weak.upgrade() {
            unlink_game(&app, &exe);
        }
    });
}

/// "Associer au jeu en cours…": lists the programs running now (those with a window, then those
/// with sound) for the preset Gaming uses, and opens the dialog. Reads only.
fn ask_link(app: &App, key: &str) {
    if channel_index(key) != Some(0) {
        return;
    }
    let windows: Vec<(String, String)> = if READ_ONLY.with(Cell::get) { Vec::new() } else { windowed_programs() };
    let opened = with_ui(|ui| {
        let data = ui.last.channels.first().cloned().unwrap_or_default();
        if ui.last.catalog.entry(0, &data.selection).is_none() {
            return false;
        }
        let sounding: Vec<String> = ui.last.apps.iter().map(|a| a.exe.clone()).collect();
        let rows = program_rows(&windows, &sounding, &ui.last.game_links);
        show_link_rows(app, ui, data.selection.clone(), preset_name(&ui.last.catalog, 0, &data), rows);
        true
    });
    if opened == Some(true) {
        app.set_linking(true);
    }
}

/// Fills the link dialog's list.
fn show_link_rows(app: &App, ui: &mut Ui, target: PresetRef, name: String, rows: Vec<(String, String, String)>) {
    sync(&ui.models.link_apps, rows.iter().map(|(exe, title, detail)| RunningApp { exe: exe.as_str().into(), title: title.as_str().into(), detail: detail.as_str().into() }).collect());
    app.set_link_preset(name.into());
    ui.linking = Some((target, rows.into_iter().map(|(exe, title, _)| (exe, title)).collect()));
}

/// The programs with a window on screen, by executable and window title.
fn windowed_programs() -> Vec<(String, String)> {
    let table = platinum::routing::all_processes().unwrap_or_default();
    platinum::gamescan::windowed().into_iter().filter_map(|w| table.iter().find(|(pid, _)| *pid == w.pid).map(|(_, exe)| (exe.clone(), w.title))).collect()
}

/// A program picked in the link dialog: linked at once in the window's copy, then sent.
fn link_game(app: &App, index: i32) {
    let picked = with_ui(|ui| {
        let (target, rows) = ui.linking.take()?;
        let (exe, title) = rows.get(usize::try_from(index).ok()?)?.clone();
        let preset_name = ui.last.catalog.entry(0, &target).map(|e| e.name.clone()).unwrap_or_default();
        games::set_link(&mut ui.last.game_links, UserLink { exe: exe.clone(), title: title.clone(), preset: target.key(), preset_name });
        show_tab(app, &ui.models, &ui.last);
        Some((target, exe, title))
    })
    .flatten();
    if let Some((target, exe, title)) = picked {
        send(Command::LinkGame(target, exe, title));
    }
}

/// "Dissocier" (the preset's "…" menu, the settings page): the program's link removed at once in
/// the window's copy, then sent.
fn unlink_game(app: &App, exe: &str) {
    with_ui(|ui| {
        games::remove_link(&mut ui.last.game_links, exe);
        show_tab(app, &ui.models, &ui.last);
        show_settings(app, &ui.models, &ui.last);
    });
    send(Command::UnlinkGame(exe.to_string()));
}

fn connect_daemon(app: &App) {
    let backend = app.global::<Backend>();

    let weak = app.as_weak();
    backend.on_start_daemon(move || {
        if let Some(app) = weak.upgrade() {
            daemon_under_way(&app, Phase::Starting);
        }
        send(Command::StartDaemon);
    });

    let weak = app.as_weak();
    backend.on_stop_daemon(move || {
        if let Some(app) = weak.upgrade() {
            daemon_under_way(&app, Phase::Stopping);
        }
        send(Command::StopDaemon);
    });

    let weak = app.as_weak();
    backend.on_set_start_with_windows(move |on| {
        if let Some(app) = weak.upgrade() {
            app.set_start_with_windows(on);
        }
        send(Command::SetStartWithWindows(on));
    });

    let weak = app.as_weak();
    backend.on_repair_audio(move || {
        if let Some(app) = weak.upgrade() {
            repair_audio(&app);
        }
    });

    // Asked once the user was told the sound cuts for a few seconds.
    let weak = app.as_weak();
    backend.on_restart_daemon(move || {
        if let Some(app) = weak.upgrade() {
            daemon_under_way(&app, Phase::Stopping);
        }
        send(Command::RestartDaemon);
    });

    backend.on_dismiss(move |id| {
        with_ui(|ui| {
            if let Some(index) = (0..ui.models.notices.row_count()).find(|&i| ui.models.notices.row_data(i).is_some_and(|n| n.id == id)) {
                ui.models.notices.remove(index);
            }
        });
        send(Command::Dismiss(id.max(0) as u64));
    });
}

/// Creates the models and hands them to the window, once.
fn install_models(app: &App) {
    let models = Models {
        columns: Rc::new(VecModel::default()),
        column_apps: (0..STRIPS).map(|_| Rc::new(VecModel::default())).collect(),
        mic_apps: Rc::new(VecModel::default()),
        column_favorites: CHANNELS.iter().map(|_| Rc::new(VecModel::default())).collect(),
        master_apps: Rc::new(VecModel::default()),
        outputs: Rc::new(VecModel::default()),
        stream_outputs: Rc::new(VecModel::default()),
        levels: Rc::new(VecModel::default()),
        mic_inputs: Rc::new(VecModel::default()),
        mic_hands_free: Rc::new(VecModel::default()),
        notices: Rc::new(VecModel::default()),
        bands: Rc::new(VecModel::default()),
        markers: Rc::new(VecModel::default()),
        curve_samples: Rc::new(VecModel::default()),
        speakers: Rc::new(VecModel::default()),
        favorites: Rc::new(VecModel::default()),
        browse_tiles: Rc::new(VecModel::default()),
        browse_counts: Rc::new(VecModel::default()),
        output_priority: Rc::new(VecModel::default()),
        input_priority: Rc::new(VecModel::default()),
        output_addable: Rc::new(VecModel::default()),
        input_addable: Rc::new(VecModel::default()),
        shortcut_rows: GROUPS.iter().map(|_| Rc::new(VecModel::default())).collect(),
        test_samples: Rc::new(VecModel::default()),
        link_apps: Rc::new(VecModel::default()),
        fallback_outputs: Rc::new(VecModel::default()),
        fallback_inputs: Rc::new(VecModel::default()),
        game_links: Rc::new(VecModel::default()),
        crash_rows: Rc::new(VecModel::default()),
    };
    app.set_fallback_outputs(shared(&models.fallback_outputs));
    app.set_fallback_inputs(shared(&models.fallback_inputs));
    app.set_game_links(shared(&models.game_links));
    app.set_crash_rows(shared(&models.crash_rows));
    app.set_sinks_default(sinks_text(&platinum::config::Config::default().streaming_sinks).into());
    app.set_test_samples(shared(&models.test_samples));
    app.set_link_apps(shared(&models.link_apps));
    app.set_columns(shared(&models.columns));
    app.set_master_apps(shared(&models.master_apps));
    app.set_mic_apps(shared(&models.mic_apps));
    app.set_outputs(shared(&models.outputs));
    app.set_stream_outputs(shared(&models.stream_outputs));
    app.set_levels(shared(&models.levels));
    app.set_mic_inputs(shared(&models.mic_inputs));
    app.set_mic_hands_free(shared(&models.mic_hands_free));
    app.set_notices(shared(&models.notices));
    app.set_eq_bands(shared(&models.bands));
    app.set_eq_markers(shared(&models.markers));
    let editor = app.global::<EqEditor>();
    editor.set_curve_samples(shared(&models.curve_samples));

    app.set_speakers(shared(&models.speakers));
    app.set_favorites(shared(&models.favorites));
    app.set_browse_tiles(shared(&models.browse_tiles));
    app.set_browse_counts(shared(&models.browse_counts));
    app.set_output_priority(shared(&models.output_priority));
    app.set_input_priority(shared(&models.input_priority));
    app.set_output_addable(shared(&models.output_addable));
    app.set_input_addable(shared(&models.input_addable));
    show_labels(app, &models);
    app.set_can_preview_overlay(!READ_ONLY.with(Cell::get));
    app.set_daemon(daemon_state(&DaemonView { summary: tr("Lecture de l'état…").into(), ..DaemonView::default() }));
    UI.with(|ui| *ui.borrow_mut() = Some(Ui { models, last: Snapshot::default(), master: Master::default(), browse: None, asked_microphone: None, test: tester::Status::default(), sample_problem: None, sample_note: None, linking: None }));
}

/// The texts Rust gives the window once rather than with each snapshot, in the current language:
/// the settings' notes, the equalizer's filter types, the shortcuts' groups, the overlay
/// positions, the language menu.
fn show_labels(app: &App, models: &Models) {
    app.set_setting_notes(ModelRc::new(VecModel::from(settings::notes().into_iter().map(SharedString::from).collect::<Vec<_>>())));
    let kinds: Vec<SharedString> = presets::FILTER_TYPES.iter().map(|(_, name)| SharedString::from(tr(name))).collect();
    app.global::<EqEditor>().set_kinds(ModelRc::new(VecModel::from(kinds)));
    let groups: Vec<ShortcutGroup> = GROUPS.iter().zip(GROUP_ICONS).zip(&models.shortcut_rows).map(|((title, icon), rows)| ShortcutGroup { title: tr(title).to_uppercase().into(), icon: icon.into(), rows: shared(rows) }).collect();
    app.set_shortcut_groups(ModelRc::new(VecModel::from(groups)));
    app.set_overlay_positions(ModelRc::new(VecModel::from(Position::ALL.iter().map(|p| SharedString::from(p.label())).collect::<Vec<_>>())));
    app.set_languages(ModelRc::new(VecModel::from(settings::language_menu(i18n::windows_language()).into_iter().map(SharedString::from).collect::<Vec<_>>())));
}

/// The language the window speaks: `PLATINUM_UI_LANG` when set (the layout check's captures in
/// each language), else platinum.ini's choice.
fn startup_language() -> Language {
    match std::env::var("PLATINUM_UI_LANG").ok().and_then(|code| Choice::parse(&code)) {
        Some(choice) => choice.resolve(),
        None => platinum::config::load().language.resolve(),
    }
}

/// Makes `language` the window's: the Rust side's texts (`i18n`), the .slint files' bundled
/// translations (French has its own empty catalog, for its decimal comma), and everything shown
/// redrawn from the last snapshot.
fn speak(app: &App, language: Language) {
    i18n::set_language(language);
    if let Err(error) = slint::select_bundled_translation(language.code()) {
        let _ = slint::select_bundled_translation("");
        eprintln!("platinum-ui: {error}");
    }
    with_ui(|ui| {
        show_labels(app, &ui.models);
        show_mixer(app, &ui.models, &ui.last);
        show_microphone(app, &ui.models, &ui.last);
        show_strip_presets(app, &ui.models, &ui.last);
        show_tab(app, &ui.models, &ui.last);
        show_browse(app, &ui.models, &ui.last, ui.browse.as_ref());
        show_devices(app, &ui.models, &ui.last);
        show_shortcuts(app, &ui.models, &ui.last);
        show_settings(app, &ui.models, &ui.last);
        show_daemon(app, &ui.models, &ui.last);
        show_tests(app, ui);
    });
}

/// Clears a drag whose pointer was lost: the press ended outside any element that could see it.
fn watch_drags(app: &App) -> slint::Timer {
    let timer = slint::Timer::default();
    let weak = app.as_weak();
    let mut still: Option<((f32, f32), Instant)> = None;
    timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), move || {
        let Some(app) = weak.upgrade() else { return };
        let drag = app.global::<Drag>();
        if drag.get_key().is_empty() {
            still = None;
            return;
        }
        let at = (drag.get_x(), drag.get_y());
        match still {
            Some((position, since)) if position == at => {
                if since.elapsed() >= DRAG_TIMEOUT {
                    drag.set_key(SharedString::default());
                    drag.set_from(-1);
                    drag.set_target(-1);
                    still = None;
                }
            }
            _ => still = Some((at, Instant::now())),
        }
    });
    timer
}

/// A panic would otherwise end the window without a trace (the release build aborts on panic and
/// has no console): each one is appended to %USERPROFILE%\Platinum\ui.log. The layout check writes
/// next to its capture instead, never in the install folder.
fn log_panics(capture: Option<&Path>) {
    let file = match capture {
        Some(capture) => capture.with_extension("panic.txt"),
        None => match platinum::autostart::install_dir() {
            Ok(folder) => folder.join("ui.log"),
            Err(_) => return,
        },
    };
    std::panic::set_hook(Box::new(move |info| {
        use std::io::Write;
        let thread = std::thread::current().name().unwrap_or("sans nom").to_string();
        let line = format!("[{}] platinum-ui : panique sur le fil « {thread} » : {info}\n", platinum::guard::stamp());
        if let Some(folder) = file.parent() {
            let _ = std::fs::create_dir_all(folder);
        }
        if let Ok(mut log) = std::fs::OpenOptions::new().create(true).append(true).open(&file) {
            let _ = log.write_all(line.as_bytes());
        }
    }));
}

/// The layout check can draw states the machine is not in, to look at them (comma-separated in
/// PLATINUM_UI_PREVIEW): `stopped`, `failed`, `starting`, `waiting`, `no-mic`, `remote` (the daemon;
/// `no-mic` runs without any microphone, the last one follows a remote streaming session on Steam
/// Streaming Speakers), `notices`,
/// `unreadable` (the Chat channel), `ask-stop`, `ask-mic`, `ask-bluetooth`, `ask-delete`,
/// `ask-reset`, `ask-name` (the questions), `select:<name>` (a preset chosen in the open tab),
/// `edit` (+4 dB of bass in the open tab), `eq-off` (its equalizer off), `popover` (the daemon's
/// popover), `preset-menu` (the tab's "…" menu), `strip-menu` (the Micro strip's preset menu, on
/// the mixer), `master-menu` (the Master's output menu, on the mixer), `settings:<n>` (the
/// settings page at section n, from 0: Général, Micro, Sorties, Mode streamer, Streaming à
/// distance, Jeux, Raccourcis et superpositions, Rapports de plantage, À propos), `crashes`
/// (sample crash reports in their section), `crash-on` (the offer to send them on), `crash-send`
/// (the send dialog on a sample report), `crash-ask` (the question after a crash), `crash-clear`
/// (the question before deleting them), `devices` and `shortcuts` (its Sorties
/// and Raccourcis sections), `shortcut-demo` (some combinations bound, one of them twice),
/// `shortcut-capture:<n>` (action n's field waiting for keys), `shortcut-note` (a combination
/// refused), `pending-restart` (two options waiting for the daemon's next start, with
/// "Redémarrer Platinum…"), `ask-restart` (its question), `startup-blocked` (Task Manager keeping
/// Platinum from starting with Windows), `ask-streamer` and `ask-classic` (the questions before a
/// mode switch); streamer mode itself is shown with `PLATINUM_UI_STREAMER=1` (classic mode with `=0`), and the language
/// with `PLATINUM_UI_LANG=<fr|en|es|de|it|pt>`. `edit-knob`: the first knob's value (Basses)
/// being typed. For the equalizer of the open tab: `band:<n>` (filter n, from 1, selected: its
/// editor and Q handles), `band-menu:<n>` (its right-click menu), `band-type` (the selected
/// filter's type menu), `hover:<n>` (filter n's reading, as under the pointer), `plus:<Hz>` (the
/// "+" on the curve at that frequency), `band-kind:<n>:<type>` and `band-q:<n>:<Q>` (an edit of
/// filter n: type from 0 in `presets::FILTER_TYPES`, Q). For the header's "Tester" row:
/// `test-playing` (the open tab's first sound playing), `test-missing` (GG's sounds absent:
/// generated ones play, and the note says so), `test-no-output` (its sounds dimmed, with the
/// reason), `prerequisite` (the banner of a missing Sonar driver), `audio-frozen` (Windows' audio
/// service frozen, with "Relancer le son de Windows"), `audio-repair` (its restart under way),
/// `audio-repair-failed` (the restart refused at Windows' prompt), `mic-recording`, `mic-recorded`, `mic-playing` (the microphone
/// test's states), `mic-note` (one of its errors) and `mic-not-relayed` (the daemon relaying no
/// microphone: "Enregistrer" dimmed, with the reason). For the preset per game, on Gaming:
/// `game-active` (Gaming's preset put there by a running game, "Activé par DELTARUNE"),
/// `game-links` (the preset linked by the user to two programs, for the "…" menu and the settings
/// page's Jeux section), `games-filter` (the browser on its "Par jeu" filter) and `link-dialog`
/// ("Associer au jeu en cours…" with sample programs). Only what is drawn changes: the engine is read-only then and drops whatever
/// the window sends, and no test sound thread is started.
fn preview(app: &App) {
    let items = PREVIEW.with(|items| items.borrow().clone());
    let microphone = app.get_daemon().microphone.to_string();
    // The samples are the daemon's own texts: they show in the language of the capture.
    let daemon = |phase: Phase, summary: &str, detail: &str| DaemonView {
        phase,
        summary: tr(summary).into(),
        detail: i18n::tr_message(detail),
        microphone: microphone.clone(),
        can_start: matches!(phase, Phase::Stopped | Phase::Failed),
        can_stop: matches!(phase, Phase::Running | Phase::Waiting | Phase::Degraded),
        ..DaemonView::default()
    };
    for item in &items {
        match item.as_str() {
            "stopped" => app.set_daemon(daemon_state(&daemon(Phase::Stopped, "Platinum est arrêté", ""))),
            "failed" => app.set_daemon(daemon_state(&daemon(Phase::Failed, "Platinum n'a pas démarré", "SteelSeries GG est ouvert : fermez GG pour démarrer Platinum"))),
            "starting" => app.set_daemon(daemon_state(&daemon(Phase::Starting, "Démarrage…", ""))),
            "waiting" => app.set_daemon(daemon_state(&daemon(Phase::Waiting, "Micro pas encore relayé", "Micro pas encore relayé"))),
            "no-mic" => app.set_daemon(daemon_state(&DaemonView { microphone: String::new(), ..daemon(Phase::Running, "Son actif, sans micro", "Aucun micro choisi : le micro virtuel reste muet") })),
            "crashes" => show_crashes(app, &sample_crashes()),
            "crash-send" => {
                show_crashes(app, &sample_crashes());
                send_crash_report(app, "");
            }
            "crash-ask" => {
                let reports = sample_crashes();
                show_crashes(app, &reports);
                app.set_crash_question(settings::crash_question(&reports[0]).into());
                app.set_asking(10);
            }
            "crash-clear" => {
                show_crashes(app, &sample_crashes());
                app.set_asking(11);
            }
            "crash-on" => with_ui(|ui| {
                ui.last.settings.crash_reports = Consent::Ask;
                show_settings(app, &ui.models, &ui.last);
            })
            .unwrap_or_default(),
            "remote" => app.set_daemon(daemon_state(&DaemonView { remote: "Steam Streaming Speakers".into(), ..daemon(Phase::Running, "Son et micro actifs, avec effets", "") })),
            "prerequisite" => set_prerequisite(app, Some((i18n::mark("Le pilote Sonar n'est pas installé : lancez Platinum-Setup.exe pour l'installer."), 2))),
            "audio-frozen" => {
                AUDIO_FROZEN.with(|frozen| frozen.set(true));
                refresh_daemon(app);
            }
            "audio-repair" => {
                REPAIR.with(|repair| *repair.borrow_mut() = Repair::Running);
                refresh_daemon(app);
            }
            "audio-repair-failed" => {
                AUDIO_FROZEN.with(|frozen| frozen.set(true));
                REPAIR.with(|repair| *repair.borrow_mut() = Repair::Failed(tr("relance annulée : Windows n'a pas reçu l'accord administrateur").into(), Instant::now()));
                refresh_daemon(app);
            }
            "notices" => {
                let notices = vec![
                    NoticeView { id: 1, text: "Sortie de Gaming non changée".into(), detail: "IStore::OpenSetting failed (HRESULT 0x80070005)".into() },
                    NoticeView { id: 2, text: "Platinum : opération non aboutie".into(), detail: "Platinum n'a pas démarré en 10 secondes : platinum-start.exe est introuvable".into() },
                ];
                with_ui(|ui| sync(&ui.models.notices, notices));
            }
            "unreadable" => {
                with_ui(|ui| edit_rows(&ui.models.columns, |c| c.key == "chat", |c| c.readable = false));
                app.set_channel_readable(app.get_channel_key() != "chat");
            }
            "ask-stop" => app.set_asking(1),
            "edit-knob" => app.global::<Preview>().set_edit_knob(true),
            "ask-mic" => {
                let name = with_ui(|ui| ui.last.mic.inputs.first().map(|m| m.name.clone())).flatten().unwrap_or_default();
                app.set_asked_microphone_name(name.into());
                app.set_asking(2);
            }
            "ask-bluetooth" => app.set_asking(3),
            "devices" => open_settings(app, settings::OUTPUTS),
            "shortcuts" => open_settings(app, settings::SHORTCUTS),
            item if item.starts_with("settings:") => open_settings(app, item["settings:".len()..].parse().unwrap_or(0)),
            "pending-restart" => {
                with_ui(|ui| {
                    ui.last.pending_restart = vec![settings::Setting::Effects, settings::Setting::Gain];
                    show_settings(app, &ui.models, &ui.last);
                });
            }
            "startup-blocked" => {
                with_ui(|ui| {
                    ui.last.installed = true;
                    ui.last.startup_blocked = true;
                    show_settings(app, &ui.models, &ui.last);
                });
            }
            "ask-restart" => app.set_asking(8),
            "shortcut-demo" => preview_shortcuts(app),
            "shortcut-note" => app.set_shortcut_note("M : ajoutez Ctrl, Alt ou Maj : cette touche seule ne pourrait plus être tapée nulle part.".into()),
            item if item.starts_with("shortcut-capture:") => app.set_shortcut_capturing(item["shortcut-capture:".len()..].parse().unwrap_or(-1)),
            "ask-delete" => app.set_asking(4),
            "ask-reset" => app.set_asking(5),
            "ask-streamer" => app.set_asking(6),
            "ask-classic" => app.set_asking(7),
            "ask-name" if app.get_naming() == 0 => ask_name(app, &app.get_channel_key(), 2),
            // A preset chosen, by its name, in the open tab: shown as the window shows a choice
            // (the read-only engine drops the command, so it is chosen again after each snapshot).
            item if item.starts_with("select:") => preview_select(app, &item["select:".len()..]),
            "edit" => edit(app, &app.get_channel_key(), Edit::Set(Field::Bass, 4.0)),
            "eq-off" => edit(app, &app.get_channel_key(), Edit::Set(Field::Equalizer, 0.0)),
            item if item.starts_with("band:") || item.starts_with("hover:") || item.starts_with("plus:") || item.starts_with("band-kind:") || item.starts_with("band-q:") => preview_band(app, item),
            item if item.starts_with("test-") || item.starts_with("mic-") => preview_test(app, item),
            "game-active" | "game-links" | "games-filter" | "link-dialog" => preview_games(app, item),
            _ => {}
        }
    }
}

/// The preset per game's previews of `preview`, on Gaming's tab: drawn in the window's copy only.
fn preview_games(app: &App, item: &str) {
    const PROGRAMS: [(&str, &str, &str); 4] = [
        ("DELTARUNE.exe", "DELTARUNE", "DELTARUNE.exe"),
        ("hl2.exe", "Half-Life 2", "hl2.exe · associé à « Soirée FPS »"),
        ("Minecraft.Windows.exe", "Minecraft", "Minecraft.Windows.exe"),
        ("Spotify.exe", "Spotify", "Spotify.exe"),
    ];
    with_ui(|ui| match item {
        "game-active" => {
            if let Some(channel) = ui.last.channels.first_mut() {
                channel.game = "DELTARUNE".into();
            }
            preset_changed(app, ui);
        }
        "game-links" => {
            let key = ui.last.channels.first().map(|c| c.selection.key()).unwrap_or_default();
            for (exe, title) in [("DELTARUNE.exe", "DELTARUNE"), ("DELTARUNE_demo.exe", "DELTARUNE Chapitres 1 et 2")] {
                games::set_link(&mut ui.last.game_links, UserLink { exe: exe.into(), title: title.into(), preset: key.clone(), preset_name: String::new() });
            }
            preset_changed(app, ui);
            show_settings(app, &ui.models, &ui.last);
        }
        "games-filter" => {
            if ui.browse.is_none() {
                ui.browse = Some(Browse { channel: 0, filter: Filter::Games, query: String::new() });
                app.set_browse_open(true);
            }
            if let Some(browse) = ui.browse.as_mut() {
                browse.filter = Filter::Games;
            }
            show_browse(app, &ui.models, &ui.last, ui.browse.as_ref());
        }
        "link-dialog" => {
            if let Some(data) = ui.last.channels.first().cloned() {
                let rows = PROGRAMS.iter().map(|(exe, title, detail)| (exe.to_string(), title.to_string(), detail.to_string())).collect();
                show_link_rows(app, ui, data.selection.clone(), preset_name(&ui.last.catalog, 0, &data), rows);
                app.set_linking(true);
            }
        }
        _ => {}
    });
}

/// The "Tester" previews of `preview`: states drawn in the window's copy only.
fn preview_test(app: &App, item: &str) {
    let key = app.get_channel_key().to_string();
    let first = sounds::samples_for(&key).first().copied();
    with_ui(|ui| {
        let test = &mut ui.test;
        match item {
            "test-playing" => test.playing = first.map(|sample| (key.clone(), sample)),
            "test-missing" => ui.sample_note = sounds::missing_note(sounds::samples_for(&key)),
            "test-no-output" => ui.sample_problem = Some(sounds::NO_OUTPUT.to_string()),
            "mic-recording" => *test = tester::Status { mic: MicPhase::Recording, position: 7.4, level: 0.62, ..tester::Status::default() },
            "mic-recorded" => *test = tester::Status { mic: MicPhase::Recorded, recorded: 12.3, ..tester::Status::default() },
            "mic-playing" => *test = tester::Status { mic: MicPhase::Playing, position: 4.2, recorded: 12.3, ..tester::Status::default() },
            "mic-note" => test.note = Some((tester::MIC_KEY.to_string(), sounds::NO_CHAT_OUTPUT.to_string())),
            // The daemon as the status file would say it when it relays nothing.
            "mic-not-relayed" => ui.last.daemon.phase = Phase::Stopped,
            _ => {}
        }
        show_tests(app, ui);
    });
}

/// Layout check: some combinations bound in the window's copy only (nothing is sent), Ctrl+Alt+Up
/// twice, to see the panel filled and a conflict flagged.
fn preview_shortcuts(app: &App) {
    const DEMO: [(&str, &str); 11] = [
        ("master_volume_up", "Ctrl+Alt+Up"),
        ("master_volume_down", "Ctrl+Alt+Down"),
        ("master_mute", "Ctrl+Alt+End"),
        ("master_stream_mute", "Ctrl+Alt+Home"),
        ("game_volume_up", "Ctrl+Shift+PageUp"),
        ("game_stream_volume_up", "Ctrl+Alt+PageUp"),
        ("chat_volume_up", "Ctrl+Alt+Up"),
        ("mic_mute", "Pause"),
        ("chatmix_game", "Ctrl+Alt+Left"),
        ("chatmix_chat", "Ctrl+Alt+Right"),
        ("output_next", "Ctrl+Alt+Numpad6"),
    ];
    with_ui(|ui| {
        let shortcuts = &mut ui.last.settings.shortcuts;
        *shortcuts = DEMO.iter().filter_map(|(action, keys)| Some((Action::parse_key(action)?, Hotkey::parse(keys).ok()?))).collect();
        shortcuts.sort_by_key(|(action, _)| action.index());
        show_shortcuts(app, &ui.models, &ui.last);
    });
}

fn preview_select(app: &App, name: &str) {
    let key = app.get_channel_key().to_string();
    let Some(index) = channel_index(&key) else { return };
    let found = with_ui(|ui| ui.last.catalog.shelf(index).and_then(|s| s.entries.iter().find(|e| e.name.eq_ignore_ascii_case(name))).map(|e| e.key.key())).flatten();
    if let Some(preset) = found {
        select_preset(app, &key, &preset);
    }
}

/// The previews that open a popup, once the window is drawn.
fn preview_popups(app: &App) {
    let items = PREVIEW.with(|items| items.borrow().clone());
    if items.iter().any(|i| i == "popover") {
        app.invoke_open_daemon_popover();
    }
    if items.iter().any(|i| i == "preset-menu") {
        app.set_open_preset_menu(true);
    }
    if items.iter().any(|i| i == "master-menu") {
        app.set_open_master_menu(true);
    }
    // The microphone's strip: the one with a favourite on this machine (GG's, copied once).
    if items.iter().any(|i| i == "strip-menu") {
        app.set_open_strip_menu(5);
    }
    let editor = app.global::<EqEditor>();
    if let Some(band) = items.iter().find_map(|i| i.strip_prefix("band-menu:")).and_then(|n| n.parse::<i32>().ok()) {
        editor.set_open_menu(band - 1);
    }
    if items.iter().any(|i| i == "band-type") {
        editor.set_open_kinds(true);
    }
}

/// The equalizer previews of `preview`: a filter selected, the "+" shown, a filter edited.
fn preview_band(app: &App, item: &str) {
    let editor = app.global::<EqEditor>();
    let key = app.get_channel_key().to_string();
    let parts: Vec<&str> = item.split(':').collect();
    let number = |i: usize| parts.get(i).and_then(|p| p.parse::<f64>().ok());
    let band = number(1).map(|n| n as i32 - 1).unwrap_or(-1);
    match (parts[0], band) {
        ("band", _) => editor.set_selected(band),
        ("hover", _) => editor.set_preview_hover(band),
        ("plus", _) => editor.set_preview_plus(number(1).map_or(-1.0, |hz| eq::frequency_to_x(hz) as f32)),
        ("band-kind", 0..) => {
            if let Some((kind, _)) = number(2).and_then(|k| presets::FILTER_TYPES.get(k as usize)) {
                edit(app, &key, Edit::Band(band as usize, BandEdit::Kind(*kind)));
            }
        }
        ("band-q", 0..) => {
            if let Some(q) = number(2) {
                edit(app, &key, Edit::Band(band as usize, BandEdit::Q(q)));
            }
        }
        _ => {}
    }
}

/// For checking the layout: saves what the window shows after a few seconds (raw RGBA after a
/// width/height header) and closes it; `page` picks the tab (6: the settings page), `browse` opens
/// the preset browser of that tab's channel, `scroll` moves the tab or the settings page down by
/// that many pixels. The engine is
/// read-only then.
fn schedule_capture(app: &App, path: std::ffi::OsString, page: i32, browse: bool, scroll: f32) {
    let weak = app.as_weak();
    slint::Timer::single_shot(Duration::from_millis(1500), {
        let weak = weak.clone();
        move || {
            if let Some(app) = weak.upgrade() {
                open_page(&app, page);
                preview(&app);
                app.set_tab_scroll(-scroll);
                app.set_settings_scroll(-scroll);
                if browse {
                    let key = usize::try_from(page - 1).ok().and_then(|i| CHANNELS.get(i)).unwrap_or(&CHANNELS[0]).key;
                    open_browse(&app, key);
                }
            }
        }
    });
    slint::Timer::single_shot(Duration::from_millis(3200), {
        let weak = weak.clone();
        move || {
            if let Some(app) = weak.upgrade() {
                preview_popups(&app);
            }
        }
    });
    slint::Timer::single_shot(Duration::from_secs(4), move || {
        if let Some(app) = weak.upgrade() {
            if let Ok(image) = app.window().take_snapshot() {
                let mut bytes = Vec::with_capacity(8 + image.as_bytes().len());
                bytes.extend_from_slice(&image.width().to_le_bytes());
                bytes.extend_from_slice(&image.height().to_le_bytes());
                bytes.extend_from_slice(image.as_bytes());
                let _ = std::fs::write(&path, bytes);
            }
        }
        let _ = slint::quit_event_loop();
    });
}

fn main() -> Result<(), slint::PlatformError> {
    // PLATINUM_UI_SNAPSHOT=<file>: the layout check (read-only, see `schedule_capture`).
    let capture = std::env::var_os("PLATINUM_UI_SNAPSHOT");
    log_panics(capture.as_deref().map(Path::new));
    // A crash leaves a report (`platinum::crash`); the layout check writes nothing there.
    if capture.is_none() {
        crash::install(crash::WINDOW);
    }
    // The window's own catalogs join the library's, and the window speaks its language from the
    // first frame.
    i18n::add_catalogs(WINDOW_CATALOGS);
    let language = startup_language();
    i18n::set_language(language);
    let app = App::new()?;
    if let Err(error) = slint::select_bundled_translation(language.code()) {
        eprintln!("platinum-ui: {error}");
    }
    // One window at a time: another launch brings this one forward instead.
    if capture.is_none() {
        let weak = app.as_weak();
        let claimed = single::claim(move || {
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(app) = weak.upgrade() {
                    let _ = app.window().show();
                    app.window().set_minimized(false);
                    single::bring_to_front();
                    // Coming back from the background: every pixel again, now and once the
                    // restore's resize has gone through (see `platinum::repaint`).
                    repaint_everything(&app);
                    let weak = app.as_weak();
                    slint::Timer::single_shot(Duration::from_millis(200), move || {
                        if let Some(app) = weak.upgrade() {
                            repaint_everything(&app);
                        }
                    });
                }
            });
        });
        if !claimed {
            return Ok(());
        }
        watch_repaints(&app);
    }
    READ_ONLY.with(|read_only| read_only.set(capture.is_some()));
    install_models(&app);
    let (commands, receiver) = channel();
    COMMANDS.with(|slot| *slot.borrow_mut() = Some(commands));
    connect_mixer(&app);
    connect_effects(&app);
    connect_presets(&app);
    connect_daemon(&app);
    connect_devices(&app);
    connect_shortcuts(&app);
    connect_settings(&app);
    connect_crashes(&app);
    connect_tests(&app);
    let _drag_watch = watch_drags(&app);
    let meter_devices = MeterDevices::default();

    let (weak, shared_devices, read_only) = (app.as_weak(), meter_devices.clone(), capture.is_some());
    let engine = std::thread::Builder::new().name("moteur".into()).spawn(move || {
        engine::run(receiver, shared_devices, read_only, move |snapshot| {
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(app) = weak.upgrade() {
                    apply(&app, snapshot);
                }
            });
        });
    });

    let _ = std::thread::Builder::new().name("vumètres".into()).spawn(move || {
        meters::run(meter_devices, move |levels| {
            let _ = slint::invoke_from_event_loop(move || {
                with_ui(|ui| sync(&ui.models.levels, levels));
            });
        });
    });

    if let Some(path) = capture {
        let page = std::env::var("PLATINUM_UI_PAGE").ok().and_then(|p| p.parse().ok()).unwrap_or(0);
        let browse = std::env::var("PLATINUM_UI_BROWSE").is_ok_and(|b| b == "1");
        let scroll = std::env::var("PLATINUM_UI_SCROLL").ok().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let items = std::env::var("PLATINUM_UI_PREVIEW").unwrap_or_default();
        PREVIEW.with(|preview| *preview.borrow_mut() = items.split(',').map(|i| i.trim().to_string()).filter(|i| !i.is_empty()).collect());
        schedule_capture(&app, path, page, browse, scroll);
    }
    // A capture that shows the banner on purpose keeps its sample.
    if !PREVIEW.with(|items| items.borrow().iter().any(|item| item == "prerequisite")) {
        check_prerequisites(&app);
    }
    refresh_crashes(&app);
    if !read_only {
        watch_audio_service(&app);
        if let Ok(folder) = crash::folder() {
            crash::prune(&folder, crash::KEEP);
        }
        offer_crash_report(&app);
    }

    let outcome = app.run();
    shut_down(engine.ok());
    outcome
}

/// After the window closed (it is hidden already): the engine is told so by dropping the only
/// sender, carries out what the window still sent, saves state.json and window.ini, and lets a
/// daemon restart under way reach its start, then the process ends. Returning at once would kill
/// the engine's threads, and could leave the daemon stopped between a restart's stop and start.
/// The wait is bounded: a stuck engine cannot keep an invisible process alive.
fn shut_down(engine: Option<std::thread::JoinHandle<()>>) {
    single::release();
    // A test sound or the microphone test stops with the window (the recording goes with it).
    TESTER.with(|tester| tester.borrow_mut().take());
    COMMANDS.with(|commands| commands.borrow_mut().take());
    // A restart of Windows' sound under way restarts Platinum at its end: it is let finish.
    let started = Instant::now();
    while REPAIRING.load(std::sync::atomic::Ordering::SeqCst) && started.elapsed() < REPAIR_LIMIT {
        std::thread::sleep(Duration::from_millis(50));
    }
    let Some(engine) = engine else { return };
    let limit = background::STOP_TIMEOUT + background::START_TIMEOUT + Duration::from_secs(5);
    let started = Instant::now();
    while !engine.is_finished() && started.elapsed() < limit {
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use platinum::preset::{Role, flat};

    /// The texts the window marks: those of its .slint files (`@tr`), and those of its Rust code.
    fn window_texts() -> (Vec<String>, Vec<String>) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let read = |folder: &str, extension: &str, markers: &[&str]| -> Vec<String> {
            let mut found: Vec<String> = Vec::new();
            for entry in std::fs::read_dir(root.join(folder)).unwrap().flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == extension) {
                    for text in i18n::marked_texts(&std::fs::read_to_string(&path).unwrap(), markers) {
                        if !found.contains(&text) {
                            found.push(text);
                        }
                    }
                }
            }
            found
        };
        (read("ui", "slint", &["@tr(\""]), read("src", "rs", &["tr(\"", "trf!(\"", "mark(\""]))
    }

    /// Every text the window marks is in every catalog, with the same placeholders: the .slint
    /// texts in the window's catalog (Slint bundles nothing else), the Rust texts in the window's
    /// or the library's (`tr` looks in both). Run with `--nocapture` for the coverage.
    #[test]
    fn every_catalog_translates_every_window_text() {
        let (slint_texts, rust_texts) = window_texts();
        assert!(slint_texts.len() > 200 && rust_texts.len() > 100, "{} + {} texts", slint_texts.len(), rust_texts.len());
        let mut report = Vec::new();
        for (language, window) in WINDOW_CATALOGS {
            let library = i18n::LIBRARY.iter().find(|(l, _)| l == language).map_or("", |(_, text)| *text);
            let (slint_missing, slint_mismatched) = i18n::catalog_problems(&slint_texts, window);
            let (rust_missing, rust_mismatched) = i18n::catalog_problems(&rust_texts, &format!("{window}\n{library}"));
            let wrong = i18n::placeholder_problems(window);
            let total = slint_texts.len() + rust_texts.len();
            let missing = slint_missing.len() + rust_missing.len();
            eprintln!("{}: {} of {} window texts translated", language.code(), total - missing, total);
            if missing > 0 || !slint_mismatched.is_empty() || !rust_mismatched.is_empty() || !wrong.is_empty() {
                let all: Vec<String> = slint_missing.into_iter().chain(rust_missing).collect();
                report.push(format!("{}: {missing} missing, placeholders differ: {:?} {:?} {:?}\n{}", language.code(), slint_mismatched, rust_mismatched, wrong, i18n::po_fragment(&all)));
            }
        }
        assert!(report.is_empty(), "{}", report.join("\n"));
    }

    fn view(phase: Phase, detail: &str) -> DaemonView {
        DaemonView { phase, detail: detail.into(), ..DaemonView::default() }
    }

    #[test]
    fn the_banner_shows_only_while_the_sound_does_not_go_through_platinum() {
        assert_eq!(banner(&view(Phase::Running, "")), (String::new(), 0));
        assert_eq!(banner(&view(Phase::Stopped, "")), ("Le son ne passe pas par Platinum.".to_string(), 1));
        let failed = banner(&view(Phase::Failed, "platinum-start.exe est introuvable"));
        assert_eq!(failed, ("Le son ne passe pas par Platinum — platinum-start.exe est introuvable".to_string(), 2));
        assert_eq!(banner(&view(Phase::Waiting, "Aucun micro choisi")).0, "Aucun micro choisi");
    }

    #[test]
    fn a_frozen_audio_service_is_said_first_and_offers_its_restart() {
        let now = Instant::now();
        assert_eq!(audio_banner(false, &Repair::Idle, now), None);
        let frozen = audio_banner(true, &Repair::Idle, now).unwrap();
        assert_eq!(frozen, ("Le service Audio de Windows ne répond plus : le son de tous les programmes est figé.".to_string(), 2));
        assert_eq!(audio_banner(true, &Repair::Running, now).map(|(_, tone)| tone), Some(0));
        assert_eq!(audio_banner(false, &Repair::Running, now).map(|(_, tone)| tone), Some(0));
        // A failure stays said while the service is frozen, and for a while once it answers.
        let failed = Repair::Failed("relance annulée".into(), now);
        assert_eq!(audio_banner(true, &failed, now + Duration::from_secs(600)), Some(("Relance du son de Windows non aboutie : relance annulée".to_string(), 2)));
        assert!(audio_banner(false, &failed, now + Duration::from_secs(5)).is_some());
        assert_eq!(audio_banner(false, &failed, now + REPAIR_FAILURE_SHOWN), None);
        // The window's state: the banner wins over the rest, its button replaces "Démarrer".
        let stopped = DaemonView { can_start: true, ..view(Phase::Stopped, "") };
        AUDIO_FROZEN.with(|cell| cell.set(true));
        let state = daemon_state(&stopped);
        AUDIO_FROZEN.with(|cell| cell.set(false));
        assert_eq!((state.banner.as_str(), state.banner_tone), (frozen.0.as_str(), 2));
        // A daemon that is gone: "Démarrer" stays beside the restart, the freeze may have ended.
        assert!(state.audio_frozen && !state.repairing_audio && state.can_start);
        let state = daemon_state(&stopped);
        assert!(!state.audio_frozen && state.can_start);
        // During the restart, nothing else starts or stops Platinum.
        REPAIR.with(|repair| *repair.borrow_mut() = Repair::Running);
        let running = daemon_state(&DaemonView { can_stop: true, ..view(Phase::Running, "") });
        REPAIR.with(|repair| *repair.borrow_mut() = Repair::Idle);
        assert!(running.repairing_audio && !running.audio_frozen && !running.can_stop && !running.can_start);
    }

    #[test]
    fn a_remote_session_is_said_above_the_mixer_and_in_the_popover() {
        let following = DaemonView { remote: "Steam Streaming Speakers".into(), ..view(Phase::Running, "") };
        assert_eq!(banner(&following), ("Streaming à distance : le son de Platinum part vers Steam Streaming Speakers".to_string(), 0));
        assert!(daemon_state(&following).remote.starts_with("Streaming à distance : le son de Platinum part vers Steam Streaming Speakers."));
        // A warning wins over it; outside a session, nothing is said.
        let waiting = DaemonView { remote: "Steam Streaming Speakers".into(), ..view(Phase::Waiting, "Aucun micro choisi") };
        assert_eq!(banner(&waiting), ("Aucun micro choisi".to_string(), 1));
        assert_eq!(daemon_state(&view(Phase::Running, "")).remote.as_str(), "");
    }

    #[test]
    fn the_stream_mix_shortcuts_come_under_their_heading() {
        let groups = shortcut_rows(&[]);
        let gaming = &groups[1];
        let labels: Vec<&str> = gaming.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(labels, ["Volume +", "Volume -", "Couper son", "Volume +", "Volume -", "Couper son"]);
        assert!(gaming.iter().enumerate().all(|(i, row)| (i == 3) == (row.section.as_str() == "MIX DE STREAM")));
        // The microphone has no stream mix; the Master has one.
        assert_eq!((groups[0].len(), groups[5].len()), (6, 3));
    }

    #[test]
    fn the_microphone_test_buttons_check_what_gg_checks() {
        let mut snapshot = Snapshot { channels: vec![ChannelData { readable: true, ..ChannelData::default() }; CHANNELS.len()], output_ids: vec![None, Some("{arena}".into())], ..Snapshot::default() };
        let idle = tester::Status::default();
        // Recording needs a microphone, relayed by the daemon.
        assert_eq!(mic_request(&idle, &snapshot, 0).err(), Some(sounds::NO_MICROPHONE));
        snapshot.mic.inputs = vec![engine::Input { id: "{seiren}".into(), name: "Seiren".into(), hands_free: false, present: true }];
        snapshot.mic.current = 0;
        assert_eq!(mic_request(&idle, &snapshot, 0).err(), Some(sounds::NOT_RELAYED));
        snapshot.daemon.phase = Phase::Waiting;
        assert_eq!(mic_request(&idle, &snapshot, 0).err(), Some(sounds::NOT_RELAYED));
        snapshot.daemon.phase = Phase::Degraded;
        assert!(matches!(mic_request(&idle, &snapshot, 0), Ok(tester::Request::Record)));
        // Listening needs a recording, then the Chat channel's output, plugged in.
        assert_eq!(mic_request(&idle, &snapshot, 1).err(), Some(sounds::NOTHING_RECORDED));
        let recorded = tester::Status { mic: MicPhase::Recorded, ..tester::Status::default() };
        assert_eq!(mic_request(&recorded, &snapshot, 1).err(), Some(sounds::NO_CHAT_OUTPUT));
        snapshot.channels[1].output_index = 1;
        assert_eq!(mic_request(&recorded, &snapshot, 1).err(), Some(sounds::NO_CHAT_OUTPUT));
        snapshot.channels[1].output_present = true;
        assert!(matches!(mic_request(&recorded, &snapshot, 1), Ok(tester::Request::Listen(id)) if id == "{arena}"));
        // While it records or plays, the same button stops it.
        let recording = tester::Status { mic: MicPhase::Recording, ..tester::Status::default() };
        assert!(matches!(mic_request(&recording, &snapshot, 0), Ok(tester::Request::Stop)));
        let playing = tester::Status { mic: MicPhase::Playing, ..tester::Status::default() };
        assert!(matches!(mic_request(&playing, &snapshot, 1), Ok(tester::Request::Stop)));
        assert!(matches!(mic_request(&playing, &snapshot, 0), Ok(tester::Request::Record)));
        // A channel's test sounds need its output plugged in; an unreadable channel has none.
        assert!(channel_has_output(&snapshot, 1) && !channel_has_output(&snapshot, 0));
        snapshot.channels[1].readable = false;
        assert!(!channel_has_output(&snapshot, 1) && chat_output(&snapshot).is_none());
    }

    #[test]
    fn header_words_start_with_a_capital() {
        assert_eq!(capitalized("actif"), "Actif");
        assert_eq!(capitalized("en attente des périphériques"), "En attente des périphériques");
        assert_eq!(capitalized(""), "");
    }

    #[test]
    fn the_about_section_says_what_it_reads_and_what_it_cannot() {
        let stopped = about_view(&Snapshot { daemon: DaemonView { summary: "Platinum est arrêté".into(), ..DaemonView::default() }, ..Snapshot::default() });
        assert_eq!((stopped.pid.as_str(), stopped.memory.as_str(), stopped.engine_memory.as_str(), stopped.updated.as_str()), ("—", "—", "—", "—"));
        assert_eq!(stopped.state.as_str(), "Arrêté — Platinum est arrêté");
        assert!(!stopped.can_open_log && !stopped.can_open_folder && stopped.log.as_str() == "Aucun journal pour l'instant");
        let about = engine::About { pid: Some(21360), memory: Some((57_042_534, 78_224_179)), worker_pid: Some(8124), worker_memory: Some((222_298_112, 251_658_240)), updated: "20261117-174426".into(), log: Some(r"C:\P\daemon.log".into()), folder: Some(r"C:\P".into()) };
        let running = about_view(&Snapshot { about, daemon: DaemonView { phase: Phase::Running, summary: "Son et micro actifs, avec effets".into(), ..DaemonView::default() }, ..Snapshot::default() });
        assert_eq!((running.pid.as_str(), running.updated.as_str(), running.folder.as_str()), ("21360", "17/11/2026 à 17:44:26", r"C:\P"));
        assert_eq!(running.memory.as_str(), "54,4 Mo privés · 74,6 Mo en mémoire vive");
        assert_eq!(running.engine_memory.as_str(), "212,0 Mo privés · 240,0 Mo en mémoire vive (processus 8124)");
        assert!(running.can_open_log && running.can_open_folder && running.state.starts_with("Actif — "));
        assert_eq!(running.version.as_str(), env!("CARGO_PKG_VERSION"));
    }

    fn channel(selection: PresetRef, preset: Preset) -> ChannelData {
        ChannelData { readable: true, preset: Some(preset), selection, ..ChannelData::default() }
    }

    #[test]
    fn the_header_says_where_a_preset_comes_from_and_whether_it_changed() {
        let catalog = catalog::build(&persist::State::default(), None, &[], String::new(), &[]);
        let custom = header(&catalog, 0, &channel(PresetRef::Custom, flat(Role::Game)), &[]);
        assert_eq!((custom.name.as_str(), custom.source.as_str(), custom.can_reset, custom.can_favorite, custom.can_link), ("Personnalisé", "Réglages actuels du canal", false, false, false));
        let neutral = header(&catalog, 0, &channel(PresetRef::Flat, flat(Role::Game)), &[]);
        assert_eq!((neutral.name.as_str(), neutral.modified, neutral.can_favorite, neutral.own), (platinum::preset::NEUTRAL_NAME, false, true, false));
        let mut changed = flat(Role::Game);
        presets::apply(&mut changed, Edit::Set(Field::Bass, 3.0));
        let modified = header(&catalog, 0, &channel(PresetRef::Flat, changed), &[]);
        assert!(modified.modified && modified.can_reset && !modified.can_save);
        let missing = ChannelData { selection_name: "Ancien".into(), ..channel(PresetRef::Own("Ancien".into()), flat(Role::Game)) };
        assert_eq!(header(&catalog, 0, &missing, &[]).name.as_str(), "Ancien");
    }

    #[test]
    fn gamings_header_says_which_game_put_its_preset_and_lists_its_links() {
        let catalog = catalog::build(&persist::State::default(), None, &[], String::new(), &[]);
        let links = [
            UserLink { exe: "DELTARUNE.exe".into(), title: String::new(), preset: "flat".into(), preset_name: "Neutre".into() },
            UserLink { exe: "hl2.exe".into(), title: "Half-Life 2".into(), preset: "own:Soir".into(), preset_name: "Soir".into() },
        ];
        let game = ChannelData { game: "DELTARUNE".into(), ..channel(PresetRef::Flat, flat(Role::Game)) };
        let gaming = header(&catalog, 0, &game, &links);
        assert_eq!((gaming.game.as_str(), gaming.can_link, gaming.links.row_count()), ("DELTARUNE", true, 1));
        assert_eq!(gaming.links.row_data(0).map(|l| l.title.to_string()), Some("DELTARUNE".to_string()));
        // Only Gaming links presets to games.
        let media = header(&catalog, 2, &ChannelData { game: "DELTARUNE".into(), ..channel(PresetRef::Flat, flat(Role::Media)) }, &links);
        assert!(media.game.is_empty() && !media.can_link && media.links.row_count() == 0);
    }

    #[test]
    fn the_link_dialog_offers_each_running_program_once() {
        let windows = [("DELTARUNE.exe".to_string(), "DELTARUNE".to_string()), ("explorer.exe".to_string(), "Explorateur".to_string()), ("Spotify.exe".to_string(), "Spotify Premium".to_string())];
        let sounding = ["spotify.exe".to_string(), "Discord.exe".to_string(), "platinum.exe".to_string()];
        let links = [UserLink { exe: "deltarune.exe".into(), title: "DELTARUNE".into(), preset: "flat".into(), preset_name: "Neutre (Platinum)".into() }];
        let rows = program_rows(&windows, &sounding, &links);
        let shown: Vec<(&str, &str)> = rows.iter().map(|(exe, title, _)| (exe.as_str(), title.as_str())).collect();
        // The shell and Platinum are left out; a program with a window keeps its window's title.
        assert_eq!(shown, [("DELTARUNE.exe", "DELTARUNE"), ("Discord.exe", "Discord"), ("Spotify.exe", "Spotify Premium")]);
        assert_eq!(rows[0].2, "DELTARUNE.exe · associé à « Neutre (Platinum) »");
        assert_eq!(rows[1].2, "Discord.exe");
    }

    #[test]
    fn the_band_editor_shows_values_as_gg_does() {
        // In French (the tests' language), with a decimal comma.
        assert_eq!(frequency_text(62.0), "62 Hz");
        assert_eq!(frequency_text(31.3), "31,3 Hz");
        assert_eq!(frequency_text(1250.0), "1,25 kHz");
        assert_eq!(frequency_text(16_000.0), "16 kHz");
        assert_eq!(gain_text(3.5), "+3,5 dB");
        assert_eq!(gain_text(-0.04), "0,0 dB");
        assert_eq!(gain_text(-4.0), "-4,0 dB");
        // What it writes, it reads back.
        assert_eq!(BandValue::Frequency.parse(&frequency_text(1250.0)), Some(BandEdit::Frequency(1250.0)));
        assert_eq!(BandValue::Gain.parse(&gain_text(3.5)), Some(BandEdit::Gain(3.5)));
        assert_eq!(short_number(0.7071, 3, 1), "0.707");
        assert_eq!(short_number(1.0, 3, 1), "1.0");
        assert_eq!(short_number(2.5, 3, 1), "2.5");
        // The microphone's neutral first filter is a low shelf, fourth of GG's six types.
        let shelf = band_view(&flat(Role::ChatCapture).equalizer().bands()[0]);
        assert_eq!((shelf.kind, shelf.kind_name.as_str(), shelf.q_text.as_str()), (4, "Plateau grave", "0,707"));
        // A type the menu does not offer is named, not chosen.
        let all_pass = band_view(&eq::Band { enabled: true, frequency: 1000.0, q: 1.0, gain: 0.0, kind: eq::Kind::AllPass });
        assert_eq!((all_pass.kind, all_pass.kind_name.as_str()), (-1, "Passe-tout"));
    }

    #[test]
    fn the_boosts_are_marked_and_drawn_in_the_curve() {
        let mut aux = flat(Role::Aux);
        presets::apply(&mut aux, Edit::Set(Field::Bass, 6.0));
        let markers = marker_views(&aux);
        let letters: Vec<&str> = markers.iter().map(|m| m.letter.as_str()).collect();
        assert_eq!(letters, ["B", "V", "A"]);
        // The bass boost is on, at its gain; the voice one is off, at 0 dB.
        assert!(markers[0].on && !markers[1].on);
        assert_eq!((markers[0].gain, markers[1].gain), (6.0, 0.0));
        assert!((f64::from(markers[0].x) - eq::frequency_to_x(80.0)).abs() < 1e-6);
        assert!(marker_views(&flat(Role::ChatRender)).is_empty());
        // The "+" follows the curve, the boost included.
        let drawn: Vec<eq::Band> = aux.equalizer().bands().into_iter().chain(aux.boosts().bands()).collect();
        let samples = curve_samples(&drawn);
        let at_80 = (eq::frequency_to_x(80.0) * CURVE_SAMPLES as f64).round() as usize;
        assert!(samples[at_80] > 5.0, "{}", samples[at_80]);
        assert_eq!(samples.len(), CURVE_SAMPLES + 1);
        assert!((f64::from(samples[0]) - eq::response_db(&drawn, 20.0)).abs() < 1e-3);
    }

    #[test]
    fn the_views_show_ggs_scales() {
        let Preset::Capture(mut mic) = flat(Role::ChatCapture) else { panic!() };
        mic.noise_reduction.enabled = true;
        let view = capture_view(&mic, true, presets::expected_reductions(&mic));
        // AI is on in the neutral microphone preset: the noise reduction shows off, as the module.
        assert!(view.ai_on && !view.nr_on && view.has_echo && !view.reductions_stuck);
        // A module that holds it on anyway shows it, and says why it is still heard.
        let stuck = capture_view(&mic, true, (true, false));
        assert!(stuck.nr_on && stuck.reductions_stuck);
        assert!((view.ai - 0.501).abs() < 0.001, "{}", view.ai);
        let Preset::Render(game) = flat(Role::Game) else { panic!() };
        let view = render_view(&game);
        assert_eq!((view.reverb, view.distance, view.loudness, view.speakers_mode), (0.5, 1.0, 1, false));
        // A boost switched off under an equalizer that is on reads 0 dB, as it is heard.
        let mut quiet = game;
        quiet.voice_clarity = platinum::preset::Toggle { enabled: false, value: -8.0 };
        assert_eq!(render_view(&quiet).voice, 0.0);
        quiet.equalizer.enabled = false;
        assert_eq!(render_view(&quiet).voice, -8.0);
        let speakers = speaker_views(&game);
        assert_eq!(speakers.len(), 7);
        assert_eq!((speakers[1].name.as_str(), speakers[1].angle), ("Avant gauche", 30.0));
    }
}
