//! Takes a few files out of an NSIS installer (Nullsoft Scriptable Install System) without running
//! it, so the setup can take the Sonar driver out of SteelSeries GG's official installer without
//! shipping 7-Zip.
//!
//! Only what GG's installer uses is supported: NSIS 3, Unicode strings, one solid LZMA stream
//! without the x86 filter. Anything else is refused with a sentence saying so, never guessed.
//!
//! Layout (NSIS 3, `Source/exehead/fileform.h`), as GG's installer has it:
//! - the executable stub, then, at a 512-byte boundary, the 28-byte *first header*: flags,
//!   `0xDEADBEEF`, `"NullsoftInst"`, the size of the header once unpacked, and the size of
//!   everything from the first header on;
//! - right after it, one LZMA stream (5 bytes of properties, then the coded data, no size). Once
//!   unpacked it is a chain of blocks, each a little-endian `u32` size then that many bytes: first
//!   the header, then the files' data;
//! - the header: flags, then 8 blocks `(offset, count)` relative to the header (pages, sections,
//!   entries, strings, …); entries are 7 `i32` (opcode then 6 parameters); strings are UTF-16LE,
//!   with codes 1 to 4 followed by one parameter unit (language string, shell folder, variable,
//!   literal);
//! - a file is an entry `EW_EXTRACTFILE` (20): parameter 1 is its name, parameter 2 the offset of
//!   its block in the data, counted from the end of the header's block. A relative name is
//!   relative to the output folder of the last `SetOutPath` (`EW_CREATEDIR` (11) with parameter 1
//!   set) before it in the script, which is also how 7-Zip names files. Paths start with
//!   `$INSTDIR\`, removed here, so names read like the install layout
//!   (`apps\sonar\driver\vad\SteelSeries-Sonar-VAD.inf`).
//!
//! The stream is solid: reaching a file means unpacking everything before it, so the reader
//! unpacks as it goes, keeps only the wanted files and stops after the last one.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const SIGNATURE: u32 = 0xDEAD_BEEF;
const MAGIC: &[u8; 12] = b"NullsoftInst";
const FIRST_HEADER: usize = 28;
/// `FH_FLAGS_NO_CRC`: no CRC32 after the data.
const NO_CRC: u32 = 4;
const BLOCKS: usize = 8;
const ENTRIES: usize = 2;
const STRINGS: usize = 3;
const ENTRY_SIZE: usize = 28;
const EW_CREATEDIR: i32 = 11;
const EW_EXTRACTFILE: i32 = 20;
/// Names of NSIS's built-in variables after `$0`…`$9` and `$R0`…`$R9`, in index order (as 7-Zip
/// names them, so names compare with its listing).
const BUILT_IN: [&str; 12] = ["CMDLINE", "INSTDIR", "OUTDIR", "EXEDIR", "LANGUAGE", "TEMP", "PLUGINSDIR", "EXEPATH", "EXEFILE", "HWNDPARENT", "_CLICK", "_OUTDIR"];
const INSTDIR: &str = "$INSTDIR";

/// The 28-byte first header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FirstHeader {
    pub flags: u32,
    /// Size of the header once unpacked.
    pub header_size: u32,
    /// Size of everything from the first header on, CRC included.
    pub archive_size: u32,
}

/// A file of the archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// Path as installed, `\`-separated, without `$INSTDIR\` (`$_12_\…` for a folder held in a
    /// script variable, `$PLUGINSDIR\…`…).
    pub path: String,
    /// Offset of the file's block in the data that follows the header.
    pub offset: u32,
}

/// A file written by `extract`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extracted {
    pub path: String,
    pub size: u64,
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    bytes.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// The first header, if these 28 bytes are one.
pub fn parse_first_header(bytes: &[u8]) -> Option<FirstHeader> {
    if u32_at(bytes, 4)? != SIGNATURE || bytes.get(8..20)? != MAGIC {
        return None;
    }
    Some(FirstHeader { flags: u32_at(bytes, 0)?, header_size: u32_at(bytes, 20)?, archive_size: u32_at(bytes, 24)? })
}

/// Where the first header is (searched at 512-byte boundaries, as the NSIS stub does) and what
/// it says; `None` when the file is no NSIS installer.
pub fn find_first_header(file: &mut (impl Read + Seek)) -> io::Result<Option<(u64, FirstHeader)>> {
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::with_capacity(1 << 16, file);
    let mut block = [0u8; 512];
    let mut at = 0u64;
    loop {
        let read = read_up_to(&mut reader, &mut block)?;
        if read < FIRST_HEADER {
            return Ok(None);
        }
        if let Some(header) = parse_first_header(&block[..FIRST_HEADER]) {
            return Ok(Some((at, header)));
        }
        if read < block.len() {
            return Ok(None);
        }
        at += block.len() as u64;
    }
}

fn read_up_to(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Whether these bytes start LZMA properties as NSIS writes them (lc/lp/pb byte, dictionary
/// size), followed by the range coder's first byte, which is always 0.
fn is_lzma(p: &[u8]) -> bool {
    p.len() >= 6 && p[0] < 9 * 5 * 5 && u32_at(p, 1).is_some_and(|dict| (1 << 12..=1 << 30).contains(&dict)) && p[5] == 0
}

/// How many bytes precede the LZMA properties after the first header: 0, or 1 for the flag byte
/// of the x86 filter when it is off. Refuses what this reader cannot unpack.
pub fn lzma_start(after_first_header: &[u8], header_size: u32) -> Result<usize, String> {
    let p = after_first_header;
    if u32_at(p, 0) == Some(header_size) {
        return Err("the installer's header is not compressed (only solid LZMA installers are supported)".into());
    }
    if is_lzma(p) {
        return Ok(0);
    }
    if p.first().is_some_and(|f| *f <= 1) && is_lzma(&p[1..]) {
        return if p[0] == 0 { Ok(1) } else { Err("the installer uses LZMA with the x86 filter, which this reader does not support".into()) };
    }
    if p.get(3) == Some(&0x80) {
        return Err("the installer is compressed file by file (non-solid), which this reader does not support".into());
    }
    Err("the installer is not compressed with LZMA (zlib or bzip2 are not supported)".into())
}

/// The files the header lists, in script order. Errors say what is wrong with the header.
pub fn items(header: &[u8]) -> Result<Vec<Item>, String> {
    let block = |index: usize| -> Result<(usize, usize), String> {
        let offset = u32_at(header, 4 + 8 * index).ok_or("header too short")? as usize;
        let count = u32_at(header, 8 + 8 * index).ok_or("header too short")? as usize;
        Ok((offset, count))
    };
    if header.len() < 4 + 8 * BLOCKS {
        return Err("header too short".into());
    }
    let (entries_at, count) = block(ENTRIES)?;
    let (strings_at, _) = block(STRINGS)?;
    let end = count.checked_mul(ENTRY_SIZE).and_then(|size| size.checked_add(entries_at)).ok_or("entry table too large")?;
    if end > header.len() || strings_at >= header.len() {
        return Err("entry or string table outside the header".into());
    }
    let strings = &header[strings_at..];
    // NSIS 3 Unicode: the string table starts with the empty string, two zero bytes.
    if strings.len() < 2 || strings[0] != 0 || strings[1] != 0 {
        return Err("the installer's strings are not Unicode (only NSIS 3 Unicode installers are supported)".into());
    }
    let mut output_folder = String::new();
    let mut found = Vec::new();
    for index in 0..count {
        let at = entries_at + index * ENTRY_SIZE;
        let parameter = |n: usize| u32_at(header, at + 4 + 4 * n).map(|v| v as i32).unwrap_or(0);
        let opcode = u32_at(header, at).map(|v| v as i32).unwrap_or(0);
        match opcode {
            EW_CREATEDIR if parameter(1) != 0 => output_folder = string(strings, parameter(0))?,
            EW_EXTRACTFILE => {
                let name = string(strings, parameter(1))?;
                let offset = u32::try_from(parameter(2)).map_err(|_| format!("negative data offset for {name}"))?;
                found.push(Item { path: install_path(&output_folder, &name), offset });
            }
            _ => {}
        }
    }
    Ok(found)
}

/// A string of the table at `offset` (in UTF-16 units), with its codes written out as NSIS
/// script text: `$INSTDIR`, `$_12_` (a script variable), `$(LSTR_3)`, `$SHELL[…]`.
pub fn string(strings: &[u8], offset: i32) -> Result<String, String> {
    if offset < 0 {
        return Ok(format!("$(LSTR_{})", -(offset as i64) - 1));
    }
    let mut units = Vec::new();
    let mut text = String::new();
    let mut at = offset as usize * 2;
    let unit = |at: usize| strings.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    loop {
        let c = unit(at).ok_or_else(|| format!("string {offset} runs past the table"))?;
        at += 2;
        match c {
            0 => break,
            1..=4 => {
                let value = unit(at).ok_or_else(|| format!("string {offset} runs past the table"))?;
                at += 2;
                if c == 4 {
                    units.push(value);
                    continue;
                }
                text.push_str(&String::from_utf16_lossy(&units));
                units.clear();
                match c {
                    3 => text.push_str(&variable(decode_short(value))),
                    2 => text.push_str(&format!("$SHELL[{:#06x}]", value)),
                    _ => text.push_str(&format!("$(LSTR_{})", decode_short(value))),
                }
            }
            _ => units.push(c),
        }
    }
    text.push_str(&String::from_utf16_lossy(&units));
    Ok(text)
}

/// NSIS stores variable and language-string numbers with the high bit of each byte set
/// (`CODE_SHORT`), so no byte of the code is zero: 7 bits from the low byte, 7 from the high.
pub fn decode_short(value: u16) -> u32 {
    (value as u32 & 0x7F) | (((value as u32 >> 8) & 0x7F) << 7)
}

/// A variable's script name, 7-Zip's way: `$0`, `$R3`, `$INSTDIR`, `$_12_` for script variables.
pub fn variable(index: u32) -> String {
    match index {
        0..=9 => format!("${index}"),
        10..=19 => format!("$R{}", index - 10),
        _ => match BUILT_IN.get((index - 20) as usize) {
            Some(name) => format!("${name}"),
            None => format!("$_{}_", index - 20 - BUILT_IN.len() as u32),
        },
    }
}

/// Whether a file name is already a full path (a drive, a UNC path, or a variable holding one),
/// rather than a name relative to the output folder.
fn is_absolute(name: &str) -> bool {
    let bytes = name.as_bytes();
    name.starts_with('$') || name.starts_with("\\\\") || (bytes.len() >= 2 && bytes[1] == b':')
}

/// The path a file is installed at, from the output folder and its name, without `$INSTDIR\`.
pub fn install_path(output_folder: &str, name: &str) -> String {
    let full = if is_absolute(name) || output_folder.is_empty() {
        name.to_string()
    } else {
        format!("{}\\{}", output_folder.trim_end_matches('\\'), name)
    };
    let full = full.replace('/', "\\");
    let full = match full.strip_prefix(INSTDIR) {
        Some(rest) if rest.is_empty() || rest.starts_with('\\') => rest.trim_start_matches('\\').to_string(),
        _ => full,
    };
    // Collapse "a\\b" and ".\" segments, which NSIS paths may carry.
    full.split('\\').filter(|part| !part.is_empty() && *part != ".").collect::<Vec<_>>().join("\\")
}

/// The data blocks to keep, by offset, with the paths each one is written to. A path the script
/// extracts more than once keeps its last block, as the install would leave it; one block shared by
/// several paths (NSIS stores identical files once) is written to each.
pub fn blocks_to_keep(listed: Vec<Item>, wanted: &dyn Fn(&str) -> bool) -> BTreeMap<u32, Vec<String>> {
    let mut by_path: BTreeMap<String, u32> = BTreeMap::new();
    for item in listed.into_iter().filter(|item| wanted(&item.path)) {
        by_path.insert(item.path, item.offset);
    }
    let mut blocks: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    for (path, offset) in by_path {
        blocks.entry(offset).or_default().push(path);
    }
    blocks
}

/// What `extract` needs to know about where it is in the unpacked stream.
enum State {
    /// Reading the 4-byte size of the header's block.
    HeaderSize,
    /// Reading the header itself.
    Header(usize),
    /// Skipping data up to the next wanted block.
    Skip,
    /// Reading the 4-byte size of a wanted block.
    BlockSize,
    /// Copying a wanted block into its files.
    Block(u64),
    /// Every wanted file is written.
    Done,
}

/// The unpacked stream, consumed as it comes: the header first, then the wanted blocks.
struct Sink<'a> {
    state: State,
    buffer: Vec<u8>,
    /// Position in the data that follows the header.
    position: u64,
    wanted: &'a dyn Fn(&str) -> bool,
    out: PathBuf,
    /// Wanted blocks still to come, by offset, with the paths that share them.
    pending: BTreeMap<u32, Vec<String>>,
    /// Files of the block being copied.
    open: Vec<(File, String)>,
    written: Vec<Extracted>,
    error: Option<String>,
}

impl Sink<'_> {
    fn fail(&mut self, message: String) -> io::Error {
        self.error = Some(message.clone());
        io::Error::other(message)
    }

    fn take(&mut self, data: &[u8]) -> io::Result<usize> {
        let mut used = 0;
        while used < data.len() {
            let rest = &data[used..];
            match self.state {
                State::HeaderSize | State::BlockSize => {
                    let need = 4 - self.buffer.len();
                    let n = need.min(rest.len());
                    self.buffer.extend_from_slice(&rest[..n]);
                    used += n;
                    if self.buffer.len() == 4 {
                        let size = u32::from_le_bytes([self.buffer[0], self.buffer[1], self.buffer[2], self.buffer[3]]);
                        self.buffer.clear();
                        if matches!(self.state, State::HeaderSize) {
                            if size & 0x8000_0000 != 0 || size > 64 << 20 {
                                return Err(self.fail(format!("implausible header size {size}")));
                            }
                            self.state = State::Header(size as usize);
                        } else {
                            self.position += 4;
                            if size & 0x8000_0000 != 0 {
                                return Err(self.fail("a file block is compressed on its own inside the solid stream".into()));
                            }
                            self.start_block(size as u64)?;
                        }
                    }
                }
                State::Header(size) => {
                    let n = (size - self.buffer.len()).min(rest.len());
                    self.buffer.extend_from_slice(&rest[..n]);
                    used += n;
                    if self.buffer.len() == size {
                        let header = std::mem::take(&mut self.buffer);
                        self.plan(&header)?;
                    }
                }
                State::Skip => {
                    let next = self.pending.keys().next().copied().map(u64::from).unwrap_or(u64::MAX);
                    let n = (next - self.position).min(rest.len() as u64) as usize;
                    used += n;
                    self.position += n as u64;
                    if self.position == next {
                        self.state = State::BlockSize;
                    }
                }
                State::Block(left) => {
                    let n = left.min(rest.len() as u64) as usize;
                    for (file, path) in &mut self.open {
                        if let Err(e) = file.write_all(&rest[..n]) {
                            let message = format!("cannot write {path}: {e}");
                            self.error = Some(message.clone());
                            return Err(io::Error::other(message));
                        }
                    }
                    used += n;
                    self.position += n as u64;
                    let left = left - n as u64;
                    self.state = State::Block(left);
                    if left == 0 {
                        self.open.clear();
                        self.next_block();
                    }
                }
                State::Done => return Err(io::Error::other("done")),
            }
        }
        Ok(used)
    }

    /// Reads the header and lists the blocks to keep.
    fn plan(&mut self, header: &[u8]) -> io::Result<()> {
        let listed = match items(header) {
            Ok(listed) => listed,
            Err(e) => return Err(self.fail(e)),
        };
        for (offset, paths) in blocks_to_keep(listed, self.wanted) {
            self.pending.insert(offset, paths);
        }
        self.next_block();
        Ok(())
    }

    fn next_block(&mut self) {
        self.state = if self.pending.is_empty() { State::Done } else { State::Skip };
    }

    /// Opens the files of the block that starts here.
    fn start_block(&mut self, size: u64) -> io::Result<()> {
        let offset = (self.position - 4) as u32;
        let paths = self.pending.remove(&offset).unwrap_or_default();
        for path in paths {
            let target = self.out.join(path.replace('\\', "/"));
            let created = target.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| File::create(&target));
            match created {
                Ok(file) => {
                    self.written.push(Extracted { path: path.clone(), size });
                    self.open.push((file, path));
                }
                Err(e) => return Err(self.fail(format!("cannot create {}: {e}", target.display()))),
            }
        }
        self.state = State::Block(size);
        if size == 0 {
            self.open.clear();
            self.next_block();
        }
        Ok(())
    }
}

impl Write for Sink<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let used = self.take(data)?;
        if matches!(self.state, State::Done) {
            // Stops the decoder: everything wanted is written.
            return Err(io::Error::other("done"));
        }
        Ok(used)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Writes the files of `installer` whose path `wanted` accepts under `out` (keeping their
/// install layout), and returns them in the order written. `progress(read, total)` is called as
/// the compressed stream is read, in bytes (the decoder hands its output over in 64 MB steps, so
/// the input is the smooth measure); reading stops early after the last wanted file. It is an
/// error when no file matches, or when the stream ends before every wanted file came.
pub fn extract(installer: &Path, wanted: &dyn Fn(&str) -> bool, out: &Path, progress: &mut dyn FnMut(u64, u64)) -> Result<Vec<Extracted>, String> {
    let mut file = File::open(installer).map_err(|e| format!("cannot open {}: {e}", installer.display()))?;
    let (start, first) = find_first_header(&mut file)
        .map_err(|e| format!("cannot read {}: {e}", installer.display()))?
        .ok_or_else(|| format!("{} is not an NSIS installer", installer.display()))?;
    let length = file.metadata().map_err(|e| e.to_string())?.len();
    let data_start = start + FIRST_HEADER as u64;
    let crc = if first.flags & NO_CRC == 0 { 4 } else { 0 };
    let data_end = (start + first.archive_size as u64).saturating_sub(crc);
    if data_end > length || data_end <= data_start {
        return Err(format!("{} is truncated (the installer says {} bytes, the file has {length})", installer.display(), start + first.archive_size as u64));
    }
    file.seek(SeekFrom::Start(data_start)).map_err(|e| e.to_string())?;
    let mut probe = [0u8; 16];
    file.read_exact(&mut probe).map_err(|e| e.to_string())?;
    let skip = lzma_start(&probe, first.header_size)?;
    file.seek(SeekFrom::Start(data_start + skip as u64)).map_err(|e| e.to_string())?;
    let total = data_end - data_start - skip as u64;
    let mut input = BufReader::with_capacity(1 << 20, Counting { inner: file.take(total), read: 0, total, progress });
    let mut sink = Sink {
        state: State::HeaderSize,
        buffer: Vec::new(),
        position: 0,
        wanted,
        out: out.to_path_buf(),
        pending: BTreeMap::new(),
        open: Vec::new(),
        written: Vec::new(),
        error: None,
    };
    let options = lzma_rs::decompress::Options { unpacked_size: lzma_rs::decompress::UnpackedSize::UseProvided(None), memlimit: None, allow_incomplete: true };
    let outcome = lzma_rs::lzma_decompress_with_options(&mut input, &mut sink, &options);
    drop(sink.open.drain(..));
    if let Some(error) = sink.error.take() {
        return Err(error);
    }
    match (&sink.state, outcome) {
        (State::Done, _) if !sink.written.is_empty() => Ok(sink.written),
        (State::Done, _) => Err("no file of the installer matches".into()),
        (_, Err(e)) => Err(format!("the installer's data cannot be unpacked: {e:?}")),
        (State::HeaderSize | State::Header(_), Ok(())) => Err("the installer's data ends before its header".into()),
        (_, Ok(())) => Err(format!("the installer's data ends before every wanted file ({} still missing)", sink.pending.values().map(Vec::len).sum::<usize>())),
    }
}

/// A reader that reports how much of it was read.
struct Counting<'a, R> {
    inner: R,
    read: u64,
    total: u64,
    progress: &'a mut dyn FnMut(u64, u64),
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buffer)?;
        self.read += n as u64;
        (self.progress)(self.read, self.total);
        Ok(n)
    }
}

/// NSIS's licence page (`PWP_LICENSE`, the page's window procedure number).
const PAGE_LICENSE: i32 = 0;
/// A page in the header: dialog, window procedure, three callbacks, flags, five texts, then five
/// parameters; the licence page's second parameter is its text.
const PAGE_SIZE: usize = 64;
const PAGES: usize = 0;
const LANGUAGE_TABLES: usize = 4;
/// Offset of `langtable_size` in the header (after the flags, the blocks and eight fields).
const LANGUAGE_TABLE_SIZE_AT: usize = 4 + 8 * BLOCKS + 4 * 8;
/// English (United States), preferred when the licence differs by language.
const ENGLISH: u16 = 1033;
/// Rich edit stream formats NSIS puts before a licence text: `SF_TEXT`, `SF_RTF`, and the
/// `SF_UNICODE` flag.
const SF_RTF: u16 = 2;

/// The text of the installer's licence page, as the installer would show it, from its header:
/// NSIS keeps it in the string table (the page's second parameter, or a language string when the
/// licence differs by language), led by one character giving its format (plain text or RTF).
/// GG's is plain text; RTF is turned into plain text. An installer without a licence page is an
/// error: the setup installs nothing it could not show the licence of.
pub fn license_text(header: &[u8]) -> Result<String, String> {
    let block = |index: usize| -> Option<(usize, usize)> { Some((u32_at(header, 4 + 8 * index)? as usize, u32_at(header, 8 + 8 * index)? as usize)) };
    let (pages_at, pages) = block(PAGES).ok_or("header too short")?;
    let (strings_at, _) = block(STRINGS).ok_or("header too short")?;
    let strings = header.get(strings_at..).ok_or("string table outside the header")?;
    let page = (0..pages)
        .map(|index| pages_at + index * PAGE_SIZE)
        .find(|at| u32_at(header, at + 4).map(|v| v as i32) == Some(PAGE_LICENSE))
        .ok_or("the installer has no licence page")?;
    let parameter = u32_at(header, page + 48).ok_or("licence page outside the header")? as i32;
    let offset = match parameter {
        0 => return Err("the installer's licence page has no text".into()),
        offset if offset > 0 => offset,
        language_string => language_string_offset(header, -(language_string as i64) - 1)?,
    };
    // The text is read raw: it is the licence, not a script string with variables.
    let mut at = offset as usize * 2;
    let mut units = Vec::new();
    loop {
        let unit = strings.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]])).ok_or("the licence runs past the string table")?;
        at += 2;
        if unit == 0 {
            break;
        }
        units.push(unit);
    }
    let (format, text) = units.split_first().ok_or("the licence text is empty")?;
    let text = String::from_utf16_lossy(text);
    let text = if format & 0x0F == SF_RTF { rtf_to_text(&text) } else { text };
    if text.trim().is_empty() {
        return Err("the licence text is empty".into());
    }
    Ok(text)
}

/// Where a language string is in the string table: in the English table when there is one, else
/// in the first.
fn language_string_offset(header: &[u8], id: i64) -> Result<i32, String> {
    let at = 4 + 8 * LANGUAGE_TABLES;
    let (tables_at, count) = (u32_at(header, at).ok_or("header too short")? as usize, u32_at(header, at + 4).ok_or("header too short")? as usize);
    let size = u32_at(header, LANGUAGE_TABLE_SIZE_AT).ok_or("header too short")? as usize;
    if count == 0 || size < 10 || id < 0 {
        return Err("the licence is a language string, but there is no language table".into());
    }
    let table = (0..count).map(|i| tables_at + i * size).find(|t| header.get(*t..*t + 2) == Some(&ENGLISH.to_le_bytes()[..])).unwrap_or(tables_at);
    let entry = table + 10 + 4 * id as usize;
    if entry + 4 > table + size {
        return Err("the licence's language string is outside its table".into());
    }
    let offset = u32_at(header, entry).ok_or("language table outside the header")? as i32;
    if offset <= 0 { Err("the licence's language string is empty".into()) } else { Ok(offset) }
}

/// Plain text out of simple RTF: paragraphs and line breaks kept, `\'hh` and `\uN` characters
/// decoded, control words, font and colour tables and other `{\*…}` groups dropped.
pub fn rtf_to_text(rtf: &str) -> String {
    let chars: Vec<char> = rtf.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    // Depth of the group being skipped (a destination that holds no text), 0 when none.
    let mut skip_from: Option<usize> = None;
    let mut depth = 0usize;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '{' => {
                depth += 1;
                i += 1;
                if skip_from.is_none() && chars.get(i) == Some(&'\\') {
                    let word: String = chars[i + 1..].iter().take_while(|c| c.is_ascii_alphabetic() || **c == '*').collect();
                    if word.starts_with('*') || ["fonttbl", "colortbl", "stylesheet", "info", "pict", "header", "footer"].contains(&word.as_str()) {
                        skip_from = Some(depth);
                    }
                }
            }
            '}' => {
                if skip_from == Some(depth) {
                    skip_from = None;
                }
                depth = depth.saturating_sub(1);
                i += 1;
            }
            '\\' => {
                i += 1;
                let Some(&next) = chars.get(i) else { break };
                if next == '\'' {
                    let hex: String = chars[i + 1..(i + 3).min(chars.len())].iter().collect();
                    if skip_from.is_none()
                        && let Ok(byte) = u8::from_str_radix(&hex, 16)
                    {
                        out.push(char::from(byte));
                    }
                    i += 3;
                } else if next.is_ascii_alphabetic() {
                    let word: String = chars[i..].iter().take_while(|c| c.is_ascii_alphabetic()).collect();
                    i += word.len();
                    let number: String = chars[i..].iter().take_while(|c| c.is_ascii_digit() || **c == '-').collect();
                    i += number.len();
                    if chars.get(i) == Some(&' ') {
                        i += 1;
                    }
                    if skip_from.is_some() {
                        continue;
                    }
                    match word.as_str() {
                        "par" | "line" => out.push_str("\r\n"),
                        "tab" => out.push('\t'),
                        "u" => {
                            if let Ok(code) = number.parse::<i32>() {
                                out.push(char::from_u32(if code < 0 { (code + 65536) as u32 } else { code as u32 }).unwrap_or('?'));
                            }
                            // The ANSI stand-in that follows a \u character.
                            if chars.get(i).is_some_and(|c| *c != '\\' && *c != '{' && *c != '}') {
                                i += 1;
                            }
                        }
                        _ => {}
                    }
                } else {
                    if skip_from.is_none() && matches!(next, '\\' | '{' | '}') {
                        out.push(next);
                    }
                    i += 1;
                }
            }
            '\r' | '\n' => i += 1,
            _ => {
                if skip_from.is_none() {
                    out.push(c);
                }
                i += 1;
            }
        }
    }
    out
}

/// The licence text of `installer` (see `license_text`).
pub fn license(installer: &Path) -> Result<String, String> {
    license_text(&header(installer)?)
}

/// The files of `installer`, without unpacking more than its header (the decoder still unpacks
/// up to 64 MB before handing anything over).
pub fn list(installer: &Path) -> Result<Vec<Item>, String> {
    items(&header(installer)?)
}

/// The installer's header, unpacked (the decoder still unpacks up to 64 MB before handing
/// anything over).
pub fn header(installer: &Path) -> Result<Vec<u8>, String> {
    let mut file = File::open(installer).map_err(|e| format!("cannot open {}: {e}", installer.display()))?;
    let (start, first) = find_first_header(&mut file).map_err(|e| e.to_string())?.ok_or_else(|| format!("{} is not an NSIS installer", installer.display()))?;
    let data_start = start + FIRST_HEADER as u64;
    file.seek(SeekFrom::Start(data_start)).map_err(|e| e.to_string())?;
    let mut probe = [0u8; 16];
    file.read_exact(&mut probe).map_err(|e| e.to_string())?;
    let skip = lzma_start(&probe, first.header_size)?;
    file.seek(SeekFrom::Start(data_start + skip as u64)).map_err(|e| e.to_string())?;
    let mut input = BufReader::with_capacity(1 << 20, file);
    /// Keeps the first `size + 4` bytes and stops the decoder there.
    struct Head {
        bytes: Vec<u8>,
        want: usize,
    }
    impl Write for Head {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            let n = (self.want - self.bytes.len()).min(data.len());
            self.bytes.extend_from_slice(&data[..n]);
            if self.bytes.len() == self.want { Err(io::Error::other("done")) } else { Ok(data.len()) }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut head = Head { bytes: Vec::new(), want: first.header_size as usize + 4 };
    let options = lzma_rs::decompress::Options { unpacked_size: lzma_rs::decompress::UnpackedSize::UseProvided(None), memlimit: None, allow_incomplete: true };
    let _ = lzma_rs::lzma_decompress_with_options(&mut input, &mut head, &options);
    if head.bytes.len() < head.want {
        return Err("the installer's header cannot be unpacked".into());
    }
    Ok(head.bytes.split_off(4))
}

#[cfg(test)]
#[path = "nsis_tests.rs"]
mod tests;
