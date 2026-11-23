//! The thread that plays the test sounds and runs the microphone test (see `sounds`). One sound at
//! a time for the whole window: anything new the window asks for ends what is under way first.
//! The thread is started by the first button pressed, never in the layout check; when the window
//! closes, the request channel closes with it, and whatever plays or records stops within a tick.
//!
//! What each request opens, all in shared mode, released as soon as it ends:
//! - `Play`: a render stream on the channel's virtual device ("Gaming (Platinum)"…, found by the
//!   role the driver gives it), in the file's own format, which Windows converts: the sound goes
//!   through the channel's effects to its real output, exactly as an application's would;
//! - `Record`: a capture stream on the recording side of the virtual microphone ("Microphone
//!   (Platinum)"), what Discord records, in its own format. The audio stays in memory (at most
//!   30 s), is never written anywhere, and is dropped by the next recording or with the window;
//! - `Listen`: a render stream on the device the window names (the Chat channel's real output),
//!   in the recording's format.

use crate::sounds::{self, Clip, MicEvent, MicPhase, Recording, Sample};
use platinum::devices::{Devices, Flow, State};
use platinum::dsp::peak;
use platinum::i18n::{mark, tr};
use platinum::trf;
use platinum::session;
use platinum::wasapi::{AudioClient, CONVERT, Format};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

pub enum Request {
    /// Plays a channel's test sound (channel key, sound), from the start.
    Play(String, Sample),
    /// Starts a new recording of the virtual microphone; the previous one is dropped.
    Record,
    /// Plays the recording on a real output, by endpoint identifier.
    Listen(String),
    /// Ends what plays or records (GG's pause, "Arrêter").
    Stop,
}

/// What the window shows of the thread's work.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    /// The test sound playing: its channel's key and the sound.
    pub playing: Option<(String, Sample)>,
    pub mic: MicPhase,
    /// Length of the recording kept, in seconds.
    pub recorded: f32,
    /// While recording or listening: how far it is, in seconds.
    pub position: f32,
    /// While recording: the level, 0..1 on the meters' scale.
    pub level: f32,
    /// What went wrong last, in French, and the tab it belongs to (a channel's key); cleared by the
    /// next request.
    pub note: Option<(String, String)>,
}

/// Room asked for in the streams' buffers: enough to ride out a slow wake-up.
const BUFFER_MS: u32 = 200;
/// How often the streams are served and requests looked at.
const TICK: Duration = Duration::from_millis(10);
/// How often progress is sent to the window: the meters' pace.
const PUBLISH_EVERY: Duration = Duration::from_millis(50);
/// After the last sample is queued: once the device has taken everything, this much more is left
/// for the audio engine to play out what it already took; and the longest wait for the device
/// to take it at all.
const TAIL: Duration = Duration::from_millis(40);
const DRAIN_LIMIT: Duration = Duration::from_millis(BUFFER_MS as u64 + 800);
/// The key of the Micro tab, which the microphone test's notes belong to.
pub const MIC_KEY: &str = "mic";

/// How a stream ended.
enum Ended {
    /// It played to the end, or the recording is full.
    Finished,
    /// The window asked for something (handed back, to be carried out next).
    Interrupted(Request),
    /// The window closed.
    Closed,
}

fn text<E: ToString>(error: E) -> String {
    error.to_string()
}

/// Stops a started stream however its loop ends (stopping before release avoids a click).
struct Started<'a>(&'a AudioClient);

impl Drop for Started<'_> {
    fn drop(&mut self) {
        let _ = self.0.stop();
    }
}

/// Runs until the window closes. `publish` receives every change of `Status`, and progress at most
/// every `PUBLISH_EVERY`.
pub fn run(requests: Receiver<Request>, publish: impl Fn(Status)) {
    let mut worker = Worker { requests: &requests, publish: &publish, status: Status::default(), recording: None, published: Instant::now(), level: 0.0 };
    let mut next = requests.recv().ok();
    while let Some(request) = next {
        next = match worker.handle(request) {
            Some(request) => Some(request),
            None => requests.recv().ok(),
        };
    }
    // The recording goes with the worker: nothing of it outlives the window.
}

struct Worker<'a> {
    requests: &'a Receiver<Request>,
    publish: &'a dyn Fn(Status),
    status: Status,
    recording: Option<Recording>,
    /// When progress was last sent, and the loudest level read since.
    published: Instant,
    level: f32,
}

impl Worker<'_> {
    fn send(&mut self) {
        (self.publish)(self.status.clone());
        self.published = Instant::now();
        self.level = 0.0;
    }

    /// Progress of a stream: sent at the meters' pace, with the loudest level read in between.
    fn progress(&mut self, position: f32, level: f32) {
        self.status.position = position;
        self.level = self.level.max(level);
        if self.published.elapsed() >= PUBLISH_EVERY {
            self.status.level = crate::meters::shown_level(self.level);
            self.send();
        }
    }

    /// Carries out a request; returns one that came in meanwhile and is to be carried out next.
    fn handle(&mut self, request: Request) -> Option<Request> {
        match request {
            Request::Play(channel, sample) => self.play_sample(channel, sample),
            Request::Record => self.record(),
            Request::Listen(device) => self.listen(&device),
            // Nothing under way: nothing to stop.
            Request::Stop => None,
        }
    }

    /// Hands back what interrupted a stream, unless it was only meant to stop it.
    fn after(&mut self, ended: Result<Ended, String>, tab: &str, what: &str) -> Option<Request> {
        match ended {
            Ok(Ended::Interrupted(Request::Stop) | Ended::Finished | Ended::Closed) => None,
            Ok(Ended::Interrupted(request)) => Some(request),
            Err(error) => {
                self.status.note = Some((tab.to_string(), trf!("{text} : {detail}", text = tr(what), detail = error)));
                None
            }
        }
    }

    fn play_sample(&mut self, channel: String, sample: Sample) -> Option<Request> {
        self.status.note = None;
        self.status.playing = Some((channel.clone(), sample));
        self.send();
        let ended = self.try_play_sample(&channel, sample);
        self.status.playing = None;
        let next = self.after(ended, &channel, mark("Son de test non joué"));
        self.send();
        next
    }

    fn try_play_sample(&mut self, channel: &str, sample: Sample) -> Result<Ended, String> {
        // GG's file, or a generated stand-in when GG is not installed.
        let clip = sounds::load(sample)?;
        let devices = Devices::open().map_err(text)?;
        let endpoints = devices.list().map_err(text)?;
        let endpoint = session::channel_output(&endpoints, channel).ok_or(tr("le périphérique virtuel de ce canal est introuvable"))?;
        if endpoint.state != State::Active {
            return Err(trf!("{device} n'est pas actif : démarrez Platinum", device = endpoint.name));
        }
        let requests = self.requests;
        render(&devices, &endpoint.id, &clip, requests, |_| {})
    }

    fn record(&mut self) -> Option<Request> {
        self.status.note = None;
        // A new recording replaces the previous one, which goes at once.
        self.recording = None;
        self.status.mic = self.status.mic.next(MicEvent::Record).unwrap_or(MicPhase::Recording);
        self.status.recorded = 0.0;
        self.status.position = 0.0;
        self.status.level = 0.0;
        self.send();
        let ended = self.try_record();
        let captured = self.recording.as_ref().is_some_and(|r| !r.is_empty());
        if !captured {
            self.recording = None;
        }
        self.status.mic = self.status.mic.next(MicEvent::RecordingEnded { captured }).unwrap_or(MicPhase::Idle);
        self.status.recorded = self.recording.as_ref().map_or(0.0, |r| r.clip.seconds());
        self.status.position = 0.0;
        self.status.level = 0.0;
        let next = self.after(ended, MIC_KEY, mark("Enregistrement interrompu"));
        self.send();
        next
    }

    fn try_record(&mut self) -> Result<Ended, String> {
        let devices = Devices::open().map_err(text)?;
        let endpoints = devices.list().map_err(text)?;
        let microphone = endpoints
            .iter()
            .find(|e| e.flow == Flow::Capture && e.state == State::Active && session::virtual_kind(e).as_deref() == Some("microphone"))
            .ok_or(tr("le micro virtuel n'est pas actif : démarrez Platinum"))?;
        let client = AudioClient::activate(&devices, &microphone.id).map_err(text)?;
        let mix = client.mix_format(devices.ole()).map_err(text)?;
        let format = mix.as_float_at(mix.rate);
        client.initialize(CONVERT, BUFFER_MS, &format).map_err(text)?;
        let reader = client.capture_client().map_err(text)?;
        let channels = usize::from(format.channels);
        self.recording = Some(Recording::new(format.rate, format.channels, format.channel_mask));
        client.start().map_err(text)?;
        let _started = Started(&client);
        loop {
            match self.requests.recv_timeout(TICK) {
                Ok(request) => return Ok(Ended::Interrupted(request)),
                Err(RecvTimeoutError::Disconnected) => return Ok(Ended::Closed),
                Err(RecvTimeoutError::Timeout) => {}
            }
            let Some(recording) = self.recording.as_mut() else { return Ok(Ended::Finished) };
            let (mut level, mut full) = (0.0f32, false);
            while !full
                && reader
                    .read(channels, |samples, frames| {
                        level = level.max(samples.map_or(0.0, peak));
                        full = recording.push(samples, frames);
                    })
                    .map_err(text)?
            {}
            let seconds = recording.clip.seconds();
            self.progress(seconds, level);
            if full {
                return Ok(Ended::Finished);
            }
        }
    }

    fn listen(&mut self, device: &str) -> Option<Request> {
        self.status.note = None;
        let phase = self.status.mic.next(MicEvent::Listen);
        let (Ok(phase), Some(recording)) = (phase, self.recording.take_if(|_| phase.is_ok())) else {
            self.status.note = Some((MIC_KEY.to_string(), sounds::NOTHING_RECORDED.to_string()));
            self.send();
            return None;
        };
        self.status.mic = phase;
        self.status.position = 0.0;
        self.send();
        let ended = self.try_listen(device, &recording.clip);
        self.recording = Some(recording);
        self.status.mic = self.status.mic.next(MicEvent::ListeningEnded).unwrap_or(MicPhase::Recorded);
        self.status.position = 0.0;
        let next = self.after(ended, MIC_KEY, mark("Écoute impossible"));
        self.send();
        next
    }

    fn try_listen(&mut self, device: &str, clip: &Clip) -> Result<Ended, String> {
        let devices = Devices::open().map_err(text)?;
        let endpoints = devices.list().map_err(text)?;
        let output = endpoints.iter().find(|e| e.id == device && e.flow == Flow::Render).ok_or(sounds::NO_CHAT_OUTPUT)?;
        if output.state != State::Active {
            return Err(trf!("{device} n'est pas branché", device = output.name));
        }
        // The recording was taken out of the worker for the time it plays: the worker is free to
        // send the progress.
        let requests = self.requests;
        render(&devices, device, clip, requests, |seconds| self.progress(seconds, 0.0))
    }
}

/// Plays `clip` on an endpoint until it ends, the window asks for something else, or the window
/// closes. `heard` gets the position, in seconds, every tick.
fn render(devices: &Devices, id: &str, clip: &Clip, requests: &Receiver<Request>, mut heard: impl FnMut(f32)) -> Result<Ended, String> {
    let client = AudioClient::activate(devices, id).map_err(text)?;
    let format = Format { rate: clip.rate, channels: clip.channels, channel_mask: clip.channel_mask, bits: 32, float: true };
    client.initialize(CONVERT, BUFFER_MS, &format).map_err(text)?;
    let writer = client.render_client().map_err(text)?;
    let capacity = client.buffer_frames().map_err(text)?;
    let channels = usize::from(clip.channels);
    let total = clip.len();
    let mut written = 0usize;
    // Queues as much as the buffer takes, a contiguous run at a time.
    let fill = |written: &mut usize, queued: usize| -> Result<(), String> {
        let mut room = capacity.saturating_sub(queued) * channels;
        while room > 0 && *written < total {
            let run = clip.run(*written, room);
            if run.is_empty() {
                break;
            }
            writer.write(run, channels).map_err(text)?;
            *written += run.len();
            room -= run.len();
        }
        Ok(())
    };
    fill(&mut written, 0)?;
    // Decoding took a moment: a stop asked meanwhile, or the window closed, plays nothing at all.
    match requests.try_recv() {
        Ok(request) => return Ok(Ended::Interrupted(request)),
        Err(std::sync::mpsc::TryRecvError::Disconnected) => return Ok(Ended::Closed),
        Err(std::sync::mpsc::TryRecvError::Empty) => {}
    }
    client.start().map_err(text)?;
    let _started = Started(&client);
    let mut queued_all: Option<Instant> = None;
    let mut drained: Option<Instant> = None;
    loop {
        match requests.recv_timeout(TICK) {
            Ok(request) => return Ok(Ended::Interrupted(request)),
            Err(RecvTimeoutError::Disconnected) => return Ok(Ended::Closed),
            Err(RecvTimeoutError::Timeout) => {}
        }
        let queued = client.queued_frames().map_err(text)?;
        if written < total {
            fill(&mut written, queued)?;
        } else {
            let since = *queued_all.get_or_insert_with(Instant::now);
            if queued == 0 {
                if drained.get_or_insert_with(Instant::now).elapsed() >= TAIL {
                    return Ok(Ended::Finished);
                }
            } else if since.elapsed() >= DRAIN_LIMIT {
                // The device stopped taking audio: nothing more will be heard.
                return Ok(Ended::Finished);
            }
        }
        heard((written / channels).saturating_sub(queued) as f32 / clip.rate.max(1) as f32);
    }
}
