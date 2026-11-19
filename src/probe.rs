//! Device probe: a passive dump of every USB HID collection, for working out the battery
//! protocol of a dongle we don't have. Users run it from the menu and paste the file into a
//! device report.
//!
//! It never writes to a device. For each collection it records the USB ids and product name
//! (no serial numbers or paths), the report descriptor, one reading of each feature report,
//! and the input reports that arrive while it listens, with the bytes that changed. A
//! battery level usually shows up as a byte that holds steady, or drifts by 1, and matches
//! what the vendor app says.
//!
//! Plain mouse and keyboard collections are skipped (Windows doesn't let apps read them
//! anyway). Vendor collections on keyboards can carry key presses, so the user is asked not
//! to type while it runs.

use hidapi::{BusType, DeviceInfo, HidApi, HidDevice};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub const LISTEN: Duration = Duration::from_secs(60);
/// GitHub issue fields hold ~65k characters.
const MAX_LEN: usize = 60_000;
/// Distinct samples kept per report, beyond first and last.
const SAMPLES: usize = 6;

pub fn path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("taskbar-battery").join("probe.txt"))
}

/// Runs the probe and saves it to `path()`. Returns where it was saved.
pub fn run_to_file(listen: Duration) -> Result<PathBuf, String> {
    let api = HidApi::new().map_err(|e| format!("hidapi failed to start: {e}"))?;
    let text = run(&api, listen);
    let path = path().ok_or("LOCALAPPDATA isn't set")?;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&path, text).map_err(|e| e.to_string())?;
    Ok(path)
}

struct Collection {
    page: u16,
    usage: u16,
    dev: Option<HidDevice>,
    out: String,
    reports: BTreeMap<(u8, usize), Seen>,
}

#[derive(Default)]
struct Seen {
    count: u32,
    first: Vec<u8>,
    last: Vec<u8>,
    changed: Vec<bool>,
    samples: Vec<(Duration, Vec<u8>)>,
}

pub fn run(api: &HidApi, listen: Duration) -> String {
    let mut infos: Vec<&DeviceInfo> = api.device_list().filter(|d| matches!(d.bus_type(), BusType::Usb)).collect();
    infos.sort_by_key(|d| (d.vendor_id(), d.product_id(), d.interface_number(), d.usage_page(), d.usage()));

    let mut text = format!(
        "{}\nProbe: listened {} s to every USB HID device, without writing to any.\n\
         Holds: USB ids, manufacturer/product names, HID descriptors, one read of each feature report,\n\
         and the raw input reports sent meanwhile. No serial numbers, paths or Bluetooth devices.\n\
         Check it before posting: if you typed during the probe, a keyboard's vendor reports may show it.\n",
        crate::diag::header(),
        listen.as_secs()
    );
    let mut cols = Vec::new();
    let mut device = None;
    for d in infos {
        let (vid, pid) = (d.vendor_id(), d.product_id());
        let mut out = String::new();
        if device != Some((vid, pid)) {
            device = Some((vid, pid));
            let _ = writeln!(
                out,
                "\n== {vid:04X}:{pid:04X} {:?} {:?} (release {:04X})",
                d.manufacturer_string().unwrap_or("?"),
                d.product_string().unwrap_or("?"),
                d.release_number()
            );
        }
        let _ = write!(out, "  if {} page {:04X} usage {:04X}", d.interface_number(), d.usage_page(), d.usage());
        let mut col = Collection {
            page: d.usage_page(),
            usage: d.usage(),
            dev: None,
            out,
            reports: BTreeMap::new(),
        };
        if is_plain_input(col.page, col.usage) {
            col.out.push_str("  (mouse/keyboard, skipped)\n");
        } else {
            col.out.push('\n');
            match d.open_device(api) {
                Ok(dev) => {
                    // Reading a collection without input reports fails, so only listen on the others.
                    if describe(&dev, &mut col.out) {
                        col.dev = Some(dev);
                    }
                }
                Err(e) => {
                    let _ = writeln!(col.out, "    open failed: {e}");
                }
            }
        }
        cols.push(col);
    }

    listen_all(&mut cols, listen);

    for col in &cols {
        text.push_str(&col.out);
        if col.dev.is_some() {
            if col.reports.is_empty() {
                text.push_str("    no input reports\n");
            }
            for ((id, len), s) in &col.reports {
                text.push_str(&summary(*id, *len, s));
            }
        }
    }
    if text.len() > MAX_LEN {
        let mut cut = MAX_LEN;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n[truncated]\n");
    }
    text
}

/// The report descriptor of an open collection.
pub fn descriptor(dev: &HidDevice) -> Option<Vec<u8>> {
    let mut buf = [0u8; hidapi::MAX_REPORT_DESCRIPTOR_SIZE];
    dev.get_report_descriptor(&mut buf).ok().map(|n| buf[..n].to_vec())
}

/// Whether a report descriptor declares any output report.
pub fn has_output(desc: &[u8]) -> bool {
    report_layout(desc).keys().any(|(k, _)| *k == Kind::Output)
}

/// Whether the descriptor declares output report `id`.
pub fn has_output_id(desc: &[u8], id: u8) -> bool {
    report_layout(desc).contains_key(&(Kind::Output, id))
}

/// Length in bytes (id included) of feature report `id`, from a report descriptor.
pub fn feature_len(desc: &[u8], id: u8) -> Option<usize> {
    report_layout(desc).get(&(Kind::Feature, id)).map(|bits| bits.div_ceil(8) as usize)
}

fn is_plain_input(page: u16, usage: u16) -> bool {
    page == 0x01 && matches!(usage, 0x02 | 0x06 | 0x07) || page == 0x0D
}

/// Writes the descriptor, its report layout, and one reading of each feature report.
/// Returns whether the collection may send input reports (true when the layout is unknown).
fn describe(dev: &HidDevice, out: &mut String) -> bool {
    let mut buf = [0u8; hidapi::MAX_REPORT_DESCRIPTOR_SIZE];
    let desc = match dev.get_report_descriptor(&mut buf) {
        Ok(n) => &buf[..n],
        Err(e) => {
            let _ = writeln!(out, "    descriptor unavailable: {e}");
            return true;
        }
    };
    let _ = writeln!(out, "    descriptor ({} bytes): {}", desc.len(), hex(desc));
    let layout = report_layout(desc);
    if layout.is_empty() {
        return true;
    }
    let has_input = layout.keys().any(|(k, _)| *k == Kind::Input);
    let list: Vec<String> = layout.iter().map(|((kind, id), bits)| format!("{} {id:02X} ({} bytes)", kind.name(), bits.div_ceil(8))).collect();
    let _ = writeln!(out, "    reports: {}", list.join(", "));

    // Windows wants a buffer as long as the longest feature report, plus the id byte that
    // reports without an id still get.
    let features = || layout.iter().filter(|((k, _), _)| *k == Kind::Feature);
    let Some(len) = features().map(|((_, id), bits)| bits.div_ceil(8) as usize + usize::from(*id == 0)).max() else {
        return has_input;
    };
    for ((_, id), _) in features() {
        let mut r = vec![0u8; len];
        r[0] = *id;
        match dev.get_feature_report(&mut r) {
            Ok(n) => {
                let _ = writeln!(out, "    feature {id:02X}: {}", hex(&r[..n.min(r.len())]));
            }
            Err(e) => {
                let _ = writeln!(out, "    feature {id:02X}: failed: {e}");
            }
        }
    }
    has_input
}

fn listen_all(cols: &mut [Collection], listen: Duration) {
    let start = Instant::now();
    let mut buf = [0u8; 1024];
    while start.elapsed() < listen {
        for col in cols.iter_mut() {
            let Some(dev) = &col.dev else { continue };
            for _ in 0..64 {
                match dev.read_timeout(&mut buf, 0) {
                    Ok(0) => break,
                    Ok(n) => record(&mut col.reports, &buf[..n], start.elapsed()),
                    Err(e) => {
                        let _ = writeln!(col.out, "    read failed: {e}");
                        col.dev = None;
                        break;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn record(reports: &mut BTreeMap<(u8, usize), Seen>, r: &[u8], at: Duration) {
    let s = reports.entry((r[0], r.len())).or_default();
    if s.count == 0 {
        s.first = r.to_vec();
        s.changed = vec![false; r.len()];
    } else if s.last != r {
        for (i, (a, b)) in s.last.iter().zip(r).enumerate() {
            s.changed[i] |= a != b;
        }
        if s.samples.len() < SAMPLES && !s.samples.iter().any(|(_, x)| x == r) {
            s.samples.push((at, r.to_vec()));
        }
    }
    s.count += 1;
    s.last = r.to_vec();
}

fn summary(id: u8, len: usize, s: &Seen) -> String {
    let changed: Vec<String> = s.changed.iter().enumerate().filter(|(_, c)| **c).map(|(i, _)| i.to_string()).collect();
    let mut out = format!("    input {id:02X} ({len} bytes) x{}", s.count);
    if changed.is_empty() {
        let _ = writeln!(out, ", never changed: {}", hex(&s.first));
    } else {
        let _ = writeln!(out, ", changing bytes [{}]", changed.join(" "));
        let _ = writeln!(out, "      first   {}", hex(&s.first));
        for (at, r) in &s.samples {
            let _ = writeln!(out, "      +{:>5.1}s {}", at.as_secs_f32(), hex(r));
        }
        let _ = writeln!(out, "      last    {}", hex(&s.last));
    }
    out
}

fn hex(b: &[u8]) -> String {
    // Trailing zero padding says nothing; keep it short.
    let end = b.iter().rposition(|x| *x != 0).map_or(1, |i| i + 1).min(b.len());
    let mut s: Vec<String> = b[..end].iter().map(|x| format!("{x:02x}")).collect();
    if end < b.len() {
        s.push(format!("(+{} zero)", b.len() - end));
    }
    s.join(" ")
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Kind {
    Input,
    Output,
    Feature,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Input => "input",
            Kind::Output => "output",
            Kind::Feature => "feature",
        }
    }
}

/// Report ids and their sizes in bits, from a HID report descriptor.
fn report_layout(desc: &[u8]) -> BTreeMap<(Kind, u8), u32> {
    let mut out = BTreeMap::new();
    let (mut id, mut size, mut count) = (0u8, 0u32, 0u32);
    let mut stack = Vec::new();
    let mut i = 0;
    while i < desc.len() {
        let prefix = desc[i];
        if prefix == 0xFE {
            // Long item: size byte, tag byte, data.
            i += 3 + desc.get(i + 1).copied().unwrap_or(0) as usize;
            continue;
        }
        let n = [0, 1, 2, 4][(prefix & 3) as usize];
        let data = desc.get(i + 1..i + 1 + n).unwrap_or(&[]);
        let value = data.iter().rev().fold(0u32, |v, b| v << 8 | u32::from(*b));
        match (prefix >> 2) & 3 {
            0 => {
                let kind = match prefix >> 4 {
                    0x8 => Some(Kind::Input),
                    0x9 => Some(Kind::Output),
                    0xB => Some(Kind::Feature),
                    _ => None,
                };
                if let Some(kind) = kind {
                    *out.entry((kind, id)).or_insert(0) += size * count;
                }
            }
            1 => match prefix >> 4 {
                0x7 => size = value,
                0x8 => id = value as u8,
                0x9 => count = value,
                0xA => stack.push((id, size, count)),
                0xB => (id, size, count) = stack.pop().unwrap_or((id, size, count)),
                _ => {}
            },
            _ => {}
        }
        i += 1 + n;
    }
    // A report with an id carries it as its first byte.
    for ((_, id), bits) in out.iter_mut() {
        if *id != 0 {
            *bits += 8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_of_vendor_descriptor() {
        // Usage page FF90, report 0x55: 63 bytes in and out (MCHOSE-style), then
        // report 0x05: one 8-byte feature report.
        let desc = [
            0x06, 0x90, 0xFF, 0x09, 0x01, 0xA1, 0x01, // page FF90, usage 1, collection
            0x85, 0x55, 0x75, 0x08, 0x95, 0x3F, 0x09, 0x01, 0x81, 0x02, // id 55, 8 bits x63, input
            0x09, 0x01, 0x91, 0x02, // output, same size
            0x85, 0x05, 0x95, 0x08, 0x09, 0x02, 0xB1, 0x02, // id 05, 8 bits x8, feature
            0xC0,
        ];
        let l = report_layout(&desc);
        assert_eq!(l.get(&(Kind::Input, 0x55)), Some(&(64 * 8)));
        assert_eq!(l.get(&(Kind::Output, 0x55)), Some(&(64 * 8)));
        assert_eq!(l.get(&(Kind::Feature, 0x05)), Some(&(9 * 8)));
        assert_eq!(l.len(), 3);
    }

    #[test]
    fn changing_bytes_are_tracked() {
        let mut r = BTreeMap::new();
        record(&mut r, &[0x03, 0x2c, 0x40, 0x01, 0x36], Duration::ZERO);
        record(&mut r, &[0x03, 0x2c, 0x40, 0x01, 0x36], Duration::from_secs(2));
        record(&mut r, &[0x03, 0x2c, 0x40, 0x01, 0x37], Duration::from_secs(4));
        let s = &r[&(0x03, 5)];
        assert_eq!(s.count, 3);
        assert_eq!(s.changed, [false, false, false, false, true]);
        assert!(summary(0x03, 5, s).contains("changing bytes [4]"));
    }

    #[test]
    fn hex_trims_zero_padding() {
        assert_eq!(hex(&[0x55, 0x65, 0x32, 0x02, 0, 0, 0]), "55 65 32 02 (+3 zero)");
        assert_eq!(hex(&[0, 0]), "00 (+1 zero)");
    }
}
