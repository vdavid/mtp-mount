//! Opening (and reopening) the MTP device behind the mount.
//!
//! The filesystem never opens a device itself: it asks a [`DeviceOpener`]. That
//! keeps the reconnect path honest (the same code opens the device the first
//! time and after a cable glitch) and lets tests hand the mount a device they
//! can make vanish.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use mtp_rs::mtp::MtpDevice;

/// Source of MTP devices for a mount.
///
/// Implementations block; they're called from a FUSE callback thread with a
/// tokio handle to drive the async `mtp-rs` calls on.
pub trait DeviceOpener: Send + Sync {
    /// Opens the device this mount belongs to. Called once at startup and again
    /// on every reconnect attempt, so it must always resolve to the *same*
    /// physical device (match on serial number, not "first available").
    fn open(&self, rt: &tokio::runtime::Handle) -> Result<MtpDevice, mtp_rs::Error>;

    /// How to name the device in user-facing messages.
    fn describe(&self) -> String;
}

/// Opens a USB MTP device by serial number.
///
/// The serial is read off the device that was opened at startup even when the
/// user didn't pass `-d`, so a reconnect can't wander onto a different phone
/// that happens to be plugged in. Devices that report no serial fall back to
/// "first available", which is the best that's on offer.
pub struct UsbOpener {
    serial: Option<String>,
    unplug: UnplugSwitch,
}

impl UsbOpener {
    pub fn new(serial: Option<String>, unplug: UnplugSwitch) -> Self {
        Self { serial, unplug }
    }
}

impl DeviceOpener for UsbOpener {
    fn open(&self, rt: &tokio::runtime::Handle) -> Result<MtpDevice, mtp_rs::Error> {
        if self.unplug.is_unplugged() {
            return Err(mtp_rs::Error::Disconnected);
        }
        match &self.serial {
            Some(serial) => rt.block_on(MtpDevice::open_by_serial(serial)),
            None => rt.block_on(MtpDevice::open_first()),
        }
    }

    fn describe(&self) -> String {
        match &self.serial {
            Some(serial) => format!("device {serial}"),
            None => "the device".to_string(),
        }
    }
}

/// A pretend USB cable, shared between the mount and whoever wants to yank it.
///
/// While it's unplugged every MTP operation fails with
/// [`mtp_rs::Error::Disconnected`] and reopening fails too, which is exactly
/// what a real cable glitch looks like from inside the filesystem.
///
/// This exists because `mtp-rs` can't simulate a disconnect: its virtual device
/// keeps serving an already-open [`MtpDevice`] even after the device is removed
/// from the discovery registry, so the reconnect path would otherwise be
/// untestable without hardware. Production code never flips it; the cost is one
/// relaxed atomic load per MTP operation.
#[derive(Clone, Debug, Default)]
pub struct UnplugSwitch(Arc<AtomicBool>);

impl UnplugSwitch {
    /// Pretend the cable came out.
    #[allow(dead_code)] // used by integration tests via lib.rs, not by the bin
    pub fn unplug(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Pretend the cable went back in.
    #[allow(dead_code)] // used by integration tests via lib.rs, not by the bin
    pub fn replug(&self) {
        self.0.store(false, Ordering::SeqCst);
    }

    /// Whether the cable is currently out.
    pub fn is_unplugged(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Whether an error means "this session is gone", as opposed to a normal
/// operation failure the caller should see.
///
/// [`mtp_rs::Error::DeviceReset`] belongs here too: the device is still plugged
/// in, but `mtp-rs` reset it in software to recover, so the session (and every
/// handle in it) is dead and the cure is the same reopen.
///
/// **Don't replace this with `mtp_rs::Error::is_disconnected()`.** That
/// predicate is `Disconnected` alone, deliberately excluding `DeviceReset`
/// (there the device is still there, so a consumer that drops it from a sidebar
/// would be throwing away a live device). A mount asks a different question:
/// "does this session need a reopen?", and after a reset it does. Swapping in
/// the narrower predicate would leave the mount answering every call with the
/// dead session's handles. `NoDevice` is likewise ours to keep, so a device
/// that's gone by the time we reopen still walks the reconnect path.
/// Pinned by `link_loss_is_broader_than_mtp_rs_is_disconnected`.
pub fn is_link_lost(error: &mtp_rs::Error) -> bool {
    matches!(
        error,
        mtp_rs::Error::Disconnected | mtp_rs::Error::DeviceReset | mtp_rs::Error::NoDevice
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_starts_plugged_in() {
        let switch = UnplugSwitch::default();
        assert!(!switch.is_unplugged());
    }

    #[test]
    fn switch_is_shared_between_clones() {
        let switch = UnplugSwitch::default();
        let remote = switch.clone();
        remote.unplug();
        assert!(switch.is_unplugged());
        remote.replug();
        assert!(!switch.is_unplugged());
    }

    #[test]
    fn session_loss_triggers_reconnect_but_plain_failures_do_not() {
        assert!(is_link_lost(&mtp_rs::Error::Disconnected));
        assert!(is_link_lost(&mtp_rs::Error::DeviceReset));
        assert!(!is_link_lost(&mtp_rs::Error::NotFound));
        assert!(!is_link_lost(&mtp_rs::Error::AccessDenied));
        assert!(!is_link_lost(&mtp_rs::Error::Timeout));
    }

    /// `mtp-rs`'s `is_disconnected()` answers "is the device gone?", which isn't
    /// the question a mount asks. Ours is "does the session need a reopen?", and
    /// a software reset needs one while the device is still plugged in. If this
    /// ever stops failing on the `DeviceReset` line, the two questions have
    /// converged and only then is swapping the predicates safe.
    #[test]
    fn link_loss_is_broader_than_mtp_rs_is_disconnected() {
        assert!(mtp_rs::Error::Disconnected.is_disconnected());
        assert!(!mtp_rs::Error::DeviceReset.is_disconnected());
        assert!(!mtp_rs::Error::NoDevice.is_disconnected());

        assert!(is_link_lost(&mtp_rs::Error::DeviceReset));
        assert!(is_link_lost(&mtp_rs::Error::NoDevice));
    }
}
