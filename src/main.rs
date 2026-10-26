mod fast;
mod keyboard;
mod platform;

use std::{error::Error, future::pending, env};
use zbus::Connection;

use crate::platform::CtrlPlatform;


// Although we use `async-std` here, you can use any async runtime of choice.
#[async_std::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // ExecStopPost: if the daemon was driving the fans, give them back to the EC.
    if env::args().any(|a| a == "--hand-back") {
        if std::path::Path::new(fast::STATE_FILE).exists() {
            match CtrlPlatform::new() {
                Ok(ctrl) => {
                    if let Err(e) = fast::hand_back(&ctrl.path) {
                        println!("Handing the fans back: {e}");
                    }
                }
                Err(()) => println!("Handing the fans back: no aorus_laptop device"),
            }
        }
        return Ok(());
    }

    let is_service = match env::var_os("IS_SERVICE") {
        Some(val) => val == "1",
        None => false,
    };
    let is_debug = match env::var_os("IS_DEBUG") {
        Some(val) => val == "1",
        None => false,
    };

    if !is_service {
        println!("gigabyted should only be run as a systemd service!");
        println!("If you need logs, use journalctl -b -u gigabyted");
        println!("gigabyted will now exit");
        return Ok(());
    }

    if is_debug {
        env::set_var("RUST_LOG", "debug");
        println!("RUST_LOG set to debug");
        println!("gigabyted will run on com.gigabyte.debug to avoid interfering with the real com.gigabyte.daemon");
    }

    // Firmware resets the CPU limits on every boot; put back the ones chosen last.
    if let Ok(saved) = std::fs::read_to_string(platform::CPU_LIMITS_FILE) {
        let v: Vec<i32> = saved.split_whitespace().filter_map(|x| x.parse().ok()).collect();
        if v.len() == 3 && platform::valid_cpu_limits(v[0], v[1], v[2]) {
            if let Err(e) = platform::apply_cpu_limits(v[0], v[1], v[2]) {
                println!("Restoring CPU limits: {e}");
            }
        }
    }

    let connection = Connection::system().await?;

    // Setup interface for kernel driver; without it there is nothing to serve, so fail and let
    // systemd show it.
    match CtrlPlatform::new() {
        Ok(ctrl) => {
            if !is_debug {
                ctrl.resume_fast_curve();
            }
            let (path, shared) = (ctrl.path.clone(), ctrl.fast.clone());
            std::thread::spawn(move || {
                // A dead loop would leave the fans at a fixed speed: exit, so systemd hands them back.
                if std::panic::catch_unwind(move || fast::run(path, shared)).is_err() {
                    std::process::exit(1);
                }
            });
            connection.object_server().at("/com/gigabyte/Platform", ctrl).await?;
        }
        Err(()) => return Err("no aorus_laptop platform device".into()),
    }

    // Request name
    if is_debug {
        connection.request_name("com.gigabyte.debug").await?;
    } else {
        connection.request_name("com.gigabyte.daemon").await?;
    }

    // Do other things or go to wait forever
    pending::<()>().await;

    Ok(())
}
