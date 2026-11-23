//! `platinum autostart`: starting the daemon at logon, through the current user's Run key.
//!
//! `enable` copies platinum.exe, platinum-start.exe and platinum-ui.exe into a folder of their own
//! (a rebuild or a move of the build folder then cannot break the logon start), writes the
//! daemon's settings to the configuration file there, and adds one value to
//! HKCU\Software\Microsoft\Windows\CurrentVersion\Run: `"<folder>\platinum-start.exe" daemon`,
//! which reads the configuration. `disable` removes that value. Nothing else is touched: GG's own
//! start entry is left to the user (Task Manager, Startup apps).
//!
//! Older Run values carry the daemon's settings as arguments
//! (`… daemon Seiren --fallback-output AirPods`); they keep working, since arguments override the
//! configuration, and `enable` turns them into the configuration file before replacing them.

use crate::com::wide;
use crate::config::{self, Arguments, Origin};
use std::ffi::c_void;
use std::path::PathBuf;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "Platinum";
/// Where GG registers its own start, and where Task Manager records it as disabled.
const GG_VALUE: &str = "SteelSeriesGG";
const APPROVED_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

const HKEY_CURRENT_USER: isize = 0x8000_0001_u32 as i32 as isize;
const HKEY_LOCAL_MACHINE: isize = 0x8000_0002_u32 as i32 as isize;
const REG_SZ: u32 = 1;
const RRF_RT_REG_SZ: u32 = 0x0000_0002;
const RRF_RT_REG_BINARY: u32 = 0x0000_0008;
const ERROR_FILE_NOT_FOUND: i32 = 2;

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegSetKeyValueW(key: isize, subkey: *const u16, value: *const u16, kind: u32, data: *const c_void, bytes: u32) -> i32;
    fn RegDeleteKeyValueW(key: isize, subkey: *const u16, value: *const u16) -> i32;
    fn RegGetValueW(key: isize, subkey: *const u16, value: *const u16, flags: u32, kind: *mut u32, data: *mut c_void, bytes: *mut u32) -> i32;
}

/// The folder Platinum is installed into: outside AppData, which packaged applications redirect.
pub fn install_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("USERPROFILE").ok_or("USERPROFILE is not set")?;
    Ok(PathBuf::from(home).join("Platinum"))
}

fn read_value(root: isize, key: &str, value: &str, flags: u32) -> Option<Vec<u8>> {
    let (key, value) = (wide(key), wide(value));
    let mut bytes = 0u32;
    unsafe {
        if RegGetValueW(root, key.as_ptr(), value.as_ptr(), flags, std::ptr::null_mut(), std::ptr::null_mut(), &mut bytes) != 0 {
            return None;
        }
        let mut data = vec![0u8; bytes as usize];
        if RegGetValueW(root, key.as_ptr(), value.as_ptr(), flags, std::ptr::null_mut(), data.as_mut_ptr().cast(), &mut bytes) != 0 {
            return None;
        }
        data.truncate(bytes as usize);
        Some(data)
    }
}

fn read_text(root: isize, key: &str, value: &str) -> Option<String> {
    let data = read_value(root, key, value, RRF_RT_REG_SZ)?;
    let units: Vec<u16> = data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|u| *u != 0).collect();
    Some(String::from_utf16_lossy(&units))
}

/// An argument as it must appear on a command line.
fn quoted(argument: &str) -> String {
    if argument.is_empty() || argument.contains([' ', '\t']) { format!("\"{argument}\"") } else { argument.to_string() }
}

/// The command Windows runs at logon for these daemon arguments.
pub fn logon_command(daemon_args: &[&str]) -> Result<String, String> {
    let launcher = install_dir()?.join("platinum-start.exe");
    let arguments: Vec<String> = daemon_args.iter().map(|a| quoted(a)).collect();
    Ok(format!("\"{}\" daemon {}", launcher.display(), arguments.join(" ")).trim_end().to_string())
}

/// `platinum autostart enable [daemon arguments]`: installs the programs, writes the configuration
/// (the arguments given over the current one; without arguments the current one is kept, and
/// written if it only came from an old logon command), then the Run value.
pub fn enable(daemon_args: &[&str]) -> Result<(), String> {
    // Checked first, as the daemon will read them: nothing is changed if they are wrong.
    let arguments = Arguments::parse(daemon_args)?;
    let target = install_dir()?;
    install_programs(&target)?;
    write_configuration(&arguments)?;
    // `--no-tray` is not a setting of the file: it stays on the command line.
    let command = logon_command(if arguments.no_tray { &["--no-tray"] } else { &[] })?;
    write_run_value(&command).map_err(|status| format!("cannot write the Run value (Windows error {status})"))?;
    println!("added    HKCU\\{RUN_KEY} \"{VALUE_NAME}\" = {command}");
    if !startup_approved() {
        println!("note: Platinum is disabled in Task Manager (Startup apps): Windows will not start it until it is enabled there again");
    }
    println!("the daemon will start at the next logon; its output goes to {}", target.join("daemon.log").display());
    print_gg_state();
    Ok(())
}

/// Writes only the Run value, `"<install folder>\platinum-start.exe" daemon` (the daemon then
/// reads the configuration file), for the window's "start with Windows" switch once Platinum is
/// installed: it copies nothing, so a window run from a build folder cannot replace the installed
/// programs. It also replaces an old Run value that carried settings, which would otherwise
/// override the file at logon. A `--no-tray` the user put in the Run value stays: it is not a
/// setting of the file, so the command line is the only place that remembers it. Error in French.
pub fn register() -> Result<(), String> {
    let launcher = install_dir()?.join("platinum-start.exe");
    if !launcher.exists() {
        return Err(crate::trf!("Platinum n'est pas installé : {file} manque", file = launcher.display()));
    }
    let no_tray = logon_arguments().is_some_and(|args| args.iter().any(|a| a == "--no-tray"));
    let command = logon_command(if no_tray { &["--no-tray"] } else { &[] })?;
    write_run_value(&command).map_err(|status| crate::trf!("impossible d'écrire la valeur de démarrage (erreur Windows {code})", code = status))
}

/// Whether the Run value still carries settings that the configuration file should hold instead
/// (an old command naming the microphone or the fallbacks); `--no-tray` alone is not one.
pub fn logon_overrides_config() -> bool {
    logon_arguments().is_some_and(|args| carries_settings(&args))
}

fn carries_settings(args: &[String]) -> bool {
    Arguments::parse(args).is_ok_and(|parsed| parsed.overrides_config())
}

/// Sets the Run value; the error is Windows' status code.
fn write_run_value(command: &str) -> Result<(), i32> {
    let data = wide(command);
    let status = unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), wide(VALUE_NAME).as_ptr(), REG_SZ, data.as_ptr().cast(), (data.len() * 2) as u32) };
    if status == 0 { Ok(()) } else { Err(status) }
}

/// Copies the programs built next to this one into the install folder.
fn install_programs(target: &std::path::Path) -> Result<(), String> {
    let source = std::env::current_exe().map_err(|e| e.to_string())?;
    let source = source.parent().ok_or("cannot find the folder of platinum.exe")?;
    std::fs::create_dir_all(target).map_err(|e| format!("cannot create {}: {e}", target.display()))?;
    for name in ["platinum.exe", "platinum-start.exe", "platinum-ui.exe"] {
        let from = source.join(name);
        let to = target.join(name);
        // The window is optional: it is copied when it was built next to the command line.
        if from == to || (name == "platinum-ui.exe" && !from.exists()) {
            continue;
        }
        std::fs::copy(&from, &to).map_err(|e| format!("cannot copy {} to {}: {e} (is the daemon running? `platinum daemon --stop` first)", from.display(), to.display()))?;
        println!("copied   {}", to.display());
    }
    Ok(())
}

/// Writes the configuration the logon start will use, before the Run value is replaced: the
/// settings an old Run value carried must not be lost with it.
fn write_configuration(arguments: &Arguments) -> Result<(), String> {
    let loaded = config::load_full();
    for problem in &loaded.problems {
        println!("platinum.ini: {problem}");
    }
    if loaded.unreadable {
        return Err("platinum.ini exists but cannot be read: not overwritten (close the program holding it, then try again)".into());
    }
    let mut settings = loaded.config;
    settings.apply(arguments);
    let file = config::path()?;
    if arguments.overrides_config() || loaded.origin != Origin::File {
        settings.save()?;
        let from = match loaded.origin {
            Origin::LogonCommand if !arguments.overrides_config() => " (from the previous logon command)",
            _ => "",
        };
        println!("wrote    {}{from}", file.display());
    } else {
        println!("kept     {}", file.display());
    }
    print_settings(&settings);
    if settings.microphone.is_none() {
        println!("no microphone chosen yet: the daemon relays Windows' default microphone until one is chosen in the window or with `platinum autostart enable <microphone>`");
    }
    Ok(())
}

fn print_settings(settings: &config::Config) {
    let or_none = |value: &Option<String>| value.clone().unwrap_or_else(|| "(none)".into());
    println!("         microphone {}, effects {}, gain {}, channels {}", or_none(&settings.microphone), settings.effects, settings.gain, settings.channels.join(","));
    println!("         fallback output {}, fallback input {}, window starts the daemon: {}", or_none(&settings.fallback_output), or_none(&settings.fallback_input), settings.start_on_open);
}

pub fn disable() -> Result<(), String> {
    let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), wide(VALUE_NAME).as_ptr()) };
    match status {
        0 => println!("removed  HKCU\\{RUN_KEY} \"{VALUE_NAME}\": the daemon no longer starts at logon"),
        ERROR_FILE_NOT_FOUND => println!("Platinum was not set to start at logon"),
        other => return Err(format!("cannot remove the Run value (Windows error {other})")),
    }
    println!("the installed copy in {} is left in place", install_dir()?.display());
    Ok(())
}

/// The command Windows runs at logon for Platinum, if it is set to start.
pub fn logon_value() -> Option<String> {
    read_text(HKEY_CURRENT_USER, RUN_KEY, VALUE_NAME)
}

/// The daemon's arguments in the logon command (after `daemon`): empty for the command written
/// now, which leaves the settings to the configuration file; `None` without a Run value.
pub fn logon_arguments() -> Option<Vec<String>> {
    let (_, rest) = split_command(&logon_value()?)?;
    let arguments = split_arguments(&rest);
    Some(arguments.into_iter().skip_while(|a| a != "daemon").skip(1).collect())
}

/// Whether Task Manager lets the Run value start Platinum. Disabling an entry in Task Manager
/// (Startup apps) keeps the Run value and writes an approval record whose first byte is odd.
/// No record means allowed.
pub fn startup_approved() -> bool {
    !approval_disables(read_value(HKEY_CURRENT_USER, APPROVED_KEY, VALUE_NAME, RRF_RT_REG_BINARY).as_deref())
}

/// Whether Windows will start Platinum at the next logon: a Run value that Task Manager allows.
pub fn starts_at_logon() -> bool {
    logon_value().is_some() && startup_approved()
}

/// An approval record (StartupApproved\Run) disables its entry when its first byte is odd.
fn approval_disables(record: Option<&[u8]>) -> bool {
    record.and_then(|r| r.first()).is_some_and(|b| b % 2 == 1)
}

/// Splits a logon command into the launcher and the arguments that follow it, as written.
pub fn split_command(command: &str) -> Option<(String, String)> {
    let command = command.trim();
    if let Some(rest) = command.strip_prefix('"') {
        let (program, arguments) = rest.split_once('"')?;
        Some((program.to_string(), arguments.trim().to_string()))
    } else {
        let (program, arguments) = command.split_once(' ').unwrap_or((command, ""));
        Some((program.to_string(), arguments.trim().to_string()))
    }
}

/// The daemon's arguments split as a command line is ("Razer Seiren" stays one argument).
pub fn split_arguments(text: &str) -> Vec<String> {
    let (mut arguments, mut current, mut quoted, mut any) = (Vec::new(), String::new(), false, false);
    for c in text.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            ' ' | '\t' if !quoted => {
                if any {
                    arguments.push(std::mem::take(&mut current));
                    any = false;
                }
            }
            _ => {
                current.push(c);
                any = true;
            }
        }
    }
    if any {
        arguments.push(current);
    }
    arguments
}

/// `platinum autostart status`: the logon start, the configuration and the daemon's state. Reads
/// only.
pub fn status() -> Result<(), String> {
    match logon_value() {
        Some(command) if startup_approved() => println!("Platinum starts at logon: {command}"),
        Some(command) => println!("Platinum is disabled in Task Manager (Startup apps), so Windows does not start it at logon; its Run value is still {command}"),
        None => println!("Platinum does not start at logon"),
    }
    let loaded = config::load_full();
    let origin = match loaded.origin {
        Origin::File => "",
        Origin::LogonCommand => " (no file yet: read from the logon command)",
        Origin::Defaults => " (no file yet: defaults)",
    };
    println!("configuration {}{origin}", config::path()?.display());
    print_settings(&loaded.config);
    for problem in &loaded.problems {
        println!("         skipped: {problem}");
    }
    match crate::status::read() {
        Some(daemon) => println!("daemon   {} ({}), updated {}: {}", daemon.state.key(), daemon.state.label(), daemon.updated, daemon.summary()),
        None => println!("daemon   no status published yet"),
    }
    print_gg_state();
    Ok(())
}

/// GG's start entry (`HKLM\…\Run\SteelSeriesGG`) and whether Task Manager has it disabled;
/// `None` without one.
pub fn gg_start_entry() -> Option<(String, bool)> {
    let command = read_text(HKEY_LOCAL_MACHINE, RUN_KEY, GG_VALUE)?;
    let approval = read_value(HKEY_LOCAL_MACHINE, APPROVED_KEY, GG_VALUE, RRF_RT_REG_BINARY);
    Some((command, approval_disables(approval.as_deref())))
}

/// GG's start entry and whether Task Manager has it disabled (first byte of the approval record:
/// even means enabled, odd means disabled).
fn print_gg_state() {
    let Some((command, disabled)) = gg_start_entry() else {
        println!("GG has no start entry in HKLM\\{RUN_KEY}");
        return;
    };
    if disabled {
        println!("GG's start at logon is disabled (Task Manager, Startup apps)");
    } else {
        println!("GG still starts at logon ({command}).");
        println!("To let Platinum take over, disable \"SteelSeries GG\" in Task Manager, Startup apps. If both start, Platinum steps aside when GG's Sonar appears.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logon_command_quotes_what_needs_it() {
        let command = logon_command(&["Razer Seiren", "--gain", "0.8"]).unwrap();
        assert!(command.starts_with('"') && command.contains("\\Platinum\\platinum-start.exe\" daemon \"Razer Seiren\" --gain 0.8"), "{command}");
        assert_eq!(quoted(""), "\"\"");
        assert_eq!(quoted("Seiren"), "Seiren");
    }

    #[test]
    fn logon_command_splits_back() {
        let command = logon_command(&["Razer Seiren", "--fallback-output", "AirPods"]).unwrap();
        let (program, arguments) = split_command(&command).unwrap();
        assert!(program.ends_with("platinum-start.exe"), "{program}");
        assert_eq!(split_arguments(&arguments), ["daemon", "Razer Seiren", "--fallback-output", "AirPods"]);
        assert_eq!(split_arguments(" a  \"\" b "), ["a", "", "b"]);
    }

    #[test]
    fn the_logon_command_leaves_the_settings_to_the_file() {
        let command = logon_command(&[]).unwrap();
        assert!(command.starts_with('"') && command.ends_with("\\Platinum\\platinum-start.exe\" daemon"), "{command}");
        let (_, arguments) = split_command(&command).unwrap();
        assert_eq!(split_arguments(&arguments), ["daemon"]);
        assert!(logon_command(&["--no-tray"]).unwrap().ends_with("platinum-start.exe\" daemon --no-tray"));
    }

    #[test]
    fn only_settings_make_the_logon_command_outdated() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert!(!carries_settings(&args(&[])));
        assert!(!carries_settings(&args(&["--no-tray"])));
        assert!(carries_settings(&args(&["Seiren", "--no-tray"])));
        assert!(carries_settings(&args(&["--fallback-output", "AirPods"])));
    }

    #[test]
    fn task_manager_disables_with_an_odd_first_byte() {
        assert!(!approval_disables(None));
        assert!(!approval_disables(Some(&[])));
        assert!(!approval_disables(Some(&[2, 0, 0, 0])));
        assert!(!approval_disables(Some(&[6, 0])));
        assert!(approval_disables(Some(&[3, 0, 0, 0, 0x10, 0x20])));
        assert!(approval_disables(Some(&[1])));
    }
}
