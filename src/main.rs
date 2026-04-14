mod buffer;
mod error;
mod fs;
mod inode;

use clap::Parser;

use crate::fs::MtpFs;

/// Mount MTP devices as local filesystems via FUSE.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// Directory where the MTP device will be mounted
    mountpoint: String,

    /// Device serial number (uses first available device if omitted)
    #[arg(short, long)]
    device: Option<String>,

    /// Run in foreground instead of daemonizing
    #[arg(short, long, default_value_t = true)]
    foreground: bool,

    /// Mount the device as read-only
    #[arg(short, long)]
    read_only: bool,
}

fn main() {
    env_logger::init();

    let cli = Cli::parse();

    let device_label = cli.device.as_deref().unwrap_or("first available device");
    println!("Mounting {device_label} at {}...", cli.mountpoint);

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

    if let Err(e) = fuser::mount2(mtp_fs, &cli.mountpoint, &config) {
        eprintln!("Mount failed: {e}");
        std::process::exit(1);
    }
}
