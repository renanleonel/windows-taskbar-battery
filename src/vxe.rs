//! VXE (ATK) wireless mice, over their 2.4 GHz dongle or a cable (USB vendor id 373B).
//!
//! Protocol from madr (https://github.com/dkbednarczyk/madr), reverse engineered on the VXE
//! MAD R and confirmed on a VXE R1 SE dongle (373B:10C9). The command is output report 0x08
//! on the vendor collection (usage page 0xFF02) that declares it:
//!
//! ```text
//! 08 04 00 00 00 00 00 00 00 00 00 00 00 00 00 00 49     (last byte: 0x55 minus the sum of the rest)
//! ```
//!
//! The reply is the next input report: `08 04 .. .. .. .. PCT CHG VH VL ..`, with the
//! percentage in byte 6, 1 in byte 7 while charging, and the voltage in mV big-endian in
//! bytes 8-9. Observed on the R1 SE: `08 04 00 00 00 02 5A 00 0F C6 ...` (90%, 4038 mV),
//! matching the ATK app. A reply with no voltage is treated as "mouse off or asleep".

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashMap;
use std::ffi::CString;
use std::time::{Duration, Instant};

const VID: u16 = 0x373B;
const USAGE_PAGE: u16 = 0xFF02;
const REPORT_ID: u8 = 0x08;
const CMD_BATTERY: u8 = 0x04;
const WAIT: Duration = Duration::from_millis(200);

/// Product ids with a known name. Others still work and show as "VXE mouse".
const NAMES: &[(u16, &str)] = &[
    (0x10C9, "VXE R1 SE"),  // NK mouse NANO dongle, tested
    (0x1040, "VXE MAD R"),  // dongle, per madr
    (0x103F, "VXE MAD R"),  // cable, per madr
];

struct Mouse {
    dev: HidDevice,
    pid: u16,
    seen: bool,
}

#[derive(Default)]
pub struct Vxe {
    open: HashMap<CString, Mouse>,
    /// Collections already checked and found without the battery command.
    skip: Vec<CString>,
}

impl Vxe {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let present: Vec<_> = api
            .device_list()
            .filter(|d| d.vendor_id() == VID && d.usage_page() == USAGE_PAGE)
            .map(|d| (d.path().to_owned(), d.product_id()))
            .collect();
        self.open.retain(|path, _| present.iter().any(|(p, _)| p == path));
        self.skip.retain(|path| present.iter().any(|(p, _)| p == path));

        // The dongle has more than one 0xFF02 collection; only one declares report 0x08.
        for (path, pid) in &present {
            if self.open.contains_key(path) || self.skip.contains(path) || self.open.values().any(|m| m.pid == *pid) {
                continue;
            }
            let Ok(dev) = api.open_path(path) else { continue };
            if !probe::descriptor(&dev).is_some_and(|d| probe::has_output_id(&d, REPORT_ID)) {
                self.skip.push(path.clone());
                continue;
            }
            self.open.insert(path.clone(), Mouse { dev, pid: *pid, seen: false });
        }

        let mut by_name: HashMap<&'static str, State> = HashMap::new();
        let mut broken = Vec::new();
        for (path, m) in self.open.iter_mut() {
            let key = format!("vxe:{:04x}", m.pid);
            let state = match query(&m.dev) {
                Ok(Some((percent, charging))) => {
                    m.seen = true;
                    diag::event(&key, "battery reply");
                    State::Battery { percent, charging }
                }
                Ok(None) => {
                    diag::event(&key, "no battery reply (mouse off or asleep)");
                    if !m.seen {
                        continue;
                    }
                    State::Unreachable
                }
                Err(e) => {
                    diag::event(&key, format!("i/o failed: {e}"));
                    broken.push(path.clone());
                    continue;
                }
            };
            let slot = by_name.entry(name(m.pid)).or_insert(state);
            if !matches!(slot, State::Battery { .. }) {
                *slot = state;
            }
        }
        for path in broken {
            self.open.remove(&path);
        }
        by_name
            .into_iter()
            .map(|(name, state)| Observation { id: format!("vxe:{}", slug(name)), name: Some(name.into()), kind: Some(Kind::Mouse), state })
            .collect()
    }
}

fn name(pid: u16) -> &'static str {
    NAMES.iter().find(|n| n.0 == pid).map_or("VXE mouse", |n| n.1)
}

fn slug(name: &str) -> String {
    name.to_ascii_lowercase().split_whitespace().collect::<Vec<_>>().join("-")
}

fn command() -> [u8; 17] {
    let mut r = [0u8; 17];
    r[0] = REPORT_ID;
    r[1] = CMD_BATTERY;
    let sum = r[..16].iter().fold(0u8, |a, b| a.wrapping_add(*b));
    r[16] = 0x55u8.wrapping_sub(sum);
    r
}

fn query(dev: &HidDevice) -> hidapi::HidResult<Option<(u8, bool)>> {
    let mut buf = [0u8; 64];
    while dev.read_timeout(&mut buf, 0)? > 0 {} // stale reports
    dev.write(&command())?;
    let deadline = Instant::now() + WAIT;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let n = dev.read_timeout(&mut buf, left.as_millis() as i32)?;
        if n == 0 {
            break;
        }
        if let Some(r) = parse(&buf[..n]) {
            return Ok(Some(r));
        }
    }
    Ok(None)
}

fn parse(r: &[u8]) -> Option<(u8, bool)> {
    match r {
        [REPORT_ID, CMD_BATTERY, _, _, _, _, percent, charging, vh, vl, ..]
            if *percent <= 100 && (*vh != 0 || *vl != 0) =>
        {
            Some((*percent, *charging == 1))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_command() {
        let c = command();
        assert_eq!(c[0], 0x08);
        assert_eq!(c[1], 0x04);
        assert_eq!(c[16], 0x49);
    }

    #[test]
    fn parses_reply() {
        // Captured from a VXE R1 SE dongle (373B:10C9) while the ATK app showed 90%.
        let r = [0x08, 0x04, 0, 0, 0, 0x02, 0x5A, 0x00, 0x0F, 0xC6, 0, 0, 0, 0, 0, 0, 0x18];
        assert_eq!(parse(&r), Some((90, false)));
        let mut charging = r;
        charging[7] = 1;
        assert_eq!(parse(&charging), Some((90, true)));
    }

    #[test]
    fn rejects_other_reports() {
        assert_eq!(parse(&[0x08, 0x04, 0, 0, 0, 0, 0x5A, 0, 0, 0]), None); // no voltage: asleep
        assert_eq!(parse(&[0x08, 0x05, 0, 0, 0, 0, 0x5A, 0, 0x0F, 0xC6]), None);
        assert_eq!(parse(&[0x08, 0x04, 0, 0, 0, 0, 0xFF, 0, 0x0F, 0xC6]), None);
        assert_eq!(parse(&[0x08, 0x04]), None);
    }

    #[test]
    fn names() {
        assert_eq!(name(0x10C9), "VXE R1 SE");
        assert_eq!(name(0x1234), "VXE mouse");
    }
}
