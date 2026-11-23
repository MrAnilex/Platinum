//! The setup's journal, `%USERPROFILE%\Platinum\install.log` (appended to: earlier installs stay
//! readable above). Lines are English, stamped with the local time; a line that cannot be written
//! is dropped rather than stopping the install. With `echo`, lines also go to the console (dry
//! run, elevated part started from a console).
//!
//! Each line is written on its own: the file is opened, the line appended and flushed, the file
//! closed. A journal read while the setup runs (or after the setup was killed) shows every line
//! up to the last one, and Explorer shows its real size. The elevated part's journal is reopened
//! the same way for every line, each time refusing a link (`open_plain_file`).

use std::io::Write;
use std::path::{Path, PathBuf};

/// Where the lines go.
enum Target {
    Nowhere,
    /// The user's journal: created if missing.
    File,
    /// The elevated part's journal: an existing plain file, links refused.
    Plain,
}

pub struct Log {
    path: PathBuf,
    target: Target,
    pub echo: bool,
}

impl Log {
    pub fn open(path: &Path) -> Log {
        if let Some(folder) = path.parent() {
            let _ = std::fs::create_dir_all(folder);
        }
        let log = Log { path: path.to_path_buf(), target: Target::File, echo: false };
        // Created now, so that it exists before the first line.
        drop(log.file());
        log
    }

    /// A journal that writes nowhere (snapshots, tests).
    pub fn none() -> Log {
        Log { path: PathBuf::new(), target: Target::Nowhere, echo: false }
    }

    /// The elevated part's journal: the file the user's part created, if it is a plain file now;
    /// it is checked again for every line.
    pub fn plain(path: &Path) -> Log {
        let target = if open_plain_file(path).is_some() { Target::Plain } else { Target::Nowhere };
        Log { path: path.to_path_buf(), target, echo: false }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn file(&self) -> Option<std::fs::File> {
        match self.target {
            Target::Nowhere => None,
            Target::File => std::fs::OpenOptions::new().create(true).append(true).open(&self.path).ok(),
            Target::Plain => open_plain_file(&self.path),
        }
    }

    /// Appends `text` and flushes it to disk; the file is closed again on return.
    fn append(&self, text: &str) {
        if let Some(mut file) = self.file() {
            if file.write_all(text.as_bytes()).is_ok() {
                let _ = file.flush();
                let _ = file.sync_data();
            }
        }
    }

    pub fn line(&mut self, text: impl AsRef<str>) {
        let line = format!("[{}] {}", platinum::guard::stamp(), text.as_ref());
        if self.echo {
            println!("{line}");
        }
        self.append(&format!("{line}\n"));
    }

    /// Appends another journal's lines, indented.
    fn indented(&self, bytes: &[u8]) {
        let text: String = String::from_utf8_lossy(bytes).lines().map(|line| format!("    {line}\n")).collect();
        if !text.is_empty() {
            self.append(&text);
        }
    }
}

/// Follows another journal (the elevated part's, written by another process) and copies its new
/// lines into this one, indented, while that process runs.
pub struct Follow {
    path: PathBuf,
    offset: usize,
}

impl Follow {
    pub fn new(path: &Path) -> Follow {
        Follow { path: path.to_path_buf(), offset: 0 }
    }

    /// Copies the lines finished since the last call; with `to_end`, an unfinished last line too
    /// (the other process has ended).
    pub fn pump(&mut self, log: &Log, to_end: bool) {
        let Ok(bytes) = std::fs::read(&self.path) else { return };
        let Some(new) = bytes.get(self.offset..) else { return };
        let length = if to_end { new.len() } else { new.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1) };
        if length > 0 {
            log.indented(&new[..length]);
            self.offset += length;
        }
    }
}

/// Opens an existing journal file for appending, refusing a link or junction: the elevated part
/// writes into a file the user's part created, and must not be steered elsewhere.
pub fn open_plain_file(path: &Path) -> Option<std::fs::File> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    let file = std::fs::OpenOptions::new().append(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(path).ok()?;
    let metadata = file.metadata().ok()?;
    (metadata.is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0).then_some(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_stamped_and_appended() {
        let path = std::env::temp_dir().join(format!("platinum-setup-log-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Log::open(&path).line("first");
        Log::open(&path).line("second");
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with('[') && lines[0].ends_with("] first"), "{}", lines[0]);
        assert!(open_plain_file(&path).is_some());
        let _ = std::fs::remove_file(&path);
        assert!(open_plain_file(&path).is_none());
    }

    /// Every line is in the file as soon as `line` returns, and the size the folder listing shows
    /// (what Explorer shows: NTFS updates it when the file is closed) follows line by line.
    #[test]
    fn each_line_is_on_disk_at_once_and_the_listing_shows_it() {
        let folder = std::env::temp_dir().join(format!("platinum-setup-log-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        let path = folder.join("install.log");
        let listed = || std::fs::read_dir(&folder).unwrap().flatten().find(|e| e.file_name() == "install.log").map(|e| e.metadata().unwrap().len());
        let mut log = Log::open(&path);
        assert_eq!(listed(), Some(0));
        log.line("one");
        let after_one = listed().unwrap();
        assert_eq!(after_one, std::fs::read(&path).unwrap().len() as u64);
        assert!(after_one > 0);
        log.line("two");
        assert_eq!(listed().unwrap(), std::fs::read(&path).unwrap().len() as u64);
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// The elevated journal writes only into a file that exists, line by line, and the user's
    /// journal takes its finished lines in, indented, as they come.
    #[test]
    fn the_elevated_journal_is_plain_and_followed_line_by_line() {
        let base = std::env::temp_dir().join(format!("platinum-setup-log-elevated-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let missing = base.join("missing.log");
        Log::plain(&missing).line("lost");
        assert!(!missing.exists());
        let journal = base.join("elevated.log");
        std::fs::write(&journal, b"").unwrap();
        let main = Log::open(&base.join("install.log"));
        let read = || std::fs::read_to_string(base.join("install.log")).unwrap();
        let mut follow = Follow::new(&journal);
        let mut elevated = Log::plain(&journal);
        elevated.line("a");
        follow.pump(&main, false);
        assert_eq!(read().lines().count(), 1);
        elevated.line("b");
        std::fs::OpenOptions::new().append(true).open(&journal).unwrap().write_all(b"unfinished").unwrap();
        follow.pump(&main, false);
        assert_eq!(read().lines().count(), 2);
        follow.pump(&main, false);
        assert_eq!(read().lines().count(), 2);
        follow.pump(&main, true);
        let text = read();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[..2].iter().all(|l| l.starts_with("    [")) && lines[2] == "    unfinished", "{text}");
        let _ = std::fs::remove_dir_all(&base);
    }
}
