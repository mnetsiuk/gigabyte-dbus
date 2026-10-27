// Keyboard backlight: three zones on the ITE IT8298 keyboard (USB 1044:7a41). The colour commands
// go through the daemon so no user process needs the keyboard's USB device: its other interfaces
// and commands reach the firmware-update channel and flash erase. Only two commands are ever sent,
// on interface 3 of firmware 1.9.0.4 (bcdDevice 1904), the one the protocol was read from:
//   SET [0x08, zone, R, G, B, level, 0, 0]  the firmware saves it to its flash by itself
//   GET [0x88, zone, 0, 0, 0, 0, 0, 0], then read back [0x88, zone, R, G, B, level, ..]
// as 8-byte feature reports without a report ID.
use std::fs::{self, File, OpenOptions};
use std::io::{Error, ErrorKind, Result};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

const SET: u8 = 0x08;
const GET: u8 = 0x88;
const ALL: u8 = 0x0A; // the zone byte for all three zones
const LEVELS: [u8; 4] = [0x00, 0x18, 0x20, 0x32]; // brightness 0-3; the firmware knows no others
const BUF_LEN: usize = 9; // report ID 0, then the 8 bytes
// Every SET restarts the firmware's save countdown (~1 s), and the sector is erased once it runs
// out. A SET sooner than SAME_SAVE after the last one only restarts it; any other SET starts a new
// save and spends one of BURST saves, which come back one per REFILL.
const SAME_SAVE: Duration = Duration::from_millis(500);
const BURST: f64 = 20.0;
const REFILL: Duration = Duration::from_secs(180);

// HIDIOCSFEATURE(9) and HIDIOCGFEATURE(9) from linux/hidraw.h: _IOC(_IOC_WRITE | _IOC_READ, 'H', nr, 9).
const fn hidioc(nr: u32) -> u32 {
    3 << 30 | (BUF_LEN as u32) << 16 | (b'H' as u32) << 8 | nr
}
const HIDIOCSFEATURE: u32 = hidioc(0x06);
const HIDIOCGFEATURE: u32 = hidioc(0x07);

// Only set_packet and get_packet make one, so byte 0 is always SET or GET.
pub struct Packet([u8; 8]);

// Zone 1-3 (left, center, right) or 0 for all three, colour 0-255 each, brightness 0-3.
pub fn set_packet(zone: i32, red: i32, green: i32, blue: i32, brightness: i32) -> Option<Packet> {
    let zone = match zone {
        0 => ALL,
        1..=3 => zone as u8,
        _ => return None,
    };
    let level = *LEVELS.get(usize::try_from(brightness).ok()?)?;
    let [r, g, b] = [red, green, blue].map(|c| u8::try_from(c).ok());
    Some(Packet([SET, zone, r?, g?, b?, level, 0, 0]))
}

fn get_packet(zone: i32) -> Option<Packet> {
    (1..=3).contains(&zone).then_some(Packet([GET, zone as u8, 0, 0, 0, 0, 0, 0]))
}

// (red, green, blue, brightness 0-3) from the reply to a GET of `zone`.
fn parse_reply(zone: i32, reply: &[u8]) -> Option<(i32, i32, i32, i32)> {
    let [cmd, z, r, g, b, level, ..] = *reply else { return None };
    if cmd != GET || i32::from(z) != zone {
        return None;
    }
    let brightness = LEVELS.iter().position(|&l| l == level)?;
    Some((r.into(), g.into(), b.into(), brightness as i32))
}

fn has(dir: &Path, name: &str, value: &str) -> bool {
    fs::read_to_string(dir.join(name)).is_ok_and(|s| s.trim() == value)
}

// A /sys/class/hidraw entry of the keyboard's interface 3 on the known firmware: the HID device's
// parent is the USB interface, and its parent the USB device.
fn is_lighting(hidraw: &Path) -> bool {
    let Ok(hid) = fs::canonicalize(hidraw.join("device")) else { return false };
    let Some(interface) = hid.parent() else { return false };
    let Some(usb) = interface.parent() else { return false };
    has(interface, "bInterfaceNumber", "03")
        && has(usb, "idVendor", "1044")
        && has(usb, "idProduct", "7a41")
        && has(usb, "bcdDevice", "1904")
}

// Found per call, so a replug or a late start just works. Only the matching node is opened.
fn open() -> Result<File> {
    for entry in fs::read_dir("/sys/class/hidraw")? {
        let sys = entry?.path();
        if !is_lighting(&sys) {
            continue;
        }
        let Some(name) = sys.file_name() else { continue };
        let file = OpenOptions::new().read(true).write(true).open(Path::new("/dev").join(name))?;
        // While it is open the node's number cannot pass to another device: check it is still ours.
        let rdev = file.metadata()?.rdev();
        if has(&sys, "dev", &format!("{}:{}", libc::major(rdev), libc::minor(rdev))) && is_lighting(&sys) {
            return Ok(file);
        }
        return Err(Error::other(format!("/dev/{} changed while opening it", name.to_string_lossy())));
    }
    Err(Error::new(ErrorKind::NotFound, "no keyboard 1044:7a41 interface 3 with firmware 1904"))
}

fn feature(file: &File, request: u32, buf: &mut [u8; BUF_LEN]) -> Result<usize> {
    // SAFETY: buf is BUF_LEN bytes, the size the request encodes; the kernel touches no more.
    let n = unsafe { libc::ioctl(file.as_raw_fd(), request as _, buf.as_mut_ptr()) };
    usize::try_from(n).map_err(|_| Error::last_os_error())
}

// The kernel strips report ID 0 (buf[0]) and sends the 8 bytes after it.
fn send(file: &File, packet: &Packet) -> Result<()> {
    // Already true by construction; anything else in byte 0 may reach the firmware's flash commands.
    if !matches!(packet.0[0], SET | GET) {
        return Err(Error::other("refusing a keyboard command"));
    }
    let mut buf = [0; BUF_LEN];
    buf[1..].copy_from_slice(&packet.0);
    feature(file, HIDIOCSFEATURE, &mut buf).map(drop)
}

// Caps flash erases at ~BURST a burst and 1 per REFILL after that, whoever the caller is.
// ponytail: in memory, so a daemon restart refills it; restarting takes root, persist it in
// /var/lib/gigabyted if that changes.
struct Budget {
    saves: f64,
    refilled: Instant,
    last_set: Option<Instant>,
}

impl Budget {
    fn new(now: Instant) -> Self {
        Budget { saves: BURST, refilled: now, last_set: None }
    }

    // Whether a SET may go out at `now`; a new save is paid for here.
    fn allow(&mut self, now: Instant) -> bool {
        let earned = now.duration_since(self.refilled).as_secs_f64() / REFILL.as_secs_f64();
        self.saves = (self.saves + earned).min(BURST);
        self.refilled = now;
        let new_save = self.last_set.is_none_or(|t| now.duration_since(t) >= SAME_SAVE);
        if new_save {
            if self.saves < 1.0 {
                return false;
            }
            self.saves -= 1.0;
        }
        self.last_set = Some(now);
        true
    }
}

// One client at a time, so no one else's command lands between a GET and its reply.
static LOCK: Mutex<Option<Budget>> = Mutex::new(None);

pub fn set(packet: &Packet) -> Result<()> {
    let mut budget = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let now = Instant::now();
    if !budget.get_or_insert_with(|| Budget::new(now)).allow(now) {
        return Err(Error::other("too many lighting changes, each one wears the keyboard's flash; try again in a few minutes"));
    }
    send(&open()?, packet)
}

pub fn get(zone: i32) -> Result<(i32, i32, i32, i32)> {
    let packet = get_packet(zone).ok_or_else(|| Error::new(ErrorKind::InvalidInput, "zone must be 1-3"))?;
    let _lock = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let file = open()?;
    send(&file, &packet)?;
    let mut buf = [0; BUF_LEN];
    let n = feature(&file, HIDIOCGFEATURE, &mut buf)?.min(BUF_LEN);
    // Report ID 0 stays in buf[0]; the reply follows it.
    buf.get(1..n)
        .and_then(|reply| parse_reply(zone, reply))
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, format!("unexpected reply {:02x?}", &buf[1..n.max(1)])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers() {
        assert_eq!(HIDIOCSFEATURE, 0xC009_4806);
        assert_eq!(HIDIOCGFEATURE, 0xC009_4807);
    }

    // No input makes a packet with another command, zone or level.
    #[test]
    fn packets() {
        let edges = [i32::MIN, -256, -1, 0, 1, 2, 3, 4, 10, 0x0A, 0x13, 0x17, 0xFF, 256, i32::MAX];
        let colours = [i32::MIN, -1, 0, 1, 127, 255, 256, i32::MAX];
        for &zone in edges.iter().chain(&[-300, 300]) {
            for &c in &colours {
                for &brightness in &edges {
                    let ok = (0..=3).contains(&zone) && (0..=255).contains(&c) && (0..=3).contains(&brightness);
                    match set_packet(zone, c, c, c, brightness) {
                        Some(Packet(p)) => {
                            assert!(ok);
                            assert_eq!(p[0], SET);
                            assert!([1, 2, 3, ALL].contains(&p[1]));
                            assert!(LEVELS.contains(&p[5]));
                            assert_eq!(p[6..], [0, 0]);
                        }
                        None => assert!(!ok, "{zone} {c} {brightness}"),
                    }
                }
            }
            match get_packet(zone) {
                Some(Packet(p)) => assert_eq!(p, [GET, zone as u8, 0, 0, 0, 0, 0, 0]),
                None => assert!(!(1..=3).contains(&zone)),
            }
        }
        assert_eq!(set_packet(0, 255, 0, 128, 3).map(|p| p.0), Some([SET, ALL, 255, 0, 128, 0x32, 0, 0]));
        assert_eq!(set_packet(2, 1, 2, 3, 1).map(|p| p.0), Some([SET, 2, 1, 2, 3, 0x18, 0, 0]));
        assert!(set_packet(1, 0, 256, 0, 0).is_none() && set_packet(1, 0, 0, -1, 0).is_none());
    }

    #[test]
    fn budget() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut b = Budget::new(t0);
        for i in 0..BURST as u64 {
            assert!(b.allow(at(i * 1000))); // a second apart: a save each
        }
        // Spent, but SETs closer than SAME_SAVE only restart the countdown: free for 40 s.
        for i in 1..100 {
            assert!(b.allow(at(19_000 + i * 400)));
        }
        assert!(!b.allow(at(58_600 + 600))); // a new save: none left
        // One save back per REFILL.
        let later = 59_200 + REFILL.as_millis() as u64;
        assert!(b.allow(at(later)));
        assert!(!b.allow(at(later + 1000)));
    }

    #[test]
    fn replies() {
        assert_eq!(parse_reply(2, &[GET, 2, 255, 0, 128, 0x20, 0, 0]), Some((255, 0, 128, 2)));
        assert_eq!(parse_reply(1, &[GET, 1, 0, 0, 0, 0x00]), Some((0, 0, 0, 0)));
        assert_eq!(parse_reply(1, &[GET, 2, 0, 0, 0, 0x00, 0, 0]), None); // another zone
        assert_eq!(parse_reply(1, &[SET, 1, 0, 0, 0, 0x00, 0, 0]), None); // not a GET reply
        assert_eq!(parse_reply(1, &[GET, 1, 0, 0, 0, 0x19, 0, 0]), None); // unknown level
        assert_eq!(parse_reply(1, &[GET, 1, 0, 0, 0]), None); // short
    }
}
