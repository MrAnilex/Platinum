//! The device manager's lists as the window shows them: each entry of a priority list with the
//! state of its device now, and the devices that can still be added. The lists themselves are
//! edited with `platinum::fallback`'s functions, the same the daemon's rules are tested with.

use platinum::devices::{Endpoint, Flow, State};
use platinum::fallback::PriorityDevice;
use platinum::i18n::{mark, tr};

/// One entry as shown.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// The device's name now, else the name it had when last seen, else its identifier.
    pub name: String,
    /// "branché", "absent", "désactivé", or why it is never chosen.
    pub state: &'static str,
    pub present: bool,
    pub excluded: bool,
    /// In use now.
    pub current: bool,
}

/// The state of a device, as the list says it: plugged in and enabled, disabled in Windows, or
/// not there (unplugged, or not even listed by Windows any more).
fn state(device: Option<&Endpoint>) -> &'static str {
    match device.map(|d| d.state) {
        Some(State::Active) => tr("branché"),
        Some(State::Disabled) => tr("désactivé"),
        _ => tr("absent"),
    }
}

/// Said of a Bluetooth headset's hands-free side that a hand-edited list holds: plugged in, but
/// the backup switch never chooses it.
const HANDS_FREE: &str = mark("mains libres Bluetooth, jamais choisi");

/// The rows of a list. `devices` are those the list can hold now (see `PriorityData`),
/// `hands_free` the Bluetooth hands-free endpoints left out of them, `current` the devices in use.
pub fn rows(list: &[PriorityDevice], devices: &[Endpoint], hands_free: &[String], current: &[String]) -> Vec<Row> {
    list.iter()
        .map(|entry| {
            let device = devices.iter().find(|d| d.id == entry.id);
            let name = device.map(|d| d.name.as_str()).filter(|n| !n.is_empty()).unwrap_or(if entry.name.is_empty() { entry.id.as_str() } else { entry.name.as_str() });
            Row {
                name: name.to_string(),
                state: if hands_free.contains(&entry.id) { tr(HANDS_FREE) } else { state(device) },
                present: device.is_some_and(|d| d.state == State::Active),
                excluded: entry.excluded,
                current: current.iter().any(|id| *id == entry.id),
            }
        })
        .collect()
}

/// The devices of one direction that the list does not hold yet, plugged-in ones first, each in
/// Windows' order.
pub fn addable<'a>(list: &[PriorityDevice], devices: &'a [Endpoint], flow: Flow) -> Vec<&'a Endpoint> {
    let mut found: Vec<&Endpoint> = devices.iter().filter(|d| d.flow == flow && !list.iter().any(|e| e.id == d.id)).collect();
    found.sort_by_key(|d| d.state != State::Active);
    found
}

/// How the "Ajouter" menu names a device: with its state when it is not plugged in.
pub fn addable_label(device: &Endpoint) -> String {
    match device.state {
        State::Active => device.name.clone(),
        _ => format!("{} ({})", device.name, state(Some(device))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, name: &str, state: State) -> Endpoint {
        Endpoint { id: id.into(), name: name.into(), flow: Flow::Render, state, virtual_role: None }
    }

    #[test]
    fn rows_say_where_each_device_is() {
        let devices = [device("a", "Haut-parleurs (Arena 7)", State::Active), device("b", "Écran", State::Disabled), device("c", "Realtek", State::Unplugged)];
        let list = [
            PriorityDevice::new("a", "Ancien nom"),
            PriorityDevice { id: "b".into(), name: "Écran".into(), excluded: true },
            PriorityDevice::new("c", "Realtek"),
            PriorityDevice::new("gone", "Casque USB"),
            PriorityDevice::new("nameless", ""),
            PriorityDevice::new("airpods-mic", "Casque (AirPods Pro)"),
        ];
        let rows = rows(&list, &devices, &["airpods-mic".to_string()], &["a".to_string()]);
        let summary: Vec<(&str, &str, bool, bool, bool)> = rows.iter().map(|r| (r.name.as_str(), r.state, r.present, r.excluded, r.current)).collect();
        assert_eq!(
            summary,
            [
                ("Haut-parleurs (Arena 7)", "branché", true, false, true),
                ("Écran", "désactivé", false, true, false),
                ("Realtek", "absent", false, false, false),
                ("Casque USB", "absent", false, false, false),
                ("nameless", "absent", false, false, false),
                ("Casque (AirPods Pro)", HANDS_FREE, false, false, false),
            ]
        );
    }

    #[test]
    fn only_devices_missing_from_the_list_can_be_added() {
        let mut capture = device("m", "Micro", State::Active);
        capture.flow = Flow::Capture;
        let devices = [device("a", "A", State::Unplugged), device("b", "B", State::Active), device("c", "C", State::Active), capture];
        let list = [PriorityDevice::new("c", "C")];
        let names: Vec<String> = addable(&list, &devices, Flow::Render).into_iter().map(addable_label).collect();
        assert_eq!(names, ["B", "A (absent)"]);
        assert_eq!(addable(&[], &devices, Flow::Capture).len(), 1);
    }
}
