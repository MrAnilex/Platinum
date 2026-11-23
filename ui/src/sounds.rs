//! The test sounds and the microphone test of the channel tabs (GG's "Tester", H11 and H12), the
//! parts that need no device: which sounds each channel offers, where GG installs them, decoding
//! them, the recording buffer, the microphone test's states and what the window says about them.
//! The thread that plays and records is `tester`.
//!
//! The sounds are GG's own files (Ogg Vorbis, in `%ProgramData%\SteelSeries\GG\apps\sonar\
//! audio_samples`). They are read where GG installed them, at the moment they are played, and
//! never copied: Platinum ships none of SteelSeries' files. Without them (GG not installed), the
//! buttons play generated sounds instead (`synthetic`, from `platinum::testsignal`), and say so.
//!
//! The recording lives in memory only, at most `RECORD_LIMIT_SECONDS` long: it is never written
//! anywhere, and it goes away with the window or the next recording.

use platinum::i18n::{mark, tr};
use platinum::trf;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

/// The test sound of the channel tabs. Only GG's spatial test is offered (the user asked for it,
/// on 18/11/2026: people test the other sounds in real use, with their own games, calls and music);
/// it checks that each speaker of a 7.1 layout comes out where it should.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sample {
    VirtualSurround,
}

impl Sample {
    /// The file's name, as GG installs it.
    pub fn file_name(self) -> &'static str {
        match self {
            Sample::VirtualSurround => "VirtualSurround.ogg",
        }
    }

    /// The button's label (GG's `audioSamplePlayer.audioSamples`), in the current language.
    pub fn label(self) -> &'static str {
        match self {
            Sample::VirtualSurround => tr("Audio spatial"),
        }
    }
}

/// The sounds a channel tab offers: the spatial test on the channels that have spatial audio
/// (Gaming, Media, Aux). None for Chat, whose effects have no spatial audio (asked by the user on
/// 22/11/2026), nor for the microphone, whose tab has the microphone test instead.
pub fn samples_for(channel: &str) -> &'static [Sample] {
    match channel {
        "game" | "media" | "aux" => &[Sample::VirtualSurround],
        _ => &[],
    }
}

/// Where GG installs its test sounds.
pub fn sample_folder() -> PathBuf {
    let base = std::env::var_os("ProgramData").map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from);
    base.join(r"SteelSeries\GG\apps\sonar\audio_samples")
}

/// The sounds of `wanted` that are not in `folder`.
pub fn missing(folder: &Path, wanted: &[Sample]) -> Vec<Sample> {
    wanted.iter().copied().filter(|sample| !folder.join(sample.file_name()).is_file()).collect()
}

/// What the tab says when GG's sound is missing (GG is not installed, or no longer is): the button
/// still plays, a generated spatial test instead (`synthetic`). French, a catalog key.
pub const SYNTHETIC_NOTE: &str = mark("Son de test de GG absent : Platinum joue un test spatial de synthèse à la place");

/// The note shown under the test button when GG's sound is missing; `None` otherwise.
pub fn missing_note(missing: &[Sample]) -> Option<String> {
    (!missing.is_empty()).then(|| SYNTHETIC_NOTE.to_string())
}

/// Why a channel's test sound cannot be played, from what the window knows; `None` when it can.
/// GG's file missing is no reason: a generated test stands in for it.
pub fn sample_problem(has_output: bool) -> Option<String> {
    (!has_output).then(|| NO_OUTPUT.to_string())
}

/// Sample rate of the generated test.
const SYNTHETIC_RATE: u32 = 48_000;

/// A generated stand-in for GG's spatial test, made by `platinum::testsignal::spatial_round`
/// (nothing of GG's): a short sound stepping round the speakers of a 7.1 layout.
pub fn synthetic(sample: Sample) -> Clip {
    use platinum::testsignal::{SPATIAL_CHANNELS, SPATIAL_MASK, spatial_round};
    match sample {
        Sample::VirtualSurround => {
            let mut clip = Clip::new(SYNTHETIC_RATE, SPATIAL_CHANNELS as u16, SPATIAL_MASK);
            clip.extend(&spatial_round(SYNTHETIC_RATE, -18.0));
            clip
        }
    }
}

/// GG's sound when its file is there, else the generated stand-in.
pub fn load(sample: Sample) -> Result<Clip, String> {
    let file = sample_folder().join(sample.file_name());
    if file.is_file() { decode(&file) } else { Ok(synthetic(sample)) }
}

/// GG's words when a channel has no real output (`redirectionNotSet.tooltip`). These notes are
/// French, the catalogs' keys: the tab translates them as it shows them.
pub const NO_OUTPUT: &str = mark("Sélectionnez une sortie dans l'onglet Mixeur pour tester le son");
/// GG's microphone test errors (`micMonitoring.errors`), and Platinum's own when its daemon does
/// not relay the microphone (the recording would be silence).
pub const NO_MICROPHONE: &str = mark("Sélectionnez un micro dans l'onglet Mixeur pour enregistrer");
pub const NOT_RELAYED: &str = mark("Platinum ne relaie pas le micro en ce moment : l'enregistrement serait muet");
pub const NO_CHAT_OUTPUT: &str = mark("Sélectionnez une sortie pour le Chat dans l'onglet Mixeur pour utiliser cette fonction");
pub const NOTHING_RECORDED: &str = mark("Enregistrez d'abord un extrait");

/// Longest sound decoded: GG's are 10 to 30 s; anything far longer is not a test sound, and would
/// only fill memory.
const CLIP_LIMIT_SECONDS: u64 = 120;

/// Frames held by each block of a clip: a second at 48 kHz.
const BLOCK_FRAMES: usize = 48_000;

/// Audio in memory: interleaved 32-bit float, channels in Windows' speaker order. It is kept in
/// blocks of `BLOCK_FRAMES` frames (the last one filling up): a clip grows without ever being
/// copied, where one vector would double its room and take up to three times the clip's size while
/// it grows (the 8-channel sample alone is 17 MB).
///
/// Deliberately neither `Debug` nor `Clone`: a clip may hold the user's voice (the microphone
/// test), which must never reach a log, nor be copied around.
pub struct Clip {
    pub rate: u32,
    pub channels: u16,
    /// Windows speaker-position bits of the channels.
    pub channel_mask: u32,
    blocks: Vec<Vec<f32>>,
}

impl Clip {
    pub fn new(rate: u32, channels: u16, channel_mask: u32) -> Clip {
        Clip { rate, channels: channels.max(1), channel_mask, blocks: Vec::new() }
    }

    fn block_len(&self) -> usize {
        BLOCK_FRAMES * usize::from(self.channels)
    }

    /// Samples held (frames times channels).
    pub fn len(&self) -> usize {
        self.blocks.iter().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.first().is_none_or(Vec::is_empty)
    }

    pub fn frames(&self) -> usize {
        self.len() / usize::from(self.channels)
    }

    pub fn seconds(&self) -> f32 {
        self.frames() as f32 / self.rate.max(1) as f32
    }

    /// Appends interleaved samples (whole frames), or `count` samples of silence for `None`.
    fn append(&mut self, mut samples: Option<&[f32]>, mut count: usize) {
        let block = self.block_len();
        while count > 0 {
            if self.blocks.last().is_none_or(|last| last.len() >= block) {
                self.blocks.push(Vec::with_capacity(block));
            }
            let Some(last) = self.blocks.last_mut() else { return };
            let take = (block - last.len()).min(count);
            match samples {
                Some(rest) => {
                    last.extend_from_slice(&rest[..take]);
                    samples = Some(&rest[take..]);
                }
                None => last.resize(last.len() + take, 0.0),
            }
            count -= take;
        }
    }

    pub fn extend(&mut self, samples: &[f32]) {
        self.append(Some(samples), samples.len());
    }

    pub fn extend_silence(&mut self, count: usize) {
        self.append(None, count);
    }

    /// The samples from `start` on that follow each other in memory, at most `most` of them:
    /// what can be handed to a stream in one write. Empty past the end.
    pub fn run(&self, start: usize, most: usize) -> &[f32] {
        let block = self.block_len();
        match self.blocks.get(start / block) {
            Some(samples) if start % block < samples.len() => {
                let from = start % block;
                &samples[from..samples.len().min(from + most)]
            }
            _ => &[],
        }
    }

    /// Rearranges every frame: place `i` takes channel `order[i]` (see `reorder`).
    pub fn reorder(&mut self, order: &[usize]) {
        for block in &mut self.blocks {
            reorder(block, order);
        }
    }

    /// The loudest sample, as a level from 0 (the tests check decoded sounds with it).
    #[cfg(test)]
    pub fn peak(&self) -> f32 {
        self.blocks.iter().map(|block| platinum::dsp::peak(block)).fold(0.0, f32::max)
    }
}

/// Decodes an Ogg Vorbis file whole.
pub fn decode(path: &Path) -> Result<Clip, String> {
    let file = std::fs::File::open(path).map_err(|e| trf!("{file} illisible ({error})", file = path.display(), error = e))?;
    decode_from(std::io::BufReader::new(file)).map_err(|e| format!("{} : {e}", path.display()))
}

/// Decodes an Ogg Vorbis stream whole, in Windows' speaker order.
pub fn decode_from(reader: impl Read + Seek) -> Result<Clip, String> {
    use lewton::inside_ogg::OggStreamReader;
    use lewton::samples::InterleavedSamples;
    let mut stream = OggStreamReader::new(reader).map_err(|e| trf!("pas un fichier Ogg Vorbis ({error})", error = e))?;
    let channels = u16::from(stream.ident_hdr.audio_channels);
    let (channel_mask, order) = windows_layout(channels);
    let mut clip = Clip::new(stream.ident_hdr.audio_sample_rate, channels, channel_mask);
    let limit = CLIP_LIMIT_SECONDS as usize * clip.rate as usize * usize::from(channels);
    while clip.len() < limit {
        let packet = match stream.read_dec_packet_generic::<InterleavedSamples<f32>>() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(error) if is_empty_packet(&error) => continue,
            Err(error) => return Err(trf!("décodage interrompu ({error})", error = error)),
        };
        let take = packet.samples.len().min(limit - clip.len());
        clip.extend(&packet.samples[..take]);
    }
    clip.reorder(&order);
    Ok(clip)
}

/// An audio packet that ends before its header does. Three of GG's six files end on one (seen on
/// 17/11/2026: the last packet, 1 to 2 ms of audio). The Vorbis specification makes a packet cut
/// that short one to discard; the decoder stops on it before touching its state, so decoding goes
/// on after it.
fn is_empty_packet(error: &lewton::VorbisError) -> bool {
    matches!(error, lewton::VorbisError::BadAudio(lewton::audio::AudioReadError::EndOfPacket))
}

/// Windows speaker bits.
const FRONT_LEFT: u32 = 0x1;
const FRONT_RIGHT: u32 = 0x2;
const FRONT_CENTER: u32 = 0x4;
const LOW_FREQUENCY: u32 = 0x8;
const BACK_LEFT: u32 = 0x10;
const BACK_RIGHT: u32 = 0x20;
const BACK_CENTER: u32 = 0x100;
const SIDE_LEFT: u32 = 0x200;
const SIDE_RIGHT: u32 = 0x400;

/// The speaker of each channel of a Vorbis stream, in the stream's order (Vorbis I specification,
/// 4.3.9); `None` beyond eight channels, where the order is the application's.
fn vorbis_speakers(channels: u16) -> Option<&'static [u32]> {
    Some(match channels {
        1 => &[FRONT_CENTER],
        2 => &[FRONT_LEFT, FRONT_RIGHT],
        3 => &[FRONT_LEFT, FRONT_CENTER, FRONT_RIGHT],
        4 => &[FRONT_LEFT, FRONT_RIGHT, BACK_LEFT, BACK_RIGHT],
        5 => &[FRONT_LEFT, FRONT_CENTER, FRONT_RIGHT, BACK_LEFT, BACK_RIGHT],
        6 => &[FRONT_LEFT, FRONT_CENTER, FRONT_RIGHT, BACK_LEFT, BACK_RIGHT, LOW_FREQUENCY],
        7 => &[FRONT_LEFT, FRONT_CENTER, FRONT_RIGHT, SIDE_LEFT, SIDE_RIGHT, BACK_CENTER, LOW_FREQUENCY],
        8 => &[FRONT_LEFT, FRONT_CENTER, FRONT_RIGHT, SIDE_LEFT, SIDE_RIGHT, BACK_LEFT, BACK_RIGHT, LOW_FREQUENCY],
        _ => return None,
    })
}

/// How a Vorbis stream's channels map onto Windows' layout: the speaker mask, and for each place
/// of Windows' order (speakers by increasing bit) the Vorbis channel that goes there. Vorbis puts
/// the centre second and the bass last; Windows puts them third and fourth. Beyond eight channels
/// the order is kept, on the first speakers.
pub fn windows_layout(channels: u16) -> (u32, Vec<usize>) {
    match vorbis_speakers(channels) {
        Some(speakers) => {
            let mut order: Vec<usize> = (0..speakers.len()).collect();
            order.sort_by_key(|&channel| speakers[channel]);
            (speakers.iter().fold(0, |mask, bit| mask | bit), order)
        }
        None => ((1u32 << channels.min(18)) - 1, (0..usize::from(channels)).collect()),
    }
}

/// Rearranges interleaved frames: place `i` of each frame takes channel `order[i]`.
pub fn reorder(samples: &mut [f32], order: &[usize]) {
    if order.iter().enumerate().all(|(place, &channel)| place == channel) {
        return;
    }
    let mut frame = vec![0.0f32; order.len()];
    for chunk in samples.chunks_exact_mut(order.len()) {
        frame.copy_from_slice(chunk);
        for (place, &channel) in order.iter().enumerate() {
            chunk[place] = frame[channel];
        }
    }
}

/// Longest recording of the microphone test.
pub const RECORD_LIMIT_SECONDS: u32 = 30;

/// The microphone test's recording: what applications record from the virtual microphone, as
/// they get it, up to `RECORD_LIMIT_SECONDS`. It grows a block at a time (see `Clip`): a short
/// recording takes only what it holds. Neither `Debug` nor `Clone`, like `Clip`.
pub struct Recording {
    pub clip: Clip,
    /// Samples it may hold.
    capacity: usize,
}

impl Recording {
    pub fn new(rate: u32, channels: u16, channel_mask: u32) -> Recording {
        let clip = Clip::new(rate, channels, channel_mask);
        let capacity = RECORD_LIMIT_SECONDS as usize * rate as usize * usize::from(clip.channels);
        Recording { clip, capacity }
    }

    /// Appends a captured packet (`None`: `frames` frames of silence), as much of it as fits.
    /// Returns true once the recording is full.
    pub fn push(&mut self, samples: Option<&[f32]>, frames: usize) -> bool {
        let room = self.capacity.saturating_sub(self.clip.len());
        let wanted = (frames * usize::from(self.clip.channels)).min(room);
        match samples {
            Some(samples) => self.clip.extend(&samples[..wanted.min(samples.len())]),
            None => self.clip.extend_silence(wanted),
        }
        self.is_full()
    }

    pub fn is_full(&self) -> bool {
        self.clip.len() >= self.capacity
    }

    pub fn is_empty(&self) -> bool {
        self.clip.is_empty()
    }
}

/// Where the microphone test is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MicPhase {
    /// Nothing recorded.
    #[default]
    Idle,
    Recording,
    /// A recording is kept, ready to be heard.
    Recorded,
    /// The recording is being heard.
    Playing,
}

/// What happens to the microphone test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicEvent {
    /// "Enregistrer": a new recording starts, the previous one is dropped.
    Record,
    /// "Arrêter", the time limit, another sound started or a device lost: `captured` says whether
    /// anything came in.
    RecordingEnded { captured: bool },
    /// "Écouter".
    Listen,
    /// "Pause", the end of the recording, another sound started or a device lost.
    ListeningEnded,
}

impl MicPhase {
    /// The phase after `event`, or why it cannot happen (in French). An end that comes when
    /// nothing was under way changes nothing.
    pub fn next(self, event: MicEvent) -> Result<MicPhase, &'static str> {
        Ok(match (self, event) {
            (_, MicEvent::Record) => MicPhase::Recording,
            (MicPhase::Recording, MicEvent::RecordingEnded { captured }) => {
                if captured {
                    MicPhase::Recorded
                } else {
                    MicPhase::Idle
                }
            }
            // Nothing to hear yet: the recording under way is not offered before it ends (the
            // tester ends it first when asked to listen).
            (MicPhase::Idle | MicPhase::Recording, MicEvent::Listen) => return Err(NOTHING_RECORDED),
            // Heard again from the start.
            (MicPhase::Recorded | MicPhase::Playing, MicEvent::Listen) => MicPhase::Playing,
            (MicPhase::Playing, MicEvent::ListeningEnded) => MicPhase::Recorded,
            (phase, _) => phase,
        })
    }

    /// The number the window's description knows the phase by.
    pub fn code(self) -> i32 {
        match self {
            MicPhase::Idle => 0,
            MicPhase::Recording => 1,
            MicPhase::Recorded => 2,
            MicPhase::Playing => 3,
        }
    }
}

/// Why "Enregistrer" cannot work, from what the window knows: a microphone must be chosen, and the
/// daemon must relay it (otherwise the virtual microphone carries silence).
pub fn record_problem(microphone_chosen: bool, relayed: bool) -> Option<&'static str> {
    if relayed {
        None
    } else if !microphone_chosen {
        Some(NO_MICROPHONE)
    } else {
        Some(NOT_RELAYED)
    }
}

/// Why "Écouter" cannot work: something must be recorded, and the Chat channel must have a real
/// output to hear it on.
pub fn listen_problem(has_recording: bool, chat_output: Option<&str>) -> Option<&'static str> {
    if !has_recording {
        Some(NOTHING_RECORDED)
    } else if chat_output.is_none_or(str::is_empty) {
        Some(NO_CHAT_OUTPUT)
    } else {
        None
    }
}

/// "0:07": a duration as the microphone test shows it.
pub fn clock(seconds: f32) -> String {
    let whole = seconds.max(0.0).floor() as u32;
    format!("{}:{:02}", whole / 60, whole % 60)
}

/// The microphone test's time line: where the recording or the listening is, or how long the
/// recording kept is; empty with nothing recorded.
pub fn mic_time(phase: MicPhase, position: f32, recorded: f32) -> String {
    match phase {
        MicPhase::Idle => String::new(),
        MicPhase::Recording => format!("{} / {}", clock(position), clock(RECORD_LIMIT_SECONDS as f32)),
        MicPhase::Recorded => trf!("Extrait de {length}", length = clock(recorded)),
        MicPhase::Playing => format!("{} / {}", clock(position), clock(recorded)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spatial_test_is_offered_where_spatial_audio_exists() {
        for channel in ["game", "media", "aux"] {
            assert_eq!(samples_for(channel), [Sample::VirtualSurround], "{channel}");
        }
        for channel in ["chat", "mic"] {
            assert!(samples_for(channel).is_empty(), "{channel}");
        }
        assert!(samples_for("mic").is_empty());
        assert_eq!(Sample::VirtualSurround.label(), "Audio spatial");
        assert_eq!(Sample::VirtualSurround.file_name(), "VirtualSurround.ogg");
    }

    #[test]
    fn the_sounds_are_looked_for_where_gg_installs_them() {
        let folder = sample_folder();
        assert!(folder.ends_with(r"SteelSeries\GG\apps\sonar\audio_samples"), "{}", folder.display());
    }

    #[test]
    fn a_missing_sound_is_replaced_and_said_so() {
        let empty = std::env::temp_dir().join(format!("platinum-no-samples-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&empty);
        let gone = missing(&empty, samples_for("game"));
        assert_eq!(gone, [Sample::VirtualSurround]);
        assert_eq!(missing_note(&gone).as_deref(), Some(SYNTHETIC_NOTE));
        std::fs::write(empty.join("VirtualSurround.ogg"), b"x").unwrap();
        assert!(missing(&empty, samples_for("game")).is_empty());
        let _ = std::fs::remove_dir_all(&empty);
        assert_eq!(missing_note(&[]), None);
        // Only a channel without output dims the button: nothing could be heard.
        assert_eq!(sample_problem(false).as_deref(), Some(NO_OUTPUT));
        assert_eq!(sample_problem(true), None);
    }

    #[test]
    fn the_generated_spatial_test_steps_round_the_speakers() {
        let clip = synthetic(Sample::VirtualSurround);
        assert_eq!((clip.rate, clip.channels, clip.channel_mask), (SYNTHETIC_RATE, 8, 0x63f));
        assert!(clip.seconds() >= 5.0 && clip.seconds() <= 12.0, "{} s", clip.seconds());
        let peak = clip.peak();
        assert!(peak > 0.05 && peak <= 1.0, "peak {peak}");
        // The bass channel (4th in Windows' order) stays silent; the front left sounds first, alone.
        let first = clip.run(0, 8 * 24_000);
        assert!(first.chunks_exact(8).all(|frame| frame[1..].iter().all(|s| *s == 0.0)));
        assert!(first.chunks_exact(8).any(|frame| frame[0] != 0.0));
        let mut bass = 0.0f32;
        let mut at = 0;
        while at < clip.len() {
            let run = clip.run(at, clip.len());
            bass = run.chunks_exact(8).map(|frame| frame[3].abs()).fold(bass, f32::max);
            at += run.len().max(1);
        }
        assert_eq!(bass, 0.0);
    }

    #[test]
    fn vorbis_channels_are_put_in_windows_order() {
        assert_eq!(windows_layout(1), (0x4, vec![0]));
        assert_eq!(windows_layout(2), (0x3, vec![0, 1]));
        // 5.1: Vorbis L C R BL BR LFE, Windows L R C LFE BL BR.
        assert_eq!(windows_layout(6), (0x3F, vec![0, 2, 1, 5, 3, 4]));
        // 7.1: Vorbis L C R SL SR BL BR LFE, Windows L R C LFE BL BR SL SR.
        assert_eq!(windows_layout(8), (0x63F, vec![0, 2, 1, 7, 5, 6, 3, 4]));
        assert_eq!(windows_layout(7).0, 0x70F);
        assert_eq!(windows_layout(10), (0x3FF, (0..10).collect()));
        let mut frames = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        reorder(&mut frames, &windows_layout(8).1);
        assert_eq!(frames[..8], [1.0, 3.0, 2.0, 8.0, 6.0, 7.0, 4.0, 5.0]);
        assert_eq!(frames[8..], frames[..8]);
        let mut stereo = vec![0.1, 0.2, 0.3, 0.4];
        reorder(&mut stereo, &windows_layout(2).1);
        assert_eq!(stereo, [0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn a_clip_grows_by_blocks_and_hands_them_out_in_runs() {
        let mut clip = Clip::new(48_000, 2, 0x3);
        assert!(clip.is_empty() && clip.run(0, 10).is_empty());
        let block = 2 * BLOCK_FRAMES;
        let first: Vec<f32> = (0..block - 4).map(|i| i as f32).collect();
        clip.extend(&first);
        // Crosses into a second block without moving the first.
        clip.extend(&[-1.0, -2.0, -3.0, -4.0, -5.0, -6.0]);
        clip.extend_silence(4);
        assert_eq!(clip.len(), block + 6);
        assert_eq!((clip.frames(), clip.blocks.len()), (BLOCK_FRAMES + 3, 2));
        assert_eq!(clip.blocks[0].capacity(), block);
        // A run never crosses a block: the writer asks again for the rest.
        assert_eq!(clip.run(block - 6, 100), [(block - 6) as f32, (block - 5) as f32, -1.0, -2.0, -3.0, -4.0]);
        assert_eq!(clip.run(block, 100), [-5.0, -6.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(clip.run(block + 2, 1), [0.0]);
        assert!(clip.run(block + 6, 100).is_empty() && clip.run(10 * block, 1).is_empty());
        assert_eq!(clip.peak(), (block - 5) as f32);
        // Reordering works block by block, on whole frames.
        let mut surround = Clip::new(48_000, 8, 0x63F);
        surround.extend(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
        surround.reorder(&windows_layout(8).1);
        assert_eq!(surround.run(0, 8), [1.0, 3.0, 2.0, 8.0, 6.0, 7.0, 4.0, 5.0]);
        assert!((Clip::new(48_000, 1, 0x4).seconds() - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn a_file_that_is_not_vorbis_is_refused() {
        // `.err()`: a clip has no `Debug` (see `Clip`).
        let error = decode_from(std::io::Cursor::new(b"RIFF....WAVEfmt ".to_vec())).err().unwrap_or_default();
        assert!(error.contains("Ogg Vorbis"), "{error}");
        assert!(decode(Path::new(r"C:\nowhere\Gaming.ogg")).err().unwrap_or_default().contains("illisible"));
    }

    /// Decodes the sounds GG installed on this machine (read only); skipped where GG is absent.
    #[test]
    fn the_installed_sounds_decode() {
        let folder = sample_folder();
        if !missing(&folder, &[Sample::VirtualSurround]).is_empty() {
            eprintln!("GG's test sounds are not installed here: skipped");
            return;
        }
        for sample in [Sample::VirtualSurround] {
            let clip = decode(&folder.join(sample.file_name())).unwrap();
            eprintln!("{}: {} Hz, {} channel(s), mask {:#x}, {:.1} s", sample.file_name(), clip.rate, clip.channels, clip.channel_mask, clip.seconds());
            assert!((8_000..=192_000).contains(&clip.rate), "{}", clip.rate);
            assert!((1..=8).contains(&clip.channels));
            assert_eq!(clip.len() % usize::from(clip.channels), 0);
            assert!(clip.seconds() > 1.0 && clip.seconds() <= CLIP_LIMIT_SECONDS as f32, "{}", clip.seconds());
            let loudest = clip.peak();
            assert!(loudest > 0.01 && loudest <= 1.5, "{} peaks at {loudest}", sample.file_name());
        }
    }

    #[test]
    fn a_recording_stops_at_thirty_seconds() {
        let mut recording = Recording::new(1000, 2, 0x3);
        assert!(recording.is_empty());
        let packet = vec![0.5f32; 2 * 100];
        let mut pushes = 0;
        while !recording.push(Some(&packet), 100) {
            pushes += 1;
            assert!(pushes < 1000);
        }
        assert_eq!(pushes + 1, 300);
        assert_eq!(recording.clip.frames(), 30 * 1000);
        assert_eq!(recording.clip.seconds(), 30.0);
        // Full: nothing more goes in, silence neither.
        assert!(recording.push(Some(&packet), 100));
        assert!(recording.push(None, 100));
        assert_eq!(recording.clip.frames(), 30 * 1000);
        // A packet that overflows is cut at the limit; silence counts as audio.
        let mut short = Recording::new(10, 1, 0x4);
        short.push(None, 295);
        assert!(!short.is_empty() && !short.is_full());
        assert!(short.push(Some(&[0.25; 10]), 10));
        assert_eq!(short.clip.len(), 300);
        assert_eq!(short.clip.run(294, 100), [0.0, 0.25, 0.25, 0.25, 0.25, 0.25]);
    }

    #[test]
    fn the_microphone_test_goes_from_recording_to_listening() {
        use MicEvent::*;
        let phase = MicPhase::Idle;
        assert_eq!(phase.next(Listen), Err(NOTHING_RECORDED));
        let phase = phase.next(Record).unwrap();
        assert_eq!(phase, MicPhase::Recording);
        assert_eq!(phase.next(Listen), Err(NOTHING_RECORDED));
        let recorded = phase.next(RecordingEnded { captured: true }).unwrap();
        assert_eq!(recorded, MicPhase::Recorded);
        let playing = recorded.next(Listen).unwrap();
        assert_eq!(playing, MicPhase::Playing);
        // Listening again starts over; a pause or the end keeps the recording.
        assert_eq!(playing.next(Listen), Ok(MicPhase::Playing));
        assert_eq!(playing.next(ListeningEnded), Ok(MicPhase::Recorded));
        // A new recording replaces the old one from any phase.
        assert_eq!(playing.next(Record), Ok(MicPhase::Recording));
        assert_eq!(recorded.next(Record), Ok(MicPhase::Recording));
        // Nothing captured: nothing to listen to.
        assert_eq!(MicPhase::Recording.next(RecordingEnded { captured: false }), Ok(MicPhase::Idle));
        // A stale end changes nothing.
        assert_eq!(MicPhase::Recorded.next(RecordingEnded { captured: false }), Ok(MicPhase::Recorded));
        assert_eq!(MicPhase::Idle.next(ListeningEnded), Ok(MicPhase::Idle));
        let codes: Vec<i32> = [MicPhase::Idle, MicPhase::Recording, MicPhase::Recorded, MicPhase::Playing].iter().map(|p| p.code()).collect();
        assert_eq!(codes, [0, 1, 2, 3]);
    }

    #[test]
    fn the_microphone_test_says_what_is_missing_in_ggs_words() {
        assert_eq!(record_problem(false, false), Some(NO_MICROPHONE));
        assert_eq!(record_problem(true, false), Some(NOT_RELAYED));
        assert_eq!(record_problem(true, true), None);
        assert_eq!(listen_problem(false, Some("{chat}")), Some(NOTHING_RECORDED));
        assert_eq!(listen_problem(true, None), Some(NO_CHAT_OUTPUT));
        assert_eq!(listen_problem(true, Some("")), Some(NO_CHAT_OUTPUT));
        assert_eq!(listen_problem(true, Some("{chat}")), None);
        assert_eq!(NOTHING_RECORDED, "Enregistrez d'abord un extrait");
        assert!(NO_MICROPHONE.starts_with("Sélectionnez un micro") && NO_CHAT_OUTPUT.starts_with("Sélectionnez une sortie pour le Chat"));
    }

    #[test]
    fn times_are_shown_in_minutes_and_seconds() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(7.9), "0:07");
        assert_eq!(clock(75.0), "1:15");
        assert_eq!(clock(-1.0), "0:00");
        assert_eq!(mic_time(MicPhase::Idle, 3.0, 0.0), "");
        assert_eq!(mic_time(MicPhase::Recording, 4.2, 0.0), "0:04 / 0:30");
        assert_eq!(mic_time(MicPhase::Recorded, 0.0, 12.5), "Extrait de 0:12");
        assert_eq!(mic_time(MicPhase::Playing, 2.0, 12.5), "0:02 / 0:12");
    }
}
