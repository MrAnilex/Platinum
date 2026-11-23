//! The setup's window: what the install (or uninstall) will do on this machine, SteelSeries'
//! licence and the options, the steps as they run, the end. The work itself runs on a thread
//! (`install::Run`, `uninstall::Run`) and reports through `install::Event`.
//!
//! `PLATINUM_SETUP_SNAPSHOT=<file>` draws one page with sample states and saves it (width,
//! height, RGBA, as ui/snapshot-to-png.mjs reads it), then quits: nothing is installed.
//! `PLATINUM_SETUP_PAGE=0..3`, `PLATINUM_SETUP_UNINSTALL=1`, `PLATINUM_SETUP_LANG=<code>`,
//! `PLATINUM_SETUP_PREVIEW=clean|gg-running|failed|declined|accepted|scroll|crash-reports` choose what
//! is drawn (`crash-reports`: the option to offer sending crash reports ticked);
//! page 4 (SteelSeries GG's licence) reads its text from the GG installer named by
//! `PLATINUM_SETUP_LICENSE_FROM` (read only).

use crate::install::{self, Event, Outcome, State};
use crate::plan::{self, Blocker, Machine, Options, Plan, Source};
use crate::{SetupWindow, StepView, log::Log, uninstall, win};
use platinum::i18n::{self, Choice, Language, tr};
use platinum::trf;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What the window runs.
#[derive(Clone, Debug)]
pub enum Mode {
    Install(Options),
    Uninstall,
}

/// The window's state between its callbacks.
struct Shared {
    mode: Mode,
    machine: Machine,
    plan: Plan,
    cancel: Arc<AtomicBool>,
    log: PathBuf,
    steps: Rc<VecModel<StepView>>,
    finished: Option<Outcome>,
    read_only: bool,
    /// Where the answer to SteelSeries GG's licence goes, while the install waits for it.
    decision: Option<std::sync::mpsc::Sender<bool>>,
}

thread_local! {
    static WINDOW: RefCell<slint::Weak<SetupWindow>> = RefCell::new(slint::Weak::default());
}

static REPAINT_PENDING: AtomicBool = AtomicBool::new(false);

/// From the window procedure (`platinum::repaint`): every pixel again, at the next turn of the
/// event loop.
fn full_repaint_requested() {
    if REPAINT_PENDING.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = slint::invoke_from_event_loop(|| {
        REPAINT_PENDING.store(false, Ordering::SeqCst);
        if let Some(window) = WINDOW.with(|slot| slot.borrow().upgrade()) {
            window.set_full_repaint(!window.get_full_repaint());
        }
    });
}

fn language() -> Language {
    match std::env::var("PLATINUM_SETUP_LANG").ok().and_then(|code| Choice::parse(&code)) {
        Some(choice) => choice.resolve(),
        None => Choice::Auto.resolve(),
    }
}

/// What the welcome page lists, translated.
pub fn facts(machine: &Machine, plan: &Plan) -> Vec<String> {
    let mut list = Vec::new();
    match &plan.source {
        Source::Download => list.push(tr("Télécharger l'installateur officiel de SteelSeries GG (environ 434 Mo) depuis steelseries.com, et n'en garder que le pilote Sonar, ses modules d'effets et les préréglages de GG (environ 42 Mo).").to_string()),
        Source::Local(path) => list.push(trf!("Prendre le pilote Sonar et les préréglages dans l'installateur de GG indiqué : {file}.", file = path.display())),
        Source::Copy => list.push(tr("Reprendre le pilote Sonar déjà extrait par une installation précédente de Platinum.").to_string()),
        Source::NotNeeded => list.push(tr("Le pilote Sonar est déjà installé : rien à télécharger de SteelSeries.").to_string()),
    }
    if plan.needs_license() {
        list.push(tr("Vous montrer la licence de SteelSeries GG, tirée de leur installateur officiel : rien n'est installé avant que vous l'acceptiez.").to_string());
    }
    if plan.driver {
        list.push(tr("Installer le pilote Sonar avec les outils de SteelSeries, comme le fait GG (autorisation d'administrateur, son coupé quelques secondes). GG lui-même n'est pas installé.").to_string());
    }
    if plan.vc_runtime {
        list.push(tr("Installer le runtime Visual C++ de Microsoft, nécessaire aux effets du micro.").to_string());
    }
    if plan.update {
        list.push(trf!("Mettre à jour Platinum dans {folder} : vos réglages et préréglages sont gardés.", folder = machine.install_dir.display()));
    } else {
        list.push(trf!("Copier Platinum dans {folder}, sans droits d'administrateur.", folder = machine.install_dir.display()));
    }
    list.push(tr("Ajouter Platinum au menu Démarrer et aux applications installées, puis le démarrer.").to_string());
    list
}

/// What the uninstall page lists, translated.
pub fn uninstall_facts(machine: &Machine) -> Vec<String> {
    let mut list = vec![
        tr("Arrêter Platinum et remettre les périphériques audio par défaut de Windows.").to_string(),
        tr("Retirer son démarrage avec Windows, ses programmes, ses raccourcis et son entrée dans les applications installées.").to_string(),
    ];
    if !uninstall::driver_offered(machine) {
        list.push(if machine.gg_installed { tr("Le pilote Sonar reste installé : il appartient à SteelSeries GG.") } else { tr("Le pilote Sonar reste installé : Platinum ne l'a pas installé.") }.to_string());
    }
    list
}

/// Why the install cannot go on now, translated; empty when it can.
pub fn blocker_text(plan: &Plan) -> String {
    let mut texts: Vec<String> = plan
        .blockers
        .iter()
        .map(|blocker| match blocker {
            Blocker::GgRunning(names) => trf!("SteelSeries GG est ouvert ({programs}). Quittez-le (icône près de l'horloge, Quitter), puis cliquez sur Réessayer. Désactivez aussi son démarrage avec Windows (Gestionnaire des tâches, Applications de démarrage).", programs = names.join(", ")),
            Blocker::Windows(version) => trf!("Le pilote Sonar demande Windows 10 version 1809 ou plus récente (cet ordinateur : build {build}).", build = version.build),
            Blocker::NotX64 => tr("Le pilote Sonar n'existe que pour Windows 64 bits sur processeur x64.").to_string(),
        })
        .collect();
    if !crate::payload::present() {
        texts.push(tr("Cet installateur a été compilé sans les programmes de Platinum : utilisez scripts\\build-setup.ps1.").to_string());
    }
    texts.join("\n")
}

fn strings(list: Vec<String>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(list.into_iter().map(SharedString::from).collect::<Vec<_>>()))
}

/// A clean machine (no GG, no driver, no runtime), for the snapshots of what a new user sees.
pub fn sample_clean_machine(install_dir: PathBuf) -> Machine {
    Machine {
        windows: platinum::machine::windows_version(),
        x64: true,
        gg_installed: false,
        gg_version: None,
        gg_running: Vec::new(),
        gg_at_logon: false,
        devices: 0,
        packages_staged: false,
        stores_missing: platinum::doctor::STORES.to_vec(),
        vc_runtime: None,
        presets: None,
        copy_complete: false,
        installed_by_platinum: false,
        install_dir,
        platinum_installed: false,
    }
}

fn fill(window: &SetupWindow, state: &Shared) {
    match &state.mode {
        Mode::Install(options) => {
            window.set_uninstall(false);
            window.set_facts(strings(facts(&state.machine, &state.plan)));
            window.set_blocker(blocker_text(&state.plan).into());
            window.set_shows_license(state.plan.needs_license());
            window.set_driver(state.plan.driver);
            window.set_autostart(options.autostart);
            window.set_desktop(options.desktop_shortcut);
            window.set_crash_reports(options.crash_reports);
        }
        Mode::Uninstall => {
            window.set_uninstall(true);
            window.set_facts(strings(uninstall_facts(&state.machine)));
            window.set_blocker(SharedString::new());
            window.set_driver_offered(uninstall::driver_offered(&state.machine));
        }
    }
}

fn step_titles(state: &Shared, window: &SetupWindow) -> Vec<&'static str> {
    match &state.mode {
        Mode::Install(_) => plan::steps(&state.plan).iter().map(|s| s.title()).collect(),
        Mode::Uninstall => uninstall::steps(&state.machine, &uninstall_options(window)).iter().map(|s| s.title()).collect(),
    }
}

fn uninstall_options(window: &SetupWindow) -> uninstall::Options {
    uninstall::Options { remove_data: window.get_remove_data(), remove_driver: window.get_remove_driver() }
}

/// Applies an event of the work thread, on the window's thread.
fn apply(window: &SetupWindow, state: &Rc<RefCell<Shared>>, event: Event) {
    match event {
        Event::Step(index, step_state) => {
            let steps = state.borrow().steps.clone();
            if let Some(mut row) = slint::Model::row_data(&*steps, index) {
                row.state = match step_state {
                    State::Running => 1,
                    State::Done => 2,
                    State::Failed => 3,
                };
                slint::Model::set_row_data(&*steps, index, row);
            }
            if step_state == State::Running {
                window.set_progress(-1.0);
                window.set_progress_text(SharedString::new());
            }
        }
        Event::Progress(fraction, text) => {
            window.set_progress(fraction.unwrap_or(-1.0));
            window.set_progress_text(text.into());
        }
        Event::License { text, version } => {
            window.set_license_text(text.into());
            window.set_license_source(trf!("Tirée de l'installateur officiel de SteelSeries GG {version}, vérifié par sa signature.", version = version).into());
            window.set_license_accepted(false);
            window.set_license_scroll(0.0);
            window.set_page(4);
        }
        Event::Finished(outcome) => {
            window.set_running(false);
            window.set_outcome_ok(outcome.ok);
            window.set_outcome_declined(outcome.declined);
            window.set_outcome_message(outcome.message.clone().into());
            window.set_outcome_notes(strings(outcome.notes.clone()));
            window.set_page(3);
            state.borrow_mut().finished = Some(outcome);
        }
    }
}

/// Starts the work on its thread.
fn start(window: &SetupWindow, state: &Rc<RefCell<Shared>>) {
    let titles = step_titles(&state.borrow(), window);
    let steps = Rc::new(VecModel::from(titles.iter().map(|t| StepView { title: tr(t).into(), state: 0 }).collect::<Vec<_>>()));
    window.set_steps(ModelRc::from(steps.clone()));
    window.set_page(2);
    window.set_running(true);
    window.set_progress(-1.0);
    window.set_progress_text(SharedString::new());
    let mut s = state.borrow_mut();
    s.steps = steps;
    s.finished = None;
    s.cancel.store(false, Ordering::SeqCst);
    if s.read_only {
        return;
    }
    let (answer, decisions) = std::sync::mpsc::channel();
    s.decision = Some(answer);
    let weak = Mutex::new(window.as_weak());
    let (machine, plan, mode, cancel, log_path) = (s.machine.clone(), s.plan.clone(), s.mode.clone(), s.cancel.clone(), s.log.clone());
    let options = uninstall_options(window);
    let (autostart, desktop, crash_reports) = (window.get_autostart(), window.get_desktop(), window.get_crash_reports());
    drop(s);
    let _ = std::thread::Builder::new().name("installation".into()).spawn(move || {
        let events = move |event: Event| {
            let weak = weak.lock().map(|w| w.clone()).unwrap_or_default();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    STATE.with(|slot| {
                        if let Some(state) = slot.borrow().as_ref() {
                            apply(&window, state, event);
                        }
                    });
                }
            });
        };
        let log = Log::open(&log_path);
        match mode {
            Mode::Install(given) => {
                let options = Options { autostart, desktop_shortcut: desktop, crash_reports, gg_installer: given.gg_installer };
                install::Run { machine, plan, options, log, events: &events, cancel, decisions: Some(decisions), reboot: false, notes: Vec::new() }.install();
            }
            Mode::Uninstall => uninstall::Run { machine, options, log, events: &events }.uninstall(),
        }
    });
}

thread_local! {
    static STATE: RefCell<Option<Rc<RefCell<Shared>>>> = const { RefCell::new(None) };
}

/// Opens the window and runs it until it closes.
pub fn run(mode: Mode) -> Result<(), slint::PlatformError> {
    let capture = std::env::var_os("PLATINUM_SETUP_SNAPSHOT").map(PathBuf::from);
    i18n::add_catalogs(crate::SETUP_CATALOGS);
    let language = language();
    i18n::set_language(language);
    let window = SetupWindow::new()?;
    if slint::select_bundled_translation(language.code()).is_err() {
        let _ = slint::select_bundled_translation("");
    }
    let previews: Vec<String> = std::env::var("PLATINUM_SETUP_PREVIEW").unwrap_or_default().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    let mut machine = Machine::gather();
    if previews.iter().any(|p| p == "clean") {
        machine = sample_clean_machine(machine.install_dir.clone());
    }
    if previews.iter().any(|p| p == "gg-running") {
        machine.gg_running = vec!["SteelSeriesGG.exe".into(), "SteelSeriesSonar.exe".into()];
    }
    let options = match &mode {
        Mode::Install(options) => options.clone(),
        Mode::Uninstall => Options::default(),
    };
    let plan = plan::plan(&machine, &options);
    let log = machine.install_dir.join("install.log");
    let state = Rc::new(RefCell::new(Shared { mode, machine, plan, cancel: Arc::new(AtomicBool::new(false)), log, steps: Rc::new(VecModel::default()), finished: None, read_only: capture.is_some(), decision: None }));
    STATE.with(|slot| *slot.borrow_mut() = Some(state.clone()));
    window.set_version(format!("Platinum {}", env!("CARGO_PKG_VERSION")).into());
    fill(&window, &state.borrow());

    window.on_next({
        let weak = window.as_weak();
        move || {
            if let Some(window) = weak.upgrade() {
                window.set_page(1);
            }
        }
    });
    window.on_back({
        let weak = window.as_weak();
        move || {
            if let Some(window) = weak.upgrade() {
                window.set_page(0);
            }
        }
    });
    window.on_install({
        let (weak, state) = (window.as_weak(), state.clone());
        move || {
            if let Some(window) = weak.upgrade() {
                start(&window, &state);
            }
        }
    });
    window.on_cancel({
        let state = state.clone();
        move || state.borrow().cancel.store(true, Ordering::SeqCst)
    });
    window.on_retry({
        let (weak, state) = (window.as_weak(), state.clone());
        move || {
            let Some(window) = weak.upgrade() else { return };
            if state.borrow().read_only {
                return;
            }
            let failed_install = window.get_page() == 3;
            {
                let mut s = state.borrow_mut();
                s.machine = Machine::gather();
                let options = match &s.mode {
                    Mode::Install(options) => Options { autostart: window.get_autostart(), desktop_shortcut: window.get_desktop(), crash_reports: window.get_crash_reports(), gg_installer: options.gg_installer.clone() },
                    Mode::Uninstall => Options::default(),
                };
                s.plan = plan::plan(&s.machine, &options);
            }
            fill(&window, &state.borrow());
            if failed_install && blocker_text(&state.borrow().plan).is_empty() {
                start(&window, &state);
            } else {
                window.set_page(0);
            }
        }
    });
    // The user's answer to SteelSeries GG's licence, which the install waits for.
    for accepted in [true, false] {
        let (weak, state) = (window.as_weak(), state.clone());
        let answer = move || {
            if let Some(window) = weak.upgrade() {
                window.set_page(2);
            }
            if let Some(decision) = &state.borrow().decision {
                let _ = decision.send(accepted);
            }
        };
        if accepted {
            window.on_accept_license(answer);
        } else {
            window.on_decline_license(answer);
        }
    }
    window.on_open_log({
        let state = state.clone();
        move || win::open(&state.borrow().log.display().to_string())
    });
    window.on_open_platinum({
        let state = state.clone();
        move || {
            let dir = state.borrow().machine.install_dir.clone();
            let _ = std::process::Command::new(dir.join("platinum-ui.exe")).current_dir(&dir).spawn();
            let _ = slint::quit_event_loop();
        }
    });
    window.on_quit({
        let state = state.clone();
        move || {
            let s = state.borrow();
            if matches!(s.mode, Mode::Uninstall) && s.finished.as_ref().is_some_and(|o| o.ok) && !s.read_only {
                uninstall::delete_self_later(&s.machine.install_dir);
            }
            let _ = slint::quit_event_loop();
        }
    });

    if let Some(path) = capture {
        schedule_capture(&window, &state, path, &previews);
    } else {
        watch_repaints(&window);
    }
    window.run()
}

fn watch_repaints(window: &SetupWindow) {
    WINDOW.with(|slot| *slot.borrow_mut() = window.as_weak());
    let timer = Rc::new(slint::Timer::default());
    let tries = Rc::new(std::cell::Cell::new(0));
    // The callback holds the timer, so it lives on after this function returns.
    let held = timer.clone();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(250), move || {
        tries.set(tries.get() + 1);
        if platinum::repaint::watch(full_repaint_requested) || tries.get() >= 40 {
            held.stop();
        }
    });
}

/// The snapshot: a page with sample states, saved after it is drawn, then the window closes.
fn schedule_capture(window: &SetupWindow, state: &Rc<RefCell<Shared>>, path: PathBuf, previews: &[String]) {
    let page = std::env::var("PLATINUM_SETUP_PAGE").ok().and_then(|p| p.parse::<i32>().ok()).unwrap_or(0);
    if std::env::var("PLATINUM_SETUP_UNINSTALL").is_ok_and(|v| v == "1") {
        let mut s = state.borrow_mut();
        s.mode = Mode::Uninstall;
        s.machine.installed_by_platinum = true;
        s.machine.gg_installed = false;
        drop(s);
        fill(window, &state.borrow());
    }
    match page {
        1 => window.set_page(1),
        2 => {
            start(window, state);
            let steps = state.borrow().steps.clone();
            let count = slint::Model::row_count(&*steps);
            for index in 0..count {
                if let Some(mut row) = slint::Model::row_data(&*steps, index) {
                    row.state = match index {
                        0..=1 => 2,
                        2 => 1,
                        _ => 0,
                    };
                    slint::Model::set_row_data(&*steps, index, row);
                }
            }
            window.set_progress(0.37);
            window.set_progress_text(tr("Téléchargement de l'installateur de SteelSeries GG : {done} Mo sur {total} Mo").replace("{done}", "160").replace("{total}", "434").into());
        }
        3 => {
            let failed = previews.iter().any(|p| p == "failed");
            let outcome = if previews.iter().any(|p| p == "declined") {
                Outcome { ok: false, declined: true, message: tr("Rien n'a été installé.").into(), notes: vec![tr("La licence de SteelSeries GG a été refusée : l'installateur téléchargé a été supprimé.").into()] }
            } else if failed {
                Outcome { ok: false, declined: false, message: tr(crate::elevated::Exit::Signature.message()).into(), notes: vec![trf!("Le journal de l'installation : {file}", file = state.borrow().log.display())] }
            } else if matches!(state.borrow().mode, Mode::Uninstall) {
                Outcome { ok: true, declined: false, message: tr("Platinum est désinstallé.").into(), notes: vec![trf!("Vos réglages et préréglages restent dans {folder}.", folder = state.borrow().machine.install_dir.display())] }
            } else {
                Outcome { ok: true, declined: false, message: tr("Platinum est installé.").into(), notes: vec![tr("SteelSeries GG démarre encore avec Windows : désactivez-le dans le Gestionnaire des tâches (Applications de démarrage) pour laisser la main à Platinum.").into()] }
            };
            start(window, state);
            apply(window, state, Event::Finished(outcome));
        }
        4 => {
            start(window, state);
            let installer = std::env::var_os("PLATINUM_SETUP_LICENSE_FROM").map(PathBuf::from);
            let text = installer.as_deref().map(crate::nsis::license).unwrap_or_else(|| Err("PLATINUM_SETUP_LICENSE_FROM names no GG installer".into())).unwrap_or_else(|error| error);
            let version = installer.as_deref().and_then(|p| p.file_name()).map(|n| install::gg_version(&n.to_string_lossy())).unwrap_or_default();
            apply(window, state, Event::License { text, version });
        }
        _ => window.set_page(0),
    }
    if previews.iter().any(|p| p == "accepted") {
        window.set_license_accepted(true);
    }
    if previews.iter().any(|p| p == "crash-reports") {
        window.set_crash_reports(true);
    }
    // `scroll`: the licence scrolled down by PLATINUM_SETUP_SCROLL pixels (900 by default).
    if previews.iter().any(|p| p == "scroll") {
        let pixels = std::env::var("PLATINUM_SETUP_SCROLL").ok().and_then(|v| v.parse::<f32>().ok()).unwrap_or(900.0);
        window.set_license_scroll(-pixels);
    }
    let weak = window.as_weak();
    slint::Timer::single_shot(Duration::from_millis(1500), move || {
        if let Some(window) = weak.upgrade()
            && let Ok(image) = window.window().take_snapshot()
        {
            let mut bytes = Vec::with_capacity(8 + image.as_bytes().len());
            bytes.extend_from_slice(&image.width().to_le_bytes());
            bytes.extend_from_slice(&image.height().to_le_bytes());
            bytes.extend_from_slice(image.as_bytes());
            let _ = std::fs::write(&path, bytes);
        }
        let _ = slint::quit_event_loop();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_welcome_page_says_what_will_happen() {
        let machine = sample_clean_machine(PathBuf::from(r"C:\Users\Test\Platinum"));
        let plan = plan::plan(&machine, &Options::default());
        let lines = facts(&machine, &plan);
        assert_eq!(lines.len(), 6, "{lines:?}");
        assert!(lines[0].contains("434") && lines[1].contains("licence de SteelSeries GG") && lines[2].contains("GG lui-même n'est pas installé"));
        assert!(lines[4].contains(r"C:\Users\Test\Platinum"));
        let mut running = machine.clone();
        running.gg_running = vec!["SteelSeriesGG.exe".into()];
        assert!(blocker_text(&plan::plan(&running, &Options::default())).contains("SteelSeriesGG.exe"));
    }
}
