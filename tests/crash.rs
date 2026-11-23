//! The crash reports, end to end: a child process (this test program, run again for one test)
//! installs the hooks, then panics or raises an access violation; the report it leaves is read
//! back. Everything happens under a temporary profile folder (`USERPROFILE`), never the user's,
//! and no audio device is opened.

use platinum::crash;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Set for the child: what it does ("panic" or "fault").
const CHILD: &str = "PLATINUM_CRASH_CHILD";

#[link(name = "kernel32")]
unsafe extern "system" {
    fn RaiseException(code: u32, flags: u32, count: u32, arguments: *const usize);
}

/// The child's part: nothing unless `CHILD` is set.
#[test]
fn crash_child() {
    let Ok(kind) = std::env::var(CHILD) else { return };
    let profile = PathBuf::from(std::env::var_os("USERPROFILE").unwrap());
    let log = profile.join("Platinum").join("daemon.log");
    crash::install_with_log(crash::DAEMON, Some(log));
    match kind.as_str() {
        "panic" => panic!("planned panic for the crash report test"),
        // An access violation raised as Windows raises one, without touching invalid memory.
        _ => unsafe { RaiseException(0xC000_0005, 0, 0, std::ptr::null()) },
    }
}

fn run_child(kind: &str, profile: &Path) -> Option<i32> {
    let _ = std::fs::remove_dir_all(profile);
    let folder = profile.join("Platinum");
    std::fs::create_dir_all(&folder).unwrap();
    let log: String = (1..=80).map(|i| format!("[20261122-1530{:02}] line {i} of C:\\Users\\Someone\\Platinum\n", i % 60)).collect();
    std::fs::write(folder.join("daemon.log"), log).unwrap();
    spawn_child(kind, profile)
}

fn spawn_child(kind: &str, profile: &Path) -> Option<i32> {
    let status = Command::new(std::env::current_exe().unwrap()).args(["crash_child", "--exact", "--test-threads=1", "--nocapture"]).env(CHILD, kind).env("USERPROFILE", profile).output().unwrap();
    status.status.code()
}

#[test]
fn a_panic_leaves_a_report_with_its_message_stack_and_log() {
    let profile = std::env::temp_dir().join(format!("platinum-crash-panic-{}", std::process::id()));
    run_child("panic", &profile);
    let reports = crash::list_in(&profile.join("Platinum").join("crashes"));
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].program, "daemon");
    assert!(reports[0].reason.starts_with("panic: planned panic for the crash report test (at "), "{}", reports[0].reason);
    let text = crash::read(&reports[0].file).unwrap();
    assert!(text.contains("\nversion: ") && text.contains("\nwindows: 10.0.") && text.contains("\nthread: "), "{text}");
    assert!(text.contains("\nstack (module+offset):\n  "), "{text}");
    // The log's last 50 lines, whole.
    assert!(text.contains("\ndaemon.log, last 50 lines:\n  [20261122-153031] line 31 of") && text.trim_end().ends_with("line 80 of C:\\Users\\Someone\\Platinum"), "{text}");
    assert!(!text.contains("line 30 of"), "{text}");
    let _ = std::fs::remove_dir_all(&profile);
}

#[test]
fn a_native_crash_leaves_a_report_and_a_minidump_and_ends_with_its_code() {
    let profile = std::env::temp_dir().join(format!("platinum-crash-fault-{}", std::process::id()));
    let code = run_child("fault", &profile);
    assert_eq!(code.map(|c| c as u32), Some(0xC000_0005));
    let folder = profile.join("Platinum").join("crashes");
    let reports = crash::list_in(&folder);
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(reports[0].reason.starts_with("access violation (0xC0000005) in ") && reports[0].reason.to_ascii_lowercase().contains(".dll+0x"), "{}", reports[0].reason);
    let text = crash::read(&reports[0].file).unwrap();
    assert!(text.contains("\nstack (module+offset):\n  ") && text.contains("\ndaemon.log, last 50 lines:\n  [20261122-153031] line 31 of"), "{text}");
    assert!(text.contains("\nminidump:\n  ") && text.contains(".dmp, kept on this PC only"), "{text}");
    let dump = reports[0].file.with_extension("dmp");
    let bytes = std::fs::read(&dump).unwrap();
    assert!(bytes.starts_with(b"MDMP"), "{}", dump.display());
    // The paths in the report are file names only: no folder (which would name the user).
    assert!(!text.contains(&profile.display().to_string()), "{text}");
    let _ = std::fs::remove_dir_all(&profile);
}

#[test]
fn a_native_crash_before_platinum_is_installed_still_leaves_its_report() {
    // A setup that crashes on a fresh PC: no Platinum folder, no log yet.
    let profile = std::env::temp_dir().join(format!("platinum-crash-fresh-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&profile);
    std::fs::create_dir_all(&profile).unwrap();
    assert_eq!(spawn_child("fault", &profile).map(|c| c as u32), Some(0xC000_0005));
    let reports = crash::list_in(&profile.join("Platinum").join("crashes"));
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(!crash::read(&reports[0].file).unwrap().contains("daemon.log"));
    let _ = std::fs::remove_dir_all(&profile);
}
