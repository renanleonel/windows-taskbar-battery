//! Delux (and other) wireless mice on the Xenta 2.4 GHz dongle (1D57:FA60).
//!
//! The dongle sends a 5-byte status report (id 0x03) on its HID Ordinal collection
//! (usage page 0x0A) about every 2 s; the last byte is the battery percentage.
//! Observed while charging: `03 2c 40 01 36` (54%) -> `03 2c 40 01 37` (55%).
//! The charging flag isn't known, so it's always reported as not charging.
//! A 0% report is dropped: one shows up now and then between normal readings (likely while
//! the mouse sleeps or drops the link), and a mouse really at 0% couldn't send it.
//!
//! The dongle stays open between scans and each scan drains the reports queued since the
//! last one without waiting, so a quiet dongle never delays the other sources.

use crate::device::{Kind, Observation, State};
use crate::diag;
use hidapi::{HidApi, HidDevice};
use std::time::{Duration, Instant};

const VID: u16 = 0x1D57;
const PID: u16 = 0xFA60;
const KEY: &str = "delux:fa60";
/// A report this recent still counts as live (covers forced scans right after a regular one).
const FRESH: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct Delux {
    dev: Option<HidDevice>,
    last: Option<(u8, Instant)>,
}

impl Delux {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        if self.dev.is_none() {
            let Some(d) = api
                .device_list()
                .find(|d| d.vendor_id() == VID && d.product_id() == PID && d.usage_page() == 0x0A)
            else {
                self.last = None;
                return Vec::new();
            };
            match api.open_path(d.path()) {
                Ok(dev) => self.dev = Some(dev),
                Err(e) => {
                    diag::event(KEY, format!("open failed: {e}"));
                    return Vec::new();
                }
            }
        }
        let dev = self.dev.as_ref().unwrap();

        let mut buf = [0u8; 64];
        let mut other = None;
        loop {
            match dev.read_timeout(&mut buf, 0) {
                Ok(0) => break,
                Ok(n) => match parse(&buf[..n]) {
                    Some(p) => self.last = Some((p, Instant::now())),
                    None => other = Some(buf[..n].iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")),
                },
                Err(e) => {
                    // Dongle unplugged: reopen on the next scan.
                    diag::event(KEY, format!("read failed: {e}"));
                    self.dev = None;
                    return Vec::new();
                }
            }
        }

        let state = match self.last {
            Some((percent, at)) if at.elapsed() < FRESH => {
                diag::event(KEY, "battery via report 0x03");
                State::Battery { percent, charging: false }
            }
            last => {
                diag::event(KEY, match other {
                    Some(r) => format!("no battery report since last scan (last: {r})"),
                    None => "no 0x03 report since last scan".to_string(),
                });
                // Never read yet (just opened): nothing to show until the first report.
                if last.is_none() {
                    return Vec::new();
                }
                State::Unreachable
            }
        };
        vec![Observation {
            id: KEY.into(),
            name: Some("Wireless mouse".into()),
            kind: Some(Kind::Mouse),
            state,
        }]
    }
}

fn parse(r: &[u8]) -> Option<u8> {
    match r {
        [0x03, _, _, _, p] if (1..=100).contains(p) => Some(*p),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn parse() {
        assert_eq!(super::parse(&[0x03, 0x2c, 0x40, 0x01, 0x36]), Some(54));
        assert_eq!(super::parse(&[0x03, 0x2c, 0x40, 0x01, 0xff]), None);
        assert_eq!(super::parse(&[0x03, 0x2c, 0x40, 0x01, 0x00]), None);
        assert_eq!(super::parse(&[0x01, 0x00]), None);
    }
}
