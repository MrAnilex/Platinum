//! What the setup will do on this machine: what is there (`Machine`, read only), what is needed
//! (`Plan`), the steps in order, the exact SteelSeries commands, and the dry run's description.
//! Everything that decides is pure, so that the tests and `--dry-run` see what the install does.

use platinum::i18n::mark;
use platinum::library::Origin;
use platinum::machine::{self, WindowsVersion};
use platinum::sonarfiles::{self, APO_PACKAGE, DEVICE_TOOL, MIGRATIONS, VAD_PACKAGE};
use std::path::{Path, PathBuf};

/// GG's installer the setup was tested with: its extraction was compared, file by file, with
/// 7-Zip's (nsis.rs tests), and its hash is pinned. Older installers stay online at versioned
/// addresses.
pub const PINNED_VERSION: &str = "123.0.0";
pub const PINNED_URL: &str = "https://engine.steelseriescdn.com/SteelSeriesGG123.0.0Setup.exe";
pub const PINNED_SIZE: u64 = 433_651_280;
pub const PINNED_SHA256: &str = "e8846a8793b707e2829702112fe47314ed3dd2433ab80dc42099e33b68288bcc";
/// SteelSeries' link to the newest installer (found in GG's own code), used only when the pinned
/// one is gone; it is then checked by its signature, and the files must all be found in it.
pub const LATEST_URL: &str = "https://steelseries.com/gg/downloads/gg/latest/windows";
pub const VC_URL: &str = "https://aka.ms/vs/17/release/vc_redist.x64.exe";
/// Where the setup keeps SteelSeries GG's licence text next to its copy of the driver package
/// (`%ProgramData%\Platinum\sonar`), so that a later run reusing the copy shows it again.
pub const LICENSE_FILE: &str = "SteelSeries-GG-License.txt";
/// The name of the installer that licence came from, beside it.
pub const LICENSE_FROM: &str = "SteelSeries-GG-License.from";
pub const MICROSOFT: &str = "Microsoft Corporation";
pub const WHQL: &str = "Microsoft Windows Hardware Compatibility Publisher";

/// The driver package and SteelSeries' device tool, as laid out in GG's installer (and in
/// Platinum's copy in `%ProgramData%\Platinum\sonar`).
pub const DRIVER_FILES: [&str; 15] = [
    r"apps\sonar\driver\apoDriverPackage\DirectML.dll",
    r"apps\sonar\driver\apoDriverPackage\DnnProcessor.dll",
    r"apps\sonar\driver\apoDriverPackage\microsoft.ai.machinelearning.dll",
    r"apps\sonar\driver\apoDriverPackage\onnxruntime.dll",
    r"apps\sonar\driver\apoDriverPackage\Sonar.AgsSetup.exe",
    r"apps\sonar\driver\apoDriverPackage\sonar.apo.cat",
    r"apps\sonar\driver\apoDriverPackage\Sonar.APO.dll",
    r"apps\sonar\driver\apoDriverPackage\Sonar.Apo.inf",
    r"apps\sonar\driver\apoDriverPackage\Sonar.APOAPI.dll",
    r"apps\sonar\driver\vad\SteelSeries-Sonar-VAD-Extension.cat",
    r"apps\sonar\driver\vad\SteelSeries-Sonar-VAD-Extension.inf",
    r"apps\sonar\driver\vad\SteelSeries-Sonar-VAD.cat",
    r"apps\sonar\driver\vad\SteelSeries-Sonar-VAD.inf",
    r"apps\sonar\driver\vad\SteelSeries-Sonar-VAD.sys",
    DEVICE_TOOL,
];
/// The two programs run as administrator, which must be signed by SteelSeries' signer, and the
/// three driver catalogs, which must be signed by Microsoft (WHQL).
pub const SIGNED_TOOLS: [&str; 2] = [r"apps\sonar\driver\apoDriverPackage\Sonar.AgsSetup.exe", DEVICE_TOOL];
pub const CATALOGS: [&str; 3] = [r"apps\sonar\driver\apoDriverPackage\sonar.apo.cat", r"apps\sonar\driver\vad\SteelSeries-Sonar-VAD.cat", r"apps\sonar\driver\vad\SteelSeries-Sonar-VAD-Extension.cat"];

/// The files taken out of GG's installer: the driver package, the device tool and the preset
/// migrations (about 42 MB of 1.3 GB). Paths as nsis.rs gives them.
pub fn wanted(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.starts_with(r"apps\sonar\driver\") || lower.starts_with(r"apps\sonar\db-migrations\") || lower == DEVICE_TOOL.to_ascii_lowercase()
}

/// What the setup's copy keeps (`%ProgramData%\Platinum\sonar`): the wanted files, and the
/// licence they were accepted under.
pub fn kept_in_copy(path: &str) -> bool {
    wanted(path) || path.eq_ignore_ascii_case(LICENSE_FILE) || path.eq_ignore_ascii_case(LICENSE_FROM)
}

/// What a folder laid out like GG's lacks of the driver files (empty: complete).
pub fn missing_driver_files(root: &Path) -> Vec<&'static str> {
    DRIVER_FILES.iter().copied().filter(|file| !root.join(file).is_file()).collect()
}

/// Whether a folder laid out like GG's holds migrations.
pub fn has_migrations(root: &Path) -> bool {
    std::fs::read_dir(root.join(MIGRATIONS)).is_ok_and(|mut entries| entries.next().is_some())
}

/// A program and the exact arguments to give it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub program: PathBuf,
    pub arguments: String,
}

impl Command {
    /// As it would be typed (and as GG's installer logs it).
    pub fn line(&self) -> String {
        format!("\"{}\" {}", self.program.display(), self.arguments)
    }
}

fn package(root: &Path, which: &str) -> String {
    root.join(which).display().to_string()
}

/// GG's first command (its installer, `[NSIS].nsi` func_11059): the settings stores of the APO,
/// one per channel.
pub fn ags_setup(root: &Path) -> Command {
    Command { program: root.join(APO_PACKAGE).join("Sonar.AgsSetup.exe"), arguments: r#""Game" "ChatRender" "ChatCapture" "Media" "Aux""#.into() }
}

/// GG's second command (func_11032): stages the three packages, creates the root device, restarts
/// Windows Audio when a package was new.
pub fn add_device(root: &Path) -> Command {
    let (vad, apo) = (package(root, VAD_PACKAGE), package(root, APO_PACKAGE));
    Command {
        program: root.join(DEVICE_TOOL),
        arguments: format!(r#"add --device-hwid "{}" --inf "{vad}\SteelSeries-Sonar-VAD.inf" --inf "{apo}\Sonar.Apo.inf" --inf "{vad}\SteelSeries-Sonar-VAD-Extension.inf" "#, machine::SONAR_HWID),
    }
}

/// GG's third command (func_11032): COM registration of the two APO DLLs of the DriverStore copy.
pub fn register(root: &Path) -> Command {
    Command { program: root.join(DEVICE_TOOL), arguments: format!(r#"register --cat="sonar.apo.cat" --com="Sonar.APO.dll" --com="Sonar.APOAPI.dll" --inf "{}\Sonar.Apo.inf" "#, package(root, APO_PACKAGE)) }
}

/// GG's uninstaller, first command (its func_1132).
pub fn unregister(root: &Path) -> Command {
    Command { program: root.join(DEVICE_TOOL), arguments: format!(r#"unregister --cat="sonar.apo.cat" --com="Sonar.APO.dll" --com="Sonar.APOAPI.dll" --inf "{}\Sonar.Apo.inf""#, package(root, APO_PACKAGE)) }
}

/// GG's uninstaller, second command, for the Sonar device only (GG also names its Engine device,
/// which Platinum never installs).
pub fn remove_device(root: &Path) -> Command {
    let (vad, apo) = (package(root, VAD_PACKAGE), package(root, APO_PACKAGE));
    Command {
        program: root.join(DEVICE_TOOL),
        arguments: format!(
            r#"remove --device-hwid "{}" --inf "{vad}\SteelSeries-Sonar-VAD-Extension.inf" --inf "{vad}\SteelSeries-Sonar-VAD.inf" --inf "{apo}\Sonar.Apo.inf" --catalog "SteelSeries.Sonar.VAD.cat" --catalog "SteelSeries.Sonar.VAD.Extension.cat""#,
            machine::SONAR_HWID
        ),
    }
}

/// The settings branch GG's uninstaller deletes after the device.
pub const SETTINGS_KEY: &str = r"SOFTWARE\SteelSeries ApS\Sonar.APO";
pub const VENDOR_KEY: &str = r"SOFTWARE\SteelSeries ApS";

/// What is on this machine, read once (read only).
#[derive(Clone, Debug)]
pub struct Machine {
    pub windows: WindowsVersion,
    pub x64: bool,
    pub gg_installed: bool,
    pub gg_version: Option<String>,
    pub gg_running: Vec<String>,
    pub gg_at_logon: bool,
    pub devices: usize,
    pub packages_staged: bool,
    pub stores_missing: Vec<&'static str>,
    pub vc_runtime: Option<(u32, u32, u32)>,
    pub presets: Option<(PathBuf, Origin)>,
    /// Platinum's copy of the driver package (`%ProgramData%\Platinum\sonar`) is complete, with
    /// the licence text it was accepted under.
    pub copy_complete: bool,
    pub installed_by_platinum: bool,
    pub install_dir: PathBuf,
    pub platinum_installed: bool,
}

impl Machine {
    pub fn gather() -> Machine {
        let install_dir = platinum::autostart::install_dir().unwrap_or_else(|_| PathBuf::from(r"C:\Users\Default\Platinum"));
        let copy = sonarfiles::driver_copy();
        Machine {
            windows: machine::windows_version(),
            x64: machine::is_x64(),
            gg_installed: machine::gg_installed(),
            gg_version: machine::gg_version(),
            gg_running: machine::gg_running(),
            gg_at_logon: platinum::autostart::gg_start_entry().is_some_and(|(_, disabled)| !disabled),
            devices: machine::sonar_devices().len(),
            packages_staged: machine::staged_packages().iter().all(|(_, folder)| folder.is_some()),
            stores_missing: platinum::doctor::missing_stores(),
            vc_runtime: machine::vc_runtime(),
            presets: platinum::library::default_source(),
            copy_complete: missing_driver_files(&copy).is_empty() && copy.join(LICENSE_FILE).is_file(),
            installed_by_platinum: sonarfiles::installed_by_platinum(),
            platinum_installed: install_dir.join("platinum.exe").is_file(),
            install_dir,
        }
    }

    /// The Sonar driver is there and complete: device, packages and settings stores.
    pub fn driver_ok(&self) -> bool {
        self.devices > 0 && self.packages_staged && self.stores_missing.is_empty()
    }
}

/// The user's choices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub autostart: bool,
    pub desktop_shortcut: bool,
    /// `crash_reports = ask` in platinum.ini: after a crash, the window offers to send the report
    /// (anonymised, as a GitHub issue the user sends). Unticked unless platinum.ini says so already.
    pub crash_reports: bool,
    /// A GG installer already on disk, instead of downloading one (`--gg-installer`).
    pub gg_installer: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Options {
        Options { autostart: true, desktop_shortcut: false, crash_reports: false, gg_installer: None }
    }
}

/// Where the SteelSeries files come from, when they are needed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    NotNeeded,
    /// Platinum's copy from an earlier run is complete: no download.
    Copy,
    Download,
    Local(PathBuf),
}

/// What stops the install until the user does something.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Blocker {
    /// The driver needs Windows 10 1809 or later.
    Windows(WindowsVersion),
    /// The driver has no ARM build.
    NotX64,
    /// GG runs: it would fight over the devices, and the driver's update restarts Windows Audio
    /// under it.
    GgRunning(Vec<String>),
}

/// What the install needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub blockers: Vec<Blocker>,
    /// Install Microsoft's Visual C++ runtime (the effects engine's onnxruntime needs it).
    pub vc_runtime: bool,
    /// Run SteelSeries' tools to install the Sonar driver.
    pub driver: bool,
    pub source: Source,
    /// Platinum is already installed: this is an update (settings, presets and state are kept).
    pub update: bool,
}

impl Plan {
    /// Whether anything needs administrator rights (one Windows prompt for all of it).
    pub fn elevated(&self) -> bool {
        self.vc_runtime || self.driver || matches!(self.source, Source::Download | Source::Local(_))
    }

    /// Whether SteelSeries GG's licence is shown and must be accepted before anything is installed:
    /// SteelSeries' files are taken from their installer (or from the copy of an earlier run).
    pub fn needs_license(&self) -> bool {
        self.source != Source::NotNeeded
    }
}

pub fn plan(machine: &Machine, options: &Options) -> Plan {
    let driver = !machine.driver_ok();
    let presets = machine.presets.is_none();
    let source = if !driver && !presets {
        Source::NotNeeded
    } else if let Some(local) = &options.gg_installer {
        Source::Local(local.clone())
    } else if machine.copy_complete && (has_migrations(&sonarfiles::driver_copy()) || !presets) {
        Source::Copy
    } else {
        Source::Download
    };
    let mut blockers = Vec::new();
    if driver && !machine.windows.supported() {
        blockers.push(Blocker::Windows(machine.windows));
    }
    if driver && !machine.x64 {
        blockers.push(Blocker::NotX64);
    }
    if !machine.gg_running.is_empty() {
        blockers.push(Blocker::GgRunning(machine.gg_running.clone()));
    }
    Plan { blockers, vc_runtime: !machine::vc_runtime_ok(machine.vc_runtime), driver, source, update: machine.platinum_installed }
}

/// One step of the install, in the order they run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    DownloadRuntime,
    DownloadGg,
    /// SteelSeries GG's licence, read from their installer, shown, accepted or declined.
    License,
    ExtractGg,
    Elevated,
    CheckDriver,
    StopPlatinum,
    CopyFiles,
    Autostart,
    Shortcuts,
    UninstallEntry,
    StartPlatinum,
    Cleanup,
}

impl Step {
    /// The line in the window's list (French, a catalog key).
    pub fn title(self) -> &'static str {
        match self {
            Step::DownloadRuntime => mark("Télécharger le runtime Visual C++ de Microsoft"),
            Step::DownloadGg => mark("Télécharger l'installateur officiel de SteelSeries GG"),
            Step::License => mark("Lire et accepter la licence de SteelSeries GG"),
            Step::ExtractGg => mark("Extraire le pilote Sonar et les préréglages"),
            Step::Elevated => mark("Installer le pilote Sonar (administrateur)"),
            Step::CheckDriver => mark("Vérifier le pilote et les périphériques audio"),
            Step::StopPlatinum => mark("Arrêter Platinum"),
            Step::CopyFiles => mark("Copier Platinum"),
            Step::Autostart => mark("Démarrage avec Windows"),
            Step::Shortcuts => mark("Raccourcis"),
            Step::UninstallEntry => mark("Inscrire Platinum dans les applications installées"),
            Step::StartPlatinum => mark("Démarrer Platinum"),
            Step::Cleanup => mark("Supprimer les fichiers téléchargés"),
        }
    }
}

/// The steps for a plan. When SteelSeries' files are used, their installer comes first, then
/// their licence; nothing is installed before it is accepted (not even Microsoft's runtime).
pub fn steps(plan: &Plan) -> Vec<Step> {
    let mut list = Vec::new();
    match plan.source {
        Source::Download => list.extend([Step::DownloadGg, Step::License]),
        Source::Local(_) | Source::Copy => list.push(Step::License),
        Source::NotNeeded => {}
    }
    if plan.vc_runtime {
        list.push(Step::DownloadRuntime);
    }
    if matches!(plan.source, Source::Download | Source::Local(_)) {
        list.push(Step::ExtractGg);
    }
    if plan.elevated() {
        list.push(Step::Elevated);
    }
    if plan.driver {
        list.push(Step::CheckDriver);
    }
    if plan.update {
        list.push(Step::StopPlatinum);
    }
    list.extend([Step::CopyFiles, Step::Autostart, Step::Shortcuts, Step::UninstallEntry, Step::StartPlatinum]);
    if plan.vc_runtime || matches!(plan.source, Source::Download | Source::Local(_)) {
        list.push(Step::Cleanup);
    }
    list
}

/// The setup's work folder for downloads and extraction (`%TEMP%\Platinum-Setup`).
pub fn work_folder() -> PathBuf {
    std::env::temp_dir().join("Platinum-Setup")
}

/// The dry run: every step with what it would do on this machine, in English.
pub fn describe(machine: &Machine, options: &Options, plan: &Plan) -> Vec<String> {
    let mut lines = Vec::new();
    let w = &machine.windows;
    lines.push(format!("machine: Windows {}.{} build {} {}", w.major, w.minor, w.build, if machine.x64 { "x64" } else { "(not x64)" }));
    lines.push(format!(
        "  GG: {}{}{}",
        if machine.gg_installed { "installed" } else { "not installed" },
        machine.gg_version.as_ref().map(|v| format!(" ({v})")).unwrap_or_default(),
        if machine.gg_running.is_empty() { String::new() } else { format!(", running: {}", machine.gg_running.join(", ")) }
    ));
    lines.push(format!(
        "  Sonar driver: {} device(s) {}, packages {}, settings stores {}",
        machine.devices,
        machine::SONAR_HWID,
        if machine.packages_staged { "staged" } else { "missing" },
        if machine.stores_missing.is_empty() { "present".to_string() } else { format!("missing ({})", machine.stores_missing.join(", ")) }
    ));
    lines.push(format!("  VC++ x64 runtime: {}", machine.vc_runtime.map_or("missing".to_string(), |(a, b, c)| format!("{a}.{b}.{c}"))));
    lines.push(format!("  presets: {}", machine.presets.as_ref().map_or("none".to_string(), |(path, origin)| format!("{origin:?} {}", path.display()))));
    lines.push(format!("  Platinum: {} in {}", if machine.platinum_installed { "installed" } else { "not installed" }, machine.install_dir.display()));
    lines.push(format!("  Platinum's driver copy: {} ({}), marker {}", if machine.copy_complete { "complete" } else { "absent or incomplete" }, sonarfiles::driver_copy().display(), if machine.installed_by_platinum { "present" } else { "absent" }));
    for blocker in &plan.blockers {
        lines.push(match blocker {
            Blocker::Windows(v) => format!("BLOCKED: Windows build {} is older than 17763 (Windows 10 1809), which the driver needs", v.build),
            Blocker::NotX64 => "BLOCKED: the Sonar driver exists for x64 Windows only".into(),
            Blocker::GgRunning(names) => format!("BLOCKED until GG is closed: {} (the window offers \"Réessayer\")", names.join(", ")),
        });
    }
    let work = work_folder();
    let copy = sonarfiles::driver_copy();
    for (index, step) in steps(plan).iter().enumerate() {
        let n = index + 1;
        match step {
            Step::DownloadRuntime => {
                lines.push(format!("{n}. download {VC_URL} to {}, check it is signed by \"{MICROSOFT}\"", work.join("vc_redist.x64.exe").display()));
            }
            Step::DownloadGg => {
                lines.push(format!("{n}. HEAD {PINNED_URL} (GG {PINNED_VERSION}, {PINNED_SIZE} bytes); if gone, HEAD {LATEST_URL} and use where it leads"));
                lines.push(format!("   download to {} (resumable .part), check SHA-256 {PINNED_SHA256} for {PINNED_VERSION}, and the Authenticode signer \"{}\"", work.join(format!("SteelSeriesGG{PINNED_VERSION}Setup.exe")).display(), crate::GG_SIGNER));
            }
            Step::License => {
                let from = match &plan.source {
                    Source::Copy => format!("{}", copy.join(LICENSE_FILE).display()),
                    _ => "the installer's NSIS licence page (in its header)".into(),
                };
                lines.push(format!("{n}. read SteelSeries GG's licence from {from} and show it in full: nothing more is done unless the user ticks \"J'ai lu et j'accepte la licence de SteelSeries GG\" and clicks Accepter; Refuser deletes {} and installs nothing; the acceptance (time, GG version, SHA-256 of the text) goes to install.log", work.display()));
            }
            Step::ExtractGg => {
                let from = match &plan.source {
                    Source::Local(path) => format!("{} (given; signer \"{}\" checked at the licence step)", path.display(), crate::GG_SIGNER),
                    _ => "the downloaded installer".into(),
                };
                lines.push(format!("{n}. extract from {from}, with the built-in NSIS reader, only apps\\sonar\\driver\\**, {DEVICE_TOOL} and apps\\sonar\\db-migrations\\** to {}, plus the accepted licence text ({LICENSE_FILE}); then delete the downloaded installer", work.join("gg").display()));
            }
            Step::Elevated => {
                let mut parts = Vec::new();
                if plan.vc_runtime {
                    parts.push("VC++ runtime".to_string());
                }
                if plan.source != Source::Copy && plan.source != Source::NotNeeded {
                    parts.push("copy of the driver package".to_string());
                }
                if plan.driver {
                    parts.push("Sonar driver".to_string());
                }
                lines.push(format!("{n}. one Windows administrator prompt, then (in \"Platinum-Setup.exe --elevated …\"): {}", parts.join(", ")));
                lines.push(format!("   create {} readable by users, writable only by administrators and SYSTEM (icacls)", sonarfiles::platinum_data().display()));
                if plan.vc_runtime {
                    lines.push(format!("   copy vc_redist.x64.exe into it, check the signer \"{MICROSOFT}\", run: vc_redist.x64.exe /install /quiet /norestart (0, 1638 and 3010 accepted)"));
                }
                if plan.source != Source::Copy && plan.source != Source::NotNeeded {
                    lines.push(format!("   copy the extracted files and {LICENSE_FILE} into {}, check {} and {} are signed by \"{}\" and the three .cat files by \"{WHQL}\"", copy.display(), SIGNED_TOOLS[0], SIGNED_TOOLS[1], crate::GG_SIGNER));
                }
                if plan.driver {
                    lines.push("   WARNING shown first: Windows Audio restarts, all sound stops for a few seconds".into());
                    for command in [ags_setup(&copy), add_device(&copy), register(&copy)] {
                        lines.push(format!("   run: {}", command.line()));
                    }
                    lines.push(format!("   if \"add\" fails (exit code other than 0 and 1, or cannot start): the same with Windows' own functions: DiInstallDriverW on the VAD, APO and extension INFs; if no device has {}: SetupDiCreateDeviceInfoW(MEDIA, generated id), its hardware id, DIF_REGISTERDEVICE, DiInstallDevice (else UpdateDriverForPlugAndPlayDevicesW); Windows Audio restarted if something is new", machine::SONAR_HWID));
                    lines.push("   if \"register\" fails: regsvr32 /s on Sonar.APO.dll and Sonar.APOAPI.dll of the APO package's DriverStore folder (SetupGetInfDriverStoreLocationW)".into());
                    lines.push(format!("   write {}", copy.join(sonarfiles::MARKER).display()));
                }
            }
            Step::CheckDriver => lines.push(format!("{n}. check the device {}, its three packages, the settings stores and the 8 Sonar endpoints (up to 20 s)", machine::SONAR_HWID)),
            Step::StopPlatinum => lines.push(format!("{n}. \"{}\" daemon --stop, wait for it (20 s), close platinum-ui.exe (taskkill, then /F after 5 s)", machine.install_dir.join("platinum.exe").display())),
            Step::CopyFiles => {
                lines.push(format!("{n}. write platinum.exe, platinum-start.exe, platinum-ui.exe, platinum.ico and Platinum-Setup.exe into {} (platinum.ini, presets\\ and state.json are kept)", machine.install_dir.display()));
                lines.push(format!("   platinum.ini: crash_reports = {} (the other settings kept; channel_names = platinum unless set otherwise, or an earlier Platinum went back to SteelSeries' names)", if options.crash_reports { "ask" } else { "off" }));
            }
            Step::Autostart => lines.push(if options.autostart {
                format!("{n}. \"{}\" autostart enable  (HKCU Run value \"Platinum\")", machine.install_dir.join("platinum.exe").display())
            } else {
                format!("{n}. \"{}\" autostart disable", machine.install_dir.join("platinum.exe").display())
            }),
            Step::Shortcuts => {
                let programs = crate::win::known_folder(&crate::win::PROGRAMS).map(|p| p.join("Platinum.lnk").display().to_string()).unwrap_or_else(|| "(Start menu)".into());
                let desktop = crate::win::known_folder(&crate::win::DESKTOP).map(|p| p.join("Platinum.lnk").display().to_string()).unwrap_or_default();
                lines.push(format!("{n}. Start menu shortcut {programs}{}", if options.desktop_shortcut { format!(", desktop shortcut {desktop}") } else { String::new() }));
            }
            Step::UninstallEntry => lines.push(format!("{n}. HKCU\\{} (DisplayName Platinum, publisher \"{}\", UninstallString \"{}\" --uninstall)", crate::install::UNINSTALL_KEY, crate::PUBLISHER, machine.install_dir.join("Platinum-Setup.exe").display())),
            Step::StartPlatinum => lines.push(format!("{n}. start \"{}\" daemon, then offer \"Ouvrir Platinum\"", machine.install_dir.join("platinum-start.exe").display())),
            Step::Cleanup => lines.push(format!("{n}. delete {}", work.display())),
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine() -> Machine {
        Machine {
            windows: WindowsVersion { major: 10, minor: 0, build: 19045 },
            x64: true,
            gg_installed: false,
            gg_version: None,
            gg_running: Vec::new(),
            gg_at_logon: false,
            devices: 0,
            packages_staged: false,
            stores_missing: vec!["Game"],
            vc_runtime: None,
            presets: None,
            copy_complete: false,
            installed_by_platinum: false,
            install_dir: PathBuf::from(r"C:\Users\Test\Platinum"),
            platinum_installed: false,
        }
    }

    #[test]
    fn a_clean_machine_gets_everything() {
        let plan = plan(&machine(), &Options::default());
        assert!(plan.blockers.is_empty() && plan.vc_runtime && plan.driver && plan.needs_license() && plan.elevated());
        assert_eq!(plan.source, Source::Download);
        // The licence comes right after the download, before anything is installed.
        assert_eq!(steps(&plan), [Step::DownloadGg, Step::License, Step::DownloadRuntime, Step::ExtractGg, Step::Elevated, Step::CheckDriver, Step::CopyFiles, Step::Autostart, Step::Shortcuts, Step::UninstallEntry, Step::StartPlatinum, Step::Cleanup]);
    }

    #[test]
    fn a_machine_with_gg_only_gets_platinum() {
        let mut m = machine();
        (m.gg_installed, m.devices, m.packages_staged, m.stores_missing, m.vc_runtime) = (true, 1, true, Vec::new(), Some((14, 44, 0)));
        m.presets = Some((PathBuf::from("database.db"), Origin::Database));
        m.platinum_installed = true;
        let plan = plan(&m, &Options::default());
        assert!(!plan.driver && !plan.vc_runtime && !plan.needs_license() && !plan.elevated() && plan.update);
        assert_eq!(steps(&plan), [Step::StopPlatinum, Step::CopyFiles, Step::Autostart, Step::Shortcuts, Step::UninstallEntry, Step::StartPlatinum]);
        // GG running blocks, with its programs named.
        m.gg_running = vec!["SteelSeriesGG.exe".into()];
        assert_eq!(super::plan(&m, &Options::default()).blockers, [Blocker::GgRunning(vec!["SteelSeriesGG.exe".into()])]);
    }

    #[test]
    fn crash_reports_are_offered_only_when_ticked() {
        // Unticked by default: the dry run says platinum.ini gets crash_reports = off.
        assert!(!Options::default().crash_reports);
        let m = machine();
        let lines = describe(&m, &Options::default(), &plan(&m, &Options::default()));
        assert!(lines.iter().any(|line| line.contains("platinum.ini: crash_reports = off")), "{lines:?}");
        let ticked = Options { crash_reports: true, ..Options::default() };
        let lines = describe(&m, &ticked, &plan(&m, &ticked));
        assert!(lines.iter().any(|line| line.contains("platinum.ini: crash_reports = ask")), "{lines:?}");
    }

    #[test]
    fn a_local_installer_or_an_earlier_copy_saves_the_download() {
        let mut options = Options { gg_installer: Some(PathBuf::from(r"D:\GG.exe")), ..Options::default() };
        let plan_local = plan(&machine(), &options);
        assert_eq!(plan_local.source, Source::Local(PathBuf::from(r"D:\GG.exe")));
        assert_eq!(steps(&plan_local)[..3], [Step::License, Step::DownloadRuntime, Step::ExtractGg]);
        assert!(!steps(&plan_local).contains(&Step::DownloadGg));
        options.gg_installer = None;
        let mut m = machine();
        // A complete copy with no migrations is enough when presets come from elsewhere.
        m.copy_complete = true;
        m.presets = Some((PathBuf::from("migrations"), Origin::Migrations));
        let from_copy = plan(&m, &options);
        assert_eq!(from_copy.source, Source::Copy);
        assert!(from_copy.driver && from_copy.elevated() && !steps(&from_copy).contains(&Step::ExtractGg));
        // The copy's licence is shown again before the driver is installed from it.
        assert_eq!(steps(&from_copy)[0], Step::License);
    }

    #[test]
    fn old_or_arm_windows_is_refused_only_when_the_driver_is_needed() {
        let mut m = machine();
        m.windows.build = 17134;
        m.x64 = false;
        assert_eq!(plan(&m, &Options::default()).blockers, [Blocker::Windows(m.windows), Blocker::NotX64]);
        (m.devices, m.packages_staged, m.stores_missing) = (1, true, Vec::new());
        m.presets = Some((PathBuf::from("db"), Origin::Database));
        assert!(plan(&m, &Options::default()).blockers.is_empty());
    }

    #[test]
    fn the_commands_are_ggs() {
        let root = Path::new(r"C:\ProgramData\Platinum\sonar");
        assert_eq!(ags_setup(root).line(), r#""C:\ProgramData\Platinum\sonar\apps\sonar\driver\apoDriverPackage\Sonar.AgsSetup.exe" "Game" "ChatRender" "ChatCapture" "Media" "Aux""#);
        assert_eq!(
            add_device(root).line(),
            r#""C:\ProgramData\Platinum\sonar\shared\Steelseries.AudioDeviceInstaller.exe" add --device-hwid "ROOT\VEN_SSGG&DEV_0001" --inf "C:\ProgramData\Platinum\sonar\apps\sonar\driver\vad\SteelSeries-Sonar-VAD.inf" --inf "C:\ProgramData\Platinum\sonar\apps\sonar\driver\apoDriverPackage\Sonar.Apo.inf" --inf "C:\ProgramData\Platinum\sonar\apps\sonar\driver\vad\SteelSeries-Sonar-VAD-Extension.inf" "#
        );
        assert_eq!(register(root).arguments, r#"register --cat="sonar.apo.cat" --com="Sonar.APO.dll" --com="Sonar.APOAPI.dll" --inf "C:\ProgramData\Platinum\sonar\apps\sonar\driver\apoDriverPackage\Sonar.Apo.inf" "#);
        assert!(unregister(root).arguments.starts_with("unregister --cat=\"sonar.apo.cat\""));
        let remove = remove_device(root).arguments;
        assert!(remove.contains(r#"--device-hwid "ROOT\VEN_SSGG&DEV_0001""#) && remove.contains(r#"--catalog "SteelSeries.Sonar.VAD.Extension.cat""#) && !remove.contains("DEV_0002"));
    }

    #[test]
    fn only_the_driver_tool_and_migrations_are_wanted() {
        assert!(wanted(r"apps\sonar\driver\vad\SteelSeries-Sonar-VAD.sys"));
        assert!(wanted(r"apps\sonar\db-migrations\sonar-1.106.0\20261028162350_squash.sql"));
        assert!(wanted(r"shared\Steelseries.AudioDeviceInstaller.exe"));
        assert!(!wanted(r"shared\goose-sqlite.exe"));
        assert!(!wanted(r"apps\sonar\Sonar.APOAPI.dll"));
        assert!(!wanted(r"$_123_\Sonar.AgsSetup.exe"));
        assert!(DRIVER_FILES.iter().all(|f| wanted(f)));
        assert!(kept_in_copy(LICENSE_FILE) && kept_in_copy(LICENSE_FROM) && !wanted(LICENSE_FILE) && !kept_in_copy("notes.txt"));
    }
}
