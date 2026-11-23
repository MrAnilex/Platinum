//! The Sonar driver installed and removed with Windows' own functions, when SteelSeries' device
//! tool fails (seen on 22/11/2026 in Windows Sandbox: its `add` stopped at once on E_NOINTERFACE,
//! before staging anything). SteelSeries' tool stays the first attempt: it is what GG does. This
//! follows `analysis/sonar/driver-install.md` (§3.3, §3.6, §5, §6.2, §6.3), Win32 only:
//!
//! - install: the three packages staged in GG's order (VAD, APO, extension) with `DiInstallDriverW`;
//!   the root device `ROOT\VEN_SSGG&DEV_0001` created only when no device has that hardware id
//!   (MEDIA class, generated id, hardware id and manufacturer, `DIF_REGISTERDEVICE`, then
//!   `DiInstallDevice`, else `UpdateDriverForPlugAndPlayDevicesW`; removed again with `DIF_REMOVE`
//!   if neither gives it a driver); Windows Audio (and its running dependents) restarted only when
//!   something new was installed, after GG's 7 s wait;
//! - register: `regsvr32 /s` on the two APO DLLs of the DriverStore copy, its folder given by
//!   `SetupGetInfDriverStoreLocationW` for the published INF (`%windir%\INF\oemNN.inf`) that has
//!   the APO package's CatalogFile and DriverVer, as SteelSeries' `register` finds it;
//! - remove: `regsvr32 /u /s`, Windows Audio stopped, every device with the hardware id uninstalled
//!   (`DiUninstallDevice`), every published INF with the catalog of the extension, VAD and APO
//!   packages uninstalled (`DiUninstallDriverW`, any version, as SteelSeries' `remove`), Windows
//!   Audio started again.
//!
//! Every call's result goes to the journal with Windows' error and its meaning. Finding the
//! published INFs only reads `%windir%\INF`. The calls never panic; every device information set
//! is destroyed and every service handle closed (drop guards). newdev.dll has no import library in
//! the GNU toolchain: its functions are resolved at run time.

use crate::install::run_program;
use crate::log::Log;
use platinum::com::{Guid, export, load_library, read_wide, wide};
use platinum::machine::SONAR_HWID;
use platinum::sonarfiles::{APO_INF, APO_PACKAGE, EXTENSION_INF, VAD_INF, VAD_PACKAGE};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// GUID_DEVCLASS_MEDIA, the class SteelSeries' tool creates the device in.
const MEDIA_CLASS: Guid = Guid { d1: 0x4d36_e96c, d2: 0xe325, d3: 0x11ce, d4: [0xbf, 0xc1, 0x08, 0x00, 0x2b, 0xe1, 0x03, 0x18] };
/// The manufacturer SteelSeries' tool writes on the device.
const MANUFACTURER: &str = "SteelSeries A/S";
const DICD_GENERATE_ID: u32 = 0x1;
const DIGCF_ALLCLASSES: u32 = 0x4;
const SPDRP_HARDWAREID: u32 = 0x01;
const SPDRP_MFG: u32 = 0x0B;
const DIF_REGISTERDEVICE: u32 = 0x19;
const DIF_REMOVE: u32 = 0x05;
const INVALID_SET: isize = -1;
const ERROR_NO_MORE_ITEMS: u32 = 259;
const ERROR_SERVICE_ALREADY_RUNNING: u32 = 1056;
const ERROR_SERVICE_NOT_ACTIVE: u32 = 1062;

#[repr(C)]
struct DeviceInfoData {
    size: u32,
    class: Guid,
    instance: u32,
    reserved: usize,
}

impl DeviceInfoData {
    fn new() -> DeviceInfoData {
        DeviceInfoData { size: std::mem::size_of::<DeviceInfoData>() as u32, class: MEDIA_CLASS, instance: 0, reserved: 0 }
    }
}

#[repr(C)]
#[derive(Default)]
struct ServiceStatus {
    kind: u32,
    state: u32,
    controls: u32,
    exit_code: u32,
    service_exit_code: u32,
    checkpoint: u32,
    wait_hint: u32,
}

#[repr(C)]
struct EnumServiceStatus {
    name: *mut u16,
    display: *mut u16,
    status: ServiceStatus,
}

#[link(name = "setupapi")]
unsafe extern "system" {
    fn SetupDiCreateDeviceInfoList(class: *const Guid, parent: *mut c_void) -> isize;
    fn SetupDiCreateDeviceInfoW(set: isize, name: *const u16, class: *const Guid, description: *const u16, parent: *mut c_void, flags: u32, data: *mut DeviceInfoData) -> i32;
    fn SetupDiSetDeviceRegistryPropertyW(set: isize, data: *mut DeviceInfoData, property: u32, buffer: *const u8, size: u32) -> i32;
    fn SetupDiGetDeviceRegistryPropertyW(set: isize, data: *mut DeviceInfoData, property: u32, kind: *mut u32, buffer: *mut u8, size: u32, required: *mut u32) -> i32;
    fn SetupDiGetDeviceInstanceIdW(set: isize, data: *mut DeviceInfoData, buffer: *mut u16, size: u32, required: *mut u32) -> i32;
    fn SetupDiCallClassInstaller(function: u32, set: isize, data: *mut DeviceInfoData) -> i32;
    fn SetupDiGetClassDevsW(class: *const Guid, enumerator: *const u16, parent: *mut c_void, flags: u32) -> isize;
    fn SetupDiEnumDeviceInfo(set: isize, index: u32, data: *mut DeviceInfoData) -> i32;
    fn SetupDiDestroyDeviceInfoList(set: isize) -> i32;
    fn SetupGetInfDriverStoreLocationW(file: *const u16, platform: *const c_void, locale: *const u16, buffer: *mut u16, size: u32, required: *mut u32) -> i32;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenSCManagerW(machine: *const u16, database: *const u16, access: u32) -> *mut c_void;
    fn OpenServiceW(manager: *mut c_void, name: *const u16, access: u32) -> *mut c_void;
    fn ControlService(service: *mut c_void, control: u32, status: *mut ServiceStatus) -> i32;
    fn QueryServiceStatus(service: *mut c_void, status: *mut ServiceStatus) -> i32;
    fn StartServiceW(service: *mut c_void, count: u32, arguments: *const *const u16) -> i32;
    fn EnumDependentServicesW(service: *mut c_void, state: u32, services: *mut c_void, size: u32, needed: *mut u32, count: *mut u32) -> i32;
    fn CloseServiceHandle(handle: *mut c_void) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetLastError() -> u32;
    fn FormatMessageW(flags: u32, source: *const c_void, id: u32, language: u32, buffer: *mut u16, size: u32, arguments: *const c_void) -> u32;
}

/// A BOOL result with the error the call left, read before anything else can change it:
/// `outcome(unsafe { Call(...) })`.
fn outcome(result: i32) -> (bool, u32) {
    let error = unsafe { GetLastError() };
    (result != 0, if result != 0 { 0 } else { error })
}

/// Names of the SetupAPI errors (Windows' message table does not have them).
fn setupapi_name(code: u32) -> Option<&'static str> {
    Some(match code {
        0xE000_0200 => "ERROR_NO_ASSOCIATED_CLASS",
        0xE000_0201 => "ERROR_CLASS_MISMATCH",
        0xE000_0202 => "ERROR_DUPLICATE_FOUND",
        0xE000_0203 => "ERROR_NO_DRIVER_SELECTED",
        0xE000_0204 => "ERROR_KEY_DOES_NOT_EXIST",
        0xE000_0205 => "ERROR_INVALID_DEVINST_NAME",
        0xE000_0206 => "ERROR_INVALID_CLASS",
        0xE000_0207 => "ERROR_DEVINST_ALREADY_EXISTS",
        0xE000_0208 => "ERROR_DEVINFO_NOT_REGISTERED",
        0xE000_0209 => "ERROR_INVALID_REG_PROPERTY",
        0xE000_020A => "ERROR_NO_INF",
        0xE000_020B => "ERROR_NO_SUCH_DEVINST",
        0xE000_020D => "ERROR_INVALID_CLASS_INSTALLER",
        0xE000_020E => "ERROR_DI_DO_DEFAULT",
        0xE000_0211 => "ERROR_NO_DEVICE_SELECTED",
        0xE000_0219 => "ERROR_INVALID_FILTER_DRIVER",
        0xE000_0228 => "ERROR_NO_COMPAT_DRIVERS",
        0xE000_022D => "ERROR_NO_SUCH_INTERFACE_DEVICE",
        0xE000_0235 => "ERROR_IN_WOW64",
        0xE000_023C => "ERROR_PNP_REGISTRY_ERROR",
        0xE000_0241 => "ERROR_AUTHENTICODE_PUBLISHER_NOT_TRUSTED",
        0xE000_0247 => "ERROR_DRIVER_STORE_ADD_FAILED",
        0xE000_0248 => "ERROR_DEVICE_INSTALL_BLOCKED",
        0xE000_0249 => "ERROR_DRIVER_INSTALL_BLOCKED",
        0xE000_024A => "ERROR_WRONG_INF_TYPE",
        0xE000_024B => "ERROR_FILE_HASH_NOT_IN_CATALOG",
        0xE000_024C => "ERROR_DRIVER_STORE_DELETE_FAILED",
        _ => return None,
    })
}

/// A Windows error as the journal shows it: its number, then SetupAPI's name for it or Windows'
/// own sentence (in the system's language).
pub fn error_text(code: u32) -> String {
    if let Some(name) = setupapi_name(code) {
        return format!("0x{code:08X} ({name})");
    }
    let mut buffer = [0u16; 512];
    // FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS
    let length = unsafe { FormatMessageW(0x1000 | 0x200, std::ptr::null(), code, 0, buffer.as_mut_ptr(), buffer.len() as u32, std::ptr::null()) };
    let message = String::from_utf16_lossy(&buffer[..(length as usize).min(buffer.len())]).trim().to_string();
    if message.is_empty() { format!("0x{code:08X} ({code})") } else { format!("0x{code:08X} ({code}: {message})") }
}

/// Journals a call: "ok", or the error it left. Returns whether it worked.
fn note(log: &mut Log, what: &str, (ok, error): (bool, u32)) -> bool {
    if ok {
        log.line(format!("  {what}: ok"));
    } else {
        log.line(format!("  {what}: failed, error {}", error_text(error)));
    }
    ok
}

/// Strings as the registry wants a REG_MULTI_SZ: UTF-16LE, each ended by a NUL, then one more.
pub fn multi_string(ids: &[&str]) -> Vec<u8> {
    let mut units: Vec<u16> = Vec::new();
    for id in ids {
        units.extend(id.encode_utf16());
        units.push(0);
    }
    units.push(0);
    units.iter().flat_map(|u| u.to_le_bytes()).collect()
}

/// A REG_SZ: UTF-16LE ended by a NUL.
fn single_string(text: &str) -> Vec<u8> {
    wide(text).iter().flat_map(|u| u.to_le_bytes()).collect()
}

/// The three INFs in the copy, in GG's install order (VAD, APO, extension).
pub fn install_order(copy: &Path) -> [PathBuf; 3] {
    [copy.join(VAD_PACKAGE).join(VAD_INF), copy.join(APO_PACKAGE).join(APO_INF), copy.join(VAD_PACKAGE).join(EXTENSION_INF)]
}

/// The same three in GG's removal order (extension, VAD, APO).
pub fn removal_order(copy: &Path) -> [PathBuf; 3] {
    let [vad, apo, extension] = install_order(copy);
    [extension, vad, apo]
}

/// How staging a package went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Staged {
    /// Put in the driver store ("inf installed" in SteelSeries' tool): Windows Audio is restarted.
    Installed,
    /// Already there and current ("inf skipped": `ERROR_NO_MORE_ITEMS`).
    Skipped,
    Failed(u32),
}

/// `DiInstallDriverW`'s result read the way SteelSeries' tool reads it.
pub fn staged((ok, error): (bool, u32)) -> Staged {
    match (ok, error) {
        (true, _) => Staged::Installed,
        (false, ERROR_NO_MORE_ITEMS) => Staged::Skipped,
        (false, code) => Staged::Failed(code),
    }
}

/// Whether a SteelSeries command's exit code calls for the fallback: anything but 0 (done) and
/// 1 (done, restart wanted), including not starting at all (-1).
pub fn needs_fallback(code: i32) -> bool {
    !matches!(code, 0 | 1)
}

/// What the fallback install did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub packages_installed: usize,
    pub device_created: bool,
    pub reboot: bool,
}

impl Report {
    /// Windows Audio is restarted only when something new was installed.
    pub fn restarts_audio(&self) -> bool {
        self.packages_installed > 0 || self.device_created
    }
}

/// The two lines of an INF's [Version] section that identify its package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InfIdentity {
    pub catalog: String,
    /// `DriverVer`, spaces removed (`09/09/2026,3.9.0.0`).
    pub version: String,
}

/// Reads CatalogFile and DriverVer from an INF (ANSI, UTF-8 or UTF-16LE with its BOM).
pub fn inf_identity(bytes: &[u8]) -> Option<InfIdentity> {
    let text = if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = rest.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)).into_owned()
    };
    let (mut in_version, mut catalog, mut version) = (false, None, None);
    for line in text.lines() {
        let line = line.split(';').next().unwrap_or("").trim();
        if let Some(section) = line.strip_prefix('[') {
            in_version = section.trim_end_matches(']').trim().eq_ignore_ascii_case("version");
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        if !in_version {
            continue;
        }
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().trim_matches('"').trim();
        if (key == "catalogfile" || key.starts_with("catalogfile.")) && catalog.is_none() {
            catalog = Some(value.to_string());
        } else if key == "driverver" {
            version = Some(value.chars().filter(|c| !c.is_whitespace()).collect());
        }
    }
    Some(InfIdentity { catalog: catalog.filter(|c: &String| !c.is_empty())?, version: version.unwrap_or_default() })
}

/// The catalog of each of the three INFs in GG 123's installer, for a removal without the copy.
pub fn known_catalog(inf: &Path) -> Option<&'static str> {
    let name = inf.file_name()?.to_string_lossy().to_ascii_lowercase();
    [(VAD_INF, "SteelSeries-Sonar-VAD.cat"), (APO_INF, "Sonar.APO.cat"), (EXTENSION_INF, "SteelSeries-Sonar-VAD-Extension.cat")].into_iter().find(|(inf, _)| inf.to_ascii_lowercase() == name).map(|(_, catalog)| catalog)
}

/// The identity of one of the copy's INFs, or its known catalog (any version) when it is missing.
fn identity_of(inf: &Path) -> Option<InfIdentity> {
    std::fs::read(inf).ok().and_then(|bytes| inf_identity(&bytes)).or_else(|| known_catalog(inf).map(|catalog| InfIdentity { catalog: catalog.to_string(), version: String::new() }))
}

/// `oemNN.inf`'s number, for names Windows gives published INFs.
fn oem_number(name: &str) -> Option<u32> {
    let lower = name.to_ascii_lowercase();
    let digits = lower.strip_prefix("oem")?.strip_suffix(".inf")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The published INFs (`oemNN.inf` in `folder`) with that catalog, and that DriverVer when one is
/// given, in number order. Only reads.
pub fn published_infs(folder: &Path, catalog: &str, version: Option<&str>) -> Vec<PathBuf> {
    let mut found: Vec<(u32, PathBuf)> = std::fs::read_dir(folder)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| Some((oem_number(&entry.file_name().to_string_lossy())?, entry.path())))
                .filter(|(_, path)| {
                    std::fs::read(path).ok().and_then(|bytes| inf_identity(&bytes)).is_some_and(|identity| identity.catalog.eq_ignore_ascii_case(catalog) && version.is_none_or(|v| v == identity.version))
                })
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found.into_iter().map(|(_, path)| path).collect()
}

fn inf_folder() -> PathBuf {
    platinum::sonarfiles::windows_folder().join("INF")
}

type InstallDriver = unsafe extern "system" fn(*mut c_void, *const u16, u32, *mut i32) -> i32;
type InstallDevice = unsafe extern "system" fn(*mut c_void, isize, *mut DeviceInfoData, *mut c_void, u32, *mut i32) -> i32;
type UpdateDriver = unsafe extern "system" fn(*mut c_void, *const u16, *const u16, u32, *mut i32) -> i32;
type UninstallDevice = unsafe extern "system" fn(*mut c_void, isize, *mut DeviceInfoData, u32, *mut i32) -> i32;
type UninstallDriver = unsafe extern "system" fn(*mut c_void, *const u16, u32, *mut i32) -> i32;

/// newdev.dll's functions (signatures of newdev.h), resolved at run time.
struct NewDev {
    install_driver: InstallDriver,
    install_device: InstallDevice,
    update_driver: UpdateDriver,
    uninstall_device: UninstallDevice,
    uninstall_driver: UninstallDriver,
}

impl NewDev {
    fn load() -> Result<NewDev, String> {
        // LOAD_LIBRARY_SEARCH_SYSTEM32: never a newdev.dll from the setup's folder.
        let module = load_library("newdev.dll", 0x800).map_err(|e| e.to_string())?;
        let function = |name: &'static [u8]| export(module, name).map_err(|e| e.to_string());
        unsafe {
            Ok(NewDev {
                install_driver: std::mem::transmute::<*mut c_void, InstallDriver>(function(b"DiInstallDriverW\0")?),
                install_device: std::mem::transmute::<*mut c_void, InstallDevice>(function(b"DiInstallDevice\0")?),
                update_driver: std::mem::transmute::<*mut c_void, UpdateDriver>(function(b"UpdateDriverForPlugAndPlayDevicesW\0")?),
                uninstall_device: std::mem::transmute::<*mut c_void, UninstallDevice>(function(b"DiUninstallDevice\0")?),
                uninstall_driver: std::mem::transmute::<*mut c_void, UninstallDriver>(function(b"DiUninstallDriverW\0")?),
            })
        }
    }
}

/// A device information set, destroyed on drop.
struct DeviceSet(isize);

impl Drop for DeviceSet {
    fn drop(&mut self) {
        if self.0 != INVALID_SET {
            unsafe { SetupDiDestroyDeviceInfoList(self.0) };
        }
    }
}

/// Every device of any class, present or not.
fn all_devices(log: &mut Log) -> Option<DeviceSet> {
    let set = DeviceSet(unsafe { SetupDiGetClassDevsW(std::ptr::null(), std::ptr::null(), std::ptr::null_mut(), DIGCF_ALLCLASSES) });
    let error = unsafe { GetLastError() };
    note(log, "SetupDiGetClassDevsW(all classes, present or not)", (set.0 != INVALID_SET, error)).then_some(set)
}

/// The devices of `set` listing the Sonar hardware id, with their instance ids.
fn sonar_devices_in(set: &DeviceSet) -> Vec<(DeviceInfoData, String)> {
    let mut found = Vec::new();
    for index in 0.. {
        let mut data = DeviceInfoData::new();
        if unsafe { SetupDiEnumDeviceInfo(set.0, index, &mut data) } == 0 {
            break;
        }
        if has_sonar_id(set.0, &mut data) {
            let name = instance_id(set.0, &mut data);
            found.push((data, name));
        }
    }
    found
}

fn instance_id(set: isize, data: &mut DeviceInfoData) -> String {
    let mut buffer = [0u16; 512];
    // One unit kept for the final NUL.
    if unsafe { SetupDiGetDeviceInstanceIdW(set, data, buffer.as_mut_ptr(), buffer.len() as u32 - 1, std::ptr::null_mut()) } != 0 {
        unsafe { read_wide(buffer.as_ptr()) }
    } else {
        format!("devnode {}", data.instance)
    }
}

fn has_sonar_id(set: isize, data: &mut DeviceInfoData) -> bool {
    let mut buffer = vec![0u8; 4096];
    let mut kind = 0u32;
    // The last four bytes stay zero: the list always ends.
    if unsafe { SetupDiGetDeviceRegistryPropertyW(set, data, SPDRP_HARDWAREID, &mut kind, buffer.as_mut_ptr(), buffer.len() as u32 - 4, std::ptr::null_mut()) } == 0 {
        return false;
    }
    let units: Vec<u16> = buffer.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    platinum::machine::multi_string(&units).iter().any(|id| id.eq_ignore_ascii_case(SONAR_HWID))
}

/// Installs the driver: stages the packages, creates the device if there is none, restarts
/// Windows Audio when something is new.
pub fn install(log: &mut Log, copy: &Path) -> Result<Report, String> {
    log.line("fallback install: Windows' own functions (newdev, SetupAPI, service manager)");
    let newdev = NewDev::load()?;
    let mut report = Report::default();
    for inf in install_order(copy) {
        let path = wide(&inf.display().to_string());
        let mut reboot = 0i32;
        let result = outcome(unsafe { (newdev.install_driver)(std::ptr::null_mut(), path.as_ptr(), 0, &mut reboot) });
        match staged(result) {
            Staged::Installed => {
                log.line(format!("  DiInstallDriverW({}): ok, installed in the driver store{}", inf.display(), if reboot != 0 { ", restart wanted" } else { "" }));
                report.packages_installed += 1;
            }
            Staged::Skipped => log.line(format!("  DiInstallDriverW({}): skipped, already in the driver store and current (error {})", inf.display(), error_text(ERROR_NO_MORE_ITEMS))),
            Staged::Failed(code) => {
                log.line(format!("  DiInstallDriverW({}): failed, error {}", inf.display(), error_text(code)));
                return Err(format!("{} could not be installed in the driver store: error {}", inf.display(), error_text(code)));
            }
        }
        report.reboot |= reboot != 0;
    }
    let existing: Vec<String> = match all_devices(log) {
        Some(set) => sonar_devices_in(&set).into_iter().map(|(_, name)| name).collect(),
        None => return Err("the devices cannot be listed".into()),
    };
    if existing.is_empty() {
        create_device(log, &newdev, &copy.join(VAD_PACKAGE).join(VAD_INF), &mut report)?;
    } else {
        log.line(format!("  device not created: {SONAR_HWID} already exists ({})", existing.join(", ")));
    }
    if report.restarts_audio() {
        log.line("  something new was installed: Windows Audio restarts in 7 s, as with SteelSeries' tool");
        std::thread::sleep(Duration::from_secs(7));
        let stopped = stop_audio(log);
        start_audio(log, &stopped);
    } else {
        log.line("  nothing new installed: Windows Audio not restarted");
    }
    log.line(format!(
        "fallback install done: {} package(s) installed, device {}, restart {}",
        report.packages_installed,
        if report.device_created { "created" } else { "already there" },
        if report.reboot { "requested by Windows" } else { "not needed" }
    ));
    Ok(report)
}

/// Creates the root device and gives it its driver; removes it again if no driver takes it.
fn create_device(log: &mut Log, newdev: &NewDev, vad_inf: &Path, report: &mut Report) -> Result<(), String> {
    let set = DeviceSet(unsafe { SetupDiCreateDeviceInfoList(&MEDIA_CLASS, std::ptr::null_mut()) });
    let error = unsafe { GetLastError() };
    if !note(log, "SetupDiCreateDeviceInfoList(MEDIA)", (set.0 != INVALID_SET, error)) {
        return Err(format!("no device information set: error {}", error_text(error)));
    }
    let mut data = DeviceInfoData::new();
    let name = wide("MEDIA");
    let result = outcome(unsafe { SetupDiCreateDeviceInfoW(set.0, name.as_ptr(), &MEDIA_CLASS, std::ptr::null(), std::ptr::null_mut(), DICD_GENERATE_ID, &mut data) });
    if !note(log, "SetupDiCreateDeviceInfoW(MEDIA, DICD_GENERATE_ID)", result) {
        return Err(format!("the device cannot be created: error {}", error_text(result.1)));
    }
    let hardware_id = multi_string(&[SONAR_HWID]);
    let result = outcome(unsafe { SetupDiSetDeviceRegistryPropertyW(set.0, &mut data, SPDRP_HARDWAREID, hardware_id.as_ptr(), hardware_id.len() as u32) });
    if !note(log, &format!("SetupDiSetDeviceRegistryPropertyW(SPDRP_HARDWAREID, {SONAR_HWID})"), result) {
        return Err(format!("the device's hardware id cannot be set: error {}", error_text(result.1)));
    }
    let manufacturer = single_string(MANUFACTURER);
    let result = outcome(unsafe { SetupDiSetDeviceRegistryPropertyW(set.0, &mut data, SPDRP_MFG, manufacturer.as_ptr(), manufacturer.len() as u32) });
    note(log, &format!("SetupDiSetDeviceRegistryPropertyW(SPDRP_MFG, {MANUFACTURER})"), result);
    let result = outcome(unsafe { SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set.0, &mut data) });
    if !note(log, "SetupDiCallClassInstaller(DIF_REGISTERDEVICE)", result) {
        return Err(format!("the device cannot be registered: error {}", error_text(result.1)));
    }
    let mut reboot = 0i32;
    let result = outcome(unsafe { (newdev.install_device)(std::ptr::null_mut(), set.0, &mut data, std::ptr::null_mut(), 0, &mut reboot) });
    let mut installed = note(log, "DiInstallDevice(best driver in the store)", result);
    if !installed {
        let (id, inf) = (wide(SONAR_HWID), wide(&vad_inf.display().to_string()));
        let result = outcome(unsafe { (newdev.update_driver)(std::ptr::null_mut(), id.as_ptr(), inf.as_ptr(), 0, &mut reboot) });
        installed = note(log, &format!("UpdateDriverForPlugAndPlayDevicesW({SONAR_HWID}, {})", vad_inf.display()), result);
    }
    report.reboot |= reboot != 0;
    if !installed {
        // Not left behind as an "Unknown device" (SteelSeries' tool leaves it).
        let result = outcome(unsafe { SetupDiCallClassInstaller(DIF_REMOVE, set.0, &mut data) });
        note(log, "SetupDiCallClassInstaller(DIF_REMOVE) of the device left without driver", result);
        return Err("no driver could be installed on the new device".into());
    }
    report.device_created = true;
    let name = instance_id(set.0, &mut data);
    log.line(format!("  device created: {name}{}", if reboot != 0 { ", restart wanted" } else { "" }));
    Ok(())
}

/// The DriverStore folder of the APO package: from its published INF (same catalog and version as
/// the copy's), else the folder the APO is registered from or the newest DriverStore copy.
fn apo_store_folder(log: &mut Log, copy: &Path) -> Option<PathBuf> {
    let inf = copy.join(APO_PACKAGE).join(APO_INF);
    let identity = identity_of(&inf)?;
    let version = (!identity.version.is_empty()).then_some(identity.version.as_str());
    let published = published_infs(&inf_folder(), &identity.catalog, version);
    log.line(format!("  published INFs with {} {}: {}", identity.catalog, identity.version, if published.is_empty() { "none".to_string() } else { names(&published) }));
    if let Some(published) = published.last() {
        let file = wide(&published.display().to_string());
        let mut buffer = [0u16; 520];
        let mut required = 0u32;
        let result = outcome(unsafe { SetupGetInfDriverStoreLocationW(file.as_ptr(), std::ptr::null(), std::ptr::null(), buffer.as_mut_ptr(), buffer.len() as u32 - 1, &mut required) });
        if note(log, &format!("SetupGetInfDriverStoreLocationW({})", published.display()), result) {
            let location = PathBuf::from(unsafe { read_wide(buffer.as_ptr()) });
            log.line(format!("  APO package in the driver store: {}", location.display()));
            if let Some(folder) = location.parent() {
                return Some(folder.to_path_buf());
            }
        }
    }
    let folder = platinum::sonarfiles::apo_package_folder();
    log.line(format!("  APO package folder found otherwise: {}", folder.as_ref().map_or("none".to_string(), |f| f.display().to_string())));
    folder
}

fn names(paths: &[PathBuf]) -> String {
    paths.iter().map(|p| p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())).collect::<Vec<_>>().join(", ")
}

/// regsvr32's arguments for one DLL.
pub fn regsvr32_arguments(dll: &Path, unregister: bool) -> String {
    format!("{}/s \"{}\"", if unregister { "/u " } else { "" }, dll.display())
}

fn regsvr32(log: &mut Log, folder: &Path, unregister: bool) -> Result<(), String> {
    let program = platinum::sonarfiles::windows_folder().join(r"System32\regsvr32.exe");
    let mut failures = Vec::new();
    for dll in ["Sonar.APO.dll", "Sonar.APOAPI.dll"] {
        match run_program(log, &program, &regsvr32_arguments(&folder.join(dll), unregister), None) {
            Ok(0) => {}
            Ok(code) => failures.push(format!("regsvr32 {dll} exited with {code}")),
            Err(error) => failures.push(error),
        }
    }
    if failures.is_empty() { Ok(()) } else { Err(failures.join("; ")) }
}

/// COM registration of the two APO DLLs of the DriverStore copy, as SteelSeries' `register` does.
pub fn register(log: &mut Log, copy: &Path) -> Result<(), String> {
    log.line("fallback register: regsvr32 on the APO DLLs of the DriverStore copy");
    let folder = apo_store_folder(log, copy).ok_or("the APO package's DriverStore folder was not found")?;
    regsvr32(log, &folder, false)
}

/// The reverse, before the packages are removed.
pub fn unregister(log: &mut Log, copy: &Path) -> Result<(), String> {
    log.line("fallback unregister: regsvr32 /u on the APO DLLs of the DriverStore copy");
    let folder = apo_store_folder(log, copy).ok_or("the APO package's DriverStore folder was not found")?;
    regsvr32(log, &folder, true)
}

/// Removes every device with the Sonar hardware id, then the three packages (any version), with
/// Windows Audio stopped meanwhile. Returns whether Windows wants a restart.
pub fn remove(log: &mut Log, copy: &Path) -> Result<bool, String> {
    log.line("fallback removal: Windows' own functions (newdev, SetupAPI, service manager)");
    let newdev = NewDev::load()?;
    let mut packages: Vec<PathBuf> = Vec::new();
    for inf in removal_order(copy) {
        match identity_of(&inf) {
            Some(identity) => {
                let found = published_infs(&inf_folder(), &identity.catalog, None);
                log.line(format!("  published INFs with {}: {}", identity.catalog, if found.is_empty() { "none".to_string() } else { names(&found) }));
                packages.extend(found);
            }
            None => log.line(format!("  {}: catalog unknown, skipped", inf.display())),
        }
    }
    let (mut reboot, mut failures) = (false, Vec::new());
    let stopped = stop_audio(log);
    match all_devices(log) {
        Some(set) => {
            let devices = sonar_devices_in(&set);
            if devices.is_empty() {
                log.line(format!("  no device with {SONAR_HWID}"));
            }
            for (mut data, name) in devices {
                let mut wants = 0i32;
                let result = outcome(unsafe { (newdev.uninstall_device)(std::ptr::null_mut(), set.0, &mut data, 0, &mut wants) });
                if !note(log, &format!("DiUninstallDevice({name})"), result) {
                    failures.push(format!("device {name} not removed: error {}", error_text(result.1)));
                }
                reboot |= wants != 0;
            }
        }
        None => failures.push("the devices cannot be listed".to_string()),
    }
    for package in packages {
        let path = wide(&package.display().to_string());
        let mut wants = 0i32;
        let result = outcome(unsafe { (newdev.uninstall_driver)(std::ptr::null_mut(), path.as_ptr(), 0, &mut wants) });
        if !note(log, &format!("DiUninstallDriverW({})", package.display()), result) {
            failures.push(format!("{} not removed: error {}", package.display(), error_text(result.1)));
        }
        reboot |= wants != 0;
    }
    start_audio(log, &stopped);
    log.line(format!("fallback removal done: {} problem(s), restart {}", failures.len(), if reboot { "requested by Windows" } else { "not needed" }));
    if failures.is_empty() { Ok(reboot) } else { Err(failures.join("; ")) }
}

/// What `stop_audio` stopped: Windows Audio, and the running services that depend on it.
struct Stopped {
    audio: bool,
    dependents: Vec<String>,
}

const AUDIO: &str = "AudioSrv";
const SC_MANAGER_CONNECT: u32 = 0x1;
const SERVICE_QUERY_STATUS: u32 = 0x4;
const SERVICE_ENUMERATE_DEPENDENTS: u32 = 0x8;
const SERVICE_START: u32 = 0x10;
const SERVICE_STOP: u32 = 0x20;
const SERVICE_ACTIVE: u32 = 0x1;
const SERVICE_CONTROL_STOP: u32 = 0x1;
const SERVICE_STOPPED: u32 = 1;
const SERVICE_RUNNING: u32 = 4;

/// A service manager or service handle, closed on drop.
struct ServiceHandle(*mut c_void);

impl Drop for ServiceHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseServiceHandle(self.0) };
        }
    }
}

fn service_manager(log: &mut Log) -> Option<ServiceHandle> {
    let manager = ServiceHandle(unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT) });
    let error = unsafe { GetLastError() };
    if manager.0.is_null() {
        note(log, "OpenSCManagerW", (false, error));
        return None;
    }
    Some(manager)
}

fn open_service(log: &mut Log, manager: &ServiceHandle, name: &str, access: u32) -> Option<ServiceHandle> {
    let wide_name = wide(name);
    let service = ServiceHandle(unsafe { OpenServiceW(manager.0, wide_name.as_ptr(), access) });
    let error = unsafe { GetLastError() };
    if service.0.is_null() {
        note(log, &format!("OpenServiceW({name})"), (false, error));
        return None;
    }
    Some(service)
}

fn wait_for(service: &ServiceHandle, state: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let mut status = ServiceStatus::default();
        if unsafe { QueryServiceStatus(service.0, &mut status) } == 0 {
            return false;
        }
        if status.state == state {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Stops one service; true when it was running and was told to stop.
fn stop_one(log: &mut Log, manager: &ServiceHandle, name: &str) -> bool {
    let Some(service) = open_service(log, manager, name, SERVICE_STOP | SERVICE_QUERY_STATUS) else { return false };
    let mut status = ServiceStatus::default();
    let result = outcome(unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) });
    if result.1 == ERROR_SERVICE_NOT_ACTIVE {
        log.line(format!("  ControlService({name}, stop): not running"));
        return false;
    }
    if !note(log, &format!("ControlService({name}, stop)"), result) {
        return false;
    }
    let stopped = wait_for(&service, SERVICE_STOPPED, Duration::from_secs(30));
    log.line(format!("  {name}: {}", if stopped { "stopped" } else { "still not stopped after 30 s" }));
    true
}

fn start_one(log: &mut Log, manager: &ServiceHandle, name: &str) {
    let Some(service) = open_service(log, manager, name, SERVICE_START | SERVICE_QUERY_STATUS) else { return };
    let result = outcome(unsafe { StartServiceW(service.0, 0, std::ptr::null()) });
    if result.1 == ERROR_SERVICE_ALREADY_RUNNING {
        log.line(format!("  StartServiceW({name}): already running"));
        return;
    }
    if note(log, &format!("StartServiceW({name})"), result) {
        let running = wait_for(&service, SERVICE_RUNNING, Duration::from_secs(30));
        log.line(format!("  {name}: {}", if running { "running" } else { "still not running after 30 s" }));
    }
}

/// The running services that depend on Windows Audio.
fn audio_dependents(log: &mut Log, manager: &ServiceHandle) -> Vec<String> {
    let Some(audio) = open_service(log, manager, AUDIO, SERVICE_ENUMERATE_DEPENDENTS) else { return Vec::new() };
    // u64 for the alignment of the ENUM_SERVICE_STATUSW entries at its start.
    let mut buffer = vec![0u64; 8 * 1024];
    let (mut needed, mut count) = (0u32, 0u32);
    let result = outcome(unsafe { EnumDependentServicesW(audio.0, SERVICE_ACTIVE, buffer.as_mut_ptr().cast(), (buffer.len() * 8) as u32, &mut needed, &mut count) });
    if !result.0 {
        note(log, "EnumDependentServicesW(AudioSrv, active)", result);
        return Vec::new();
    }
    let room = buffer.len() * 8 / std::mem::size_of::<EnumServiceStatus>();
    let entries = buffer.as_ptr().cast::<EnumServiceStatus>();
    let names: Vec<String> = (0..(count as usize).min(room)).map(|i| unsafe { read_wide((*entries.add(i)).name) }).filter(|name| !name.is_empty()).collect();
    log.line(format!("  running services depending on AudioSrv: {}", if names.is_empty() { "none".to_string() } else { names.join(", ") }));
    names
}

/// Stops Windows Audio, its running dependents first: all sound stops.
fn stop_audio(log: &mut Log) -> Stopped {
    log.line("  stopping Windows Audio: all sound stops for a few seconds");
    let Some(manager) = service_manager(log) else { return Stopped { audio: false, dependents: Vec::new() } };
    let dependents: Vec<String> = audio_dependents(log, &manager).into_iter().filter(|name| stop_one(log, &manager, name)).collect();
    let audio = stop_one(log, &manager, AUDIO);
    Stopped { audio, dependents }
}

/// Starts Windows Audio (even if it was not stopped here: it must run after a driver change),
/// then the dependents that were stopped.
fn start_audio(log: &mut Log, stopped: &Stopped) {
    let Some(manager) = service_manager(log) else { return };
    if !stopped.audio {
        log.line("  Windows Audio had not been stopped here; starting it anyway");
    }
    start_one(log, &manager, AUDIO);
    for name in &stopped.dependents {
        start_one(log, &manager, name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hardware_id_is_a_double_nul_terminated_multi_string() {
        let bytes = multi_string(&[SONAR_HWID]);
        assert_eq!(bytes.len(), (SONAR_HWID.len() + 2) * 2);
        assert_eq!(&bytes[..4], &[b'R', 0, b'O', 0]);
        assert_eq!(&bytes[bytes.len() - 4..], &[0, 0, 0, 0]);
        let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(platinum::machine::multi_string(&units), [SONAR_HWID]);
        assert_eq!(multi_string(&["A", "B"]), [b'A', 0, 0, 0, b'B', 0, 0, 0, 0, 0]);
        assert_eq!(single_string("AB"), [b'A', 0, b'B', 0, 0, 0]);
    }

    #[test]
    fn packages_go_in_ggs_order_and_come_out_in_its_reverse() {
        let copy = Path::new(r"C:\ProgramData\Platinum\sonar");
        let names = |list: [PathBuf; 3]| list.map(|p| p.file_name().unwrap().to_string_lossy().into_owned());
        assert_eq!(names(install_order(copy)), [VAD_INF, APO_INF, EXTENSION_INF]);
        assert_eq!(names(removal_order(copy)), [EXTENSION_INF, VAD_INF, APO_INF]);
        assert!(install_order(copy)[1].ends_with(r"apps\sonar\driver\apoDriverPackage\Sonar.Apo.inf"));
    }

    #[test]
    fn results_are_read_as_steelseries_tool_reads_them() {
        assert_eq!(staged((true, 0)), Staged::Installed);
        assert_eq!(staged((false, ERROR_NO_MORE_ITEMS)), Staged::Skipped);
        assert_eq!(staged((false, 0xE000_0247)), Staged::Failed(0xE000_0247));
        assert!(!needs_fallback(0) && !needs_fallback(1));
        assert!(needs_fallback(-1) && needs_fallback(2) && needs_fallback(25));
        assert!(!Report::default().restarts_audio());
        assert!(Report { packages_installed: 1, ..Report::default() }.restarts_audio());
        assert!(Report { device_created: true, ..Report::default() }.restarts_audio());
        assert!(!Report { reboot: true, ..Report::default() }.restarts_audio());
    }

    #[test]
    fn errors_are_named() {
        assert_eq!(error_text(0xE000_0203), "0xE0000203 (ERROR_NO_DRIVER_SELECTED)");
        assert_eq!(error_text(0xE000_0247), "0xE0000247 (ERROR_DRIVER_STORE_ADD_FAILED)");
        assert!(error_text(5).starts_with("0x00000005 (5: "), "{}", error_text(5));
        assert!(error_text(0x8000_4002).starts_with("0x80004002 ("), "{}", error_text(0x8000_4002));
        assert_eq!(error_text(0xE000_FFFF), "0xE000FFFF (3758161919)");
    }

    #[test]
    fn regsvr32_is_told_to_be_silent() {
        let dll = Path::new(r"C:\Windows\System32\DriverStore\FileRepository\sonar.apo.inf_amd64_x\Sonar.APO.dll");
        assert_eq!(regsvr32_arguments(dll, false), r#"/s "C:\Windows\System32\DriverStore\FileRepository\sonar.apo.inf_amd64_x\Sonar.APO.dll""#);
        assert_eq!(regsvr32_arguments(dll, true), format!("/u {}", regsvr32_arguments(dll, false)));
    }

    const SYNTHETIC: &str = "; synthetic\r\n[Version]\r\nSignature   = \"$WINDOWS NT$\"\r\nClass       = AudioProcessingObject\r\nDriverVer = 09/09/2026, 3.9.0.0 ; date, version\r\nCatalogFile = Sonar.APO.cat\r\n\r\n[Strings]\r\nCatalogFile = other.cat\r\nDriverVer = 1\r\n";

    #[test]
    fn inf_identity_reads_the_version_section_only() {
        let expected = InfIdentity { catalog: "Sonar.APO.cat".into(), version: "09/09/2026,3.9.0.0".into() };
        assert_eq!(inf_identity(SYNTHETIC.as_bytes()), Some(expected.clone()));
        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend(SYNTHETIC.encode_utf16().flat_map(|u| u.to_le_bytes()));
        assert_eq!(inf_identity(&utf16), Some(expected));
        assert_eq!(inf_identity(b"[Version]\nCatalogFile.NTamd64=a.cat\n").map(|i| i.catalog), Some("a.cat".to_string()));
        assert_eq!(inf_identity(b"[Version]\nClass=MEDIA\n"), None);
        assert_eq!(inf_identity(b""), None);
    }

    #[test]
    fn known_catalogs_cover_the_three_packages() {
        let copy = Path::new(r"C:\x");
        assert!(install_order(copy).iter().all(|inf| known_catalog(inf).is_some()));
        assert_eq!(known_catalog(Path::new(r"C:\x\SONAR.APO.INF")), Some("Sonar.APO.cat"));
        assert_eq!(known_catalog(Path::new(r"C:\x\other.inf")), None);
    }

    #[test]
    fn published_infs_are_found_by_catalog_and_version() {
        let folder = std::env::temp_dir().join(format!("platinum-setup-oem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        let older = SYNTHETIC.replace("3.9.0.0", "3.8.0.0");
        for (name, text) in [("oem12.inf", SYNTHETIC), ("oem7.inf", older.as_str()), ("oem3.inf", "[Version]\nCatalogFile=x.cat\n"), ("sonar.inf", SYNTHETIC), ("oemA.inf", SYNTHETIC)] {
            std::fs::write(folder.join(name), text).unwrap();
        }
        let found = |version| published_infs(&folder, "sonar.apo.CAT", version).iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect::<Vec<_>>();
        assert_eq!(found(None), ["oem7.inf", "oem12.inf"]);
        assert_eq!(found(Some("09/09/2026,3.9.0.0")), ["oem12.inf"]);
        assert!(found(Some("1")).is_empty());
        assert!(published_infs(&folder.join("missing"), "x.cat", None).is_empty());
        let _ = std::fs::remove_dir_all(&folder);
        assert_eq!(oem_number("OEM42.INF"), Some(42));
        assert_eq!(oem_number("oem.inf"), None);
        assert_eq!(oem_number("oem4.inf.bak"), None);
    }

    /// Read-only, on a machine where GG installed the driver: the published INFs and the DriverStore
    /// folder are found the way `register` and `remove` will find them, and the device list matches
    /// Platinum's. Skipped elsewhere.
    #[test]
    fn lookups_match_the_installed_driver_when_gg_is_here() {
        let gg = platinum::sonarfiles::gg_folder();
        if !gg.join(APO_PACKAGE).join(APO_INF).is_file() || platinum::machine::sonar_devices().is_empty() {
            eprintln!("skipped: GG's driver is not installed here");
            return;
        }
        let mut log = Log::none();
        for inf in install_order(&gg) {
            let identity = identity_of(&inf).unwrap();
            assert!(!published_infs(&inf_folder(), &identity.catalog, Some(&identity.version)).is_empty(), "{}", inf.display());
        }
        let folder = apo_store_folder(&mut log, &gg).unwrap();
        assert!(folder.join("Sonar.APO.dll").is_file() && folder.join("Sonar.APOAPI.dll").is_file(), "{}", folder.display());
        let set = all_devices(&mut log).unwrap();
        let mut ours: Vec<String> = sonar_devices_in(&set).into_iter().map(|(_, name)| name.to_ascii_uppercase()).collect();
        let mut platinum: Vec<String> = platinum::machine::sonar_devices().into_iter().map(|d| d.instance.to_ascii_uppercase()).collect();
        ours.sort();
        platinum.sort();
        assert_eq!(ours, platinum);
    }
}
