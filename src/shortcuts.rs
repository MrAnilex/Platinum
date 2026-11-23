//! The daemon's keyboard shortcuts and overlays.
//!
//! A thread of its own owns a hidden window: it registers the combinations of platinum.ini with
//! Windows (`RegisterHotKey`), hands each one pressed to the daemon's mixer thread, and shows the
//! overlay that thread sends back (`overlay::Overlay`, on the same thread since its fade runs on
//! window timers). It reads platinum.ini again whenever the file changes (looked at every 2 s), so
//! that a shortcut set in the window works at once, without restarting anything.
//!
//! The shortcuts themselves are carried out on the daemon's mixer thread (`carry_out`), the one
//! that owns the channels' settings stores for the backup switch: the vendor's DLL leaks memory on
//! every opening of a store, so the daemon opens each one once, there. Each action makes the writes
//! the window makes for the same change (`hotkeys::plan`).

use crate::apo::Stores;
use crate::com::wide;
use crate::i18n::tr;
use crate::config::{self, Config};
use crate::devices::{Devices, Endpoint, Flow, State};
use crate::hotkeys::{self, Action, Hotkey, MOD_NOREPEAT, Mixer, Target, Write};
use crate::mixer::{self, Mix, STRIP_CHANNELS};
use crate::overlay::{self, Content, Glyph, Kind, Overlay};
use crate::streamer::Mode;
use crate::wasapi::EndpointVolume;
use crate::{fallback, outputs, session};
use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::SystemTime;

type Hwnd = *mut c_void;
type WndProc = unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize;
type TimerProc = unsafe extern "system" fn(Hwnd, u32, usize, u32);

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

#[link(name = "user32")]
unsafe extern "system" {
    fn RegisterClassW(class: *const WndClass) -> u16;
    fn CreateWindowExW(ex_style: u32, class: *const u16, title: *const u16, style: u32, x: i32, y: i32, w: i32, h: i32, parent: Hwnd, menu: *mut c_void, instance: *mut c_void, param: *mut c_void) -> Hwnd;
    fn DefWindowProcW(window: Hwnd, message: u32, wparam: usize, lparam: isize) -> isize;
    fn DestroyWindow(window: Hwnd) -> i32;
    fn GetMessageW(msg: *mut Msg, window: Hwnd, min: u32, max: u32) -> i32;
    fn TranslateMessage(msg: *const Msg) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> isize;
    fn PostMessageW(window: Hwnd, message: u32, wparam: usize, lparam: isize) -> i32;
    fn PostQuitMessage(code: i32);
    fn SetTimer(window: Hwnd, id: usize, milliseconds: u32, procedure: Option<TimerProc>) -> usize;
    fn KillTimer(window: Hwnd, id: usize) -> i32;
    fn RegisterHotKey(window: Hwnd, id: i32, modifiers: u32, key: u32) -> i32;
    fn UnregisterHotKey(window: Hwnd, id: i32) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> *mut c_void;
    fn GetLastError() -> u32;
}

const WM_DESTROY: u32 = 0x0002;
const WM_CLOSE: u32 = 0x0010;
const WM_TIMER: u32 = 0x0113;
const WM_HOTKEY: u32 = 0x0312;
/// An overlay is waiting in `Shared::pending`.
const WM_SHOW: u32 = 0x8000 + 2;
const RELOAD_TIMER: usize = 1;
const RELOAD_MS: u32 = 2000;
const ERROR_HOTKEY_ALREADY_REGISTERED: u32 = 1409;

fn log(message: impl AsRef<str>) {
    println!("[{}] {}", crate::guard::stamp(), message.as_ref());
}

/// Shows overlays from any thread: the content waits here until the shortcuts thread takes it.
#[derive(Clone, Default)]
pub struct Overlays {
    window: Arc<AtomicIsize>,
    pending: Arc<Mutex<Option<Content>>>,
}

impl Overlays {
    /// Shows `content` (if the overlay options allow it), replacing the one on screen.
    pub fn show(&self, content: Content) {
        *self.pending.lock().unwrap_or_else(PoisonError::into_inner) = Some(content);
        let window = self.window.load(Ordering::SeqCst);
        if window != 0 {
            unsafe { PostMessageW(window as Hwnd, WM_SHOW, 0, 0) };
        }
    }
}

/// The shortcuts thread; dropping it unregisters the combinations and ends the thread.
pub struct Shortcuts {
    overlays: Overlays,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Shortcuts {
    /// Closes the thread's window and waits for the thread. A daemon that stops right after its
    /// start may get here before the window exists: the close waits for it (or for the thread to
    /// give up), at most a few seconds, rather than joining a thread nobody told to end.
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else { return };
        let started = std::time::Instant::now();
        loop {
            let window = self.overlays.window.load(Ordering::SeqCst);
            if window != 0 {
                unsafe { PostMessageW(window as Hwnd, WM_CLOSE, 0, 0) };
                break;
            }
            if thread.is_finished() {
                break;
            }
            if started.elapsed() > std::time::Duration::from_secs(5) {
                log("keyboard shortcuts: their thread did not start in 5 s; left behind");
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = thread.join();
    }
}

impl Shortcuts {
    /// Starts the thread: each combination pressed sends its action to `actions`.
    pub fn start(actions: Sender<Action>) -> Shortcuts {
        let overlays = Overlays::default();
        let shared = overlays.clone();
        let thread = std::thread::Builder::new().name("raccourcis".into()).spawn(move || unsafe { run(actions, shared) }).ok();
        Shortcuts { overlays, thread }
    }

    /// The handle that shows overlays.
    pub fn overlays(&self) -> Overlays {
        self.overlays.clone()
    }
}

/// What the shortcuts thread keeps between messages.
struct Host {
    window: Hwnd,
    actions: Sender<Action>,
    /// The combinations registered now; the hotkey of entry k has the id k + 1.
    registered: Vec<(Action, Hotkey)>,
    settings: overlay::Settings,
    /// platinum.ini's time when last read (`None`: never read, or no file).
    stamp: Option<Option<SystemTime>>,
    overlay: Option<Overlay>,
    pending: Arc<Mutex<Option<Content>>>,
}

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
}

/// The combinations to register: the first action of each combination (a conflict is flagged in
/// the window; the later ones would only fail to register).
fn distinct(bindings: &[(Action, Hotkey)]) -> Vec<(Action, Hotkey)> {
    let mut kept: Vec<(Action, Hotkey)> = Vec::new();
    for (action, hotkey) in bindings {
        if !kept.iter().any(|(_, k)| k == hotkey) {
            kept.push((*action, *hotkey));
        }
    }
    kept
}

impl Host {
    /// Reads platinum.ini again when it changed, and registers its combinations anew when they
    /// changed.
    fn reload(&mut self) {
        let stamp = config::path().ok().and_then(|file| std::fs::metadata(file).ok()).and_then(|meta| meta.modified().ok());
        if self.stamp == Some(stamp) {
            return;
        }
        self.stamp = Some(stamp);
        let settings = config::load();
        self.settings = settings.overlays;
        let wanted = distinct(&settings.shortcuts);
        if wanted == self.registered {
            return;
        }
        self.unregister();
        let mut taken = Vec::new();
        for (index, (action, hotkey)) in wanted.iter().enumerate() {
            let modifiers = hotkey.modifiers | if action.repeats() { 0 } else { MOD_NOREPEAT };
            if unsafe { RegisterHotKey(self.window, index as i32 + 1, modifiers, hotkey.key) } == 0 {
                let error = unsafe { GetLastError() };
                let why = if error == ERROR_HOTKEY_ALREADY_REGISTERED { "another program uses it".to_string() } else { format!("error {error}") };
                taken.push(format!("{} ({}): {why}", hotkey.to_text(), action.key()));
            }
        }
        self.registered = wanted;
        let count = self.registered.len() - taken.len();
        log(format!("keyboard shortcuts: {count} registered"));
        for problem in taken {
            log(format!("keyboard shortcut not registered: {problem}"));
        }
    }

    fn unregister(&mut self) {
        for index in 0..self.registered.len() {
            unsafe { UnregisterHotKey(self.window, index as i32 + 1) };
        }
        self.registered.clear();
    }

    fn pressed(&self, id: usize) {
        if let Some((action, _)) = id.checked_sub(1).and_then(|index| self.registered.get(index)) {
            let _ = self.actions.send(*action);
        }
    }

    fn show_pending(&mut self) {
        let Some(content) = self.pending.lock().unwrap_or_else(PoisonError::into_inner).take() else { return };
        if !self.settings.shows(content.kind) {
            return;
        }
        if self.overlay.is_none() {
            self.overlay = Overlay::create();
        }
        if let Some(overlay) = &self.overlay {
            overlay.show(&content, self.settings.position);
        }
    }
}

/// Runs `work` on the thread's host, if it is set up. Nothing it calls sends a message back to
/// this window, so the host is never borrowed twice.
fn with_host(work: impl FnOnce(&mut Host)) {
    HOST.with(|host| {
        if let Some(host) = host.borrow_mut().as_mut() {
            work(host);
        }
    });
}

unsafe extern "system" fn procedure(window: Hwnd, message: u32, wparam: usize, lparam: isize) -> isize {
    unsafe {
        match message {
            WM_HOTKEY => {
                with_host(|host| host.pressed(wparam));
                0
            }
            WM_SHOW => {
                with_host(Host::show_pending);
                0
            }
            WM_TIMER if wparam == RELOAD_TIMER => {
                with_host(Host::reload);
                0
            }
            WM_CLOSE => {
                DestroyWindow(window);
                0
            }
            WM_DESTROY => {
                KillTimer(window, RELOAD_TIMER);
                with_host(Host::unregister);
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(window, message, wparam, lparam),
        }
    }
}

unsafe fn run(actions: Sender<Action>, shared: Overlays) {
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class_name = wide("PlatinumShortcuts");
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
        let window = CreateWindowExW(0, class_name.as_ptr(), wide("Platinum").as_ptr(), 0, 0, 0, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null_mut());
        if window.is_null() {
            log("keyboard shortcuts unavailable: their window cannot be created");
            return;
        }
        let host = Host { window, actions, registered: Vec::new(), settings: overlay::Settings::default(), stamp: None, overlay: None, pending: shared.pending.clone() };
        HOST.with(|slot| *slot.borrow_mut() = Some(host));
        with_host(Host::reload);
        shared.window.store(window as isize, Ordering::SeqCst);
        SetTimer(window, RELOAD_TIMER, RELOAD_MS, None);
        let mut msg: Msg = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        shared.window.store(0, Ordering::SeqCst);
        HOST.with(|slot| slot.borrow_mut().take());
    }
}

// ---------------------------------------------------------------------------------------------
// Carrying a shortcut out (the daemon's mixer thread).

/// What a shortcut did: the overlay to show, a line for the log (a device change), and a problem
/// (said once by the caller while it lasts).
#[derive(Debug, Default)]
pub struct Outcome {
    pub overlay: Option<Content>,
    pub log: Option<String>,
    pub problem: Option<String>,
}

impl Outcome {
    fn problem(problem: String) -> Outcome {
        Outcome { problem: Some(problem), ..Outcome::default() }
    }
}

/// Whether an action needs the device list (the microphone's volume, a device change); the others
/// only touch the settings stores, and a held volume key repeats thirty times a second.
pub fn needs_devices(action: Action) -> bool {
    matches!(action, Action::PreviousOutput | Action::NextOutput | Action::VolumeUp(Target::Mic) | Action::VolumeDown(Target::Mic) | Action::Mute(Target::Mic))
}

/// Carries out one shortcut with the daemon's stores and devices (`endpoints` and `hands_free`
/// only when `needs_devices`).
pub fn carry_out(action: Action, stores: &mut Stores, devices: Option<&Devices>, endpoints: &[Endpoint], hands_free: &[String], settings: &Config) -> Outcome {
    if let Some(error) = stores.api_error() {
        return Outcome::problem(format!("shortcut {}: the channels' settings are unavailable ({error})", action.key()));
    }
    // A stream mix's volume changes nothing heard outside streamer mode: nothing is written.
    if settings.mode != Mode::Streamer {
        if let Some(overlay) = action.outside_streamer_mode() {
            return Outcome { overlay: Some(overlay), log: None, problem: Some(format!("shortcut {}: streamer mode is off", action.key())) };
        }
    }
    let outcome = match action {
        Action::PreviousOutput | Action::NextOutput => switch_output(action == Action::NextOutput, stores, endpoints, hands_free, settings),
        _ => adjust(action, stores, devices, endpoints),
    };
    outcome.unwrap_or_else(|error| Outcome::problem(format!("shortcut {}: {error}", action.key())))
}

/// A volume, mute or ChatMix shortcut: reads what the action needs, plans, writes.
fn adjust(action: Action, stores: &mut Stores, devices: Option<&Devices>, endpoints: &[Endpoint]) -> Result<Outcome, String> {
    let mut state = Mixer::default();
    match action {
        Action::VolumeUp(Target::Mic) | Action::VolumeDown(Target::Mic) | Action::Mute(Target::Mic) => {
            state.mic = microphone_volume(devices, endpoints).and_then(|volume| Some((volume.level().ok()?, volume.muted().ok()?)));
        }
        Action::ChatMixGame | Action::ChatMixChat => {
            let game = stores.get(STRIP_CHANNELS[0]).ok_or("Gaming's settings cannot be read")?;
            let (gain, routed) = (mixer::read_gain(game, mixer::CHATMIX_GAME), outputs::routed(game));
            let chat = stores.get(STRIP_CHANNELS[1]).ok_or("Chat's settings cannot be read")?;
            state.chatmix = (gain, mixer::read_gain(chat, mixer::CHATMIX_CHAT));
            state.chatmix_usable = mixer::chatmix_reason(routed.as_deref(), outputs::routed(chat).as_deref()).is_empty();
        }
        _ => {
            // Only the mix the action changes is read: a held key repeats thirty times a second.
            let mix = action.mix().unwrap_or(Mix::Personal);
            for (index, channel) in STRIP_CHANNELS.iter().enumerate() {
                let store = stores.get(*channel).ok_or_else(|| format!("the {} channel's settings cannot be read", channel.label()))?;
                let (volume, muted) = (mixer::read_mix_volume(store, mix)?, mixer::read_mix_muted(store, mix)?);
                match mix {
                    Mix::Personal => (state.volumes[index], state.mutes[index]) = (volume, muted),
                    Mix::Stream => (state.stream_volumes[index], state.stream_mutes[index]) = (volume, muted),
                }
            }
            state.master = mixer::load_master();
        }
    }
    let plan = hotkeys::plan(action, &state);
    for write in &plan.writes {
        execute(write, stores, devices, endpoints)?;
    }
    if let Some(master) = plan.master.filter(|master| *master != state.master) {
        mixer::save_master(&master).map_err(|e| format!("the Master's position was not saved ({e})"))?;
    }
    Ok(Outcome { overlay: plan.overlay, log: None, problem: plan.problem.map(|p| format!("shortcut {}: {p}", action.key())) })
}

/// A strip's settings store (0 Gaming to 3 Aux).
fn strip(stores: &mut Stores, index: usize) -> Result<&crate::apo::Store<'static>, String> {
    let channel = STRIP_CHANNELS[index.min(STRIP_CHANNELS.len() - 1)];
    stores.get(channel).ok_or_else(|| format!("the {} channel's settings cannot be read", channel.label()))
}

/// One write of a plan.
fn execute(write: &Write, stores: &mut Stores, devices: Option<&Devices>, endpoints: &[Endpoint]) -> Result<(), String> {
    match write {
        Write::Volume(index, volume) => mixer::write_volume(strip(stores, *index)?, *volume),
        Write::Mute(index, on) => mixer::write_muted(strip(stores, *index)?, *on),
        Write::StreamVolume(index, volume) => mixer::write_mix_volume(strip(stores, *index)?, Mix::Stream, *volume),
        Write::StreamMute(index, on) => mixer::write_mix_muted(strip(stores, *index)?, Mix::Stream, *on),
        Write::ChatMix(game, chat) => {
            let store = stores.get(STRIP_CHANNELS[0]).ok_or("Gaming's settings cannot be read")?;
            mixer::write_gain(store, mixer::CHATMIX_GAME, *game)?;
            let store = stores.get(STRIP_CHANNELS[1]).ok_or("Chat's settings cannot be read")?;
            mixer::write_gain(store, mixer::CHATMIX_CHAT, *chat)
        }
        Write::MicLevel(level) => microphone_volume(devices, endpoints).ok_or("the virtual microphone is not active")?.set_level(*level).map_err(|e| e.to_string()),
        Write::MicMute(on) => microphone_volume(devices, endpoints).ok_or("the virtual microphone is not active")?.set_muted(*on).map_err(|e| e.to_string()),
    }
}

/// The Windows volume of the virtual microphone's recording side (what Discord records), as the
/// window's Micro fader sets it.
fn microphone_volume(devices: Option<&Devices>, endpoints: &[Endpoint]) -> Option<EndpointVolume> {
    let endpoint = endpoints.iter().find(|e| e.flow == Flow::Capture && e.state == State::Active && session::virtual_kind(e).as_deref() == Some("microphone"))?;
    EndpointVolume::activate(devices?, &endpoint.id).ok()
}

/// "Previous / next device": every output channel moves to the device before or after the one
/// they play to, among the priority list's usable devices (every usable output without a list),
/// as the window's Master menu moves them; the echo cancellation follows. With the backup switch
/// on, the list turns so that this device comes first, or the switch would move the channels back.
/// In streamer mode this is the personal mix's device: only the channels in that mix move (one
/// out of it names no device, and must stay out), and the device is remembered as the mix's.
fn switch_output(forward: bool, stores: &mut Stores, endpoints: &[Endpoint], hands_free: &[String], settings: &Config) -> Result<Outcome, String> {
    let routed: Vec<Option<String>> = outputs::CHANNELS.iter().map(|(channel, _)| stores.get(*channel).and_then(outputs::routed)).collect();
    let current = outputs::distinct(&routed).first().map(|id| id.to_string());
    let candidates = fallback::cycle(&settings.output_priority, endpoints, hands_free);
    let ids: Vec<&str> = candidates.iter().map(|e| e.id.as_str()).collect();
    let Some(index) = hotkeys::next_device(&ids, current.as_deref(), forward) else {
        let title = if forward { tr("Périphérique suivant") } else { tr("Périphérique précédent") };
        return Ok(Outcome { overlay: Some(Content::unavailable(Kind::Device, Glyph::Device, title, tr("Aucun périphérique de sortie disponible"))), log: None, problem: Some("no output device to switch to".into()) });
    };
    let target = candidates[index];
    let streamer = settings.mode == Mode::Streamer;
    let mut moved = 0;
    for ((channel, _), now) in outputs::CHANNELS.iter().zip(&routed) {
        if now.as_deref() == Some(target.id.as_str()) || (streamer && now.is_none()) {
            continue;
        }
        let store = stores.get(*channel).ok_or_else(|| format!("the {} channel's settings cannot be read", channel.label()))?;
        outputs::set_output(store, Some(&target.id))?;
        moved += 1;
    }
    let mut log = (moved > 0).then(|| format!("shortcut: output channels to {} ({})", target.name, if forward { "next device" } else { "previous device" }));
    if moved > 0 {
        let routed: Vec<Option<String>> = outputs::CHANNELS.iter().map(|(channel, _)| stores.get(*channel).and_then(outputs::routed)).collect();
        let stream_device = settings.stream_device.resolve(endpoints);
        let stream = outputs::StreamReference::of(settings.mode, &settings.channels, stream_device.as_deref());
        let synced = stores.get(crate::apo::Channel::Mic).ok_or_else(|| "the microphone's settings cannot be read".to_string()).and_then(|mic| outputs::sync_echo(mic, &routed, stream));
        if let Err(error) = synced {
            log = Some(format!("{}; echo cancellation not pointed at it ({error})", log.unwrap_or_default()));
        }
    }
    let rotate = settings.auto_switch && !settings.output_priority.is_empty();
    let remember = streamer && moved > 0;
    if rotate || remember {
        let saved = config::load_full().for_saving().and_then(|mut config| {
            let mut changed = rotate && fallback::rotate_to(&mut config.output_priority, &target.id);
            if remember && config.personal_device.as_deref() != Some(target.id.as_str()) {
                config.personal_device = Some(target.id.clone());
                changed = true;
            }
            if changed { config.save() } else { Ok(()) }
        });
        if let Err(error) = saved {
            log = Some(format!("{}; platinum.ini was not updated (priority list, personal mix) ({error})", log.unwrap_or_else(|| "shortcut".into())));
        }
    }
    Ok(Outcome { overlay: Some(Content::device(&target.name, forward)), log, problem: None })
}

/// `platinum shortcuts --check`: registers each combination for an instant on this thread and
/// gives it back at once, to tell which ones another program holds. Returns, for each, the error
/// Windows gave (`None`: free).
pub fn check(hotkeys: &[Hotkey]) -> Vec<(Hotkey, Option<u32>)> {
    hotkeys
        .iter()
        .enumerate()
        .map(|(index, hotkey)| {
            // Without a window, the hotkey belongs to this thread; id 0xB000 + n stays clear of
            // the daemon's ids (which live in its own process anyway).
            let id = 0xB000 + index as i32;
            let registered = unsafe { RegisterHotKey(std::ptr::null_mut(), id, hotkey.modifiers | MOD_NOREPEAT, hotkey.key) } != 0;
            let error = if registered {
                unsafe { UnregisterHotKey(std::ptr::null_mut(), id) };
                None
            } else {
                Some(unsafe { GetLastError() })
            };
            (*hotkey, error)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_combination_used_twice_is_registered_once() {
        let up = Hotkey::parse("Ctrl+Alt+Up").unwrap();
        let down = Hotkey::parse("Ctrl+Alt+Down").unwrap();
        let bindings = [(Action::VolumeUp(Target::Game), up), (Action::VolumeUp(Target::Chat), up), (Action::VolumeDown(Target::Game), down)];
        assert_eq!(distinct(&bindings), [(Action::VolumeUp(Target::Game), up), (Action::VolumeDown(Target::Game), down)]);
    }

    #[test]
    fn only_device_and_microphone_shortcuts_list_the_devices() {
        assert!(needs_devices(Action::NextOutput) && needs_devices(Action::Mute(Target::Mic)));
        assert!(!needs_devices(Action::VolumeUp(Target::Master)) && !needs_devices(Action::ChatMixGame));
    }
}
