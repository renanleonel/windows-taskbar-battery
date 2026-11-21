//! Support diagnostics: a small log of what each device source saw, so users can attach it to
//! a device report. Written to %LOCALAPPDATA%\taskbar-battery\diagnostics.log and copied
//! from the right-click menu.
//!
//! Privacy: it holds hardware ids (USB VID/PID, HID++ feature and error codes, Bluetooth
//! appearance and class of device), vendor product names, Windows error messages and the app
//! and Windows versions. Never Bluetooth addresses or Bluetooth names (users rename those),
//! serial numbers or file paths. A Bluetooth device is tagged with a 16-bit hash of its
//! address (`bt#3f2a`), enough to match up lines about it but not to recover the address.
//!
//! Sources scan every few seconds, so an event is only written when it differs from the last
//! one written for the same key.

use std::collections::HashMap;
use std::fmt::Display;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

/// Past this, the log moves to diagnostics.old.log and a new one starts.
const MAX_LEN: u64 = 256 * 1024;
/// How much "Copy diagnostics" takes from the end of the log (GitHub issue fields hold ~65k chars).
const COPY_LEN: usize = 32 * 1024;

static LAST: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("taskbar-battery").join("diagnostics.log"))
}

pub fn header() -> String {
    format!("taskbar-battery {} | {} | {}", env!("CARGO_PKG_VERSION"), crate::settings::windows_version(), std::env::consts::ARCH)
}

/// Where the log lives, for the crash message box (paths never go in the log itself).
pub fn log_path() -> Option<PathBuf> {
    path()
}

/// Marks the start of a session in the log.
pub fn start() {
    write(&format!("--- {}", header()));
}

/// Records `msg` for `key` (e.g. "logi:c539:2:01"), unless it's what was last recorded for it.
/// Each source should record one event per key per scan, or alternating messages would
/// defeat the deduplication.
pub fn event(key: &str, msg: impl Display) {
    let msg = msg.to_string();
    {
        let mut last = LAST.lock().unwrap();
        let last = last.get_or_insert_with(HashMap::new);
        if last.get(key) == Some(&msg) {
            return;
        }
        last.insert(key.to_string(), msg.clone());
    }
    write(&format!("[{key}] {msg}"));
}

/// Records a crash. Called from the panic hook, so it skips the dedup map (whose lock the
/// panicking thread may hold).
pub fn crash(msg: impl Display) {
    write(&format!("[crash] {msg}"));
}

fn write(line: &str) {
    if cfg!(debug_assertions) {
        eprintln!("{line}");
    }
    let Some(path) = path() else { return };
    if fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LEN) {
        let _ = fs::rename(&path, path.with_extension("old.log"));
    }
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{} {line}", timestamp());
    }
}

fn timestamp() -> String {
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

/// The end of the log, headed by the current versions, for pasting into a report.
/// None if nothing has been logged yet.
pub fn report() -> Option<String> {
    let text = fs::read_to_string(path()?).ok()?;
    Some(format!("{}\n\n{}", header(), tail(&text, COPY_LEN)))
}

/// The last `max` bytes of `text`, starting at a line boundary.
fn tail(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    match text[start..].find('\n') {
        Some(i) => &text[start + i + 1..],
        None => &text[start..],
    }
}

/// Short tag for a Bluetooth address: FNV-1a folded to 16 bits, so ~2^32 addresses share
/// each tag and it can't be turned back into the address.
pub fn bt_tag(addr: u64) -> String {
    let mut h: u32 = 0x811C_9DC5;
    for b in addr.to_le_bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{:04x}", (h ^ (h >> 16)) & 0xFFFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_starts_at_a_line() {
        assert_eq!(tail("aaa\nbbb\nccc\n", 100), "aaa\nbbb\nccc\n");
        assert_eq!(tail("aaa\nbbb\nccc\n", 6), "ccc\n");
        assert_eq!(tail("ééé\nccc\n", 7), "ccc\n"); // cut lands inside a multi-byte char
    }

    #[test]
    fn bt_tag_is_short_and_stable() {
        assert_eq!(bt_tag(0xF4EE25FBBA46), bt_tag(0xF4EE25FBBA46));
        assert_ne!(bt_tag(0xF4EE25FBBA46), bt_tag(0xF4EE25FBBA47));
        assert_eq!(bt_tag(0xF4EE25FBBA46).len(), 4);
    }
}
