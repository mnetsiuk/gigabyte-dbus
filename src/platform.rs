use std::path::PathBuf;
use udev::Device;
use zbus::dbus_interface;

pub struct CtrlPlatform {
    pub path: PathBuf
}

pub const CPU_LIMITS_FILE: &str = "/var/lib/gigabyted/cpu-limits";
const RAPL_ZONES: [&str; 2] = ["/sys/class/powercap/intel-rapl:0", "/sys/class/powercap/intel-rapl-mmio:0"];

// Any local user may call us, so only accept sane package limits: 5 W <= PL1 <= PL2 <= 135 W, 1-128 s window.
pub fn valid_cpu_limits(pl1_mw: i32, pl2_mw: i32, tau_s: i32) -> bool {
    (5_000..=135_000).contains(&pl1_mw) && (pl1_mw..=135_000).contains(&pl2_mw) && (1..=128).contains(&tau_s)
}

// Firmware programs both the MSR and the MMIO RAPL interface and the lower one wins, so set both.
pub fn apply_cpu_limits(pl1_mw: i32, pl2_mw: i32, tau_s: i32) -> std::io::Result<()> {
    for zone in RAPL_ZONES.iter().filter(|z| std::path::Path::new(z).exists()) {
        std::fs::write(format!("{zone}/constraint_0_power_limit_uw"), (pl1_mw as i64 * 1000).to_string())?;
        std::fs::write(format!("{zone}/constraint_1_power_limit_uw"), (pl2_mw as i64 * 1000).to_string())?;
        std::fs::write(format!("{zone}/constraint_0_time_window_us"), (tau_s as i64 * 1_000_000).to_string())?;
    }
    Ok(())
}

impl CtrlPlatform {
    pub fn new() -> Result<Self, ()> {
        let mut enumerator = udev::Enumerator::new().unwrap();

        enumerator.match_subsystem("platform").unwrap();
        enumerator.match_sysname("aorus_laptop").unwrap();
        if let Some(device) = (enumerator.scan_devices().map_err(|err| {
            println!("Could not scan devices: {:?}", err)
        })?)
        .next()
        {
            return Ok(Self {
                path: device.syspath().to_path_buf(),
            });
        }
        Err(())
    }
}

#[dbus_interface(name = "com.gigabyte.Platform")]
impl CtrlPlatform {
    // TODO: Implement
    fn set_fan_mode(&self, value: i32) -> i32 {
        let mut device = Device::from_syspath(&self.path).unwrap();
        device.set_attribute_value("fan_mode", &(value).to_string()).unwrap();
        0
    }
    fn set_fan_speed(&self, value: i32) -> i32 {
        let mut device = Device::from_syspath(&self.path).unwrap();
        device.set_attribute_value("fan_custom_speed", &(value).to_string()).unwrap();
        0
    }
    fn set_charge_mode(&self, value: i32) -> i32 {
        let mut device = Device::from_syspath(&self.path).unwrap();
        device.set_attribute_value("charge_mode", &(value).to_string()).unwrap();
        0
    }
    fn set_charge_limit(&self, value: i32) -> i32 {
        let mut device = Device::from_syspath(&self.path).unwrap();
        device.set_attribute_value("charge_limit", &(value).to_string()).unwrap();
        0
    }
    fn set_fan_curve_index(&self, value: i32) -> i32 {
        let mut device = Device::from_syspath(&self.path).unwrap();
        device.set_attribute_value("fan_curve_index", &(value).to_string()).unwrap();
        0
    }
    fn set_fan_curve_data(&self, speed: i32, temp: i32) -> i32 {
        let value = speed << 8 | temp;
        let mut device = Device::from_syspath(&self.path).unwrap();
        device.set_attribute_value("fan_curve_data", &(value).to_string()).unwrap();
        0
    }
    fn set_gpu_boost(&self, value: i32) -> i32 {
        let mut device = Device::from_syspath(&self.path).unwrap();
        device.set_attribute_value("gpu_boost", &(value).to_string()).unwrap();
        0
    }
    // get methods
    fn get_fan_mode(&self) -> i32 {
        let device = Device::from_syspath(&self.path).unwrap();
        let fan_mode = device.attribute_value("fan_mode").unwrap();
        fan_mode.to_str().unwrap().parse::<i32>().unwrap()
    }
    fn get_fan_speed(&self) -> i32 {
        let device = Device::from_syspath(&self.path).unwrap();
        let fan_speed = device.attribute_value("fan_custom_speed").unwrap();
        fan_speed.to_str().unwrap().parse::<i32>().unwrap()
    }
    fn get_charge_mode(&self) -> i32 {
        let device = Device::from_syspath(&self.path).unwrap();
        let charge_mode = device.attribute_value("charge_mode").unwrap();
        charge_mode.to_str().unwrap().parse::<i32>().unwrap()
    }
    fn get_charge_limit(&self) -> i32 {
        let device = Device::from_syspath(&self.path).unwrap();
        let charge_limit = device.attribute_value("charge_limit").unwrap();
        charge_limit.to_str().unwrap().parse::<i32>().unwrap()
    }
    fn get_fan_curve_index(&self) -> i32 {
        let device = Device::from_syspath(&self.path).unwrap();
        let fan_curve_index = device.attribute_value("fan_curve_index").unwrap();
        fan_curve_index.to_str().unwrap().parse::<i32>().unwrap()
    }
    fn get_fan_curve_data(&self) -> (i32, i32) {
        let device = Device::from_syspath(&self.path).unwrap();
        let fan_curve_data = device.attribute_value("fan_curve_data").unwrap();
        let mut parts = fan_curve_data.to_str().unwrap().split_whitespace();
        let temp = parts.next().unwrap().parse::<i32>().unwrap();
        let speed = parts.next().unwrap().parse::<i32>().unwrap();
        (temp, speed)
    }
    fn get_battery_cycle(&self) -> i32 {
        let device = Device::from_syspath(&self.path).unwrap();
        let battery_cycle = device.attribute_value("battery_cycle").unwrap();
        battery_cycle.to_str().unwrap().parse::<i32>().unwrap()
    }
    fn get_usb_s3_toggle(&self) -> i32 {
        let device = Device::from_syspath(&self.path).unwrap();
        let usb_s3_toggle = device.attribute_value("usb_charge_s3_toggle").unwrap();
        usb_s3_toggle.to_str().unwrap().parse::<i32>().unwrap()
    }
    fn get_usb_s4_toggle(&self) -> i32 {
        let device = Device::from_syspath(&self.path).unwrap();
        let usb_s4_toggle = device.attribute_value("usb_charge_s4_toggle").unwrap();
        usb_s4_toggle.to_str().unwrap().parse::<i32>().unwrap()
    }
    fn get_gpu_boost(&self) -> i32 {
        let device = Device::from_syspath(&self.path).unwrap();
        let gpu_boost = device.attribute_value("gpu_boost").unwrap();
        gpu_boost.to_str().unwrap().parse::<i32>().unwrap()
    }
    fn get_power_on_time(&self) -> (i32, i32, i32, i32, i32) {
        let device = Device::from_syspath(&self.path).unwrap();
        let power_on_time = device.attribute_value("power_on_time").unwrap();
        let mut parts = power_on_time.to_str().unwrap().split_whitespace();
        let day = parts.next().unwrap().parse::<i32>().unwrap();
        let hour = parts.next().unwrap().parse::<i32>().unwrap();
        let minute = parts.next().unwrap().parse::<i32>().unwrap();
        let month = parts.next().unwrap().parse::<i32>().unwrap();
        let year = parts.next().unwrap().parse::<i32>().unwrap();
        (day, hour, minute, month, year)
    }
    fn get_light_sensor(&self) -> (i32, i32, i32, i32) {
        let device = Device::from_syspath(&self.path).unwrap();
        let light_sensor = device.attribute_value("light_sensor").unwrap();
        // TODO: Check for older models, as they don't use the same format
        let mut parts = light_sensor.to_str().unwrap().split_whitespace();
        let version = parts.next().unwrap().parse::<i32>().unwrap();
        let lux_low = parts.next().unwrap().parse::<i32>().unwrap();
        let lux_med = parts.next().unwrap().parse::<i32>().unwrap();
        let lux_high = parts.next().unwrap().parse::<i32>().unwrap();
        (version, lux_low, lux_med, lux_high)
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
        if let Err(e) = std::fs::write(CPU_LIMITS_FILE, format!("{pl1_mw} {pl2_mw} {tau_s}\n")) {
            println!("SetCpuPowerLimits: could not remember limits: {e}");
        }
        0
    }
    // NVIDIA Dynamic Boost is run by nvidia-powerd: enable and start it, or stop and disable it,
    // so the choice also holds after a reboot.
    fn set_dynamic_boost(&self, on: i32) -> i32 {
        let verb = if on != 0 { "enable" } else { "disable" };
        match std::process::Command::new("systemctl").args([verb, "--now", "nvidia-powerd.service"]).status() {
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
    fn get_fan_pwm(&self) -> i32 {
        let device = Device::from_syspath(&self.path).unwrap();
        let fan_pwm = device.attribute_value("fan_pwm").unwrap();
        fan_pwm.to_str().unwrap().parse::<i32>().unwrap()
    }
}
#[cfg(test)]
mod tests {
    use super::valid_cpu_limits;

    #[test]
    fn cpu_limit_bounds() {
        assert!(valid_cpu_limits(55_000, 115_000, 56));
        assert!(valid_cpu_limits(15_000, 30_000, 56));
        assert!(!valid_cpu_limits(70_000, 60_000, 56)); // PL2 below PL1
        assert!(!valid_cpu_limits(4_000, 30_000, 56));
        assert!(!valid_cpu_limits(55_000, 200_000, 56));
        assert!(!valid_cpu_limits(55_000, 115_000, 0));
    }
}
