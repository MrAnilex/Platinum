//! The uninstall (`Platinum-Setup.exe --uninstall`, the "Installed apps" entry's command): stops
//! Platinum and puts Windows' devices back, removes its start at logon, its device names, its
//! programs, shortcuts and entry; the user's settings and presets only when asked. The Sonar driver
//! is offered for removal only when Platinum's setup installed it (its marker) and GG is not
//! installed: GG and Platinum share it, and GG's own uninstaller removes it.

use crate::install::{Event, Outcome, State, run_program, stop_platinum};
use crate::log::Log;
use crate::plan::{self, Machine};
use crate::win;
use platinum::i18n::{mark, tr};
use platinum::sonarfiles;
use platinum::trf;
use std::path::{Path, PathBuf};

/// The user's choices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// Also delete platinum.ini, the presets, the state and the journals.
    pub remove_data: bool,
    /// Also remove the Sonar driver (offered only when `driver_offered`).
    pub remove_driver: bool,
}

/// Whether removing the driver may be offered: Platinum's setup installed it and GG is not here.
pub fn driver_offered(machine: &Machine) -> bool {
    machine.installed_by_platinum && !machine.gg_installed
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    StopPlatinum,
    Autostart,
    Names,
    Files,
    Shortcuts,
    Entry,
    UserData,
    Driver,
}

impl Step {
    pub fn title(self) -> &'static str {
        match self {
            Step::StopPlatinum => mark("Arrêter Platinum"),
            Step::Autostart => mark("Retirer le démarrage avec Windows"),
            Step::Names => mark("Remettre les noms d'origine des périphériques audio"),
            Step::Files => mark("Supprimer les programmes de Platinum"),
            Step::Shortcuts => mark("Supprimer les raccourcis"),
            Step::Entry => mark("Retirer Platinum des applications installées"),
            Step::UserData => mark("Supprimer vos réglages et préréglages"),
            Step::Driver => mark("Désinstaller le pilote Sonar (administrateur)"),
        }
    }
}

pub fn steps(machine: &Machine, options: &Options) -> Vec<Step> {
    let mut list = vec![Step::StopPlatinum, Step::Autostart, Step::Names, Step::Files, Step::Shortcuts, Step::Entry];
    if options.remove_data {
        list.push(Step::UserData);
    }
    if options.remove_driver && driver_offered(machine) {
        list.push(Step::Driver);
    }
    list
}

fn shortcut_paths() -> Vec<PathBuf> {
    [win::known_folder(&win::PROGRAMS), win::known_folder(&win::DESKTOP)].into_iter().flatten().map(|folder| folder.join("Platinum.lnk")).collect()
}

/// `%LOCALAPPDATA%\Platinum` (the record of the previous default devices).
fn local_data() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("Platinum"))
}

/// The dry run of the uninstall.
pub fn describe(machine: &Machine, options: &Options) -> Vec<String> {
    let dir = &machine.install_dir;
    let platinum = dir.join("platinum.exe");
    let mut lines = vec![format!("uninstall from {}{}", dir.display(), if machine.platinum_installed { "" } else { " (Platinum's programs are not there)" })];
    if !driver_offered(machine) {
        lines.push(format!("driver: kept ({})", if machine.gg_installed { "GG is installed and owns it" } else { "Platinum's setup did not install it (no marker)" }));
    }
    let copy = sonarfiles::driver_copy();
    for (index, step) in steps(machine, options).iter().enumerate() {
        let n = index + 1;
        lines.push(match step {
            Step::StopPlatinum => format!("{n}. \"{}\" daemon --stop (default devices put back), close platinum-ui.exe", platinum.display()),
            Step::Autostart => format!("{n}. \"{}\" autostart disable", platinum.display()),
            Step::Names => format!("{n}. \"{}\" rename --undo", platinum.display()),
            Step::Files => format!("{n}. delete platinum.exe, platinum-start.exe, platinum-ui.exe, platinum.ico from {}; {} deletes itself after closing", dir.display(), crate::SETUP_NAME),
            Step::Shortcuts => format!("{n}. delete {}", shortcut_paths().iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")),
            Step::Entry => format!("{n}. delete HKCU\\{}", crate::install::UNINSTALL_KEY),
            Step::UserData => format!("{n}. delete {} (platinum.ini, presets, state, journals) and {}", dir.display(), local_data().map(|p| p.display().to_string()).unwrap_or_default()),
            Step::Driver => format!(
                "{n}. one administrator prompt (sound stops a few seconds), then: {} (if it fails: regsvr32 /u /s on the DriverStore DLLs) ; {} (if it fails: DiUninstallDevice on every device with {}, DiUninstallDriverW on the published INFs of the extension, VAD and APO packages, Windows Audio stopped meanwhile) ; delete HKLM\\{} ; delete {}",
                plan::unregister(&copy).line(),
                plan::remove_device(&copy).line(),
                platinum::machine::SONAR_HWID,
                plan::SETTINGS_KEY,
                sonarfiles::platinum_data().display()
            ),
        });
    }
    lines
}

pub struct Run<'a> {
    pub machine: Machine,
    pub options: Options,
    pub log: Log,
    pub events: &'a (dyn Fn(Event) + Sync),
}

impl Run<'_> {
    pub fn uninstall(mut self) {
        self.log.line(format!("Platinum setup {} — uninstall", env!("CARGO_PKG_VERSION")));
        for line in describe(&self.machine, &self.options) {
            self.log.line(format!("plan: {line}"));
        }
        let mut reboot = false;
        let mut problems = Vec::new();
        for (index, step) in steps(&self.machine, &self.options).iter().enumerate() {
            (self.events)(Event::Step(index, State::Running));
            match self.step(*step, &mut reboot) {
                Ok(()) => (self.events)(Event::Step(index, State::Done)),
                Err(problem) => {
                    // The rest goes on: an uninstall leaves as little as it can.
                    self.log.line(format!("step {:?} failed: {problem}", step));
                    problems.push(problem);
                    (self.events)(Event::Step(index, State::Failed));
                }
            }
        }
        let mut notes = problems;
        if reboot {
            notes.push(tr("Windows demande un redémarrage pour finir de retirer le pilote.").into());
        }
        if !self.options.remove_data {
            notes.push(trf!("Vos réglages et préréglages restent dans {folder}.", folder = self.machine.install_dir.display()));
        }
        if self.machine.gg_installed {
            notes.push(tr("SteelSeries GG est toujours installé : réactivez son démarrage avec Windows si vous voulez le retrouver.").into());
        }
        self.log.line("uninstall finished");
        (self.events)(Event::Finished(Outcome { ok: true, declined: false, message: tr("Platinum est désinstallé.").into(), notes }));
    }

    fn step(&mut self, step: Step, reboot: &mut bool) -> Result<(), String> {
        let dir = self.machine.install_dir.clone();
        let platinum = dir.join("platinum.exe");
        match step {
            Step::StopPlatinum => {
                stop_platinum(&mut self.log, &dir);
                Ok(())
            }
            Step::Autostart | Step::Names if !platinum.is_file() => Ok(()),
            Step::Autostart => run_program(&mut self.log, &platinum, "autostart disable", Some(&dir)).map(|_| ()),
            Step::Names => run_program(&mut self.log, &platinum, "rename --undo", Some(&dir)).map(|_| ()),
            Step::Files => {
                for name in ["platinum.exe", "platinum-start.exe", "platinum-ui.exe", "platinum.ico", "platinum.old.exe"] {
                    remove_file(&mut self.log, &dir.join(name))?;
                }
                Ok(())
            }
            Step::Shortcuts => {
                for link in shortcut_paths() {
                    remove_file(&mut self.log, &link)?;
                }
                let _ = run_program(&mut self.log, Path::new("ie4uinit.exe"), "-show", None);
                Ok(())
            }
            Step::Entry => {
                win::delete_tree(win::HKEY_CURRENT_USER, crate::install::UNINSTALL_KEY)?;
                self.log.line(format!("deleted HKCU\\{}", crate::install::UNINSTALL_KEY));
                Ok(())
            }
            Step::UserData => {
                let me = std::env::current_exe().ok().and_then(|p| std::fs::canonicalize(p).ok());
                if let Ok(entries) = std::fs::read_dir(&dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        // The running setup deletes itself once closed.
                        if std::fs::canonicalize(&path).ok() == me {
                            continue;
                        }
                        let removed = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
                        if let Err(error) = removed {
                            self.log.line(format!("cannot delete {}: {error}", path.display()));
                        }
                    }
                }
                if let Some(local) = local_data() {
                    let _ = std::fs::remove_dir_all(&local);
                }
                self.log.line(format!("deleted the settings and presets in {}", dir.display()));
                Ok(())
            }
            Step::Driver => {
                (self.events)(Event::Progress(None, tr("Windows demande l'autorisation de retirer le pilote. Le son se coupera quelques secondes.").into()));
                let journal = plan::work_folder().join(format!("elevated-remove-{}.log", std::process::id()));
                std::fs::create_dir_all(plan::work_folder()).map_err(|e| e.to_string())?;
                std::fs::write(&journal, b"").map_err(|e| e.to_string())?;
                let program = std::env::current_exe().map_err(|e| e.to_string())?;
                self.log.line("elevated removal's journal (copied here as it is written):");
                let mut follow = crate::log::Follow::new(&journal);
                let log = &self.log;
                let outcome = win::run_elevated(&program, &format!("--elevated-remove --log \"{}\"", journal.display()), || follow.pump(log, false));
                follow.pump(&self.log, true);
                let _ = std::fs::remove_file(&journal);
                if let Ok(code) = &outcome {
                    crate::install::record_elevated_crash(&mut self.log, *code, "uninstall");
                }
                match outcome {
                    Ok(code) => match crate::elevated::Exit::from_code(code) {
                        crate::elevated::Exit::Ok => Ok(()),
                        crate::elevated::Exit::Reboot => {
                            *reboot = true;
                            Ok(())
                        }
                        failure => Err(tr(failure.message()).into()),
                    },
                    Err(win::Elevation::Refused) => Err(tr("L'autorisation d'administrateur a été refusée : le pilote Sonar est resté installé.").into()),
                    Err(win::Elevation::Failed(error)) => Err(trf!("La partie administrateur n'a pas pu démarrer : {error}", error = error)),
                }
            }
        }
    }
}

fn remove_file(log: &mut Log, path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => {
            log.line(format!("deleted {}", path.display()));
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(trf!("Impossible de supprimer {file} : {error}", file = path.display(), error = error)),
    }
}

/// When the setup runs from the install folder (the uninstall entry's copy), deletes it once it
/// has closed, and the folder if nothing else is left in it.
pub fn delete_self_later(dir: &Path) {
    use std::os::windows::process::CommandExt;
    let Ok(me) = std::env::current_exe() else { return };
    let inside = match (std::fs::canonicalize(&me), std::fs::canonicalize(dir)) {
        (Ok(me), Ok(dir)) => me.starts_with(dir),
        _ => false,
    };
    if !inside {
        return;
    }
    let script = format!("/d /c ping -n 4 127.0.0.1 >nul & del /f /q \"{}\" & rmdir \"{}\"", me.display(), dir.display());
    let _ = std::process::Command::new("cmd.exe").raw_arg(script).creation_flags(0x0800_0000).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use platinum::machine::WindowsVersion;

    fn machine(installed_by_platinum: bool, gg_installed: bool) -> Machine {
        Machine {
            windows: WindowsVersion { major: 10, minor: 0, build: 26200 },
            x64: true,
            gg_installed,
            gg_version: None,
            gg_running: Vec::new(),
            gg_at_logon: false,
            devices: 1,
            packages_staged: true,
            stores_missing: Vec::new(),
            vc_runtime: Some((14, 44, 0)),
            presets: None,
            copy_complete: installed_by_platinum,
            installed_by_platinum,
            install_dir: PathBuf::from(r"C:\Users\Test\Platinum"),
            platinum_installed: true,
        }
    }

    #[test]
    fn the_driver_goes_only_if_platinum_put_it_there_and_gg_is_gone() {
        let all = Options { remove_data: true, remove_driver: true };
        assert!(steps(&machine(true, false), &all).contains(&Step::Driver));
        assert!(!steps(&machine(true, true), &all).contains(&Step::Driver));
        assert!(!steps(&machine(false, false), &all).contains(&Step::Driver));
        let keep = Options { remove_data: false, remove_driver: false };
        assert_eq!(steps(&machine(true, false), &keep), [Step::StopPlatinum, Step::Autostart, Step::Names, Step::Files, Step::Shortcuts, Step::Entry]);
        assert!(describe(&machine(false, true), &keep).iter().any(|l| l.contains("GG is installed and owns it")));
    }
}
