//! `platinum rename`: giving the virtual channels Platinum's names, and giving them back theirs.
//!
//! Windows shows an endpoint as "<description> (<interface name>)". Both parts are properties of
//! the endpoint that a standard user may rewrite, as the Windows sound settings do when the user
//! renames a device; Windows then rebuilds the combined friendly name itself (writing that one is
//! refused with E_ACCESSDENIED, observed on 15/11/2026). The virtual devices are found by the role the driver tags them
//! with, so they are found again whatever they are called.
//!
//! Which names the channels carry is `channel_names` in platinum.ini (`ChannelNames`): Platinum's
//! by default, at the user's request, since they read better in Windows and in every application.
//! The daemon applies the choice when it starts (`apply_at_start`), which also puts Platinum's
//! names back where Windows reset them; the window's settings page writes the key and applies it at
//! once (`choose`).
//!
//! Before the first rename, the names in place are written to `original-names.txt` in the install
//! folder (never Platinum's own: a name of ours found there is kept as the driver's); `rename
//! --undo` and SteelSeries' names put them back (or the driver's names, if that file is gone).
//! Before the key existed, that file's presence was the choice itself: `config` still reads it so
//! for a platinum.ini written then.

use crate::devices::{Devices, Endpoint, NameField};
use std::path::PathBuf;

/// The names the virtual channels carry in Windows (`channel_names` in platinum.ini).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChannelNames {
    /// "Gaming (Platinum)", "Microphone (Platinum)"…
    #[default]
    Platinum,
    /// The ones the devices had before Platinum: the driver's, "SteelSeries Sonar - Gaming"…
    SteelSeries,
}

impl ChannelNames {
    pub fn key(self) -> &'static str {
        match self {
            ChannelNames::Platinum => "platinum",
            ChannelNames::SteelSeries => "steelseries",
        }
    }

    pub fn parse(text: &str) -> Option<ChannelNames> {
        match text.trim().to_ascii_lowercase().as_str() {
            "platinum" => Some(ChannelNames::Platinum),
            "steelseries" => Some(ChannelNames::SteelSeries),
            _ => None,
        }
    }
}

/// Role given by the driver, Platinum's name for it, and the driver's own name.
const NAMES: [(u32, &str, &str); 6] = [
    (1, "Chat", "SteelSeries Sonar - Chat"),
    (2, "Gaming", "SteelSeries Sonar - Gaming"),
    (3, "Microphone", "SteelSeries Sonar - Microphone"),
    (4, "Stream", "SteelSeries Sonar - Stream"),
    (5, "Media", "SteelSeries Sonar - Media"),
    (6, "Aux", "SteelSeries Sonar - Aux"),
];
const INTERFACE: &str = "Platinum";
const DRIVER_INTERFACE: &str = "SteelSeries Sonar Virtual Audio Device";

/// The three names an endpoint carries, in the order they are written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Names {
    pub description: String,
    pub interface: String,
}

impl Names {
    fn friendly(&self) -> String {
        format!("{} ({})", self.description, self.interface)
    }

    fn value(&self, field: NameField) -> String {
        match field {
            NameField::Description => self.description.clone(),
            NameField::Interface => self.interface.clone(),
            NameField::Friendly => self.friendly(),
        }
    }
}

/// Platinum's names for a role of the virtual driver.
pub fn platinum_names(role: u32) -> Option<Names> {
    NAMES.iter().find(|(r, _, _)| *r == role).map(|(_, name, _)| Names { description: name.to_string(), interface: INTERFACE.to_string() })
}

fn driver_names(role: u32) -> Option<Names> {
    NAMES.iter().find(|(r, _, _)| *r == role).map(|(_, _, name)| Names { description: name.to_string(), interface: DRIVER_INTERFACE.to_string() })
}

fn originals_file() -> Result<PathBuf, String> {
    Ok(crate::autostart::install_dir()?.join("original-names.txt"))
}

/// One line per endpoint: identifier, description, interface name, separated by tabs.
fn format_originals(entries: &[(String, Names)]) -> String {
    entries.iter().map(|(id, names)| format!("{id}\t{}\t{}\n", names.description, names.interface)).collect()
}

fn parse_originals(text: &str) -> Vec<(String, Names)> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let (id, description, interface) = (parts.next()?, parts.next()?, parts.next()?);
            Some((id.to_string(), Names { description: description.to_string(), interface: interface.to_string() }))
        })
        .collect()
}

fn virtual_endpoints(devices: &Devices) -> Result<Vec<Endpoint>, String> {
    let endpoints: Vec<Endpoint> = devices.list().map_err(|e| e.to_string())?.into_iter().filter(|e| e.virtual_role.is_some()).collect();
    if endpoints.is_empty() {
        return Err("no virtual channel device on this machine: the audio driver does not seem to be installed".into());
    }
    Ok(endpoints)
}

fn current_names(devices: &Devices, endpoint: &Endpoint) -> Names {
    Names {
        description: devices.name(&endpoint.id, NameField::Description).unwrap_or_default(),
        interface: devices.name(&endpoint.id, NameField::Interface).unwrap_or_default(),
    }
}

/// What a rename did: one line per name written (for the command line), and the failures.
#[derive(Default)]
struct Outcome {
    written: Vec<String>,
    failures: Vec<String>,
}

impl Outcome {
    /// Writes the two names, field by field.
    fn write(&mut self, devices: &Devices, endpoint: &Endpoint, names: &Names) {
        for field in [NameField::Description, NameField::Interface] {
            let value = names.value(field);
            match devices.set_name(&endpoint.id, field, &value) {
                Ok(()) => self.written.push(format!("{:<7} {:<11} {value}", endpoint.flow.label(), field.label())),
                Err(error) => self.failures.push(format!("{} {}: {error}", endpoint.name, field.label())),
            }
        }
    }

    /// Prints what was written (the command line), then says whether everything was.
    fn report(self) -> Result<(), String> {
        self.written.iter().for_each(|line| println!("{line}"));
        failures_error(&self.failures)
    }
}

fn failures_error(failures: &[String]) -> Result<(), String> {
    if failures.is_empty() { Ok(()) } else { Err(format!("some names could not be written:\n  {}", failures.join("\n  "))) }
}

/// `platinum names`: the names each virtual device carries now.
pub fn show() -> Result<(), String> {
    let devices = Devices::open().map_err(|e| e.to_string())?;
    for endpoint in virtual_endpoints(&devices)? {
        let names = current_names(&devices, &endpoint);
        let role = endpoint.virtual_role.unwrap_or(0);
        println!("{:<7} role {role}  {:<34} ({})  {}", endpoint.flow.label(), names.description, names.interface, endpoint.id);
    }
    Ok(())
}

/// `platinum rename`: Platinum's names on every virtual device.
pub fn apply() -> Result<(), String> {
    let devices = Devices::open().map_err(|e| e.to_string())?;
    to_platinum(&devices)?.report()
}

/// Platinum's names on every virtual device, the names in place kept first in
/// `original-names.txt` (only the first time).
fn to_platinum(devices: &Devices) -> Result<Outcome, String> {
    let endpoints = virtual_endpoints(devices)?;
    let mut outcome = Outcome::default();
    if let Some(line) = keep_originals(devices, &endpoints)? {
        outcome.written.push(line);
    }
    for endpoint in &endpoints {
        if let Some(names) = endpoint.virtual_role.and_then(platinum_names) {
            outcome.write(devices, endpoint, &names);
        }
    }
    Ok(outcome)
}

/// Writes `original-names.txt` when it is missing: the names found the first time are the ones to
/// come back to, never overwritten with ours. Returns the line saying so when it was written.
fn keep_originals(devices: &Devices, endpoints: &[Endpoint]) -> Result<Option<String>, String> {
    let file = originals_file()?;
    if file.exists() {
        return Ok(None);
    }
    let originals: Vec<(String, Names)> = endpoints.iter().map(|e| (e.id.clone(), originals_to_keep(&current_names(devices, e), e.virtual_role.unwrap_or(0)))).collect();
    std::fs::create_dir_all(file.parent().unwrap_or(&file)).map_err(|e| format!("cannot create {}: {e}", file.display()))?;
    std::fs::write(&file, format_originals(&originals)).map_err(|e| format!("cannot write {}: {e}", file.display()))?;
    Ok(Some(format!("original names kept in {}", file.display())))
}

/// The names to keep as an endpoint's originals: those it carries, except a field that is already
/// Platinum's (the file was deleted after a rename), which is kept as the driver's.
fn originals_to_keep(current: &Names, role: u32) -> Names {
    let (Some(ours), Some(driver)) = (platinum_names(role), driver_names(role)) else { return current.clone() };
    Names {
        description: if current.description == ours.description || current.description.is_empty() { driver.description } else { current.description.clone() },
        interface: if current.interface == ours.interface || current.interface.is_empty() { driver.interface } else { current.interface.clone() },
    }
}

/// Whether an endpoint carries (part of) Platinum's names.
fn carries_platinum_names(current: &Names, role: u32) -> bool {
    platinum_names(role).is_some_and(|ours| current.description == ours.description || current.interface == ours.interface)
}

/// Whether `original-names.txt` exists: before `channel_names` existed, it meant that the user had
/// chosen Platinum's names (`config::names_for_old_file`).
pub fn originals_exist() -> bool {
    originals_file().is_ok_and(|file| file.exists())
}

/// Whether the channels are to carry Platinum's names, as platinum.ini says (`channel_names`).
pub fn chosen() -> bool {
    crate::config::load().channel_names == ChannelNames::Platinum
}

/// The window's choice of names: Platinum's (`true`) or the ones the devices had before, as
/// `rename` and `rename --undo` do, without printing. The window writes `channel_names` first. The
/// error (technical, in English as the command line says it) lists the names that could not be
/// written.
pub fn choose(devices: &Devices, platinum: bool) -> Result<(), String> {
    let outcome = if platinum { to_platinum(devices)? } else { to_originals(devices, |_, _| true)? };
    failures_error(&outcome.failures)
}

/// The fields of an endpoint's names that differ from the wanted ones, in writing order.
fn differing(current: &Names, wanted: &Names) -> Vec<NameField> {
    [NameField::Description, NameField::Interface].into_iter().filter(|field| current.value(*field) != wanted.value(*field)).collect()
}

/// At the daemon's start, the names platinum.ini chooses. Platinum's: the names in place are kept
/// in `original-names.txt` first (once), then only the fields that differ are written, which also
/// puts back what Windows reset: re-enumerating the devices (a driver update, a USB reset) gives
/// the interface name back its driver's value, "Gaming (SteelSeries Sonar Virtual Audio Device)",
/// while the description stays. SteelSeries': the channels still carrying Platinum's names get
/// their original ones back; the others are left as they are. Returns what it did, in English, for
/// the daemon's log; nothing when there was nothing to do.
pub fn apply_at_start(devices: &Devices, choice: ChannelNames) -> Result<Vec<String>, String> {
    let endpoints: Vec<Endpoint> = devices.list().map_err(|e| e.to_string())?.into_iter().filter(|e| e.virtual_role.is_some()).collect();
    if endpoints.is_empty() {
        return Ok(Vec::new());
    }
    if choice == ChannelNames::SteelSeries {
        let outcome = to_originals(devices, carries_platinum_names)?;
        let mut done: Vec<String> = outcome.written.iter().map(|line| format!("SteelSeries' names: {}", line.trim())).collect();
        done.extend(outcome.failures.iter().map(|failure| format!("SteelSeries' name not put back: {failure}")));
        return Ok(done);
    }
    let mut done: Vec<String> = keep_originals(devices, &endpoints)?.into_iter().collect();
    for endpoint in &endpoints {
        let Some(wanted) = endpoint.virtual_role.and_then(platinum_names) else { continue };
        let current = current_names(devices, endpoint);
        for field in differing(&current, &wanted) {
            let (before, after) = (current.value(field), wanted.value(field));
            match devices.set_name(&endpoint.id, field, &after) {
                Ok(()) => done.push(format!("name: {} {} {} \"{before}\" -> \"{after}\"", wanted.description, endpoint.flow.label(), field.label())),
                Err(error) => done.push(format!("name: {} {} {} not put back ({error})", wanted.description, endpoint.flow.label(), field.label())),
            }
        }
    }
    Ok(done)
}

/// `platinum rename --undo`: the names the devices had before the first rename. platinum.ini is
/// left alone (the uninstaller runs this too, and a later install starts from Platinum's names
/// again): while it says `channel_names = platinum`, the daemon's next start renames them again,
/// which is said.
pub fn undo() -> Result<(), String> {
    let devices = Devices::open().map_err(|e| e.to_string())?;
    to_originals(&devices, |_, _| true)?.report()?;
    if chosen() {
        println!("note: platinum.ini says channel_names = platinum: the daemon gives the channels Platinum's names again when it starts (set channel_names = steelseries, or choose SteelSeries in the window's settings, to keep these)");
    }
    Ok(())
}

/// The names the devices had before the first rename (the driver's, when `original-names.txt` is
/// gone), on the virtual devices `wanted` picks by their current names and role; the file is
/// removed once every name is back (a later rename keeps the names in place then, which are these).
fn to_originals(devices: &Devices, wanted: impl Fn(&Names, u32) -> bool) -> Result<Outcome, String> {
    let file = originals_file()?;
    let originals = std::fs::read_to_string(&file).map(|text| parse_originals(&text)).unwrap_or_default();
    let mut outcome = Outcome::default();
    let mut missing_said = false;
    for endpoint in virtual_endpoints(devices)? {
        let role = endpoint.virtual_role.unwrap_or(0);
        if !wanted(&current_names(devices, &endpoint), role) {
            continue;
        }
        if originals.is_empty() && !missing_said {
            outcome.written.push(format!("{} not found: putting back the driver's names", file.display()));
            missing_said = true;
        }
        let kept = originals.iter().find(|(id, _)| *id == endpoint.id).map(|(_, names)| names.clone());
        if let Some(names) = kept.or_else(|| driver_names(role)) {
            outcome.write(devices, &endpoint, &names);
        }
    }
    // Kept while a name failed, so that a second try still knows them.
    if outcome.failures.is_empty() && file.exists() {
        std::fs::remove_file(&file).map_err(|e| format!("names put back, but {} could not be removed: {e}", file.display()))?;
        outcome.written.push(format!("{} removed", file.display()));
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_windows_pattern() {
        let names = platinum_names(2).unwrap();
        assert_eq!(names.value(NameField::Friendly), "Gaming (Platinum)");
        assert_eq!(driver_names(3).unwrap().value(NameField::Friendly), "SteelSeries Sonar - Microphone (SteelSeries Sonar Virtual Audio Device)");
        assert!(platinum_names(9).is_none());
    }

    #[test]
    fn only_the_names_windows_reset_are_written_again() {
        let wanted = platinum_names(2).unwrap();
        // Re-enumerated: the description stayed, the interface name is the driver's again.
        let reset = Names { description: "Gaming".into(), interface: DRIVER_INTERFACE.into() };
        assert_eq!(differing(&reset, &wanted), [NameField::Interface]);
        assert!(differing(&wanted, &wanted).is_empty());
        assert_eq!(differing(&driver_names(2).unwrap(), &wanted), [NameField::Description, NameField::Interface]);
    }

    #[test]
    fn our_own_names_are_never_kept_as_the_originals() {
        // A user's own name stays; a field of ours, or an empty one, is kept as the driver's.
        let own = Names { description: "Casque de jeu".into(), interface: DRIVER_INTERFACE.into() };
        assert_eq!(originals_to_keep(&own, 2), own);
        let ours = platinum_names(2).unwrap();
        assert_eq!(originals_to_keep(&ours, 2), driver_names(2).unwrap());
        let half = Names { description: "Gaming".into(), interface: String::new() };
        assert_eq!(originals_to_keep(&half, 2), driver_names(2).unwrap());
        // Which channels SteelSeries' choice puts back: those with a name of ours.
        assert!(carries_platinum_names(&ours, 2) && carries_platinum_names(&half, 2));
        assert!(!carries_platinum_names(&driver_names(2).unwrap(), 2) && !carries_platinum_names(&own, 2));
        assert!(!carries_platinum_names(&ours, 9));
    }

    #[test]
    fn the_choice_is_read_and_written_as_platinum_ini_says_it() {
        assert_eq!(ChannelNames::default(), ChannelNames::Platinum);
        for choice in [ChannelNames::Platinum, ChannelNames::SteelSeries] {
            assert_eq!(ChannelNames::parse(choice.key()), Some(choice));
        }
        assert_eq!(ChannelNames::parse(" SteelSeries "), Some(ChannelNames::SteelSeries));
        assert_eq!(ChannelNames::parse("sonar"), None);
    }

    #[test]
    fn originals_survive_a_round_trip() {
        let entries = vec![("{0.0.0.00000000}.{abc}".to_string(), Names { description: "SteelSeries Sonar - Chat".into(), interface: DRIVER_INTERFACE.into() })];
        assert_eq!(parse_originals(&format_originals(&entries)), entries);
        assert!(parse_originals("broken line\n").is_empty());
    }
}
