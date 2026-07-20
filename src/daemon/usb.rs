//! Production wiring: real USB hotplug into supervisor commands.
//!
//! Everything USB-specific about the daemon is here, so the supervisor stays a
//! plain state machine over a channel (see [`crate::daemon::supervisor`]).

use std::sync::mpsc::Sender;
use std::sync::Arc;

use futures::StreamExt;
use log::{info, warn};
use mtp_rs::mtp::{watch_devices, HotplugEvent, MtpDeviceInfo};

use crate::daemon::paths::device_dir_name;
use crate::daemon::supervisor::{Command, DeviceChange, DeviceIdent, DeviceSource};
use crate::device::{DeviceOpener, UnplugSwitch};

/// How the daemon names and identifies a device the USB watch reported.
///
/// The key has to come out the same for an arrival and the matching departure,
/// because the departure is what tells the supervisor which mount to take down.
/// Both events carry the same [`MtpDeviceInfo`], so deriving the key purely
/// from its fields is what makes that hold.
pub fn ident_of(info: &MtpDeviceInfo) -> DeviceIdent {
    let serial = info
        .serial_number
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    DeviceIdent {
        key: device_dir_name(serial, info.vendor_id, info.product_id, info.location_id),
        label: info.display(),
        serial: serial.map(str::to_string),
    }
}

/// Opens real USB devices, matched by serial number.
pub struct UsbSource;

impl DeviceSource for UsbSource {
    fn opener(&self, ident: &DeviceIdent) -> Arc<dyn DeviceOpener> {
        // A device that reports no serial can only be reopened as "first
        // available". That's the same compromise the CLI makes, and the daemon
        // never reopens anyway: reconnect is off, so a device that goes away is
        // unmounted and re-mounted from a fresh hotplug event.
        Arc::new(crate::device::UsbOpener::new(
            ident.serial.clone(),
            UnplugSwitch::default(),
        ))
    }
}

/// Start watching USB and feed what it sees to the supervisor.
///
/// Devices already plugged in arrive as [`HotplugEvent::Arrived`] on the first
/// poll, so this is the only enumeration the daemon does. Listing devices
/// separately at startup would mount each of them twice.
///
/// # Errors
///
/// Returns an error if the OS refuses to set up USB hotplug notifications,
/// which leaves the daemon with nothing to do.
pub fn spawn_hotplug_watch(
    rt: &tokio::runtime::Handle,
    commands: Sender<Command>,
) -> Result<(), mtp_rs::Error> {
    let mut watch = watch_devices()?;
    rt.spawn(async move {
        while let Some(event) = watch.next().await {
            let change = match event {
                HotplugEvent::Arrived(info) => {
                    info!("Plugged in: {}", info.display());
                    DeviceChange::Arrived(ident_of(&info))
                }
                HotplugEvent::Left(info) => {
                    info!("Unplugged: {}", info.display());
                    DeviceChange::Left(ident_of(&info))
                }
            };
            // A closed channel means the supervisor stopped; so should this.
            if commands.send(Command::Device(change)).is_err() {
                return;
            }
        }
        warn!("The USB hotplug watch ended; no further devices will be picked up.");
    });
    Ok(())
}
