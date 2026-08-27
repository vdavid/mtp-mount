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
use log::info;

use mtp_mount::daemon::dryrun::{DryRun, DryRunCommand};
use mtp_mount::daemon::supervisor::{Command, Supervisor, SupervisorConfig};
use mtp_mount::daemon::unmount::clean_stale_mounts;
use mtp_mount::daemon::usb::{spawn_dry_run_watch, spawn_hotplug_watch, UsbSource};
use mtp_mount::daemon::{mount_root_from_env, RUNTIME_SUBDIR};
use mtp_mount::fs::DEFAULT_FULL_DOWNLOAD_LIMIT;
use mtp_mount::hints::{indent, BUSY_HINT, PERMISSION_HINT};
use mtp_mount::shutdown::spawn_signal_handler;
use mtp_mount::size::{format_size, parse_size};
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

    /// Largest file to read from a device with no partial-read support, e.g. 8G (0 for no limit)
    #[arg(long, value_name = "SIZE", value_parser = parse_size, default_value = DEFAULT_FULL_DOWNLOAD_LIMIT_ARG)]
    full_download_limit: u64,

    /// Report what would be mounted as devices come and go, and mount nothing
    #[arg(long)]
    dry_run: bool,
}

/// The default for `--full-download-limit`, spelled the way `--help` shows it.
const DEFAULT_FULL_DOWNLOAD_LIMIT_ARG: &str = "4G";

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

CHECKING A DEVICE WITHOUT MOUNTING IT (--dry-run):
        mtp-mountd --dry-run

    Watches for devices and prints what it WOULD do: the fields each device
    reports, the mount key derived from them, and the path it would mount at.
    Nothing is mounted, no directory is created, and the device is never
    opened, so this works on a machine with no FUSE at all.

    Plug the device in, wait for the PLUGGED IN block, then unplug it. The
    UNPLUGGED block says whether its key MATCHES the arrival. It has to: the
    key is how a departure finds the mount to take down, so a device whose
    two keys disagree would leave its mount behind. Do that a few times and
    read the summary. --spool-dir and -r do nothing in this mode.

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
    freeze every process touching the mount for the length of the window.

    A few devices (some Nintendo Switch MTP apps, simple PTP responders) can
    only send whole objects rather than byte ranges. Reading a file there starts
    one whole-file download that holds the device until it finishes, so files
    above --full-download-limit (default {limit}) are refused with \"File too
    large\" instead. Raise it, or pass 0, when you mean to copy a big one.",
        busy = indent(BUSY_HINT, "        "),
        permission = indent(PERMISSION_HINT, "        "),
        limit = format_size(DEFAULT_FULL_DOWNLOAD_LIMIT),
    )
}

fn main() {
    env_logger::init();
    let cli = Cli::parse();

    // Resolving the mount root touches nothing on disk, which is what lets a
    // dry run report the path it would use and then stop before anything is
    // created.
    let mount_root = match mount_root_from_env(cli.mount_root.as_deref()) {
        Ok(root) => root,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    if cli.dry_run {
        run_dry_run(mount_root);
        return;
    }

    // The spool is resolved before any USB work: a daemon that can't spool
    // should say so and exit, not claim a USB interface first and fail
    // per-device afterwards.
    let spool_dir = match spool::spool_dir_from_env(cli.spool_dir.as_deref())
        .and_then(|dir| spool::prepare_spool_dir(&dir).map(|()| dir))
    {
        Ok(dir) => dir,
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
    {
        let commands = commands.clone();
        spawn_signal_handler(&handle, move |signal| {
            let _ = commands.send(Command::Stop(format!("got {signal}")));
        });
    }

    info!(
        "Watching for MTP devices; mounts appear under {}",
        mount_root.display()
    );

    let supervisor = Supervisor::new(
        SupervisorConfig {
            full_download_limit: cli.full_download_limit,
            ..SupervisorConfig::new(mount_root, spool_dir, cli.read_only)
        },
        Arc::new(UsbSource),
        handle,
        commands,
    );

    // Runs on the main thread on purpose: mounting bridges async to sync with
    // `Handle::block_on`, which panics inside a runtime thread.
    supervisor.run(inbox);
    info!("Stopped.");
}

/// Watch for devices and report what would be mounted, mounting nothing.
///
/// The hotplug path is the one part of the daemon that no test can exercise, so
/// this is how it gets checked against a real device. It creates nothing: the
/// mount root isn't made, the stale-mount sweep doesn't run (it unmounts
/// things), and no device is opened. See [`mtp_mount::daemon::dryrun`].
fn run_dry_run(mount_root: PathBuf) {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Can't start the async runtime: {e}");
            std::process::exit(1);
        }
    };
    let handle = rt.handle().clone();

    let (events, inbox) = mpsc::channel();
    if let Err(e) = spawn_dry_run_watch(&handle, events.clone()) {
        eprintln!("Can't watch for USB devices: {e}");
        std::process::exit(1);
    }
    spawn_signal_handler(&handle, move |signal| {
        let _ = events.send(DryRunCommand::Stop(format!("got {signal}")));
    });

    // On the main thread for the same reason `Supervisor::run` is: the runtime
    // carries the watch and the signal handler.
    DryRun::new(mount_root).run(inbox);
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
        assert_eq!(
            parse_size(DEFAULT_FULL_DOWNLOAD_LIMIT_ARG),
            Ok(DEFAULT_FULL_DOWNLOAD_LIMIT)
        );
        assert_eq!(
            Cli::parse_from(["mtp-mountd"]).full_download_limit,
            DEFAULT_FULL_DOWNLOAD_LIMIT
        );
    }
}
