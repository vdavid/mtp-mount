mod buffer;
mod device;
mod error;
mod fill;
mod fs;
mod hints;
mod inode;
mod reconnect;
mod shutdown;
mod size;
mod sparse_cache;
mod spool;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;

use crate::device::{UnplugSwitch, UsbOpener};
use crate::fill::DEFAULT_STOP_TIMEOUT;
use crate::fs::{MtpFs, MtpFsConfig, DEFAULT_FULL_DOWNLOAD_LIMIT};
use crate::hints::{indent, open_failure_hint, BUSY_HINT, PERMISSION_HINT};
use crate::reconnect::ReconnectPolicy;
use crate::shutdown::{spawn_signal_handler, Shutdown};
use crate::size::{format_size, parse_size};

/// The default for `--full-download-limit`, spelled the way `--help` should show
/// it. `default_value_parser_matches_the_constant` keeps it honest.
const DEFAULT_FULL_DOWNLOAD_LIMIT_ARG: &str = "4G";

/// Mount MTP devices as local filesystems via FUSE.
///
/// Plug in your Android phone or camera, run this, and use regular
/// commands (ls, cp, cat, rm, mv, mkdir) on the device's storage.
#[derive(Parser, Debug)]
#[command(
    version,
    about,
    after_long_help = long_help()
)]
struct Cli {
    /// Where to mount (the directory must already exist)
    mountpoint: Option<String>,

    /// Device serial number (connects to the first available device if omitted)
    #[arg(short, long, value_name = "SERIAL")]
    device: Option<String>,

    /// Run in foreground instead of daemonizing
    #[arg(short, long, default_value_t = true)]
    foreground: bool,

    /// Mount as read-only (no writes, deletes, or renames)
    #[arg(short, long)]
    read_only: bool,

    /// List connected MTP devices and exit
    #[arg(short, long)]
    list: bool,

    /// Directory for spooling writes and read caches (defaults to your cache dir)
    #[arg(long, value_name = "PATH")]
    spool_dir: Option<PathBuf>,

    /// Seconds to wait for a disconnected device to come back (default 0: unmount right away)
    #[arg(long, value_name = "SECONDS", default_value_t = ReconnectPolicy::DEFAULT_TIMEOUT_SECS)]
    reconnect_timeout: u64,

    /// Largest file to read from a device with no partial-read support, e.g. 8G (0 for no limit)
    #[arg(long, value_name = "SIZE", value_parser = parse_size, default_value = DEFAULT_FULL_DOWNLOAD_LIMIT_ARG)]
    full_download_limit: u64,
}

/// The hand-written `--help` sections.
///
/// The troubleshooting remedies come from [`crate::hints`], the same text the
/// device-open failure prints, so the two can't drift apart.
fn long_help() -> String {
    format!(
        "\
EXAMPLES:
    Mount the first available device:
        mtp-mount /mnt/phone

    List connected devices (shows serial numbers for -d):
        mtp-mount --list

    Mount a specific device:
        mtp-mount -d ABC123 /mnt/phone

    Mount read-only (safer for browsing, no accidental deletes):
        mtp-mount -r /mnt/phone

    Unmount:
        umount /mnt/phone

    Give a worn-out cable a minute to recover (0 unmounts on the first drop):
        mtp-mount --reconnect-timeout 60 /mnt/phone

    Copy a file bigger than {limit} off a device with no partial-read support
    (0 removes the limit entirely):
        mtp-mount --full-download-limit 32G /mnt/switch

    Show debug output (handy for troubleshooting):
        RUST_LOG=debug mtp-mount /mnt/phone

TROUBLESHOOTING:
    \"No MTP device found\"
        Make sure the phone is unlocked, USB mode is set to \"File Transfer\"
        (not \"Charging only\"), and the USB debugging prompt is accepted.

    \"interface is busy\"
{busy}

    \"Permission denied\" on /dev/bus/usb
{permission}

NOTES:
    Files are uploaded to the device when you close them, not on each write.
    While a file is open for writing it's spooled to disk under your cache
    directory (--spool-dir overrides it), so uploads bigger than RAM work.
    MTP doesn't support partial writes, hardlinks, symlinks, or chmod.

    Most devices can hand over a byte range, so reading a file fetches only the
    part you touch. A few (some Nintendo Switch MTP apps, simple PTP responders)
    can only send whole objects. There, the first read starts one whole-file
    download and later reads wait for their bytes to arrive; nothing you do to
    the file interrupts it. That download holds the device for its whole length
    and everything else on the mount waits behind it, so files above
    --full-download-limit (default {limit}) are refused with \"File too large\"
    instead. Raise it, or pass 0, when you mean to copy a big one.

    If the device disconnects, the mount is taken down. Pass
    --reconnect-timeout SECONDS to wait for it to come back instead: the mount
    then picks up where it left off, including files you have open. Useful with
    a flaky cable that keeps dropping the link. It's off by default because
    commands WAIT during that window rather than failing, so anything walking
    the mount point (a file manager, a backup job) freezes until the device
    returns or the window ends.",
        busy = indent(BUSY_HINT, "        "),
        permission = indent(PERMISSION_HINT, "        "),
        limit = format_size(DEFAULT_FULL_DOWNLOAD_LIMIT),
    )
}

fn list_devices() {
    match mtp_rs::MtpDevice::list_devices() {
        Ok(devices) if devices.is_empty() => {
            println!("No MTP devices found.");
            println!();
            println!("Make sure your device is unlocked, USB mode is set to");
            println!("\"File Transfer\", and the USB debugging prompt is accepted.");
        }
        Ok(devices) => {
            println!("Found {} MTP device(s):\n", devices.len());
            for (i, dev) in devices.iter().enumerate() {
                let mfr = dev.manufacturer.as_deref().unwrap_or("Unknown");
                let product = dev.product.as_deref().unwrap_or("Unknown");
                let serial = dev.serial_number.as_deref().unwrap_or("(no serial)");
                println!(
                    "  [{}] {} {} (serial: {}, USB {:04x}:{:04x})",
                    i, mfr, product, serial, dev.vendor_id, dev.product_id
                );
            }
            println!();
            println!("Use -d SERIAL to mount a specific device.");
        }
        Err(e) => {
            eprintln!("Failed to list devices: {e}");
            std::process::exit(1);
        }
    }
}

fn main() {
    env_logger::init();

    let cli = Cli::parse();

    if cli.list {
        list_devices();
        return;
    }

    let mountpoint = match &cli.mountpoint {
        Some(m) => m,
        None => {
            eprintln!("Error: <MOUNTPOINT> is required (or use --list to see devices)");
            std::process::exit(1);
        }
    };

    // Resolved before the device opens: a broken spool dir should fail before
    // we claim the USB interface, and never silently fall back to a tmpfs $TMPDIR.
    let spool_dir = match spool::spool_dir_from_env(cli.spool_dir.as_deref())
        .and_then(|dir| spool::prepare_spool_dir(&dir).map(|()| dir))
    {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    let device_label = cli.device.as_deref().unwrap_or("first available device");
    println!("Mounting {device_label} at {mountpoint}...");

    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let handle = rt.handle().clone();

    let device = rt.block_on(async {
        if let Some(serial) = &cli.device {
            mtp_rs::MtpDevice::open_by_serial(serial).await
        } else {
            mtp_rs::MtpDevice::open_first().await
        }
    });

    let device = match device {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Failed to open MTP device: {e}");
            if let Some(hint) = open_failure_hint(&e) {
                eprintln!();
                eprintln!("{hint}");
            }
            std::process::exit(1);
        }
    };

    // Reconnect targets the serial of the device we actually opened, so a
    // replug can't silently land us on a different phone.
    let serial = match device.device_info().serial_number.trim() {
        "" => {
            eprintln!(
                "Note: this device reports no serial number, so a reconnect \
                 opens whichever MTP device is available."
            );
            None
        }
        serial => Some(serial.to_string()),
    };
    let unplug = UnplugSwitch::default();
    let opener = Arc::new(UsbOpener::new(serial, unplug.clone()));

    let mtp_fs = MtpFs::new(
        device,
        opener,
        handle,
        MtpFsConfig {
            read_only: cli.read_only,
            spool_dir,
            reconnect: ReconnectPolicy::from_secs(cli.reconnect_timeout),
            full_download_limit: cli.full_download_limit,
            unplug,
        },
    );
    let shutdown = mtp_fs.shutdown();
    let fills = mtp_fs.fills();
    let mount_options = mtp_fs.mount_options();

    let mut config = fuser::Config::default();
    config.mount_options = mount_options;

    // The session runs on its own thread rather than through `fuser::mount`,
    // so this thread can unmount when the filesystem gives up on a device that
    // never came back.
    let session = match fuser::spawn_mount(mtp_fs, mountpoint, &config) {
        Ok(session) => session,
        Err(e) => {
            eprintln!("Mount failed: {e}");
            std::process::exit(1);
        }
    };

    println!("Mounted. Press Ctrl+C to unmount.");

    // Ctrl+C has to come through here rather than kill the process where it
    // stands. A whole-object read holds the device's MTP session for the entire
    // transfer, and a process that dies mid-transfer leaves the responder in the
    // middle of a USB transaction; on Android that's the failure that needs a
    // physical replug. Catching the signal is what buys the chance to cancel.
    let signalled = Arc::new(Shutdown::default());
    spawn_signal_handler(rt.handle(), {
        let signalled = Arc::clone(&signalled);
        move |name| {
            println!("\nGot {name}, unmounting...");
            signalled.request(name);
        }
    });

    // Either the filesystem gives up on the device, the person asks us to stop,
    // or the mount ends the normal way (`umount`, or the kernel tearing the
    // session down).
    let mut gave_up = false;
    loop {
        if shutdown.wait_timeout(Duration::from_millis(250)).is_some() {
            gave_up = true;
            break;
        }
        if signalled.is_requested() || session.guard.is_finished() {
            break;
        }
    }

    let ended = if gave_up || signalled.is_requested() {
        session.umount_and_join()
    } else {
        session.join()
    };

    // Now that nothing new can arrive, tell any whole-object download in flight
    // to cancel, and give it a moment to actually do so before the runtime it
    // lives on goes away. Dropping the runtime with a transfer live is the
    // silent mid-transaction abort this is here to prevent.
    if !fills.stop_and_wait(DEFAULT_STOP_TIMEOUT) {
        eprintln!(
            "A download was still finishing when we stopped waiting; \
             the device may need a moment before it answers again."
        );
    }

    if let Err(e) = ended {
        eprintln!("Mount ended with an error: {e}");
        std::process::exit(1);
    }
    if gave_up {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_cli_definition_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn default_value_parser_matches_the_constant() {
        // `--help` shows "4G" while the code reasons in bytes. If those two ever
        // disagree, the help text lies about what the mount actually does.
        assert_eq!(
            parse_size(DEFAULT_FULL_DOWNLOAD_LIMIT_ARG),
            Ok(DEFAULT_FULL_DOWNLOAD_LIMIT)
        );
        let cli = Cli::parse_from(["mtp-mount", "/mnt/phone"]);
        assert_eq!(cli.full_download_limit, DEFAULT_FULL_DOWNLOAD_LIMIT);
    }

    #[test]
    fn the_limit_accepts_a_size_and_a_zero() {
        let cli = Cli::parse_from(["mtp-mount", "--full-download-limit", "32G", "/mnt/switch"]);
        assert_eq!(cli.full_download_limit, 32 * 1024 * 1024 * 1024);
        let cli = Cli::parse_from(["mtp-mount", "--full-download-limit", "0", "/mnt/switch"]);
        assert_eq!(cli.full_download_limit, 0);
    }
}
