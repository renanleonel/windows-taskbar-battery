//! Rapoo wireless mice on Rapoo's own 2.4 GHz dongle (VID 24AE), e.g. the VT2 Pro Max (1423).
//!
//! The dongle broadcasts a 13-byte status report (id 0x07) on its vendor collection (usage
//! page 0xFF00, usage 0x0002) every few seconds, unasked: `07 20 … <battery> …`, with the
//! battery percentage at byte 8. Observed from a VT2 Pro Max: `07 20 03 40 06 40 06 01 41 00
//! 01 01 00` (65%). Same layout as clicksync's Rapoo driver. No charging flag is known, so it's
//! always reported as not charging.
//!
//! Nothing is written to the device. Each collection stays open between scans and each scan
//! drains the reports queued since the last one without waiting.

use crate::device::{Kind, Observation, State};
use crate::diag;
use hidapi::{HidApi, HidDevice};
use std::collections::HashMap;
use std::ffi::CString;
use std::time::{Duration, Instant};

const VID: u16 = 0x24AE;
const USAGE_PAGE: u16 = 0xFF00;
const USAGE: u16 = 0x0002;
const REPORT_ID: u8 = 0x07;
const STATUS: u8 = 0x20;
/// A report this recent still counts as live. Reports come every ~5 s and scans every 15 s,
/// so this covers a forced scan right after a regular one.
const FRESH: Duration = Duration::from_secs(20);

struct Tracked {
    dev: HidDevice,
    key: String,
    last: Option<(u8, Instant)>,
}

#[derive(Default)]
pub struct Rapoo {
    open: HashMap<CString, Tracked>,
}

impl Rapoo {
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
            let key = format!("rapoo:{pid:04x}");
            match api.open_path(path) {
                Ok(dev) => {
                    self.open.insert(path.clone(), Tracked { dev, key, last: None });
                }
                Err(e) => diag::event(&key, format!("open failed: {e}")),
            }
        }

        let mut out = Vec::new();
        let mut broken = Vec::new();
        for (path, t) in self.open.iter_mut() {
            let mut buf = [0u8; 64];
            let mut failed = false;
            loop {
                match t.dev.read_timeout(&mut buf, 0) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Some(p) = parse(&buf[..n]) {
                            if t.last.is_none() {
                                diag::event(&t.key, format!("status report: {p}% ({n} bytes)"));
                            }
                            t.last = Some((p, Instant::now()));
                        }
                    }
                    Err(e) => {
                        // Dongle unplugged: reopen on the next scan.
                        diag::event(&t.key, format!("read failed: {e}"));
                        failed = true;
                        break;
                    }
                }
            }
            if failed {
                broken.push(path.clone());
                continue;
            }
            // Never read yet (just opened): nothing to show until the first report.
            let Some((percent, at)) = t.last else { continue };
            let state = if at.elapsed() < FRESH {
                State::Battery { percent, charging: false }
            } else {
                diag::event(&t.key, "no status report since last scan");
                State::Unreachable
            };
            out.push(Observation {
                id: t.key.clone(),
                name: Some("Rapoo wireless mouse".into()),
                kind: Some(Kind::Mouse),
                state,
            });
        }
        for path in broken {
            self.open.remove(&path);
        }
        out
    }
}

/// The battery percentage from a status report (report id included).
fn parse(r: &[u8]) -> Option<u8> {
    match r {
        [REPORT_ID, STATUS, _, _, _, _, _, _, p, ..] if *p <= 100 => Some(*p),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn status_report() {
        // VT2 Pro Max, from a device probe (issue #3).
        assert_eq!(parse(&[0x07, 0x20, 0x03, 0x40, 0x06, 0x40, 0x06, 0x01, 0x41, 0x00, 0x01, 0x01, 0x00]), Some(65));
        assert_eq!(parse(&[0x07, 0x20, 0x03, 0x40, 0x06, 0x40, 0x06, 0x01, 0xC8]), None); // out of range
        assert_eq!(parse(&[0x07, 0x21, 0x03, 0x40, 0x06, 0x40, 0x06, 0x01, 0x41]), None); // not a status report
        assert_eq!(parse(&[0x06, 0x20, 0x03, 0x40, 0x06, 0x40, 0x06, 0x01, 0x41]), None); // other report id
        assert_eq!(parse(&[0x07, 0x20, 0x03]), None); // too short
    }
}
