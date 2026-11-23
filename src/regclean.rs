//! Registry hygiene: the "stream added" and "stream removed" events the effects engine leaves.
//!
//! Every time the vendor's microphone effects engine starts or stops, it writes a value under
//! `HKLM\SOFTWARE\SteelSeries ApS\Sonar.APO\ChatCapture\Settings\NotificationClients\<client>\
//! StreamAdded` (or `StreamRemoved`), for GG's service to read and delete. Without that service
//! nobody does: one offline run of the engine left 8 of them (17/11/2026), and the daemon starts a
//! new engine about every hour and a half (`worker`, `recycle`). So at its start and after each
//! replacement of the microphone worker, the daemon deletes those values, only those, and never a
//! key, and only while GG's Sonar is not running (it would be the one to read them).
//!
//! The choice of what to delete is a pure function (`selection`), tested; the registry part around
//! it only lists and deletes.

use crate::com::wide;

/// The engine's notification clients, under HKLM.
pub const NOTIFICATION_CLIENTS: &str = r"SOFTWARE\SteelSeries ApS\Sonar.APO\ChatCapture\Settings\NotificationClients";
/// The event keys whose values are deleted; the other events (settings changed, profiles…) are
/// left alone.
pub const STREAM_EVENTS: [&str; 2] = ["StreamAdded", "StreamRemoved"];

/// One event key of one notification client, with the names of its values.
#[derive(Clone, Debug, PartialEq)]
pub struct EventKey {
    pub client: String,
    pub event: String,
    pub values: Vec<String>,
}

/// Whether an event key is one of the stream events.
pub fn is_stream_event(event: &str) -> bool {
    STREAM_EVENTS.iter().any(|known| known.eq_ignore_ascii_case(event))
}

/// The values to delete, as (key under HKLM, value name): every named value of the clients'
/// stream event keys. Never a key, never a key's default value, never another event's value.
pub fn selection(keys: &[EventKey]) -> Vec<(String, String)> {
    keys.iter()
        .filter(|key| is_stream_event(&key.event) && !key.client.is_empty() && !key.client.contains('\\'))
        .flat_map(|key| key.values.iter().filter(|name| !name.is_empty()).map(move |name| (format!(r"{NOTIFICATION_CLIENTS}\{}\{}", key.client, key.event), name.clone())))
        .collect()
}

/// What a cleanup did.
#[derive(Clone, Debug, PartialEq)]
pub enum Cleaned {
    /// This many values deleted, and why some could not be, if any could not.
    Removed { count: usize, problem: Option<String> },
    /// GG's Sonar runs: its service reads those events, they are its own.
    GgRunning,
}

/// Deletes the stream events' values, unless GG's Sonar runs. The error says why they could not
/// be listed.
pub fn clean() -> Result<Cleaned, String> {
    if crate::daemon::sonar_running() {
        return Ok(Cleaned::GgRunning);
    }
    let selected = selection(&read_event_keys()?);
    let mut count = 0;
    let mut problem = None;
    for (key, name) in &selected {
        match delete_value(key, name) {
            Ok(()) => count += 1,
            Err(error) => problem = Some(error),
        }
    }
    Ok(Cleaned::Removed { count, problem })
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegOpenKeyExW(key: isize, subkey: *const u16, options: u32, access: u32, result: *mut isize) -> i32;
    fn RegEnumKeyExW(key: isize, index: u32, name: *mut u16, name_len: *mut u32, reserved: *mut u32, class: *mut u16, class_len: *mut u32, time: *mut std::ffi::c_void) -> i32;
    fn RegEnumValueW(key: isize, index: u32, name: *mut u16, name_len: *mut u32, reserved: *mut u32, kind: *mut u32, data: *mut u8, data_len: *mut u32) -> i32;
    fn RegDeleteValueW(key: isize, name: *const u16) -> i32;
    fn RegCloseKey(key: isize) -> i32;
}

const HKEY_LOCAL_MACHINE: isize = 0x8000_0002_u32 as i32 as isize;
const KEY_READ: u32 = 0x0002_0019;
const KEY_SET_VALUE: u32 = 0x0002;
const ERROR_FILE_NOT_FOUND: i32 = 2;
/// The longest key name, and the longest value name, in UTF-16 units (with the final zero).
const KEY_NAME_MAX: usize = 256;
const VALUE_NAME_MAX: usize = 16_384;

/// An open registry key, closed when dropped.
struct Key(isize);

impl Key {
    /// `path` under HKLM; `None` when it does not exist.
    fn open(path: &str, access: u32) -> Result<Option<Key>, String> {
        let mut handle = 0isize;
        match unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, wide(path).as_ptr(), 0, access, &mut handle) } {
            0 => Ok(Some(Key(handle))),
            ERROR_FILE_NOT_FOUND => Ok(None),
            status => Err(format!(r"HKLM\{path} cannot be opened (Windows error {status})")),
        }
    }

    fn subkeys(&self) -> Vec<String> {
        let mut names = Vec::new();
        let mut buffer = vec![0u16; KEY_NAME_MAX];
        for index in 0.. {
            let mut length = buffer.len() as u32;
            let status = unsafe { RegEnumKeyExW(self.0, index, buffer.as_mut_ptr(), &mut length, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
            if status != 0 {
                break;
            }
            names.push(String::from_utf16_lossy(&buffer[..length as usize]));
        }
        names
    }

    fn value_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        let mut buffer = vec![0u16; VALUE_NAME_MAX];
        for index in 0.. {
            let mut length = buffer.len() as u32;
            let status = unsafe { RegEnumValueW(self.0, index, buffer.as_mut_ptr(), &mut length, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
            if status != 0 {
                break;
            }
            names.push(String::from_utf16_lossy(&buffer[..length as usize]));
        }
        names
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

/// Every event key of every notification client, with its values' names. Reads only.
fn read_event_keys() -> Result<Vec<EventKey>, String> {
    let Some(root) = Key::open(NOTIFICATION_CLIENTS, KEY_READ)? else { return Ok(Vec::new()) };
    let mut keys = Vec::new();
    for client in root.subkeys() {
        let Ok(Some(client_key)) = Key::open(&format!(r"{NOTIFICATION_CLIENTS}\{client}"), KEY_READ) else { continue };
        for event in client_key.subkeys() {
            let Ok(Some(event_key)) = Key::open(&format!(r"{NOTIFICATION_CLIENTS}\{client}\{event}"), KEY_READ) else { continue };
            keys.push(EventKey { client: client.clone(), event, values: event_key.value_names() });
        }
    }
    Ok(keys)
}

/// Deletes one value (never its key).
fn delete_value(path: &str, name: &str) -> Result<(), String> {
    let Some(key) = Key::open(path, KEY_SET_VALUE)? else { return Ok(()) };
    match unsafe { RegDeleteValueW(key.0, wide(name).as_ptr()) } {
        0 | ERROR_FILE_NOT_FOUND => Ok(()),
        status => Err(format!(r"HKLM\{path}\{name} not deleted (Windows error {status})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(client: &str, event: &str, values: &[&str]) -> EventKey {
        EventKey { client: client.into(), event: event.into(), values: values.iter().map(|v| v.to_string()).collect() }
    }

    #[test]
    fn only_the_stream_events_values_are_chosen() {
        let keys = [
            key("235045741339", "StreamAdded", &["17666437384", "17666437391"]),
            key("235045741339", "StreamRemoved", &["17666440012"]),
            // Settings and profiles are other events: theirs stay.
            key("352176128", "GlobalSettingChanged", &["17666437384"]),
            key("352176128", "ProfileChanged", &["1"]),
            // A key's default value is not one of the engine's events.
            key("352176128", "streamadded", &["", "42"]),
            key("27499718699", "StreamRemoved", &[]),
        ];
        let base = NOTIFICATION_CLIENTS;
        assert_eq!(
            selection(&keys),
            [
                (format!(r"{base}\235045741339\StreamAdded"), "17666437384".to_string()),
                (format!(r"{base}\235045741339\StreamAdded"), "17666437391".to_string()),
                (format!(r"{base}\235045741339\StreamRemoved"), "17666440012".to_string()),
                (format!(r"{base}\352176128\streamadded"), "42".to_string()),
            ]
        );
        assert!(selection(&[]).is_empty());
    }

    #[test]
    fn nothing_outside_a_client_is_chosen() {
        // A client name that would climb out of NotificationClients, or none at all, is skipped.
        assert!(selection(&[key("", "StreamAdded", &["1"]), key(r"..\..\Game", "StreamAdded", &["1"])]).is_empty());
        assert!(is_stream_event("StreamAdded") && is_stream_event("STREAMREMOVED"));
        assert!(!is_stream_event("StreamChanged") && !is_stream_event("ApplicationAdded"));
    }
}
