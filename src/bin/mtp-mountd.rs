//! `mtp-mountd`: mounts every MTP device that's plugged in, for as long as it's
//! plugged in.
//!
//! This binary is the wiring, nothing more: resolve the paths, start the USB
//! watch and the signal handler, and hand the supervisor a channel. The
//! behavior lives in [`mtp_mount::daemon`].

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;

use clap::Parser;
use log::{error, info};

use mtp_mount::daemon::supervisor::{Command, Supervisor, SupervisorConfig};
use mtp_mount::daemon::unmount::clean_stale_mounts;
use mtp_mount::daemon::usb::{spawn_hotplug_watch, UsbSource};
use mtp_mount::daemon::{mount_root_from_env, RUNTIME_SUBDIR};
use mtp_mount::hints::{indent, BUSY_HINT, PERMISSION_HINT};
use mtp_mount::spool;

/// Mount MTP devices automatically, as they're plugged in.
///
/// Runs in the foreground and logs to stdout, so it works as a
/// `systemd --user` service. Every device that's plugged in gets a mount under
/// the mount root; unplug it and the mount goes away.
#[derive(Parser, Debug)]
#[command(name = "mtp-mountd", version, about, after_long_help = long_help())]
struct Cli {
    /// Directory to mount devices under (defaults to $XDG_RUNTIME_DIR/mtp)
    #[arg(long, value_name = "PATH")]
    mount_root: Option<PathBuf>,

    /// Directory for spooling writes and read caches (defaults to your cache dir)
    #[arg(long, value_name = "PATH")]
    spool_dir: Option<PathBuf>,

    /// Mount every device read-only (no writes, deletes, or renames)
    #[arg(short, long)]
    read_only: bool,
}

fn long_help() -> String {
    format!(
        "\
WHAT IT DOES:
    Watches for MTP devices and mounts each one it finds under the mount root,
    in a directory named after the device's serial number:

        $XDG_RUNTIME_DIR/{RUNTIME_SUBDIR}/<serial>/

    Devices that report no serial number get usb-<vendor>-<product>-<port>
    instead, which lasts as long as the device stays in the same port.

    A device with more than one storage (internal memory plus an SD card, say)
    gets ONE mount, with each storage as a subdirectory under it.

    Unplug a device and its mount goes away right then, so nothing is left for
    a file manager to hang on. Plug it back in and it's mounted again.

RUNNING IT:
    As a systemd --user service (the intended way):
        systemctl --user enable --now mtp-mountd

    In a terminal, to watch what it's doing:
        RUST_LOG=info mtp-mountd

TROUBLESHOOTING:
    Nothing gets mounted
        Make sure the phone is unlocked, USB mode is set to \"File Transfer\"
        (not \"Charging only\"), and the USB debugging prompt is accepted.
        Run with RUST_LOG=debug to see what the daemon sees.

    \"interface is busy\" in the log
{busy}

    \"Permission denied\" on /dev/bus/usb
{permission}

NOTES:
    Files are uploaded to the device when you close them, not on each write.
    While a file is open for writing it's spooled to disk under your cache
    directory (--spool-dir overrides it), so uploads bigger than RAM work.
    MTP doesn't support partial writes, hardlinks, symlinks, or chmod.

    There's no reconnect window: a device that goes away is unmounted at once,
    and hotplug mounts it again when it comes back. Waiting instead would
    freeze every process touching the mount for the length of the window.",
        busy = indent(BUSY_HINT, "        "),
        permission = indent(PERMISSION_HINT, "        "),
    )
}

fn main() {
    env_logger::init();
    let cli = Cli::parse();

    // Both directories are resolved before any USB work: a daemon that can't
    // spool or can't make mount points should say so and exit, not claim a USB
    // interface first and fail per-device afterwards.
    let spool_dir = match spool::spool_dir_from_env(cli.spool_dir.as_deref())
        .and_then(|dir| spool::prepare_spool_dir(&dir).map(|()| dir))
    {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    let mount_root = match mount_root_from_env(cli.mount_root.as_deref()) {
        Ok(root) => root,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    // A daemon that was killed rather than stopped left its mounts in the
    // kernel's mount table with nothing serving them. Clear them before
    // reusing the paths, and clear them even for devices that aren't back:
    // each one wedges whatever walks it.
    for path in clean_stale_mounts(&mount_root) {
        info!("Cleaned up a stale mount at {}", path.display());
    }

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Can't start the async runtime: {e}");
            std::process::exit(1);
        }
    };
    let handle = rt.handle().clone();

    let (commands, inbox) = mpsc::channel();

    if let Err(e) = spawn_hotplug_watch(&handle, commands.clone()) {
        eprintln!("Can't watch for USB devices: {e}");
        std::process::exit(1);
    }
    spawn_signal_handler(&handle, commands.clone());

    info!(
        "Watching for MTP devices; mounts appear under {}",
        mount_root.display()
    );

    let supervisor = Supervisor::new(
        SupervisorConfig::new(mount_root, spool_dir, cli.read_only),
        Arc::new(UsbSource),
        handle,
        commands,
    );

    // Runs on the main thread on purpose: mounting bridges async to sync with
    // `Handle::block_on`, which panics inside a runtime thread.
    supervisor.run(inbox);
    info!("Stopped.");
}

/// Turn a stop signal into a [`Command::Stop`].
///
/// `systemd` stops services with `SIGTERM`, and a person stops one in a
/// terminal with `SIGINT`. Both have to unmount everything on the way out:
/// mounts left behind after a `systemctl --user stop` are exactly the wedged
/// directories this daemon exists to avoid.
fn spawn_signal_handler(rt: &tokio::runtime::Handle, commands: mpsc::Sender<Command>) {
    rt.spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};

        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(stream) => stream,
            Err(e) => {
                error!("Can't listen for SIGTERM: {e}");
                return;
            }
        };
        let mut interrupt = match signal(SignalKind::interrupt()) {
            Ok(stream) => stream,
            Err(e) => {
                error!("Can't listen for SIGINT: {e}");
                return;
            }
        };

        let signal_name = tokio::select! {
            _ = terminate.recv() => "SIGTERM",
            _ = interrupt.recv() => "SIGINT",
        };
        let _ = commands.send(Command::Stop(format!("got {signal_name}")));
    });
}
