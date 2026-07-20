//! Remedies for the failures people actually hit when opening a device.
//!
//! The same wording backs both the `--help` troubleshooting section and the
//! error message printed when opening the device fails, so the two can't drift.

/// Another process holds the USB interface.
pub const BUSY_HINT: &str = "\
Another program already claimed the USB interface.

On Linux that's almost always gvfs: run `gio mount -l` to find the device,
then `gio mount -u <mount-uri>` to release it. To stop gvfs from grabbing
devices at all: `systemctl --user mask gvfs-mtp-volume-monitor`.

On macOS it's `ptpcamerad`: stop it with `sudo killall ptpcamerad`, then
reconnect the device (launchd starts it again on the next connect).";

/// The OS refused access to the USB device node.
pub const PERMISSION_HINT: &str = "\
The OS denied access to the USB device.

On Linux this is a missing udev rule: your user has no write access to
/dev/bus/usb/*. Add yourself to the `plugdev` group, or install a udev rule
for the device's vendor ID.
See: https://github.com/vdavid/mtp-mount#requirements";

/// Pick the remedy for a device-open failure, if there is a specific one.
pub fn open_failure_hint(e: &mtp_rs::Error) -> Option<&'static str> {
    if e.is_exclusive_access() {
        Some(BUSY_HINT)
    } else if e.is_permission_denied() {
        Some(PERMISSION_HINT)
    } else {
        None
    }
}

/// Indent every non-empty line, for embedding a hint in the `--help` sections.
pub fn indent(text: &str, prefix: &str) -> String {
    text.lines()
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else {
                format!("{prefix}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_gets_the_release_the_device_remedy() {
        // What mtp-rs maps a USB `EBUSY` / `kIOReturnExclusiveAccess` to when
        // another process (gvfs, ptpcamerad) holds the interface.
        let e = mtp_rs::Error::ExclusiveAccess;
        assert!(e.is_exclusive_access(), "precondition: {e}");
        let hint = open_failure_hint(&e).expect("busy needs a hint");
        assert!(hint.contains("gio mount -l"));
        assert!(hint.contains("gvfs-mtp-volume-monitor"));
        assert!(hint.contains("ptpcamerad"));
    }

    #[test]
    fn permission_denied_points_at_udev() {
        let e = mtp_rs::Error::PermissionDenied;
        assert!(e.is_permission_denied(), "precondition: {e}");
        let hint = open_failure_hint(&e).expect("permission denied needs a hint");
        assert!(hint.contains("udev"));
    }

    #[test]
    fn other_failures_get_no_hint() {
        let e = mtp_rs::Error::NotFound;
        assert!(!e.is_exclusive_access());
        assert!(!e.is_permission_denied());
        assert!(open_failure_hint(&e).is_none());
    }

    #[test]
    fn indent_prefixes_content_lines_only() {
        assert_eq!(indent("a\n\nb", "  "), "  a\n\n  b");
    }
}
