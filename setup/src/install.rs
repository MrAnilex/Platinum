//! The install, as the user (the setup is not elevated: `%USERPROFILE%` and HKCU are the real
//! user's). The parts that need administrator rights (the Visual C++ runtime, the copy of the
//! driver package, SteelSeries' tools) are handed to the setup itself started elevated
//! (`elevated.rs`), behind one Windows prompt.
//!
//! Every step can run again: downloads resume, the extraction starts over (seconds), SteelSeries'
//! tools skip what is installed already (GG runs them at every update), files are overwritten,
//! and Platinum's settings, presets and state are never touched.

use crate::log::Log;
use crate::plan::{self, Machine, Options, Plan, Source, Step};
use crate::{nsis, payload, win};
use platinum::i18n::{mark, tr};
use platinum::trf;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

pub const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Platinum";
/// CREATE_NO_WINDOW: SteelSeries' tools and Platinum's command line are console programs.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How a step went, for the window's list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Running,
    Done,
    Failed,
}

/// What the install tells the window as it goes.
#[derive(Clone, Debug)]
pub enum Event {
    Step(usize, State),
    /// Progress of the running step (0 to 1, `None`: unknown) and a line saying what happens
    /// (already translated).
    Progress(Option<f32>, String),
    /// SteelSeries GG's licence, read from their installer, to show in full; the install then
    /// waits for the user's answer (`Run::decisions`). `version` names the installer it came from.
    License { text: String, version: String },
    Finished(Outcome),
}

/// How the install ended. Texts are translated.
#[derive(Clone, Debug, Default)]
pub struct Outcome {
    pub ok: bool,
    /// The user declined SteelSeries GG's licence: nothing was installed.
    pub declined: bool,
    pub message: String,
    pub notes: Vec<String>,
}

/// Why the install stops before its end.
enum Stop {
    Failed(String),
    /// SteelSeries GG's licence was declined (or the window closed while it was shown).
    Declined,
}

impl From<String> for Stop {
    fn from(message: String) -> Stop {
        Stop::Failed(message)
    }
}

impl From<&str> for Stop {
    fn from(message: &str) -> Stop {
        Stop::Failed(message.to_string())
    }
}

/// The work of one install, with what it reports and where it writes its journal.
pub struct Run<'a> {
    pub machine: Machine,
    pub plan: Plan,
    pub options: Options,
    pub log: Log,
    pub events: &'a (dyn Fn(Event) + Sync),
    pub cancel: Arc<AtomicBool>,
    /// The user's answer to SteelSeries GG's licence (true: accepted). Without a way to ask, or
    /// once the window is gone, the licence counts as declined.
    pub decisions: Option<std::sync::mpsc::Receiver<bool>>,
    pub reboot: bool,
    pub notes: Vec<String>,
}

fn megabytes(bytes: u64) -> String {
    platinum::i18n::decimal(format!("{:.0}", bytes as f64 / 1_000_000.0))
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn MultiByteToWideChar(page: u32, flags: u32, text: *const u8, length: i32, wide: *mut u16, room: i32) -> i32;
}

/// A console program's output as text: UTF-8 when it is valid UTF-8, else the OEM code page
/// (what console programs such as icacls write into a pipe: `traités` arrives as cp850).
pub fn console_text(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    // CP_OEMCP
    let length = i32::try_from(bytes.len()).unwrap_or(i32::MAX);
    let room = unsafe { MultiByteToWideChar(1, 0, bytes.as_ptr(), length, std::ptr::null_mut(), 0) };
    if room <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut wide = vec![0u16; room as usize];
    let written = unsafe { MultiByteToWideChar(1, 0, bytes.as_ptr(), length, wide.as_mut_ptr(), room) };
    String::from_utf16_lossy(&wide[..written.clamp(0, room) as usize])
}
/// Runs a console program hidden, with `arguments` exactly as given, and returns its exit code
/// and output (logged).
pub fn run_program(log: &mut Log, program: &Path, arguments: &str, folder: Option<&Path>) -> Result<i32, String> {
    use std::os::windows::process::CommandExt;
    log.line(format!("run: \"{}\" {arguments}", program.display()));
    let mut command = Command::new(program);
    command.raw_arg(arguments).creation_flags(CREATE_NO_WINDOW);
    if let Some(folder) = folder {
        command.current_dir(folder);
    }
    let output = command.output().map_err(|e| format!("cannot start {}: {e}", program.display()))?;
    for line in console_text(&output.stdout).lines().chain(console_text(&output.stderr).lines()) {
        let line = line.trim_matches(|c: char| c == '\0' || c.is_whitespace());
        if !line.is_empty() {
            log.line(format!("  | {line}"));
        }
    }
    let code = output.status.code().unwrap_or(-1);
    log.line(format!("  exit code {code}"));
    Ok(code)
}

impl Run<'_> {
    fn progress(&self, fraction: Option<f32>, text: String) {
        (self.events)(Event::Progress(fraction, text));
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Runs the steps in order; the first failure ends the install.
    pub fn install(mut self) {
        let steps = plan::steps(&self.plan);
        self.log.line(format!("Platinum setup {} — install{}", env!("CARGO_PKG_VERSION"), if self.plan.update { " (update)" } else { "" }));
        for line in plan::describe(&self.machine, &self.options, &self.plan) {
            self.log.line(format!("plan: {line}"));
        }
        let mut work = Work::default();
        for (index, step) in steps.iter().enumerate() {
            if self.cancelled() {
                self.finish_failed(index, tr("Installation annulée.").into());
                return;
            }
            (self.events)(Event::Step(index, State::Running));
            self.log.line(format!("step {}: {:?}", index + 1, step));
            let started = Instant::now();
            match self.step(*step, &mut work) {
                Ok(()) => {
                    self.log.line(format!("step {} done in {:.1} s", index + 1, started.elapsed().as_secs_f64()));
                    (self.events)(Event::Step(index, State::Done));
                }
                Err(Stop::Declined) => {
                    self.finish_declined(&work);
                    return;
                }
                Err(Stop::Failed(message)) => {
                    self.log.line(format!("step {} FAILED: {message}", index + 1));
                    (self.events)(Event::Step(index, State::Failed));
                    self.finish_failed(index, message);
                    return;
                }
            }
        }
        if self.machine.gg_at_logon {
            self.notes.push(tr("SteelSeries GG démarre encore avec Windows : désactivez-le dans le Gestionnaire des tâches (Applications de démarrage) pour laisser la main à Platinum.").into());
        }
        if self.reboot {
            self.notes.push(tr("Windows demande un redémarrage pour finir d'installer le pilote.").into());
        }
        self.log.line("install finished");
        (self.events)(Event::Finished(Outcome { ok: true, declined: false, message: tr("Platinum est installé.").into(), notes: self.notes.clone() }));
    }

    fn finish_failed(&mut self, _index: usize, message: String) {
        (self.events)(Event::Finished(Outcome { ok: false, declined: false, message, notes: vec![trf!("Le journal de l'installation : {file}", file = self.log.path().display())] }));
    }

    /// The licence was declined: the download and the work folder go, nothing else was done.
    fn finish_declined(&mut self, work: &Work) {
        if let Some((installer, true)) = &work.installer {
            let _ = std::fs::remove_file(installer);
        }
        let folder = plan::work_folder();
        match std::fs::remove_dir_all(&folder) {
            Ok(()) => self.log.line(format!("deleted {}", folder.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => self.log.line(format!("could not delete {} ({e})", folder.display())),
        }
        self.log.line("install ended: SteelSeries GG's licence declined, nothing installed");
        (self.events)(Event::Finished(Outcome { ok: false, declined: true, message: tr("Rien n'a été installé.").into(), notes: vec![tr("La licence de SteelSeries GG a été refusée : l'installateur téléchargé a été supprimé.").into()] }));
    }

    fn step(&mut self, step: Step, work: &mut Work) -> Result<(), Stop> {
        match step {
            Step::DownloadRuntime => Ok(self.download_runtime(work)?),
            Step::DownloadGg => Ok(self.download_gg(work)?),
            Step::License => self.license(work),
            Step::ExtractGg => Ok(self.extract(work)?),
            Step::Elevated => Ok(self.elevated(work)?),
            Step::CheckDriver => Ok(self.check_driver()?),
            Step::StopPlatinum => {
                self.progress(None, tr("Le son passe sans Platinum pendant quelques secondes.").into());
                stop_platinum(&mut self.log, &self.machine.install_dir);
                Ok(())
            }
            Step::CopyFiles => Ok(self.copy_files()?),
            Step::Autostart => Ok(self.autostart()?),
            Step::Shortcuts => Ok(self.shortcuts()?),
            Step::UninstallEntry => Ok(register_uninstall(&self.machine.install_dir)?),
            Step::StartPlatinum => Ok(self.start()?),
            Step::Cleanup => {
                let folder = plan::work_folder();
                match std::fs::remove_dir_all(&folder) {
                    Ok(()) => self.log.line(format!("deleted {}", folder.display())),
                    Err(e) => self.log.line(format!("could not delete {} ({e}): left for Windows' disk cleanup", folder.display())),
                }
                Ok(())
            }
        }
    }

    fn download_runtime(&mut self, work: &mut Work) -> Result<(), String> {
        let target = plan::work_folder().join("vc_redist.x64.exe");
        let answer = crate::net::head(plan::VC_URL)?;
        self.log.line(format!("VC++ runtime: {} -> HTTP {} {} bytes", plan::VC_URL, answer.status, answer.length.unwrap_or(0)));
        if answer.status != 200 {
            return Err(trf!("Le runtime Visual C++ ne peut pas être téléchargé (HTTP {status}).", status = answer.status));
        }
        self.fetch(&answer.url, &target, answer.length, mark("Téléchargement du runtime Visual C++ : {done} Mo sur {total} Mo"))?;
        win::require_signer(&target, plan::MICROSOFT).map_err(|e| trf!("Le runtime téléchargé n'est pas signé par Microsoft : {error}", error = e))?;
        self.log.line(format!("{} signed by {}", target.display(), plan::MICROSOFT));
        work.runtime = Some(target);
        Ok(())
    }

    /// Downloads with progress lines; `template` is a catalog key with `{done}` and `{total}`.
    fn fetch(&mut self, url: &str, target: &Path, expected: Option<u64>, template: &'static str) -> Result<(), String> {
        let events = self.events;
        let mut last = Instant::now() - Duration::from_secs(1);
        let mut progress = |done: u64, total: Option<u64>| {
            if last.elapsed() < Duration::from_millis(200) {
                return;
            }
            last = Instant::now();
            let fraction = total.map(|t| done as f32 / t.max(1) as f32);
            let text = tr(template).replace("{done}", &megabytes(done)).replace("{total}", &total.map(megabytes).unwrap_or_else(|| "?".into()));
            events(Event::Progress(fraction, text));
        };
        let mut lines = Vec::new();
        let outcome = crate::net::download(url, target, expected, &mut progress, &self.cancel, &mut |line| lines.push(line));
        for line in lines {
            self.log.line(line);
        }
        outcome.map_err(|e| if e == "cancelled" { tr("Installation annulée.").to_string() } else { trf!("Le téléchargement a échoué : {error}", error = e) })?;
        self.log.line(format!("downloaded {} ({} bytes)", target.display(), std::fs::metadata(target).map(|m| m.len()).unwrap_or(0)));
        Ok(())
    }

    fn download_gg(&mut self, work: &mut Work) -> Result<(), String> {
        let pinned = crate::net::head(plan::PINNED_URL);
        let (url, expected, pinned_hash, name) = match &pinned {
            Ok(answer) if answer.status == 200 && answer.length.is_none_or(|l| l == plan::PINNED_SIZE) => {
                self.log.line(format!("using GG {} (pinned, tested): {}", plan::PINNED_VERSION, plan::PINNED_URL));
                (answer.url.clone(), Some(plan::PINNED_SIZE), true, format!("SteelSeriesGG{}Setup.exe", plan::PINNED_VERSION))
            }
            other => {
                let why = match other {
                    Ok(answer) => format!("HTTP {}, {} bytes", answer.status, answer.length.unwrap_or(0)),
                    Err(error) => error.clone(),
                };
                let latest = crate::net::head(plan::LATEST_URL)?;
                if latest.status != 200 {
                    return Err(trf!("L'installateur de SteelSeries GG ne peut pas être téléchargé (HTTP {status}).", status = latest.status));
                }
                let name = installer_name(&latest.url);
                self.log.line(format!("pinned GG {} unavailable ({why}); using the latest installer instead: {} ({} bytes)", plan::PINNED_VERSION, latest.url, latest.length.unwrap_or(0)));
                self.notes.push(trf!("L'installateur testé (GG {pinned}) n'est plus en ligne : le pilote vient de {name}.", pinned = plan::PINNED_VERSION, name = name));
                (latest.url, latest.length, false, name)
            }
        };
        let target = plan::work_folder().join(&name);
        self.fetch(&url, &target, expected, mark("Téléchargement de l'installateur de SteelSeries GG : {done} Mo sur {total} Mo"))?;
        self.progress(None, tr("Vérification de la signature de SteelSeries…").into());
        if pinned_hash {
            let digest = win::hex(&win::sha256_file(&target)?);
            if digest != plan::PINNED_SHA256 {
                let _ = std::fs::remove_file(&target);
                return Err(trf!("Le fichier téléchargé ne correspond pas à l'installateur attendu (SHA-256 {digest}) : il a été supprimé, réessayez.", digest = digest));
            }
            self.log.line(format!("SHA-256 {digest} matches GG {}", plan::PINNED_VERSION));
        }
        if let Err(error) = win::require_signer(&target, crate::GG_SIGNER) {
            let _ = std::fs::remove_file(&target);
            return Err(trf!("L'installateur téléchargé n'est pas signé par SteelSeries : {error}", error = error));
        }
        self.log.line(format!("{} signed by {}", target.display(), crate::GG_SIGNER));
        work.gg_used = Some(name);
        work.installer = Some((target, true));
        Ok(())
    }

    /// SteelSeries GG's licence: read from their installer (checked first: a local one's
    /// signature is checked here), or from the copy an earlier run kept with it; shown in full;
    /// the install waits for the user's answer and goes on only once it is accepted.
    fn license(&mut self, work: &mut Work) -> Result<(), Stop> {
        let (text, from) = match &self.plan.source {
            Source::Copy => {
                let copy = platinum::sonarfiles::driver_copy();
                let text = std::fs::read_to_string(copy.join(plan::LICENSE_FILE)).map_err(|e| trf!("Le texte de la licence de SteelSeries GG ne peut pas être lu : rien n'est installé ({error}).", error = e))?;
                let from = std::fs::read_to_string(copy.join(plan::LICENSE_FROM)).map(|f| f.trim().to_string()).unwrap_or_default();
                (text, from)
            }
            source => {
                if let (Source::Local(path), None) = (source, &work.installer) {
                    win::require_signer(path, crate::GG_SIGNER).map_err(|e| trf!("Cet installateur n'est pas signé par SteelSeries : {error}", error = e))?;
                    self.log.line(format!("{} signed by {}", path.display(), crate::GG_SIGNER));
                    work.gg_used = path.file_name().map(|n| installer_name(&n.to_string_lossy()));
                    work.installer = Some((path.clone(), false));
                }
                let (installer, _) = work.installer.as_ref().ok_or("no installer to read the licence from")?;
                self.progress(None, tr("Lecture de la licence de SteelSeries GG dans leur installateur…").into());
                let text = nsis::license(installer).map_err(|e| trf!("Le texte de la licence de SteelSeries GG ne peut pas être lu dans leur installateur : rien n'est installé ({error}).", error = e))?;
                (text, work.gg_used.clone().unwrap_or_default())
            }
        };
        let version = gg_version(&from);
        let units: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let digest = win::hex(&win::sha256_bytes(&units)?);
        self.log.line(format!("licence: SteelSeries GG's licence read from {} (GG {version}): {} characters, SHA-256 of its UTF-16LE text {digest}; shown to the user", if from.is_empty() { "the earlier copy" } else { from.as_str() }, units.len() / 2));
        self.progress(None, tr("En attente de votre réponse à la licence de SteelSeries GG…").into());
        (self.events)(Event::License { text: text.clone(), version: version.clone() });
        let accepted = self.decisions.as_ref().is_some_and(|answers| answers.recv() == Ok(true));
        if !accepted {
            self.log.line(format!("licence: declined by the user at {} (GG {version}, SHA-256 {digest})", platinum::guard::stamp()));
            return Err(Stop::Declined);
        }
        self.log.line(format!("licence: ACCEPTED by the user at {} (GG {version}, SHA-256 of the text {digest})", platinum::guard::stamp()));
        work.license = Some((text, from));
        Ok(())
    }

    fn extract(&mut self, work: &mut Work) -> Result<(), String> {
        let (installer, downloaded) = work.installer.take().ok_or("no installer to extract")?;
        let out = plan::work_folder().join("gg");
        let _ = std::fs::remove_dir_all(&out);
        let events = self.events;
        let mut last = Instant::now() - Duration::from_secs(1);
        let mut progress = |read: u64, total: u64| {
            if last.elapsed() < Duration::from_millis(200) {
                return;
            }
            last = Instant::now();
            events(Event::Progress(Some(read as f32 / total.max(1) as f32), tr("Extraction du pilote Sonar (seuls 42 Mo sur 1,3 Go sont gardés)…").into()));
        };
        let started = Instant::now();
        let files = nsis::extract(&installer, &plan::wanted, &out, &mut progress).map_err(|e| trf!("Le pilote ne peut pas être extrait de l'installateur : {error}", error = e))?;
        let bytes: u64 = files.iter().map(|f| f.size).sum();
        self.log.line(format!("extracted {} files ({bytes} bytes) in {:.1} s to {}", files.len(), started.elapsed().as_secs_f64(), out.display()));
        let missing = plan::missing_driver_files(&out);
        if !missing.is_empty() {
            return Err(trf!("Des fichiers du pilote manquent dans cet installateur : {files}", files = missing.join(", ")));
        }
        if !plan::has_migrations(&out) {
            self.log.line("no preset migrations in this installer: GG's presets will be missing");
        }
        // The accepted licence goes with the files, so that a later run using the copy shows it.
        if let Some((text, from)) = &work.license {
            std::fs::write(out.join(plan::LICENSE_FILE), text).map_err(|e| e.to_string())?;
            std::fs::write(out.join(plan::LICENSE_FROM), from).map_err(|e| e.to_string())?;
        }
        if downloaded {
            match std::fs::remove_file(&installer) {
                Ok(()) => self.log.line(format!("deleted {}", installer.display())),
                Err(e) => self.log.line(format!("could not delete {}: {e}", installer.display())),
            }
        }
        work.extracted = Some(out);
        Ok(())
    }

    fn elevated(&mut self, work: &mut Work) -> Result<(), String> {
        let journal = plan::work_folder().join(format!("elevated-{}.log", std::process::id()));
        std::fs::create_dir_all(plan::work_folder()).map_err(|e| e.to_string())?;
        std::fs::write(&journal, b"").map_err(|e| format!("cannot create {}: {e}", journal.display()))?;
        let mut arguments = format!("--elevated --log \"{}\"", journal.display());
        if self.plan.vc_runtime {
            let runtime = work.runtime.as_ref().ok_or("the runtime was not downloaded")?;
            arguments.push_str(&format!(" --vcredist \"{}\"", runtime.display()));
        }
        if let Some(source) = &work.extracted {
            arguments.push_str(&format!(" --source \"{}\"", source.display()));
        }
        if self.plan.driver {
            arguments.push_str(" --driver");
            if let Some(name) = &work.gg_used {
                arguments.push_str(&format!(" --from \"{name}\""));
            }
        }
        if self.plan.driver {
            self.progress(None, tr("Windows demande l'autorisation d'installer le pilote. Le son se coupera quelques secondes pendant le redémarrage du service Audio de Windows.").into());
        } else {
            self.progress(None, tr("Windows demande l'autorisation d'installer le runtime Visual C++.").into());
        }
        let program = std::env::current_exe().map_err(|e| e.to_string())?;
        self.log.line(format!("elevated: \"{}\" {arguments}", program.display()));
        self.log.line("elevated part's journal (copied here as it is written):");
        let mut follow = crate::log::Follow::new(&journal);
        let log = &self.log;
        let outcome = win::run_elevated(&program, &arguments, || follow.pump(log, false));
        follow.pump(&self.log, true);
        let _ = std::fs::remove_file(&journal);
        let code = match outcome {
            Ok(code) => code,
            Err(win::Elevation::Refused) => return Err(tr("L'autorisation d'administrateur a été refusée : rien n'a été installé. Cliquez sur Réessayer pour la redemander.").into()),
            Err(win::Elevation::Failed(error)) => return Err(trf!("La partie administrateur n'a pas pu démarrer : {error}", error = error)),
        };
        self.log.line(format!("elevated part exited with {code}"));
        record_elevated_crash(&mut self.log, code, "install");
        match crate::elevated::Exit::from_code(code) {
            crate::elevated::Exit::Ok => Ok(()),
            crate::elevated::Exit::Reboot => {
                self.reboot = true;
                Ok(())
            }
            failure => Err(tr(failure.message()).into()),
        }
    }

    fn check_driver(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            self.progress(None, tr("Attente des périphériques audio Sonar…").into());
            let devices = platinum::machine::sonar_devices();
            let staged = platinum::machine::staged_packages().iter().all(|(_, folder)| folder.is_some());
            let stores = platinum::doctor::missing_stores();
            let endpoints = platinum::devices::Devices::open()
                .and_then(|d| d.list())
                .map(|list| list.iter().filter(|e| platinum::session::virtual_kind(e).is_some()).count())
                .unwrap_or(0);
            if !devices.is_empty() && staged && stores.is_empty() && endpoints >= 8 {
                self.log.line(format!("driver ok: {} device(s), packages staged, settings stores present, {endpoints} Sonar endpoints", devices.len()));
                return Ok(());
            }
            if Instant::now() >= deadline {
                self.log.line(format!("driver check: {} device(s), packages staged {staged}, stores missing {stores:?}, {endpoints} endpoints", devices.len()));
                if devices.is_empty() || !staged {
                    return Err(tr("Le pilote Sonar n'est pas installé après les outils de SteelSeries : voyez le journal.").into());
                }
                if !stores.is_empty() {
                    return Err(tr("Les réglages du module audio Sonar n'ont pas été créés : voyez le journal.").into());
                }
                // Device and packages are there; the endpoints may need a restart of Windows.
                self.reboot = true;
                self.notes.push(tr("Les périphériques audio Sonar n'apparaissent pas encore : redémarrez Windows si le son ne passe pas par Platinum.").into());
                return Ok(());
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    /// platinum.ini's `crash_reports`, as ticked on the options page; the other settings stay (a
    /// platinum.ini from before `channel_names` gets the names it had, see `platinum::config`).
    /// Not fatal: Platinum runs with the file as it was.
    fn write_settings(&mut self) {
        let consent = if self.options.crash_reports { platinum::crash::Consent::Ask } else { platinum::crash::Consent::Off };
        let saved = platinum::config::load_full().for_saving().and_then(|mut settings| {
            settings.crash_reports = consent;
            settings.save().map(|()| settings.channel_names)
        });
        match saved {
            Ok(names) => self.log.line(format!("platinum.ini: crash_reports = {}, channel_names = {}", consent.key(), names.key())),
            Err(error) => self.log.line(format!("platinum.ini not written ({error}): crash_reports left as it was")),
        }
    }

    fn copy_files(&mut self) -> Result<(), String> {
        if !payload::present() {
            return Err(tr("Cet installateur a été compilé sans les programmes de Platinum : utilisez scripts\\build-setup.ps1.").into());
        }
        let dir = self.machine.install_dir.clone();
        std::fs::create_dir_all(&dir).map_err(|e| trf!("Impossible de créer {folder} : {error}", folder = dir.display(), error = e))?;
        for (name, bytes) in payload::files() {
            let target = dir.join(name);
            // A program still exiting keeps its file a moment: tried for 10 s.
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match std::fs::write(&target, bytes) {
                    Ok(()) => break,
                    Err(e) if Instant::now() < deadline => {
                        self.log.line(format!("{} busy ({e}), retrying", target.display()));
                        std::thread::sleep(Duration::from_millis(500));
                    }
                    Err(e) => return Err(trf!("Impossible d'écrire {file} : {error}", file = target.display(), error = e)),
                }
            }
            self.log.line(format!("wrote {} ({} bytes)", target.display(), bytes.len()));
        }
        // Left by an update that renamed a running program (`platinum.old.exe`).
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().to_ascii_lowercase().ends_with(".old.exe") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        let me = std::env::current_exe().map_err(|e| e.to_string())?;
        let copy = dir.join(crate::SETUP_NAME);
        if !same_file(&me, &copy) {
            std::fs::copy(&me, &copy).map_err(|e| trf!("Impossible d'écrire {file} : {error}", file = copy.display(), error = e))?;
            self.log.line(format!("copied the setup to {} (for uninstalling)", copy.display()));
        }
        self.write_settings();
        Ok(())
    }

    fn autostart(&mut self) -> Result<(), String> {
        let platinum = self.machine.install_dir.join("platinum.exe");
        let arguments = if self.options.autostart { "autostart enable" } else { "autostart disable" };
        let code = run_program(&mut self.log, &platinum, arguments, Some(&self.machine.install_dir))?;
        if code != 0 && self.options.autostart {
            return Err(tr("Platinum n'a pas pu s'inscrire au démarrage de Windows : voyez le journal.").into());
        }
        Ok(())
    }

    fn shortcuts(&mut self) -> Result<(), String> {
        let dir = &self.machine.install_dir;
        let (target, icon) = (dir.join("platinum-ui.exe"), dir.join("platinum-ui.exe"));
        let description = tr("Mixeur et effets audio");
        let mut folders = vec![win::known_folder(&win::PROGRAMS)];
        if self.options.desktop_shortcut {
            folders.push(win::known_folder(&win::DESKTOP));
        }
        for folder in folders {
            let folder = folder.ok_or("cannot find the Start menu or the desktop")?;
            let link = folder.join("Platinum.lnk");
            win::create_shortcut(&link, &target, dir, &icon, description).map_err(|e| trf!("Le raccourci {file} n'a pas pu être créé : {error}", file = link.display(), error = e))?;
            self.log.line(format!("shortcut {}", link.display()));
        }
        // Windows keeps icons in a cache: refreshed so the shortcut shows Platinum's.
        let _ = run_program(&mut self.log, Path::new("ie4uinit.exe"), "-show", None);
        Ok(())
    }

    fn start(&mut self) -> Result<(), String> {
        let dir = &self.machine.install_dir;
        Command::new(dir.join("platinum-start.exe")).arg("daemon").current_dir(dir).spawn().map_err(|e| trf!("Platinum n'a pas pu démarrer : {error}", error = e))?;
        self.log.line("started platinum-start.exe daemon");
        std::thread::sleep(Duration::from_secs(3));
        let _ = run_program(&mut self.log, &dir.join("platinum.exe"), "daemon --status", Some(dir));
        Ok(())
    }
}

/// What the install keeps between its steps.
#[derive(Default)]
struct Work {
    runtime: Option<PathBuf>,
    /// The GG installer, and whether it was downloaded (then deleted after extraction).
    installer: Option<(PathBuf, bool)>,
    extracted: Option<PathBuf>,
    /// The installer the SteelSeries files come from, by name, for the marker.
    gg_used: Option<String>,
    /// SteelSeries GG's licence as accepted, and the installer it came from.
    license: Option<(String, String)>,
}

/// The administrator part ended with a crash code (a panic or a fault: its own exits are small
/// numbers): the setup records its crash report, with install.log's tail, where the part's journal
/// was copied. `what` says which part ("install", "uninstall").
pub fn record_elevated_crash(log: &mut Log, code: u32, what: &str) {
    if !platinum::crash::is_crash_code(Some(code as i32)) {
        return;
    }
    let reason = format!("administrator part ({what}) ended with {}", platinum::crash::exit_description(Some(code as i32)));
    let log_file = log.path().to_path_buf();
    match platinum::crash::record(platinum::crash::SETUP_ADMIN, 0, reason, Vec::new(), Some(&log_file)) {
        Ok(file) => log.line(format!("crash report: {}", file.display())),
        Err(error) => log.line(format!("crash report not written: {error}")),
    }
}

/// GG's version from its installer's name (`SteelSeriesGG123.0.0Setup.exe` gives `123.0.0`); the
/// name itself otherwise, or "?" without one.
pub fn gg_version(name: &str) -> String {
    let core = name.strip_prefix("SteelSeriesGG").and_then(|rest| rest.strip_suffix("Setup.exe"));
    match core {
        Some(version) if !version.is_empty() => version.to_string(),
        _ if name.is_empty() => "?".into(),
        _ => name.to_string(),
    }
}

/// The file name of an installer from the address it was found at, kept to safe characters.
pub fn installer_name(url: &str) -> String {
    let last = url.split(['?', '#']).next().unwrap_or("").rsplit('/').next().unwrap_or("");
    let clean: String = last.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')).collect();
    if clean.to_ascii_lowercase().ends_with(".exe") && clean.len() > 4 { clean } else { "SteelSeriesGGSetup.exe".into() }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Stops Platinum's background service and closes its window, waiting for them (as
/// scripts/install.cmd does): sound passes without Platinum for those seconds.
pub fn stop_platinum(log: &mut Log, dir: &Path) {
    let platinum = dir.join("platinum.exe");
    if platinum.is_file() {
        let _ = run_program(log, &platinum, "daemon --stop", Some(dir));
    }
    let running = |names: &[&str]| platinum::routing::all_processes().unwrap_or_default().iter().any(|(_, exe)| names.iter().any(|n| exe.eq_ignore_ascii_case(n)));
    let deadline = Instant::now() + Duration::from_secs(20);
    while running(&["platinum.exe", "platinum.old.exe"]) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
    }
    if running(&["platinum.exe", "platinum.old.exe"]) {
        log.line("WARNING: Platinum still runs after 20 s");
    }
    if running(&["platinum-ui.exe"]) {
        let _ = run_program(log, Path::new("taskkill.exe"), "/IM platinum-ui.exe", None);
        let deadline = Instant::now() + Duration::from_secs(5);
        while running(&["platinum-ui.exe"]) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(500));
        }
        if running(&["platinum-ui.exe"]) {
            let _ = run_program(log, Path::new("taskkill.exe"), "/F /IM platinum-ui.exe", None);
        }
    }
}

/// The per-user "Installed apps" entry, pointing at the setup's copy with `--uninstall`.
pub fn register_uninstall(dir: &Path) -> Result<(), String> {
    use win::{HKEY_CURRENT_USER as HKCU, set_dword, set_text};
    let uninstall = format!("\"{}\" --uninstall", dir.join(crate::SETUP_NAME).display());
    set_text(HKCU, UNINSTALL_KEY, "DisplayName", "Platinum")?;
    set_text(HKCU, UNINSTALL_KEY, "DisplayVersion", env!("CARGO_PKG_VERSION"))?;
    set_text(HKCU, UNINSTALL_KEY, "Publisher", crate::PUBLISHER)?;
    set_text(HKCU, UNINSTALL_KEY, "DisplayIcon", &format!("{},0", dir.join("platinum-ui.exe").display()))?;
    set_text(HKCU, UNINSTALL_KEY, "InstallLocation", &dir.display().to_string())?;
    set_text(HKCU, UNINSTALL_KEY, "UninstallString", &uninstall)?;
    set_text(HKCU, UNINSTALL_KEY, "Comments", "Mixer and effects for the Sonar audio driver, without SteelSeries GG")?;
    set_dword(HKCU, UNINSTALL_KEY, "NoModify", 1)?;
    set_dword(HKCU, UNINSTALL_KEY, "NoRepair", 1)?;
    let setup = std::fs::metadata(dir.join(crate::SETUP_NAME)).map(|m| m.len()).unwrap_or(0);
    set_dword(HKCU, UNINSTALL_KEY, "EstimatedSize", ((payload::size() + setup) / 1024) as u32)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// icacls' "1 fichiers correctement traités" arrives in the OEM code page (0x82 is the same
    /// letter in cp437 and cp850); UTF-8 passes as is.
    #[test]
    fn console_output_is_decoded_from_the_oem_code_page() {
        assert_eq!(console_text("déjà ’".as_bytes()), "déjà ’");
        let oem = console_text(b"trait\x82s");
        assert!(oem.starts_with("trait") && oem.ends_with('s') && oem.chars().count() == 7 && !oem.contains('\u{FFFD}'), "{oem}");
        assert_eq!(console_text(b""), "");
    }

    #[test]
    fn installer_names_are_kept_safe() {
        assert_eq!(installer_name("https://engine.steelseriescdn.com/SteelSeriesGG124.0.0Setup.exe"), "SteelSeriesGG124.0.0Setup.exe");
        assert_eq!(installer_name("https://x.test/a/..%5C..%5Cevil.exe?y=1"), "..5C..5Cevil.exe");
        assert_eq!(installer_name("https://x.test/download"), "SteelSeriesGGSetup.exe");
    }

    #[test]
    fn gg_versions_come_from_installer_names() {
        assert_eq!(gg_version("SteelSeriesGG123.0.0Setup.exe"), "123.0.0");
        assert_eq!(gg_version("SteelSeriesGGSetup.exe"), "SteelSeriesGGSetup.exe");
        assert_eq!(gg_version("gg.exe"), "gg.exe");
        assert_eq!(gg_version(""), "?");
    }
}
