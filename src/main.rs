#[allow(dead_code)]
mod buffer;
mod error;
mod fs;
#[allow(dead_code)]
mod inode;

use clap::Parser;

/// Mount MTP devices as local filesystems via FUSE.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// Directory where the MTP device will be mounted
    mountpoint: String,

    /// Device index or name (uses first available device if omitted)
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
}
