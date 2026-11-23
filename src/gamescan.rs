//! Finding games on this machine, for the preset per game (`games`): what each launcher records,
//! and what runs. Reads only: the registry (Steam's running application and its installed ones,
//! GOG's and Ubisoft Connect's installs, the Microsoft Store packages), Epic's manifests, Steam's
//! library manifests, and the running processes.
//!
//! Cost matters, since the daemon looks every few seconds for as long as it runs: Steam's running
//! application is one registry value; the processes are one Toolhelp snapshot, each process being
//! opened once in its life to read its path, its package and its start (`Scanner` keeps them).
//! What launchers record about installs changes rarely and is read again only now and then.

use crate::com::wide;
use crate::games::{GameKey, Installed, Process, family_name, parse_epic_manifest};
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::PathBuf;

const HKEY_CURRENT_USER: isize = 0x8000_0001_u32 as i32 as isize;
const HKEY_LOCAL_MACHINE: isize = 0x8000_0002_u32 as i32 as isize;
const KEY_READ: u32 = 0x0002_0019;
const RRF_RT_REG_SZ: u32 = 0x0000_0002;
const RRF_RT_REG_DWORD: u32 = 0x0000_0010;

const STEAM_KEY: &str = r"Software\Valve\Steam";
const GOG_KEY: &str = r"SOFTWARE\WOW6432Node\GOG.com\Games";
const UBISOFT_KEY: &str = r"SOFTWARE\WOW6432Node\Ubisoft\Launcher\Installs";
const PACKAGES_KEY: &str = r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\Repository\Packages";

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegOpenKeyExW(key: isize, subkey: *const u16, options: u32, access: u32, result: *mut isize) -> i32;
    fn RegEnumKeyExW(key: isize, index: u32, name: *mut u16, name_len: *mut u32, reserved: *mut u32, class: *mut u16, class_len: *mut u32, time: *mut c_void) -> i32;
    fn RegCloseKey(key: isize) -> i32;
    fn RegGetValueW(key: isize, subkey: *const u16, value: *const u16, flags: u32, kind: *mut u32, data: *mut c_void, bytes: *mut u32) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
    fn QueryFullProcessImageNameW(process: *mut c_void, flags: u32, name: *mut u16, size: *mut u32) -> i32;
    fn GetProcessTimes(process: *mut c_void, creation: *mut u64, exit: *mut u64, kernel: *mut u64, user: *mut u64) -> i32;
    fn GetPackageFamilyName(process: *mut c_void, length: *mut u32, name: *mut u16) -> i32;
    fn GetSystemTimeAsFileTime(time: *mut u64);
    fn CloseHandle(handle: *mut c_void) -> i32;
}

type EnumProc = unsafe extern "system" fn(*mut c_void, isize) -> i32;

#[link(name = "user32")]
unsafe extern "system" {
    fn EnumWindows(callback: EnumProc, param: isize) -> i32;
    fn IsWindowVisible(window: *mut c_void) -> i32;
    fn GetWindowTextW(window: *mut c_void, text: *mut u16, max: i32) -> i32;
    fn GetWindowThreadProcessId(window: *mut c_void, pid: *mut u32) -> u32;
    fn GetWindow(window: *mut c_void, command: u32) -> *mut c_void;
    fn GetWindowLongPtrW(window: *mut c_void, index: i32) -> isize;
}

const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const GW_OWNER: u32 = 4;
const GWL_EXSTYLE: i32 = -20;
const WS_EX_TOOLWINDOW: isize = 0x80;

/// The subkeys of a registry key (none when it is missing).
fn subkeys(root: isize, path: &str) -> Vec<String> {
    let mut names = Vec::new();
    unsafe {
        let mut key = 0isize;
        if RegOpenKeyExW(root, wide(path).as_ptr(), 0, KEY_READ, &mut key) != 0 {
            return names;
        }
        let mut buffer = [0u16; 512];
        for index in 0.. {
            let mut length = buffer.len() as u32;
            let status = RegEnumKeyExW(key, index, buffer.as_mut_ptr(), &mut length, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
            if status != 0 {
                break;
            }
            names.push(String::from_utf16_lossy(&buffer[..length as usize]));
        }
        RegCloseKey(key);
    }
    names
}

/// A text value of a registry key; `None` when it is missing or empty.
fn read_text(root: isize, path: &str, value: &str) -> Option<String> {
    let (path, value) = (wide(path), wide(value));
    let mut bytes = 0u32;
    unsafe {
        if RegGetValueW(root, path.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), std::ptr::null_mut(), &mut bytes) != 0 || bytes < 2 {
            return None;
        }
        let mut data = vec![0u16; (bytes as usize).div_ceil(2)];
        if RegGetValueW(root, path.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), data.as_mut_ptr().cast(), &mut bytes) != 0 {
            return None;
        }
        let text = String::from_utf16_lossy(&data[..(bytes as usize / 2)]);
        let text = text.trim_end_matches('\0').trim().to_string();
        (!text.is_empty()).then_some(text)
    }
}

/// A number value of a registry key.
fn read_dword(root: isize, path: &str, value: &str) -> Option<u32> {
    let (path, value) = (wide(path), wide(value));
    let mut data = 0u32;
    let mut bytes = 4u32;
    let status = unsafe { RegGetValueW(root, path.as_ptr(), value.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), (&mut data as *mut u32).cast(), &mut bytes) };
    (status == 0).then_some(data)
}

/// Windows' clock in file time (100 ns since 1601), as process start times are given.
pub fn now() -> u64 {
    let mut time = 0u64;
    unsafe { GetSystemTimeAsFileTime(&mut time) };
    time
}

/// The Steam application running now (`HKCU\Software\Valve\Steam\RunningAppID`, which Steam keeps
/// at 0 while none runs): one registry value, cheap enough for every look.
pub fn steam_running() -> Option<u32> {
    read_dword(HKEY_CURRENT_USER, STEAM_KEY, "RunningAppID").filter(|id| *id != 0)
}

/// A Steam application's name as Steam records it for the user, else its library manifest's.
pub fn steam_name(app: u32) -> Option<String> {
    read_text(HKEY_CURRENT_USER, &format!(r"{STEAM_KEY}\Apps\{app}"), "Name").or_else(|| steam_manifest_name(app))
}

/// Steam's library folders (`libraryfolders.vdf` next to Steam), its own folder first.
fn steam_libraries() -> Vec<PathBuf> {
    let Some(steam) = read_text(HKEY_CURRENT_USER, STEAM_KEY, "SteamPath") else { return Vec::new() };
    let steam = PathBuf::from(steam.replace('/', "\\"));
    let mut folders = vec![steam.clone()];
    if let Ok(text) = std::fs::read_to_string(steam.join("steamapps").join("libraryfolders.vdf")) {
        for path in vdf_values(&text, "path") {
            let folder = PathBuf::from(path);
            if !folders.iter().any(|known| known.to_string_lossy().eq_ignore_ascii_case(&folder.to_string_lossy())) {
                folders.push(folder);
            }
        }
    }
    folders
}

/// The name in an application's library manifest (`steamapps\appmanifest_<id>.acf`).
fn steam_manifest_name(app: u32) -> Option<String> {
    steam_libraries().into_iter().find_map(|folder| {
        let text = std::fs::read_to_string(folder.join("steamapps").join(format!("appmanifest_{app}.acf"))).ok()?;
        vdf_values(&text, "name").into_iter().next()
    })
}

/// The values of a key in Valve's text format (`"key"		"value"` lines), unescaped.
fn vdf_values(text: &str, key: &str) -> Vec<String> {
    let wanted = format!("\"{key}\"");
    text.lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix(&wanted)?;
            let value = rest.trim().strip_prefix('"')?.strip_suffix('"')?;
            Some(value.replace("\\\\", "\\"))
        })
        .collect()
}

/// Steam applications installed for the user (`Apps\<id>\Installed` = 1).
pub fn steam_installed() -> Vec<Installed> {
    let apps = format!(r"{STEAM_KEY}\Apps");
    subkeys(HKEY_CURRENT_USER, &apps)
        .into_iter()
        .filter_map(|id| {
            let app: u32 = id.parse().ok()?;
            let key = format!(r"{apps}\{id}");
            (read_dword(HKEY_CURRENT_USER, &key, "Installed") == Some(1)).then(|| Installed { key: GameKey::Steam(app), name: steam_name(app).unwrap_or_else(|| format!("Steam {app}")), folder: String::new() })
        })
        .collect()
}

/// Epic Games' manifests folder.
fn epic_folder() -> PathBuf {
    let data = std::env::var_os("ProgramData").map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from);
    data.join("Epic").join("EpicGamesLauncher").join("Data").join("Manifests")
}

/// The games Epic Games installed, from its manifests, with their folders.
pub fn epic_installed() -> Vec<Installed> {
    let Ok(listing) = std::fs::read_dir(epic_folder()) else { return Vec::new() };
    listing
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|x| x.eq_ignore_ascii_case("item")))
        .filter_map(|path| std::fs::read(&path).ok())
        .filter_map(|bytes| parse_epic_manifest(&String::from_utf8_lossy(&bytes)))
        .collect()
}

/// The games GOG Galaxy installed (`GOG.com\Games\<product id>`: `gameName`, `path`).
pub fn gog_installed() -> Vec<Installed> {
    subkeys(HKEY_LOCAL_MACHINE, GOG_KEY)
        .into_iter()
        .filter_map(|id| {
            let key = format!(r"{GOG_KEY}\{id}");
            let folder = read_text(HKEY_LOCAL_MACHINE, &key, "path")?;
            let product = read_text(HKEY_LOCAL_MACHINE, &key, "gameID").unwrap_or(id);
            let name = read_text(HKEY_LOCAL_MACHINE, &key, "gameName").unwrap_or_else(|| last_folder(&folder));
            Some(Installed { key: GameKey::Gog(product), name, folder })
        })
        .collect()
}

/// The games Ubisoft Connect installed (`Ubisoft\Launcher\Installs\<id>`: `InstallDir`).
pub fn ubisoft_installed() -> Vec<Installed> {
    subkeys(HKEY_LOCAL_MACHINE, UBISOFT_KEY)
        .into_iter()
        .filter_map(|id| {
            let folder = read_text(HKEY_LOCAL_MACHINE, &format!(r"{UBISOFT_KEY}\{id}"), "InstallDir")?;
            Some(Installed { key: GameKey::Ubisoft(id), name: last_folder(&folder), folder })
        })
        .collect()
}

/// The Microsoft Store packages installed for the user, each family once.
pub fn store_installed() -> Vec<Installed> {
    let mut found: Vec<Installed> = Vec::new();
    for full in subkeys(HKEY_CURRENT_USER, PACKAGES_KEY) {
        let Some(family) = family_name(&full) else { continue };
        if found.iter().any(|known| matches!(&known.key, GameKey::Store(f) if f.eq_ignore_ascii_case(&family))) {
            continue;
        }
        let shown = read_text(HKEY_CURRENT_USER, &format!(r"{PACKAGES_KEY}\{full}"), "DisplayName").filter(|name| !name.starts_with('@'));
        let name = shown.unwrap_or_else(|| family.split('_').next().unwrap_or(&family).to_string());
        found.push(Installed { key: GameKey::Store(family), name, folder: String::new() });
    }
    found
}

/// Every game installed by a launcher Platinum knows.
pub fn installed() -> Vec<Installed> {
    let mut all = steam_installed();
    all.extend(epic_installed());
    all.extend(store_installed());
    all.extend(gog_installed());
    all.extend(ubisoft_installed());
    all
}

/// The Steam applications installed in Steam's libraries, with their folders, from their library
/// manifests: a process running from inside the folder is the game. `RunningAppID` only sees a
/// game Steam started; this also finds one started from its own executable or a shortcut, which
/// Steam does not track (seen on 17/11/2026 with DELTARUNE).
pub fn steam_folder_installs() -> Vec<Installed> {
    let mut found = Vec::new();
    for library in steam_libraries() {
        let apps = library.join("steamapps");
        let Ok(listing) = std::fs::read_dir(&apps) else { continue };
        let common = apps.join("common");
        for entry in listing.flatten() {
            let file = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if !(file.starts_with("appmanifest_") && file.ends_with(".acf")) {
                continue;
            }
            if let Some(game) = std::fs::read_to_string(entry.path()).ok().and_then(|text| parse_steam_manifest(&text, &common)) {
                found.push(game);
            }
        }
    }
    found
}

/// A library manifest (`steamapps\appmanifest_<id>.acf`: `appid`, `name`, `installdir`) as an
/// installed game whose folder is `common\<installdir>`. `None` without an id or a folder.
fn parse_steam_manifest(text: &str, common: &std::path::Path) -> Option<Installed> {
    let first = |key: &str| vdf_values(text, key).into_iter().find(|value| !value.is_empty());
    let app: u32 = first("appid")?.parse().ok()?;
    let folder = first("installdir")?;
    let name = first("name").unwrap_or_else(|| folder.clone());
    Some(Installed { key: GameKey::Steam(app), name, folder: common.join(folder).to_string_lossy().into_owned() })
}

/// The installs a running process can be told by, from its folder: Steam, Epic, GOG, Ubisoft
/// Connect.
pub fn folder_installs() -> Vec<Installed> {
    let mut all = steam_folder_installs();
    all.extend(epic_installed());
    all.extend(gog_installed());
    all.extend(ubisoft_installed());
    all.retain(|game| !game.folder.is_empty());
    all
}

/// A folder's last part ("Assassin's Creed Mirage" for "D:\Ubisoft\Assassin's Creed Mirage\").
fn last_folder(path: &str) -> String {
    path.trim_end_matches(['\\', '/']).rsplit(['\\', '/']).next().unwrap_or(path).to_string()
}

/// The running processes, each opened once in its life: its path, package and start do not
/// change, so a process already seen is not opened again. A process identifier reused by another
/// program (another executable name) is read again.
#[derive(Default)]
pub struct Scanner {
    known: HashMap<u32, Process>,
}

impl Scanner {
    pub fn scan(&mut self) -> Vec<Process> {
        let Ok(table) = crate::routing::process_table() else { return self.known.values().cloned().collect() };
        let mut fresh: HashMap<u32, Process> = HashMap::with_capacity(table.len());
        for (pid, exe, _) in table {
            let process = match self.known.remove(&pid) {
                Some(process) if process.exe == exe => process,
                _ => read_process(pid, exe),
            };
            fresh.insert(pid, process);
        }
        self.known = fresh;
        let mut list: Vec<Process> = self.known.values().cloned().collect();
        list.sort_by_key(|p| p.pid);
        list
    }

    /// Forgets every process (the feature was turned off: its memory goes back).
    pub fn clear(&mut self) {
        self.known = HashMap::new();
    }
}

/// What a process's handle tells: its path, its package's family, its start. What cannot be read
/// (a protected process, the system's) stays empty.
fn read_process(pid: u32, exe: String) -> Process {
    let mut process = Process { pid, exe, ..Process::default() };
    if pid == 0 {
        return process;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return process;
        }
        let mut name = [0u16; 1024];
        let mut size = name.len() as u32;
        if QueryFullProcessImageNameW(handle, 0, name.as_mut_ptr(), &mut size) != 0 {
            process.path = String::from_utf16_lossy(&name[..size as usize]);
        }
        let mut family = [0u16; 256];
        let mut length = family.len() as u32;
        if GetPackageFamilyName(handle, &mut length, family.as_mut_ptr()) == 0 && length > 0 {
            process.family = String::from_utf16_lossy(&family[..(length as usize - 1).min(family.len())]).trim_end_matches('\0').to_string();
        }
        let (mut creation, mut exit, mut kernel, mut user) = (0u64, 0u64, 0u64, 0u64);
        if GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) != 0 {
            process.created = creation;
        }
        CloseHandle(handle);
    }
    process
}

/// A program with a window on screen: its executable and its window's title.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Windowed {
    pub pid: u32,
    pub title: String,
}

unsafe extern "system" fn collect_window(window: *mut c_void, param: isize) -> i32 {
    let found = unsafe { &mut *(param as *mut Vec<Windowed>) };
    unsafe {
        if IsWindowVisible(window) == 0 || !GetWindow(window, GW_OWNER).is_null() || GetWindowLongPtrW(window, GWL_EXSTYLE) & WS_EX_TOOLWINDOW != 0 {
            return 1;
        }
        let mut text = [0u16; 256];
        let length = GetWindowTextW(window, text.as_mut_ptr(), text.len() as i32);
        if length <= 0 {
            return 1;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(window, &mut pid);
        let title = String::from_utf16_lossy(&text[..length as usize]).trim().to_string();
        if pid != 0 && !title.is_empty() && !found.iter().any(|w| w.pid == pid) {
            found.push(Windowed { pid, title });
        }
    }
    1
}

/// The programs with a visible top-level window (not a tool window, not owned by another), each
/// process once with its first window's title.
pub fn windowed() -> Vec<Windowed> {
    let mut found: Vec<Windowed> = Vec::new();
    unsafe { EnumWindows(collect_window, (&mut found as *mut Vec<Windowed>) as isize) };
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_steam_manifest_gives_the_game_and_its_folder() {
        let text = "\"AppState\"\n{\n\t\"appid\"\t\t\"1671210\"\n\t\"universe\"\t\t\"1\"\n\t\"name\"\t\t\"DELTARUNE\"\n\t\"installdir\"\t\t\"DELTARUNE\"\n\t\"InstalledDepots\"\n\t{\n\t\t\"1671212\"\n\t\t{\n\t\t\t\"manifest\"\t\t\"42\"\n\t\t}\n\t}\n}\n";
        let common = std::path::Path::new(r"D:\SteamLibrary\steamapps\common");
        let game = parse_steam_manifest(text, common).unwrap();
        assert_eq!(game.key, GameKey::Steam(1671210));
        assert_eq!(game.name, "DELTARUNE");
        assert_eq!(game.folder, r"D:\SteamLibrary\steamapps\common\DELTARUNE");
        assert!(crate::games::is_under(r"D:\SteamLibrary\steamapps\common\DELTARUNE\DELTARUNE.exe", &game.folder));
        // No folder, no id: nothing a process could be told by.
        assert!(parse_steam_manifest("\"AppState\"\n{\n\t\"appid\"\t\t\"228980\"\n}\n", common).is_none());
        assert!(parse_steam_manifest("\"AppState\"\n{\n\t\"installdir\"\t\t\"x\"\n}\n", common).is_none());
    }

    #[test]
    fn valves_text_format_is_read_with_its_escapes() {
        let text = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"C:\\\\Program Files (x86)\\\\Steam\"\n\t\t\"label\"\t\t\"\"\n\t}\n\t\"1\"\n\t{\n\t\t\"path\"\t\t\"D:\\\\SteamLibrary\"\n\t}\n}\n";
        assert_eq!(vdf_values(text, "path"), [r"C:\Program Files (x86)\Steam", r"D:\SteamLibrary"]);
        assert_eq!(vdf_values("\t\"name\"\t\t\"Counter-Strike 2\"\n\t\"names\"\t\"x\"\n", "name"), ["Counter-Strike 2"]);
        assert_eq!(last_folder(r"D:\Ubisoft\Assassin's Creed Mirage\"), "Assassin's Creed Mirage");
    }

    #[test]
    fn this_process_is_read_with_its_path_and_start() {
        let me = std::process::id();
        let mut scanner = Scanner::default();
        let first = scanner.scan();
        let mine = first.iter().find(|p| p.pid == me).expect("the test runs");
        assert!(mine.path.to_lowercase().ends_with(".exe") && mine.created > 0 && mine.created <= now());
        assert!(mine.family.is_empty(), "a test is no packaged application");
        // Seen again without opening it: the same.
        let again = scanner.scan();
        assert_eq!(again.iter().find(|p| p.pid == me), Some(mine));
    }
}
