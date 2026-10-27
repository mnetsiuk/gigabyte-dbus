// The daemon's own fan curve. The EC raises its fan duty by about 5% every 3 s, so a curve it runs
// takes ~45 s to reach full speed. Here the EC sits in Fixed mode and the daemon sets the duty from
// the same curve every half second, from the CPU's own sensor, so the fans react within a second.
use std::fs;
use std::io::{Error, ErrorKind, Result};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

pub const STATE_FILE: &str = "/var/lib/gigabyted/fast-curve";
pub const FULL_DUTY: i32 = 229;
const NORMAL: i32 = 0;
const CURVE: i32 = 3;
const FIXED: i32 = 5;
const HOT: i32 = 90; // °C: full duty, whatever the curve says
const TICK: Duration = Duration::from_millis(500);
const HOLD_S: f64 = 3.0; // after the last rise, before the duty may fall
const DROP_PER_S: f64 = FULL_DUTY as f64 * 0.02; // then 2% of full duty per second
const GIVE_UP_AFTER: u32 = 4; // failed ticks in a row (2 s) before handing the fans back
const RESEND_EVERY: u32 = 20; // ticks (10 s): write the duty again in case the EC lost it

pub type Curve = Vec<(i32, i32)>; // (°C, duty 0-229)

pub struct Driving {
    pub curve: Curve,
    pub fresh: bool, // just (re)started: the loop starts its ramp over
}

// Some while the daemon drives the fans. Also serialises everything that touches the driver's
// shared fan curve index.
pub type Shared = Arc<Mutex<Option<Driving>>>;

// Nothing panics while holding the lock, so a poisoned one still holds a valid state.
pub fn lock(shared: &Shared) -> MutexGuard<'_, Option<Driving>> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

fn invalid(what: String) -> Error {
    Error::new(ErrorKind::InvalidData, what)
}

// The EC's 15 points: temperature rising, duty never falling, and full duty at the top, so no
// curve can hold the fans low when it is hot.
pub fn valid_curve(c: &[(i32, i32)]) -> bool {
    c.len() == 15
        && c.iter().all(|&(t, d)| (0..=100).contains(&t) && (0..=FULL_DUTY).contains(&d))
        && c.windows(2).all(|w| w[1].0 > w[0].0 && w[1].1 >= w[0].1)
        && c[14].1 == FULL_DUTY
}

// Duty for a temperature, linear between points, flat beyond the ends.
pub fn duty_for(c: &[(i32, i32)], temp: i32) -> i32 {
    let (first, last) = (c[0], c[c.len() - 1]);
    if temp <= first.0 {
        return first.1;
    }
    for w in c.windows(2) {
        let ((t0, d0), (t1, d1)) = (w[0], w[1]);
        if temp <= t1 {
            return d0 + (d1 - d0) * (temp - t0) / (t1 - t0);
        }
    }
    last.1
}

// Up at once; down only HOLD_S after the last rise, then DROP_PER_S, so short spikes do not make
// the fans pump.
pub struct Ramp {
    duty: f64,
    last_rise: f64,
}

impl Ramp {
    pub fn new(duty: i32, now: f64) -> Self {
        Ramp { duty: duty as f64, last_rise: now }
    }

    pub fn step(&mut self, target: i32, now: f64, dt: f64) -> i32 {
        let target = target as f64;
        if target >= self.duty {
            self.duty = target;
            self.last_rise = now;
        } else if now - self.last_rise >= HOLD_S {
            self.duty = (self.duty - DROP_PER_S * dt).max(target);
        }
        self.duty.round() as i32
    }
}

fn read_int(path: &Path) -> Result<i32> {
    let s = fs::read_to_string(path)?;
    s.trim().parse().map_err(|_| invalid(format!("{}: {s:?}", path.display())))
}

// °C from an hwmon millidegree file; anything outside 0-125 °C is a failed read, not a temperature.
fn read_temp(path: &Path) -> Result<i32> {
    let t = read_int(path)? / 1000;
    if (0..=125).contains(&t) {
        Ok(t)
    } else {
        Err(invalid(format!("{}: {t} °C", path.display())))
    }
}

fn read_attr(dev: &Path, name: &str) -> Result<i32> {
    read_int(&dev.join(name))
}

fn write_attr(dev: &Path, name: &str, value: i32) -> Result<()> {
    fs::write(dev.join(name), value.to_string())
}

// The curve the EC holds, as the driver keeps it. Call with the lock held.
pub fn read_curve(dev: &Path) -> Result<Curve> {
    (0..15)
        .map(|i| {
            write_attr(dev, "fan_curve_index", i)?;
            let s = fs::read_to_string(dev.join("fan_curve_data"))?;
            let v: Vec<i32> = s.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            match v[..] {
                [temp, duty] => Ok((temp, duty)),
                _ => Err(invalid(format!("fan_curve_data: {s:?}"))),
            }
        })
        .collect()
}

// Call with the lock held. Half written, a curve may fall as it gets hotter, so on failure the EC
// is taken off Curve mode, unless it is known to be in another one.
pub fn write_curve(dev: &Path, c: &[(i32, i32)]) -> Result<()> {
    let result = c.iter().enumerate().try_for_each(|(i, &(temp, duty))| {
        write_attr(dev, "fan_curve_index", i as i32)?;
        write_attr(dev, "fan_curve_data", duty << 8 | temp)
    });
    if result.is_err() && !read_attr(dev, "fan_mode").is_ok_and(|m| m != CURVE) {
        if let Err(e) = write_attr(dev, "fan_mode", NORMAL) {
            println!("Fan curve: half written, and leaving Curve mode failed: {e}");
        }
    }
    result
}

pub fn save(c: &Curve) -> Result<()> {
    let points: Vec<String> = c.iter().map(|(t, d)| format!("{t}:{d}")).collect();
    fs::write(STATE_FILE, points.join(" ") + "\n")
}

// The curve to resume with after a restart, if the daemon was driving the fans.
pub fn load() -> Option<Curve> {
    let s = fs::read_to_string(STATE_FILE).ok()?;
    let c: Curve = s
        .split_whitespace()
        .map(|p| {
            let (t, d) = p.split_once(':')?;
            Some((t.parse().ok()?, d.parse().ok()?))
        })
        .collect::<Option<_>>()?;
    valid_curve(&c).then_some(c)
}

pub fn forget() {
    if let Err(e) = fs::remove_file(STATE_FILE) {
        if e.kind() != ErrorKind::NotFound {
            println!("Fast curve: {STATE_FILE}: {e}");
        }
    }
}

fn ec_hwmon(dev: &Path) -> Result<PathBuf> {
    fs::read_dir(dev.join("hwmon"))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("hwmon")))
        .ok_or_else(|| invalid("no hwmon under the driver".into()))
}

// Fixed mode at the duty the fans run at now, so taking over causes no jump.
pub fn take_over(dev: &Path) -> Result<()> {
    if read_attr(dev, "fan_mode")? != FIXED {
        let duty = read_int(&ec_hwmon(dev)?.join("pwm1"))?.clamp(0, FULL_DUTY);
        write_attr(dev, "fan_custom_speed", duty)?;
        write_attr(dev, "fan_mode", FIXED)?;
    }
    Ok(())
}

// Back to the EC: Normal, then Curve if the EC holds a usable curve (a write that stopped halfway
// may have left a broken one; Normal is the firmware's own profile). Normal first because driver
// 0.2.0 refuses Fixed -> Curve. Call with the lock held.
pub fn hand_back(dev: &Path) -> Result<()> {
    write_attr(dev, "fan_mode", NORMAL)?;
    match read_curve(dev) {
        Ok(c) if valid_curve(&c) => write_attr(dev, "fan_mode", CURVE),
        _ => {
            println!("Fast curve: the EC curve is not usable, leaving the fans on Normal");
            Ok(())
        }
    }
}

struct Sensors {
    core: PathBuf, // coretemp package: the CPU's own, fastest sensor
    ec_cpu: PathBuf,
    ec_gpu: PathBuf,
    pwm: PathBuf,
}

impl Sensors {
    fn find(dev: &Path) -> Result<Self> {
        let ec = ec_hwmon(dev)?;
        let core = fs::read_dir("/sys/class/hwmon")?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| fs::read_to_string(p.join("name")).is_ok_and(|n| n.trim() == "coretemp"))
            .ok_or_else(|| invalid("no coretemp sensor".into()))?;
        Ok(Sensors {
            core: core.join("temp1_input"),
            ec_cpu: ec.join("temp1_input"),
            ec_gpu: ec.join("temp2_input"),
            pwm: ec.join("pwm1"),
        })
    }

    // The hottest reading; never less than what the EC itself would go by.
    fn hottest(&self) -> Result<i32> {
        Ok(read_temp(&self.core)?.max(read_temp(&self.ec_cpu)?).max(read_temp(&self.ec_gpu)?))
    }
}

// Every sensor the loop needs can be read now.
pub fn sensors_ok(dev: &Path) -> Result<()> {
    Sensors::find(dev)?.hottest().map(|_| ())
}

struct Control {
    sensors: Option<Sensors>,
    ramp: Option<Ramp>,
    written: i32,
    errors: u32,
    ticks: u32,
}

impl Control {
    fn reset(&mut self) {
        self.ramp = None;
        self.written = -1;
        self.errors = 0;
    }

    // One tick: Ok(None) while driving, Ok(Some(mode)) when another fan mode was set meanwhile.
    fn tick(&mut self, dev: &Path, curve: &[(i32, i32)], now: f64, dt: f64) -> Result<Option<i32>> {
        let mode = read_attr(dev, "fan_mode")?;
        if mode != FIXED {
            return Ok(Some(mode));
        }
        if self.sensors.is_none() {
            self.sensors = Some(Sensors::find(dev)?);
        }
        let Some(s) = &self.sensors else { return Ok(None) };
        let temp = s.hottest()?;
        let target = if temp >= HOT { FULL_DUTY } else { duty_for(curve, temp) };
        // Start from the duty the fans run at, so taking over causes no jump down.
        let ramp = self
            .ramp
            .get_or_insert_with(|| Ramp::new(read_int(&s.pwm).unwrap_or(target).clamp(0, FULL_DUTY).max(target), now));
        let duty = ramp.step(target, now, dt).clamp(0, FULL_DUTY);
        self.ticks = self.ticks.wrapping_add(1);
        if duty != self.written || self.ticks.is_multiple_of(RESEND_EVERY) {
            write_attr(dev, "fan_custom_speed", duty)?;
            self.written = duty;
        }
        Ok(None)
    }
}

// The control loop, on its own thread for the daemon's lifetime. The state is only dropped once
// the fans are confirmed back with the EC; until then it keeps trying, and the state file stays
// for ExecStopPost.
pub fn run(dev: PathBuf, shared: Shared) {
    let start = Instant::now();
    let mut last = 0.0;
    let mut ctl = Control { sensors: None, ramp: None, written: -1, errors: 0, ticks: 0 };
    loop {
        std::thread::sleep(TICK);
        let now = start.elapsed().as_secs_f64();
        let dt = now - last;
        last = now;
        let mut state = lock(&shared);
        let Some(driving) = state.as_mut() else {
            ctl.reset();
            continue;
        };
        if driving.fresh {
            driving.fresh = false;
            ctl.reset();
        }
        let curve = driving.curve.clone();
        match ctl.tick(&dev, &curve, now, dt) {
            Ok(None) => ctl.errors = 0,
            Ok(Some(mode)) => {
                // Driver 0.2.0 takes Fixed -> Curve as done while the EC stays in Fixed.
                if mode == CURVE {
                    if let Err(e) = hand_back(&dev) {
                        println!("Fast curve: fan mode 3 was set elsewhere, handing back failed: {e}");
                        continue;
                    }
                }
                println!("Fast curve: fan mode {mode} was set elsewhere, stopping");
                *state = None;
                forget();
            }
            Err(e) => {
                ctl.errors += 1;
                ctl.sensors = None; // the driver may have been reloaded
                if ctl.errors < GIVE_UP_AFTER {
                    continue;
                }
                match hand_back(&dev) {
                    Ok(()) => {
                        println!("Fast curve: {e}; handed the fans back to the EC");
                        *state = None;
                        forget();
                    }
                    Err(h) if ctl.errors == GIVE_UP_AFTER || ctl.errors.is_multiple_of(120) => {
                        println!("Fast curve: {e}; handing the fans back failed ({h}), retrying")
                    }
                    Err(_) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FACTORY: [(i32, i32); 15] = [
        (0, 57), (50, 68), (53, 80), (56, 91), (59, 103), (62, 114), (65, 125), (68, 137),
        (71, 148), (74, 160), (77, 171), (80, 183), (83, 194), (86, 206), (89, 229),
    ];

    #[test]
    fn curve() {
        assert!(valid_curve(&FACTORY));
        assert!(!valid_curve(&FACTORY[..14]));
        let mut falling = FACTORY;
        falling[7].1 = 60;
        assert!(!valid_curve(&falling));
        let mut capped = FACTORY; // may not stay below full duty at the top
        capped[14].1 = 206;
        assert!(!valid_curve(&capped));
        assert_eq!(duty_for(&FACTORY, 20), 61); // between 0 °C and 50 °C
        assert_eq!(duty_for(&FACTORY, 50), 68);
        assert_eq!(duty_for(&FACTORY, 84), 198);
        assert_eq!(duty_for(&FACTORY, 99), 229);
        assert_eq!(duty_for(&FACTORY, -5), 57);
    }

    #[test]
    fn ramp() {
        let mut r = Ramp::new(57, 0.0);
        assert_eq!(r.step(183, 0.5, 0.5), 183); // up at once
        assert_eq!(r.step(57, 1.0, 0.5), 183); // a dip right after is ignored
        assert_eq!(r.step(57, 3.4, 0.5), 183); // still within the hold
        assert_eq!(r.step(57, 4.0, 0.5), 181); // then falls 2%/s
        assert_eq!(r.step(57, 4.5, 0.5), 178);
        assert_eq!(r.step(190, 5.0, 0.5), 190); // and jumps up again
        for i in 0..200 {
            r.step(57, 10.0 + i as f64 * 0.5, 0.5);
        }
        assert_eq!(r.step(57, 200.0, 0.5), 57); // never below the target
    }
}
