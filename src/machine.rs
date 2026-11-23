//! What this machine has of what Platinum needs, read only: Windows' version and architecture,
//! the Visual C++ runtime (the effects engine's onnxruntime needs it), GG (installed, running,
//! starting at logon), and the Sonar driver (its root device and its three packages). Shared by
//! `platinum doctor`, the window's banner and the setup.

use crate::com::{Guid, read_wide, wide};
use crate::sonarfiles::{self, APO_INF, EXTENSION_INF, VAD_INF};
use std::ffi::c_void;
use std::path::PathBuf;

/// The Sonar driver's root device, created by SteelSeries' device tool.
pub const SONAR_HWID: &str = r"ROOT\VEN_SSGG&DEV_0001";
/// The driver's INFs are decorated `NTamd64.10.0...17763`: Windows 10 1809, x64.
pub const MIN_BUILD: u32 = 17763;
/// The Visual C++ runtime GG's installer brings is 14.44; the engine's onnxruntime (1.9) was built
/// with Visual Studio 2019, whose last runtime is 14.29.
pub const MIN_VC_RUNTIME: (u32, u32) = (14, 29);
/// GG's programs that matter while it runs (its update services are left out: they run as
/// services and do not touch the audio devices).
pub const GG_PROCESSES: [&str; 9] = ["SteelSeriesGG.exe", "SteelSeriesGGClient.exe", "SteelSeriesSonar.exe", "SteelSeriesEngine.exe", "SteelSeriesPrism.exe", "SteelSeriesMoments.exe", "SteelSeriesGGEZ.exe", "SteelSeriesCaptureSvc.exe", "SteelSeriesCVGameSense.exe"];
/// GG's entry in "Installed apps" (HKLM, 64- or 32-bit view).
const GG_UNINSTALL_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\SteelSeries GG";
const GG_UNINSTALL_KEY_32: &str = r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\SteelSeries GG";
const VC_KEY: &str = r"SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64";
const VC_FILES: [&str; 3] = ["vcruntime140.dll", "vcruntime140_1.dll", "msvcp140.dll"];
/// GUID_DEVCLASS_MEDIA: the class SteelSeries' tool creates the root device in.
const MEDIA_CLASS: Guid = Guid { d1: 0x4d36_e96c, d2: 0xe325, d3: 0x11ce, d4: [0xbf, 0xc1, 0x08, 0x00, 0x2b, 0xe1, 0x03, 0x18] };

/// Windows' version, as the kernel reports it (not the compatibility-shimmed one).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowsVersion {
    pub major: u32,
    pub minor: u32,
    pub build: u32,
}

impl WindowsVersion {
    /// Whether the Sonar driver's INFs accept this Windows (10 1809 or later).
    pub fn supported(&self) -> bool {
        self.major > 10 || (self.major == 10 && self.build >= MIN_BUILD)
    }
}

#[repr(C)]
struct OsVersionInfo {
    size: u32,
    major: u32,
    minor: u32,
    build: u32,
    platform: u32,
    service_pack: [u16; 128],
}

#[repr(C)]
struct SystemInfo {
    architecture: u16,
    reserved: u16,
    page_size: u32,
    minimum_address: *mut c_void,
    maximum_address: *mut c_void,
    active_processors: usize,
    processors: u32,
    processor_type: u32,
    granularity: u32,
    level: u16,
    revision: u16,
}

#[repr(C)]
struct DeviceInfoData {
    size: u32,
    class: Guid,
    instance: u32,
    reserved: usize,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn RtlGetVersion(info: *mut OsVersionInfo) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetNativeSystemInfo(info: *mut SystemInfo);
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegGetValueW(key: isize, subkey: *const u16, value: *const u16, flags: u32, kind: *mut u32, data: *mut c_void, bytes: *mut u32) -> i32;
}

#[link(name = "setupapi")]
unsafe extern "system" {
    fn SetupDiGetClassDevsW(class: *const Guid, enumerator: *const u16, parent: *mut c_void, flags: u32) -> isize;
    fn SetupDiEnumDeviceInfo(set: isize, index: u32, data: *mut DeviceInfoData) -> i32;
    fn SetupDiGetDeviceRegistryPropertyW(set: isize, data: *mut DeviceInfoData, property: u32, kind: *mut u32, buffer: *mut u8, size: u32, required: *mut u32) -> i32;
    fn SetupDiGetDeviceInstanceIdW(set: isize, data: *mut DeviceInfoData, buffer: *mut u16, size: u32, required: *mut u32) -> i32;
    fn SetupDiDestroyDeviceInfoList(set: isize) -> i32;
}

const HKEY_LOCAL_MACHINE: isize = 0x8000_0002_u32 as i32 as isize;
const RRF_RT_REG_DWORD: u32 = 0x0000_0010;
const RRF_SUBKEY_WOW6464KEY: u32 = 0x0001_0000;
const RRF_SUBKEY_WOW6432KEY: u32 = 0x0002_0000;
const DIGCF_PRESENT: u32 = 0x2;
const SPDRP_HARDWAREID: u32 = 0x1;
const PROCESSOR_ARCHITECTURE_AMD64: u16 = 9;

pub fn windows_version() -> WindowsVersion {
    let mut info = OsVersionInfo { size: std::mem::size_of::<OsVersionInfo>() as u32, major: 0, minor: 0, build: 0, platform: 0, service_pack: [0; 128] };
    unsafe { RtlGetVersion(&mut info) };
    WindowsVersion { major: info.major, minor: info.minor, build: info.build }
}

/// Whether Windows runs on an x64 processor (the driver has no ARM64 build).
pub fn is_x64() -> bool {
    let mut info: SystemInfo = unsafe { std::mem::zeroed() };
    unsafe { GetNativeSystemInfo(&mut info) };
    info.architecture == PROCESSOR_ARCHITECTURE_AMD64
}

fn machine_dword(key: &str, value: &str, view: u32) -> Option<u32> {
    let (key, value) = (wide(key), wide(value));
    let mut data = 0u32;
    let mut bytes = 4u32;
    let status = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), value.as_ptr(), RRF_RT_REG_DWORD | view, std::ptr::null_mut(), (&mut data as *mut u32).cast(), &mut bytes) };
    (status == 0).then_some(data)
}

/// The Visual C++ 2015-2022 x64 runtime: its version (major, minor, build) when it is registered
/// as installed (either registry view) and its DLLs are in System32.
pub fn vc_runtime() -> Option<(u32, u32, u32)> {
    let system = sonarfiles::windows_folder().join("System32");
    if !VC_FILES.iter().all(|file| system.join(file).is_file()) {
        return None;
    }
    [RRF_SUBKEY_WOW6464KEY, RRF_SUBKEY_WOW6432KEY]
        .into_iter()
        .filter(|view| machine_dword(VC_KEY, "Installed", *view) == Some(1))
        .filter_map(|view| Some((machine_dword(VC_KEY, "Major", view)?, machine_dword(VC_KEY, "Minor", view)?, machine_dword(VC_KEY, "Bld", view).unwrap_or(0))))
        .max()
}

/// Whether a runtime version is recent enough for the effects engine.
pub fn vc_runtime_ok(version: Option<(u32, u32, u32)>) -> bool {
    version.is_some_and(|(major, minor, _)| (major, minor) >= MIN_VC_RUNTIME)
}

/// Whether SteelSeries GG is installed: its "Installed apps" entry, or its program in its folder.
pub fn gg_installed() -> bool {
    sonarfiles::machine_text(GG_UNINSTALL_KEY, "UninstallString").is_some()
        || sonarfiles::machine_text(GG_UNINSTALL_KEY_32, "UninstallString").is_some()
        || sonarfiles::gg_folder().join("SteelSeriesGG.exe").is_file()
}

/// GG's version from its "Installed apps" entry.
pub fn gg_version() -> Option<String> {
    sonarfiles::machine_text(GG_UNINSTALL_KEY, "DisplayVersion").or_else(|| sonarfiles::machine_text(GG_UNINSTALL_KEY_32, "DisplayVersion"))
}

/// GG's programs running now, by name.
pub fn gg_running() -> Vec<String> {
    gg_among(crate::routing::all_processes().unwrap_or_default().into_iter().map(|(_, name)| name))
}

/// The names among `processes` that are GG's, once each, as GG_PROCESSES writes them.
pub fn gg_among(processes: impl IntoIterator<Item = String>) -> Vec<String> {
    let running: Vec<String> = processes.into_iter().map(|p| p.to_ascii_lowercase()).collect();
    GG_PROCESSES.iter().filter(|name| running.contains(&name.to_ascii_lowercase())).map(|name| name.to_string()).collect()
}

/// One device with the Sonar driver's hardware id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SonarDevice {
    /// Its instance id (`ROOT\MEDIA\0000`).
    pub instance: String,
    /// Plugged in (for a root device: installed and started at least once since boot).
    pub present: bool,
}

/// The devices whose hardware ids include the Sonar driver's, present or not.
pub fn sonar_devices() -> Vec<SonarDevice> {
    let all = devices_with_hardware_id(SONAR_HWID, false);
    let present = devices_with_hardware_id(SONAR_HWID, true);
    all.into_iter().map(|instance| SonarDevice { present: present.contains(&instance), instance }).collect()
}

/// Instance ids of the MEDIA devices listing `hwid` among their hardware ids.
fn devices_with_hardware_id(hwid: &str, present_only: bool) -> Vec<String> {
    let mut found = Vec::new();
    unsafe {
        let set = SetupDiGetClassDevsW(&MEDIA_CLASS, std::ptr::null(), std::ptr::null_mut(), if present_only { DIGCF_PRESENT } else { 0 });
        if set == -1 {
            return found;
        }
        for index in 0.. {
            let mut data = DeviceInfoData { size: std::mem::size_of::<DeviceInfoData>() as u32, class: MEDIA_CLASS, instance: 0, reserved: 0 };
            if SetupDiEnumDeviceInfo(set, index, &mut data) == 0 {
                break;
            }
            let mut buffer = vec![0u8; 4096];
            let mut kind = 0u32;
            if SetupDiGetDeviceRegistryPropertyW(set, &mut data, SPDRP_HARDWAREID, &mut kind, buffer.as_mut_ptr(), buffer.len() as u32, std::ptr::null_mut()) == 0 {
                continue;
            }
            let units: Vec<u16> = buffer.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            if !multi_string(&units).iter().any(|id| id.eq_ignore_ascii_case(hwid)) {
                continue;
            }
            let mut instance = [0u16; 512];
            if SetupDiGetDeviceInstanceIdW(set, &mut data, instance.as_mut_ptr(), instance.len() as u32, std::ptr::null_mut()) != 0 {
                found.push(read_wide(instance.as_ptr()));
            }
        }
        SetupDiDestroyDeviceInfoList(set);
    }
    found
}

/// The strings of a REG_MULTI_SZ.
pub fn multi_string(units: &[u16]) -> Vec<String> {
    units.split(|u| *u == 0).take_while(|part| !part.is_empty()).map(String::from_utf16_lossy).collect()
}

/// The three packages of the Sonar driver and the DriverStore folder of each, when staged.
pub fn staged_packages() -> Vec<(&'static str, Option<PathBuf>)> {
    [VAD_INF, APO_INF, EXTENSION_INF].into_iter().map(|inf| (inf, sonarfiles::driver_store_folders(inf).into_iter().next())).collect()
}

/// Whether the Sonar driver is installed: its device exists and its three packages are staged.
pub fn driver_installed() -> bool {
    !sonar_devices().is_empty() && staged_packages().iter().all(|(_, folder)| folder.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_10_1809_is_the_floor() {
        let v = |major, build| WindowsVersion { major, minor: 0, build };
        assert!(v(10, 17763).supported());
        assert!(v(10, 26200).supported());
        assert!(!v(10, 17134).supported());
        assert!(!v(6, 9600).supported());
        assert!(windows_version().major >= 6);
    }

    #[test]
    fn runtime_versions_compare_by_major_and_minor() {
        assert!(vc_runtime_ok(Some((14, 44, 35211))));
        assert!(vc_runtime_ok(Some((14, 29, 0))));
        assert!(!vc_runtime_ok(Some((14, 16, 27012))));
        assert!(!vc_runtime_ok(None));
    }

    #[test]
    fn gg_processes_are_recognised_by_name() {
        let names = ["explorer.exe", "steelseriessonar.exe", "SteelSeriesGG.exe", "Steelseries.AudioDeviceInstaller.exe", "SteelSeriesGG.exe"].map(String::from);
        assert_eq!(gg_among(names), ["SteelSeriesGG.exe", "SteelSeriesSonar.exe"]);
    }

    #[test]
    fn multi_strings_split_at_nuls() {
        let units: Vec<u16> = "ROOT\\VEN_SSGG&DEV_0001\0OTHER\0\0junk".encode_utf16().collect();
        assert_eq!(multi_string(&units), [SONAR_HWID, "OTHER"]);
        assert!(multi_string(&[0, 0]).is_empty());
    }
}
