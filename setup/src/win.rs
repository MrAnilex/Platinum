//! The Windows calls the setup needs, without crates: Authenticode checks (WinVerifyTrust and the
//! signer's name), SHA-256 (BCrypt), elevation (the token, and `ShellExecuteExW` with "runas"),
//! known folders, shortcuts (IShellLinkW), the per-user "Installed apps" entry, and a few small
//! helpers. wintrust.dll has no import library in the GNU toolchain: it is loaded at run time.

use platinum::com::{Com, Guid, Ole, check, export, load_library, read_wide, wide};
use std::ffi::c_void;
use std::io::Read;
use std::path::{Path, PathBuf};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut c_void;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    fn GetExitCodeProcess(process: *mut c_void, code: *mut u32) -> i32;
    fn GetLastError() -> u32;
    fn AttachConsole(pid: u32) -> i32;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: *mut c_void, access: u32, token: *mut *mut c_void) -> i32;
    fn GetTokenInformation(token: *mut c_void, class: u32, info: *mut c_void, length: u32, returned: *mut u32) -> i32;
    fn RegSetKeyValueW(key: isize, subkey: *const u16, value: *const u16, kind: u32, data: *const c_void, bytes: u32) -> i32;
    fn RegDeleteTreeW(key: isize, subkey: *const u16) -> i32;
    fn RegDeleteKeyW(key: isize, subkey: *const u16) -> i32;
}

#[link(name = "bcrypt")]
unsafe extern "system" {
    fn BCryptOpenAlgorithmProvider(algorithm: *mut *mut c_void, id: *const u16, implementation: *const u16, flags: u32) -> i32;
    fn BCryptCreateHash(algorithm: *mut c_void, hash: *mut *mut c_void, object: *mut u8, object_len: u32, secret: *const u8, secret_len: u32, flags: u32) -> i32;
    fn BCryptHashData(hash: *mut c_void, input: *const u8, len: u32, flags: u32) -> i32;
    fn BCryptFinishHash(hash: *mut c_void, output: *mut u8, len: u32, flags: u32) -> i32;
    fn BCryptDestroyHash(hash: *mut c_void) -> i32;
    fn BCryptCloseAlgorithmProvider(algorithm: *mut c_void, flags: u32) -> i32;
}

#[link(name = "crypt32")]
unsafe extern "system" {
    fn CertGetNameStringW(cert: *const c_void, kind: u32, flags: u32, kind_para: *const c_void, name: *mut u16, length: u32) -> u32;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn ShellExecuteExW(info: *mut ShellExecuteInfo) -> i32;
    fn ShellExecuteW(window: *mut c_void, verb: *const u16, file: *const u16, parameters: *const u16, directory: *const u16, show: i32) -> isize;
    fn SHGetKnownFolderPath(id: *const Guid, flags: u32, token: *mut c_void, path: *mut *mut u16) -> i32;
}

#[link(name = "ole32")]
unsafe extern "system" {
    fn CoTaskMemFree(memory: *mut c_void);
}

pub const HKEY_CURRENT_USER: isize = 0x8000_0001_u32 as i32 as isize;
pub const HKEY_LOCAL_MACHINE: isize = 0x8000_0002_u32 as i32 as isize;
const REG_SZ: u32 = 1;
const REG_DWORD: u32 = 4;
const ERROR_CANCELLED: u32 = 1223;

/// SHA-256 of a file, through Windows' own implementation.
pub fn sha256_file(path: &Path) -> Result<[u8; 32], String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut digest = [0u8; 32];
    unsafe {
        let mut algorithm = std::ptr::null_mut();
        if BCryptOpenAlgorithmProvider(&mut algorithm, wide("SHA256").as_ptr(), std::ptr::null(), 0) != 0 {
            return Err("SHA-256 is not available".into());
        }
        let mut hash = std::ptr::null_mut();
        if BCryptCreateHash(algorithm, &mut hash, std::ptr::null_mut(), 0, std::ptr::null(), 0, 0) != 0 {
            BCryptCloseAlgorithmProvider(algorithm, 0);
            return Err("SHA-256 is not available".into());
        }
        let mut buffer = vec![0u8; 1 << 20];
        let mut outcome = Ok(());
        loop {
            match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if BCryptHashData(hash, buffer.as_ptr(), n as u32, 0) != 0 {
                        outcome = Err("SHA-256 failed".to_string());
                        break;
                    }
                }
                Err(e) => {
                    outcome = Err(format!("cannot read {}: {e}", path.display()));
                    break;
                }
            }
        }
        if outcome.is_ok() && BCryptFinishHash(hash, digest.as_mut_ptr(), 32, 0) != 0 {
            outcome = Err("SHA-256 failed".into());
        }
        BCryptDestroyHash(hash);
        BCryptCloseAlgorithmProvider(algorithm, 0);
        outcome?;
    }
    Ok(digest)
}

/// SHA-256 of bytes in memory (the licence text, for the journal).
pub fn sha256_bytes(data: &[u8]) -> Result<[u8; 32], String> {
    let mut digest = [0u8; 32];
    unsafe {
        let mut algorithm = std::ptr::null_mut();
        if BCryptOpenAlgorithmProvider(&mut algorithm, wide("SHA256").as_ptr(), std::ptr::null(), 0) != 0 {
            return Err("SHA-256 is not available".into());
        }
        let mut hash = std::ptr::null_mut();
        let ok = BCryptCreateHash(algorithm, &mut hash, std::ptr::null_mut(), 0, std::ptr::null(), 0, 0) == 0
            && data.chunks(1 << 20).all(|chunk| BCryptHashData(hash, chunk.as_ptr(), chunk.len() as u32, 0) == 0)
            && BCryptFinishHash(hash, digest.as_mut_ptr(), 32, 0) == 0;
        if !hash.is_null() {
            BCryptDestroyHash(hash);
        }
        BCryptCloseAlgorithmProvider(algorithm, 0);
        if !ok {
            return Err("SHA-256 failed".into());
        }
    }
    Ok(digest)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[repr(C)]
struct FileInfo {
    size: u32,
    path: *const u16,
    file: *mut c_void,
    known_subject: *const Guid,
}

#[repr(C)]
struct TrustData {
    size: u32,
    policy_data: *mut c_void,
    sip_data: *mut c_void,
    ui_choice: u32,
    revocation: u32,
    union_choice: u32,
    file: *mut FileInfo,
    state_action: u32,
    state: *mut c_void,
    url_reference: *mut u16,
    provider_flags: u32,
    ui_context: u32,
    signature_settings: *mut c_void,
}

#[repr(C)]
struct ProviderSigner {
    size: u32,
    verify_as_of: [u32; 2],
    chain_length: u32,
    chain: *const ProviderCert,
}

#[repr(C)]
struct ProviderCert {
    size: u32,
    cert: *const c_void,
}

/// WINTRUST_ACTION_GENERIC_VERIFY_V2: Authenticode.
const VERIFY_V2: Guid = Guid { d1: 0x00AA_C56B, d2: 0xCD44, d3: 0x11D0, d4: [0x8C, 0xC2, 0x00, 0xC0, 0x4F, 0xC2, 0x95, 0xEE] };
const WTD_UI_NONE: u32 = 2;
const WTD_REVOKE_NONE: u32 = 0;
const WTD_CHOICE_FILE: u32 = 1;
const WTD_STATEACTION_VERIFY: u32 = 1;
const WTD_STATEACTION_CLOSE: u32 = 2;
const WTD_REVOCATION_CHECK_NONE: u32 = 0x10;
const CERT_NAME_SIMPLE_DISPLAY_TYPE: u32 = 4;

/// Checks a file's Authenticode signature (embedded, or a signed catalog file) and returns the
/// name of its signer ("GN Hearing A/S", "Microsoft Corporation"…). Revocation is not checked
/// online: the setup must work offline once its files are downloaded.
pub fn signer(path: &Path) -> Result<String, String> {
    let wintrust = load_library("wintrust.dll", 0).map_err(|e| e.to_string())?;
    unsafe {
        let verify: unsafe extern "system" fn(*mut c_void, *const Guid, *mut TrustData) -> i32 = std::mem::transmute(export(wintrust, b"WinVerifyTrust\0").map_err(|e| e.to_string())?);
        let provider_data: unsafe extern "system" fn(*mut c_void) -> *mut c_void = std::mem::transmute(export(wintrust, b"WTHelperProvDataFromStateData\0").map_err(|e| e.to_string())?);
        let signer_of: unsafe extern "system" fn(*mut c_void, u32, i32, u32) -> *const ProviderSigner = std::mem::transmute(export(wintrust, b"WTHelperGetProvSignerFromChain\0").map_err(|e| e.to_string())?);
        let path_wide = wide(&path.display().to_string());
        let mut file = FileInfo { size: std::mem::size_of::<FileInfo>() as u32, path: path_wide.as_ptr(), file: std::ptr::null_mut(), known_subject: std::ptr::null() };
        let mut data = TrustData {
            size: std::mem::size_of::<TrustData>() as u32,
            policy_data: std::ptr::null_mut(),
            sip_data: std::ptr::null_mut(),
            ui_choice: WTD_UI_NONE,
            revocation: WTD_REVOKE_NONE,
            union_choice: WTD_CHOICE_FILE,
            file: &mut file,
            state_action: WTD_STATEACTION_VERIFY,
            state: std::ptr::null_mut(),
            url_reference: std::ptr::null_mut(),
            provider_flags: WTD_REVOCATION_CHECK_NONE,
            ui_context: 0,
            signature_settings: std::ptr::null_mut(),
        };
        // INVALID_HANDLE_VALUE as the window: no interface at all.
        let status = verify(-1isize as *mut c_void, &VERIFY_V2, &mut data);
        let mut name = String::new();
        if status == 0 && !data.state.is_null() {
            let provider = provider_data(data.state);
            if !provider.is_null() {
                let signer = signer_of(provider, 0, 0, 0);
                if !signer.is_null() && (*signer).chain_length > 0 && !(*signer).chain.is_null() {
                    let cert = (*(*signer).chain).cert;
                    let mut buffer = [0u16; 256];
                    let length = CertGetNameStringW(cert, CERT_NAME_SIMPLE_DISPLAY_TYPE, 0, std::ptr::null(), buffer.as_mut_ptr(), buffer.len() as u32);
                    if length > 1 {
                        name = String::from_utf16_lossy(&buffer[..length as usize - 1]);
                    }
                }
            }
        }
        data.state_action = WTD_STATEACTION_CLOSE;
        verify(-1isize as *mut c_void, &VERIFY_V2, &mut data);
        if status != 0 {
            return Err(format!("{}: invalid or missing signature (WinVerifyTrust 0x{:08X})", path.display(), status as u32));
        }
        if name.is_empty() {
            return Err(format!("{}: signature valid, but its signer cannot be read", path.display()));
        }
        Ok(name)
    }
}

/// Checks that a file is validly signed by `expected` (exact signer name).
pub fn require_signer(path: &Path, expected: &str) -> Result<(), String> {
    let name = signer(path)?;
    if name == expected { Ok(()) } else { Err(format!("{} is signed by \"{name}\", not by \"{expected}\"", path.display())) }
}

#[repr(C)]
struct ShellExecuteInfo {
    size: u32,
    mask: u32,
    window: *mut c_void,
    verb: *const u16,
    file: *const u16,
    parameters: *const u16,
    directory: *const u16,
    show: i32,
    instance: *mut c_void,
    id_list: *mut c_void,
    class: *const u16,
    class_key: *mut c_void,
    hot_key: u32,
    icon: *mut c_void,
    process: *mut c_void,
}

/// Whether this process runs elevated (as administrator).
pub fn is_elevated() -> bool {
    unsafe {
        let mut token = std::ptr::null_mut();
        // 0x0008 = TOKEN_QUERY
        if OpenProcessToken(GetCurrentProcess(), 0x0008, &mut token) == 0 {
            return false;
        }
        let mut elevation = 0u32;
        let mut returned = 0u32;
        // 20 = TokenElevation
        let ok = GetTokenInformation(token, 20, (&mut elevation as *mut u32).cast(), 4, &mut returned) != 0;
        CloseHandle(token);
        ok && elevation != 0
    }
}

/// Why an elevated run did not happen.
#[derive(Debug, PartialEq, Eq)]
pub enum Elevation {
    /// The user said no to Windows' question.
    Refused,
    Failed(String),
}

/// Runs `program` with `parameters` as administrator (Windows asks the user), hidden, and waits
/// for it, calling `waiting` every half second meanwhile; returns its exit code.
pub fn run_elevated(program: &Path, parameters: &str, mut waiting: impl FnMut()) -> Result<u32, Elevation> {
    let (verb, file, parameters) = (wide("runas"), wide(&program.display().to_string()), wide(parameters));
    let mut info = ShellExecuteInfo {
        size: std::mem::size_of::<ShellExecuteInfo>() as u32,
        // SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC
        mask: 0x40 | 0x100,
        window: std::ptr::null_mut(),
        verb: verb.as_ptr(),
        file: file.as_ptr(),
        parameters: parameters.as_ptr(),
        directory: std::ptr::null(),
        // SW_HIDE
        show: 0,
        instance: std::ptr::null_mut(),
        id_list: std::ptr::null_mut(),
        class: std::ptr::null(),
        class_key: std::ptr::null_mut(),
        hot_key: 0,
        icon: std::ptr::null_mut(),
        process: std::ptr::null_mut(),
    };
    unsafe {
        if ShellExecuteExW(&mut info) == 0 {
            let error = GetLastError();
            return Err(if error == ERROR_CANCELLED { Elevation::Refused } else { Elevation::Failed(format!("Windows error {error}")) });
        }
        if info.process.is_null() {
            return Err(Elevation::Failed("no process handle".into()));
        }
        // WAIT_TIMEOUT
        while WaitForSingleObject(info.process, 500) == 0x102 {
            waiting();
        }
        let mut code = 0u32;
        GetExitCodeProcess(info.process, &mut code);
        CloseHandle(info.process);
        Ok(code)
    }
}

/// Opens a file, a folder or an address with its usual program.
pub fn open(target: &str) {
    unsafe {
        // SW_SHOWNORMAL
        ShellExecuteW(std::ptr::null_mut(), wide("open").as_ptr(), wide(target).as_ptr(), std::ptr::null(), std::ptr::null(), 1);
    }
}

/// Writes to the console of the command that started this program, if any (the setup is a
/// window program, which has none of its own).
pub fn attach_console() {
    unsafe {
        // ATTACH_PARENT_PROCESS
        AttachConsole(u32::MAX);
    }
}

/// FOLDERID_Programs: the user's Start menu programs.
pub const PROGRAMS: Guid = Guid { d1: 0xA77F_5D77, d2: 0x2E2B, d3: 0x44C3, d4: [0xA6, 0xA2, 0xAB, 0xA6, 0x01, 0x05, 0x4A, 0x51] };
/// FOLDERID_Desktop.
pub const DESKTOP: Guid = Guid { d1: 0xB4BF_CC3A, d2: 0xDB2C, d3: 0x424C, d4: [0xB0, 0x29, 0x7F, 0xE9, 0x9A, 0x87, 0xC6, 0x41] };

pub fn known_folder(id: &Guid) -> Option<PathBuf> {
    unsafe {
        let mut path = std::ptr::null_mut();
        let status = SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut path);
        let found = (status >= 0 && !path.is_null()).then(|| PathBuf::from(read_wide(path)));
        if !path.is_null() {
            CoTaskMemFree(path.cast());
        }
        found
    }
}

const CLSID_SHELL_LINK: Guid = Guid { d1: 0x0002_1401, d2: 0, d3: 0, d4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46] };
const IID_SHELL_LINK: Guid = Guid { d1: 0x0002_14F9, d2: 0, d3: 0, d4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46] };
const IID_PERSIST_FILE: Guid = Guid { d1: 0x0000_010B, d2: 0, d3: 0, d4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46] };

/// Writes a shortcut (`link`, a .lnk file) to `target`. COM must be usable on this thread in the
/// multithreaded apartment: call it from a worker thread, not from the window's.
pub fn create_shortcut(link: &Path, target: &Path, folder: &Path, icon: &Path, description: &str) -> Result<(), String> {
    let ole = Ole::load().map_err(|e| e.to_string())?;
    let shell_link = ole.create(&CLSID_SHELL_LINK, &IID_SHELL_LINK, "creating a shortcut").map_err(|e| e.to_string())?;
    unsafe {
        type SetText = unsafe extern "system" fn(*mut c_void, *const u16) -> i32;
        type SetIcon = unsafe extern "system" fn(*mut c_void, *const u16, i32) -> i32;
        let call = |slot: usize, text: &str, what: &'static str| -> Result<(), String> {
            let function: SetText = std::mem::transmute(shell_link.entry(slot));
            check(what, function(shell_link.0, wide(text).as_ptr())).map_err(|e| e.to_string())
        };
        // IShellLinkW: SetDescription 7, SetWorkingDirectory 9, SetIconLocation 17, SetPath 20.
        call(20, &target.display().to_string(), "IShellLinkW::SetPath")?;
        call(9, &folder.display().to_string(), "IShellLinkW::SetWorkingDirectory")?;
        call(7, description, "IShellLinkW::SetDescription")?;
        let set_icon: SetIcon = std::mem::transmute(shell_link.entry(17));
        check("IShellLinkW::SetIconLocation", set_icon(shell_link.0, wide(&icon.display().to_string()).as_ptr(), 0)).map_err(|e| e.to_string())?;
        let persist: Com = shell_link.query(&IID_PERSIST_FILE, "IPersistFile").map_err(|e| e.to_string())?;
        // IPersistFile::Save, slot 6.
        let save: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> i32 = std::mem::transmute(persist.entry(6));
        if let Some(parent) = link.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        check("IPersistFile::Save", save(persist.0, wide(&link.display().to_string()).as_ptr(), 1)).map_err(|e| e.to_string())
    }
}

/// A text value under HKEY_CURRENT_USER or HKEY_LOCAL_MACHINE.
pub fn set_text(root: isize, key: &str, value: &str, text: &str) -> Result<(), String> {
    let data = wide(text);
    let status = unsafe { RegSetKeyValueW(root, wide(key).as_ptr(), wide(value).as_ptr(), REG_SZ, data.as_ptr().cast(), (data.len() * 2) as u32) };
    if status == 0 { Ok(()) } else { Err(format!("cannot write {key}\\{value} (Windows error {status})")) }
}

pub fn set_dword(root: isize, key: &str, value: &str, number: u32) -> Result<(), String> {
    let status = unsafe { RegSetKeyValueW(root, wide(key).as_ptr(), wide(value).as_ptr(), REG_DWORD, (&number as *const u32).cast(), 4) };
    if status == 0 { Ok(()) } else { Err(format!("cannot write {key}\\{value} (Windows error {status})")) }
}

/// Deletes a key and everything under it; fine when it is already gone.
pub fn delete_tree(root: isize, key: &str) -> Result<(), String> {
    let status = unsafe { RegDeleteTreeW(root, wide(key).as_ptr()) };
    let gone = unsafe { RegDeleteKeyW(root, wide(key).as_ptr()) };
    // 2 = ERROR_FILE_NOT_FOUND
    if (status == 0 || status == 2) && (gone == 0 || gone == 2) { Ok(()) } else { Err(format!("cannot delete {key} (Windows error {status})")) }
}

/// Deletes a key only when nothing is left under it.
pub fn delete_if_empty(root: isize, key: &str) {
    unsafe {
        RegDeleteKeyW(root, wide(key).as_ptr());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_is_windows_own() {
        let file = std::env::temp_dir().join(format!("platinum-setup-sha-{}", std::process::id()));
        std::fs::write(&file, b"abc").unwrap();
        let digest = sha256_file(&file).unwrap();
        let _ = std::fs::remove_file(&file);
        assert_eq!(hex(&digest), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(hex(&sha256_bytes(b"abc").unwrap()), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn signers_are_read_and_unsigned_files_refused() {
        // GG's installer, when the workspace has it, is signed by GN Hearing A/S.
        if let Some(installer) = crate::tests::local_gg_installer() {
            assert_eq!(signer(&installer).unwrap(), crate::GG_SIGNER);
            assert!(require_signer(&installer, "Microsoft Corporation").is_err());
        }
        let unsigned = std::env::temp_dir().join(format!("platinum-setup-unsigned-{}.exe", std::process::id()));
        std::fs::write(&unsigned, b"MZ not a program").unwrap();
        assert!(signer(&unsigned).is_err());
        let _ = std::fs::remove_file(&unsigned);
    }

    #[test]
    fn known_folders_exist() {
        assert!(known_folder(&PROGRAMS).is_some_and(|p| p.ends_with("Programs")));
        assert!(known_folder(&DESKTOP).is_some());
    }
}
