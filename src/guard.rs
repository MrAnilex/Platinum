//! Checks that an experiment left the user's audio alone: the settings store is unchanged, and no
//! audio stream was opened behind our back. Also measures what the experiment cost in memory.

use crate::apo::{Apo, Channel};
use crate::devices::{Devices, State};
use std::ffi::c_void;

/// One setting's identity and value, as text.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub channel: Channel,
    pub id: i32,
    pub name: String,
    pub value: String,
}

/// Every setting of every channel.
pub fn snapshot(apo: &Apo) -> Result<Vec<Entry>, String> {
    let text = |e: crate::com::Error| e.to_string();
    let mut entries = Vec::new();
    for channel in Channel::ALL {
        let store = apo.open(channel).map_err(text)?;
        for index in 0..store.len().map_err(text)? {
            let setting = store.setting_at(index).map_err(text)?;
            let value = setting.value().map(|v| v.to_string()).unwrap_or_else(|e| format!("(unreadable: {e})"));
            entries.push(Entry { channel, id: setting.id().map_err(text)?, name: setting.name().map_err(text)?, value });
        }
    }
    Ok(entries)
}

/// The settings whose value differs between two snapshots, as (before, after).
pub fn changes<'a>(before: &'a [Entry], after: &'a [Entry]) -> Vec<(&'a Entry, Option<&'a Entry>)> {
    before
        .iter()
        .filter_map(|old| {
            let new = after.iter().find(|e| e.channel == old.channel && e.id == old.id);
            match new {
                Some(new) if new.value == old.value => None,
                other => Some((old, other)),
            }
        })
        .collect()
}

/// Names of the active endpoints on which this process has a running audio stream.
pub fn own_streams(devices: &Devices) -> Result<Vec<String>, String> {
    let pid = std::process::id();
    let mut found = Vec::new();
    for endpoint in devices.list().map_err(|e| e.to_string())?.iter().filter(|e| e.state == State::Active) {
        // An endpoint whose sessions cannot be listed is skipped: this is a report, not a gate.
        let Ok(sessions) = devices.sessions(&endpoint.id) else { continue };
        if sessions.iter().any(|s| s.pid == pid && s.active) {
            found.push(format!("{} {}", endpoint.flow.label(), endpoint.name));
        }
    }
    Ok(found)
}

/// PROCESS_MEMORY_COUNTERS_EX.
#[repr(C)]
#[derive(Default)]
struct MemoryCounters {
    size: u32,
    page_faults: u32,
    peak_working_set: usize,
    working_set: usize,
    quota_peak_paged_pool: usize,
    quota_paged_pool: usize,
    quota_peak_non_paged_pool: usize,
    quota_non_paged_pool: usize,
    pagefile: usize,
    peak_pagefile: usize,
    private: usize,
}

/// SYSTEMTIME.
#[repr(C)]
#[derive(Default)]
struct SystemTime {
    year: u16,
    month: u16,
    day_of_week: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    milliseconds: u16,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut c_void;
    fn K32GetProcessMemoryInfo(process: *mut c_void, counters: *mut MemoryCounters, size: u32) -> i32;
    fn GetProcessTimes(process: *mut c_void, creation: *mut u64, exit: *mut u64, kernel: *mut u64, user: *mut u64) -> i32;
    fn GetLocalTime(time: *mut SystemTime);
    fn GetProcessHandleCount(process: *mut c_void, count: *mut u32) -> i32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

/// How many kernel handles this process holds open: the vendor's settings DLL leaks some on every
/// opening of a store, so a count that keeps growing points at it.
pub fn handle_count() -> u32 {
    let mut count = 0u32;
    let ok = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) };
    if ok == 0 { 0 } else { count }
}

/// Processor time this process has used so far (all threads, user and kernel), in seconds.
pub fn cpu_seconds() -> f64 {
    let (mut creation, mut exit, mut kernel, mut user) = (0u64, 0u64, 0u64, 0u64);
    let ok = unsafe { GetProcessTimes(GetCurrentProcess(), &mut creation, &mut exit, &mut kernel, &mut user) };
    // FILETIME counts are in units of 100 nanoseconds.
    if ok == 0 { 0.0 } else { (kernel + user) as f64 / 10_000_000.0 }
}

/// Local date and time as "YYYYMMDD-HHMMSS", for file names.
pub fn stamp() -> String {
    let mut time = SystemTime::default();
    unsafe { GetLocalTime(&mut time) };
    format!("{:04}{:02}{:02}-{:02}{:02}{:02}", time.year, time.month, time.day, time.hour, time.minute, time.second)
}

/// Private memory and working set of this process, in bytes.
pub fn memory() -> (usize, usize) {
    let mut counters = MemoryCounters { size: std::mem::size_of::<MemoryCounters>() as u32, ..Default::default() };
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.size) };
    if ok == 0 { (0, 0) } else { (counters.private, counters.working_set) }
}

const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

/// Private memory and working set of another process, in bytes (the window shows the daemon's).
/// Opened with the least access that can read them, which a process of the same user grants;
/// `None` when it cannot be opened or read.
pub fn process_memory(pid: u32) -> Option<(usize, usize)> {
    if pid == 0 {
        return None;
    }
    let mut counters = MemoryCounters { size: std::mem::size_of::<MemoryCounters>() as u32, ..Default::default() };
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return None;
        }
        let ok = K32GetProcessMemoryInfo(process, &mut counters, counters.size);
        CloseHandle(process);
        (ok != 0).then_some((counters.private, counters.working_set))
    }
}

pub fn megabytes(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// A registry branch as logical lines ("[key]" alone, or "[key] name=value"), exported with the
/// standard `reg export` tool into `file`. Reading only.
pub fn registry_snapshot(branch: &str, file: &std::path::Path) -> Result<Vec<String>, String> {
    if let Some(folder) = file.parent() {
        std::fs::create_dir_all(folder).map_err(|e| format!("cannot create {}: {e}", folder.display()))?;
    }
    let output = std::process::Command::new("reg")
        .arg("export")
        .arg(branch)
        .arg(file)
        .arg("/y")
        .output()
        .map_err(|e| format!("cannot run reg export: {e}"))?;
    if !output.status.success() {
        return Err(format!("reg export {branch} failed: {}", String::from_utf8_lossy(&output.stderr).trim()));
    }
    let bytes = std::fs::read(file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    Ok(parse_registry_export(&bytes))
}

/// Logical lines of a .reg file (UTF-16 with a byte-order mark, values continued with a final '\').
fn parse_registry_export(bytes: &[u8]) -> Vec<String> {
    let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let text = String::from_utf16_lossy(units.strip_prefix(&[0xFEFF]).unwrap_or(&units));
    let (mut entries, mut key, mut pending) = (Vec::new(), String::new(), String::new());
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if pending.is_empty() {
            pending.push_str(line);
        } else {
            pending.push_str(line.trim_start());
        }
        if pending.ends_with('\\') {
            pending.pop();
            continue;
        }
        let logical = std::mem::take(&mut pending);
        if logical.starts_with('[') {
            key = logical.clone();
            entries.push(logical);
        } else if !logical.is_empty() {
            entries.push(format!("{key} {logical}"));
        }
    }
    entries
}

/// Lines present in only one of two registry snapshots: (added, removed).
pub fn registry_changes<'a>(before: &'a [String], after: &'a [String]) -> (Vec<&'a String>, Vec<&'a String>) {
    let old: std::collections::HashSet<&String> = before.iter().collect();
    let new: std::collections::HashSet<&String> = after.iter().collect();
    (after.iter().filter(|line| !old.contains(line)).collect(), before.iter().filter(|line| !new.contains(line)).collect())
}

/// A logical .reg line split into its key and, for a value line, the value's name and data.
fn split_registry_line(line: &str) -> Option<(&str, Option<(&str, &str)>)> {
    let key_end = line.find(']')?;
    let key = line.get(1..key_end)?;
    let rest = line[key_end + 1..].trim_start();
    if rest.is_empty() {
        return Some((key, None));
    }
    if let Some(data) = rest.strip_prefix("@=") {
        return Some((key, Some(("", data))));
    }
    let name_end = rest.find("\"=")?;
    Some((key, Some((rest.get(1..name_end)?, &rest[name_end + 2..]))))
}

/// The key in the short form `reg` accepts on its command line.
fn short_key(key: &str) -> String {
    key.replacen("HKEY_LOCAL_MACHINE", "HKLM", 1).replacen("HKEY_CURRENT_USER", "HKCU", 1)
}

fn value_switch(name: &str) -> String {
    if name.is_empty() { "/ve".to_string() } else { format!("/v \"{name}\"") }
}

/// Commands that undo what changed between two registry snapshots, for the user to run on purpose.
/// Keys that appeared are deleted (only the topmost new key of each new branch), values that
/// appeared are deleted, and changed DWORD or string values are set back. Anything else is listed
/// for restoring by hand.
pub fn undo_commands(before: &[String], added: &[&String], removed: &[&String]) -> Vec<String> {
    let old_keys: std::collections::HashSet<&str> = before.iter().filter_map(|l| split_registry_line(l)).filter(|(_, v)| v.is_none()).map(|(k, _)| k).collect();
    let new_keys: Vec<&str> = added.iter().filter_map(|l| split_registry_line(l)).filter(|(k, v)| v.is_none() && !old_keys.contains(k)).map(|(k, _)| k).collect();
    let mut commands = Vec::new();
    for key in &new_keys {
        let inside_another = new_keys.iter().any(|other| other != key && key.starts_with(&format!("{other}\\")));
        if !inside_another {
            commands.push(format!("reg delete \"{}\" /f", short_key(key)));
        }
    }
    let old_values: Vec<(&str, &str, &str)> = removed.iter().filter_map(|l| split_registry_line(l)).filter_map(|(k, v)| v.map(|(n, d)| (k, n, d))).collect();
    for line in added {
        let Some((key, Some((name, _)))) = split_registry_line(line) else { continue };
        if new_keys.iter().any(|new| key == *new || key.starts_with(&format!("{new}\\"))) {
            continue; // deleted with its key
        }
        match old_values.iter().find(|(k, n, _)| *k == key && *n == name) {
            None => commands.push(format!("reg delete \"{}\" {} /f", short_key(key), value_switch(name))),
            Some((_, _, data)) => {
                if let Some(hex) = data.strip_prefix("dword:") {
                    commands.push(format!("reg add \"{}\" {} /t REG_DWORD /d 0x{hex} /f", short_key(key), value_switch(name)));
                } else if data.starts_with('"') && data.ends_with('"') && !data[1..data.len() - 1].contains('\\') {
                    commands.push(format!("reg add \"{}\" {} /t REG_SZ /d {data} /f", short_key(key), value_switch(name)));
                } else {
                    commands.push(format!("(restore by hand) [{key}] {}={data}", if name.is_empty() { "@".to_string() } else { format!("\"{name}\"") }));
                }
            }
        }
    }
    for line in removed {
        let Some((key, value)) = split_registry_line(line) else { continue };
        let still_there = added.iter().filter_map(|l| split_registry_line(l)).any(|(k, v)| k == key && v.map(|(n, _)| n) == value.map(|(n, _)| n));
        if !still_there && !key.is_empty() {
            commands.push(format!("(restore by hand, it disappeared) {line}"));
        }
    }
    commands
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(text: &str) -> Vec<u8> {
        std::iter::once(0xFEFFu16).chain(text.encode_utf16()).flat_map(|u| u.to_le_bytes()).collect()
    }

    #[test]
    fn registry_export_lines_carry_their_key_and_join_continuations() {
        let export = "Windows Registry Editor Version 5.00\r\n\r\n[HKEY_LOCAL_MACHINE\\SOFTWARE\\A]\r\n\"x\"=dword:00000001\r\n\"blob\"=hex:01,02,\\\r\n  03,04\r\n\r\n[HKEY_LOCAL_MACHINE\\SOFTWARE\\A\\B]\r\n@=\"v\"\r\n";
        let lines = parse_registry_export(&utf16(export));
        assert_eq!(
            lines,
            vec![
                " Windows Registry Editor Version 5.00".to_string(),
                "[HKEY_LOCAL_MACHINE\\SOFTWARE\\A]".to_string(),
                "[HKEY_LOCAL_MACHINE\\SOFTWARE\\A] \"x\"=dword:00000001".to_string(),
                "[HKEY_LOCAL_MACHINE\\SOFTWARE\\A] \"blob\"=hex:01,02,03,04".to_string(),
                "[HKEY_LOCAL_MACHINE\\SOFTWARE\\A\\B]".to_string(),
                "[HKEY_LOCAL_MACHINE\\SOFTWARE\\A\\B] @=\"v\"".to_string(),
            ]
        );
        let after: Vec<String> = lines.iter().map(|l| l.replace("00000001", "00000002")).chain(std::iter::once("[HKEY_LOCAL_MACHINE\\SOFTWARE\\A\\C]".to_string())).collect();
        let (added, removed) = registry_changes(&lines, &after);
        assert_eq!(added.len(), 2);
        assert_eq!(removed, vec![&lines[2]]);
    }

    #[test]
    fn undo_commands_reverse_what_was_added_or_changed() {
        let root = "[HKEY_LOCAL_MACHINE\\SOFTWARE\\S\\Apps]";
        let before: Vec<String> = vec![root.into(), format!("{root} \"Count\"=dword:00000002"), format!("{root} \"Name\"=\"old\"")];
        let after: Vec<String> = vec![
            root.into(),
            format!("{root} \"Count\"=dword:00000003"),
            format!("{root} \"Name\"=\"new\""),
            format!("{root} \"Extra\"=dword:00000001"),
            "[HKEY_LOCAL_MACHINE\\SOFTWARE\\S\\Apps\\platinum.exe]".into(),
            "[HKEY_LOCAL_MACHINE\\SOFTWARE\\S\\Apps\\platinum.exe] \"AppId\"=\"{1}\"".into(),
            "[HKEY_LOCAL_MACHINE\\SOFTWARE\\S\\Apps\\platinum.exe\\Store]".into(),
        ];
        let (added, removed) = registry_changes(&before, &after);
        let commands = undo_commands(&before, &added, &removed);
        assert_eq!(
            commands,
            vec![
                "reg delete \"HKLM\\SOFTWARE\\S\\Apps\\platinum.exe\" /f".to_string(),
                "reg add \"HKLM\\SOFTWARE\\S\\Apps\" /v \"Count\" /t REG_DWORD /d 0x00000002 /f".to_string(),
                "reg add \"HKLM\\SOFTWARE\\S\\Apps\" /v \"Name\" /t REG_SZ /d \"old\" /f".to_string(),
                "reg delete \"HKLM\\SOFTWARE\\S\\Apps\" /v \"Extra\" /f".to_string(),
            ]
        );
    }
}
