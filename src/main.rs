#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bluetooth;
mod corsair;
mod corsair_slipstream;
mod delux;
mod device;
mod diag;
mod headsetcontrol;
mod hid_battery;
mod logitech;
mod mchose;
mod mchose_mouse;
mod notify;
mod playstation;
mod probe;
mod rapoo;
mod razer;
mod render;
mod settings;
mod steelseries;
mod vxe;
mod widget;
mod xbox;

pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Initializes COM (multithreaded) on the calling thread, needed for the Bluetooth APIs.
pub(crate) fn init_com() {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
    }
}

/// The release build has no console and aborts on panic, so a crash would otherwise close
/// the app without a trace. Log it, and tell the user where the log is.
fn install_crash_handler() {
    static IN_HOOK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    std::panic::set_hook(Box::new(|info| {
        // The message box pumps messages; a second panic while it's up just aborts.
        if IN_HOOK.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let what = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown error".into());
        // Our own sources show as relative paths (src\widget.rs); a dependency's is absolute
        // (the builder's cargo registry), and the log holds no paths, so drop those.
        let at = match info.location() {
            Some(l) if !std::path::Path::new(l.file()).is_absolute() => format!(" at {}:{}", l.file(), l.line()),
            _ => String::new(),
        };
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        let msg = format!("{what}{at} (thread {thread})");
        diag::crash(&msg);
        if cfg!(debug_assertions) {
            eprintln!("{msg}");
            return;
        }
        let log = diag::path().map(|p| p.display().to_string()).unwrap_or_else(|| "diagnostics.log".into());
        let text = wide(&format!(
            "Taskbar Battery stopped because of an error:

{msg}

Please open an issue on GitHub and attach this file:
{log}"
        ));
        let title = wide("Taskbar Battery");
        unsafe {
            use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND};
            MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR | MB_SETFOREGROUND);
        }
    }));
}

fn main() {
    install_crash_handler();
    match std::env::args().nth(1).as_deref() {
        // Debug helpers (they print only in debug builds; release builds have no console).
        Some("--test-toast") => {
            init_com();
            settings::register_app_id();
            if let Err(e) = notify::show("Low battery", "G Pro Wireless is at 12%. Time to charge it.") {
                eprintln!("toast failed: {e}");
            }
        }
        Some("--probe") => {
            let secs = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(probe::LISTEN.as_secs());
            match probe::run_to_file(std::time::Duration::from_secs(secs)) {
                Ok(path) => println!("{}", std::fs::read_to_string(&path).unwrap_or_default()),
                Err(e) => eprintln!("probe failed: {e}"),
            }
        }
        Some("--list") => logitech::hidpp::list(&hidapi::HidApi::new().expect("hidapi")),
        Some("--once") => {
            init_com();
            diag::start();
            let api = hidapi::HidApi::new().expect("hidapi");
            let mut obs = logitech::Logitech::new().scan(&api);
            obs.extend(playstation::scan(&api));
            let mut delux = delux::Delux::default();
            let _ = delux.scan(&api); // opens the dongle; its status report comes every ~2 s
            let mut rapoo = rapoo::Rapoo::default();
            let _ = rapoo.scan(&api); // opens the dongle; its status report comes every ~5 s
            let _ = xbox::scan(); // first call only starts controller enumeration
            std::thread::sleep(std::time::Duration::from_millis(6000));
            obs.extend(delux.scan(&api));
            obs.extend(rapoo.scan(&api));
            obs.extend(mchose::Mchose::default().scan(&api));
            obs.extend(mchose_mouse::MchoseMice::default().scan(&api));
            obs.extend(corsair::Corsair::default().scan(&api));
            obs.extend(corsair_slipstream::CorsairSlipstream::default().scan(&api));
            obs.extend(razer::Razer::default().scan(&api));
            obs.extend(steelseries::SteelSeries::default().scan(&api));
            obs.extend(vxe::Vxe::default().scan(&api));
            obs.extend(hid_battery::HidBattery::default().scan(&api));
            obs.extend(xbox::scan());
            obs.extend(bluetooth::scan());
            match headsetcontrol::find() {
                Some(exe) => {
                    eprintln!("[headsetcontrol] using {}", exe.display());
                    let hc = headsetcontrol::scan(&exe, |pid| device::read_natively(&obs, pid));
                    obs.extend(hc);
                }
                None => eprintln!("[headsetcontrol] not found (optional)"),
            }
            for r in device::Tracker::default().update(obs) {
                println!("{:<10} {:<36} {}  [{}]", format!("{:?}", r.kind), r.name, r.status, r.id);
            }
        }
        Some("--demo") if cfg!(debug_assertions) => {
            widget::DEMO.store(true, std::sync::atomic::Ordering::Relaxed);
            widget::run();
        }
        _ => widget::run(),
    }
}
