//! The part of the setup that runs as administrator, started by the user's setup through one
//! Windows prompt (`Platinum-Setup.exe --elevated …` or `--elevated-remove …`). It takes no
//! command to run from anywhere: only where the files are, and what to do.
//!
//! Install: `%ProgramData%\Platinum` is made writable only by administrators and SYSTEM (owner
//! and permissions set, links refused), the Visual C++ runtime is copied there, checked (signed by
//! Microsoft) and installed; the extracted SteelSeries files are copied there laid out like GG's
//! folder and checked (SteelSeries' two programs signed by "GN Hearing A/S", the three driver
//! catalogs by Microsoft's WHQL signer) before anything runs; then SteelSeries' three commands run
//! exactly as GG's installer runs them (`plan::ags_setup`, `add_device`, `register`), and the
//! marker says that Platinum installed the driver. When the device tool's `add` or `register`
//! fails (any exit code but 0 and 1, or it cannot start), `driver` does the same work with
//! Windows' own functions as a second attempt.
//!
//! Remove: SteelSeries' two uninstall commands, as GG's uninstaller runs them for Sonar, each
//! followed by `driver`'s equivalent if it fails, then the settings branch, then
//! `%ProgramData%\Platinum`.
//!
//! Its journal goes to the file the user's setup created (`--log`), reopened for every line and
//! only if it is a plain file (no link); the user's setup copies its lines into install.log as
//! they are written.

use crate::driver;
use crate::install::run_program;
use crate::log::Log;
use crate::plan::{self, Command};
use crate::win;
use platinum::i18n::mark;
use platinum::sonarfiles;
use std::path::{Path, PathBuf};

/// How the elevated part ended, as its exit code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Ok,
    /// Done; Windows wants a restart.
    Reboot,
    NotElevated,
    BadArguments,
    Runtime,
    Copy,
    Signature,
    Driver,
    Remove,
}

impl Exit {
    pub fn code(self) -> u32 {
        match self {
            Exit::Ok => 0,
            Exit::Reboot => 3010,
            Exit::NotElevated => 20,
            Exit::BadArguments => 21,
            Exit::Runtime => 22,
            Exit::Copy => 23,
            Exit::Signature => 24,
            Exit::Driver => 25,
            Exit::Remove => 26,
        }
    }

    pub fn from_code(code: u32) -> Exit {
        [Exit::Ok, Exit::Reboot, Exit::NotElevated, Exit::BadArguments, Exit::Runtime, Exit::Copy, Exit::Signature, Exit::Driver, Exit::Remove].into_iter().find(|e| e.code() == code).unwrap_or(Exit::Driver)
    }

    /// What the window says (French, a catalog key).
    pub fn message(self) -> &'static str {
        match self {
            Exit::Ok | Exit::Reboot => mark("Terminé."),
            Exit::NotElevated => mark("La partie administrateur n'a pas reçu les droits d'administrateur."),
            Exit::BadArguments => mark("La partie administrateur a reçu des arguments incorrects."),
            Exit::Runtime => mark("Le runtime Visual C++ n'a pas pu être installé : voyez le journal."),
            Exit::Copy => mark("Les fichiers du pilote n'ont pas pu être copiés dans %ProgramData%\\Platinum : voyez le journal."),
            Exit::Signature => mark("Les fichiers du pilote ne portent pas les signatures attendues de SteelSeries et de Microsoft : rien n'a été installé."),
            Exit::Driver => mark("Le pilote Sonar n'a pu être installé ni par l'outil de SteelSeries ni par les fonctions de Windows : voyez le journal."),
            Exit::Remove => mark("Le pilote Sonar n'a pu être retiré ni par l'outil de SteelSeries ni par les fonctions de Windows : voyez le journal."),
        }
    }
}

/// The elevated part's arguments.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Arguments {
    pub log: Option<PathBuf>,
    pub vcredist: Option<PathBuf>,
    pub source: Option<PathBuf>,
    pub driver: bool,
    pub from: Option<String>,
}

pub fn parse(args: &[String]) -> Result<Arguments, String> {
    let mut parsed = Arguments::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let mut value = || iter.next().cloned().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--elevated" | "--elevated-remove" => {}
            "--log" => parsed.log = Some(PathBuf::from(value()?)),
            "--vcredist" => parsed.vcredist = Some(PathBuf::from(value()?)),
            "--source" => parsed.source = Some(PathBuf::from(value()?)),
            "--from" => parsed.from = Some(value()?),
            "--driver" => parsed.driver = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(parsed)
}

fn journal(arguments: &Arguments) -> Log {
    match &arguments.log {
        Some(path) => Log::plain(path),
        None => Log::none(),
    }
}

/// `--elevated`: the install's administrator part. Returns the exit code.
pub fn install(args: &[String]) -> u32 {
    let arguments = match parse(args) {
        Ok(arguments) => arguments,
        Err(_) => return Exit::BadArguments.code(),
    };
    let mut log = journal(&arguments);
    let exit = install_with(&arguments, &mut log);
    log.line(format!("elevated part ends: {exit:?} (exit code {})", exit.code()));
    exit.code()
}

fn install_with(arguments: &Arguments, log: &mut Log) -> Exit {
    if !win::is_elevated() {
        log.line("not elevated");
        return Exit::NotElevated;
    }
    let data = sonarfiles::platinum_data();
    if let Err(error) = protected_folder(log, &data) {
        log.line(format!("cannot prepare {}: {error}", data.display()));
        return Exit::Copy;
    }
    let mut reboot = false;
    if let Some(runtime) = &arguments.vcredist {
        match install_runtime(log, &data, runtime) {
            Ok(wants_reboot) => reboot |= wants_reboot,
            Err(error) => {
                log.line(format!("VC++ runtime: {error}"));
                return Exit::Runtime;
            }
        }
    }
    let copy = sonarfiles::driver_copy();
    if let Some(source) = &arguments.source
        && let Err(exit) = copy_package(log, source, &copy)
    {
        return exit;
    }
    if arguments.driver {
        if let Err(exit) = check_package(log, &copy) {
            return exit;
        }
        match install_driver(log, &copy, arguments.from.as_deref()) {
            Ok(wants_reboot) => reboot |= wants_reboot,
            Err(exit) => return exit,
        }
    }
    if reboot { Exit::Reboot } else { Exit::Ok }
}

fn is_link(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink() || m.file_attributes() & 0x400 != 0)
}

/// Creates the folder (a link put there in its place is removed first), makes administrators its
/// owner and lets only administrators and SYSTEM write in it; users may read and run.
fn protected_folder(log: &mut Log, folder: &Path) -> Result<(), String> {
    if is_link(folder) {
        log.line(format!("{} is a link: removed", folder.display()));
        std::fs::remove_dir(folder).or_else(|_| std::fs::remove_file(folder)).map_err(|e| e.to_string())?;
    }
    std::fs::create_dir_all(folder).map_err(|e| e.to_string())?;
    let icacls = sonarfiles::windows_folder().join(r"System32\icacls.exe");
    let target = folder.display().to_string();
    // S-1-5-32-544 Administrators, S-1-5-18 SYSTEM, S-1-5-32-545 Users.
    for arguments in [format!("\"{target}\" /setowner *S-1-5-32-544 /T /C /Q"), format!("\"{target}\" /inheritance:r /grant:r *S-1-5-32-544:(OI)(CI)F *S-1-5-18:(OI)(CI)F *S-1-5-32-545:(OI)(CI)RX /T /C /Q")] {
        let code = run_program(log, &icacls, &arguments, None)?;
        if code != 0 {
            return Err(format!("icacls exited with {code}"));
        }
    }
    Ok(())
}

/// Copies the runtime into the protected folder, checks it there, installs it. Returns whether
/// Windows wants a restart.
fn install_runtime(log: &mut Log, data: &Path, runtime: &Path) -> Result<bool, String> {
    let temp = data.join("temp");
    if is_link(&temp) {
        std::fs::remove_dir(&temp).map_err(|e| e.to_string())?;
    }
    std::fs::create_dir_all(&temp).map_err(|e| e.to_string())?;
    let copy = temp.join("vc_redist.x64.exe");
    std::fs::copy(runtime, &copy).map_err(|e| format!("cannot copy {}: {e}", runtime.display()))?;
    let outcome = (|| {
        win::require_signer(&copy, plan::MICROSOFT)?;
        log.line(format!("{} signed by {}", copy.display(), plan::MICROSOFT));
        let code = run_program(log, &copy, "/install /quiet /norestart", None)?;
        // 1638: a newer version is installed; 3010 and 1641: done, restart wanted.
        match code {
            0 | 1638 => Ok(false),
            3010 | 1641 => Ok(true),
            other => Err(format!("vc_redist.x64.exe exited with {other}")),
        }
    })();
    let _ = std::fs::remove_dir_all(&temp);
    outcome
}

/// Replaces Platinum's copy of the SteelSeries files with the extracted ones (the marker kept),
/// then checks their signatures there.
fn copy_package(log: &mut Log, source: &Path, copy: &Path) -> Result<(), Exit> {
    let marker = std::fs::read(copy.join(sonarfiles::MARKER)).ok();
    if copy.exists() {
        if is_link(copy) {
            let _ = std::fs::remove_dir(copy);
        } else if let Err(error) = std::fs::remove_dir_all(copy) {
            log.line(format!("cannot clear {}: {error}", copy.display()));
            return Err(Exit::Copy);
        }
    }
    let mut copied = 0;
    if let Err(error) = copy_wanted(source, source, copy, &mut copied) {
        log.line(format!("copy into {} failed: {error}", copy.display()));
        return Err(Exit::Copy);
    }
    if let Some(marker) = marker {
        let _ = std::fs::write(copy.join(sonarfiles::MARKER), marker);
    }
    log.line(format!("copied {copied} files from {} to {}", source.display(), copy.display()));
    check_package(log, copy)
}

fn copy_wanted(root: &Path, folder: &Path, target: &Path, copied: &mut usize) -> Result<(), String> {
    for entry in std::fs::read_dir(folder).map_err(|e| format!("{}: {e}", folder.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            copy_wanted(root, &path, target, copied)?;
            continue;
        }
        let relative = path.strip_prefix(root).map_err(|e| e.to_string())?;
        if !plan::kept_in_copy(&relative.display().to_string()) {
            continue;
        }
        let to = target.join(relative);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        std::fs::copy(&path, &to).map_err(|e| format!("{} -> {}: {e}", path.display(), to.display()))?;
        *copied += 1;
    }
    Ok(())
}

/// The copy is complete, and the programs and catalogs carry the expected signatures.
fn check_package(log: &mut Log, copy: &Path) -> Result<(), Exit> {
    let missing = plan::missing_driver_files(copy);
    if !missing.is_empty() {
        log.line(format!("missing from {}: {}", copy.display(), missing.join(", ")));
        return Err(Exit::Copy);
    }
    let checks = plan::SIGNED_TOOLS.iter().map(|f| (*f, crate::GG_SIGNER)).chain(plan::CATALOGS.iter().map(|f| (*f, plan::WHQL)));
    for (file, signer) in checks {
        let path = copy.join(file);
        match win::require_signer(&path, signer) {
            Ok(()) => log.line(format!("{} signed by {signer}", path.display())),
            Err(error) => {
                log.line(format!("signature check failed: {error}"));
                let _ = std::fs::remove_dir_all(copy);
                return Err(Exit::Signature);
            }
        }
    }
    Ok(())
}

/// Runs one SteelSeries command from the copy's folder.
fn run(log: &mut Log, command: &Command, folder: &Path) -> Result<i32, Exit> {
    run_program(log, &command.program, &command.arguments, Some(folder)).map_err(|error| {
        log.line(error);
        Exit::Driver
    })
}

/// Runs one of SteelSeries' device-tool commands; -1 when it cannot start (journaled), so that
/// the fallback takes over.
fn run_tool(log: &mut Log, command: &Command, folder: &Path) -> i32 {
    run_program(log, &command.program, &command.arguments, Some(folder)).unwrap_or_else(|error| {
        log.line(error);
        -1
    })
}

/// SteelSeries' three commands, as GG runs them, with Windows' own functions (`driver`) as the
/// second attempt when the device tool fails. Returns whether Windows wants a restart.
fn install_driver(log: &mut Log, copy: &Path, from: Option<&str>) -> Result<bool, Exit> {
    let existed = !platinum::machine::sonar_devices().is_empty();
    log.line("Windows Audio restarts if a package is new: all sound stops for a few seconds");
    // GG ignores this one's exit code; the stores it creates are checked just after.
    run(log, &plan::ags_setup(copy), copy)?;
    let missing = platinum::doctor::missing_stores();
    if !missing.is_empty() {
        log.line(format!("settings stores still missing after Sonar.AgsSetup.exe: {}", missing.join(", ")));
        return Err(Exit::Driver);
    }
    let mut reboot = false;
    let mut how = "SteelSeries' device tool";
    // 0 done, 1 done but a restart is wanted (GG's reading); anything else is a failure.
    match run_tool(log, &plan::add_device(copy), copy) {
        0 => {}
        1 => reboot = true,
        code => {
            log.line(format!("SteelSeries' tool failed to add the driver (exit code {code}): second attempt with Windows' own functions"));
            match driver::install(log, copy) {
                Ok(report) => {
                    reboot |= report.reboot;
                    how = "Windows' own functions (SteelSeries' device tool had failed)";
                }
                Err(error) => {
                    log.line(format!("fallback install failed: {error}"));
                    return Err(Exit::Driver);
                }
            }
        }
    }
    match run_tool(log, &plan::register(copy), copy) {
        0 => {}
        1 => reboot = true,
        // GG only acts on 1; Platinum loads the DLLs itself, and Windows uses the INF's own
        // registration: the fallback's failure is noted, not fatal.
        code => {
            log.line(format!("SteelSeries' tool failed to register the APO (exit code {code}): second attempt with regsvr32"));
            match driver::register(log, copy) {
                Ok(()) => log.line("fallback register done"),
                Err(error) => log.line(format!("fallback register failed: {error} (the global COM registration may be missing; Platinum does not need it)")),
            }
        }
    }
    let now = platinum::machine::sonar_devices();
    if now.is_empty() {
        log.line("no Sonar device after installing the driver");
        return Err(Exit::Driver);
    }
    if !existed {
        let note = format!("The Sonar driver was installed by Platinum's setup on {} from {}, with {how}.\nDevice: {}\n", platinum::guard::stamp(), from.unwrap_or("SteelSeries GG's installer"), now[0].instance);
        if let Err(error) = std::fs::write(copy.join(sonarfiles::MARKER), note) {
            log.line(format!("cannot write the marker: {error}"));
        }
    }
    Ok(reboot)
}

/// `--elevated-remove`: the uninstall's administrator part. Returns the exit code.
pub fn remove(args: &[String]) -> u32 {
    let arguments = match parse(args) {
        Ok(arguments) => arguments,
        Err(_) => return Exit::BadArguments.code(),
    };
    let mut log = journal(&arguments);
    let exit = remove_with(&mut log);
    log.line(format!("elevated removal ends: {exit:?} (exit code {})", exit.code()));
    exit.code()
}

fn remove_with(log: &mut Log) -> Exit {
    if !win::is_elevated() {
        return Exit::NotElevated;
    }
    let copy = sonarfiles::driver_copy();
    let tool = copy.join(sonarfiles::DEVICE_TOOL);
    let tool_usable = match win::require_signer(&tool, crate::GG_SIGNER) {
        Ok(()) => true,
        Err(error) => {
            log.line(format!("SteelSeries' tool cannot be used ({error}): Windows' own functions only"));
            false
        }
    };
    log.line("Windows Audio restarts: all sound stops for a few seconds");
    let mut reboot = false;
    let mut failed = false;
    let code = if tool_usable { run_tool(log, &plan::unregister(&copy), &copy) } else { -1 };
    if driver::needs_fallback(code) {
        log.line(format!("SteelSeries' unregister did not succeed (exit code {code}): second attempt with regsvr32"));
        if let Err(error) = driver::unregister(log, &copy) {
            log.line(format!("fallback unregister failed: {error}"));
        }
    }
    match if tool_usable { run_tool(log, &plan::remove_device(&copy), &copy) } else { -1 } {
        0 => {}
        1 => reboot = true,
        code => {
            log.line(format!("SteelSeries' remove did not succeed (exit code {code}): second attempt with Windows' own functions"));
            match driver::remove(log, &copy) {
                Ok(wants_reboot) => reboot |= wants_reboot,
                Err(error) => {
                    log.line(format!("fallback removal failed: {error}"));
                    failed = true;
                }
            }
        }
    }
    let left = platinum::machine::sonar_devices();
    if !left.is_empty() {
        log.line(format!("Sonar devices still listed: {}", left.iter().map(|d| d.instance.as_str()).collect::<Vec<_>>().join(", ")));
    }
    if let Err(error) = win::delete_tree(win::HKEY_LOCAL_MACHINE, plan::SETTINGS_KEY) {
        log.line(error);
    } else {
        log.line(format!("deleted HKLM\\{}", plan::SETTINGS_KEY));
    }
    win::delete_if_empty(win::HKEY_LOCAL_MACHINE, plan::VENDOR_KEY);
    let data = sonarfiles::platinum_data();
    match std::fs::remove_dir_all(&data) {
        Ok(()) => log.line(format!("deleted {}", data.display())),
        Err(error) => log.line(format!("cannot delete {}: {error}", data.display())),
    }
    if failed {
        Exit::Remove
    } else if reboot {
        Exit::Reboot
    } else {
        Exit::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_go_both_ways() {
        for exit in [Exit::Ok, Exit::Reboot, Exit::NotElevated, Exit::BadArguments, Exit::Runtime, Exit::Copy, Exit::Signature, Exit::Driver, Exit::Remove] {
            assert_eq!(Exit::from_code(exit.code()), exit);
        }
        assert_eq!(Exit::from_code(12345), Exit::Driver);
    }

    #[test]
    fn arguments_are_read_and_unknown_ones_refused() {
        let args: Vec<String> = ["--elevated", "--log", r"C:\t\e.log", "--source", r"C:\t\gg", "--driver", "--from", "SteelSeriesGG123.0.0Setup.exe"].map(String::from).to_vec();
        let parsed = parse(&args).unwrap();
        assert_eq!(parsed.log, Some(PathBuf::from(r"C:\t\e.log")));
        assert_eq!(parsed.source, Some(PathBuf::from(r"C:\t\gg")));
        assert!(parsed.driver && parsed.vcredist.is_none());
        assert_eq!(parsed.from.as_deref(), Some("SteelSeriesGG123.0.0Setup.exe"));
        assert!(parse(&["--run".to_string(), "cmd".to_string()]).is_err());
        assert!(parse(&["--log".to_string()]).is_err());
    }

    /// The copy keeps only the files the setup wants, laid out as in GG's installer.
    #[test]
    fn only_wanted_files_are_copied() {
        let base = std::env::temp_dir().join(format!("platinum-setup-copy-{}", std::process::id()));
        let source = base.join("gg");
        let target = base.join("copy");
        let _ = std::fs::remove_dir_all(&base);
        for file in [r"apps\sonar\driver\vad\a.inf", r"apps\sonar\db-migrations\sonar-1.0.0\1_x.sql", r"shared\Steelseries.AudioDeviceInstaller.exe", r"shared\other.exe"] {
            let path = source.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"x").unwrap();
        }
        let mut copied = 0;
        copy_wanted(&source, &source, &target, &mut copied).unwrap();
        assert_eq!(copied, 3);
        assert!(target.join(r"apps\sonar\driver\vad\a.inf").is_file() && !target.join(r"shared\other.exe").exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
