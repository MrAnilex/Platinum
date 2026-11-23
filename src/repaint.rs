//! Keeps a Slint window drawn in full on Windows (the window and the setup use Slint's software
//! renderer through its winit backend).
//!
//! Why a window could show only its meters and faders, the rest see-through (seen by the user on
//! 18/11/2026 when bringing Platinum back from the background), from Slint 1.18, winit 0.30 and
//! softbuffer 0.4 as they are in the cargo registry:
//! - Slint creates its winit windows transparent on Windows (`with_transparent(true)` in
//!   `WinitWindowAdapter::window_attributes`), and winit then calls `DwmEnableBlurBehindWindow`
//!   with an empty region: the compositor honours the alpha of every pixel of the window;
//! - the software renderer repaints only what changed since the last frame when its buffer is
//!   reused (`RepaintBufferType::ReusedBuffer`, buffer age 1), and softbuffer copies only those
//!   rectangles to the window (`present_with_damage`), then validates the whole window;
//! - when Windows needs the window drawn again in full (restored after a long minimisation, a
//!   display or session change, a lost compositor surface), it sends `WM_PAINT` with the whole
//!   window to update, but nothing in that chain looks at the update region: the frame copies only
//!   what Slint found dirty, the meters and faders that move all the time, and validates the rest,
//!   which stays as the compositor left it, and, being transparent for it, shows what is behind.
//!
//! The fix has two parts, both here: the window is made opaque for the compositor again
//! (`DwmEnableBlurBehindWindow` off: Platinum's windows have an opaque background and need no
//! transparency), so that a pixel never drawn can no longer show the desktop; and the window
//! procedure is watched (classic subclassing, chained to winit's) for the moments a full repaint
//! is needed: a `WM_PAINT` with a real update region (not winit's own internal paint requests),
//! a restore or resize, a window shown again, a display or compositor change. Then the callback
//! given to `watch` runs; the window answers by making its whole area dirty for the renderer
//! (a full-size rectangle under everything changes colour by one step), so the next frame repaints
//! and copies every pixel.

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};

#[repr(C)]
struct BlurBehind {
    flags: u32,
    enable: i32,
    region: *mut c_void,
    transition_on_maximized: i32,
}

#[repr(C)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

type WindowProc = unsafe extern "system" fn(*mut c_void, u32, usize, isize) -> isize;

#[link(name = "user32")]
unsafe extern "system" {
    fn EnumWindows(callback: unsafe extern "system" fn(*mut c_void, isize) -> i32, param: isize) -> i32;
    fn GetWindowThreadProcessId(window: *mut c_void, pid: *mut u32) -> u32;
    fn IsWindowVisible(window: *mut c_void) -> i32;
    fn GetWindowTextLengthW(window: *mut c_void) -> i32;
    fn GetWindow(window: *mut c_void, command: u32) -> *mut c_void;
    fn GetUpdateRect(window: *mut c_void, rect: *mut Rect, erase: i32) -> i32;
    fn SetWindowLongPtrW(window: *mut c_void, index: i32, value: isize) -> isize;
    fn GetWindowLongPtrW(window: *mut c_void, index: i32) -> isize;
    fn CallWindowProcW(previous: isize, window: *mut c_void, message: u32, wparam: usize, lparam: isize) -> isize;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcessId() -> u32;
}

const GWLP_WNDPROC: i32 = -4;
const GW_OWNER: u32 = 4;
const DWM_BB_ENABLE: u32 = 0x1;
const WM_SIZE: u32 = 0x0005;
const WM_PAINT: u32 = 0x000F;
const WM_SHOWWINDOW: u32 = 0x0018;
const WM_DISPLAYCHANGE: u32 = 0x007E;
const WM_DWMCOMPOSITIONCHANGED: u32 = 0x031E;
const SIZE_MINIMIZED: usize = 1;

/// The watched window, its previous window procedure and the callback (a `fn()`), once set.
static WINDOW: AtomicUsize = AtomicUsize::new(0);
static PREVIOUS: AtomicIsize = AtomicIsize::new(0);
static CALLBACK: AtomicUsize = AtomicUsize::new(0);

/// Whether a window message calls for every pixel to be drawn again. `has_update_region`: for
/// `WM_PAINT`, whether Windows invalidated part of the window (winit's own redraw requests come
/// as paints without one).
pub fn needs_full_repaint(message: u32, wparam: usize, lparam: isize, has_update_region: bool) -> bool {
    match message {
        WM_PAINT => has_update_region,
        // Restored, maximised or resized to a real size; not while minimised (0×0).
        WM_SIZE => wparam != SIZE_MINIMIZED && lparam != 0,
        WM_SHOWWINDOW => wparam != 0,
        WM_DISPLAYCHANGE | WM_DWMCOMPOSITIONCHANGED => true,
        _ => false,
    }
}

unsafe extern "system" fn watched(window: *mut c_void, message: u32, wparam: usize, lparam: isize) -> isize {
    let has_update_region = message == WM_PAINT && {
        let mut rect = Rect { left: 0, top: 0, right: 0, bottom: 0 };
        unsafe { GetUpdateRect(window, &mut rect, 0) != 0 }
    };
    if needs_full_repaint(message, wparam, lparam, has_update_region) {
        let callback = CALLBACK.load(Ordering::SeqCst);
        if callback != 0 {
            let callback: fn() = unsafe { std::mem::transmute(callback) };
            callback();
        }
    }
    unsafe { CallWindowProcW(PREVIOUS.load(Ordering::SeqCst), window, message, wparam, lparam) }
}

/// This process's main window: visible, top-level, unowned, with a title.
fn main_window() -> Option<*mut c_void> {
    unsafe extern "system" fn visit(window: *mut c_void, found: isize) -> i32 {
        unsafe {
            let mut pid = 0;
            GetWindowThreadProcessId(window, &mut pid);
            if pid == GetCurrentProcessId() && IsWindowVisible(window) != 0 && GetWindow(window, GW_OWNER).is_null() && GetWindowTextLengthW(window) > 0 {
                *(found as *mut usize) = window as usize;
                return 0;
            }
        }
        1
    }
    let mut found = 0usize;
    unsafe { EnumWindows(visit, &mut found as *mut usize as isize) };
    (found != 0).then_some(found as *mut c_void)
}

/// Makes this process's main window opaque for the compositor and calls `full_repaint` whenever
/// it must be drawn again in full (see the module's notes). `full_repaint` runs inside the window
/// procedure, on the window's thread: it should only post the work (`slint::invoke_from_event_loop`).
/// Returns false while the window does not exist yet (call again a little later); true once
/// watched, and on later calls.
pub fn watch(full_repaint: fn()) -> bool {
    if WINDOW.load(Ordering::SeqCst) != 0 {
        return true;
    }
    let Some(window) = main_window() else { return false };
    unsafe {
        let blur = BlurBehind { flags: DWM_BB_ENABLE, enable: 0, region: std::ptr::null_mut(), transition_on_maximized: 0 };
        // Resolved at run time: the GNU toolchain ships no import library for dwmapi.
        if let Ok(dwm) = crate::com::load_library("dwmapi.dll", 0)
            && let Ok(function) = crate::com::export(dwm, b"DwmEnableBlurBehindWindow\0")
        {
            let enable: unsafe extern "system" fn(*mut c_void, *const BlurBehind) -> i32 = std::mem::transmute(function);
            enable(window, &blur);
        }
        // winit's procedure is known before ours is installed, so a message arriving in between
        // still reaches it.
        let previous = GetWindowLongPtrW(window, GWLP_WNDPROC);
        if previous == 0 {
            return false;
        }
        PREVIOUS.store(previous, Ordering::SeqCst);
        CALLBACK.store(full_repaint as usize, Ordering::SeqCst);
        let procedure: WindowProc = watched;
        if SetWindowLongPtrW(window, GWLP_WNDPROC, procedure as isize) == 0 {
            CALLBACK.store(0, Ordering::SeqCst);
            return false;
        }
    }
    WINDOW.store(window as usize, Ordering::SeqCst);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_repaints_follow_what_windows_asks() {
        // A system paint with a region, not winit's internal ones.
        assert!(needs_full_repaint(WM_PAINT, 0, 0, true));
        assert!(!needs_full_repaint(WM_PAINT, 0, 0, false));
        // Restored to 1210×820, not minimised.
        let size = (820 << 16) | 1210;
        assert!(needs_full_repaint(WM_SIZE, 0, size, false));
        assert!(!needs_full_repaint(WM_SIZE, SIZE_MINIMIZED, 0, false));
        assert!(needs_full_repaint(WM_SHOWWINDOW, 1, 0, false));
        assert!(!needs_full_repaint(WM_SHOWWINDOW, 0, 0, false));
        assert!(needs_full_repaint(WM_DISPLAYCHANGE, 32, 0, false));
        assert!(!needs_full_repaint(0x0200, 0, 0, false));
    }
}
