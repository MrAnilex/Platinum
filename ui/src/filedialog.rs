//! Windows' own Open and Save As dialogs, for exporting and importing presets.
//!
//! Each dialog runs on a thread of its own: it has its own message loop, which must not run inside
//! a callback of the window's event loop. The window is its owner: Windows keeps the dialog above
//! it and holds its input until the dialog closes, as for any modal dialog.

use platinum::i18n::tr;
use std::ffi::c_void;
use std::path::PathBuf;

#[repr(C)]
struct OpenFileName {
    struct_size: u32,
    owner: *mut c_void,
    instance: *mut c_void,
    filter: *const u16,
    custom_filter: *mut u16,
    max_custom_filter: u32,
    filter_index: u32,
    file: *mut u16,
    max_file: u32,
    file_title: *mut u16,
    max_file_title: u32,
    initial_dir: *const u16,
    title: *const u16,
    flags: u32,
    file_offset: u16,
    file_extension: u16,
    default_extension: *const u16,
    custom_data: isize,
    hook: *const c_void,
    template_name: *const u16,
    reserved: *mut c_void,
    reserved_number: u32,
    flags_ex: u32,
}

#[link(name = "comdlg32")]
unsafe extern "system" {
    fn GetOpenFileNameW(dialog: *mut OpenFileName) -> i32;
    fn GetSaveFileNameW(dialog: *mut OpenFileName) -> i32;
}

#[link(name = "ole32")]
unsafe extern "system" {
    fn CoInitializeEx(reserved: *mut c_void, flags: u32) -> i32;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn GetForegroundWindow() -> *mut c_void;
    fn GetWindowThreadProcessId(window: *mut c_void, pid: *mut u32) -> u32;
}

const OFN_OVERWRITEPROMPT: u32 = 0x0000_0002;
const OFN_HIDEREADONLY: u32 = 0x0000_0004;
const OFN_NOCHANGEDIR: u32 = 0x0000_0008;
const OFN_PATHMUSTEXIST: u32 = 0x0000_0800;
const OFN_FILEMUSTEXIST: u32 = 0x0000_1000;
const OFN_EXPLORER: u32 = 0x0008_0000;
const COINIT_APARTMENTTHREADED: u32 = 0x2;
/// Room for the chosen path, in UTF-16 units.
const PATH_ROOM: usize = 1024;

/// The window in front, when it is ours: the dialog's owner. Read on the window's thread, as the
/// user clicks.
pub fn owner() -> isize {
    unsafe {
        let window = GetForegroundWindow();
        let mut pid = 0u32;
        GetWindowThreadProcessId(window, &mut pid);
        if pid == std::process::id() { window as isize } else { 0 }
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Shows the dialog; `None` when the user cancels.
fn show(owner: isize, title: &str, suggested: &str, save: bool) -> Option<PathBuf> {
    // The dialog's shell views want a single-threaded apartment on their thread.
    unsafe { CoInitializeEx(std::ptr::null_mut(), COINIT_APARTMENTTHREADED) };
    let filter: Vec<u16> = format!("{}\0*.json\0{}\0*.*\0\0", tr("Préréglage Sonar (*.json)"), tr("Tous les fichiers (*.*)")).encode_utf16().collect();
    let (title, extension) = (wide(title), wide("json"));
    let mut file = vec![0u16; PATH_ROOM];
    for (slot, unit) in file.iter_mut().zip(suggested.encode_utf16().take(PATH_ROOM - 1)) {
        *slot = unit;
    }
    let flags = OFN_EXPLORER | OFN_NOCHANGEDIR | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY | if save { OFN_OVERWRITEPROMPT } else { OFN_FILEMUSTEXIST };
    let mut dialog = OpenFileName {
        struct_size: std::mem::size_of::<OpenFileName>() as u32,
        owner: owner as *mut c_void,
        instance: std::ptr::null_mut(),
        filter: filter.as_ptr(),
        custom_filter: std::ptr::null_mut(),
        max_custom_filter: 0,
        filter_index: 1,
        file: file.as_mut_ptr(),
        max_file: PATH_ROOM as u32,
        file_title: std::ptr::null_mut(),
        max_file_title: 0,
        initial_dir: std::ptr::null(),
        title: title.as_ptr(),
        flags,
        file_offset: 0,
        file_extension: 0,
        default_extension: extension.as_ptr(),
        custom_data: 0,
        hook: std::ptr::null(),
        template_name: std::ptr::null(),
        reserved: std::ptr::null_mut(),
        reserved_number: 0,
        flags_ex: 0,
    };
    let chosen = unsafe { if save { GetSaveFileNameW(&mut dialog) } else { GetOpenFileNameW(&mut dialog) } };
    if chosen == 0 {
        return None;
    }
    let length = file.iter().position(|&unit| unit == 0).unwrap_or(file.len());
    Some(PathBuf::from(String::from_utf16_lossy(&file[..length])))
}

/// Asks where to save `text`, then writes it there. `done` gets what happened, on the dialog's
/// thread: `None` when the user cancelled.
pub fn save(owner: isize, title: String, suggested: String, text: String, done: impl FnOnce(Option<Result<PathBuf, String>>) + Send + 'static) {
    let _ = std::thread::Builder::new().name("enregistrer sous".into()).spawn(move || {
        let outcome = show(owner, &title, &suggested, true).map(|path| std::fs::write(&path, text.as_bytes()).map(|_| path).map_err(|e| e.to_string()));
        done(outcome);
    });
}

/// Asks for a file to open and reads it. `done` gets the file and its text, on the dialog's
/// thread: `None` when the user cancelled.
pub fn open(owner: isize, title: String, done: impl FnOnce(Option<Result<(PathBuf, String), String>>) + Send + 'static) {
    let _ = std::thread::Builder::new().name("ouvrir".into()).spawn(move || {
        let outcome = show(owner, &title, "", false).map(|path| {
            std::fs::read(&path).map(|bytes| (path.clone(), String::from_utf8_lossy(&bytes).into_owned())).map_err(|e| format!("{} : {e}", path.display()))
        });
        done(outcome);
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_dialog_structure_has_windows_size() {
        // OPENFILENAMEW is 152 bytes on 64-bit Windows.
        assert_eq!(std::mem::size_of::<super::OpenFileName>(), 152);
    }
}
