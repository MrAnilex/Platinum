//! HTTPS downloads through Windows' own WinHTTP (no TLS crate): a HEAD request to see where a
//! link leads and how big the file is, and a GET that resumes a partial file (`Range`) and retries
//! after a network error. WinHTTP follows redirects (never from HTTPS to HTTP) and uses the
//! system's proxy settings.

use platinum::com::wide;
use std::ffi::c_void;
use std::io::{Seek, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[link(name = "winhttp")]
unsafe extern "system" {
    fn WinHttpOpen(agent: *const u16, access: u32, proxy: *const u16, bypass: *const u16, flags: u32) -> *mut c_void;
    fn WinHttpConnect(session: *mut c_void, server: *const u16, port: u16, reserved: u32) -> *mut c_void;
    fn WinHttpOpenRequest(connect: *mut c_void, verb: *const u16, object: *const u16, version: *const u16, referrer: *const u16, accept: *const *const u16, flags: u32) -> *mut c_void;
    fn WinHttpAddRequestHeaders(request: *mut c_void, headers: *const u16, length: u32, modifiers: u32) -> i32;
    fn WinHttpSendRequest(request: *mut c_void, headers: *const u16, headers_length: u32, optional: *const c_void, optional_length: u32, total: u32, context: usize) -> i32;
    fn WinHttpReceiveResponse(request: *mut c_void, reserved: *mut c_void) -> i32;
    fn WinHttpQueryHeaders(request: *mut c_void, info: u32, name: *const u16, buffer: *mut c_void, length: *mut u32, index: *mut u32) -> i32;
    fn WinHttpQueryOption(handle: *mut c_void, option: u32, buffer: *mut c_void, length: *mut u32) -> i32;
    fn WinHttpReadData(request: *mut c_void, buffer: *mut c_void, length: u32, read: *mut u32) -> i32;
    fn WinHttpSetTimeouts(handle: *mut c_void, resolve: i32, connect: i32, send: i32, receive: i32) -> i32;
    fn WinHttpCloseHandle(handle: *mut c_void) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetLastError() -> u32;
}

/// A generic name: no detail about the user or the machine goes to the servers.
const AGENT: &str = "Platinum-Setup/0.1";
/// WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY (Windows 8.1 and later).
const AUTOMATIC_PROXY: u32 = 4;
const WINHTTP_FLAG_SECURE: u32 = 0x0080_0000;
const WINHTTP_ADDREQ_FLAG_ADD: u32 = 0x2000_0000;
const WINHTTP_QUERY_STATUS_CODE: u32 = 19;
const WINHTTP_QUERY_CONTENT_LENGTH: u32 = 5;
const WINHTTP_QUERY_FLAG_NUMBER: u32 = 0x2000_0000;
const WINHTTP_OPTION_URL: u32 = 34;
/// Attempts of a download before giving up, each resuming where the last stopped.
const ATTEMPTS: u32 = 4;

/// A WinHTTP handle, closed on drop.
struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

fn last_error(what: &str) -> String {
    format!("{what} failed (WinHTTP error {})", unsafe { GetLastError() })
}

/// An HTTPS address cut into host, port and path; `None` for anything else.
pub fn split_url(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("https://")?;
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().ok()?),
        None => (authority, 443),
    };
    (!host.is_empty()).then(|| (host.to_string(), port, path.to_string()))
}

/// What a request answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    pub status: u32,
    pub length: Option<u64>,
    /// The address after redirects.
    pub url: String,
}

/// A request sent and its response headers received; the body can then be read.
struct Response {
    _session: Handle,
    _connect: Handle,
    request: Handle,
    answer: Answer,
}

fn send(verb: &str, url: &str, range_from: u64) -> Result<Response, String> {
    let (host, port, path) = split_url(url).ok_or_else(|| format!("not an HTTPS address: {url}"))?;
    unsafe {
        let session = Handle(WinHttpOpen(wide(AGENT).as_ptr(), AUTOMATIC_PROXY, std::ptr::null(), std::ptr::null(), 0));
        if session.0.is_null() {
            return Err(last_error("WinHttpOpen"));
        }
        WinHttpSetTimeouts(session.0, 30_000, 30_000, 30_000, 60_000);
        let connect = Handle(WinHttpConnect(session.0, wide(&host).as_ptr(), port, 0));
        if connect.0.is_null() {
            return Err(last_error(&format!("connecting to {host}")));
        }
        let request = Handle(WinHttpOpenRequest(connect.0, wide(verb).as_ptr(), wide(&path).as_ptr(), std::ptr::null(), std::ptr::null(), std::ptr::null(), WINHTTP_FLAG_SECURE));
        if request.0.is_null() {
            return Err(last_error("WinHttpOpenRequest"));
        }
        if range_from > 0 {
            let header = wide(&format!("Range: bytes={range_from}-"));
            WinHttpAddRequestHeaders(request.0, header.as_ptr(), u32::MAX, WINHTTP_ADDREQ_FLAG_ADD);
        }
        if WinHttpSendRequest(request.0, std::ptr::null(), 0, std::ptr::null(), 0, 0, 0) == 0 {
            return Err(last_error(&format!("sending the request to {host}")));
        }
        if WinHttpReceiveResponse(request.0, std::ptr::null_mut()) == 0 {
            return Err(last_error(&format!("receiving the answer of {host}")));
        }
        let mut status = 0u32;
        let mut size = 4u32;
        WinHttpQueryHeaders(request.0, WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER, std::ptr::null(), (&mut status as *mut u32).cast(), &mut size, std::ptr::null_mut());
        let mut text = [0u16; 32];
        let mut size = (text.len() * 2) as u32;
        let length = (WinHttpQueryHeaders(request.0, WINHTTP_QUERY_CONTENT_LENGTH, std::ptr::null(), text.as_mut_ptr().cast(), &mut size, std::ptr::null_mut()) != 0)
            .then(|| String::from_utf16_lossy(&text[..size as usize / 2]).trim().parse::<u64>().ok())
            .flatten();
        let mut buffer = vec![0u16; 2048];
        let mut size = (buffer.len() * 2) as u32;
        let final_url = if WinHttpQueryOption(request.0, WINHTTP_OPTION_URL, buffer.as_mut_ptr().cast(), &mut size) != 0 {
            String::from_utf16_lossy(&buffer[..size as usize / 2]).trim_end_matches('\0').to_string()
        } else {
            url.to_string()
        };
        Ok(Response { _session: session, _connect: connect, request, answer: Answer { status, length, url: final_url } })
    }
}

/// Where a link leads and how big the file is, without downloading it.
pub fn head(url: &str) -> Result<Answer, String> {
    send("HEAD", url, 0).map(|response| response.answer)
}

/// Downloads `url` into `target`, through `target` + ".part", which a later call resumes. Calls
/// `progress(bytes, total)` as it goes. `expected` is the size the file must have, when known.
/// Gives up after `ATTEMPTS` network errors in a row, or when `cancel` is set.
pub fn download(url: &str, target: &Path, expected: Option<u64>, progress: &mut dyn FnMut(u64, Option<u64>), cancel: &AtomicBool, log: &mut dyn FnMut(String)) -> Result<(), String> {
    let part = target.with_extension(format!("{}.part", target.extension().and_then(|e| e.to_str()).unwrap_or("")));
    if let Some(folder) = target.parent() {
        std::fs::create_dir_all(folder).map_err(|e| format!("cannot create {}: {e}", folder.display()))?;
    }
    let mut failures = 0;
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err("cancelled".into());
        }
        let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        if expected.is_some_and(|size| have == size) {
            break;
        }
        // More than expected: the file changed on the server, or the part is not this file.
        let resume_from = if expected.is_some_and(|size| have > size) { 0 } else { have };
        match fetch(url, &part, resume_from, expected, progress, cancel) {
            Ok(()) => break,
            Err(error) if error == "cancelled" => return Err(error),
            Err(error) => {
                failures += 1;
                log(format!("download attempt {failures} of {url} failed: {error}"));
                if failures >= ATTEMPTS {
                    return Err(error);
                }
                std::thread::sleep(Duration::from_secs(2u64 << failures.min(4)));
            }
        }
    }
    let size = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if let Some(wanted) = expected
        && size != wanted
    {
        return Err(format!("downloaded {size} bytes of {url}, {wanted} expected"));
    }
    let _ = std::fs::remove_file(target);
    std::fs::rename(&part, target).map_err(|e| format!("cannot rename {} to {}: {e}", part.display(), target.display()))
}

fn fetch(url: &str, part: &Path, from: u64, expected: Option<u64>, progress: &mut dyn FnMut(u64, Option<u64>), cancel: &AtomicBool) -> Result<(), String> {
    let response = send("GET", url, from)?;
    let (mut file, mut written) = match response.answer.status {
        206 => {
            let mut file = std::fs::OpenOptions::new().append(true).open(part).map_err(|e| format!("cannot open {}: {e}", part.display()))?;
            let at = file.seek(std::io::SeekFrom::End(0)).map_err(|e| e.to_string())?;
            (file, at)
        }
        // The whole file: the server ignored the range, or none was asked.
        200 => (std::fs::File::create(part).map_err(|e| format!("cannot create {}: {e}", part.display()))?, 0),
        // The part is complete already (or the file changed): start again from nothing.
        416 => {
            let _ = std::fs::remove_file(part);
            return Err("the server refused to resume; starting again".into());
        }
        status => return Err(format!("the server answered HTTP {status}")),
    };
    let total = expected.or_else(|| response.answer.length.map(|length| length + written));
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err("cancelled".into());
        }
        let mut read = 0u32;
        if unsafe { WinHttpReadData(response.request.0, buffer.as_mut_ptr().cast(), buffer.len() as u32, &mut read) } == 0 {
            return Err(last_error("reading the download"));
        }
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read as usize]).map_err(|e| format!("cannot write {}: {e}", part.display()))?;
        written += u64::from(read);
        progress(written, total);
    }
    file.flush().map_err(|e| e.to_string())?;
    match total {
        Some(total) if written < total => Err(format!("the connection ended after {written} of {total} bytes")),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_https_addresses_are_split() {
        assert_eq!(split_url("https://engine.steelseriescdn.com/SteelSeriesGG123.0.0Setup.exe"), Some(("engine.steelseriescdn.com".into(), 443, "/SteelSeriesGG123.0.0Setup.exe".into())));
        assert_eq!(split_url("https://example.test:8443"), Some(("example.test".into(), 8443, "/".into())));
        assert_eq!(split_url("https://aka.ms/vs/17/release/vc_redist.x64.exe?x=1").map(|s| s.2), Some("/vs/17/release/vc_redist.x64.exe?x=1".into()));
        assert_eq!(split_url("http://steelseries.com/"), None);
        assert_eq!(split_url("https://:443/"), None);
    }
}
