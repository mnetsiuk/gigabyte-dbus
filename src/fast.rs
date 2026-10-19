// The daemon's own fan curve. The EC raises its fan duty by about 5% every 3 s, so a curve it runs
// takes ~45 s to reach full speed. Here the EC sits in Fixed mode and the daemon sets the duty from
// the same curve every half second, from the CPU's own sensor, so the fans react within a second.
use std::fs;
use std::io::{Error, ErrorKind, Result};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const STATE_FILE: &str = "/var/lib/gigabyted/fast-curve";
const NORMAL: i32 = 0;
const CURVE: i32 = 3;
const FIXED: i32 = 5;
const FULL_DUTY: i32 = 229;
const TICK: Duration = Duration::from_millis(500);
const HOLD_S: f64 = 3.0; // after the last rise, before the duty may fall
const DROP_PER_S: f64 = FULL_DUTY as f64 * 0.02; // then 2% of full duty per second

pub type Curve = Vec<(i32, i32)>; // (°C, duty 0-229)
pub type Shared = Arc<Mutex<Option<Curve>>>; // Some while the daemon drives the fans

fn invalid(what: String) -> Error {
    Error::new(ErrorKind::InvalidData, what)
}

// The EC's 15 points: temperature rising, duty never falling.
pub fn valid_curve(c: &[(i32, i32)]) -> bool {
    c.len() == 15
        && c.iter().all(|&(t, d)| (0..=100).contains(&t) && (0..=FULL_DUTY).contains(&d))
        && c.windows(2).all(|w| w[1].0 > w[0].0 && w[1].1 >= w[0].1)
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

fn read_attr(dev: &Path, name: &str) -> Result<i32> {
    read_int(&dev.join(name))
}

fn write_attr(dev: &Path, name: &str, value: i32) -> Result<()> {
    fs::write(dev.join(name), value.to_string())
}

// The curve the EC holds now, as the driver keeps it.
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

fn ec_hwmon(dev: &Path) -> Result<PathBuf> {
    fs::read_dir(dev.join("hwmon"))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.file_name().map_or(false, |n| n.to_string_lossy().starts_with("hwmon")))
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

// Back to the EC running the same curve at its own pace. The driver refuses Fixed -> Curve,
// so go through Normal.
pub fn hand_back(dev: &Path) {
    for mode in [NORMAL, CURVE] {
        if let Err(e) = write_attr(dev, "fan_mode", mode) {
            println!("Fast curve: handing the fans back: {e}");
        }
    }
}

pub fn forget() {
    if let Err(e) = fs::remove_file(STATE_FILE) {
        if e.kind() != ErrorKind::NotFound {
            println!("Fast curve: {STATE_FILE}: {e}");
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
            .find(|p| fs::read_to_string(p.join("name")).map_or(false, |n| n.trim() == "coretemp"))
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
        Ok(read_int(&self.core)?.max(read_int(&self.ec_cpu)?).max(read_int(&self.ec_gpu)?) / 1000)
    }
}

struct Control {
    sensors: Option<Sensors>,
    ramp: Option<Ramp>,
    written: i32,
}

impl Control {
    // One tick; Ok(false) when someone else changed the fan mode.
    fn tick(&mut self, dev: &Path, curve: &[(i32, i32)], now: f64, dt: f64) -> Result<bool> {
        if read_attr(dev, "fan_mode")? != FIXED {
            return Ok(false);
        }
        if self.sensors.is_none() {
            self.sensors = Some(Sensors::find(dev)?);
        }
        let s = self.sensors.as_ref().unwrap();
        let target = duty_for(curve, s.hottest()?);
        if self.ramp.is_none() {
            let running = read_int(&s.pwm).unwrap_or(target);
            self.ramp = Some(Ramp::new(running.max(target), now));
        }
        let duty = self.ramp.as_mut().unwrap().step(target, now, dt);
        if duty != self.written {
            write_attr(dev, "fan_custom_speed", duty)?;
            self.written = duty;
        }
        Ok(true)
    }
}

// The control loop, on its own thread for the daemon's lifetime.
pub fn run(dev: PathBuf, shared: Shared) {
    let start = Instant::now();
    let mut last = 0.0;
    let mut ctl = Control { sensors: None, ramp: None, written: -1 };
    loop {
        std::thread::sleep(TICK);
        let now = start.elapsed().as_secs_f64();
        let dt = now - last;
        last = now;
        let mut state = shared.lock().unwrap();
        let Some(curve) = state.clone() else {
            ctl.ramp = None;
            ctl.written = -1;
            continue;
        };
        match ctl.tick(&dev, &curve, now, dt) {
            Ok(true) => {}
            Ok(false) => {
                println!("Fast curve: the fan mode was changed elsewhere, stopping");
                *state = None;
                forget();
            }
            Err(e) => {
                println!("Fast curve: {e}; handing the fans back to the EC");
                *state = None;
                forget();
                hand_back(&dev);
                ctl.sensors = None;
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
