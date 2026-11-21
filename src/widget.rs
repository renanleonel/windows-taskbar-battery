//! Window plumbing.
//!
//! - A hidden top-level "controller" window receives broadcasts (TaskbarCreated after an
//!   Explorer restart, theme changes, device changes), owns the timer and the context menu.
//! - The visible "widget" is a layered child window of the taskbar (Shell_TrayWnd), placed
//!   just left of the notification area. It dies with Explorer and gets recreated.
//! - A worker thread scans devices and posts WM_APP_UPDATE to the controller.

use crate::device::{Reading, Status, Tracker};
use crate::{bluetooth, corsair, corsair_slipstream, delux, device, diag, headsetcontrol, hid_battery, init_com, logitech, mchose, mchose_mouse, notify, playstation, probe, rapoo, razer, render, settings, steelseries, vxe, wide, xbox};
use std::cell::RefCell;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, SetProcessWorkingSetSize};
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_APP_UPDATE: u32 = WM_APP + 1;
const WM_APP_MENU: u32 = WM_APP + 2;
const TIMER_PLACE: usize = 1;
const CMD_REFRESH: usize = 1;
const CMD_AUTOSTART: usize = 2;
const CMD_EXIT: usize = 3;
const CMD_ALERTS: usize = 4;
const CMD_COPY_DIAGNOSTICS: usize = 5;
const CMD_PROBE: usize = 6;
/// WM_POWERBROADCAST: resumed from sleep/hibernate.
const PBT_APMRESUMEAUTOMATIC: usize = 0x12;
const CMD_DEVICE_BASE: usize = 100;

/// HID++ is cheap to poll. Bluetooth reads wake the device's radio and HeadsetControl spawns
/// a process, so poll those less often.
const POLL_LOGITECH: Duration = Duration::from_secs(15);
const POLL_BLUETOOTH: Duration = Duration::from_secs(60);

static READINGS: Mutex<Option<Vec<Reading>>> = Mutex::new(None);
static WAKE: OnceLock<Sender<()>> = OnceLock::new();

struct App {
    controller: HWND,
    widget: HWND,
    tooltip: HWND,
    taskbar_created: u32,
    light: bool,
    /// Rendered widget size; (0, 0) when there is nothing to show.
    size: (i32, i32),
    /// Taskbar layout the current bitmap was rendered for.
    layout: Option<render::Layout>,
}

thread_local! {
    static APP: RefCell<App> = const {
        RefCell::new(App {
            controller: null_mut(),
            widget: null_mut(),
            tooltip: null_mut(),
            taskbar_created: 0,
            light: false,
            size: (0, 0),
            layout: None,
        })
    };
}

/// Short borrows only; never hold one across a Win32 call that can dispatch messages.
fn with<R>(f: impl FnOnce(&mut App) -> R) -> R {
    APP.with(|a| f(&mut a.borrow_mut()))
}

pub fn run() {
    unsafe {
        let mutex_name = wide(r"Local\taskbar-battery");
        CreateMutexW(null(), 0, mutex_name.as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // The widget stays hidden until a device is found, so say where it is.
            init_com();
            let _ = notify::show(
                "Taskbar Battery is already running",
                "Its battery levels show on the taskbar, left of the clock, once it finds a supported device.",
            );
            return;
        }
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        settings::ensure_autostart();
        settings::register_app_id();
        diag::start();
        diag::event("autostart", settings::autostart());
        if !settings::welcomed() {
            // Nothing shows on the taskbar until a device is found; confirm it started.
            std::thread::spawn(|| {
                init_com();
                match notify::show(
                    "Taskbar Battery is running",
                    "Battery levels show on the taskbar, left of the clock. Right-click them for options. Nothing shows until a supported device is found.",
                ) {
                    Ok(()) => settings::set_welcomed(),
                    Err(e) => diag::event("notify", format!("first-run toast failed: {e}")),
                }
            });
        }

        let hinst = GetModuleHandleW(null());
        let ctrl_class = wide("TaskbarBatteryController");
        register(hinst, &ctrl_class, controller_proc);
        register(hinst, &wide("TaskbarBatteryWidget"), widget_proc);

        let taskbar_created = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
        let controller = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            ctrl_class.as_ptr(),
            ctrl_class.as_ptr(),
            WS_POPUP,
            0, 0, 0, 0,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        with(|a| {
            a.controller = controller;
            a.taskbar_created = taskbar_created;
            a.light = settings::light_theme();
        });

        let (tx, rx) = mpsc::channel();
        let _ = WAKE.set(tx);
        let controller_addr = controller as usize;
        std::thread::spawn(move || worker(controller_addr, rx));

        ensure_widget();
        SetTimer(controller, TIMER_PLACE, 1000, None);

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe fn register(hinst: HINSTANCE, class: &[u16], proc_: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT) {
    let wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(proc_),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinst,
        hIcon: null_mut(),
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        hbrBackground: null_mut(),
        lpszMenuName: null(),
        lpszClassName: class.as_ptr(),
    };
    RegisterClassW(&wc);
}

/// Debug builds: `--demo` shows fixed fake readings covering every visual state.
pub static DEMO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn demo_readings() -> Vec<Reading> {
    use crate::device::Kind;
    let r = |name: &str, kind, percent, charging, stale| Reading {
        id: name.into(),
        name: name.into(),
        kind,
        status: Status::Battery { percent, charging, stale },
    };
    vec![
        r("Headset", Kind::Headset, 64, true, false),
        r("Earbuds", Kind::Earbuds, 12, false, false),
        r("Mouse", Kind::Mouse, 87, false, true),
        r("Keyboard", Kind::Keyboard, 45, false, false),
    ]
}

fn worker(controller: usize, rx: Receiver<()>) {
    if DEMO.load(std::sync::atomic::Ordering::Relaxed) {
        *READINGS.lock().unwrap() = Some(demo_readings());
        unsafe { PostMessageW(controller as HWND, WM_APP_UPDATE, 0, 0) };
        return;
    }
    init_com();
    let mut api = match hidapi::HidApi::new() {
        Ok(api) => api,
        Err(e) => {
            diag::event("hid", format!("hidapi failed to start: {e}"));
            return;
        }
    };
    let mut logi = logitech::Logitech::new();
    let mut delux = delux::Delux::default();
    let mut mchose = mchose::Mchose::default();
    let mut mchose_mice = mchose_mouse::MchoseMice::default();
    let mut corsair = corsair::Corsair::default();
    let mut corsair_slipstream = corsair_slipstream::CorsairSlipstream::default();
    let mut hid_battery = hid_battery::HidBattery::default();
    let mut rapoo = rapoo::Rapoo::default();
    let mut razer = razer::Razer::default();
    let mut steelseries = steelseries::SteelSeries::default();
    let mut vxe = vxe::Vxe::default();
    let mut tracker = Tracker::default();
    let mut alerts = notify::LowBatteryAlerts::default();
    let mut bt = Vec::new();
    let mut last_bt: Option<Instant> = None;
    let mut forced = true;

    loop {
        let _ = api.refresh_devices();
        let mut obs = logi.scan(&api);
        obs.extend(playstation::scan(&api));
        obs.extend(delux.scan(&api));
        obs.extend(mchose.scan(&api));
        obs.extend(mchose_mice.scan(&api));
        obs.extend(corsair.scan(&api));
        obs.extend(rapoo.scan(&api));
        obs.extend(razer.scan(&api));
        obs.extend(steelseries.scan(&api));
        obs.extend(vxe.scan(&api));
        obs.extend(corsair_slipstream.scan(&api));
        obs.extend(hid_battery.scan(&api));
        obs.extend(xbox::scan());
        if forced || last_bt.is_none_or(|t| t.elapsed() >= POLL_BLUETOOTH) {
            bt = bluetooth::scan();
            if let Some(exe) = headsetcontrol::find() {
                bt.extend(headsetcontrol::scan(&exe, |pid| device::read_natively(&obs, pid)));
            }
            last_bt = Some(Instant::now());
        }
        // Re-feed the last Bluetooth results between Bluetooth scans so they don't look gone.
        obs.extend(bt.iter().cloned());
        let readings = tracker.update(obs);
        alerts.check(&readings);
        *READINGS.lock().unwrap() = Some(readings);
        unsafe { PostMessageW(controller as HWND, WM_APP_UPDATE, 0, 0) };
        trim_working_set();

        forced = match rx.recv_timeout(POLL_LOGITECH) {
            Ok(()) => {
                // Device changes come in bursts; let things settle, then scan once.
                std::thread::sleep(Duration::from_millis(1500));
                while rx.try_recv().is_ok() {}
                true
            }
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => return,
        };
    }
}

/// The app is idle between scans, so hand its pages back to Windows until the next one.
/// They move to the standby list and come back cheaply when touched.
fn trim_working_set() {
    unsafe { SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX) };
}

fn wake() {
    if let Some(tx) = WAKE.get() {
        let _ = tx.send(());
    }
}

fn readings() -> Vec<Reading> {
    READINGS.lock().unwrap().clone().unwrap_or_default()
}

unsafe fn find_tray() -> HWND {
    FindWindowW(wide("Shell_TrayWnd").as_ptr(), null())
}

unsafe fn ensure_widget() {
    let widget = with(|a| a.widget);
    if !widget.is_null() && IsWindow(widget) != 0 {
        return;
    }
    let tray = find_tray();
    if tray.is_null() {
        return; // Explorer not up yet; the timer retries.
    }
    let class = wide("TaskbarBatteryWidget");
    let w = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        class.as_ptr(),
        null(),
        WS_CHILD | WS_CLIPSIBLINGS,
        0, 0, 1, 1,
        tray,
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    );
    let (controller, old_tip) = with(|a| (a.controller, a.tooltip));
    if !old_tip.is_null() {
        DestroyWindow(old_tip); // its tool was the previous (dead) widget
    }
    let tip = create_tooltip(controller, w);
    with(|a| {
        a.widget = w;
        a.tooltip = tip;
        a.size = (0, 0);
        a.layout = None;
    });
    redraw();
}

/// Hover tooltip listing each device's full name and status.
unsafe fn create_tooltip(owner: HWND, widget: HWND) -> HWND {
    let icc = INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_WIN95_CLASSES };
    InitCommonControlsEx(&icc);
    let tip = CreateWindowExW(
        WS_EX_TOPMOST,
        wide("tooltips_class32").as_ptr(),
        null(),
        WS_POPUP | TTS_NOPREFIX | TTS_ALWAYSTIP,
        CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT,
        owner,
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    );
    if tip.is_null() {
        return tip;
    }
    let mut text = wide("");
    let mut ti = tool_info(widget, owner, &mut text);
    SendMessageW(tip, TTM_ADDTOOLW, 0, &mut ti as *mut _ as LPARAM);
    SendMessageW(tip, TTM_SETMAXTIPWIDTH, 0, 600); // enables multi-line text
    tip
}

unsafe fn tool_info(widget: HWND, owner: HWND, text: &mut [u16]) -> TTTOOLINFOW {
    let mut ti: TTTOOLINFOW = zeroed();
    ti.cbSize = size_of::<TTTOOLINFOW>() as u32;
    ti.uFlags = TTF_IDISHWND | TTF_SUBCLASS;
    ti.hwnd = owner;
    ti.uId = widget as usize;
    ti.lpszText = text.as_mut_ptr();
    ti
}

unsafe fn set_tooltip_text(lines: &[String]) {
    let (tip, widget, controller) = with(|a| (a.tooltip, a.widget, a.controller));
    if tip.is_null() {
        return;
    }
    let mut text = wide(&lines.join("\r\n"));
    let ti = tool_info(widget, controller, &mut text);
    SendMessageW(tip, TTM_UPDATETIPTEXTW, 0, &ti as *const _ as LPARAM);
}

unsafe fn redraw() {
    let (widget, light) = with(|a| (a.widget, a.light));
    if widget.is_null() || IsWindow(widget) == 0 {
        return;
    }
    let tray = find_tray();
    if tray.is_null() {
        return;
    }
    let layout = taskbar_layout(tray);
    let dpi = GetDpiForWindow(tray).max(96);
    let hidden = settings::hidden_devices();
    // Disconnected devices drop off the bar (they stay listed in the right-click menu).
    let shown: Vec<_> = readings()
        .into_iter()
        .filter(|r| !hidden.contains(&r.id) && r.status != Status::NotConnected)
        .collect();
    set_tooltip_text(&shown.iter().map(|r| format!("{}: {}", r.name, r.status)).collect::<Vec<_>>());
    let items: Vec<_> = shown.iter().map(|r| (r.kind, r.status)).collect();
    let size = if items.is_empty() { (0, 0) } else { render::draw(widget, &items, dpi, layout, light) };
    with(|a| {
        a.size = size;
        a.layout = Some(layout);
    });
    place();
}

/// A taskbar docked left/right is taller than wide; lay the widget out as a column there.
unsafe fn taskbar_layout(tray: HWND) -> render::Layout {
    let mut tr: RECT = zeroed();
    GetWindowRect(tray, &mut tr);
    let (w, h) = (tr.right - tr.left, tr.bottom - tr.top);
    if h > w {
        render::Layout::Column { width: w }
    } else {
        render::Layout::Row { height: h }
    }
}

/// Keeps the widget next to the notification area (left of it on a horizontal taskbar, above
/// it on a vertical one) and above the taskbar's XAML content.
unsafe fn place() {
    let (widget, (width, height), rendered) = with(|a| (a.widget, a.size, a.layout));
    if widget.is_null() || IsWindow(widget) == 0 {
        return;
    }
    let tray = find_tray();
    if tray.is_null() {
        return;
    }
    // Taskbar moved to another edge or resized: the bitmap was drawn for the old shape.
    if rendered != Some(taskbar_layout(tray)) {
        redraw(); // sets the new layout, then calls back here
        return;
    }
    if width == 0 {
        ShowWindow(widget, SW_HIDE); // nothing to show
        return;
    }
    let mut tr: RECT = zeroed();
    GetWindowRect(tray, &mut tr);
    let mut nr: RECT = zeroed();
    let notify = FindWindowExW(tray, null_mut(), wide("TrayNotifyWnd").as_ptr(), null());
    let has_notify = !notify.is_null() && GetWindowRect(notify, &mut nr) != 0 && nr.right > nr.left;
    let margin = 4 * GetDpiForWindow(tray).max(96) as i32 / 96;
    let (x, y) = match rendered {
        Some(render::Layout::Column { .. }) => {
            let bottom = if has_notify { nr.top } else { tr.bottom };
            (0, bottom - tr.top - height - margin)
        }
        _ => {
            let right = if has_notify { nr.left } else { tr.right };
            (right - tr.left - width - margin, 0)
        }
    };

    let mut cur: RECT = zeroed();
    GetWindowRect(widget, &mut cur);
    let moved = cur.left - tr.left != x
        || cur.top - tr.top != y
        || cur.right - cur.left != width
        || cur.bottom - cur.top != height;
    let on_top = GetWindow(tray, GW_CHILD) == widget;
    if moved || !on_top || IsWindowVisible(widget) == 0 {
        SetWindowPos(widget, HWND_TOP, x, y, width, height, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    }
}

unsafe fn show_menu(hwnd: HWND) {
    let devices = readings();
    let hidden = settings::hidden_devices();
    let menu = CreatePopupMenu();
    let mut labels = Vec::new(); // keep the UTF-16 buffers alive while the menu exists

    for (i, r) in devices.iter().enumerate() {
        let label = wide(&format!("{}  —  {}", r.name, r.status));
        let check = if hidden.contains(&r.id) { MF_UNCHECKED } else { MF_CHECKED };
        AppendMenuW(menu, MF_STRING | check, CMD_DEVICE_BASE + i, label.as_ptr());
        labels.push(label);
    }
    if devices.is_empty() {
        let label = wide("No devices found");
        AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, label.as_ptr());
        labels.push(label);
    }
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    let (refresh, autostart, exit) = (wide("Refresh now"), wide("Start with Windows"), wide("Exit"));
    let alerts = wide("Low battery alerts");
    let copy_label = wide("Copy diagnostics");
    let probe_label = wide("Probe an unsupported device (60 s)");
    let checked = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
    AppendMenuW(menu, MF_STRING, CMD_REFRESH, refresh.as_ptr());
    AppendMenuW(menu, MF_STRING | checked(settings::alerts_enabled()), CMD_ALERTS, alerts.as_ptr());
    AppendMenuW(menu, MF_STRING | checked(settings::autostart_enabled()), CMD_AUTOSTART, autostart.as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    AppendMenuW(menu, MF_STRING, CMD_COPY_DIAGNOSTICS, copy_label.as_ptr());
    let probing = if PROBING.load(Ordering::Relaxed) { MF_GRAYED } else { 0 };
    AppendMenuW(menu, MF_STRING | probing, CMD_PROBE, probe_label.as_ptr());
    AppendMenuW(menu, MF_STRING, CMD_EXIT, exit.as_ptr());

    let mut pt: POINT = zeroed();
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RIGHTALIGN, pt.x, pt.y, 0, hwnd, null());
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);

    match cmd as usize {
        0 => {}
        CMD_REFRESH => wake(),
        CMD_AUTOSTART => settings::set_autostart(!settings::autostart_enabled()),
        CMD_ALERTS => settings::set_alerts(!settings::alerts_enabled()),
        CMD_COPY_DIAGNOSTICS => copy_diagnostics(hwnd),
        CMD_PROBE => start_probe(),
        CMD_EXIT => {
            let widget = with(|a| a.widget);
            if !widget.is_null() {
                DestroyWindow(widget);
            }
            DestroyWindow(hwnd);
        }
        c if c >= CMD_DEVICE_BASE => {
            if let Some(r) = devices.get(c - CMD_DEVICE_BASE) {
                settings::toggle_hidden(&r.id);
                redraw();
            }
        }
        _ => {}
    }
}

static PROBING: AtomicBool = AtomicBool::new(false);

/// Runs the device probe in the background, then opens its file in Notepad.
fn start_probe() {
    if PROBING.swap(true, Ordering::Relaxed) {
        return;
    }
    std::thread::spawn(|| {
        init_com();
        let _ = notify::show(
            "Probing devices for 60 seconds",
            "Use the device as usual but don't type. A text file opens when it's done: paste it into a device report on GitHub.",
        );
        match probe::run_to_file(probe::LISTEN) {
            Ok(path) => {
                let _ = std::process::Command::new("notepad.exe").arg(path).spawn();
            }
            Err(e) => {
                diag::event("probe", format!("failed: {e}"));
                let _ = notify::show("Probe failed", &e);
            }
        }
        PROBING.store(false, Ordering::Relaxed);
    });
}

/// Puts the diagnostics log on the clipboard and confirms with a toast.
unsafe fn copy_diagnostics(hwnd: HWND) {
    let (title, body) = match diag::report() {
        Some(text) if set_clipboard(hwnd, &text) => ("Diagnostics copied", "Paste them into your device report on GitHub."),
        Some(_) => ("Couldn't copy diagnostics", "Another app is using the clipboard. Try again."),
        None => ("No diagnostics yet", "Nothing has been logged so far."),
    };
    // Toasts are WinRT, and COM is only initialized on the worker thread.
    std::thread::spawn(move || {
        init_com();
        let _ = notify::show(title, body);
    });
}

unsafe fn set_clipboard(hwnd: HWND, text: &str) -> bool {
    const CF_UNICODETEXT: u32 = 13;
    let units: Vec<u16> = text.replace('\n', "\r\n").encode_utf16().chain(std::iter::once(0)).collect();
    if OpenClipboard(hwnd) == 0 {
        return false;
    }
    let mut ok = false;
    let mem = GlobalAlloc(GMEM_MOVEABLE, units.len() * 2);
    if !mem.is_null() {
        let dst = GlobalLock(mem) as *mut u16;
        if !dst.is_null() {
            std::ptr::copy_nonoverlapping(units.as_ptr(), dst, units.len());
            GlobalUnlock(mem);
            EmptyClipboard();
            ok = !SetClipboardData(CF_UNICODETEXT, mem).is_null();
        }
        if !ok {
            GlobalFree(mem); // on success the clipboard owns it
        }
    }
    CloseClipboard();
    ok
}

unsafe extern "system" fn controller_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let taskbar_created = with(|a| a.taskbar_created);
    match msg {
        WM_APP_UPDATE => redraw(),
        WM_TIMER => {
            ensure_widget();
            place();
        }
        WM_SETTINGCHANGE => {
            with(|a| a.light = settings::light_theme());
            redraw();
        }
        WM_DEVICECHANGE => {
            wake();
            return 1;
        }
        WM_POWERBROADCAST => {
            if wp == PBT_APMRESUMEAUTOMATIC {
                wake(); // PC woke from sleep: devices may have changed meanwhile
            }
            return 1;
        }
        WM_APP_MENU => show_menu(hwnd),
        WM_DESTROY => PostQuitMessage(0),
        m if m == taskbar_created && m != 0 => {
            // Explorer restarted: the old widget died with the old taskbar.
            with(|a| a.widget = null_mut());
            ensure_widget();
        }
        _ => return DefWindowProcW(hwnd, msg, wp, lp),
    }
    0
}

unsafe extern "system" fn widget_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_RBUTTONUP => {
            let controller = with(|a| a.controller);
            PostMessageW(controller, WM_APP_MENU, 0, 0);
            0
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
