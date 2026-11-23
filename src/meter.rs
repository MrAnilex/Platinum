//! Sound levels on audio devices, to check by measurement that audio flows where it should.
//!
//! An output is read through the peak meter Windows keeps for it, which opens nothing. A microphone
//! has to be opened to be measured: a shared-mode capture stream is started on it, the samples are
//! only used to compute a level and are then discarded.

use crate::com;
use crate::devices::{Devices, Endpoint, Flow};
use crate::dsp::peak;
use crate::wasapi::{AudioClient, CONVERT, CaptureClient, Meter};
use std::time::{Duration, Instant};

enum Probe {
    Output(Meter),
    Input { client: AudioClient, capture: CaptureClient, channels: usize },
}

impl Probe {
    fn open(devices: &Devices, endpoint: &Endpoint) -> com::Result<Probe> {
        if endpoint.flow == Flow::Render {
            return Ok(Probe::Output(Meter::activate(devices, &endpoint.id)?));
        }
        let client = AudioClient::activate(devices, &endpoint.id)?;
        let mix = client.mix_format(devices.ole())?;
        client.initialize(CONVERT, 250, &mix.as_float_at(mix.rate))?;
        let capture = client.capture_client()?;
        client.start()?;
        Ok(Probe::Input { client, capture, channels: usize::from(mix.channels) })
    }

    /// Peak level since the previous reading; `None` when a microphone delivered nothing new.
    fn read(&self) -> com::Result<Option<f32>> {
        match self {
            Probe::Output(meter) => meter.peak().map(Some),
            Probe::Input { capture, channels, .. } => {
                let mut level = None;
                while capture.read(*channels, |samples, _| level = Some(samples.map_or(0.0, peak).max(level.unwrap_or(0.0))))? {}
                Ok(level)
            }
        }
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        if let Probe::Input { client, .. } = self {
            let _ = client.stop();
        }
    }
}

#[derive(Default, Clone, Copy)]
struct Tally {
    readings: u32,
    sum: f64,
    highest: f32,
}

impl Tally {
    fn add(&mut self, level: f32) {
        self.readings += 1;
        self.sum += f64::from(level);
        self.highest = self.highest.max(level);
    }

    fn mean(&self) -> f64 {
        self.sum / f64::from(self.readings.max(1))
    }
}

/// Prints the level of each device once a second for `seconds`, then the mean over the whole time.
pub fn run(devices: &Devices, endpoints: &[&Endpoint], seconds: f64) -> Result<(), String> {
    let probes: Vec<Probe> = endpoints.iter().map(|endpoint| Probe::open(devices, endpoint).map_err(|e| format!("{}: {e}", endpoint.name))).collect::<Result<_, _>>()?;
    let started = Instant::now();
    let mut totals = vec![Tally::default(); probes.len()];
    let mut second = vec![Tally::default(); probes.len()];
    let mut next_line = 1.0;
    loop {
        std::thread::sleep(Duration::from_millis(20));
        for (index, probe) in probes.iter().enumerate() {
            if let Some(level) = probe.read().map_err(|e| format!("{}: {e}", endpoints[index].name))? {
                totals[index].add(level);
                second[index].add(level);
            }
        }
        let elapsed = started.elapsed().as_secs_f64();
        if elapsed >= next_line {
            let levels: Vec<String> = second.iter().map(|tally| format!("{:.4}", tally.mean())).collect();
            println!("[{next_line:>3.0} s] {}", levels.join("  "));
            second.fill(Tally::default());
            next_line += 1.0;
        }
        if elapsed >= seconds {
            break;
        }
    }
    for (endpoint, tally) in endpoints.iter().zip(&totals) {
        println!("{:<7} {}: mean level {:.4}, highest {:.4} ({} readings)", endpoint.flow.label(), endpoint.name, tally.mean(), tally.highest, tally.readings);
    }
    Ok(())
}
