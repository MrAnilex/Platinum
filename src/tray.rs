//! Platinum's icon in the notification area, next to the clock, shown by the daemon for as long as
//! it runs. A click opens the window (`platinum-ui.exe`, next to `platinum.exe`); the right-click
//! menu opens it too, or stops Platinum after asking.
//!
//! Plain Win32 on a thread of its own: a hidden window receives the icon's messages. The icon is
//! the logo, built into the program from `ui/tray.ico`. Its tooltip is asked for again every 2 s
//! (the daemon builds it from status.txt). If Windows refuses the icon (taskbar not ready at
//! logon) it is offered again on the same timer, and if Explorer restarts it is put back.
//!
//! The hidden window also hears the end of the session: logging off or shutting down asks the
//! daemon to stop and waits a few seconds for it to put the default devices back. A process with a
//! window never gets those events through its console handler, so this is the only place they
//! arrive; the window is created even when no icon is wanted.

use crate::com::wide;
use crate::i18n::tr;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

type Hwnd = *mut c_void;
type WndProc = unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize;

#[repr(C)]
struct WndClass {
    style: u32,
    procedure: WndProc,
    class_extra: i32,
    window_extra: i32,
    instance: *mut c_void,
    icon: *mut c_void,
    cursor: *mut c_void,
    background: *mut c_void,
    menu_name: *const u16,
    class_name: *const u16,
}

#[repr(C)]
struct Msg {
    window: Hwnd,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    point: [i32; 2],
    private: u32,
}

#[repr(C)]
struct NotifyIconData {
    size: u32,
    window: Hwnd,
    id: u32,
    flags: u32,
    callback_message: u32,
    icon: *mut c_void,
    tip: [u16; 128],
    state: u32,
    state_mask: u32,
    info: [u16; 256],
    version: u32,
    info_title: [u16; 64],
    info_flags: u32,
    guid: crate::com::Guid,
    balloon_icon: *mut c_void,
}

// The layout Windows expects on 64-bit.
const _: () = assert!(std::mem::size_of::<NotifyIconData>() == 976);

type TimerProc = unsafe extern "system" fn(Hwnd, u32, usize, u32);

#[link(name = "user32")]
unsafe extern "system" {
    fn RegisterClassW(class: *const WndClass) -> u16;
    fn CreateWindowExW(ex_style: u32, class: *const u16, title: *const u16, style: u32, x: i32, y: i32, w: i32, h: i32, parent: Hwnd, menu: *mut c_void, instance: *mut c_void, param: *mut c_void) -> Hwnd;
    fn DefWindowProcW(window: Hwnd, message: u32, wparam: usize, lparam: isize) -> isize;
    fn GetMessageW(msg: *mut Msg, window: Hwnd, min: u32, max: u32) -> i32;
    fn TranslateMessage(msg: *const Msg) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> isize;
    fn PostMessageW(window: Hwnd, message: u32, wparam: usize, lparam: isize) -> i32;
    fn DestroyWindow(window: Hwnd) -> i32;
    fn PostQuitMessage(code: i32);
    fn CreatePopupMenu() -> *mut c_void;
    fn AppendMenuW(menu: *mut c_void, flags: u32, id: usize, text: *const u16) -> i32;
    fn SetMenuDefaultItem(menu: *mut c_void, item: u32, by_position: u32) -> i32;
    fn TrackPopupMenu(menu: *mut c_void, flags: u32, x: i32, y: i32, reserved: i32, window: Hwnd, rect: *const c_void) -> i32;
    fn DestroyMenu(menu: *mut c_void) -> i32;
    fn SetForegroundWindow(window: Hwnd) -> i32;
    fn GetCursorPos(point: *mut [i32; 2]) -> i32;
    fn RegisterWindowMessageW(name: *const u16) -> u32;
    fn GetSystemMetrics(index: i32) -> i32;
    fn CreateIconFromResourceEx(bits: *const u8, size: u32, icon: i32, version: u32, cx: i32, cy: i32, flags: u32) -> *mut c_void;
    fn LoadIconW(instance: *mut c_void, name: *const u16) -> *mut c_void;
    fn DestroyIcon(icon: *mut c_void) -> i32;
    fn AllowSetForegroundWindow(process: u32) -> i32;
    fn MessageBoxW(window: Hwnd, text: *const u16, caption: *const u16, kind: u32) -> i32;
    fn SetTimer(window: Hwnd, id: usize, milliseconds: u32, procedure: Option<TimerProc>) -> usize;
    fn KillTimer(window: Hwnd, id: usize) -> i32;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn Shell_NotifyIconW(message: u32, data: *const NotifyIconData) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> *mut c_void;
}

const LOGO: &[u8] = include_bytes!("../ui/tray.ico");

const WM_DESTROY: u32 = 0x0002;
const WM_CLOSE: u32 = 0x0010;
const WM_QUERYENDSESSION: u32 = 0x0011;
const WM_ENDSESSION: u32 = 0x0016;
const WM_NULL: u32 = 0x0000;
const WM_TIMER: u32 = 0x0113;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_CONTEXTMENU: u32 = 0x007B;
const WM_TRAY: u32 = 0x8000 + 1;
const NIM_ADD: u32 = 0;
const NIM_MODIFY: u32 = 1;
const NIM_DELETE: u32 = 2;
const NIF_MESSAGE: u32 = 1;
const NIF_ICON: u32 = 2;
const NIF_TIP: u32 = 4;
const MF_STRING: u32 = 0;
const MF_SEPARATOR: u32 = 0x800;
const TPM_RIGHTBUTTON: u32 = 0x0002;
const TPM_RETURNCMD: u32 = 0x0100;
const SM_CXSMICON: i32 = 49;
const ASFW_ANY: u32 = u32::MAX;
const MB_YESNO: u32 = 0x0004;
const MB_ICONWARNING: u32 = 0x0030;
const MB_DEFBUTTON2: u32 = 0x0100;
const MB_SETFOREGROUND: u32 = 0x0001_0000;
const IDYES: i32 = 6;
const MENU_OPEN: usize = 1;
const MENU_STOP: usize = 2;
const REFRESH_TIMER: usize = 1;
const REFRESH_MS: u32 = 2000;
/// How long the end of the session is held for the daemon's cleanup.
const END_SESSION_WAIT: Duration = Duration::from_secs(4);

/// What the icon does, given by its owner.
pub struct Handlers {
    /// The tooltip, asked for again every 2 s (Windows shows 127 characters at most).
    pub tip: Box<dyn Fn() -> String + Send + Sync>,
    /// Asks the owner to stop: "Arrêter Platinum" (once confirmed) and the end of the session.
    pub stop: Box<dyn Fn() + Send + Sync>,
    /// Waits, at most the time given, until the stop's cleanup is done; true when it is.
    pub wait_cleanup: Box<dyn Fn(Duration) -> bool + Send + Sync>,
    /// "Arrêter Platinum…" asks for confirmation first, saying what stopping does to the sound.
    pub confirm_stop: bool,
    /// Show the icon. Without it, only the hidden window exists, for the end of the session.
    pub icon: bool,
}

/// The owner's handlers and the icon's state. One icon per process.
struct Setup {
    handlers: Handlers,
    /// The tooltip last given to Windows.
    tip: Mutex<String>,
    icon: AtomicIsize,
}

static SETUP: OnceLock<Setup> = OnceLock::new();
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();
/// Whether Windows holds the icon now.
static ADDED: AtomicBool = AtomicBool::new(false);
/// A menu or the confirmation is open: further clicks wait for it.
static BUSY: AtomicBool = AtomicBool::new(false);

/// Whether the icon is in the notification area.
pub fn shown() -> bool {
    ADDED.load(Ordering::SeqCst)
}

/// The icon in the notification area; dropping it removes the icon.
pub struct Tray {
    window: Arc<AtomicIsize>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Tray {
    fn drop(&mut self) {
        let window = self.window.load(Ordering::SeqCst);
        if window != 0 {
            unsafe { PostMessageW(window as Hwnd, WM_CLOSE, 0, 0) };
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Shows the icon with a fixed tooltip; `stop` runs at once when the user picks "Arrêter
/// Platinum" (no confirmation: for previews). Returns `None` when this process already has one.
pub fn show(tip: &str, stop: impl Fn() + Send + Sync + 'static) -> Option<Tray> {
    let tip = tip.to_string();
    show_with(Handlers { tip: Box::new(move || tip.clone()), stop: Box::new(stop), wait_cleanup: Box::new(|_| true), confirm_stop: false, icon: true })
}

/// Shows the icon (or only the hidden window, see `Handlers::icon`). Returns `None` when this
/// process already has one.
pub fn show_with(handlers: Handlers) -> Option<Tray> {
    SETUP.set(Setup { handlers, tip: Mutex::new(String::new()), icon: AtomicIsize::new(0) }).ok()?;
    let window = Arc::new(AtomicIsize::new(0));
    let shared = window.clone();
    let thread = std::thread::spawn(move || unsafe { run(&shared) });
    Some(Tray { window, thread: Some(thread) })
}

/// The logo at the size Windows uses for notification icons: the closest entry of the icon file
/// (they are PNG images, which Windows accepts here).
fn logo_icon() -> *mut c_void {
    let wanted = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16) as u32;
    let count = LOGO.get(4..6).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]) as usize);
    let mut best: Option<(u32, usize, usize)> = None;
    for k in 0..count {
        let entry = 6 + 16 * k;
        let Some(e) = LOGO.get(entry..entry + 16) else { break };
        let size = if e[0] == 0 { 256 } else { u32::from(e[0]) };
        let bytes = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as usize;
        let offset = u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as usize;
        let better = match best {
            None => true,
            // Prefer the smallest entry at least as large as wanted, else the largest.
            Some((current, _, _)) => (size >= wanted && (current < wanted || size < current)) || (current < wanted && size > current),
        };
        if better {
            best = Some((size, offset, bytes));
        }
    }
    if let Some((_, offset, bytes)) = best {
        if let Some(data) = LOGO.get(offset..offset + bytes) {
            let icon = unsafe { CreateIconFromResourceEx(data.as_ptr(), data.len() as u32, 1, 0x0003_0000, wanted as i32, wanted as i32, 0) };
            if !icon.is_null() {
                return icon;
            }
        }
    }
    // IDI_APPLICATION
    unsafe { LoadIconW(std::ptr::null_mut(), 32512 as *const u16) }
}

/// Adds, updates or removes the icon, and keeps `ADDED` saying whether Windows holds it.
unsafe fn notify(window: Hwnd, message: u32) {
    let Some(setup) = SETUP.get() else { return };
    let mut data: NotifyIconData = unsafe { std::mem::zeroed() };
    data.size = std::mem::size_of::<NotifyIconData>() as u32;
    data.window = window;
    data.id = 1;
    data.flags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    data.callback_message = WM_TRAY;
    data.icon = setup.icon.load(Ordering::SeqCst) as *mut c_void;
    let tip = setup.tip.lock().map(|t| t.clone()).unwrap_or_default();
    for (slot, unit) in data.tip.iter_mut().zip(tip.encode_utf16().take(127)) {
        *slot = unit;
    }
    let accepted = unsafe { Shell_NotifyIconW(message, &data) } != 0;
    match message {
        // An add refused because the icon is in fact there already shows as a working modify.
        NIM_ADD => ADDED.store(accepted || unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) } != 0, Ordering::SeqCst),
        NIM_MODIFY if !accepted => ADDED.store(false, Ordering::SeqCst),
        NIM_DELETE => ADDED.store(false, Ordering::SeqCst),
        _ => {}
    }
}

/// Asks the owner for the tooltip; gives it to Windows when it changed, and offers the icon again
/// while Windows has not taken it.
unsafe fn refresh(window: Hwnd) {
    let Some(setup) = SETUP.get() else { return };
    if !setup.handlers.icon {
        return;
    }
    let tip = (setup.handlers.tip)();
    let changed = match setup.tip.lock() {
        Ok(mut current) if *current != tip => {
            *current = tip;
            true
        }
        _ => false,
    };
    unsafe {
        if !ADDED.load(Ordering::SeqCst) {
            notify(window, NIM_ADD);
        } else if changed {
            notify(window, NIM_MODIFY);
        }
    }
}

/// Starts the window, next to this program, or brings the open one forward (it handles that).
fn open_window() {
    let Ok(exe) = std::env::current_exe() else { return };
    let Some(folder) = exe.parent() else { return };
    // The window may take the foreground: the click gave it to this process.
    unsafe { AllowSetForegroundWindow(ASFW_ANY) };
    let _ = std::process::Command::new(folder.join("platinum-ui.exe")).spawn();
}

/// What stopping does, as the confirmation says it: reopening the window brings Platinum back only
/// when the configuration lets the window start it, and only a window opened anew does so.
fn stop_question(start_on_open: bool) -> &'static str {
    if start_on_open {
        tr("Le son et le micro passeront directement par Windows, sans les effets de Platinum, jusqu'à ce que vous rouvriez la fenêtre de Platinum (fermez-la d'abord si elle est ouverte).\n\nArrêter Platinum maintenant ?")
    } else {
        tr("Le son et le micro passeront directement par Windows, sans les effets de Platinum, jusqu'à ce que vous redémarriez Platinum depuis sa fenêtre (bouton Démarrer).\n\nArrêter Platinum maintenant ?")
    }
}

/// Asks whether to stop, saying what it does to the sound and the microphone.
unsafe fn confirm_stop(window: Hwnd) -> bool {
    let text = stop_question(crate::config::load().start_on_open);
    unsafe { MessageBoxW(window, wide(text).as_ptr(), wide(tr("Arrêter Platinum")).as_ptr(), MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2 | MB_SETFOREGROUND) == IDYES }
}

unsafe fn menu(window: Hwnd) {
    let Some(setup) = SETUP.get() else { return };
    if BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        let menu = CreatePopupMenu();
        AppendMenuW(menu, MF_STRING, MENU_OPEN, wide(tr("Ouvrir Platinum")).as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        let stop = if setup.handlers.confirm_stop { tr("Arrêter Platinum…") } else { tr("Arrêter Platinum") };
        AppendMenuW(menu, MF_STRING, MENU_STOP, wide(stop).as_ptr());
        SetMenuDefaultItem(menu, MENU_OPEN as u32, 0);
        let mut point = [0i32; 2];
        GetCursorPos(&mut point);
        // Without this, the menu would not close when clicking elsewhere.
        SetForegroundWindow(window);
        let chosen = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_RETURNCMD, point[0], point[1], 0, window, std::ptr::null()) as usize;
        PostMessageW(window, WM_NULL, 0, 0);
        DestroyMenu(menu);
        match chosen {
            MENU_OPEN => open_window(),
            MENU_STOP if !setup.handlers.confirm_stop || confirm_stop(window) => (setup.handlers.stop)(),
            _ => {}
        }
    }
    BUSY.store(false, Ordering::SeqCst);
}

unsafe extern "system" fn procedure(window: Hwnd, message: u32, wparam: usize, lparam: isize) -> isize {
    unsafe {
        match message {
            WM_TRAY => {
                match lparam as u32 & 0xFFFF {
                    WM_LBUTTONUP if !BUSY.load(Ordering::SeqCst) => open_window(),
                    WM_RBUTTONUP | WM_CONTEXTMENU => menu(window),
                    _ => {}
                }
                0
            }
            WM_TIMER if wparam == REFRESH_TIMER => {
                refresh(window);
                0
            }
            // The session may end: Platinum never objects.
            WM_QUERYENDSESSION => 1,
            WM_ENDSESSION => {
                if wparam != 0 {
                    // It does end: put the default devices back before Windows ends the process.
                    if let Some(setup) = SETUP.get() {
                        (setup.handlers.stop)();
                        (setup.handlers.wait_cleanup)(END_SESSION_WAIT);
                    }
                }
                0
            }
            WM_CLOSE => {
                DestroyWindow(window);
                0
            }
            WM_DESTROY => {
                KillTimer(window, REFRESH_TIMER);
                if ADDED.load(Ordering::SeqCst) {
                    notify(window, NIM_DELETE);
                }
                PostQuitMessage(0);
                0
            }
            _ if Some(&message) == TASKBAR_CREATED.get() => {
                // Explorer restarted: the icon has to be added again.
                if SETUP.get().is_some_and(|s| s.handlers.icon) {
                    ADDED.store(false, Ordering::SeqCst);
                    notify(window, NIM_ADD);
                }
                0
            }
            _ => DefWindowProcW(window, message, wparam, lparam),
        }
    }
}

unsafe fn run(shared: &AtomicIsize) {
    unsafe {
        let Some(setup) = SETUP.get() else { return };
        if setup.handlers.icon {
            setup.icon.store(logo_icon() as isize, Ordering::SeqCst);
            if let Ok(mut tip) = setup.tip.lock() {
                *tip = (setup.handlers.tip)();
            }
        }
        let _ = TASKBAR_CREATED.set(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()));
        let instance = GetModuleHandleW(std::ptr::null());
        let class_name = wide("PlatinumTray");
        let class = WndClass {
            style: 0,
            procedure,
            class_extra: 0,
            window_extra: 0,
            instance,
            icon: std::ptr::null_mut(),
            cursor: std::ptr::null_mut(),
            background: std::ptr::null_mut(),
            menu_name: std::ptr::null(),
            class_name: class_name.as_ptr(),
        };
        RegisterClassW(&class);
        // A hidden top-level window (not message-only: those miss Explorer's broadcast and the
        // end-of-session messages).
        let window = CreateWindowExW(0, class_name.as_ptr(), wide("Platinum").as_ptr(), 0, 0, 0, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null_mut());
        if window.is_null() {
            return;
        }
        shared.store(window as isize, Ordering::SeqCst);
        if setup.handlers.icon {
            notify(window, NIM_ADD);
            SetTimer(window, REFRESH_TIMER, REFRESH_MS, None);
        }
        let mut msg: Msg = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        shared.store(0, Ordering::SeqCst);
        let icon = setup.icon.swap(0, Ordering::SeqCst);
        if icon != 0 {
            DestroyIcon(icon as *mut c_void);
        }
    }
}
