//! `Platinum-Setup.exe`: installs Platinum for the current user, and the Sonar driver taken from
//! SteelSeries GG's official installer (downloaded on the user's PC, never shipped with Platinum),
//! installed with SteelSeries' own tools exactly as GG's installer does. GG itself is never
//! installed. See README.md, "Install".
//!
//! Command line (the window is the default):
//! - `--uninstall`: the uninstall window ("Installed apps" runs this);
//! - `--dry-run [--uninstall] [--gg-installer <file>]`: prints every step it would take on this
//!   machine, does nothing;
//! - `--gg-installer <file>`: takes the SteelSeries files from a GG installer already on disk;
//! - `--extract <gg installer> <folder>`: only extracts the files the setup uses (checking);
//! - `--list <gg installer>`: lists an installer's files and which ones the setup takes;
//! - `--elevated …`, `--elevated-remove …`: the administrator parts, started by the setup itself.

#![windows_subsystem = "windows"]

mod driver;
mod elevated;
mod install;
mod log;
mod net;
mod nsis;
mod payload;
mod plan;
mod uninstall;
mod win;
mod window;

slint::include_modules!();

use platinum::i18n::{self, Language};
use std::path::PathBuf;

/// The signer of GG's installer and of SteelSeries' tools (Authenticode).
pub const GG_SIGNER: &str = "GN Hearing A/S";
/// The publisher shown in "Installed apps".
pub const PUBLISHER: &str = "Platinum (open source)";
/// The setup's name, also of its copy in the install folder (for uninstalling).
pub const SETUP_NAME: &str = "Platinum-Setup.exe";

/// The setup's catalogs (its .slint file's texts and its Rust texts), French being the source.
/// The .slint texts are also bundled by Slint from the same files (build.rs).
pub const SETUP_CATALOGS: i18n::Sources = &[
    (Language::En, include_str!("../lang/en/LC_MESSAGES/platinum-setup.po")),
    (Language::Es, include_str!("../lang/es/LC_MESSAGES/platinum-setup.po")),
    (Language::De, include_str!("../lang/de/LC_MESSAGES/platinum-setup.po")),
    (Language::It, include_str!("../lang/it/LC_MESSAGES/platinum-setup.po")),
    (Language::Pt, include_str!("../lang/pt/LC_MESSAGES/platinum-setup.po")),
];

const USAGE: &str = "Platinum-Setup.exe                 install Platinum (window)
Platinum-Setup.exe --uninstall     uninstall Platinum (window)
Platinum-Setup.exe --dry-run [--uninstall] [--gg-installer <file>] [--assume-clean]
                                   print every step it would take on this machine; changes nothing
Platinum-Setup.exe --gg-installer <file>
                                   take the Sonar driver from a SteelSeries GG installer on disk
Platinum-Setup.exe --extract <gg installer> <folder>
                                   only extract the files the setup uses from a GG installer
Platinum-Setup.exe --list <gg installer>
                                   list the files of a GG installer (reads its header only)";

fn option(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|at| args.get(at + 1)).cloned()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    if flag("--elevated") || flag("--elevated-remove") {
        if let Some(journal) = option(&args, "--log") {
            elevated_panics_to(PathBuf::from(journal));
        }
    }
    if flag("--elevated") {
        std::process::exit(elevated::install(&args) as i32);
    }
    if flag("--elevated-remove") {
        std::process::exit(elevated::remove(&args) as i32);
    }
    if flag("--dry-run") || flag("--help") || flag("-h") || flag("--extract") || flag("--list") {
        win::attach_console();
        let code = console(&args);
        std::process::exit(code);
    }
    // A crash of the window leaves a report next to install.log (`platinum::crash`); the
    // snapshot, which installs nothing, writes none.
    if std::env::var_os("PLATINUM_SETUP_SNAPSHOT").is_none() {
        platinum::crash::install(platinum::crash::SETUP);
    }
    let mode = if flag("--uninstall") {
        window::Mode::Uninstall
    } else {
        // An update keeps the crash reports' choice made before.
        let crash_reports = platinum::config::load().crash_reports == platinum::crash::Consent::Ask;
        window::Mode::Install(plan::Options { gg_installer: option(&args, "--gg-installer").map(PathBuf::from), crash_reports, ..plan::Options::default() })
    };
    if let Err(error) = window::run(mode) {
        win::attach_console();
        eprintln!("Platinum-Setup: {error}");
        std::process::exit(1);
    }
}

/// The administrator part writes nothing in the user's folders (it runs elevated, possibly as
/// another account): a panic goes to its journal, which the setup copies into install.log, and the
/// setup records the crash report itself when it sees the part end with a crash code.
fn elevated_panics_to(journal: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::Log::plain(&journal).line(format!("PANIC: {info}"));
        previous(info);
    }));
}

/// The console commands; returns the exit code.
fn console(args: &[String]) -> i32 {
    let flag = |name: &str| args.iter().any(|a| a == name);
    println!();
    if flag("--help") || flag("-h") {
        println!("{USAGE}");
        return 0;
    }
    if let Some(installer) = option(args, "--list") {
        return match nsis::list(std::path::Path::new(&installer)) {
            Ok(items) => {
                for item in &items {
                    println!("{:>12}  {}{}", item.offset, item.path, if plan::wanted(&item.path) { "  (taken by the setup)" } else { "" });
                }
                println!("{} entries, {} taken by the setup", items.len(), items.iter().filter(|i| plan::wanted(&i.path)).count());
                0
            }
            Err(error) => {
                println!("{error}");
                1
            }
        };
    }
    if flag("--extract") {
        let (Some(installer), Some(folder)) = (option(args, "--extract"), args.iter().position(|a| a == "--extract").and_then(|at| args.get(at + 2)).cloned()) else {
            println!("{USAGE}");
            return 2;
        };
        let started = std::time::Instant::now();
        let mut last = 0;
        let mut progress = |read: u64, total: u64| {
            let percent = read * 100 / total.max(1);
            if percent >= last + 10 {
                last = percent;
                println!("  {percent}% of the compressed data read");
            }
        };
        return match nsis::extract(std::path::Path::new(&installer), &plan::wanted, std::path::Path::new(&folder), &mut progress) {
            Ok(files) => {
                for file in &files {
                    println!("{:>10}  {}", file.size, file.path);
                }
                let missing = plan::missing_driver_files(std::path::Path::new(&folder));
                println!("{} files, {} bytes, in {:.1} s; driver files missing: {}", files.len(), files.iter().map(|f| f.size).sum::<u64>(), started.elapsed().as_secs_f64(), if missing.is_empty() { "none".to_string() } else { missing.join(", ") });
                i32::from(!missing.is_empty())
            }
            Err(error) => {
                println!("{error}");
                1
            }
        };
    }
    let machine = plan::Machine::gather();
    if flag("--uninstall") {
        let options = uninstall::Options { remove_data: false, remove_driver: uninstall::driver_offered(&machine) };
        println!("Platinum-Setup --dry-run --uninstall (nothing is changed)");
        for line in uninstall::describe(&machine, &options) {
            println!("{line}");
        }
        return 0;
    }
    // `--assume-clean`: what a machine without GG, driver or runtime would get (for reviewing the
    // whole sequence on a machine that has them).
    let machine = if flag("--assume-clean") { window::sample_clean_machine(machine.install_dir.clone()) } else { machine };
    let options = plan::Options { gg_installer: option(args, "--gg-installer").map(PathBuf::from), ..plan::Options::default() };
    let plan = plan::plan(&machine, &options);
    println!(
        "Platinum-Setup --dry-run{} (nothing is changed){}",
        if flag("--assume-clean") { " --assume-clean: a machine without GG, Sonar driver nor VC++ runtime" } else { "" },
        if payload::present() { String::new() } else { "; WARNING: this build carries no Platinum programs".to_string() }
    );
    for line in plan::describe(&machine, &options, &plan) {
        println!("{line}");
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// GG 123's installer, when the workspace has it (`PLATINUM_GG_INSTALLER` names another).
    pub fn local_gg_installer() -> Option<PathBuf> {
        let path = std::env::var_os("PLATINUM_GG_INSTALLER").map(PathBuf::from).unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../SteelSeriesGG123.0.0Setup.exe"));
        path.is_file().then_some(path)
    }

    fn setup_texts() -> (Vec<String>, Vec<String>) {
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

    /// Every text the setup marks is in every catalog, with the same placeholders.
    #[test]
    fn every_catalog_translates_every_setup_text() {
        let (slint_texts, rust_texts) = setup_texts();
        assert!(slint_texts.len() > 30 && rust_texts.len() > 40, "{} + {} texts", slint_texts.len(), rust_texts.len());
        let mut report = Vec::new();
        for (language, catalog) in SETUP_CATALOGS {
            let (slint_missing, slint_mismatched) = i18n::catalog_problems(&slint_texts, catalog);
            let (rust_missing, rust_mismatched) = i18n::catalog_problems(&rust_texts, catalog);
            let wrong = i18n::placeholder_problems(catalog);
            let missing = slint_missing.len() + rust_missing.len();
            if missing > 0 || !slint_mismatched.is_empty() || !rust_mismatched.is_empty() || !wrong.is_empty() {
                let all: Vec<String> = slint_missing.into_iter().chain(rust_missing).collect();
                report.push(format!("{}: {missing} missing, placeholders differ: {:?} {:?} {:?}\n{}", language.code(), slint_mismatched, rust_mismatched, wrong, i18n::po_fragment(&all)));
            }
        }
        assert!(report.is_empty(), "{}", report.join("\n"));
    }
}
