//! GG's backup device switch ("Changement automatique de périphérique de secours"): which real
//! device the channels and the microphone should use, from the user's priority lists.
//!
//! Two lists, kept in Platinum's configuration: one for the outputs, shared by the four output
//! channels as in GG's "link all" case, and one for the microphones. Each entry is a device
//! identifier with the name it had when last seen (it may be unplugged now) and an "excluded" mark:
//! an excluded device is never chosen automatically (GG's "Périphériques que vous ne voulez jamais
//! utiliser").
//!
//! The rule is GG's: the highest device of the list that is usable now wins, so a channel moves off
//! a device that disappears and comes back to it when it returns. Usable means plugged in and
//! enabled (`State::Active`), a real device (not one of the virtual channels) and not the hands-free
//! side of a Bluetooth headset, which would switch the headset to its call profile and spoil the
//! music. The microphone the user chose in the window comes before the input list: the list only
//! says what to use while it is missing.
//!
//! Everything here is pure (devices and lists in, a choice out), so that the daemon, the command
//! line and the window agree, and the tests can check it without any device.

use crate::devices::{Endpoint, Flow, State};

/// One entry of a priority list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriorityDevice {
    /// The endpoint's identifier: it stays the same across reboots, renames and replugging.
    pub id: String,
    /// Its name when last seen, to show it while it is unplugged.
    pub name: String,
    /// Never chosen automatically.
    pub excluded: bool,
}

impl PriorityDevice {
    pub fn new(id: &str, name: &str) -> PriorityDevice {
        PriorityDevice { id: id.to_string(), name: name.to_string(), excluded: false }
    }
}

/// Whether an endpoint may be chosen for `flow` at all: a real device of that direction, plugged in
/// and enabled, and not a Bluetooth hands-free endpoint (`hands_free` lists those, by identifier).
pub fn usable(endpoint: &Endpoint, flow: Flow, hands_free: &[String]) -> bool {
    endpoint.flow == flow && endpoint.state == State::Active && !is_virtual(endpoint) && !hands_free.iter().any(|id| *id == endpoint.id)
}

/// One of the virtual channel devices (by the role their driver tags them with, or by their
/// original name when the tag is missing).
pub fn is_virtual(endpoint: &Endpoint) -> bool {
    crate::session::virtual_kind(endpoint).is_some()
}

/// The device the list picks now: its highest entry that is not excluded and whose device is
/// usable. `None` when no entry is.
pub fn choose<'a>(list: &[PriorityDevice], endpoints: &'a [Endpoint], flow: Flow, hands_free: &[String]) -> Option<&'a Endpoint> {
    list.iter().filter(|entry| !entry.excluded).find_map(|entry| endpoints.iter().find(|e| e.id == entry.id && usable(e, flow, hands_free)))
}

/// What the switch does to the output channels.
#[derive(Debug)]
pub struct OutputPlan<'a> {
    /// The device every routed channel should play to; `None` when no device of the list is usable
    /// (the channels are then left where they are: a channel without sound helps nobody).
    pub target: Option<&'a Endpoint>,
    /// The channels to move, by their position in the `routed` list given, with the device they
    /// play to now.
    pub moves: Vec<(usize, String)>,
}

/// Which output channels move, given where each plays now (`None`: not routed, or unreadable;
/// such a channel is never touched). A routed channel moves when it plays to anything but the
/// target: its device went away, a device higher in the list came back, or the user put another
/// device first.
pub fn plan_outputs<'a>(list: &[PriorityDevice], endpoints: &'a [Endpoint], hands_free: &[String], routed: &[Option<String>]) -> OutputPlan<'a> {
    let target = choose(list, endpoints, Flow::Render, hands_free);
    let moves = match target {
        Some(target) => routed.iter().enumerate().filter_map(|(index, now)| now.as_ref().filter(|id| **id != target.id).map(|id| (index, id.clone()))).collect(),
        None => Vec::new(),
    };
    OutputPlan { target, moves }
}

/// The real microphone the daemon relays. Without the switch, exactly the one configured
/// (`configured`: an identifier or part of a name, as in platinum.ini), refused when it is missing,
/// disabled or a Bluetooth headset's. With it, the configured one while it is usable, otherwise
/// the first usable device of the input list. The error says why nothing can be relayed (in
/// English, for the daemon's log; `NO_MICROPHONE` when nothing is chosen anywhere).
pub fn microphone<'a>(configured: Option<&str>, list: &[PriorityDevice], auto_switch: bool, endpoints: &'a [Endpoint], hands_free: &[String]) -> Result<&'a Endpoint, String> {
    let inputs: Vec<&'a Endpoint> = endpoints.iter().filter(|e| e.flow == Flow::Capture).collect();
    let preferred = configured.map(|spec| configured_microphone(spec, &inputs, hands_free));
    match preferred {
        Some(Ok(endpoint)) => return Ok(endpoint),
        Some(Err(_)) | None if auto_switch => {
            if let Some(backup) = choose(list, endpoints, Flow::Capture, hands_free) {
                return Ok(backup);
            }
        }
        _ => {}
    }
    preferred.unwrap_or_else(|| Err(NO_MICROPHONE.to_string()))
}

/// Why no microphone is relayed when none is chosen.
pub const NO_MICROPHONE: &str = "no microphone chosen";

/// The microphone to relay on a first run, while none is chosen anywhere: the first of
/// `candidates` (Windows' default inputs, then the ones recorded before Platinum made the virtual
/// microphone the default) that is usable (a real input, plugged in, not a Bluetooth headset's
/// hands-free side). `None` rather than any other input: Windows' choice is the user's.
pub fn default_microphone<'a>(candidates: &[String], endpoints: &'a [Endpoint], hands_free: &[String]) -> Option<&'a Endpoint> {
    candidates.iter().find_map(|id| endpoints.iter().find(|e| e.id == *id && usable(e, Flow::Capture, hands_free)))
}

/// The configured microphone, as the daemon has always looked it up: by identifier or by part of
/// its name among the inputs, then checked.
fn configured_microphone<'a>(spec: &str, inputs: &[&'a Endpoint], hands_free: &[String]) -> Result<&'a Endpoint, String> {
    let owned: Vec<Endpoint> = inputs.iter().map(|e| (*e).clone()).collect();
    let found = crate::devices::find_device(&owned, spec)?;
    let endpoint = inputs.iter().copied().find(|e| e.id == found.id).ok_or_else(|| format!("no audio device matches \"{spec}\""))?;
    if endpoint.state != State::Active {
        return Err(format!("{} is {}", endpoint.name, endpoint.state.label()));
    }
    if hands_free.iter().any(|id| *id == endpoint.id) {
        return Err(format!("{} is the microphone of a Bluetooth headset: not opened", endpoint.name));
    }
    Ok(endpoint)
}

/// Puts a device at the top of a list (adding it when it is missing, and no longer excluded): the
/// user chose it by hand while the switch is on, and the switch must not take it away again.
/// Returns whether the list changed.
pub fn promote(list: &mut Vec<PriorityDevice>, id: &str, name: &str) -> bool {
    let before = list.clone();
    let mut entry = match list.iter().position(|e| e.id == id) {
        Some(index) => list.remove(index),
        None => PriorityDevice::new(id, name),
    };
    entry.excluded = false;
    if !name.is_empty() {
        entry.name = name.to_string();
    }
    list.insert(0, entry);
    *list != before
}

/// The outputs a "previous / next device" shortcut goes round (GG: "the ranking and the exclusions
/// serve the device change shortcuts"): with a priority list, its devices that are not excluded and
/// usable now, in the list's order; without one, every usable real output, as Windows lists them.
pub fn cycle<'a>(list: &[PriorityDevice], endpoints: &'a [Endpoint], hands_free: &[String]) -> Vec<&'a Endpoint> {
    if list.is_empty() {
        return endpoints.iter().filter(|e| usable(e, Flow::Render, hands_free)).collect();
    }
    list.iter().filter(|entry| !entry.excluded).filter_map(|entry| endpoints.iter().find(|e| e.id == entry.id && usable(e, Flow::Render, hands_free))).collect()
}

/// Brings a device to the head of a list by turning the entries that are not excluded round, their
/// order kept (excluded entries keep their places). With the switch on, the device a shortcut
/// chose must come first or the switch would take it away again; `promote` would do that too, but
/// the next press would then come back to the previous device instead of moving on. Returns
/// whether the list changed.
pub fn rotate_to(list: &mut [PriorityDevice], id: &str) -> bool {
    let places: Vec<usize> = list.iter().enumerate().filter(|(_, e)| !e.excluded).map(|(i, _)| i).collect();
    let Some(start) = places.iter().position(|&i| list[i].id == id).filter(|&start| start > 0) else { return false };
    let entries: Vec<PriorityDevice> = places.iter().map(|&i| list[i].clone()).collect();
    for (k, &place) in places.iter().enumerate() {
        list[place] = entries[(start + k) % entries.len()].clone();
    }
    true
}

/// Moves an entry up (`delta` < 0) or down; false when it cannot move.
pub fn move_entry(list: &mut [PriorityDevice], index: usize, delta: isize) -> bool {
    let Some(to) = index.checked_add_signed(delta).filter(|&to| to < list.len()) else { return false };
    if index >= list.len() || to == index {
        return false;
    }
    let entry = list[index].clone();
    if to < index {
        list[to..=index].rotate_right(1);
    } else {
        list[index..=to].rotate_left(1);
    }
    list[to] = entry;
    true
}

/// Marks an entry excluded or not; false when nothing changes.
pub fn set_excluded(list: &mut [PriorityDevice], index: usize, excluded: bool) -> bool {
    match list.get_mut(index) {
        Some(entry) if entry.excluded != excluded => {
            entry.excluded = excluded;
            true
        }
        _ => false,
    }
}

/// Removes an entry; false when there is none at `index`.
pub fn remove(list: &mut Vec<PriorityDevice>, index: usize) -> bool {
    if index < list.len() {
        list.remove(index);
        true
    } else {
        false
    }
}

/// Adds a device at the end of a list, unless it is there already.
pub fn add(list: &mut Vec<PriorityDevice>, id: &str, name: &str) -> bool {
    if id.is_empty() || list.iter().any(|e| e.id == id) {
        return false;
    }
    list.push(PriorityDevice::new(id, name));
    true
}

/// Gives each entry whose device is listed now the name it has now, so that the configuration
/// remembers the last name seen.
pub fn refresh_names(list: &mut [PriorityDevice], endpoints: &[Endpoint]) {
    for entry in list.iter_mut() {
        if let Some(endpoint) = endpoints.iter().find(|e| e.id == entry.id && !e.name.is_empty()) {
            entry.name.clone_from(&endpoint.name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(id: &str, flow: Flow, state: State) -> Endpoint {
        Endpoint { id: id.into(), name: format!("Device {id}"), flow, state, virtual_role: None }
    }

    fn entry(id: &str, excluded: bool) -> PriorityDevice {
        PriorityDevice { id: id.into(), name: format!("Device {id}"), excluded }
    }

    /// Arena 7 speakers, AirPods (with their hands-free side), a screen, a disabled device, a
    /// virtual channel, and the Seiren microphone with an Arena 7 one.
    fn machine() -> Vec<Endpoint> {
        let mut gaming = endpoint("gaming", Flow::Render, State::Active);
        gaming.virtual_role = Some(2);
        vec![
            endpoint("arena", Flow::Render, State::Active),
            endpoint("airpods", Flow::Render, State::Unplugged),
            endpoint("airpods-hf", Flow::Render, State::Active),
            endpoint("screen", Flow::Render, State::Active),
            endpoint("disabled", Flow::Render, State::Disabled),
            gaming,
            endpoint("seiren", Flow::Capture, State::Active),
            endpoint("arena-mic", Flow::Capture, State::Active),
            endpoint("airpods-mic", Flow::Capture, State::Active),
        ]
    }

    fn hands_free() -> Vec<String> {
        vec!["airpods-hf".into(), "airpods-mic".into()]
    }

    #[test]
    fn a_first_run_relays_windows_default_real_microphone() {
        let mut devices = machine();
        let mut virtual_mic = endpoint("virtual-mic", Flow::Capture, State::Active);
        virtual_mic.virtual_role = Some(3);
        devices.push(virtual_mic);
        let ids = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let pick = |candidates: &[&str]| default_microphone(&ids(candidates), &devices, &hands_free()).map(|e| e.id.as_str());
        // The virtual microphone, a headset's hands-free side, an output and a missing device are
        // passed over for the next candidate.
        assert_eq!(pick(&["virtual-mic", "airpods-mic", "arena", "gone", "arena-mic", "seiren"]), Some("arena-mic"));
        assert_eq!(pick(&["seiren"]), Some("seiren"));
        // Nothing usable among Windows' choices: nothing, not some other input.
        assert_eq!(pick(&["virtual-mic", "airpods-mic"]), None);
        assert_eq!(pick(&[]), None);
    }

    #[test]
    fn the_highest_usable_device_of_the_list_wins() {
        let devices = machine();
        let pick = |list: &[PriorityDevice]| choose(list, &devices, Flow::Render, &hands_free()).map(|e| e.id.as_str());
        // Unplugged, disabled, hands-free, virtual and missing devices are passed over.
        let list = [entry("airpods", false), entry("disabled", false), entry("airpods-hf", false), entry("gaming", false), entry("gone", false), entry("screen", false), entry("arena", false)];
        assert_eq!(pick(&list), Some("screen"));
        // An excluded device is never chosen, even first and plugged in.
        assert_eq!(pick(&[entry("arena", true), entry("screen", false)]), Some("screen"));
        assert_eq!(pick(&[entry("arena", true)]), None);
        assert_eq!(pick(&[]), None);
        // A device of the other direction does not count.
        assert_eq!(pick(&[entry("seiren", false), entry("arena", false)]), Some("arena"));
    }

    #[test]
    fn routed_channels_follow_the_list_and_the_others_stay() {
        let devices = machine();
        let list = [entry("airpods", false), entry("arena", false), entry("screen", false)];
        let routed = [Some("airpods".to_string()), Some("arena".to_string()), None, Some("screen".to_string())];
        let plan = plan_outputs(&list, &devices, &hands_free(), &routed);
        // The AirPods are gone: the Arena 7 take over; the unrouted channel is left alone.
        assert_eq!(plan.target.map(|e| e.id.as_str()), Some("arena"));
        assert_eq!(plan.moves, [(0, "airpods".to_string()), (3, "screen".to_string())]);
        // Nothing to do once every routed channel plays to the target.
        let settled = [Some("arena".to_string()), Some("arena".to_string()), None, Some("arena".to_string())];
        assert!(plan_outputs(&list, &devices, &hands_free(), &settled).moves.is_empty());
        // The AirPods come back: everything goes back to them.
        let mut back = devices.clone();
        back[1].state = State::Active;
        let plan = plan_outputs(&list, &back, &hands_free(), &settled);
        assert_eq!(plan.target.map(|e| e.id.as_str()), Some("airpods"));
        assert_eq!(plan.moves.len(), 3);
        // A channel on an excluded device is moved off it.
        let excluded = [entry("screen", true), entry("arena", false)];
        assert_eq!(plan_outputs(&excluded, &devices, &hands_free(), &[Some("screen".into())]).moves, [(0, "screen".to_string())]);
        // No device of the list usable: nobody moves, rather than losing the sound.
        let none = [entry("airpods", false), entry("disabled", false)];
        let plan = plan_outputs(&none, &devices, &hands_free(), &routed);
        assert!(plan.target.is_none() && plan.moves.is_empty());
    }

    #[test]
    fn the_chosen_microphone_comes_first_and_the_list_backs_it_up() {
        let mut devices = machine();
        let list = [entry("airpods-mic", false), entry("arena-mic", false)];
        let pick = |devices: &[Endpoint], configured: Option<&str>, auto: bool| microphone(configured, &list, auto, devices, &hands_free()).map(|e| e.id.clone());
        assert_eq!(pick(&devices, Some("Device seiren"), true), Ok("seiren".to_string()));
        assert_eq!(pick(&devices, Some("seiren"), false), Ok("seiren".to_string()));
        // Unplugged: the list's first usable device (the AirPods' microphone is hands-free).
        devices[6].state = State::Unplugged;
        assert_eq!(pick(&devices, Some("seiren"), true), Ok("arena-mic".to_string()));
        // Without the switch the daemon waits for it, as it always has.
        assert_eq!(pick(&devices, Some("seiren"), false), Err("Device seiren is unplugged".to_string()));
        // Nothing chosen: the list alone with the switch, nothing without.
        assert_eq!(pick(&devices, None, true), Ok("arena-mic".to_string()));
        assert_eq!(pick(&devices, None, false), Err(NO_MICROPHONE.to_string()));
        // A Bluetooth headset's microphone is refused, whatever the list says.
        assert!(pick(&devices, Some("airpods-mic"), false).unwrap_err().contains("Bluetooth"));
        // No backup usable either: the configured microphone's own problem is reported.
        devices[7].state = State::Disabled;
        assert_eq!(pick(&devices, Some("seiren"), true), Err("Device seiren is unplugged".to_string()));
    }

    #[test]
    fn lists_are_edited_in_place() {
        let mut list = vec![entry("a", false), entry("b", true), entry("c", false)];
        assert!(move_entry(&mut list, 2, -2));
        assert_eq!(list.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["c", "a", "b"]);
        assert!(move_entry(&mut list, 0, 1));
        assert_eq!(list.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["a", "c", "b"]);
        assert!(!move_entry(&mut list, 0, -1) && !move_entry(&mut list, 2, 1) && !move_entry(&mut list, 7, -1));
        assert!(set_excluded(&mut list, 0, true) && !set_excluded(&mut list, 0, true) && list[0].excluded);
        // Chosen by hand: first, and no longer excluded; a new device is added in front.
        assert!(promote(&mut list, "b", "B (renamed)"));
        assert_eq!((list[0].id.as_str(), list[0].name.as_str(), list[0].excluded), ("b", "B (renamed)", false));
        assert!(!promote(&mut list, "b", "B (renamed)"));
        assert!(promote(&mut list, "d", "D"));
        assert_eq!(list.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["d", "b", "a", "c"]);
        assert!(add(&mut list, "e", "E") && !add(&mut list, "e", "E") && !add(&mut list, "", "?"));
        assert!(remove(&mut list, 0) && !remove(&mut list, 9));
        assert_eq!(list.len(), 4);
        let mut named = vec![entry("arena", false), entry("gone", false)];
        named[1].name = "Old name".into();
        refresh_names(&mut named, &[Endpoint { name: "Haut-parleurs (Arena 7)".into(), ..endpoint("arena", Flow::Render, State::Active) }]);
        assert_eq!((named[0].name.as_str(), named[1].name.as_str()), ("Haut-parleurs (Arena 7)", "Old name"));
    }

    #[test]
    fn device_shortcuts_go_round_the_usable_devices() {
        let devices = machine();
        let ids = |list: &[PriorityDevice]| cycle(list, &devices, &hands_free()).into_iter().map(|e| e.id.as_str()).collect::<Vec<_>>();
        // Without a list: every usable real output, in Windows' order.
        assert_eq!(ids(&[]), ["arena", "screen"]);
        // With one: its order, without the excluded, unplugged or hands-free devices.
        assert_eq!(ids(&[entry("screen", false), entry("airpods", false), entry("airpods-hf", false), entry("arena", false)]), ["screen", "arena"]);
        assert_eq!(ids(&[entry("screen", true), entry("arena", false)]), ["arena"]);
        // Turning the list keeps its order round, and leaves the excluded entries in place.
        let mut list = vec![entry("a", false), entry("x", true), entry("b", false), entry("c", false)];
        assert!(rotate_to(&mut list, "c"));
        assert_eq!(list.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["c", "x", "a", "b"]);
        assert!(rotate_to(&mut list, "a"));
        assert_eq!(list.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["a", "x", "b", "c"]);
        assert!(!rotate_to(&mut list, "a") && !rotate_to(&mut list, "x") && !rotate_to(&mut list, "gone"));
    }
}
