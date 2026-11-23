//! Thin wrappers over the Windows audio session API (WASAPI): shared-mode capture and render
//! streams in 32-bit float, and the endpoint peak meter. Everything here is documented Windows API.

use crate::com::{Com, Error, Guid, Ole, Result, check, export, load_library, wide};
use crate::devices::Devices;
use std::ffi::c_void;

const IID_AUDIO_CLIENT: Guid = Guid { d1: 0x1CB9_AD4C, d2: 0xDBFA, d3: 0x4C32, d4: [0xB1, 0x78, 0xC2, 0xF5, 0x68, 0xA7, 0x03, 0xB2] };
const IID_CAPTURE_CLIENT: Guid = Guid { d1: 0xC8AD_BD64, d2: 0xE71E, d3: 0x48A0, d4: [0xA4, 0xDE, 0x18, 0x5C, 0x39, 0x5C, 0xD3, 0x17] };
const IID_RENDER_CLIENT: Guid = Guid { d1: 0xF294_ACFC, d2: 0x3146, d3: 0x4483, d4: [0xA7, 0xBF, 0xAD, 0xDC, 0xA7, 0xC2, 0x60, 0xE2] };
const IID_METER: Guid = Guid { d1: 0xC022_16F6, d2: 0x8C67, d3: 0x4B5B, d4: [0x9D, 0x00, 0xD0, 0x08, 0xE7, 0x3E, 0x00, 0x64] };
const SUBTYPE_FLOAT: Guid = Guid { d1: 0x0000_0003, d2: 0x0000, d3: 0x0010, d4: [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71] };

const FORMAT_FLOAT: u16 = 3;
const FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// The stream wakes an event each time data is ready.
pub const EVENT_DRIVEN: u32 = 0x0004_0000;
/// Let Windows convert between the format asked for and the device's own (rate, channels, sample type).
pub const CONVERT: u32 = 0x8000_0000 | 0x0800_0000;

/// Marks a written buffer as silence, whatever it contains.
const BUFFER_SILENT: u32 = 2;

// Vtable positions (IUnknown takes entries 0 to 2).
const CLIENT_INITIALIZE: usize = 3;
const CLIENT_BUFFER_SIZE: usize = 4;
const CLIENT_PADDING: usize = 6;
const CLIENT_MIX_FORMAT: usize = 8;
const CLIENT_PERIOD: usize = 9;
const CLIENT_START: usize = 10;
const CLIENT_STOP: usize = 11;
const CLIENT_SET_EVENT: usize = 13;
const CLIENT_SERVICE: usize = 14;
const CAPTURE_GET: usize = 3;
const CAPTURE_RELEASE: usize = 4;
const CAPTURE_NEXT_SIZE: usize = 5;
const RENDER_GET: usize = 3;
const RENDER_RELEASE: usize = 4;
const METER_PEAK: usize = 3;

/// WAVEFORMATEXTENSIBLE; the first 18 bytes are a WAVEFORMATEX. Windows declares it byte-packed.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub(crate) struct WaveFormat {
    tag: u16,
    channels: u16,
    rate: u32,
    bytes_per_second: u32,
    block_align: u16,
    bits: u16,
    extra_size: u16,
    valid_bits: u16,
    channel_mask: u32,
    sub_format: Guid,
}

const _: () = assert!(std::mem::size_of::<WaveFormat>() == 40);
/// Size of the part every format has (WAVEFORMATEX).
const BASE_SIZE: usize = 18;
/// Value of `extra_size` when the extensible part is present.
const EXTENSION_SIZE: u16 = 22;

/// What matters to us in a device's format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub rate: u32,
    pub channels: u16,
    /// Which speaker each channel is (Windows speaker-position bits).
    pub channel_mask: u32,
    pub bits: u16,
    pub float: bool,
}

impl Format {
    /// The same layout carried as 32-bit float at `rate`: the format our streams ask for.
    pub fn as_float_at(self, rate: u32) -> Format {
        Format { rate, bits: 32, float: true, ..self }
    }

    pub(crate) fn to_wave(self) -> WaveFormat {
        let block_align = self.channels * (self.bits / 8);
        WaveFormat {
            tag: FORMAT_EXTENSIBLE,
            channels: self.channels,
            rate: self.rate,
            bytes_per_second: self.rate * u32::from(block_align),
            block_align,
            bits: self.bits,
            extra_size: EXTENSION_SIZE,
            valid_bits: self.bits,
            channel_mask: self.channel_mask,
            sub_format: SUBTYPE_FLOAT,
        }
    }
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} Hz, {} channel{}, {}-bit {}", self.rate, self.channels, if self.channels == 1 { "" } else { "s" }, self.bits, if self.float { "float" } else { "integer" })
    }
}

/// Reads a WAVEFORMATEX, or the WAVEFORMATEXTENSIBLE it starts, from memory owned by someone else.
/// `raw` must point to a valid format structure.
pub(crate) unsafe fn read_format(raw: *const u8) -> Format {
    unsafe {
        // Read the common part first: the extensible part is only there when the format says so.
        let mut wave: WaveFormat = std::mem::zeroed();
        std::ptr::copy_nonoverlapping(raw, (&raw mut wave).cast::<u8>(), BASE_SIZE);
        let extensible = wave.tag == FORMAT_EXTENSIBLE && wave.extra_size >= EXTENSION_SIZE;
        if extensible {
            std::ptr::copy_nonoverlapping(raw, (&raw mut wave).cast::<u8>(), std::mem::size_of::<WaveFormat>());
        }
        let (channels, sub_format) = (wave.channels, wave.sub_format);
        Format {
            rate: wave.rate,
            channels,
            channel_mask: if extensible && wave.channel_mask != 0 { wave.channel_mask } else { default_mask(channels) },
            bits: wave.bits,
            float: wave.tag == FORMAT_FLOAT || (extensible && sub_format == SUBTYPE_FLOAT),
        }
    }
}

/// Speaker positions Windows assumes when a format does not say: front centre for mono, front left
/// and right for stereo, the first positions in order beyond that.
fn default_mask(channels: u16) -> u32 {
    match channels {
        1 => 0x4,
        2 => 0x3,
        n => (1u32 << n.min(18)) - 1,
    }
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateEventW(security: *mut c_void, manual_reset: i32, initial_state: i32, name: *const u16) -> *mut c_void;
    fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

/// An auto-reset event, signalled by a stream when data is ready.
pub struct Event(*mut c_void);

impl Event {
    pub fn new() -> Result<Event> {
        let handle = unsafe { CreateEventW(std::ptr::null_mut(), 0, 0, std::ptr::null()) };
        if handle.is_null() { Err(Error::Invalid("cannot create an event".into())) } else { Ok(Event(handle)) }
    }

    /// Waits until the event is signalled or the time is up.
    pub fn wait(&self, milliseconds: u32) {
        unsafe {
            WaitForSingleObject(self.0, milliseconds);
        }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// One endpoint's audio client: describes the device, then becomes a stream once initialised.
pub struct AudioClient(Com);

impl AudioClient {
    /// Obtains the audio client of an endpoint. Nothing is opened until `initialize`.
    pub fn activate(devices: &Devices, id: &str) -> Result<AudioClient> {
        Ok(AudioClient(devices.activate(id, &IID_AUDIO_CLIENT, "activating the audio client")?))
    }

    /// The format the Windows audio engine runs this device at in shared mode.
    pub fn mix_format(&self, ole: &Ole) -> Result<Format> {
        unsafe {
            let get: unsafe extern "system" fn(*mut c_void, *mut *mut u8) -> i32 = std::mem::transmute(self.0.entry(CLIENT_MIX_FORMAT));
            let mut raw = std::ptr::null_mut();
            check("IAudioClient::GetMixFormat", get(self.0.0, &mut raw))?;
            let format = read_format(raw);
            ole.free(raw.cast());
            Ok(format)
        }
    }

    /// How often the audio engine exchanges data with this device, in frames at `rate`.
    pub fn period_frames(&self, rate: u32) -> Result<usize> {
        unsafe {
            let get: unsafe extern "system" fn(*mut c_void, *mut i64, *mut i64) -> i32 = std::mem::transmute(self.0.entry(CLIENT_PERIOD));
            let (mut default, mut minimum) = (0i64, 0i64);
            check("IAudioClient::GetDevicePeriod", get(self.0.0, &mut default, &mut minimum))?;
            // Periods are in units of 100 nanoseconds.
            Ok((i128::from(default) * i128::from(rate) / 10_000_000) as usize)
        }
    }

    /// Opens a shared-mode stream in `format` (32-bit float) with room for `buffer_ms` of audio.
    pub fn initialize(&self, flags: u32, buffer_ms: u32, format: &Format) -> Result<()> {
        unsafe {
            let wave = format.to_wave();
            let call: unsafe extern "system" fn(*mut c_void, i32, u32, i64, i64, *const WaveFormat, *const Guid) -> i32 = std::mem::transmute(self.0.entry(CLIENT_INITIALIZE));
            // 0 = shared mode; durations are in units of 100 nanoseconds.
            check("IAudioClient::Initialize", call(self.0.0, 0, flags, i64::from(buffer_ms) * 10_000, 0, &wave, std::ptr::null()))
        }
    }

    /// Capacity of the stream's buffer, in frames.
    pub fn buffer_frames(&self) -> Result<usize> {
        self.count(CLIENT_BUFFER_SIZE, "IAudioClient::GetBufferSize")
    }

    /// Frames queued in a render stream that the device has not consumed yet.
    pub fn queued_frames(&self) -> Result<usize> {
        self.count(CLIENT_PADDING, "IAudioClient::GetCurrentPadding")
    }

    fn count(&self, entry: usize, what: &'static str) -> Result<usize> {
        unsafe {
            let call: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32 = std::mem::transmute(self.0.entry(entry));
            let mut frames = 0;
            check(what, call(self.0.0, &mut frames))?;
            Ok(frames as usize)
        }
    }

    pub fn set_event(&self, event: &Event) -> Result<()> {
        unsafe {
            let call: unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32 = std::mem::transmute(self.0.entry(CLIENT_SET_EVENT));
            check("IAudioClient::SetEventHandle", call(self.0.0, event.0))
        }
    }

    pub fn start(&self) -> Result<()> {
        self.simple(CLIENT_START, "IAudioClient::Start")
    }

    pub fn stop(&self) -> Result<()> {
        self.simple(CLIENT_STOP, "IAudioClient::Stop")
    }

    fn simple(&self, entry: usize, what: &'static str) -> Result<()> {
        unsafe {
            let call: unsafe extern "system" fn(*mut c_void) -> i32 = std::mem::transmute(self.0.entry(entry));
            check(what, call(self.0.0))
        }
    }

    fn service(&self, interface: &Guid, what: &'static str) -> Result<Com> {
        unsafe {
            let call: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32 = std::mem::transmute(self.0.entry(CLIENT_SERVICE));
            let mut raw = std::ptr::null_mut();
            check(what, call(self.0.0, interface, &mut raw))?;
            Ok(Com(raw))
        }
    }

    /// The reading side of an initialised capture stream.
    pub fn capture_client(&self) -> Result<CaptureClient> {
        Ok(CaptureClient(self.service(&IID_CAPTURE_CLIENT, "getting the capture client")?))
    }

    /// The writing side of an initialised render stream.
    pub fn render_client(&self) -> Result<RenderClient> {
        Ok(RenderClient(self.service(&IID_RENDER_CLIENT, "getting the render client")?))
    }
}

pub struct CaptureClient(Com);

impl CaptureClient {
    /// Hands the next captured packet to `sink` as interleaved float samples and a frame count;
    /// the samples are `None` when the packet is silence. Returns false when nothing is waiting.
    pub fn read(&self, channels: usize, sink: impl FnOnce(Option<&[f32]>, usize)) -> Result<bool> {
        unsafe {
            let next: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32 = std::mem::transmute(self.0.entry(CAPTURE_NEXT_SIZE));
            let mut frames = 0u32;
            check("IAudioCaptureClient::GetNextPacketSize", next(self.0.0, &mut frames))?;
            if frames == 0 {
                return Ok(false);
            }
            let get: unsafe extern "system" fn(*mut c_void, *mut *mut u8, *mut u32, *mut u32, *mut u64, *mut u64) -> i32 = std::mem::transmute(self.0.entry(CAPTURE_GET));
            let (mut data, mut flags) = (std::ptr::null_mut(), 0u32);
            check("IAudioCaptureClient::GetBuffer", get(self.0.0, &mut data, &mut frames, &mut flags, std::ptr::null_mut(), std::ptr::null_mut()))?;
            if frames == 0 {
                return Ok(false);
            }
            if flags & BUFFER_SILENT != 0 || data.is_null() {
                sink(None, frames as usize);
            } else {
                sink(Some(std::slice::from_raw_parts(data.cast::<f32>(), frames as usize * channels)), frames as usize);
            }
            let release: unsafe extern "system" fn(*mut c_void, u32) -> i32 = std::mem::transmute(self.0.entry(CAPTURE_RELEASE));
            check("IAudioCaptureClient::ReleaseBuffer", release(self.0.0, frames))?;
            Ok(true)
        }
    }
}

pub struct RenderClient(Com);

impl RenderClient {
    /// Queues interleaved float samples. The caller makes sure they fit in the free part of the buffer.
    pub fn write(&self, samples: &[f32], channels: usize) -> Result<()> {
        let frames = samples.len() / channels;
        if frames == 0 {
            return Ok(());
        }
        unsafe {
            let data = self.buffer(frames)?;
            std::ptr::copy_nonoverlapping(samples.as_ptr(), data.cast::<f32>(), frames * channels);
            self.release(frames, 0)
        }
    }

    /// Queues `frames` frames of silence.
    pub fn write_silence(&self, frames: usize) -> Result<()> {
        if frames == 0 {
            return Ok(());
        }
        unsafe {
            self.buffer(frames)?;
            self.release(frames, BUFFER_SILENT)
        }
    }

    unsafe fn buffer(&self, frames: usize) -> Result<*mut u8> {
        unsafe {
            let get: unsafe extern "system" fn(*mut c_void, u32, *mut *mut u8) -> i32 = std::mem::transmute(self.0.entry(RENDER_GET));
            let mut data = std::ptr::null_mut();
            check("IAudioRenderClient::GetBuffer", get(self.0.0, frames as u32, &mut data))?;
            Ok(data)
        }
    }

    unsafe fn release(&self, frames: usize, flags: u32) -> Result<()> {
        unsafe {
            let release: unsafe extern "system" fn(*mut c_void, u32, u32) -> i32 = std::mem::transmute(self.0.entry(RENDER_RELEASE));
            check("IAudioRenderClient::ReleaseBuffer", release(self.0.0, frames as u32, flags))
        }
    }
}

/// Frames of silence that fill a render buffer of `capacity` frames holding `queued` frames up again.
pub fn silence_to_queue(capacity: usize, queued: usize) -> usize {
    capacity.saturating_sub(queued)
}

/// A shared-mode render stream that plays an inaudible noise floor. Windows feeds the loopback
/// captures of an endpoint only while something plays on it; this stream keeps them fed, as the
/// vendor's service does on the outputs its effects engine listens to.
///
/// Not plain silence: frames marked `AUDCLNT_BUFFERFLAGS_SILENT` reach the loopback as packets
/// flagged silent, and the vendor's engine keeps losing memory on those (measured on 17/11/2026:
/// +6.5 MB/min with silent references, against ~2 MB/min while real sound played). The stream
/// writes real samples instead, a pseudo-random noise at about -120 dBFS (`NOISE_FLOOR`), far below
/// any speaker's own hiss and below one step of a 16-bit device.
///
/// Timer-driven, with a long buffer: the owner calls `top_up` well within `buffer_ms`.
pub struct SilentStream {
    // Declared first: released before the client it comes from.
    render: RenderClient,
    client: AudioClient,
    capacity: usize,
    channels: usize,
    /// The noise to queue, one buffer's worth, made once: `top_up` copies a part of it.
    noise: Vec<f32>,
}

/// Peak level of the keep-alive noise: about -120 dBFS.
pub const NOISE_FLOOR: f32 = 1.0e-6;

/// `count` samples of pseudo-random noise within `±level`, never exactly zero, the same for a
/// given `seed` (a 32-bit xorshift: no dependency, and good enough for a noise nobody hears).
pub fn noise_floor(count: usize, level: f32, seed: u32) -> Vec<f32> {
    let mut state = seed.max(1);
    (0..count)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            // From (0, 1], centred: never zero, so no packet ever looks silent.
            let unit = (state as f32 + 1.0) / (u32::MAX as f32 + 1.0);
            let value = (unit * 2.0 - 1.0) * level;
            if value == 0.0 { level } else { value }
        })
        .collect()
}

impl SilentStream {
    /// Opens the stream on an output, in its own mix format, fills its buffer with the noise floor
    /// and starts it.
    pub fn open(devices: &Devices, id: &str, buffer_ms: u32) -> Result<SilentStream> {
        let client = AudioClient::activate(devices, id)?;
        let mix = client.mix_format(devices.ole())?;
        let format = mix.as_float_at(mix.rate);
        client.initialize(CONVERT, buffer_ms, &format)?;
        let render = client.render_client()?;
        let capacity = client.buffer_frames()?;
        let channels = usize::from(format.channels).max(1);
        let noise = noise_floor(capacity * channels, NOISE_FLOOR, 0x9E37_79B9);
        render.write(&noise, channels)?;
        client.start()?;
        Ok(SilentStream { render, client, capacity, channels, noise })
    }

    /// Fills the buffer up again with the noise floor; returns how many frames were queued. Fails
    /// once the device is gone: unplugged, disabled, taken in exclusive mode, or Windows' audio
    /// service restarted.
    pub fn top_up(&self) -> Result<usize> {
        let frames = silence_to_queue(self.capacity, self.client.queued_frames()?);
        let samples = (frames * self.channels).min(self.noise.len());
        self.render.write(&self.noise[..samples], self.channels)?;
        Ok(samples / self.channels)
    }
}

impl Drop for SilentStream {
    fn drop(&mut self) {
        // The stream closes when its clients are released.
        let _ = self.client.stop();
    }
}

/// The peak meter Windows keeps for every endpoint. Reading it opens no stream; on a microphone it
/// only moves while some application is recording from it.
pub struct Meter(Com);

impl Meter {
    pub fn activate(devices: &Devices, id: &str) -> Result<Meter> {
        Ok(Meter(devices.activate(id, &IID_METER, "activating the peak meter")?))
    }

    /// Peak level since the previous reading, from 0 to 1.
    pub fn peak(&self) -> Result<f32> {
        unsafe {
            let call: unsafe extern "system" fn(*mut c_void, *mut f32) -> i32 = std::mem::transmute(self.0.entry(METER_PEAK));
            let mut peak = 0.0;
            check("IAudioMeterInformation::GetPeakValue", call(self.0.0, &mut peak))?;
            Ok(peak)
        }
    }
}

const IID_ENDPOINT_VOLUME: Guid = Guid { d1: 0x5CDF_2C82, d2: 0x841E, d3: 0x4546, d4: [0x97, 0x22, 0x0C, 0xF7, 0x40, 0x78, 0x22, 0x9A] };

/// The volume and mute of a device, as the Windows volume mixer shows them.
pub struct EndpointVolume(Com);

impl EndpointVolume {
    // Vtable positions (IUnknown takes 0 to 2).
    const SET_SCALAR: usize = 7;
    const GET_SCALAR: usize = 9;
    const SET_MUTE: usize = 14;
    const GET_MUTE: usize = 15;

    pub fn activate(devices: &Devices, id: &str) -> Result<EndpointVolume> {
        Ok(EndpointVolume(devices.activate(id, &IID_ENDPOINT_VOLUME, "activating the device volume")?))
    }

    /// Volume from 0 to 1 (Windows' slider position).
    pub fn level(&self) -> Result<f32> {
        unsafe {
            let call: unsafe extern "system" fn(*mut c_void, *mut f32) -> i32 = std::mem::transmute(self.0.entry(Self::GET_SCALAR));
            let mut level = 0.0;
            check("IAudioEndpointVolume::GetMasterVolumeLevelScalar", call(self.0.0, &mut level))?;
            Ok(level)
        }
    }

    pub fn set_level(&self, level: f32) -> Result<()> {
        unsafe {
            let call: unsafe extern "system" fn(*mut c_void, f32, *const Guid) -> i32 = std::mem::transmute(self.0.entry(Self::SET_SCALAR));
            check("IAudioEndpointVolume::SetMasterVolumeLevelScalar", call(self.0.0, level.clamp(0.0, 1.0), std::ptr::null()))
        }
    }

    pub fn muted(&self) -> Result<bool> {
        unsafe {
            let call: unsafe extern "system" fn(*mut c_void, *mut i32) -> i32 = std::mem::transmute(self.0.entry(Self::GET_MUTE));
            let mut muted = 0;
            check("IAudioEndpointVolume::GetMute", call(self.0.0, &mut muted))?;
            Ok(muted != 0)
        }
    }

    pub fn set_muted(&self, muted: bool) -> Result<()> {
        unsafe {
            let call: unsafe extern "system" fn(*mut c_void, i32, *const Guid) -> i32 = std::mem::transmute(self.0.entry(Self::SET_MUTE));
            check("IAudioEndpointVolume::SetMute", call(self.0.0, i32::from(muted), std::ptr::null()))
        }
    }
}

/// Asks Windows to schedule the calling thread as an audio thread. Best effort: without it the
/// thread simply keeps its normal priority.
pub fn prioritize_audio_thread() {
    let Ok(module) = load_library("avrt.dll", 0) else { return };
    let Ok(address) = export(module, b"AvSetMmThreadCharacteristicsW\0") else { return };
    unsafe {
        let set: unsafe extern "system" fn(*const u16, *mut u32) -> *mut c_void = std::mem::transmute(address);
        let mut task_index = 0u32;
        set(wide("Pro Audio").as_ptr(), &mut task_index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_keep_alive_noise_is_inaudible_but_never_silent() {
        let noise = noise_floor(48_000, NOISE_FLOOR, 7);
        assert!(noise.iter().all(|v| *v != 0.0 && v.abs() <= NOISE_FLOOR));
        // About -120 dBFS at its peak, far below a 16-bit step (-96 dBFS).
        let peak = noise.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(20.0 * peak.log10() < -119.0);
        // Spread on both sides of zero, so it adds no offset.
        let mean = noise.iter().map(|v| f64::from(*v)).sum::<f64>() / noise.len() as f64;
        assert!(mean.abs() < f64::from(NOISE_FLOOR) * 0.05);
        assert_eq!(noise_floor(16, NOISE_FLOOR, 7), noise_floor(16, NOISE_FLOOR, 7));
    }

    #[test]
    fn silence_fills_the_buffer_up_and_never_more() {
        assert_eq!(silence_to_queue(48_000, 0), 48_000);
        assert_eq!(silence_to_queue(48_000, 30_000), 18_000);
        assert_eq!(silence_to_queue(48_000, 48_000), 0);
        // A device that reports more than its capacity gets nothing rather than an overflow.
        assert_eq!(silence_to_queue(48_000, 50_000), 0);
    }

    #[test]
    fn streams_ask_for_the_device_layout_in_float() {
        let mix = Format { rate: 44_100, channels: 6, channel_mask: 0x3F, bits: 16, float: false };
        let asked = mix.as_float_at(mix.rate);
        assert_eq!(asked, Format { rate: 44_100, channels: 6, channel_mask: 0x3F, bits: 32, float: true });
        let wave = asked.to_wave();
        let (block_align, bytes_per_second) = (wave.block_align, wave.bytes_per_second);
        assert_eq!((block_align, bytes_per_second), (24, 44_100 * 24));
    }
}
