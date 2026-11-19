//! Draws the widget into a per-pixel-alpha bitmap and pushes it with UpdateLayeredWindow,
//! so text blends cleanly over the (translucent) Windows 11 taskbar.

use crate::device::{Kind, Status};
use crate::wide;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{HWND, POINT, SIZE};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::{UpdateLayeredWindow, ULW_ALPHA};

pub const LOW_BATTERY: u8 = 15;
const BOLT: &str = "\u{E945}";
/// Opacity for the last known value of a sleeping device.
const STALE_OPACITY: u32 = 130;
/// Newer glyph (clearer at small sizes); older Segoe Fluent Icons versions only have the pair.
const EARBUD_SINGLE: &str = "\u{F27C}";
const EARBUD_PAIR: &str = "\u{F4C0}";
/// Drawn from Segoe MDL2 Assets: the same code point in Segoe Fluent Icons is a flat rounded
/// box at 15 px that reads as a keyboard; the MDL2 one has grips.
const GAMEPAD: &str = "\u{E7FC}";

/// Segoe Fluent Icons glyph per device kind (the gamepad is drawn from Segoe MDL2 Assets).
fn icon(kind: Kind) -> &'static str {
    match kind {
        Kind::Headset => "\u{E7F6}",
        Kind::Earbuds => EARBUD_SINGLE, // swapped for EARBUD_PAIR if the font lacks it
        Kind::Mouse => "\u{E962}",
        Kind::Keyboard => "\u{E765}",
        Kind::Gamepad => GAMEPAD,
        Kind::Other => "\u{E83F}", // battery
    }
}

struct Seg {
    text: Vec<u16>,
    icon: bool,
    color: u32, // 0xRRGGBB
    opacity: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    gap_after: i32,
    /// Index of the device this segment belongs to.
    item: usize,
}

/// How the widget is laid out on the taskbar.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layout {
    /// Horizontal taskbar of this height: devices in a row.
    Row { height: i32 },
    /// Vertical (left/right) taskbar of this width: devices stacked, icon above the label.
    Column { width: i32 },
}

/// Renders the widget and returns its (width, height) in physical pixels.
pub unsafe fn draw(hwnd: HWND, items: &[(Kind, Status)], dpi: u32, layout: Layout, light: bool) -> (i32, i32) {
    let s = |v: i32| v * dpi as i32 / 96;
    let fg = if light { 0x000000 } else { 0xFFFFFF };
    let red = if light { 0xC42B1C } else { 0xFF6B6B };
    let green = if light { 0x0F7B0F } else { 0x6CCB5F };

    let mut segs = Vec::new();
    let seg = |item: usize, text: &str, icon: bool, color: u32, opacity: u32, gap_after: i32| Seg {
        text: wide(text),
        icon,
        color,
        opacity,
        x: 0,
        y: 0,
        w: 0,
        h: 0,
        gap_after,
        item,
    };
    for (i, (kind, status)) in items.iter().enumerate() {
        let gap = if i == items.len() - 1 { 0 } else { s(12) };
        let (label, color, opacity, charging) = match *status {
            Status::Battery { percent, charging, stale } => {
                let color = if percent <= LOW_BATTERY && !charging { red } else { fg };
                (format!("{percent}%"), color, if stale { STALE_OPACITY } else { 255 }, charging)
            }
            Status::NotConnected => ("—".to_string(), fg, 110, false),
        };
        segs.push(seg(i, icon(*kind), true, color, opacity, s(5)));
        if charging {
            segs.push(seg(i, &label, false, color, opacity, s(2)));
            segs.push(seg(i, BOLT, true, green, opacity, gap));
        } else {
            segs.push(seg(i, &label, false, color, opacity, gap));
        }
    }

    let screen = GetDC(null_mut());
    let dc = CreateCompatibleDC(screen);
    ReleaseDC(null_mut(), screen);
    let text_font = font("Segoe UI", -s(12));
    let icon_font = font("Segoe Fluent Icons", -s(15));
    let symbol_font = font("Segoe MDL2 Assets", -s(15));
    let gamepad = wide(GAMEPAD);
    let font_of = |seg: &Seg| match (seg.icon, seg.text == gamepad) {
        (false, _) => text_font,
        (true, true) => symbol_font,
        (true, false) => icon_font,
    };

    // Fall back to the earbud pair if this font version lacks the single earbud.
    SelectObject(dc, icon_font);
    let single = wide(EARBUD_SINGLE);
    let mut glyph = 0u16;
    if GetGlyphIndicesW(dc, single.as_ptr(), 1, &mut glyph, GGI_MARK_NONEXISTING_GLYPHS) == GDI_ERROR as u32 || glyph == 0xFFFF {
        for seg in segs.iter_mut().filter(|s| s.text == single) {
            seg.text = wide(EARBUD_PAIR);
        }
    }

    // Measure.
    for seg in &mut segs {
        SelectObject(dc, font_of(seg));
        let mut size: SIZE = zeroed();
        GetTextExtentPoint32W(dc, seg.text.as_ptr(), seg.text.len() as i32 - 1, &mut size);
        seg.w = size.cx;
        seg.h = size.cy;
    }

    // Layout.
    let pad = s(6);
    let (width, height) = match layout {
        Layout::Row { height } => {
            let mut x = pad;
            for seg in &mut segs {
                seg.x = x;
                seg.y = (height - seg.h) / 2;
                x += seg.w + seg.gap_after;
            }
            (x + pad, height)
        }
        Layout::Column { width } => {
            // Per device: the icon on one line, the label (and charging bolt) centered below it.
            let mut y = pad;
            for i in 0..items.len() {
                let (first, rest) = {
                    let mut it = segs.iter_mut().filter(|s| s.item == i);
                    (it.next(), it.collect::<Vec<_>>())
                };
                if let Some(icon) = first {
                    icon.x = (width - icon.w) / 2;
                    icon.y = y;
                    y += icon.h + s(1);
                }
                let line_w: i32 = rest.iter().map(|s| s.w).sum::<i32>()
                    + rest.iter().rev().skip(1).map(|s| s.gap_after).sum::<i32>();
                let line_h = rest.iter().map(|s| s.h).max().unwrap_or(0);
                let mut x = (width - line_w) / 2;
                for seg in rest {
                    seg.x = x;
                    seg.y = y + (line_h - seg.h) / 2;
                    x += seg.w + seg.gap_after;
                }
                y += line_h;
                if i + 1 < items.len() {
                    y += s(8);
                }
            }
            (width, y + pad)
        }
    };

    // White-on-black text into a 32bpp DIB; coverage becomes alpha below.
    let mut bi: BITMAPINFO = zeroed();
    bi.bmiHeader = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        biHeight: -height, // top-down
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB as _,
        ..zeroed()
    };
    let mut bits: *mut c_void = null_mut();
    let bmp = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    if bmp.is_null() || bits.is_null() {
        DeleteObject(text_font);
        DeleteObject(icon_font);
        DeleteObject(symbol_font);
        DeleteDC(dc);
        return (width, height);
    }
    let old_bmp = SelectObject(dc, bmp);
    SetBkMode(dc, TRANSPARENT as _);
    SetTextColor(dc, 0x00FF_FFFF);
    for seg in &segs {
        SelectObject(dc, font_of(seg));
        TextOutW(dc, seg.x, seg.y, seg.text.as_ptr(), seg.text.len() as i32 - 1);
    }
    GdiFlush();

    // Per-pixel color/opacity from each segment's box, then convert coverage -> premultiplied BGRA.
    let mut paint = vec![(fg, 255u32); (width * height) as usize];
    for seg in &segs {
        let (x0, x1) = (seg.x.max(0), (seg.x + seg.w).min(width));
        let (y0, y1) = (seg.y.max(0), (seg.y + seg.h).min(height));
        for y in y0..y1 {
            for x in x0..x1 {
                paint[(y * width + x) as usize] = (seg.color, seg.opacity);
            }
        }
    }
    let px = std::slice::from_raw_parts_mut(bits as *mut u32, (width * height) as usize);
    for (i, p) in px.iter_mut().enumerate() {
        let (color, opacity) = paint[i];
        let cov = ((*p >> 16) & 0xFF).max((*p >> 8) & 0xFF).max(*p & 0xFF);
        let a = cov * opacity / 255;
        // Alpha 1 keeps the whole rect clickable while staying invisible.
        *p = if a == 0 {
            0x0100_0000
        } else {
            let ch = |shift: u32| ((color >> shift) & 0xFF) * a / 255;
            (a << 24) | (ch(16) << 16) | (ch(8) << 8) | ch(0)
        };
    }

    let size = SIZE { cx: width, cy: height };
    let origin = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
    UpdateLayeredWindow(hwnd, null_mut(), null(), &size, dc, &origin, 0, &blend, ULW_ALPHA);

    SelectObject(dc, old_bmp);
    DeleteObject(bmp);
    DeleteObject(text_font);
    DeleteObject(icon_font);
    DeleteObject(symbol_font);
    DeleteDC(dc);
    (width, height)
}

unsafe fn font(face: &str, height: i32) -> HFONT {
    let mut lf: LOGFONTW = zeroed();
    lf.lfHeight = height;
    lf.lfWeight = FW_NORMAL as _;
    lf.lfQuality = ANTIALIASED_QUALITY as _; // grayscale AA: coverage maps cleanly to alpha
    for (dst, src) in lf.lfFaceName.iter_mut().zip(face.encode_utf16().take(31)) {
        *dst = src;
    }
    CreateFontIndirectW(&lf)
}
