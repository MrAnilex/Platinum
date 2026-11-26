//! Restarting Windows' audio service when it no longer answers (`platinum audio-restart`).
//!
//! When the service (Audiosrv) is frozen, every program's sound is stuck, and asking it to stop
//! politely may hang too. What brought the sound back every time (26/11/2026, and about twenty
//! times under GG since August) was ending its process. So the command ends the service's own
//! process, then starts the service again at once, without waiting for the minute Windows leaves
//! before its own restart. It refuses to end a process the service shares with others.
//!
//! It needs administrator rights. The window runs it through Windows' elevation prompt
//! (`run_elevated`), and its lines go to the daemon's log (`--log`), since the elevated process has
//! no console to show.

use crate::com::wide;
use crate::i18n::tr;
use crate::trf;
use std::ffi::c_void;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The command, for the window and the help.
pub const SUBCOMMAND: &str = "audio-restart";
const SERVICE: &str = "Audiosrv";
/// How long the ended process may take to go, then the service to be seen stopped, then started.
const PROCESS_WAIT: Duration = Duration::from_secs(5);
const STOPPED_WAIT: Duration = Duration::from_secs(10);
const RUNNING_WAIT: Duration = Duration::from_secs(20);

#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenSCManagerW(machine: *const u16, database: *const u16, access: u32) -> *mut c_void;
    fn OpenServiceW(manager: *mut c_void, name: *const u16, access: u32) -> *mut c_void;
    fn QueryServiceStatusEx(service: *mut c_void, level: i32, buffer: *mut u8, size: u32, needed: *mut u32) -> i32;
    fn StartServiceW(service: *mut c_void, count: u32, arguments: *const *const u16) -> i32;
    fn CloseServiceHandle(handle: *mut c_void) -> i32;
    fn OpenProcessToken(process: *mut c_void, access: u32, token: *mut *mut c_void) -> i32;
    fn LookupPrivilegeValueW(system: *const u16, name: *const u16, luid: *mut Luid) -> i32;
    fn AdjustTokenPrivileges(token: *mut c_void, disable_all: i32, state: *const TokenPrivileges, length: u32, previous: *mut c_void, returned: *mut u32) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
    fn TerminateProcess(process: *mut c_void, code: u32) -> i32;
    fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    fn GetExitCodeProcess(process: *mut c_void, code: *mut u32) -> i32;
    fn GetCurrentProcess() -> *mut c_void;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn GetLastError() -> u32;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn ShellExecuteExW(info: *mut ShellExecuteInfo) -> i32;
}

const SC_MANAGER_CONNECT: u32 = 0x0001;
const SERVICE_QUERY_STATUS: u32 = 0x0004;
const SERVICE_START: u32 = 0x0010;
const SC_STATUS_PROCESS_INFO: i32 = 0;
const SERVICE_WIN32_OWN_PROCESS: u32 = 0x10;
const SERVICE_STOPPED: u32 = 1;
const SERVICE_RUNNING: u32 = 4;
const PROCESS_TERMINATE: u32 = 0x0001;
const SYNCHRONIZE: u32 = 0x0010_0000;
const TOKEN_ADJUST_PRIVILEGES: u32 = 0x0020;
const TOKEN_QUERY: u32 = 0x0008;
const SE_PRIVILEGE_ENABLED: u32 = 0x0002;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_SERVICE_ALREADY_RUNNING: u32 = 1056;
const ERROR_CANCELLED: u32 = 1223;
const WAIT_TIMEOUT: u32 = 0x102;

#[repr(C)]
#[derive(Default)]
struct ServiceStatusProcess {
    service_type: u32,
    current_state: u32,
    controls_accepted: u32,
    win32_exit_code: u32,
    service_exit_code: u32,
    check_point: u32,
    wait_hint: u32,
    process_id: u32,
    service_flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Luid {
    low: u32,
    high: i32,
}

#[repr(C)]
struct TokenPrivileges {
    count: u32,
    luid: Luid,
    attributes: u32,
}

#[repr(C)]
struct ShellExecuteInfo {
    size: u32,
    mask: u32,
    window: *mut c_void,
    verb: *const u16,
    file: *const u16,
    parameters: *const u16,
    directory: *const u16,
    show: i32,
    instance: *mut c_void,
    id_list: *mut c_void,
    class: *const u16,
    class_key: *mut c_void,
    hot_key: u32,
    icon: *mut c_void,
    process: *mut c_void,
}

/// A service manager or service handle, closed when dropped.
struct ServiceHandle(*mut c_void);

impl Drop for ServiceHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseServiceHandle(self.0) };
        }
    }
}

/// A process handle, closed when dropped.
struct ProcessHandle(*mut c_void);

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// The service's state as the command needs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Seen {
    state: u32,
    pid: u32,
    /// The service runs in a process of its own (it does on Windows 10 and 11).
    own_process: bool,
}

/// What to do with the service as it is found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Plan {
    /// Not running: only start it.
    Start,
    /// End its process, then start it.
    End(u32),
    /// Its process holds other services too: ending it would take them down as well.
    Refuse(u32),
}

fn plan(seen: Seen) -> Plan {
    match (seen.state, seen.pid) {
        (SERVICE_STOPPED, _) | (_, 0) => Plan::Start,
        (_, pid) if seen.own_process => Plan::End(pid),
        (_, pid) => Plan::Refuse(pid),
    }
}

fn state_name(state: u32) -> &'static str {
    match state {
        1 => "stopped",
        2 => "starting",
        3 => "stopping",
        4 => "running",
        5 => "resuming",
        6 => "pausing",
        7 => "paused",
        _ => "in an unknown state",
    }
}

/// `platinum audio-restart [--log <file>]`: ends Windows' audio service and starts it again,
/// printing each step, and adding it to `log` when given.
pub fn run(log: Option<&Path>) -> Result<(), String> {
    let mut say = |line: &str| {
        println!("{line}");
        if let Some(file) = log {
            if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(file) {
                let _ = writeln!(file, "[{}] audio-restart: {line}", crate::guard::stamp());
            }
        }
    };
    let result = restart(&mut say);
    if let Err(error) = &result {
        say(&format!("failed: {error}"));
    }
    result
}

fn restart(say: &mut dyn FnMut(&str)) -> Result<(), String> {
    let manager = ServiceHandle(unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT) });
    if manager.0.is_null() {
        return Err(format!("cannot reach the service manager (Windows error {})", unsafe { GetLastError() }));
    }
    let service = ServiceHandle(unsafe { OpenServiceW(manager.0, wide(SERVICE).as_ptr(), SERVICE_QUERY_STATUS | SERVICE_START) });
    if service.0.is_null() {
        let error = unsafe { GetLastError() };
        return Err(if error == ERROR_ACCESS_DENIED { "administrator rights are needed to restart Windows' audio service".into() } else { format!("cannot open the {SERVICE} service (Windows error {error})") });
    }
    let seen = query(&service)?;
    say(&format!("{SERVICE} is {}, process {}{}", state_name(seen.state), seen.pid, if seen.own_process { " (its own)" } else { " (shared with other services)" }));
    match plan(seen) {
        Plan::Refuse(pid) => return Err(format!("process {pid} also holds other services: Platinum does not end it")),
        Plan::Start => {}
        Plan::End(pid) => {
            if end_process(pid, || query(&service).is_ok_and(|seen| seen.pid == pid))? {
                say(&format!("process {pid} ended"));
                let stopped = wait_for(&service, STOPPED_WAIT, |seen| seen.state == SERVICE_STOPPED || seen.pid != pid);
                say(&format!("{SERVICE} is {}", stopped.map_or("not seen stopped yet", |seen| state_name(seen.state))));
            } else {
                say(&format!("process {pid} no longer holds {SERVICE} (Windows restarted it meanwhile): left alone"));
            }
        }
    }
    if unsafe { StartServiceW(service.0, 0, std::ptr::null()) } == 0 {
        let error = unsafe { GetLastError() };
        // Windows' own recovery may have been quicker.
        if error != ERROR_SERVICE_ALREADY_RUNNING {
            say(&format!("starting {SERVICE} failed (Windows error {error}); waiting for Windows' own restart"));
        }
    }
    match wait_for(&service, RUNNING_WAIT, |seen| seen.state == SERVICE_RUNNING) {
        Some(seen) => {
            say(&format!("{SERVICE} runs again, process {}", seen.pid));
            Ok(())
        }
        None => Err(format!("{SERVICE} did not start again within {} s", RUNNING_WAIT.as_secs())),
    }
}

fn query(service: &ServiceHandle) -> Result<Seen, String> {
    let mut status = ServiceStatusProcess::default();
    let mut needed = 0u32;
    let size = std::mem::size_of::<ServiceStatusProcess>() as u32;
    if unsafe { QueryServiceStatusEx(service.0, SC_STATUS_PROCESS_INFO, (&mut status as *mut ServiceStatusProcess).cast(), size, &mut needed) } == 0 {
        return Err(format!("cannot read the state of {SERVICE} (Windows error {})", unsafe { GetLastError() }));
    }
    Ok(Seen { state: status.current_state, pid: status.process_id, own_process: status.service_type & SERVICE_WIN32_OWN_PROCESS != 0 })
}

/// The service once `done` holds, looked at every 200 ms for at most `limit`.
fn wait_for(service: &ServiceHandle, limit: Duration, done: impl Fn(Seen) -> bool) -> Option<Seen> {
    let started = Instant::now();
    loop {
        if let Ok(seen) = query(service) {
            if done(seen) {
                return Some(seen);
            }
        }
        if started.elapsed() >= limit {
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Ends process `pid` and waits for it to go; false when `still_the_service` says, once the
/// process is open, that it no longer holds the service (left alone then). The open handle keeps
/// Windows from giving the number to another process, so the check holds until the end. A service
/// process may only be opened with the debug privilege, which administrators hold but do not have
/// switched on.
fn end_process(pid: u32, still_the_service: impl Fn() -> bool) -> Result<bool, String> {
    let mut process = ProcessHandle(unsafe { OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE, 0, pid) });
    if process.0.is_null() && unsafe { GetLastError() } == ERROR_ACCESS_DENIED && enable_debug_privilege() {
        process = ProcessHandle(unsafe { OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE, 0, pid) });
    }
    if process.0.is_null() {
        return Err(format!("cannot open process {pid} to end it (Windows error {})", unsafe { GetLastError() }));
    }
    if !still_the_service() {
        return Ok(false);
    }
    if unsafe { TerminateProcess(process.0, 1) } == 0 {
        return Err(format!("cannot end process {pid} (Windows error {})", unsafe { GetLastError() }));
    }
    if unsafe { WaitForSingleObject(process.0, PROCESS_WAIT.as_millis() as u32) } == WAIT_TIMEOUT {
        return Err(format!("process {pid} did not end within {} s", PROCESS_WAIT.as_secs()));
    }
    Ok(true)
}

/// Switches the debug privilege on for this process; true when it is on.
fn enable_debug_privilege() -> bool {
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let token = ProcessHandle(token);
        let mut luid = Luid::default();
        if LookupPrivilegeValueW(std::ptr::null(), wide("SeDebugPrivilege").as_ptr(), &mut luid) == 0 {
            return false;
        }
        let privileges = TokenPrivileges { count: 1, luid, attributes: SE_PRIVILEGE_ENABLED };
        // It succeeds without granting a privilege the account lacks: the error then says so.
        AdjustTokenPrivileges(token.0, 0, &privileges, 0, std::ptr::null_mut(), std::ptr::null_mut()) != 0 && GetLastError() == 0
    }
}

/// The command-line program that carries the command: `platinum.exe` next to the running
/// program, else in the install folder.
pub fn program() -> Option<PathBuf> {
    let here = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|folder| folder.join("platinum.exe")));
    let installed = crate::autostart::install_dir().ok().map(|folder| folder.join("platinum.exe"));
    [here, installed].into_iter().flatten().find(|path| path.exists())
}

/// Runs `platinum audio-restart --log <daemon.log next to it>` as administrator (Windows asks the
/// user), hidden, and waits for it. Error in the current language.
pub fn run_elevated() -> Result<(), String> {
    let program = program().ok_or_else(|| tr("platinum.exe est introuvable").to_string())?;
    let log = program.with_file_name("daemon.log");
    let parameters = format!("{SUBCOMMAND} --log \"{}\"", log.display());
    let (verb, file, parameters) = (wide("runas"), wide(&program.display().to_string()), wide(&parameters));
    let mut info = ShellExecuteInfo {
        size: std::mem::size_of::<ShellExecuteInfo>() as u32,
        // SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC
        mask: 0x40 | 0x100,
        window: std::ptr::null_mut(),
        verb: verb.as_ptr(),
        file: file.as_ptr(),
        parameters: parameters.as_ptr(),
        directory: std::ptr::null(),
        // SW_HIDE
        show: 0,
        instance: std::ptr::null_mut(),
        id_list: std::ptr::null_mut(),
        class: std::ptr::null(),
        class_key: std::ptr::null_mut(),
        hot_key: 0,
        icon: std::ptr::null_mut(),
        process: std::ptr::null_mut(),
    };
    unsafe {
        if ShellExecuteExW(&mut info) == 0 {
            let error = GetLastError();
            return Err(if error == ERROR_CANCELLED { tr("relance annulée : Windows n'a pas reçu l'accord administrateur").to_string() } else { trf!("la relance n'a pas pu être lancée (erreur Windows {code})", code = error) });
        }
        if info.process.is_null() {
            return Err(tr("la relance n'a pas pu être suivie").into());
        }
        let process = ProcessHandle(info.process);
        let limit = PROCESS_WAIT + STOPPED_WAIT + RUNNING_WAIT + Duration::from_secs(10);
        if WaitForSingleObject(process.0, limit.as_millis() as u32) == WAIT_TIMEOUT {
            return Err(tr("la relance du service Audio ne s'est pas terminée").into());
        }
        let mut code = 0u32;
        if GetExitCodeProcess(process.0, &mut code) == 0 {
            return Err(tr("la relance n'a pas pu être suivie").into());
        }
        if code == 0 { Ok(()) } else { Err(trf!("la relance du service Audio a échoué (détails dans {file})", file = log.display().to_string())) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_process_of_its_own_is_ended() {
        let running = |pid, own_process| Seen { state: SERVICE_RUNNING, pid, own_process };
        assert_eq!(plan(running(24_672, true)), Plan::End(24_672));
        assert_eq!(plan(running(1692, false)), Plan::Refuse(1692));
        // Stopped, or between two processes: only started.
        assert_eq!(plan(Seen { state: SERVICE_STOPPED, pid: 0, own_process: true }), Plan::Start);
        assert_eq!(plan(Seen { state: 2, pid: 0, own_process: true }), Plan::Start);
        // Stopping but stuck: its process is ended all the same.
        assert_eq!(plan(Seen { state: 3, pid: 88, own_process: true }), Plan::End(88));
        assert_eq!(state_name(SERVICE_RUNNING), "running");
        assert_eq!(state_name(42), "in an unknown state");
    }

    #[test]
    fn the_structures_have_windows_sizes() {
        assert_eq!(std::mem::size_of::<ServiceStatusProcess>(), 36);
        assert_eq!(std::mem::size_of::<TokenPrivileges>(), 16);
        assert_eq!(std::mem::size_of::<ShellExecuteInfo>(), 112);
    }
}
