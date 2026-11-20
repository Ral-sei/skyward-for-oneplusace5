//! The functions the USB port offers, which init sets up as
//! `sys.usb.config` asks: adb while the recovery waits, with files over MTP
//! beside it while file transfer is on.

use crate::{prop, set_prop, wait_prop};
use anyhow::{ensure, Result};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static MTP: AtomicBool = AtomicBool::new(false);

/// What the port offers while nothing else has it. The file transfer
/// function is offered beside adb only once it has its descriptors: a
/// recovery whose own rules never bring that function up, as OPLUS's stock
/// recovery does not, keeps adb alone rather than losing the port to a
/// configuration init cannot make.
pub fn resting() -> &'static str {
    if MTP.load(Ordering::Relaxed) && prop("sys.usb.ffs.mtp.ready").as_deref() == Some("1") {
        "mtp,adb"
    } else {
        "adb"
    }
}

/// Whether MTP goes beside adb while the port rests: from when its function
/// has its descriptors until it goes.
pub fn set_mtp(on: bool) {
    MTP.store(on, Ordering::Relaxed);
}

/// Ask init for `config` and wait until it is in place. Going to "none"
/// also waits for adbd to be gone, so its function is free.
pub fn set(config: &str, timeout: Duration) -> Result<()> {
    // The controller the phone's bootloader named, which a stock recovery
    // sets from its own init; a recovery that has not seen that line yet
    // would otherwise ask init to bind a gadget to no controller.
    if config != "none" {
        if prop("sys.usb.controller").is_none() {
            let named = prop("ro.boot.usbcontroller").or_else(|| prop("vendor.usb.controller")).or_else(|| {
                std::fs::read_dir("/sys/class/udc").ok()?.next()?.ok().map(|entry| entry.file_name().to_string_lossy().into_owned())
            });
            if let Some(controller) = named {
                set_prop("sys.usb.controller", &controller)?;
            }
        }
        if let Some(controller) = prop("sys.usb.controller") {
            peripheral(&controller, timeout);
        }
    }
    set_prop("sys.usb.config", config)?;
    wait_prop("sys.usb.state", config, timeout)?;
    if config == "none" {
        let start = Instant::now();
        while !matches!(prop("init.svc.adbd").as_deref(), None | Some("stopped")) {
            ensure!(start.elapsed() < timeout, "adbd still runs {} ms after the port went to none", timeout.as_millis());
            thread::sleep(Duration::from_millis(10));
        }
    }
    Ok(())
}

/// The port in device mode, where a gadget binds. The phone's own USB
/// companion chip turns it when a computer is plugged in; a recovery that
/// has none of that up leaves the controller as the kernel started it, and
/// nothing ever enumerates. The stock init of this phone's builds turns the
/// role by writing the controller's mode, through "idle" first, which drops
/// whatever role it holds before "peripheral" takes.
fn peripheral(controller: &str, timeout: Duration) {
    let udc = Path::new("/sys/class/udc").join(controller);
    let mode = Path::new("/sys/bus/platform/devices").join(platform()).join("mode");
    if std::fs::read_to_string(&mode).map(|m| m.trim() == "peripheral").unwrap_or(false) && udc.exists() {
        return;
    }
    let switched = if std::fs::write(&mode, "idle").is_ok() {
        let start = Instant::now();
        while udc.exists() && start.elapsed() < timeout {
            thread::sleep(Duration::from_millis(10));
        }
        std::fs::write(&mode, "peripheral").is_ok()
    } else if let Ok(roles) = std::fs::read_dir("/sys/class/usb_role") {
        // The role switch's own interface, where the mode attribute is not.
        let mut wrote = false;
        for role in roles.filter_map(|e| e.ok()) {
            wrote |= std::fs::write(role.path().join("role"), "device").is_ok();
        }
        wrote
    } else {
        false
    };
    if switched {
        let start = Instant::now();
        while !udc.exists() && start.elapsed() < timeout {
            thread::sleep(Duration::from_millis(10));
        }
    }
}

/// What the port is as the kernel and init have it, and what is missing
/// where it does not come up: the configuration init was asked for, the one
/// it made, whether adbd has its descriptors, the controller, the device
/// controllers that are there, the policy, the controller's mode, the
/// charge driver's switch and the role switches. A port whose gadget is
/// bound and whose device controllers are there still shows a computer
/// nothing where the switch in front of them was never turned.
pub fn status() -> String {
    let names = |dir: &str| -> Vec<String> {
        std::fs::read_dir(dir)
            .map(|entries| entries.filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned())).collect())
            .unwrap_or_default()
    };
    let trimmed = |path: &str| std::fs::read_to_string(path).map(|v| v.trim().to_string()).ok();
    let roles = std::fs::read_dir("/sys/class/usb_role")
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| {
                    let role = trimmed(&e.path().join("role").to_string_lossy()).unwrap_or_default();
                    format!("{}={role}", e.file_name().to_string_lossy())
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    format!(
        "config {:?} state {:?} ffs.ready {:?} controller {:?} udc {:?} enforcing {} mode {:?} otg_switch {:?} roles {:?}",
        prop("sys.usb.config"),
        prop("sys.usb.state"),
        prop("sys.usb.ffs.ready"),
        prop("sys.usb.controller"),
        names("/sys/class/udc"),
        trimmed("/sys/fs/selinux/enforce").unwrap_or_default(),
        trimmed(&format!("/sys/bus/platform/devices/{}/mode", platform())),
        trimmed("/sys/class/oplus_chg/usb/otg_switch"),
        roles
    )
}

/// The platform device the controller's mode is an attribute of: the
/// controller's own folder, as the phone's bootloader names it, which a
/// stock init writes "peripheral" to.
fn platform() -> String {
    prop("ro.boot.usb.dwc3_msm").filter(|named| !named.is_empty()).unwrap_or_else(|| "a600000.ssusb".to_string())
}

/// Hand the port to `config` through "none", which stops the programs
/// serving it and takes every function off; init then sets the port up
/// once the programs behind the new configuration are ready.
pub fn switch(config: &str, timeout: Duration) -> Result<()> {
    set("none", timeout)?;
    match set(config, timeout) {
        Ok(()) => Ok(()),
        Err(e) => {
            // A port init cannot make with file transfer beside adb keeps
            // adb, which is what the recovery is reached over.
            if config == "mtp,adb" {
                let _ = set("adb", timeout);
            }
            Err(e)
        }
    }
}
