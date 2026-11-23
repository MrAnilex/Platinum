//! One window at a time. A second `platinum-ui.exe` (a click on the icon next to the clock, the
//! Start menu) asks the first one to come forward, then exits.
//!
//! Windows only lets the foreground process put a window in front. The second launch is that
//! process (the user just started it), so it hands the right on (`AllowSetForegroundWindow`)
//! before it signals; otherwise the first window could only flash in the taskbar.

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The claimed mutex, kept until the window closes (`release`).
static CLAIM: AtomicUsize = AtomicUsize::new(0);

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateMutexW(security: *mut c_void, owner: i32, name: *const u16) -> *mut c_void;
    fn CreateEventW(security: *mut c_void, manual: i32, initial: i32, name: *const u16) -> *mut c_void;
    fn SetEvent(event: *mut c_void) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    fn GetLastError() -> u32;
    fn GetCurrentProcessId() -> u32;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn EnumWindows(callback: unsafe extern "system" fn(*mut c_void, isize) -> i32, param: isize) -> i32;
    fn GetWindowThreadProcessId(window: *mut c_void, pid: *mut u32) -> u32;
    fn IsWindowVisible(window: *mut c_void) -> i32;
    fn GetWindowTextLengthW(window: *mut c_void) -> i32;
    fn ShowWindow(window: *mut c_void, command: i32) -> i32;
    fn SetForegroundWindow(window: *mut c_void) -> i32;
    fn AllowSetForegroundWindow(pid: u32) -> i32;
}

const INSTANCE: &str = "Local\\platinum-ui";
const SHOW: &str = "Local\\platinum-ui-show";
const ERROR_ALREADY_EXISTS: u32 = 183;
const INFINITE: u32 = u32::MAX;
const SW_RESTORE: i32 = 9;
/// Any process may bring its window forward (the first window's pid is not known here).
const ASFW_ANY: u32 = u32::MAX;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Claims the single window. When another one is open, asks it to come forward and returns false:
/// this process should then exit.
pub fn claim(on_show_request: impl Fn() + Send + 'static) -> bool {
    unsafe {
        // The event before the mutex, in every launch: once the first window's mutex exists, its
        // event does too, so a second launch always has someone to signal.
        let event = CreateEventW(std::ptr::null_mut(), 0, 0, wide(SHOW).as_ptr());
        let mutex = CreateMutexW(std::ptr::null_mut(), 0, wide(INSTANCE).as_ptr());
        if !mutex.is_null() && GetLastError() == ERROR_ALREADY_EXISTS {
            AllowSetForegroundWindow(ASFW_ANY);
            if !event.is_null() {
                SetEvent(event);
                CloseHandle(event);
            }
            CloseHandle(mutex);
            return false;
        }
        // The mutex stays open until the window closes; the event for the life of the process.
        CLAIM.store(mutex as usize, Ordering::SeqCst);
        if !event.is_null() {
            let event = event as usize;
            let _ = std::thread::Builder::new().name("fenêtre unique".into()).spawn(move || {
                loop {
                    if WaitForSingleObject(event as *mut c_void, INFINITE) != 0 {
                        return;
                    }
                    on_show_request();
                }
            });
        }
        true
    }
}

/// Gives the single window up once it is closed: the process may still finish some work (a
/// restart of the daemon under way), and a window opened meanwhile must open, not ask this one to
/// come forward.
pub fn release() {
    let mutex = CLAIM.swap(0, Ordering::SeqCst);
    if mutex != 0 {
        unsafe {
            CloseHandle(mutex as *mut c_void);
        }
    }
}

unsafe extern "system" fn bring(window: *mut c_void, _: isize) -> i32 {
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(window, &mut pid);
        if pid == GetCurrentProcessId() && IsWindowVisible(window) != 0 && GetWindowTextLengthW(window) > 0 {
            ShowWindow(window, SW_RESTORE);
            SetForegroundWindow(window);
            return 0;
        }
        1
    }
}

/// Restores this process's window and puts it in front.
pub fn bring_to_front() {
    unsafe {
        EnumWindows(bring, 0);
    }
}
