//! The level meters: about twenty times a second, the peak level of the real output (Master), then
//! of each channel's virtual device, then of the stream mix's device in streamer mode. Reading a
//! meter opens no stream.
//!
//! A meter that cannot be read (its device is disabled, unplugged, or does not exist yet) is
//! looked for again every few seconds, on its own: the channels come and go with the daemon, and
//! each one must come back as soon as its device does, whatever the others do.

use crate::engine::{CHANNELS, MeterDevices};
use platinum::devices::{Devices, Flow, State};
use platinum::session;
use platinum::wasapi::Meter;
use std::time::{Duration, Instant};

/// The place of the stream mix's meter among the levels: after the Master and the channels.
pub const STREAM_METER: usize = CHANNELS.len() + 1;

/// How often a missing meter is looked for again.
const RETRY: Duration = Duration::from_secs(3);

/// One meter and the device it reads.
struct Slot {
    id: String,
    meter: Meter,
}

/// Runs for the life of the window. `chosen` holds the real output the channels play to and the
/// stream mix's device (set by the engine); `publish` receives one level per meter: Master, then
/// the channels in `CHANNELS` order, then the stream mix (`STREAM_METER`).
pub fn run(chosen: MeterDevices, publish: impl Fn(Vec<f32>)) {
    let mut devices: Option<Devices> = None;
    let mut slots: Vec<Option<Slot>> = (0..=STREAM_METER).map(|_| None).collect();
    let mut last_search = Instant::now() - RETRY;
    loop {
        let master = chosen.master.lock().ok().and_then(|m| m.clone());
        let stream = chosen.stream.lock().ok().and_then(|m| m.clone());
        for (slot, wanted) in [(0, &master), (STREAM_METER, &stream)] {
            if slots[slot].as_ref().map(|s| &s.id) != wanted.as_ref() {
                // Another device was chosen: its meter is looked for now, not in a few seconds.
                if slots[slot].is_some() {
                    last_search = Instant::now() - RETRY;
                }
                slots[slot] = None;
            }
        }
        // The stream mix's meter is only wanted in streamer mode (a device chosen for it).
        let missing = slots.iter().enumerate().any(|(index, slot)| slot.is_none() && (index != STREAM_METER || stream.is_some()));
        if missing && last_search.elapsed() >= RETRY {
            last_search = Instant::now();
            search(&mut devices, &mut slots, master.as_deref(), stream.as_deref());
        }
        let mut levels = vec![0.0; slots.len()];
        let mut failures = 0;
        for (level, slot) in levels.iter_mut().zip(slots.iter_mut()) {
            let Some(open) = slot else { continue };
            match open.meter.peak() {
                Ok(peak) => *level = shown_level(peak),
                Err(_) => {
                    *slot = None;
                    failures += 1;
                }
            }
        }
        // Every meter failing at once: Windows' audio service probably restarted, and the device
        // API with it.
        if failures > 0 && slots.iter().all(Option::is_none) {
            devices = None;
        }
        publish(levels);
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Opens the meters that are missing, reopening the device API if needed.
fn search(devices: &mut Option<Devices>, slots: &mut [Option<Slot>], master: Option<&str>, stream: Option<&str>) {
    if devices.is_none() {
        *devices = Devices::open().ok();
    }
    let Some(api) = devices.as_ref() else { return };
    let Ok(endpoints) = api.list() else {
        *devices = None;
        return;
    };
    let mut wanted: Vec<Option<String>> = vec![master.map(str::to_string)];
    wanted.extend(CHANNELS.iter().map(|def| {
        endpoints
            .iter()
            .find(|e| e.flow == Flow::Render && e.state == State::Active && session::virtual_kind(e).as_deref() == Some(def.meter))
            .map(|e| e.id.clone())
    }));
    wanted.push(stream.map(str::to_string));
    for (slot, id) in slots.iter_mut().zip(wanted) {
        if slot.is_some() {
            continue;
        }
        let Some(id) = id else { continue };
        if let Ok(meter) = Meter::activate(api, &id) {
            *slot = Some(Slot { id, meter });
        }
    }
}

/// A peak level (0..1, linear) as the height of a meter: -60 dB at the bottom, 0 dB at the top.
pub fn shown_level(peak: f32) -> f32 {
    if peak <= 0.001 { 0.0 } else { ((20.0 * peak.log10() + 60.0) / 60.0).clamp(0.0, 1.0) }
}

#[cfg(test)]
mod tests {
    use super::shown_level;

    #[test]
    fn levels_are_shown_in_decibels() {
        assert_eq!(shown_level(0.0), 0.0);
        assert_eq!(shown_level(1.0), 1.0);
        assert!((shown_level(0.1) - 2.0 / 3.0).abs() < 1e-5);
        assert_eq!(shown_level(4.0), 1.0);
    }
}
