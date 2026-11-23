//! Per-application routing: which audio device a given process plays to or records from.
//!
//! Windows keeps this preference itself (it is what the "App volume and device preferences" page
//! edits) and exposes it through an internal WinRT class. The class is not documented; its layout
//! is taken as observed on Windows 10 and 11, with a different interface identifier before 21H2.

use crate::com::{Com, Error, Guid, Ole, Result, check, export, load_library, read_wide, wide};
use crate::devices::Flow;
use std::ffi::c_void;

const CLASS_NAME: &str = "Windows.Media.Internal.AudioPolicyConfig";
const IID_21H2: Guid = Guid { d1: 0xAB3D_4648, d2: 0xE242, d3: 0x459F, d4: [0xB0, 0x2F, 0x54, 0x1C, 0x70, 0x30, 0x63, 0x24] };
const IID_DOWNLEVEL: Guid = Guid { d1: 0x2A59_116D, d2: 0x6C4F, d3: 0x45E0, d4: [0xA7, 0x4F, 0x70, 0x7E, 0x3F, 0xEF, 0x92, 0x58] };

// Vtable positions: IUnknown (3), IInspectable (3), then 19 unrelated methods.
const SET_PERSISTED: usize = 3 + 22;
const GET_PERSISTED: usize = 3 + 23;

// The preference is stored under a device interface path built around the endpoint identifier.
const PATH_PREFIX: &str = r"\\?\SWD#MMDEVAPI#";
const RENDER_SUFFIX: &str = "#{e6327cad-dcec-4949-ae8a-991e976a79d2}";
const CAPTURE_SUFFIX: &str = "#{2eef81be-33fa-4800-9670-1cd474972c3f}";

const ROLE_CONSOLE: i32 = 0;
const ROLE_MULTIMEDIA: i32 = 1;

type Hstring = *mut c_void;

pub struct Routing {
    policy: Com,
    create_string: unsafe extern "system" fn(*const u16, u32, *mut Hstring) -> i32,
    delete_string: unsafe extern "system" fn(Hstring) -> i32,
    string_buffer: unsafe extern "system" fn(Hstring, *mut u32) -> *const u16,
}

impl Routing {
    pub fn open() -> Result<Routing> {
        Ole::load()?;
        let module = load_library("combase.dll", 0)?;
        unsafe {
            let create_string: unsafe extern "system" fn(*const u16, u32, *mut Hstring) -> i32 = std::mem::transmute(export(module, b"WindowsCreateString\0")?);
            let delete_string: unsafe extern "system" fn(Hstring) -> i32 = std::mem::transmute(export(module, b"WindowsDeleteString\0")?);
            let string_buffer = std::mem::transmute(export(module, b"WindowsGetStringRawBuffer\0")?);
            let get_factory: unsafe extern "system" fn(Hstring, *const Guid, *mut *mut c_void) -> i32 = std::mem::transmute(export(module, b"RoGetActivationFactory\0")?);

            let name = wide(CLASS_NAME);
            let mut class = std::ptr::null_mut();
            check("WindowsCreateString", create_string(name.as_ptr(), (name.len() - 1) as u32, &mut class))?;
            let mut raw = std::ptr::null_mut();
            let mut hresult = get_factory(class, &IID_21H2, &mut raw);
            if hresult < 0 {
                hresult = get_factory(class, &IID_DOWNLEVEL, &mut raw);
            }
            delete_string(class);
            check("getting the Windows audio policy class", hresult)?;
            Ok(Routing { policy: Com(raw), create_string, delete_string, string_buffer })
        }
    }

    /// Device a process is routed to, or `None` when it follows the default device.
    pub fn get(&self, pid: u32, flow: Flow) -> Result<Option<String>> {
        unsafe {
            let call: unsafe extern "system" fn(*mut c_void, u32, i32, i32, *mut Hstring) -> i32 = std::mem::transmute(self.policy.entry(GET_PERSISTED));
            let mut path: Hstring = std::ptr::null_mut();
            check("GetPersistedDefaultAudioEndpoint", call(self.policy.0, pid, flow as i32, ROLE_MULTIMEDIA, &mut path))?;
            if path.is_null() {
                return Ok(None);
            }
            let mut len = 0;
            let text = read_wide((self.string_buffer)(path, &mut len));
            (self.delete_string)(path);
            let id = text.strip_prefix(PATH_PREFIX).unwrap_or(&text);
            let id = id.strip_suffix(RENDER_SUFFIX).or_else(|| id.strip_suffix(CAPTURE_SUFFIX)).unwrap_or(id);
            Ok(if id.is_empty() { None } else { Some(id.to_string()) })
        }
    }

    /// Routes a process to a device, or back to the default device when `device_id` is `None`.
    /// A stream that is already playing moves at once.
    pub fn set(&self, pid: u32, flow: Flow, device_id: Option<&str>) -> Result<()> {
        unsafe {
            let mut path: Hstring = std::ptr::null_mut();
            if let Some(id) = device_id {
                let suffix = if flow == Flow::Render { RENDER_SUFFIX } else { CAPTURE_SUFFIX };
                let text = wide(&format!("{PATH_PREFIX}{id}{suffix}"));
                check("WindowsCreateString", (self.create_string)(text.as_ptr(), (text.len() - 1) as u32, &mut path))?;
            }
            let call: unsafe extern "system" fn(*mut c_void, u32, i32, i32, Hstring) -> i32 = std::mem::transmute(self.policy.entry(SET_PERSISTED));
            let mut result = Ok(());
            for role in [ROLE_MULTIMEDIA, ROLE_CONSOLE] {
                result = result.and(check("SetPersistedDefaultAudioEndpoint", call(self.policy.0, pid, flow as i32, role, path)));
            }
            if !path.is_null() {
                (self.delete_string)(path);
            }
            result
        }
    }
}

#[repr(C)]
struct ProcessEntry {
    size: u32,
    usage: u32,
    pid: u32,
    heap: usize,
    module: u32,
    threads: u32,
    parent: u32,
    priority: i32,
    flags: u32,
    exe: [u16; 260],
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> *mut c_void;
    fn Process32FirstW(snapshot: *mut c_void, entry: *mut ProcessEntry) -> i32;
    fn Process32NextW(snapshot: *mut c_void, entry: *mut ProcessEntry) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

/// Every running process, as (identifier, executable name).
pub fn all_processes() -> Result<Vec<(u32, String)>> {
    Ok(process_table()?.into_iter().map(|(pid, exe, _)| (pid, exe)).collect())
}

/// Every running process, as (identifier, executable name, number of threads).
pub fn process_table() -> Result<Vec<(u32, String, u32)>> {
    let mut found = Vec::new();
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(2, 0); // 2 = processes
        if snapshot as isize == -1 {
            return Err(Error::Invalid("cannot list the running processes".into()));
        }
        let mut entry: ProcessEntry = std::mem::zeroed();
        entry.size = std::mem::size_of::<ProcessEntry>() as u32;
        let mut more = Process32FirstW(snapshot, &mut entry);
        while more != 0 {
            found.push((entry.pid, read_wide(entry.exe.as_ptr()), entry.threads));
            more = Process32NextW(snapshot, &mut entry);
        }
        CloseHandle(snapshot);
    }
    Ok(found)
}

/// Running processes matching `app`: a process identifier, or an executable name with or without `.exe`.
pub fn processes(app: &str) -> Result<Vec<(u32, String)>> {
    let wanted_pid = app.parse::<u32>().ok();
    let wanted_name = app.to_lowercase();
    let found: Vec<(u32, String)> = all_processes()?
        .into_iter()
        .filter(|(pid, exe)| {
            let lower = exe.to_lowercase();
            wanted_pid == Some(*pid) || lower == wanted_name || lower.strip_suffix(".exe") == Some(wanted_name.as_str())
        })
        .collect();
    if found.is_empty() { Err(Error::Invalid(format!("no running process matches \"{app}\""))) } else { Ok(found) }
}
