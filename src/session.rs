//! Bringing the virtual channels up and down without the vendor's software.
//!
//! `start` enables the virtual audio devices and makes them the Windows defaults; `stop` puts the
//! previous defaults back and disables the devices again. The defaults that were in place before
//! `start` are kept in a small text file so that `stop` can restore them, even after a reboot.

use crate::devices::{Devices, Endpoint, Flow, Role, State};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Friendly-name prefix of the virtual audio devices created by the installed driver.
const VIRTUAL_PREFIX: &str = "SteelSeries Sonar - ";

/// Channel names accepted by `start`, with the virtual devices each one needs.
const CHANNELS: [(&str, &[(Flow, &str)]); 6] = [
    ("game", &[(Flow::Render, "gaming")]),
    ("chat", &[(Flow::Render, "chat")]),
    ("media", &[(Flow::Render, "media")]),
    ("aux", &[(Flow::Render, "aux")]),
    ("mic", &[(Flow::Capture, "microphone"), (Flow::Render, "microphone")]),
    ("stream", &[(Flow::Render, "stream"), (Flow::Capture, "stream")]),
];

/// Which channel a virtual device is ("gaming", "aux", ...): from the role the driver tags it
/// with, so that renaming the device changes nothing; from its original name when the tag is missing.
pub fn virtual_kind(endpoint: &Endpoint) -> Option<String> {
    if let Some(role) = endpoint.virtual_role {
        return role_kind(role).map(str::to_string);
    }
    let rest = endpoint.name.strip_prefix(VIRTUAL_PREFIX)?;
    Some(rest.split(" (").next().unwrap_or(rest).trim().to_ascii_lowercase())
}

/// The channel behind each role of the virtual audio driver (its INF: Chat 1, Game 2, ChatCapture 3,
/// Stream 4, Media 5, Aux 6).
pub fn role_kind(role: u32) -> Option<&'static str> {
    Some(match role {
        1 => "chat",
        2 => "gaming",
        3 => "microphone",
        4 => "stream",
        5 => "media",
        6 => "aux",
        _ => return None,
    })
}

/// The output device of a channel given by its user-facing name (game, chat, media, aux).
pub fn channel_output<'a>(endpoints: &'a [Endpoint], channel: &str) -> Option<&'a Endpoint> {
    let (_, parts) = CHANNELS.iter().find(|(name, _)| *name == channel)?;
    let (flow, kind) = parts.first()?;
    if *flow == Flow::Render { find(endpoints, Flow::Render, kind) } else { None }
}

/// The playback side of the virtual microphone: what is played into it is what applications record
/// from the virtual microphone.
pub fn microphone_feed(endpoints: &[Endpoint]) -> Option<&Endpoint> {
    find(endpoints, Flow::Render, "microphone")
}

fn state_file() -> Result<PathBuf, String> {
    let base = std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is not set")?;
    Ok(PathBuf::from(base).join("Platinum").join("previous-defaults.txt"))
}

fn find<'a>(endpoints: &'a [Endpoint], flow: Flow, kind: &str) -> Option<&'a Endpoint> {
    endpoints.iter().find(|e| e.flow == flow && virtual_kind(e).as_deref() == Some(kind))
}

fn text<T>(result: crate::com::Result<T>) -> Result<T, String> {
    result.map_err(|e| e.to_string())
}

pub fn start(devices: &Devices, requested: &[&str]) -> Result<(), String> {
    let selected: Vec<&(&str, &[(Flow, &str)])> = if requested.is_empty() {
        CHANNELS.iter().collect()
    } else {
        requested
            .iter()
            .map(|name| CHANNELS.iter().find(|(channel, _)| channel == name).ok_or_else(|| format!("unknown channel \"{name}\" (expected game, chat, media, aux, mic or stream)")))
            .collect::<Result<_, _>>()?
    };

    let endpoints = text(devices.list())?;
    if !endpoints.iter().any(|e| virtual_kind(e).is_some()) {
        return Err("no virtual audio device found: the audio driver does not seem to be installed".into());
    }

    save_previous_defaults(devices, &endpoints)?;

    let mut wanted: Vec<&Endpoint> = Vec::new();
    for (channel, parts) in &selected {
        for (flow, kind) in parts.iter() {
            match find(&endpoints, *flow, kind) {
                Some(endpoint) => wanted.push(endpoint),
                None => println!("channel {channel}: no {} device \"{kind}\" on this machine, skipped", flow.label()),
            }
        }
    }
    for endpoint in &wanted {
        if endpoint.state == State::Disabled {
            text(devices.set_enabled(&endpoint.id, true))?;
            println!("enabled  {:<7} {}", endpoint.flow.label(), endpoint.name);
        }
    }
    wait_until_active(devices, &wanted)?;

    let has = |name: &str| selected.iter().any(|(channel, _)| *channel == name);
    let mut defaults: Vec<(Flow, &str, &[Role])> = Vec::new();
    if has("game") {
        defaults.push((Flow::Render, "gaming", &[Role::Console, Role::Multimedia]));
    }
    if has("chat") {
        defaults.push((Flow::Render, "chat", &[Role::Communications]));
    }
    if has("mic") {
        defaults.push((Flow::Capture, "microphone", &Role::ALL));
    }
    for (flow, kind, roles) in defaults {
        if let Some(endpoint) = find(&endpoints, flow, kind) {
            for role in roles {
                text(devices.set_default(&endpoint.id, *role))?;
            }
            let roles: Vec<&str> = roles.iter().map(|r| r.label()).collect();
            println!("default  {:<7} {}  ({})", flow.label(), endpoint.name, roles.join(", "));
        }
    }
    Ok(())
}

/// The default devices recorded by `start` before it made the virtual ones the defaults, as
/// (direction, role, identifier); empty without a record.
pub fn previous_defaults() -> Vec<(Flow, Role, String)> {
    let saved = state_file().ok().and_then(|file| std::fs::read_to_string(file).ok()).unwrap_or_default();
    parse_previous_defaults(&saved)
}

fn parse_previous_defaults(saved: &str) -> Vec<(Flow, Role, String)> {
    saved
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, ' ');
            Some((fields.next().and_then(Flow::parse)?, fields.next().and_then(Role::parse)?, fields.next()?.trim().to_string()))
        })
        .collect()
}

pub fn stop(devices: &Devices) -> Result<(), String> {
    let endpoints = text(devices.list())?;
    let file = state_file()?;
    if let Ok(saved) = std::fs::read_to_string(&file) {
        let sinks = streaming_sinks();
        for line in saved.lines() {
            let mut fields = line.splitn(3, ' ');
            let (Some(flow), Some(role), Some(id)) = (fields.next().and_then(Flow::parse), fields.next().and_then(Role::parse), fields.next()) else {
                continue;
            };
            // A record written before streaming sinks were left out may still name one.
            if is_streaming_sink(&endpoints, id, &sinks) {
                println!("previous {} default for {} was a streaming output, left to Windows", flow.label(), role.label());
                continue;
            }
            match endpoints.iter().find(|e| e.id == id) {
                Some(endpoint) if endpoint.state == State::Active => {
                    text(devices.set_default(id, role))?;
                    println!("default  {:<7} {}  ({})", flow.label(), endpoint.name, role.label());
                }
                _ => println!("previous {} default for {} is no longer available, left to Windows", flow.label(), role.label()),
            }
        }
        std::fs::remove_file(&file).map_err(|e| format!("cannot remove {}: {e}", file.display()))?;
    } else {
        println!("no saved defaults: Windows will choose the default devices");
    }
    for endpoint in endpoints.iter().filter(|e| virtual_kind(e).is_some() && e.state == State::Active) {
        text(devices.set_enabled(&endpoint.id, false))?;
        println!("disabled {:<7} {}", endpoint.flow.label(), endpoint.name);
    }
    Ok(())
}

/// Disables one channel's virtual devices, leaving the others and the defaults alone: the daemon
/// does it for the Stream device once streamer mode is left.
pub fn disable(devices: &Devices, channel: &str) -> Result<(), String> {
    let (_, parts) = CHANNELS.iter().find(|(name, _)| *name == channel).ok_or_else(|| format!("unknown channel \"{channel}\""))?;
    let endpoints = text(devices.list())?;
    for (flow, kind) in parts.iter() {
        if let Some(endpoint) = find(&endpoints, *flow, kind).filter(|e| e.state == State::Active) {
            text(devices.set_enabled(&endpoint.id, false))?;
            println!("disabled {:<7} {}", endpoint.flow.label(), endpoint.name);
        }
    }
    Ok(())
}

/// The remote-streaming sinks named in the configuration ("Steam Streaming Speakers"): a default
/// a streaming host imposed for its session, never one to record or put back.
fn streaming_sinks() -> Vec<String> {
    crate::config::load().streaming_sinks
}

/// Whether the endpoint `id` is one of the streaming sinks.
fn is_streaming_sink(endpoints: &[Endpoint], id: &str, sinks: &[String]) -> bool {
    endpoints.iter().any(|e| e.id == id && e.flow == Flow::Render && crate::remote::is_sink_name(&e.name, sinks))
}

/// Records the current default devices, unless a record already exists or a default is itself
/// virtual or a streaming sink. A sink recorded during a Sunshine session was put back at every
/// stop, and the next start then took it for a session (seen on 17/11/2026).
fn save_previous_defaults(devices: &Devices, endpoints: &[Endpoint]) -> Result<(), String> {
    let file = state_file()?;
    if file.exists() {
        return Ok(());
    }
    let sinks = streaming_sinks();
    let mut lines = String::new();
    for flow in Flow::ALL {
        for role in Role::ALL {
            if let Some(id) = text(devices.default(flow, role))? {
                let is_virtual = endpoints.iter().any(|e| e.id == id && virtual_kind(e).is_some());
                if !is_virtual && !is_streaming_sink(endpoints, &id, &sinks) {
                    lines.push_str(&format!("{} {} {id}\n", flow.label(), role.label()));
                }
            }
        }
    }
    if let Some(folder) = file.parent() {
        std::fs::create_dir_all(folder).map_err(|e| format!("cannot create {}: {e}", folder.display()))?;
    }
    std::fs::write(&file, lines).map_err(|e| format!("cannot write {}: {e}", file.display()))
}

/// Enabling a device is asynchronous; give Windows a few seconds to bring the devices up.
fn wait_until_active(devices: &Devices, wanted: &[&Endpoint]) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let now = text(devices.list())?;
        let pending: Vec<&str> = wanted.iter().filter(|w| !now.iter().any(|e| e.id == w.id && e.state == State::Active)).map(|w| w.name.as_str()).collect();
        if pending.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("still not active after 5 s: {}", pending.join(", ")));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_record_of_previous_defaults_reads_back() {
        let saved = "render console {0.0.0.00000000}.{a}\ncapture communications {0.0.1.00000000}.{b c}\nnonsense line\ncapture \n";
        let parsed = parse_previous_defaults(saved);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1], (Flow::Capture, Role::Communications, "{0.0.1.00000000}.{b c}".to_string()));
    }
}
