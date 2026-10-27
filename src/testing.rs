#![allow(clippy::unwrap_used)] // a debugging tool, not installed: a panic is a fine error here
extern crate udev;

fn main() {
    let mut enumerator = udev::Enumerator::new().unwrap();

    enumerator.match_subsystem("platform").unwrap();

    for device in enumerator.scan_devices().unwrap() {
        println!("found device using kernel driver {:?}: {:?}", device.sysname(), device.syspath());
        if device.sysname() == "aorus_laptop" {
            for attribute in device.attributes() {
                println!("attribute {:?} = {:?}", attribute.name(), attribute.value());
            }
        }
    }
}