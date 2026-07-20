mod buffer;
mod error;
mod fs;
mod hints;
mod inode;
mod sparse_cache;
mod spool;

use std::path::PathBuf;

use clap::Parser;

use crate::fs::MtpFs;
use crate::hints::{indent, open_failure_hint, BUSY_HINT, PERMISSION_HINT};

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
    MTP doesn't support partial writes, hardlinks, symlinks, or chmod.",
        busy = indent(BUSY_HINT, "        "),
        permission = indent(PERMISSION_HINT, "        "),
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

    let mtp_fs = MtpFs::new(device, cli.read_only, handle, spool_dir);
    let mount_options = mtp_fs.mount_options();

    let mut config = fuser::Config::default();
    config.mount_options = mount_options;

    println!("Mounted. Press Ctrl+C to unmount.");

    if let Err(e) = fuser::mount2(mtp_fs, mountpoint, &config) {
        eprintln!("Mount failed: {e}");
        std::process::exit(1);
    }
}
