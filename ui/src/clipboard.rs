//! Text to the Windows clipboard, for a crash report the user sends by hand (or one too long for
//! the GitHub link). Win32 only: Slint has no clipboard call of its own, and the library stays
//! without dependencies.

use std::ffi::c_void;

#[link(name = "user32")]
unsafe extern "system" {
    fn OpenClipboard(owner: *mut c_void) -> i32;
    fn EmptyClipboard() -> i32;
    fn SetClipboardData(format: u32, data: *mut c_void) -> *mut c_void;
    fn CloseClipboard() -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
    fn GlobalLock(memory: *mut c_void) -> *mut c_void;
    fn GlobalUnlock(memory: *mut c_void) -> i32;
    fn GlobalFree(memory: *mut c_void) -> *mut c_void;
}

const CF_UNICODETEXT: u32 = 13;
const GMEM_MOVEABLE: u32 = 0x0002;

/// The text as the clipboard holds it: UTF-16, Windows line breaks, a terminating zero.
pub fn clipboard_units(text: &str) -> Vec<u16> {
    text.replace("\r\n", "\n").replace('\n', "\r\n").encode_utf16().chain(std::iter::once(0)).collect()
}

/// Puts `text` on the clipboard, replacing what was there. Error in French, for the window.
pub fn set_text(text: &str) -> Result<(), String> {
    let units = clipboard_units(text);
    let bytes = units.len() * 2;
    unsafe {
        // Another program may hold the clipboard for an instant.
        let mut opened = false;
        for _ in 0..10 {
            if OpenClipboard(std::ptr::null_mut()) != 0 {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if !opened {
            return Err("le presse-papiers est occupé par un autre programme".into());
        }
        let result = (|| {
            EmptyClipboard();
            let memory = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if memory.is_null() {
                return Err("mémoire insuffisante pour le presse-papiers".to_string());
            }
            let target = GlobalLock(memory).cast::<u16>();
            if target.is_null() {
                GlobalFree(memory);
                return Err("mémoire du presse-papiers inaccessible".to_string());
            }
            std::ptr::copy_nonoverlapping(units.as_ptr(), target, units.len());
            GlobalUnlock(memory);
            // Once given to the clipboard, the memory is the system's.
            if SetClipboardData(CF_UNICODETEXT, memory).is_null() {
                GlobalFree(memory);
                return Err("le presse-papiers a refusé le texte".to_string());
            }
            Ok(())
        })();
        CloseClipboard();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clipboard_gets_windows_line_breaks_and_a_terminating_zero() {
        assert_eq!(clipboard_units("a\nb\r\nc"), "a\r\nb\r\nc\0".encode_utf16().collect::<Vec<_>>());
        assert_eq!(clipboard_units(""), [0]);
    }
}
