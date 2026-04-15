mod buffer;
mod error;
mod fs;
mod inode;

use clap::Parser;

use crate::fs::MtpFs;

/// Mount MTP devices as local filesystems via FUSE.
///
/// Plug in your Android phone or camera, run this, and use regular
/// commands (ls, cp, cat, rm, mv, mkdir) on the device's storage.
#[derive(Parser, Debug)]
#[command(
    version,
    about,
    after_long_help = "\
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
        Another program already claimed the USB interface. On Linux, check
        if gvfs-mtp auto-mounted it: `gio mount -l` and unmount first.

    \"Permission denied\" on /dev/bus/usb
        Add yourself to the `plugdev` group, or set up a udev rule.
        See: https://github.com/vdavid/mtp-mount#requirements

NOTES:
    Files are uploaded to the device when you close them, not on each write.
    MTP doesn't support partial writes, hardlinks, symlinks, or chmod."
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
                let serial = dev
                    .serial_number
                    .as_deref()
                    .unwrap_or("(no serial)");
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
            std::process::exit(1);
        }
    };

    let mtp_fs = MtpFs::new(device, cli.read_only, handle);
    let mount_options = mtp_fs.mount_options();

    let mut config = fuser::Config::default();
    config.mount_options = mount_options;

    println!("Mounted. Press Ctrl+C to unmount.");

    if let Err(e) = fuser::mount2(mtp_fs, mountpoint, &config) {
        eprintln!("Mount failed: {e}");
        std::process::exit(1);
    }
}
