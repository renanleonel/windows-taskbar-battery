//! Attack Shark wireless mice on their 2.4 GHz dongle (USB vendor id 373E), e.g. the R5 Ultra
//! (373E:0047, "R5 Ultra Mouse 2.4G").
//!
//! The dongle's vendor collection (usage page 0xFFFF, usage 0x0000) holds a 64-byte feature
//! report without an id. Reading it (nothing is written to the dongle) returns a status
//! block with the battery percentage in data byte 7. From a device probe, with the mouse at
//! 94% in Attack Shark's app:
//!
//! ```text
//! (id 00) a1 00 02 02 00 83 00 5e 00 ...      0x5e = 94%
//! ```
//!
//! The charging flag isn't known, so it's always reported as not charging. A zero level is
//! taken as "mouse off or asleep".

use crate::device::{Kind, Observation, State};
use crate::diag;
use hidapi::{HidApi, HidDevice};
use std::collections::HashMap;
use std::ffi::CString;

pub const VID: u16 = 0x373E;
const USAGE_PAGE: u16 = 0xFFFF;
const USAGE: u16 = 0x0000;
/// Report id byte plus 64 data bytes.
const REPORT_LEN: usize = 65;
/// Index of the battery byte in the buffer (the report id byte comes first).
const BATTERY_AT: usize = 8;

/// Product ids with a known name. Others still work and show as "Attack Shark mouse".
const NAMES: &[(u16, &str)] = &[(0x0047, "Attack Shark R5 Ultra")];

struct Dongle {
    dev: HidDevice,
    pid: u16,
    seen: bool,
}

#[derive(Default)]
pub struct AttackShark {
    open: HashMap<CString, Dongle>,
}

impl AttackShark {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let present: Vec<_> = api
            .device_list()
            .filter(|d| d.vendor_id() == VID && d.usage_page() == USAGE_PAGE && d.usage() == USAGE)
            .map(|d| (d.path().to_owned(), d.product_id()))
            .collect();
        self.open.retain(|path, _| present.iter().any(|(p, _)| p == path));
        for (path, pid) in &present {
            if self.open.contains_key(path) {
                continue;
            }
            match api.open_path(path) {
                Ok(dev) => {
                    self.open.insert(path.clone(), Dongle { dev, pid: *pid, seen: false });
                }
                Err(e) => diag::event(&key(*pid), format!("open failed: {e}")),
            }
        }

        let mut out = Vec::new();
        let mut broken = Vec::new();
        for (path, d) in self.open.iter_mut() {
            let key = key(d.pid);
            let mut buf = [0u8; REPORT_LEN];
            let state = match d.dev.get_feature_report(&mut buf) {
                Ok(n) => match parse(&buf[..n.min(REPORT_LEN)]) {
                    Some(percent) => {
                        d.seen = true;
                        diag::event(&key, "battery via feature report 0x00");
                        State::Battery { percent, charging: false }
                    }
                    None => {
                        diag::event(&key, format!("no battery level (report: {})", hex(&buf[..n.min(12)])));
                        if !d.seen {
                            continue; // nothing to show until the first reading
                        }
                        State::Unreachable
                    }
                },
                Err(e) => {
                    // Dongle unplugged: reopen on the next scan.
                    diag::event(&key, format!("read failed: {e}"));
                    broken.push(path.clone());
                    continue;
                }
            };
            out.push(Observation { id: key, name: Some(name(d.pid).into()), kind: Some(Kind::Mouse), state });
        }
        for path in broken {
            self.open.remove(&path);
        }
        // A dongle can expose the collection more than once; list it once.
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out.dedup_by(|a, b| a.id == b.id);
        out
    }
}

fn key(pid: u16) -> String {
    format!("attackshark:{pid:04x}")
}

fn name(pid: u16) -> &'static str {
    NAMES.iter().find(|n| n.0 == pid).map_or("Attack Shark mouse", |n| n.1)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

/// The battery percentage from the feature report buffer (id byte first).
fn parse(r: &[u8]) -> Option<u8> {
    match r.get(BATTERY_AT) {
        Some(&p) if (1..=100).contains(&p) => Some(p),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_probe_report() {
        // From the R5 Ultra probe (issue #6), with the mouse at 94%.
        let mut r = [0u8; REPORT_LEN];
        r[..9].copy_from_slice(&[0x00, 0xa1, 0x00, 0x02, 0x02, 0x00, 0x83, 0x00, 0x5e]);
        assert_eq!(parse(&r), Some(94));
    }

    #[test]
    fn rejects_empty_or_out_of_range() {
        assert_eq!(parse(&[0u8; REPORT_LEN]), None);
        let mut r = [0u8; REPORT_LEN];
        r[BATTERY_AT] = 0xff;
        assert_eq!(parse(&r), None);
        assert_eq!(parse(&[0x00, 0xa1]), None);
    }

    #[test]
    fn names() {
        assert_eq!(name(0x0047), "Attack Shark R5 Ultra");
        assert_eq!(name(0x1234), "Attack Shark mouse");
    }
}
