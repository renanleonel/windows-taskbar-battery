//! HyperX Cloud Flight S (0951:16EA), battery over its vendor HID collection.
//!
//! The dongle exposes the control protocol on interface 5, usage page 0xFF13,
//! usage 0x0001, as a 62-byte report with id 0x06. Battery and charging come
//! back as 0xBB 0x02 / 0xBB 0x03 reports on the input endpoint (same layout
//! documented by the Nogenuity project). NGENUITY is optional.

use crate::device::{Kind, Observation, State};
use crate::diag;
use hidapi::{HidApi, HidDevice};
use std::time::{Duration, Instant};

const VENDOR: u16 = 0x0951;
const PRODUCT: u16 = 0x16EA;
const INTERFACE: i32 = 5;
const USAGE_PAGE: u16 = 0xFF13;
const USAGE: u16 = 0x0001;
const REPORT_ID: u8 = 0x06;
const SUB_BATTERY: u8 = 0x02;
const SUB_CHARGING: u8 = 0x03;

/// Builds the 62-byte second-command-set request used by the Classic DSP family.
fn request(sub: u8) -> [u8; 62] {
    let mut report = [0u8; 62];
    report[0] = REPORT_ID;
    report[2] = 0x02;
    report[4] = 0x9A;
    report[7..10].copy_from_slice(&[0x68, 0x4A, 0x8E]);
    report[10] = 0x0A;
    report[14] = 0xBB;
    report[15] = sub;
    report
}

fn query(device: &HidDevice) -> Option<(u8, bool)> {
    let mut battery = None;
    let mut charging = false;

    for sub in [SUB_BATTERY, SUB_CHARGING] {
        if device.write(&request(sub)).is_err() {
            return None;
        }

        let deadline = Instant::now() + Duration::from_millis(1200);
        let mut report = [0u8; 62];
        while Instant::now() < deadline {
            let left = (deadline - Instant::now()).as_millis().max(1) as i32;
            match device.read_timeout(&mut report, left) {
                Ok(0) => continue,
                Ok(len) if len >= 8 => {
                    if report[0] != 0x0A && report[2] == 0xBB && report[3] == sub {
                        if sub == SUB_BATTERY {
                            battery = Some(report[7]);
                        } else {
                            charging = report[4] == 1;
                        }
                        break;
                    }
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
    }

    let percent = battery?;
    if percent > 100 {
        return None;
    }
    Some((percent, charging))
}

pub fn scan(api: &HidApi) -> Vec<Observation> {
    for info in api.device_list() {
        if info.vendor_id() != VENDOR
            || info.product_id() != PRODUCT
            || info.interface_number() != INTERFACE
            || info.usage_page() != USAGE_PAGE
            || info.usage() != USAGE
        {
            continue;
        }
        let Ok(device) = api.open_path(info.path()) else {
            diag::event("hyperx:16ea", "could not open the vendor HID collection");
            continue;
        };
        let state = match query(&device) {
            Some((percent, charging)) => State::Battery { percent, charging },
            None => {
                diag::event("hyperx:16ea", "no battery reply (headset off or protocol error)");
                State::Unreachable
            }
        };
        return vec![Observation {
            id: format!("hyperx:{PRODUCT:04x}"),
            name: Some("HyperX Cloud Flight S".to_string()),
            kind: Some(Kind::Headset),
            state,
        }];
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_layout() {
        let report = request(SUB_BATTERY);
        assert_eq!(report[0], 0x06);
        assert_eq!(
            &report[..17],
            &[
                0x06, 0x00, 0x02, 0x00, 0x9A, 0x00, 0x00, 0x68, 0x4A, 0x8E, 0x0A, 0x00, 0x00, 0x00,
                0xBB, 0x02, 0x00
            ]
        );
        assert!(report[17..].iter().all(|&byte| byte == 0));
    }
}
