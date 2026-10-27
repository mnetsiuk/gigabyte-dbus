# gigabyte-dbus

`gigabyted` is a root daemon on the system bus that lets desktop applications control a Gigabyte
laptop through the [aorus-laptop kernel driver](https://github.com/tangalbert919/gigabyte-laptop-wmi)
without running as root themselves.

This is a fork of [tangalbert919/gigabyte-dbus](https://github.com/tangalbert919/gigabyte-dbus) for
the **AORUS 5 SE** (driver 0.2.0). It adds safety checks on every value it passes to the EC, a fast
fan curve, CPU power limits, NVIDIA Dynamic Boost and the keyboard backlight. Its client is
[Aero Control Center](https://github.com/tangalbert919/AeroControlCenter).

## Access

Bus name `com.gigabyte.daemon`, object `/com/gigabyte/Platform`, interface `com.gigabyte.Platform`.
Every method is listed with its ranges in [`interface.xml`](interface.xml).

Only members of **wheel** and root may call it (`gigabyted.conf`). On Debian and Ubuntu, where
administrators are in the `sudo` group, add your user to `wheel` or change the group in
`/usr/share/dbus-1/system.d/gigabyted.conf`.

Setters return 0 on success and -1 when a value is refused or the driver fails; the reason is in
`journalctl -u gigabyted`. Getters answer a D-Bus error on failure.

## How it runs

- **Started by the driver.** A udev rule starts `gigabyted.service` when the `aorus_laptop` driver
  binds, and the unit is bound to that device, so it also stops when the driver goes away. There is
  nothing to enable.
- **State** lives in `/var/lib/gigabyted`: `cpu-limits` (restored on every start, since the
  firmware resets them at boot) and `fast-curve` (present while the daemon drives the fans).
- **Fan safety.** A fixed speed must be at least 57 of 229 (25 %), and switching to fixed mode
  never starts below that. A curve must have 15 points with rising temperatures, never-falling duty
  and full duty at the top; curve mode is refused while the EC holds anything else.

### Fast fan curve

The EC raises its fan duty by about 5 % every 3 s, so a curve it runs takes ~45 s to reach full
speed. `SetFastCurve(1)` puts the EC in fixed mode and lets the daemon set the duty from the same
curve every 0.5 s, from the hottest of the CPU's own sensor and the EC's CPU and GPU readings: the
fans react within a second, and run at full duty from 90 °C. The duty falls only 3 s after the last
rise, then 2 % per second.

It survives a restart: the curve is kept in `/var/lib/gigabyted/fast-curve` and resumed on start
(retrying for ~30 s while the sensors appear). The fans are **handed back** to the EC's curve
(normal mode, then curve mode if the EC curve is valid) on `SetFastCurve(0)`, when another fan mode
or speed is set, when a sensor cannot be read for 2 s, and by `ExecStopPost` whenever the daemon
stops or dies.

### CPU power limits and Dynamic Boost

`SetCpuPowerLimits(pl1_mw, pl2_mw, tau_s)` sets the package limits through **Intel RAPL** (both the
MSR and the MMIO zone, since the lower one wins); AMD CPUs are not supported. It accepts
5 W <= PL1 <= PL2 <= **135 W** and a 1-128 s window, and restores them on every start.

`SetDynamicBoost(on)` enables and starts, or stops and disables, NVIDIA's `nvidia-powerd.service`,
which runs Dynamic Boost; it needs the NVIDIA driver's `nvidia-powerd`.

### Keyboard backlight

```
SetKeyboardZone(i zone, i red, i green, i blue, i brightness) -> i
GetKeyboardZone(i zone) -> (i red, i green, i blue, i brightness)
```

Zones 1 left, 2 center, 3 right, or 0 for all three (set only); colours 0-255; brightness 0-3. The
keyboard saves every change itself, by erasing and rewriting a flash sector about a second after
the last one, so the daemon limits how often that can happen, whoever asks: changes less than half a
second apart count as one, 20 may come at once, and after that one every 3 minutes. A change over
the limit returns -1. The daemon talks only to interface 3 of the ITE keyboard
(USB 1044:7a41) with firmware 1.9.0.4 (`bcdDevice` 1904), the one the protocol was read from, and
sends only the colour commands, so user programs no longer need access to the keyboard's USB
device, whose other commands reach its firmware update and flash erase. Remove any udev rule that
gives users that access (`TAG+="uaccess"` for 1044:7a41).

## Building and installing

You need Rust (1.87 or newer) and `make`.

```sh
make                # release build; DEBUG=1 for a debug build
cargo test          # the safety rules and the keyboard packets
sudo make install   # DESTDIR=... for staging
sudo udevadm control --reload && sudo udevadm trigger
```

The last step (or reloading the driver: `modprobe -r aorus_laptop && modprobe aorus_laptop`, or a
reboot) is needed once, so the new udev rule sees the driver bind and starts the daemon.
`make install` puts the rule in `/etc/udev/rules.d`; packages pass
`UDEVRULESDIR=/usr/lib/udev/rules.d`. Uninstall with `sudo make uninstall`.

Packages: `pkg/PKGBUILD` (Arch) and `pkg/gigabyte-dbus.spec` (RPM) build tag `v1.1.0`.

## License

MPL-2.0. Inspired by [asusctl](https://gitlab.com/asus-linux/asusctl).
