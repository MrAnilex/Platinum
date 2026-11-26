//! Silent streams that keep the microphone's echo-cancellation references playing.
//!
//! The vendor's microphone effects engine, hosted in the daemon, listens to the outputs named in
//! its echo-cancellation settings (205 to 208, `outputs::AEC_HEARD`…) through loopback captures it
//! opens itself. Windows feeds a loopback capture only while something plays on its endpoint. With
//! nothing playing on a reference, the daemon was measured growing by 6.4 MB a minute with its
//! handle count rising; with sound on it, the handles froze (17/11/2026). The vendor's service
//! keeps render streams of its own open on those outputs; the daemon does the same with silence.
//!
//! The daemon's mixer thread reads the references (through the stores it keeps, and only when the
//! microphone's settings may have changed, see `StoreClock`) and hands them to `KeepAlive`, whose
//! thread keeps one shared-mode stream of inaudible noise (`wasapi::SilentStream`, about -120 dBFS) open on each of them
//! that is an active output, and never on the virtual microphone's playback side, on an input, or
//! on a Bluetooth headset's hands-free side. A stream whose device goes away is tried again every
//! `RETRY`, and said once in the log.

use crate::com::{self, wide};
use crate::devices::{Devices, Endpoint, Flow, State};
use crate::wasapi::SilentStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// Silence queued in each stream: a second, topped up every `TOP_UP`, leaves the thread plenty of
/// slack and wakes it a few times a second only.
const BUFFER_MS: u32 = 1000;
const TOP_UP: Duration = Duration::from_millis(400);
/// How often a reference that could not be given a stream is tried again.
pub const RETRY: Duration = Duration::from_secs(10);
/// The driver's role for the virtual microphone (`PKEY_SS_VAD_ROLE`): its playback side is where
/// the relay writes the microphone, never a reference to keep playing.
const MICROPHONE_ROLE: u32 = 3;

/// How many keep-alive streams are open now, for the daemon's memory lines.
static OPEN: AtomicUsize = AtomicUsize::new(0);

/// How many keep-alive streams are open now.
pub fn open_streams() -> usize {
    OPEN.load(Ordering::SeqCst)
}

fn log(message: impl AsRef<str>) {
    println!("[{}] {}", crate::guard::stamp(), message.as_ref());
}

/// The devices to keep playing, from the four references as the store holds them: each named
/// device once, in order, empty ones left out.
pub fn references(held: &[String]) -> Vec<String> {
    let mut wanted: Vec<String> = Vec::new();
    for id in held.iter().map(|id| id.trim()).filter(|id| !id.is_empty()) {
        if !wanted.iter().any(|known| known.eq_ignore_ascii_case(id)) {
            wanted.push(id.to_string());
        }
    }
    wanted
}

/// Why no stream may be opened on an endpoint, if it may not: only an active output qualifies, and
/// not the virtual microphone's playback side.
pub fn refusal(endpoint: &Endpoint) -> Option<&'static str> {
    if endpoint.flow != Flow::Render {
        Some("an input, not an output")
    } else if endpoint.virtual_role == Some(MICROPHONE_ROLE) {
        Some("the virtual microphone's playback side")
    } else if endpoint.state != State::Active {
        Some(endpoint.state.label())
    } else {
        None
    }
}

/// What changes from the streams open to those wanted: (to open, to close), each in order.
fn changes(wanted: &[String], open: &[&str]) -> (Vec<String>, Vec<String>) {
    let to_open = wanted.iter().filter(|id| !open.iter().any(|o| o.eq_ignore_ascii_case(id))).cloned().collect();
    let to_close = open.iter().filter(|o| !wanted.iter().any(|id| id.eq_ignore_ascii_case(o))).map(|o| o.to_string()).collect();
    (to_open, to_close)
}

/// The devices wanted, and why the ones left out are closed (for the log).
type Wanted = (Vec<String>, &'static str);

/// How long dropping the keep-alive waits for its thread. Opening a stream on a device that is
/// coming back can wait on Windows' audio service for as long as the service is frozen (26/11/2026:
/// minutes); the daemon's stop must not wait with it.
const JOIN_WAIT: Duration = Duration::from_secs(3);

/// The keep-alive thread. Dropping it closes every stream and waits for the thread, at most
/// `JOIN_WAIT`: a thread stuck in a call to Windows is left behind, and ends when the call does.
pub struct KeepAlive {
    commands: Option<Sender<Wanted>>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Closed by the thread when it ends.
    ended: Receiver<()>,
}

impl KeepAlive {
    /// Starts the thread, with no stream open yet.
    pub fn start() -> Result<KeepAlive, String> {
        let (commands, received) = std::sync::mpsc::channel();
        let (ending, ended) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("keep-alive".into())
            .spawn(move || {
                serve(&received);
                drop(ending);
            })
            .map_err(|e| format!("cannot start the keep-alive thread: {e}"))?;
        Ok(KeepAlive { commands: Some(commands), thread: Some(thread), ended })
    }

    /// Keeps a stream open on each of `devices` (identifiers) and on no other; `why` says, for the
    /// log, why the others close.
    pub fn want(&self, devices: Vec<String>, why: &'static str) {
        if let Some(commands) = &self.commands {
            let _ = commands.send((devices, why));
        }
    }
}

impl Drop for KeepAlive {
    fn drop(&mut self) {
        // The thread ends when its channel closes, after closing its streams.
        self.commands = None;
        let Some(thread) = self.thread.take() else { return };
        match self.ended.recv_timeout(JOIN_WAIT) {
            Err(RecvTimeoutError::Timeout) => log(format!("keep-alive: its thread is still in a call to Windows' audio service after {} s; left behind", JOIN_WAIT.as_secs())),
            // Ended (or panicked): joining is immediate.
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                let _ = thread.join();
            }
        }
    }
}

fn serve(commands: &Receiver<Wanted>) {
    let mut keeper = Keeper::default();
    loop {
        let command = match keeper.wait(Instant::now()) {
            Some(timeout) => commands.recv_timeout(timeout),
            // Nothing to keep up or to try again: sleep until told something.
            None => commands.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match command {
            Ok((devices, why)) => keeper.want(&devices, why),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        keeper.service(Instant::now());
    }
    keeper.close_all("Platinum stops");
}

struct Open {
    id: String,
    name: String,
    stream: SilentStream,
}

/// A reference without a stream, tried again at `next`.
struct Waiting {
    id: String,
    next: Instant,
    /// What the log last said about it, so that a lasting problem is said once.
    said: String,
}

/// What the keep-alive thread keeps.
#[derive(Default)]
struct Keeper {
    /// Reopened after a failure: Windows' audio service may have restarted under it.
    devices: Option<Devices>,
    open: Vec<Open>,
    waiting: Vec<Waiting>,
}

impl Keeper {
    /// How long the thread may sleep: until the next top-up while a stream is open, until the next
    /// attempt while one waits; `None` with nothing to do.
    fn wait(&self, now: Instant) -> Option<Duration> {
        if !self.open.is_empty() {
            return Some(TOP_UP);
        }
        self.waiting.iter().map(|w| w.next).min().map(|next| next.saturating_duration_since(now).max(Duration::from_millis(50)))
    }

    fn want(&mut self, devices: &[String], why: &str) {
        let open: Vec<&str> = self.open.iter().map(|o| o.id.as_str()).collect();
        let (to_open, to_close) = changes(devices, &open);
        for id in &to_close {
            if let Some(index) = self.open.iter().position(|o| o.id.eq_ignore_ascii_case(id)) {
                let closed = self.open.remove(index);
                log(format!("keep-alive: stream on {} closed ({why})", closed.name));
            }
        }
        self.waiting.retain(|w| devices.iter().any(|id| id.eq_ignore_ascii_case(&w.id)));
        for id in to_open {
            if !self.waiting.iter().any(|w| w.id.eq_ignore_ascii_case(&id)) {
                self.try_open(id, String::new(), Instant::now());
            }
        }
        self.publish();
    }

    /// Tops the streams up, and tries again the references whose time has come.
    fn service(&mut self, now: Instant) {
        let mut lost: Vec<(String, String, String)> = Vec::new();
        self.open.retain(|open| match open.stream.top_up() {
            Ok(_) => true,
            Err(error) => {
                lost.push((open.id.clone(), open.name.clone(), error.to_string()));
                false
            }
        });
        for (id, name, error) in lost {
            log(format!("keep-alive: stream on {name} lost ({error}); tried again every {} s", RETRY.as_secs()));
            self.devices = None;
            self.waiting.push(Waiting { id, next: now + RETRY, said: error });
        }
        let (due, later): (Vec<Waiting>, Vec<Waiting>) = std::mem::take(&mut self.waiting).into_iter().partition(|w| w.next <= now);
        self.waiting = later;
        for waiting in due {
            self.try_open(waiting.id, waiting.said, now);
        }
        self.publish();
    }

    /// Opens a stream on `id`, or puts it back among those waiting. `said` is what the log last
    /// said about it.
    fn try_open(&mut self, id: String, said: String, now: Instant) {
        match self.open_one(&id) {
            Ok((name, stream)) => {
                log(format!("keep-alive: stream opened on {name}"));
                self.open.push(Open { id, name, stream });
            }
            Err(problem) => {
                if problem != said {
                    log(format!("keep-alive: no stream on {problem}; tried again every {} s", RETRY.as_secs()));
                }
                self.waiting.push(Waiting { id, next: now + RETRY, said: problem });
            }
        }
    }

    /// The device's name and its stream, or what keeps it from having one ("<name> (<reason>)").
    fn open_one(&mut self, id: &str) -> Result<(String, SilentStream), String> {
        if self.devices.is_none() {
            self.devices = Some(Devices::open().map_err(|e| format!("{id} (Windows audio devices unavailable: {e})"))?);
        }
        let Some(devices) = &self.devices else { return Err(format!("{id} (Windows audio devices unavailable)")) };
        let endpoint = match devices.endpoint(id) {
            Ok(endpoint) => endpoint,
            Err(com::Error::Call { hresult, .. }) if hresult == crate::devices::NOT_FOUND => return Err(format!("{id} (absent)")),
            Err(error) => {
                self.devices = None;
                return Err(format!("{id} ({error})"));
            }
        };
        if let Some(reason) = refusal(&endpoint) {
            return Err(format!("{} ({reason})", endpoint.name));
        }
        // Playing to a Bluetooth headset's hands-free side opens its call link, which degrades
        // everything it plays, as opening its microphone does.
        if devices.is_hands_free(id) {
            return Err(format!("{} (a Bluetooth headset's hands-free side)", endpoint.name));
        }
        match SilentStream::open(devices, id, BUFFER_MS) {
            Ok(stream) => Ok((endpoint.name, stream)),
            Err(error) => {
                self.devices = None;
                Err(format!("{} ({error})", endpoint.name))
            }
        }
    }

    fn close_all(&mut self, why: &str) {
        for closed in self.open.drain(..) {
            log(format!("keep-alive: stream on {} closed ({why})", closed.name));
        }
        self.waiting.clear();
        self.publish();
    }

    fn publish(&self) {
        OPEN.store(self.open.len(), Ordering::SeqCst);
    }
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegOpenKeyExW(key: isize, subkey: *const u16, options: u32, access: u32, result: *mut isize) -> i32;
    fn RegQueryInfoKeyW(
        key: isize,
        class: *mut u16,
        class_len: *mut u32,
        reserved: *mut u32,
        subkeys: *mut u32,
        max_subkey_len: *mut u32,
        max_class_len: *mut u32,
        values: *mut u32,
        max_value_name_len: *mut u32,
        max_value_len: *mut u32,
        security_descriptor: *mut u32,
        last_write: *mut u64,
    ) -> i32;
    fn RegCloseKey(key: isize) -> i32;
}

const HKEY_LOCAL_MACHINE: isize = 0x8000_0002_u32 as i32 as isize;
const KEY_QUERY_VALUE: u32 = 0x0001;

/// When a channel's settings last changed, read from the last-write time of their registry key
/// (`apo::Channel::registry_key`): a few microseconds, where reading the settings again goes
/// through the vendor's DLL. The key stays open; one that cannot be opened or read says nothing,
/// and the caller then reads the settings every time.
pub struct StoreClock {
    path: String,
    key: Option<isize>,
}

impl StoreClock {
    pub fn new(channel: crate::apo::Channel) -> StoreClock {
        StoreClock { path: channel.registry_key(), key: None }
    }

    /// The key's last-write time (100-nanosecond units), `None` when it cannot be read.
    pub fn last_write(&mut self) -> Option<u64> {
        let key = match self.key {
            Some(key) => key,
            None => {
                let mut key = 0isize;
                if unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, wide(&self.path).as_ptr(), 0, KEY_QUERY_VALUE, &mut key) } != 0 {
                    return None;
                }
                self.key = Some(key);
                key
            }
        };
        let mut time = 0u64;
        let none = std::ptr::null_mut::<u32>;
        let status = unsafe { RegQueryInfoKeyW(key, std::ptr::null_mut(), none(), none(), none(), none(), none(), none(), none(), none(), none(), &mut time) };
        if status != 0 {
            // Deleted and made again (GG reinstalled): opened afresh next time.
            self.close();
            return None;
        }
        Some(time)
    }

    fn close(&mut self) {
        if let Some(key) = self.key.take() {
            unsafe { RegCloseKey(key) };
        }
    }
}

impl Drop for StoreClock {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(flow: Flow, state: State, role: Option<u32>) -> Endpoint {
        Endpoint { id: "{x}".into(), name: "X".into(), flow, state, virtual_role: role }
    }

    #[test]
    fn each_named_reference_is_kept_once() {
        let held = ["{arena}", "{stream}", "", "{ARENA}"].map(String::from);
        assert_eq!(references(&held), ["{arena}", "{stream}"]);
        assert!(references(&[" ".to_string(), String::new()]).is_empty());
    }

    #[test]
    fn only_active_outputs_get_a_stream_and_never_the_virtual_microphone() {
        assert_eq!(refusal(&endpoint(Flow::Render, State::Active, None)), None);
        // The Stream channel is a reference in streamer mode: it qualifies like a real output.
        assert_eq!(refusal(&endpoint(Flow::Render, State::Active, Some(4))), None);
        assert!(refusal(&endpoint(Flow::Render, State::Active, Some(MICROPHONE_ROLE))).is_some());
        assert!(refusal(&endpoint(Flow::Capture, State::Active, None)).is_some());
        assert_eq!(refusal(&endpoint(Flow::Render, State::Unplugged, None)), Some("unplugged"));
        assert_eq!(refusal(&endpoint(Flow::Render, State::Disabled, None)), Some("disabled"));
    }

    #[test]
    fn streams_follow_the_references() {
        let wanted = ["{arena}".to_string(), "{stream}".to_string()];
        assert_eq!(changes(&wanted, &[]), (wanted.to_vec(), Vec::new()));
        assert_eq!(changes(&wanted, &["{stream}", "{airpods}"]), (vec!["{arena}".to_string()], vec!["{airpods}".to_string()]));
        assert_eq!(changes(&wanted, &["{ARENA}", "{stream}"]), (Vec::new(), Vec::new()));
        assert_eq!(changes(&[], &["{arena}"]), (Vec::new(), vec!["{arena}".to_string()]));
    }

    #[test]
    fn a_missing_registry_key_tells_nothing() {
        let mut clock = StoreClock { path: r"SOFTWARE\Platinum test key that does not exist".into(), key: None };
        assert_eq!(clock.last_write(), None);
        assert_eq!(clock.key, None);
    }
}
