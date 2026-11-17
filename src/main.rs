#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bluetooth;
mod delux;
mod device;
mod diag;
mod headsetcontrol;
mod hyperx;
mod logitech;
mod notify;
mod playstation;
mod render;
mod settings;
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

fn main() {
    match std::env::args().nth(1).as_deref() {
        // Debug helpers (they print only in debug builds; release builds have no console).
        Some("--test-toast") => {
            init_com();
            settings::register_app_id();
            if let Err(e) = notify::show("Low battery", "G Pro Wireless is at 12%. Time to charge it.") {
                eprintln!("toast failed: {e}");
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
            let _ = xbox::scan(); // first call only starts controller enumeration
            std::thread::sleep(std::time::Duration::from_millis(2500));
            obs.extend(delux.scan(&api));
            obs.extend(xbox::scan());
            obs.extend(bluetooth::scan());
            obs.extend(hyperx::scan(&api));
            match headsetcontrol::find() {
                Some(exe) => {
                    eprintln!("[headsetcontrol] using {}", exe.display());
                    let hc = headsetcontrol::scan(&exe, |pid| logitech::reports_pid(&obs, pid));
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
