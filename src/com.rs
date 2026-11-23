//! Minimal COM plumbing shared by the Windows backends: no dependencies, raw vtable calls.

use std::ffi::c_void;
use std::fmt;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Guid {
    pub d1: u32,
    pub d2: u16,
    pub d3: u16,
    pub d4: [u8; 8],
}

impl fmt::Display for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = &self.d4;
        write!(f, "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}", self.d1, self.d2, self.d3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7])
    }
}

#[derive(Debug)]
pub enum Error {
    Load(String),
    Call { what: &'static str, hresult: i32 },
    Invalid(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Load(msg) | Error::Invalid(msg) => f.write_str(msg),
            Error::Call { what, hresult } => write!(f, "{what} failed (HRESULT 0x{:08X})", *hresult as u32),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn check(what: &'static str, hresult: i32) -> Result<()> {
    if hresult < 0 { Err(Error::Call { what, hresult }) } else { Ok(()) }
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    fn GetLastError() -> u32;
}

/// NUL-terminated UTF-16 copy of `text`.
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Reads a NUL-terminated UTF-16 string. `ptr` must be valid or null.
pub unsafe fn read_wide(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe {
        let mut len = 0;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
    }
}

/// Loads a DLL and keeps it loaded for the life of the process.
pub fn load_library(name: &str, flags: u32) -> Result<*mut c_void> {
    let module = unsafe { LoadLibraryExW(wide(name).as_ptr(), std::ptr::null_mut(), flags) };
    if module.is_null() {
        return Err(Error::Load(format!("cannot load {name} (Windows error {})", unsafe { GetLastError() })));
    }
    Ok(module)
}

/// Address of an exported function. `name` must end with a NUL byte.
pub fn export(module: *mut c_void, name: &'static [u8]) -> Result<*mut c_void> {
    let address = unsafe { GetProcAddress(module, name.as_ptr()) };
    if address.is_null() {
        let shown = String::from_utf8_lossy(&name[..name.len() - 1]);
        return Err(Error::Load(format!("export {shown} not found")));
    }
    Ok(address)
}

/// An owned COM interface pointer; released on drop.
pub struct Com(pub *mut c_void);

impl Com {
    /// Raw function pointer stored at `index` in the object's vtable.
    pub unsafe fn entry(&self, index: usize) -> *const c_void {
        unsafe { *(*(self.0 as *const *const *const c_void)).add(index) }
    }

    /// Another interface of the same object (QueryInterface).
    pub fn query(&self, interface: &Guid, what: &'static str) -> Result<Com> {
        unsafe {
            let query: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32 = std::mem::transmute(self.entry(0));
            let mut raw = std::ptr::null_mut();
            check(what, query(self.0, interface, &mut raw))?;
            Ok(Com(raw))
        }
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let release: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(self.entry(2));
                release(self.0);
            }
        }
    }
}

/// The COM runtime functions used here, resolved from ole32 at run time.
pub struct Ole {
    create: unsafe extern "system" fn(*const Guid, *mut c_void, u32, *const Guid, *mut *mut c_void) -> i32,
    free: unsafe extern "system" fn(*mut c_void),
    clear: unsafe extern "system" fn(*mut c_void) -> i32,
}

impl Ole {
    /// Initialises COM on the calling thread (harmless if already done) and resolves the functions.
    pub fn load() -> Result<Ole> {
        let module = load_library("ole32.dll", 0)?;
        unsafe {
            let initialize: unsafe extern "system" fn(*mut c_void, u32) -> i32 = std::mem::transmute(export(module, b"CoInitializeEx\0")?);
            check("CoInitializeEx", initialize(std::ptr::null_mut(), 0))?;
            Ok(Ole {
                create: std::mem::transmute(export(module, b"CoCreateInstance\0")?),
                free: std::mem::transmute(export(module, b"CoTaskMemFree\0")?),
                clear: std::mem::transmute(export(module, b"PropVariantClear\0")?),
            })
        }
    }

    /// Creates a registered COM object.
    pub fn create(&self, class: &Guid, interface: &Guid, what: &'static str) -> Result<Com> {
        let mut raw = std::ptr::null_mut();
        // 23 = CLSCTX_ALL
        check(what, unsafe { (self.create)(class, std::ptr::null_mut(), 23, interface, &mut raw) })?;
        Ok(Com(raw))
    }

    /// Takes ownership of a string allocated by COM.
    pub unsafe fn take_wide(&self, ptr: *mut u16) -> String {
        unsafe {
            let text = read_wide(ptr);
            (self.free)(ptr.cast());
            text
        }
    }

    /// Frees memory allocated by COM.
    pub unsafe fn free(&self, ptr: *mut c_void) {
        unsafe {
            (self.free)(ptr);
        }
    }

    /// Releases whatever a PROPVARIANT owns.
    pub unsafe fn clear_variant(&self, variant: *mut c_void) {
        unsafe {
            (self.clear)(variant);
        }
    }
}
