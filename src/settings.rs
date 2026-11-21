//! Registry-backed settings: start-with-Windows and the taskbar theme (plus the Windows
//! version, for diagnostics).

use crate::wide;
use std::ffi::c_void;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::*;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APP_KEY: &str = r"Software\taskbar-battery";
const RUN_VALUE: &str = "TaskbarBattery";
/// Windows' own on/off switch for each Run entry (Task Manager > Startup apps, Settings >
/// Apps > Startup). A 12-byte binary value: an odd first byte means disabled, and Windows
/// then skips the Run entry at logon even though it's there.
const STARTUP_APPROVED_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
const APPROVED_ON: [u8; 12] = [0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

pub fn light_theme() -> bool {
    read_dword(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize", "SystemUsesLightTheme") == Some(1)
}

/// Whether Windows will actually start the app at logon: the Run entry is there and not
/// turned off in Windows' Startup apps.
pub fn autostart_enabled() -> bool {
    run_entry_present() && !startup_disabled_by_windows()
}

fn run_entry_present() -> bool {
    let (k, v) = (wide(RUN_KEY), wide(RUN_VALUE));
    let mut size = 0u32;
    unsafe {
        RegGetValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr(), RRF_RT_REG_SZ, null_mut(), null_mut(), &mut size)
            == ERROR_SUCCESS
    }
}

/// Turned off in Task Manager > Startup apps (or Settings > Apps > Startup).
fn startup_disabled_by_windows() -> bool {
    read_binary(STARTUP_APPROVED_KEY, RUN_VALUE).is_some_and(|b| is_disabled(&b))
}

fn is_disabled(approved: &[u8]) -> bool {
    approved.first().is_some_and(|b| b & 1 == 1)
}

/// For diagnostics: why the app would or wouldn't start with Windows.
pub fn autostart_status() -> &'static str {
    match (run_entry_present(), startup_disabled_by_windows()) {
        (false, _) => "off (no Run entry)",
        (true, true) => "off (turned off in Windows' Startup apps)",
        (true, false) => "on",
    }
}

/// Adds (with the current exe path) or removes the Run entry.
fn write_run_entry(on: bool) {
    let (k, v) = (wide(RUN_KEY), wide(RUN_VALUE));
    unsafe {
        if on {
            let Ok(exe) = std::env::current_exe() else { return };
            let data = wide(&format!("\"{}\"", exe.display()));
            RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr(), REG_SZ, data.as_ptr() as *const c_void, (data.len() * 2) as u32);
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr());
        }
    }
}

/// From the menu: also flips Windows' Startup apps switch, so checking "Start with Windows"
/// undoes a "Disable" from Task Manager, and remembers the choice.
pub fn set_autostart(on: bool) {
    write_run_entry(on);
    let (k, v) = (wide(STARTUP_APPROVED_KEY), wide(RUN_VALUE));
    unsafe {
        if on {
            RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr(), REG_BINARY, APPROVED_ON.as_ptr() as *const c_void, APPROVED_ON.len() as u32);
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr());
        }
    }
    write_dword(APP_KEY, "AutostartOptOut", u32::from(!on));
}

/// Release builds register themselves to start with Windows (and refresh the path if the
/// exe moved), unless the user turned it off from the menu. A "Disable" in Windows' Startup
/// apps is left alone; the menu shows it unchecked and can turn it back on.
pub fn ensure_autostart() {
    if cfg!(debug_assertions) || read_dword(APP_KEY, "AutostartOptOut") == Some(1) {
        return;
    }
    write_run_entry(true);
}

pub fn alerts_enabled() -> bool {
    read_dword(APP_KEY, "LowBatteryAlerts") != Some(0)
}

pub fn set_alerts(on: bool) {
    write_dword(APP_KEY, "LowBatteryAlerts", u32::from(on));
}

const NOTIFICATION_ICON: &[u8] = include_bytes!("../assets/icon-64.png");

/// Registers the AppUserModelID so Windows accepts our toast notifications, with our icon
/// (written to %LOCALAPPDATA%\taskbar-battery\icon.png, since Windows needs a file).
pub fn register_app_id() {
    let key = format!(r"Software\Classes\AppUserModelId\{}", crate::notify::APP_ID);
    write_string(&key, "DisplayName", "Taskbar Battery");
    if let Some(dir) = std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("taskbar-battery")) {
        let icon = dir.join("icon.png");
        if std::fs::create_dir_all(&dir).is_ok() && std::fs::write(&icon, NOTIFICATION_ICON).is_ok() {
            write_string(&key, "IconUri", &icon.to_string_lossy());
        }
    }
}

fn write_string(key: &str, name: &str, value: &str) {
    let (k, n, data) = (wide(key), wide(name), wide(value));
    unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), REG_SZ, data.as_ptr() as *const c_void, (data.len() * 2) as u32);
    }
}

/// Device ids the user unchecked in the menu.
pub fn hidden_devices() -> Vec<String> {
    read_string(APP_KEY, "Hidden").map(|s| s.lines().map(str::to_string).collect()).unwrap_or_default()
}

pub fn toggle_hidden(id: &str) {
    let mut hidden = hidden_devices();
    match hidden.iter().position(|h| h == id) {
        Some(i) => {
            hidden.remove(i);
        }
        None => hidden.push(id.to_string()),
    }
    let (k, n, data) = (wide(APP_KEY), wide("Hidden"), wide(&hidden.join("\n")));
    unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), REG_SZ, data.as_ptr() as *const c_void, (data.len() * 2) as u32);
    }
}

/// E.g. "Windows 11 24H2 build 26100.2033", for diagnostics.
pub fn windows_version() -> String {
    const KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let build: u32 = read_string_in(HKEY_LOCAL_MACHINE, KEY, "CurrentBuild").and_then(|b| b.parse().ok()).unwrap_or(0);
    let ubr = read_dword_in(HKEY_LOCAL_MACHINE, KEY, "UBR").unwrap_or(0);
    let release = read_string_in(HKEY_LOCAL_MACHINE, KEY, "DisplayVersion").unwrap_or_default();
    // ProductName still says "Windows 10" on Windows 11; the build tells them apart.
    let name = if build >= 22000 { "Windows 11" } else { "Windows 10" };
    format!("{name} {release} build {build}.{ubr}")
}

fn read_string(key: &str, name: &str) -> Option<String> {
    read_string_in(HKEY_CURRENT_USER, key, name)
}

fn read_string_in(root: HKEY, key: &str, name: &str) -> Option<String> {
    let (k, n) = (wide(key), wide(name));
    let mut size = 0u32;
    unsafe {
        if RegGetValueW(root, k.as_ptr(), n.as_ptr(), RRF_RT_REG_SZ, null_mut(), null_mut(), &mut size) != ERROR_SUCCESS {
            return None;
        }
        let mut buf = vec![0u16; (size as usize).div_ceil(2)];
        if RegGetValueW(root, k.as_ptr(), n.as_ptr(), RRF_RT_REG_SZ, null_mut(), buf.as_mut_ptr() as *mut c_void, &mut size) != ERROR_SUCCESS {
            return None;
        }
        let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }
}

fn read_binary(key: &str, name: &str) -> Option<Vec<u8>> {
    let (k, n) = (wide(key), wide(name));
    let mut buf = [0u8; 64];
    let mut size = buf.len() as u32;
    let r = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), RRF_RT_REG_BINARY, null_mut(), buf.as_mut_ptr() as *mut c_void, &mut size)
    };
    (r == ERROR_SUCCESS).then(|| buf[..size as usize].to_vec())
}

fn read_dword(key: &str, name: &str) -> Option<u32> {
    read_dword_in(HKEY_CURRENT_USER, key, name)
}

fn read_dword_in(root: HKEY, key: &str, name: &str) -> Option<u32> {
    let (k, n) = (wide(key), wide(name));
    let (mut value, mut size) = (0u32, 4u32);
    let r = unsafe {
        RegGetValueW(
            root,
            k.as_ptr(),
            n.as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            &mut value as *mut u32 as *mut c_void,
            &mut size,
        )
    };
    (r == ERROR_SUCCESS).then_some(value)
}

fn write_dword(key: &str, name: &str, value: u32) {
    let (k, n) = (wide(key), wide(name));
    unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), REG_DWORD, &value as *const u32 as *const c_void, 4);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_approved_flag() {
        assert!(!is_disabled(&APPROVED_ON));
        assert!(!is_disabled(&[0x06, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
        assert!(is_disabled(&[0x03, 0, 0, 0, 0x8a, 0x3c, 0x1d, 0x5e, 0x2f, 0x5b, 0xdb, 0x01]));
        assert!(is_disabled(&[0x07, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
        assert!(!is_disabled(&[]));
    }
}
