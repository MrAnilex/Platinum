//! `platinum doctor`: what Platinum needs on this machine, what is there, and what to do about
//! each problem. Reads only (registry, files, device lists, the daemon's status file); nothing is
//! written, no audio device is opened.
//!
//! The same checks give the window its banner (`banner`): the most important missing piece, in
//! one French sentence (translated), or nothing when all is well.

use crate::devices::{Devices, Flow, State};
use crate::i18n::mark;
use crate::library::{self, Origin};
use crate::machine;
use crate::sonarfiles::{self, Source};
use std::path::PathBuf;

/// How bad a finding is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    /// Works, but something is off (GG running or starting at logon, no preset library…).
    Warning,
    /// Platinum cannot do its job until it is fixed.
    Problem,
}

impl Level {
    pub fn label(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warning => "warning",
            Level::Problem => "problem",
        }
    }
}

/// One finding: what was checked, how it stands, details, and what to do (English, for the
/// command line).
#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub name: &'static str,
    pub level: Level,
    pub detail: String,
    pub fix: Option<String>,
}

fn check(name: &'static str, level: Level, detail: impl Into<String>, fix: Option<&str>) -> Check {
    Check { name, level, detail: detail.into(), fix: fix.map(str::to_string) }
}

const SETUP: &str = "run Platinum-Setup.exe: it installs the Sonar driver from SteelSeries GG's official installer (GG itself is not installed)";
/// The settings stores SteelSeries' `Sonar.AgsSetup.exe` creates, one per channel.
pub const STORES: [&str; 5] = ["Game", "ChatRender", "ChatCapture", "Media", "Aux"];
pub const STORE_ROOT: &str = r"SOFTWARE\SteelSeries ApS\Sonar.APO";

/// What the machine-level checks found, gathered once (each piece is cheap but not free).
#[derive(Clone, Debug, Default)]
pub struct Findings {
    pub windows: Option<machine::WindowsVersion>,
    pub x64: bool,
    pub devices: Vec<machine::SonarDevice>,
    pub packages: Vec<(&'static str, Option<PathBuf>)>,
    pub stores_missing: Vec<&'static str>,
    pub vc_runtime: Option<(u32, u32, u32)>,
    pub gg_installed: bool,
    pub gg_running: Vec<String>,
    pub engine: Option<(PathBuf, Source)>,
}

impl Findings {
    /// Gathers what `banner` needs (no device list, no library).
    pub fn gather() -> Findings {
        Findings {
            windows: Some(machine::windows_version()),
            x64: machine::is_x64(),
            devices: machine::sonar_devices(),
            packages: machine::staged_packages(),
            stores_missing: missing_stores(),
            vc_runtime: machine::vc_runtime(),
            gg_installed: machine::gg_installed(),
            gg_running: machine::gg_running(),
            engine: Some(sonarfiles::engine_dll()),
        }
    }

    pub fn driver_installed(&self) -> bool {
        !self.devices.is_empty() && self.packages.iter().all(|(_, folder)| folder.is_some())
    }
}

/// The channels' settings stores missing from the registry.
pub fn missing_stores() -> Vec<&'static str> {
    STORES.iter().copied().filter(|store| !registry_key_exists(&format!(r"{STORE_ROOT}\{store}"))).collect()
}

fn registry_key_exists(key: &str) -> bool {
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn RegOpenKeyExW(key: isize, subkey: *const u16, options: u32, access: u32, result: *mut isize) -> i32;
        fn RegCloseKey(key: isize) -> i32;
    }
    const HKEY_LOCAL_MACHINE: isize = 0x8000_0002_u32 as i32 as isize;
    const KEY_READ: u32 = 0x0002_0019;
    let mut handle = 0isize;
    let opened = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, crate::com::wide(key).as_ptr(), 0, KEY_READ, &mut handle) } == 0;
    if opened {
        unsafe { RegCloseKey(handle) };
    }
    opened
}

/// The window's banner: the most important missing prerequisite, as a French sentence marked for
/// translation (`i18n::tr` it before showing) and how bad it is, or `None`. GG running is left
/// out: the window already says so from the daemon's state.
pub fn banner(found: &Findings) -> Option<(&'static str, Level)> {
    if found.windows.is_some_and(|w| !w.supported()) || !found.x64 {
        return Some((mark("Cette version de Windows n'est pas prise en charge par le pilote Sonar (Windows 10 1809 ou plus récent, 64 bits)."), Level::Problem));
    }
    if !found.driver_installed() {
        return Some((mark("Le pilote Sonar n'est pas installé : lancez Platinum-Setup.exe pour l'installer."), Level::Problem));
    }
    if !found.stores_missing.is_empty() {
        return Some((mark("Les réglages du module audio Sonar manquent : relancez Platinum-Setup.exe pour réparer l'installation."), Level::Problem));
    }
    if found.engine.as_ref().is_some_and(|(_, source)| *source == Source::Missing) {
        return Some((mark("Le moteur d'effets Sonar est introuvable : relancez Platinum-Setup.exe pour réparer l'installation."), Level::Problem));
    }
    if !machine::vc_runtime_ok(found.vc_runtime) {
        return Some((mark("Le runtime Visual C++ manque : les effets du micro ne peuvent pas démarrer. Relancez Platinum-Setup.exe."), Level::Warning));
    }
    None
}

/// Every check, in the order printed.
pub fn run() -> Vec<Check> {
    let found = Findings::gather();
    let mut checks = Vec::new();

    let windows = found.windows.unwrap_or(machine::WindowsVersion { major: 0, minor: 0, build: 0 });
    let detail = format!("Windows {}.{} build {}, {}", windows.major, windows.minor, windows.build, if found.x64 { "x64" } else { "not x64" });
    checks.push(if windows.supported() && found.x64 {
        check("Windows", Level::Ok, detail, None)
    } else {
        check("Windows", Level::Problem, detail, Some("the Sonar driver needs Windows 10 1809 (build 17763) or later, on an x64 processor"))
    });

    let devices: Vec<String> = found.devices.iter().map(|d| format!("{} ({})", d.instance, if d.present { "present" } else { "not present" })).collect();
    checks.push(match found.devices.len() {
        0 => check("Sonar device", Level::Problem, format!("no device with hardware id {}", machine::SONAR_HWID), Some(SETUP)),
        1 if found.devices[0].present => check("Sonar device", Level::Ok, devices.join(", "), None),
        1 => check("Sonar device", Level::Warning, devices.join(", "), Some("the device exists but is not started: restart Windows, or reinstall the driver with Platinum-Setup.exe")),
        _ => check("Sonar device", Level::Warning, devices.join(", "), Some("more than one Sonar device: remove the extra one in Device Manager (Sound, video and game controllers), or reinstall with Platinum-Setup.exe")),
    });

    let staged: Vec<String> = found.packages.iter().map(|(inf, folder)| format!("{inf}: {}", folder.as_ref().map_or("not staged".to_string(), |f| f.display().to_string()))).collect();
    let all_staged = found.packages.iter().all(|(_, folder)| folder.is_some());
    checks.push(check("Driver packages", if all_staged { Level::Ok } else { Level::Problem }, staged.join("; "), (!all_staged).then_some(SETUP)));

    checks.push(endpoint_check());

    checks.push(if found.stores_missing.is_empty() {
        check("Settings stores", Level::Ok, format!(r"HKLM\{STORE_ROOT}\{{{}}}", STORES.join(", ")), None)
    } else {
        check("Settings stores", Level::Problem, format!(r"missing under HKLM\{STORE_ROOT}: {}", found.stores_missing.join(", ")), Some("run Platinum-Setup.exe again (it runs SteelSeries' Sonar.AgsSetup.exe, which creates them)"))
    });

    let (api, api_source) = sonarfiles::api_dll();
    checks.push(dll_check("Settings API", &api, api_source));
    let (engine, engine_source) = found.engine.clone().unwrap_or_else(sonarfiles::engine_dll);
    let mut engine_check = dll_check("Effects engine", &engine, engine_source);
    if engine_source != Source::Missing {
        let missing = engine.parent().map(sonarfiles::missing_apo_files).unwrap_or_default();
        if !missing.is_empty() {
            engine_check.level = Level::Problem;
            engine_check.detail.push_str(&format!("; missing next to it: {}", missing.join(", ")));
            engine_check.fix = Some(SETUP.into());
        }
    }
    checks.push(engine_check);
    let package = sonarfiles::apo_package_folder();
    checks.push(check("APO package", if package.is_some() { Level::Ok } else { Level::Problem }, package.map_or("no DriverStore folder of sonar.apo.inf".to_string(), |p| p.display().to_string()), None));

    checks.push(match found.vc_runtime {
        Some((major, minor, build)) if machine::vc_runtime_ok(found.vc_runtime) => check("VC++ runtime", Level::Ok, format!("Visual C++ 2015-2022 x64 runtime {major}.{minor}.{build}"), None),
        Some((major, minor, build)) => check("VC++ runtime", Level::Problem, format!("Visual C++ x64 runtime {major}.{minor}.{build} is too old (14.29 or later needed)"), Some("install the latest from https://aka.ms/vs/17/release/vc_redist.x64.exe, or run Platinum-Setup.exe")),
        None => check("VC++ runtime", Level::Problem, "Visual C++ 2015-2022 x64 runtime not found: the microphone effects engine (onnxruntime) cannot load", Some("install it from https://aka.ms/vs/17/release/vc_redist.x64.exe, or run Platinum-Setup.exe")),
    });

    let gg = if found.gg_installed { format!("installed{}", machine::gg_version().map(|v| format!(" ({v})")).unwrap_or_default()) } else { "not installed".to_string() };
    checks.push(check("GG", Level::Ok, gg, None));
    checks.push(if found.gg_running.is_empty() {
        check("GG running", Level::Ok, "no GG program runs", None)
    } else {
        check("GG running", Level::Warning, found.gg_running.join(", "), Some("quit SteelSeries GG (icon next to the clock, Quit): while GG's Sonar runs, Platinum leaves the devices to it"))
    });
    checks.push(match crate::autostart::gg_start_entry() {
        None => check("GG at logon", Level::Ok, "GG does not start at logon", None),
        Some((_, true)) => check("GG at logon", Level::Ok, "GG's start at logon is disabled (Task Manager)", None),
        Some((command, false)) => check("GG at logon", Level::Warning, format!("GG starts at logon: {command}"), Some("disable \"SteelSeries GG\" in Task Manager, Startup apps")),
    });
    checks.push(if sonarfiles::installed_by_platinum() {
        check("Driver owner", Level::Ok, format!("installed by Platinum's setup (copy in {})", sonarfiles::driver_copy().display()), None)
    } else if found.gg_installed {
        check("Driver owner", Level::Ok, "installed by GG (uninstalling GG removes it)", None)
    } else {
        check("Driver owner", Level::Ok, "no record of who installed it", None)
    });

    checks.push(match library::default_source() {
        Some((path, Origin::Database)) => check("Presets", Level::Ok, format!("GG's database {}", path.display()), None),
        Some((path, Origin::Migrations)) => match library::Library::from_migrations(&path) {
            Ok(library) => check("Presets", if library.skipped.is_empty() { Level::Ok } else { Level::Warning }, format!("rebuilt from GG's migrations in {}: {} presets{}", path.display(), library.configs.len(), if library.skipped.is_empty() { String::new() } else { format!(", {} skipped", library.skipped.len()) }), None),
            Err(error) => check("Presets", Level::Warning, error.to_string(), Some(SETUP)),
        },
        None => check("Presets", Level::Warning, "neither GG's database nor its migrations: only Platinum's neutral preset and your own", Some(SETUP)),
    });

    let samples = gg_samples_folder();
    checks.push(if samples.is_dir() {
        check("Test sounds", Level::Ok, format!("GG's samples in {}", samples.display()), None)
    } else {
        check("Test sounds", Level::Ok, "GG's samples are absent: the window plays synthetic test sounds", None)
    });

    let install = crate::autostart::install_dir().unwrap_or_default();
    let installed = install.join("platinum.exe").is_file();
    checks.push(check("Platinum", if installed { Level::Ok } else { Level::Warning }, format!("{} {}; {}", install.display(), if installed { "installed" } else { "not installed" }, if crate::autostart::starts_at_logon() { "starts at logon" } else { "does not start at logon" }), (!installed).then_some("run Platinum-Setup.exe")));
    let published = crate::status::read();
    checks.push(match &published {
        Some(status) => {
            let message = if status.message.is_empty() { String::new() } else { format!(": {}", status.message) };
            check("Daemon", if status.state.is_live() { Level::Ok } else { Level::Warning }, format!("{} (status of {}){message}", status.state.key(), status.updated), None)
        }
        None => check("Daemon", Level::Warning, "no status published (never started on this account)", Some("start Platinum from the Start menu")),
    });
    checks.push(mic_worker_check(published.as_ref()));
    checks.push(crash_check(&crate::crash::list()));
    checks
}

/// The crash reports kept on this PC (`crash`): how many, and the newest one. Information only:
/// one crash is no fault of this machine.
fn crash_check(reports: &[crate::crash::Summary]) -> Check {
    match reports.first() {
        None => check("Crash reports", Level::Ok, "none on this PC", None),
        Some(newest) => check(
            "Crash reports",
            Level::Ok,
            format!("{} on this PC, newest {} ({}): {}", reports.len(), crate::crash::readable_stamp(&newest.stamp), newest.program, newest.reason),
            Some("`platinum crashes` lists them; the window's settings (Crash reports) can send one, anonymised, after you read it"),
        ),
    }
}

/// The microphone worker (`worker`): its process and memory, the last time it was replaced, or the
/// microphone relayed inside the daemon because no worker could start.
fn mic_worker_check(published: Option<&crate::status::Status>) -> Check {
    use crate::status::State;
    let Some(status) = published.filter(|status| status.state.is_live()) else { return check("Mic worker", Level::Ok, "none (the daemon does not run)", None) };
    let last = if status.replaced.is_empty() { "not replaced yet".to_string() } else { format!("last replaced {}", status.replaced) };
    if status.worker != 0 {
        return check("Mic worker", Level::Ok, format!("{}; {last}", crate::daemon::worker_text(true, status.worker)), None);
    }
    if matches!(status.state, State::Running | State::Degraded) && !status.microphone.is_empty() {
        return check("Mic worker", Level::Warning, format!("the microphone is relayed inside the daemon (no worker could start, or the daemon is of a version without workers); {last}"), Some("restart Platinum; daemon.log says why no worker starts (\"no mic worker\")"));
    }
    check("Mic worker", Level::Ok, format!("none runs now (no microphone relayed at the moment); {last}"), None)
}

/// GG's test sounds (`%ProgramData%\SteelSeries\GG\apps\sonar\audio_samples`).
pub fn gg_samples_folder() -> PathBuf {
    sonarfiles::program_data().join(r"SteelSeries\GG\apps\sonar\audio_samples")
}

fn dll_check(name: &'static str, path: &std::path::Path, source: Source) -> Check {
    match source {
        Source::Missing => check(name, Level::Problem, format!("not found (looked for GG's {}, the DriverStore and Platinum's copy)", path.display()), Some(SETUP)),
        source => check(name, if path.is_file() { Level::Ok } else { Level::Problem }, format!("{} ({})", path.display(), source.label()), None),
    }
}

/// The virtual endpoints the driver made, by role, and their state.
fn endpoint_check() -> Check {
    let listed = Devices::open().and_then(|devices| devices.list());
    let endpoints = match listed {
        Ok(endpoints) => endpoints,
        Err(error) => return check("Sonar endpoints", Level::Problem, format!("cannot list the audio devices: {error}"), None),
    };
    let virtual_ones: Vec<_> = endpoints.iter().filter(|e| crate::session::virtual_kind(e).is_some()).collect();
    if virtual_ones.is_empty() {
        return check("Sonar endpoints", Level::Problem, "no Sonar audio device in Windows", Some(SETUP));
    }
    let describe = |flow: Flow| -> String {
        virtual_ones
            .iter()
            .filter(|e| e.flow == flow)
            .map(|e| format!("{} {}", crate::session::virtual_kind(e).unwrap_or_default(), if e.state == State::Active { "active" } else { e.state.label() }))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let renders = virtual_ones.iter().filter(|e| e.flow == Flow::Render).count();
    let captures = virtual_ones.len() - renders;
    let detail = format!("{renders} outputs ({}), {captures} inputs ({})", describe(Flow::Render), describe(Flow::Capture));
    // Disabled endpoints are normal while neither Platinum nor GG runs: both enable them.
    check("Sonar endpoints", if renders >= 6 && captures >= 2 { Level::Ok } else { Level::Warning }, detail, (renders < 6 || captures < 2).then_some("some Sonar devices are missing: reinstall the driver with Platinum-Setup.exe"))
}

/// Prints the checks, then a one-line summary.
pub fn print(checks: &[Check]) {
    println!("platinum doctor: what Platinum needs on this machine (read-only)");
    for c in checks {
        println!("  {:<8} {:<16} {}", c.level.label(), c.name, c.detail);
        if let Some(fix) = &c.fix {
            println!("  {:<8} {:<16} -> {fix}", "", "");
        }
    }
    let problems = checks.iter().filter(|c| c.level == Level::Problem).count();
    let warnings = checks.iter().filter(|c| c.level == Level::Warning).count();
    println!("{problems} problem(s), {warnings} warning(s)");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy() -> Findings {
        Findings {
            windows: Some(machine::WindowsVersion { major: 10, minor: 0, build: 26200 }),
            x64: true,
            devices: vec![machine::SonarDevice { instance: r"ROOT\MEDIA\0000".into(), present: true }],
            packages: vec![("a.inf", Some(PathBuf::from("x"))), ("b.inf", Some(PathBuf::from("y")))],
            stores_missing: Vec::new(),
            vc_runtime: Some((14, 44, 0)),
            gg_installed: false,
            gg_running: Vec::new(),
            engine: Some((PathBuf::from("Sonar.APO.dll"), Source::DriverStore)),
        }
    }

    #[test]
    fn crash_reports_are_counted_never_a_fault() {
        let none = crash_check(&[]);
        assert_eq!((none.level, none.detail.as_str()), (Level::Ok, "none on this PC"));
        let newest = crate::crash::Summary { file: PathBuf::new(), name: "20261122-153000-mic-worker.txt".into(), stamp: "20261122-153000".into(), program: "mic-worker".into(), reason: "access violation (0xC0000005) in Sonar.APO.dll+0x1A2B3".into() };
        let two = crash_check(&[newest.clone(), newest]);
        assert_eq!(two.level, Level::Ok);
        assert_eq!(two.detail, "2 on this PC, newest 2026-11-22 15:30:00 (mic-worker): access violation (0xC0000005) in Sonar.APO.dll+0x1A2B3");
        assert!(two.fix.unwrap().contains("platinum crashes"));
    }

    #[test]
    fn the_banner_names_the_most_important_problem() {
        assert_eq!(banner(&healthy()), None);
        let mut found = healthy();
        // GG running is the daemon's to say, not the banner's.
        found.gg_running = vec!["SteelSeriesSonar.exe".into()];
        assert_eq!(banner(&found), None);
        found.vc_runtime = None;
        assert!(banner(&found).is_some_and(|(text, level)| text.contains("Visual C++") && level == Level::Warning));
        found.engine = Some((PathBuf::from("Sonar.APO.dll"), Source::Missing));
        assert!(banner(&found).is_some_and(|(text, _)| text.contains("moteur d'effets")));
        found.stores_missing = vec!["Game"];
        assert!(banner(&found).is_some_and(|(text, _)| text.contains("réglages du module")));
        found.packages[1].1 = None;
        assert!(banner(&found).is_some_and(|(text, level)| text.contains("pilote Sonar n'est pas installé") && level == Level::Problem));
        found.x64 = false;
        assert!(banner(&found).is_some_and(|(text, _)| text.contains("Windows")));
    }

    #[test]
    fn the_mic_worker_check_tells_a_worker_from_a_relay_inside_the_daemon() {
        use crate::status::{State, Status};
        assert_eq!(mic_worker_check(None).level, Level::Ok);
        let mut status = Status::new(State::Running);
        status.microphone = "Microphone (Razer Seiren Mini)".into();
        // Running with a microphone and no worker: relayed inside the daemon.
        let inside = mic_worker_check(Some(&status));
        assert!(inside.level == Level::Warning && inside.detail.contains("inside the daemon") && inside.detail.contains("not replaced yet"), "{inside:?}");
        // A worker (this test program stands in for one): its memory, and the last replacement.
        status.worker = std::process::id();
        status.replaced = "20261122-153000, 412 MB, gap 140 ms".into();
        let worker = mic_worker_check(Some(&status));
        assert!(worker.level == Level::Ok && worker.detail.contains("private") && worker.detail.ends_with("last replaced 20261122-153000, 412 MB, gap 140 ms"), "{worker:?}");
        // Waiting for devices, or stopped: nothing wrong.
        status.worker = 0;
        status.state = State::WaitingDevices;
        assert_eq!(mic_worker_check(Some(&status)).level, Level::Ok);
        status.state = State::Stopped;
        assert!(mic_worker_check(Some(&status)).detail.contains("does not run"));
    }

    #[test]
    fn levels_order_from_ok_to_problem() {
        assert!(Level::Ok < Level::Warning && Level::Warning < Level::Problem);
        assert_eq!(Level::Problem.label(), "problem");
    }
}
