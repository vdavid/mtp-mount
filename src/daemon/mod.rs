//! The `mtp-mountd` daemon: one mount per device, for as long as the device is there.
//!
//! The CLI mounts one device at one path because a person asked it to. The
//! daemon does the same thing on the device's schedule instead: a phone shows
//! up, it gets a mount; the cable comes out, the mount goes away. Everything
//! below the mount is the same code the CLI uses ([`crate::fs::MtpFs`], the
//! spool dir, the open-failure hints).
//!
//! The parts:
//!
//! - [`supervisor`]: the loop that owns every mount. Its input is a channel of
//!   [`Command`]s, never USB directly, which is the seam tests drive.
//! - [`usb`]: the production wiring, turning `mtp-rs`'s hotplug stream into
//!   those commands.
//! - [`paths`]: where mounts live and what each one is called.
//! - [`unmount`]: taking a mount down for good, and proving it's gone.
//! - [`dryrun`]: `--dry-run`, which watches real devices and reports what would
//!   happen without mounting anything. It's how the hotplug path, the one part
//!   no test can reach, gets checked against a real phone.

pub mod dryrun;
pub mod paths;
pub mod supervisor;
pub mod unmount;
pub mod usb;

pub use dryrun::{DepartureVerdict, DeviceFacts, DryRun, DryRunCommand};
pub use paths::{device_dir_name, mount_root_from_env, resolve_mount_root, RUNTIME_SUBDIR};
pub use supervisor::{
    Command, DeviceChange, DeviceIdent, DeviceSource, Supervisor, SupervisorConfig,
};
pub use unmount::{clean_stale_mounts, force_unmount, is_mountpoint};
pub use usb::{facts_of, ident_of, spawn_dry_run_watch, spawn_hotplug_watch, UsbSource};
