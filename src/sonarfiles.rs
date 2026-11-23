//! Where SteelSeries' Sonar pieces are on this machine: GG's install folder when GG is installed,
//! otherwise the driver package Windows keeps in its DriverStore, or the copy Platinum's setup
//! keeps in `%ProgramData%\Platinum\sonar` (laid out like GG's folder).
//!
//! Platinum loads two of SteelSeries' DLLs: the settings API (`Sonar.APOAPI.dll`) and the effects
//! engine (`Sonar.APO.dll`, which loads onnxruntime and DirectML from its own folder). GG's copies
//! come first, so nothing changes on a machine with GG; without GG, the DriverStore copy is the one
//! the audio engine itself loads (the APO's COM registration points there), byte-identical to the
//! one in GG's installer. The DriverStore build of the settings API answers Platinum's reads
//! exactly as GG's other build does (`platinum status` and `list` compared on 17/11/2026).

use crate::com::wide;
use std::ffi::c_void;
use std::path::{Path, PathBuf};

/// The microphone effects object of `Sonar.APO.dll` ("Sonar APO MFX MIC Class").
pub const MIC_APO_CLSID: &str = "{4B7758C2-9361-4F88-83E9-44D8F05184EF}";
/// The settings API's control object in `Sonar.APOAPI.dll` ("BasicControl Class").
pub const API_CLSID: &str = "{DEA05346-7BE3-4DB0-AE9F-14423648EA7B}";
/// The INF of the APO package, as named in GG's installer (the DriverStore folder starts with it,
/// in lower case).
pub const APO_INF: &str = "Sonar.Apo.inf";
pub const VAD_INF: &str = "SteelSeries-Sonar-VAD.inf";
pub const EXTENSION_INF: &str = "SteelSeries-Sonar-VAD-Extension.inf";
/// The DLLs the APO package must hold for the engine to load (GG's service checks the same list).
pub const APO_FILES: [&str; 6] = ["Sonar.APO.dll", "Sonar.APOAPI.dll", "DnnProcessor.dll", "DirectML.dll", "onnxruntime.dll", "microsoft.ai.machinelearning.dll"];
/// Inside GG's folder (and Platinum's copy, laid out the same way).
pub const APO_PACKAGE: &str = r"apps\sonar\driver\apoDriverPackage";
pub const VAD_PACKAGE: &str = r"apps\sonar\driver\vad";
pub const DEVICE_TOOL: &str = r"shared\Steelseries.AudioDeviceInstaller.exe";
pub const MIGRATIONS: &str = r"apps\sonar\db-migrations";
/// The file Platinum's setup writes next to its copy when it installed the driver itself (GG did
/// not): only then may Platinum's uninstall offer to remove the driver.
pub const MARKER: &str = "installed-by-platinum.txt";

/// Where a DLL was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Named by an environment variable (`PLATINUM_APO_DLL`, `PLATINUM_ENGINE_DLL`).
    Environment,
    /// GG's install folder.
    Gg,
    /// The driver package in Windows' DriverStore.
    DriverStore,
    /// The copy kept by Platinum's setup.
    PlatinumCopy,
    /// Nowhere: the path is GG's usual one, and the file is missing.
    Missing,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Environment => "environment variable",
            Source::Gg => "GG's folder",
            Source::DriverStore => "Windows DriverStore",
            Source::PlatinumCopy => "Platinum's copy",
            Source::Missing => "not found",
        }
    }
}

/// GG's install folder (`%ProgramFiles%\SteelSeries\GG`).
pub fn gg_folder() -> PathBuf {
    let base = std::env::var_os("ProgramW6432").or_else(|| std::env::var_os("ProgramFiles")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
    base.join(r"SteelSeries\GG")
}

/// `%ProgramData%`.
pub fn program_data() -> PathBuf {
    std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
}

/// Platinum's machine-wide folder (`%ProgramData%\Platinum`), written only by the setup.
pub fn platinum_data() -> PathBuf {
    program_data().join("Platinum")
}

/// The setup's copy of the driver package and SteelSeries' device tool, laid out like GG's folder.
pub fn driver_copy() -> PathBuf {
    platinum_data().join("sonar")
}

/// Whether Platinum's setup installed the driver (its marker is there).
pub fn installed_by_platinum() -> bool {
    driver_copy().join(MARKER).is_file()
}

/// `%SystemRoot%`.
pub fn windows_folder() -> PathBuf {
    std::env::var_os("SystemRoot").or_else(|| std::env::var_os("windir")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
}

/// The DriverStore folders of a package (`<inf name in lower case>_amd64_<hash>`), newest first.
pub fn driver_store_folders(inf: &str) -> Vec<PathBuf> {
    let repository = windows_folder().join(r"System32\DriverStore\FileRepository");
    let prefix = format!("{}_", inf.to_ascii_lowercase());
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&repository)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().to_ascii_lowercase().starts_with(&prefix))
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| (e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), e.path()))
                .collect()
        })
        .unwrap_or_default();
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().map(|(_, path)| path).collect()
}

const HKEY_LOCAL_MACHINE: isize = 0x8000_0002_u32 as i32 as isize;
const RRF_RT_REG_SZ: u32 = 0x0000_0002;
const RRF_RT_REG_EXPAND_SZ: u32 = 0x0000_0004;
/// RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ, without RRF_NOEXPAND: expanded.
const RRF_TEXT: u32 = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ;

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegGetValueW(key: isize, subkey: *const u16, value: *const u16, flags: u32, kind: *mut u32, data: *mut c_void, bytes: *mut u32) -> i32;
}

/// A text value under HKEY_LOCAL_MACHINE (`""` for the default value); `None` when missing.
pub fn machine_text(key: &str, value: &str) -> Option<String> {
    let (key, value) = (wide(key), wide(value));
    let value_ptr = if value.len() <= 1 { std::ptr::null() } else { value.as_ptr() };
    let mut bytes = 0u32;
    unsafe {
        if RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), value_ptr, RRF_TEXT, std::ptr::null_mut(), std::ptr::null_mut(), &mut bytes) != 0 {
            return None;
        }
        let mut data = vec![0u16; (bytes as usize).div_ceil(2) + 1];
        let mut size = (data.len() * 2) as u32;
        if RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), value_ptr, RRF_TEXT, std::ptr::null_mut(), data.as_mut_ptr().cast(), &mut size) != 0 {
            return None;
        }
        let text = String::from_utf16_lossy(&data[..(size as usize / 2)]);
        let text = text.trim_end_matches('\0').trim().to_string();
        (!text.is_empty()).then_some(text)
    }
}

/// The file a COM class is registered with (`HKLM\SOFTWARE\Classes\CLSID\<clsid>\InprocServer32`),
/// when that file exists. SteelSeries' device tool registers the APO DLLs from the DriverStore.
pub fn registered_dll(clsid: &str) -> Option<PathBuf> {
    let path = machine_text(&format!(r"SOFTWARE\Classes\CLSID\{clsid}\InprocServer32"), "")?;
    let path = PathBuf::from(path.trim_matches('"'));
    path.is_file().then_some(path)
}

/// The installed APO package folder (holding `Sonar.APO.dll` and the libraries it loads): the
/// folder the APO is registered from, else the newest DriverStore copy that has the DLLs.
pub fn apo_package_folder() -> Option<PathBuf> {
    if let Some(folder) = registered_dll(MIC_APO_CLSID).and_then(|dll| dll.parent().map(Path::to_path_buf)) {
        return Some(folder);
    }
    driver_store_folders(APO_INF).into_iter().find(|folder| folder.join("Sonar.APO.dll").is_file())
}

/// Picks the first candidate that exists; GG's usual path (missing) when none does.
fn first_existing(candidates: Vec<(Source, PathBuf)>, fallback: PathBuf) -> (PathBuf, Source) {
    candidates.into_iter().find(|(_, path)| path.is_file()).map(|(source, path)| (path, source)).unwrap_or((fallback, Source::Missing))
}

/// The settings API to load, and where it was found: `PLATINUM_APO_DLL`, GG's folder, the
/// DriverStore package, Platinum's copy.
pub fn api_dll() -> (PathBuf, Source) {
    let gg = gg_folder().join(r"apps\sonar\Sonar.APOAPI.dll");
    if let Some(path) = std::env::var_os("PLATINUM_APO_DLL") {
        return (PathBuf::from(path), Source::Environment);
    }
    let mut candidates = vec![(Source::Gg, gg.clone()), (Source::Gg, gg_folder().join(APO_PACKAGE).join("Sonar.APOAPI.dll"))];
    if let Some(dll) = registered_dll(API_CLSID) {
        candidates.push((Source::DriverStore, dll));
    }
    if let Some(folder) = apo_package_folder() {
        candidates.push((Source::DriverStore, folder.join("Sonar.APOAPI.dll")));
    }
    candidates.push((Source::PlatinumCopy, driver_copy().join(APO_PACKAGE).join("Sonar.APOAPI.dll")));
    first_existing(candidates, gg)
}

/// The microphone effects engine to load, and where it was found: `PLATINUM_ENGINE_DLL`, GG's
/// folder, the DriverStore package, Platinum's copy. Its folder must hold the libraries it loads
/// at run time (`fxhost::Engine::create` adds it to the DLL search path).
pub fn engine_dll() -> (PathBuf, Source) {
    let gg = gg_folder().join(APO_PACKAGE).join("Sonar.APO.dll");
    if let Some(path) = std::env::var_os("PLATINUM_ENGINE_DLL") {
        return (PathBuf::from(path), Source::Environment);
    }
    let mut candidates = vec![(Source::Gg, gg.clone())];
    if let Some(folder) = apo_package_folder() {
        candidates.push((Source::DriverStore, folder.join("Sonar.APO.dll")));
    }
    candidates.push((Source::PlatinumCopy, driver_copy().join(APO_PACKAGE).join("Sonar.APO.dll")));
    first_existing(candidates, gg)
}

/// The folders that may hold GG's database migrations: Platinum's copy, then GG's own.
pub fn migrations_folders() -> Vec<PathBuf> {
    vec![driver_copy().join(MIGRATIONS), gg_folder().join(MIGRATIONS)]
}

/// The APO package files missing from a folder.
pub fn missing_apo_files(folder: &Path) -> Vec<&'static str> {
    APO_FILES.iter().copied().filter(|name| !folder.join(name).is_file()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_existing_candidate_wins() {
        let here = std::env::current_exe().unwrap();
        let missing = PathBuf::from(r"C:\no\such\file.dll");
        assert_eq!(first_existing(vec![(Source::Gg, missing.clone()), (Source::DriverStore, here.clone())], missing.clone()), (here.clone(), Source::DriverStore));
        assert_eq!(first_existing(vec![(Source::Gg, missing.clone())], missing.clone()), (missing, Source::Missing));
    }

    #[test]
    fn paths_are_laid_out_like_ggs_folder() {
        assert!(driver_copy().ends_with(r"Platinum\sonar"));
        assert!(gg_folder().ends_with(r"SteelSeries\GG"));
        assert!(migrations_folders()[0].ends_with(r"Platinum\sonar\apps\sonar\db-migrations"));
        let missing = missing_apo_files(Path::new(r"C:\no\such\folder"));
        assert_eq!(missing.len(), APO_FILES.len());
    }
}
