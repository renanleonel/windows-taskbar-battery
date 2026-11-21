//! Generic battery reading for USB devices that report it the standard HID way: a "Battery
//! Strength" field (Generic Device Controls page 0x06, usage 0x20) in an input or feature
//! report. No vendor protocol is involved; the field's report, position, size and range all
//! come from the device's own report descriptor.
//!
//! Feature reports are read on every scan. Input reports arrive whenever the device sends
//! them (often only when the level changes), so the collection stays open and the last
//! value is kept.
//!
//! Vendors with their own module are skipped, so nothing is listed twice.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{BusType, HidApi, HidDevice};
use std::collections::HashMap;
use std::ffi::CString;

const BATTERY_STRENGTH: u32 = 0x0006_0020;

/// Vendors read by their own module (Logitech, Razer, SteelSeries, Corsair, Sony, Microsoft,
/// Delux, MCHOSE, Rapoo, VXE).
const NATIVE_VIDS: &[u16] = &[0x046D, 0x1532, 0x1038, 0x1B1C, 0x054C, 0x045E, 0x1D57, 0x3837, 0x291D, 0x5253, 0x24AE, 0x373B];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ReportKind {
    Input,
    Feature,
}

/// Where the battery field sits.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Field {
    kind: ReportKind,
    id: u8,
    /// Bit offset in the report's data (after the report id byte, if any).
    offset: u32,
    size: u32,
    min: i32,
    max: i32,
    /// Data bytes of the whole report (without the id).
    report_len: usize,
}

struct Tracked {
    dev: HidDevice,
    key: String,
    name: String,
    field: Field,
    last: Option<u8>,
}

#[derive(Default)]
pub struct HidBattery {
    open: HashMap<CString, Tracked>,
    /// Collections already checked and found without a battery field (or unopenable).
    skip: Vec<CString>,
}

impl HidBattery {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let present: Vec<_> = api
            .device_list()
            .filter(|d| matches!(d.bus_type(), BusType::Usb) && !NATIVE_VIDS.contains(&d.vendor_id()))
            .filter(|d| !(d.usage_page() == 0x01 && matches!(d.usage(), 0x02 | 0x06 | 0x07)))
            .map(|d| (d.path().to_owned(), d.vendor_id(), d.product_id(), d.product_string().unwrap_or("").to_string()))
            .collect();
        self.open.retain(|path, _| present.iter().any(|(p, ..)| p == path));
        self.skip.retain(|path| present.iter().any(|(p, ..)| p == path));

        for (path, vid, pid, product) in &present {
            if self.open.contains_key(path) || self.skip.contains(path) {
                continue;
            }
            let field = api.open_path(path).ok().and_then(|dev| {
                let field = probe::descriptor(&dev).and_then(|d| battery_field(&d))?;
                Some((dev, field))
            });
            let Some((dev, field)) = field else {
                self.skip.push(path.clone());
                continue;
            };
            let key = format!("hid:{vid:04x}:{pid:04x}");
            diag::event(&key, format!("battery strength field: {field:?}"));
            let name = if product.trim().is_empty() { format!("USB device {vid:04X}:{pid:04X}") } else { product.trim().to_string() };
            self.open.insert(path.clone(), Tracked { dev, key, name, field, last: None });
        }

        let mut out = Vec::new();
        let mut broken = Vec::new();
        for (path, t) in self.open.iter_mut() {
            match read(&t.dev, &t.field) {
                Ok(Some(p)) => t.last = Some(p),
                Ok(None) => {}
                Err(e) => {
                    diag::event(&t.key, format!("i/o failed: {e}"));
                    broken.push(path.clone());
                    continue;
                }
            }
            let Some(percent) = t.last else { continue };
            out.push(Observation { id: t.key.clone(), name: Some(t.name.clone()), kind: Some(Kind::from_name(&t.name)), state: State::Battery { percent, charging: false } });
        }
        for path in broken {
            self.open.remove(&path);
        }
        // Several collections of one device can carry the field; list it once.
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out.dedup_by(|a, b| a.id == b.id);
        out
    }
}

/// The newest battery percentage: a fresh feature report, or the last queued input report.
fn read(dev: &HidDevice, f: &Field) -> hidapi::HidResult<Option<u8>> {
    match f.kind {
        ReportKind::Feature => {
            let mut buf = vec![0u8; f.report_len + 1];
            buf[0] = f.id;
            let n = dev.get_feature_report(&mut buf)?;
            Ok(extract(&buf[1..n.max(1)], f))
        }
        ReportKind::Input => {
            let mut buf = [0u8; 1024];
            let mut found = None;
            loop {
                let n = dev.read_timeout(&mut buf, 0)?;
                if n == 0 {
                    break;
                }
                // Reports with an id start with it; without ids, the data starts at once.
                let data = match f.id {
                    0 => &buf[..n],
                    id if buf[0] == id => &buf[1..n],
                    _ => continue,
                };
                found = extract(data, f).or(found);
            }
            Ok(found)
        }
    }
}

/// Reads the field from report data (without the id) and scales it to 0-100.
fn extract(data: &[u8], f: &Field) -> Option<u8> {
    if f.size == 0 || f.size > 32 || (f.offset + f.size).div_ceil(8) as usize > data.len() {
        return None;
    }
    let mut raw: u32 = 0;
    for i in 0..f.size {
        let bit = f.offset + i;
        if data[(bit / 8) as usize] >> (bit % 8) & 1 == 1 {
            raw |= 1 << i;
        }
    }
    let v = i64::from(raw);
    let (min, max) = (i64::from(f.min), i64::from(f.max));
    if max <= min || v < min || v > max {
        return None;
    }
    Some(((v - min) * 100 / (max - min)) as u8)
}

/// Finds the first Battery Strength field in a report descriptor.
fn battery_field(desc: &[u8]) -> Option<Field> {
    #[derive(Clone, Copy, Default)]
    struct Globals {
        page: u32,
        min: i32,
        max: i32,
        size: u32,
        count: u32,
        id: u8,
    }
    let mut g = Globals::default();
    let mut stack = Vec::new();
    let mut usages: Vec<u32> = Vec::new();
    let (mut usage_min, mut usage_max): (Option<u32>, Option<u32>) = (None, None);
    let mut offsets: HashMap<(u8, u8), u32> = HashMap::new(); // (main tag, id) -> bits so far
    let mut found: Option<(Field, u8)> = None; // field and its main tag

    let mut i = 0;
    while i < desc.len() {
        let prefix = desc[i];
        if prefix == 0xFE {
            i += 3 + desc.get(i + 1).copied().unwrap_or(0) as usize;
            continue;
        }
        let n = [0, 1, 2, 4][(prefix & 3) as usize];
        let data = desc.get(i + 1..i + 1 + n).unwrap_or(&[]);
        let value = data.iter().rev().fold(0u32, |v, b| v << 8 | u32::from(*b));
        let signed = match n {
            1 => i32::from(value as u8 as i8),
            2 => i32::from(value as u16 as i16),
            _ => value as i32,
        };
        let full_usage = |v: u32| if n == 4 { v } else { g.page << 16 | v };
        match ((prefix >> 2) & 3, prefix >> 4) {
            // Main items: Input, Output, Feature.
            (0, tag @ (0x8 | 0x9 | 0xB)) => {
                let start = *offsets.get(&(tag, g.id)).unwrap_or(&0);
                if found.is_none() && tag != 0x9 && value & 1 == 0 {
                    for j in 0..g.count {
                        let usage = if !usages.is_empty() {
                            usages[(j as usize).min(usages.len() - 1)]
                        } else if let (Some(lo), Some(hi)) = (usage_min, usage_max) {
                            (lo + j).min(hi)
                        } else {
                            0
                        };
                        if usage == BATTERY_STRENGTH {
                            let kind = if tag == 0x8 { ReportKind::Input } else { ReportKind::Feature };
                            let field = Field { kind, id: g.id, offset: start + j * g.size, size: g.size, min: g.min, max: g.max, report_len: 0 };
                            found = Some((field, tag));
                            break;
                        }
                    }
                }
                offsets.insert((tag, g.id), start + g.size * g.count);
                usages.clear();
                (usage_min, usage_max) = (None, None);
            }
            // Collection / End Collection clear the local items too.
            (0, _) => {
                usages.clear();
                (usage_min, usage_max) = (None, None);
            }
            (1, 0x0) => g.page = value,
            (1, 0x1) => g.min = signed,
            (1, 0x2) => g.max = if g.min < 0 { signed } else { value as i32 },
            (1, 0x7) => g.size = value,
            (1, 0x8) => g.id = value as u8,
            (1, 0x9) => g.count = value,
            (1, 0xA) => stack.push(g),
            (1, 0xB) => g = stack.pop().unwrap_or(g),
            (2, 0x0) => usages.push(full_usage(value)),
            (2, 0x1) => usage_min = Some(full_usage(value)),
            (2, 0x2) => usage_max = Some(full_usage(value)),
            _ => {}
        }
        i += 1 + n;
    }
    let (mut field, tag) = found?;
    field.report_len = offsets.get(&(tag, field.id)).copied().unwrap_or(0).div_ceil(8) as usize;
    Some(field)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Consumer control collection with a battery strength byte in input report 2.
    const INPUT_DESC: &[u8] = &[
        0x05, 0x0C, 0x09, 0x01, 0xA1, 0x01, // consumer control
        0x85, 0x02, // report id 2
        0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x09, 0xE9, 0x81, 0x02, // 8 one-bit buttons
        0x05, 0x06, 0x09, 0x20, // generic device controls: battery strength
        0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02, // 0-255, one byte
        0xC0,
    ];

    #[test]
    fn finds_input_field() {
        let f = battery_field(INPUT_DESC).unwrap();
        assert_eq!(f, Field { kind: ReportKind::Input, id: 2, offset: 8, size: 8, min: 0, max: 255, report_len: 2 });
        assert_eq!(extract(&[0x00, 0xFF], &f), Some(100));
        assert_eq!(extract(&[0x00, 0x80], &f), Some(50));
        assert_eq!(extract(&[0x00], &f), None); // too short
    }

    #[test]
    fn finds_feature_field_with_extended_usage() {
        // Vendor page, feature report without id: padding byte, then battery 0-100 via a
        // 4-byte usage (page 0x0006, usage 0x0020).
        let desc = [
            0x06, 0x00, 0xFF, 0x09, 0x01, 0xA1, 0x01, //
            0x75, 0x08, 0x95, 0x01, 0xB1, 0x03, // constant padding byte
            0x15, 0x00, 0x25, 0x64, 0x0B, 0x20, 0x00, 0x06, 0x00, 0xB1, 0x02, // battery
            0xC0,
        ];
        let f = battery_field(&desc).unwrap();
        assert_eq!((f.kind, f.id, f.offset, f.size, f.min, f.max, f.report_len), (ReportKind::Feature, 0, 8, 8, 0, 100, 2));
        assert_eq!(extract(&[0x00, 73], &f), Some(73));
        assert_eq!(extract(&[0x00, 101], &f), None); // out of range
    }

    #[test]
    fn no_field_without_battery_usage() {
        assert_eq!(battery_field(&INPUT_DESC[..20]), None);
        assert_eq!(battery_field(&[]), None);
    }
}
