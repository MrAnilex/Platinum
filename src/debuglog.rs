//! Collects what this process writes with OutputDebugString, without involving any debugger, debug
//! viewer or other process. Some third-party components (the vendor's effects engine among them)
//! report their decisions only there.
//!
//! OutputDebugString raises a special exception that a debugger would pick up; when none does, the
//! function falls back to a buffer shared with debug viewers. A vectored exception handler sees the
//! exception first: it copies the text and tells Windows the exception was handled, so the message
//! never leaves the process, and other processes' messages are never touched.

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

const DBG_PRINTEXCEPTION_C: u32 = 0x4001_0006;
const DBG_PRINTEXCEPTION_WIDE_C: u32 = 0x4001_000A;
const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;
const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
/// Longest message kept, in characters; OutputDebugString itself stops at 4 KB.
const MAX_LENGTH: usize = 4096;
/// Most messages kept: a verbose component logs statistics every few seconds.
const MAX_LINES: usize = 10_000;

/// EXCEPTION_RECORD on 64-bit Windows.
#[repr(C)]
struct ExceptionRecord {
    code: u32,
    flags: u32,
    record: *mut ExceptionRecord,
    address: *mut c_void,
    parameters: u32,
    information: [usize; 15],
}

#[repr(C)]
struct ExceptionPointers {
    record: *mut ExceptionRecord,
    context: *mut c_void,
}

const _: () = assert!(std::mem::offset_of!(ExceptionRecord, parameters) == 24);
const _: () = assert!(std::mem::offset_of!(ExceptionRecord, information) == 32);

#[link(name = "kernel32")]
unsafe extern "system" {
    fn AddVectoredExceptionHandler(first: u32, handler: unsafe extern "system" fn(*mut ExceptionPointers) -> i32) -> *mut c_void;
    fn RemoveVectoredExceptionHandler(handle: *mut c_void) -> u32;
}

static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Text of `length` units at `text` (the count includes the final NUL), stopping at a NUL.
unsafe fn wide_text(text: *const u16, length: usize) -> String {
    if text.is_null() {
        return String::new();
    }
    unsafe {
        let units = std::slice::from_raw_parts(text, length.min(MAX_LENGTH));
        let end = units.iter().position(|u| *u == 0).unwrap_or(units.len());
        String::from_utf16_lossy(&units[..end])
    }
}

unsafe fn ansi_text(text: *const u8, length: usize) -> String {
    if text.is_null() {
        return String::new();
    }
    unsafe {
        let bytes = std::slice::from_raw_parts(text, length.min(MAX_LENGTH));
        let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[..end]).into_owned()
    }
}

unsafe extern "system" fn on_exception(pointers: *mut ExceptionPointers) -> i32 {
    unsafe {
        if pointers.is_null() || (*pointers).record.is_null() || !ACTIVE.load(Ordering::SeqCst) {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let record = &*(*pointers).record;
        // Both exceptions carry the length (with the final NUL) then the text in their first two
        // parameters; the wide one carries an ANSI copy after them.
        let text = match record.code {
            DBG_PRINTEXCEPTION_WIDE_C if record.parameters >= 2 => wide_text(record.information[1] as *const u16, record.information[0]),
            DBG_PRINTEXCEPTION_C if record.parameters >= 2 => ansi_text(record.information[1] as *const u8, record.information[0]),
            _ => return EXCEPTION_CONTINUE_SEARCH,
        };
        let text = text.trim_end();
        if !text.is_empty() {
            if let Ok(mut lines) = LINES.lock() {
                if lines.len() < MAX_LINES {
                    lines.push(text.to_string());
                }
            }
        }
        EXCEPTION_CONTINUE_EXECUTION
    }
}

/// Collects this process's debug messages until dropped.
pub struct DebugLog {
    handler: *mut c_void,
}

impl DebugLog {
    /// Starts collecting. The note says why nothing will be collected, if that is the case.
    pub fn start() -> (DebugLog, Option<String>) {
        if ACTIVE.swap(true, Ordering::SeqCst) {
            return (DebugLog { handler: std::ptr::null_mut() }, Some("debug messages are already being collected elsewhere in this process".into()));
        }
        if let Ok(mut lines) = LINES.lock() {
            lines.clear();
        }
        let handler = unsafe { AddVectoredExceptionHandler(1, on_exception) };
        if handler.is_null() {
            ACTIVE.store(false, Ordering::SeqCst);
            return (DebugLog { handler }, Some("debug messages cannot be collected (no exception handler)".into()));
        }
        (DebugLog { handler }, None)
    }

    /// The messages collected so far.
    pub fn lines(&self) -> Vec<String> {
        if self.handler.is_null() {
            return Vec::new();
        }
        LINES.lock().map(|lines| lines.clone()).unwrap_or_default()
    }
}

impl Drop for DebugLog {
    fn drop(&mut self) {
        if !self.handler.is_null() {
            unsafe {
                RemoveVectoredExceptionHandler(self.handler);
            }
            ACTIVE.store(false, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::com::wide;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OutputDebugStringW(text: *const u16);
        fn OutputDebugStringA(text: *const u8);
    }

    #[test]
    fn collects_this_process_messages_in_both_encodings() {
        let (log, note) = DebugLog::start();
        assert!(note.is_none(), "{note:?}");
        unsafe {
            OutputDebugStringW(wide("platinum wide message é\n").as_ptr());
            OutputDebugStringA(b"platinum ansi message\0".as_ptr());
        }
        let lines = log.lines();
        assert!(lines.iter().any(|l| l == "platinum wide message é"), "lines: {lines:?}");
        assert!(lines.iter().any(|l| l == "platinum ansi message"), "lines: {lines:?}");
        // A second collector in the same process is refused rather than racing the first.
        let (second, note) = DebugLog::start();
        assert!(note.is_some() && second.lines().is_empty());
        drop(second);
        drop(log);
        assert!(!ACTIVE.load(Ordering::SeqCst));
    }
}
