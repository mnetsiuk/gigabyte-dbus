use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use zbus::{dbus_interface, fdo};

use crate::fast;
use crate::fast::FULL_DUTY;

pub struct CtrlPlatform {
    pub path: PathBuf,
    pub fast: fast::Shared,
}

pub const CPU_LIMITS_FILE: &str = "/var/lib/gigabyted/cpu-limits";
const RAPL_ZONES: [&str; 2] = ["/sys/class/powercap/intel-rapl:0", "/sys/class/powercap/intel-rapl-mmio:0"];
const MIN_FIXED: i32 = 57; // 25%: a fixed speed may not stop the fans
const NORMAL: i32 = 0;
const CURVE: i32 = 3;
const FIXED: i32 = 5;

// Any local user may call us, so only accept sane package limits: 5 W <= PL1 <= PL2 <= 135 W, 1-128 s window.
pub fn valid_cpu_limits(pl1_mw: i32, pl2_mw: i32, tau_s: i32) -> bool {
    (5_000..=135_000).contains(&pl1_mw) && (pl1_mw..=135_000).contains(&pl2_mw) && (1..=128).contains(&tau_s)
}

// Firmware programs both the MSR and the MMIO RAPL interface and the lower one wins, so set both.
pub fn apply_cpu_limits(pl1_mw: i32, pl2_mw: i32, tau_s: i32) -> std::io::Result<()> {
    for zone in RAPL_ZONES.iter().filter(|z| std::path::Path::new(z).exists()) {
        fs::write(format!("{zone}/constraint_0_power_limit_uw"), (pl1_mw as i64 * 1000).to_string())?;
        fs::write(format!("{zone}/constraint_1_power_limit_uw"), (pl2_mw as i64 * 1000).to_string())?;
        fs::write(format!("{zone}/constraint_0_time_window_us"), (tau_s as i64 * 1_000_000).to_string())?;
    }
    Ok(())
}

// The driver sends these straight to the EC without checking them.
pub fn valid_fixed_speed(duty: i32) -> bool {
    (MIN_FIXED..=FULL_DUTY).contains(&duty)
}

// Fixed mode runs whatever fan_custom_speed holds, which the fast curve or take_over may have
// left below the minimum, even at 0: switch to it at no less than MIN_FIXED.
pub fn fixed_mode_speed(stored: i32) -> i32 {
    stored.clamp(MIN_FIXED, FULL_DUTY)
}

pub fn valid_curve_point(duty: i32, temp: i32) -> bool {
    (0..=FULL_DUTY).contains(&duty) && (0..=100).contains(&temp)
}

fn refuse(name: &str, value: impl std::fmt::Debug) -> i32 {
    println!("{name}: refusing {value:?}");
    -1
}

// Another fan mode or speed took effect, so the daemon no longer drives the fans.
fn stop_driving(state: &mut Option<fast::Driving>) {
    *state = None;
    fast::forget();
}

impl CtrlPlatform {
    pub fn new() -> Result<Self, ()> {
        let mut enumerator = udev::Enumerator::new().map_err(|e| println!("udev: {e}"))?;
        enumerator.match_subsystem("platform").map_err(|e| println!("udev: {e}"))?;
        enumerator.match_sysname("aorus_laptop").map_err(|e| println!("udev: {e}"))?;
        let device = enumerator
            .scan_devices()
            .map_err(|e| println!("Could not scan devices: {e}"))?
            .next()
            .ok_or(())?;
        Ok(Self {
            path: device.syspath().to_path_buf(),
            fast: Arc::new(Mutex::new(None)),
        })
    }

    fn fast_state(&self) -> MutexGuard<'_, Option<fast::Driving>> {
        fast::lock(&self.fast)
    }

    // After a restart: drive the fans again if the daemon did before.
    pub fn resume_fast_curve(&self) {
        let Some(curve) = fast::load() else {
            fast::forget(); // missing or unusable
            return;
        };
        // Started on the driver's bind event, the daemon can be a moment ahead of its attributes.
        for _ in 0..20 {
            if self.path.join("fan_mode").exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let mut state = self.fast_state();
        match fast::sensors_ok(&self.path).and_then(|_| fast::take_over(&self.path)) {
            Ok(()) => *state = Some(fast::Driving { curve, fresh: true }),
            Err(e) => {
                println!("Fast curve: could not resume: {e}");
                fast::forget();
            }
        }
    }

    // A driver attribute; -1 instead of dying when the driver refuses the value.
    fn write(&self, name: &str, value: i32) -> i32 {
        match fs::write(self.path.join(name), value.to_string()) {
            Ok(()) => 0,
            Err(e) => {
                println!("{name} = {value}: {e}");
                -1
            }
        }
    }

    // A driver attribute as at least `count` whitespace-separated numbers.
    fn read(&self, name: &str, count: usize) -> fdo::Result<Vec<i32>> {
        let text = fs::read_to_string(self.path.join(name)).map_err(|e| fdo::Error::Failed(format!("{name}: {e}")))?;
        let values: Vec<i32> = text.split_whitespace().map_while(|x| x.parse().ok()).collect();
        if values.len() < count {
            return Err(fdo::Error::Failed(format!("{name}: unexpected {text:?}")));
        }
        Ok(values)
    }

    fn read_one(&self, name: &str) -> fdo::Result<i32> {
        Ok(self.read(name, 1)?[0])
    }
}

// The driver checks fan_mode (0-5), charge_mode (0-1), charge_limit (60-100), gpu_boost (0-3) and
// the curve index (0-14) itself; a refused value comes back as -1. Methods that change the fans hold
// the fast-curve lock: the curve thread does not write meanwhile, and keeps driving the fans when
// the change fails.
#[dbus_interface(name = "com.gigabyte.Platform")]
impl CtrlPlatform {
    fn set_fan_mode(&self, value: i32) -> i32 {
        if !(0..=5).contains(&value) {
            return refuse("fan_mode", value);
        }
        let mut state = self.fast_state();
        // The driver refuses Fixed/AutoMax -> Curve yet reports success, so go through Normal.
        if value == CURVE && self.read_one("fan_mode").is_ok_and(|m| m > CURVE) && self.write("fan_mode", NORMAL) != 0 {
            return -1;
        }
        if value == FIXED {
            match self.read_one("fan_custom_speed") {
                Ok(stored) if self.write("fan_custom_speed", fixed_mode_speed(stored)) == 0 => {}
                Ok(_) => return -1,
                Err(e) => {
                    println!("SetFanMode: {e}");
                    return -1;
                }
            }
        }
        let result = self.write("fan_mode", value);
        if result == 0 {
            stop_driving(&mut state);
        }
        result
    }
    fn set_fan_speed(&self, value: i32) -> i32 {
        if !valid_fixed_speed(value) {
            return refuse("fan_custom_speed", value);
        }
        let mut state = self.fast_state();
        let result = self.write("fan_custom_speed", value);
        if result == 0 {
            stop_driving(&mut state);
        }
        result
    }
    fn set_charge_mode(&self, value: i32) -> i32 {
        self.write("charge_mode", value)
    }
    fn set_charge_limit(&self, value: i32) -> i32 {
        self.write("charge_limit", value)
    }
    // One point at a time through the driver's shared index; SetFanCurve is safer.
    fn set_fan_curve_index(&self, value: i32) -> i32 {
        let _state = self.fast_state();
        self.write("fan_curve_index", value)
    }
    fn set_fan_curve_data(&self, speed: i32, temp: i32) -> i32 {
        if !valid_curve_point(speed, temp) {
            return refuse("fan_curve_data", (speed, temp));
        }
        let _state = self.fast_state();
        self.write("fan_curve_data", speed << 8 | temp)
    }
    // All 15 (°C, duty 0-229) points at once, under the lock so no other client moves the shared
    // index in between, then read back. The top point must be full duty. A curve the daemon runs
    // follows the new one at once.
    fn set_fan_curve(&self, points: Vec<(i32, i32)>) -> i32 {
        if !fast::valid_curve(&points) {
            return refuse("SetFanCurve", &points);
        }
        let mut state = self.fast_state();
        if let Err(e) = fast::write_curve(&self.path, &points) {
            println!("SetFanCurve: {e}");
            // Half written, it may fall as it gets hotter: do not let the EC run it.
            if self.read_one("fan_mode").is_ok_and(|m| m == CURVE) {
                self.write("fan_mode", NORMAL);
            }
            return -1;
        }
        match fast::read_curve(&self.path) {
            Ok(back) if back == points => {}
            other => {
                println!("SetFanCurve: read back {other:?}");
                return -1;
            }
        }
        if let Some(driving) = state.as_mut() {
            driving.curve = points;
            if let Err(e) = fast::save(&driving.curve) {
                println!("SetFanCurve: could not remember it: {e}");
            }
        }
        0
    }
    fn set_gpu_boost(&self, value: i32) -> i32 {
        self.write("gpu_boost", value)
    }
    // get methods
    fn get_fan_mode(&self) -> fdo::Result<i32> {
        self.read_one("fan_mode")
    }
    fn get_fan_speed(&self) -> fdo::Result<i32> {
        self.read_one("fan_custom_speed")
    }
    fn get_charge_mode(&self) -> fdo::Result<i32> {
        self.read_one("charge_mode")
    }
    fn get_charge_limit(&self) -> fdo::Result<i32> {
        self.read_one("charge_limit")
    }
    fn get_fan_curve_index(&self) -> fdo::Result<i32> {
        let _state = self.fast_state();
        self.read_one("fan_curve_index")
    }
    // (temperature, speed) at the current index
    fn get_fan_curve_data(&self) -> fdo::Result<(i32, i32)> {
        let _state = self.fast_state();
        let v = self.read("fan_curve_data", 2)?;
        Ok((v[0], v[1]))
    }
    // All 15 (°C, duty) points.
    fn get_fan_curve(&self) -> fdo::Result<Vec<(i32, i32)>> {
        let _state = self.fast_state();
        fast::read_curve(&self.path).map_err(|e| fdo::Error::Failed(format!("fan curve: {e}")))
    }
    fn get_battery_cycle(&self) -> fdo::Result<i32> {
        self.read_one("battery_cycle")
    }
    fn get_usb_s3_toggle(&self) -> fdo::Result<i32> {
        self.read_one("usb_charge_s3_toggle")
    }
    fn get_usb_s4_toggle(&self) -> fdo::Result<i32> {
        self.read_one("usb_charge_s4_toggle")
    }
    fn get_gpu_boost(&self) -> fdo::Result<i32> {
        self.read_one("gpu_boost")
    }
    // (day, hour, minute, month, year)
    fn get_power_on_time(&self) -> fdo::Result<(i32, i32, i32, i32, i32)> {
        let v = self.read("power_on_time", 5)?;
        Ok((v[0], v[1], v[2], v[3], v[4]))
    }
    // (version, lux low, medium, high)
    // TODO: Check for older models, as they don't use the same format
    fn get_light_sensor(&self) -> fdo::Result<(i32, i32, i32, i32)> {
        let v = self.read("light_sensor", 4)?;
        Ok((v[0], v[1], v[2], v[3]))
    }
    // PL1/PL2 in milliwatts and the PL1 window in seconds; remembered and restored on the next start.
    fn set_cpu_power_limits(&self, pl1_mw: i32, pl2_mw: i32, tau_s: i32) -> i32 {
        if !valid_cpu_limits(pl1_mw, pl2_mw, tau_s) {
            return -1;
        }
        if let Err(e) = apply_cpu_limits(pl1_mw, pl2_mw, tau_s) {
            println!("SetCpuPowerLimits: {e}");
            return -1;
        }
        if let Err(e) = fs::write(CPU_LIMITS_FILE, format!("{pl1_mw} {pl2_mw} {tau_s}\n")) {
            println!("SetCpuPowerLimits: could not remember limits: {e}");
        }
        0
    }
    // NVIDIA Dynamic Boost is run by nvidia-powerd: enable and start it, or stop and disable it,
    // so the choice also holds after a reboot. Queued (--no-block) without a daemon reload, so a
    // slow start or stop does not stall every other caller of this single-threaded interface.
    fn set_dynamic_boost(&self, on: i32) -> i32 {
        let verb = if on != 0 { "enable" } else { "disable" };
        let args = ["--no-block", "--no-reload", verb, "--now", "nvidia-powerd.service"];
        match std::process::Command::new("systemctl").args(args).status() {
            Ok(status) if status.success() => 0,
            Ok(status) => {
                println!("SetDynamicBoost: systemctl {verb} {status}");
                -1
            }
            Err(e) => {
                println!("SetDynamicBoost: {e}");
                -1
            }
        }
    }
    // 1: run the curve the EC holds from the daemon, from the CPU's own sensor (see fast.rs);
    // calling it again re-reads the curve. 0: hand the fans back to the EC.
    fn set_fast_curve(&self, on: i32) -> i32 {
        let mut state = self.fast_state();
        if on == 0 {
            if state.is_some() {
                if let Err(e) = fast::hand_back(&self.path) {
                    println!("SetFastCurve: {e}; still driving the fans");
                    return -1;
                }
            }
            stop_driving(&mut state);
            return 0;
        }
        let curve = match fast::read_curve(&self.path) {
            Ok(c) if fast::valid_curve(&c) => c,
            Ok(c) => return refuse("SetFastCurve, EC curve", c),
            Err(e) => {
                println!("SetFastCurve: {e}");
                return -1;
            }
        };
        if let Err(e) = fast::sensors_ok(&self.path) {
            println!("SetFastCurve: {e}");
            return -1;
        }
        // Remember it first: ExecStopPost and the GUI go by the file.
        if let Err(e) = fast::save(&curve) {
            println!("SetFastCurve: {e}");
            return -1;
        }
        if let Err(e) = fast::take_over(&self.path) {
            println!("SetFastCurve: {e}");
            match state.as_ref() {
                Some(driving) => drop(fast::save(&driving.curve)),
                None => fast::forget(),
            }
            return -1;
        }
        *state = Some(fast::Driving { curve, fresh: true });
        0
    }
    fn get_fast_curve(&self) -> i32 {
        self.fast_state().is_some() as i32
    }
    fn get_fan_pwm(&self) -> fdo::Result<i32> {
        self.read_one("fan_pwm")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_limit_bounds() {
        assert!(valid_cpu_limits(55_000, 115_000, 56));
        assert!(valid_cpu_limits(15_000, 30_000, 56));
        assert!(!valid_cpu_limits(70_000, 60_000, 56)); // PL2 below PL1
        assert!(!valid_cpu_limits(4_000, 30_000, 56));
        assert!(!valid_cpu_limits(55_000, 200_000, 56));
        assert!(!valid_cpu_limits(55_000, 115_000, 0));
    }

    #[test]
    fn fan_bounds() {
        assert!(valid_fixed_speed(57) && valid_fixed_speed(229));
        assert!(!valid_fixed_speed(0) && !valid_fixed_speed(56) && !valid_fixed_speed(230));
        assert!(valid_curve_point(229, 100) && valid_curve_point(0, 0));
        assert!(!valid_curve_point(255, 60)); // would reach the EC as a duty above 100%
        assert!(!valid_curve_point(100, 101) && !valid_curve_point(100, -1));
    }

    #[test]
    fn fixed_mode_minimum() {
        assert_eq!(fixed_mode_speed(0), MIN_FIXED); // left by the fast curve or take_over
        assert_eq!(fixed_mode_speed(56), MIN_FIXED);
        assert_eq!(fixed_mode_speed(57), 57);
        assert_eq!(fixed_mode_speed(183), 183);
        assert_eq!(fixed_mode_speed(255), FULL_DUTY);
        assert!((-300..600).all(|s| valid_fixed_speed(fixed_mode_speed(s))));
    }
}
