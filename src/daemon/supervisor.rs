//! The loop that owns every mount the daemon has.
//!
//! # The seam
//!
//! The supervisor never touches USB. Its whole input is a channel of
//! [`Command`]s: a device arrived, a device left, a mount gave up on its
//! device, stop. [`crate::daemon::usb`] is what turns `mtp-rs`'s hotplug stream
//! into those commands in production.
//!
//! That's deliberate, and it's the only way this code is testable. USB hotplug
//! can't be simulated: there's no way to make a container believe a phone was
//! plugged in, so a supervisor that called `watch_devices()` itself could only
//! ever be tested with a person and a cable. With the channel, a test sends
//! `Command::Device(DeviceChange::Arrived(..))` and a real FUSE mount over a
//! real (virtual) MTP device appears at a real path, with everything below the
//! seam being the production code path. The same trick applies to the device
//! itself through [`DeviceSource`]: production hands back a [`UsbOpener`], the
//! tests hand back an opener for an `mtp-rs` virtual device.
//!
//! [`UsbOpener`]: crate::device::UsbOpener
//!
//! # Threading
//!
//! [`Supervisor::run`] blocks the thread it's called on and must NOT be called
//! from inside a tokio runtime: opening a device and mounting it both go
//! through `Handle::block_on`, the same sync-over-async bridge the FUSE
//! callbacks use. The daemon runs it on the main thread and keeps the runtime
//! for the hotplug watch and the signal handler.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use log::{debug, error, info, warn};

use crate::daemon::unmount::{force_unmount, wait_until_unmounted};
use crate::device::{DeviceOpener, UnplugSwitch};
use crate::fs::{MtpFs, MtpFsConfig};
use crate::hints::open_failure_hint;
use crate::reconnect::ReconnectPolicy;

/// How long a mount gets to leave the filesystem before the daemon complains.
pub const DEFAULT_UNMOUNT_TIMEOUT: Duration = Duration::from_secs(10);

/// A device the daemon knows about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceIdent {
    /// Identity and directory name in one: see [`crate::daemon::device_dir_name`].
    ///
    /// One string does both jobs so that "is this device already mounted?" and
    /// "where does it go?" can't disagree. It has to be derivable from what an
    /// arrival *and* a departure report, since a departure is the only thing
    /// that says which mount to take down.
    pub key: String,
    /// What the device is called in the log.
    pub label: String,
    /// Serial number, when the device reports one. What a reopen matches on.
    pub serial: Option<String>,
}

/// A device showed up or went away.
#[derive(Debug, Clone)]
pub enum DeviceChange {
    Arrived(DeviceIdent),
    Left(DeviceIdent),
}

/// Everything the supervisor reacts to.
#[derive(Debug, Clone)]
pub enum Command {
    /// The device set changed.
    Device(DeviceChange),
    /// A mount decided its device is gone for good and asked to be taken down.
    ///
    /// The filesystem can't unmount itself (see [`crate::shutdown`]), and this
    /// can beat the hotplug departure: a mount notices a dead session on its
    /// next operation, while the USB watch only notices on its next poll.
    GiveUp { key: String, reason: String },
    /// Unmount everything and return from [`Supervisor::run`].
    Stop(String),
}

/// Where the supervisor gets devices from.
///
/// Production returns a [`UsbOpener`](crate::device::UsbOpener) matched to the
/// device's serial; tests return an opener for a virtual device.
pub trait DeviceSource: Send + Sync {
    /// The opener for a device the watch reported. It's used to open the device
    /// now and, if the mount ever needs it, to reopen the same one later, so it
    /// must resolve to that device and not to "whatever is plugged in".
    fn opener(&self, ident: &DeviceIdent) -> Arc<dyn DeviceOpener>;
}

/// How the daemon mounts things.
pub struct SupervisorConfig {
    /// Directory that holds one subdirectory per mounted device.
    pub mount_root: PathBuf,
    /// Disk-backed spool for write buffers and read caches (see [`crate::spool`]).
    pub spool_dir: PathBuf,
    /// Mount every device read-only.
    pub read_only: bool,
    /// How long to wait for a mount to actually leave the filesystem.
    pub unmount_timeout: Duration,
}

impl SupervisorConfig {
    /// Config with the default unmount timeout.
    pub fn new(mount_root: PathBuf, spool_dir: PathBuf, read_only: bool) -> Self {
        Self {
            mount_root,
            spool_dir,
            read_only,
            unmount_timeout: DEFAULT_UNMOUNT_TIMEOUT,
        }
    }
}

/// One device's mount.
struct ActiveMount {
    path: PathBuf,
    label: String,
    session: fuser::BackgroundSession,
    /// Tells the give-up watcher to stop, so it doesn't outlive the mount.
    watcher_stop: Arc<AtomicBool>,
}

/// Mounts devices as they arrive and unmounts them as they leave.
pub struct Supervisor {
    config: SupervisorConfig,
    source: Arc<dyn DeviceSource>,
    rt: tokio::runtime::Handle,
    /// Handed to each mount's give-up watcher so it can report back.
    commands: Sender<Command>,
    mounts: HashMap<String, ActiveMount>,
}

impl Supervisor {
    /// Build a supervisor. `commands` must be a sender for the same channel
    /// whose receiver goes to [`Supervisor::run`].
    pub fn new(
        config: SupervisorConfig,
        source: Arc<dyn DeviceSource>,
        rt: tokio::runtime::Handle,
        commands: Sender<Command>,
    ) -> Self {
        Self {
            config,
            source,
            rt,
            commands,
            mounts: HashMap::new(),
        }
    }

    /// Handle commands until [`Command::Stop`] arrives or every sender is
    /// dropped, then unmount everything.
    ///
    /// Blocks the calling thread. See the module docs on why it can't run
    /// inside a tokio runtime.
    pub fn run(mut self, commands: Receiver<Command>) {
        // A mount root that can't be created isn't recoverable, but it also
        // isn't worth stopping the process over before the caller has logged
        // anything: every mount attempt reports it instead.
        if let Err(e) = std::fs::create_dir_all(&self.config.mount_root) {
            error!(
                "Can't create the mount root {}: {e}",
                self.config.mount_root.display()
            );
        }

        let stop_reason = loop {
            match commands.recv() {
                Ok(Command::Device(DeviceChange::Arrived(ident))) => self.mount(ident),
                Ok(Command::Device(DeviceChange::Left(ident))) => {
                    self.unmount(&ident.key, "the device was unplugged")
                }
                Ok(Command::GiveUp { key, reason }) => self.unmount(&key, &reason),
                Ok(Command::Stop(reason)) => break reason,
                // Defensive: the supervisor keeps a sender of its own for the
                // give-up watchers, so the channel can't actually close while
                // this loop is running. Breaking beats spinning if that ever
                // stops being true.
                Err(_) => break "every event source is gone".to_string(),
            }
        };

        info!("Shutting down: {stop_reason}");
        self.unmount_all();
    }

    /// Devices currently mounted, by key. Used by tests.
    pub fn mounted_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.mounts.keys().cloned().collect();
        keys.sort();
        keys
    }

    fn mount(&mut self, ident: DeviceIdent) {
        if let Some(existing) = self.mounts.get(&ident.key) {
            warn!(
                "{} is already mounted at {}; ignoring this arrival. \
                 Two devices reporting the same serial number look identical from here.",
                ident.label,
                existing.path.display()
            );
            return;
        }

        let path = self.config.mount_root.join(&ident.key);
        if let Err(e) = std::fs::create_dir_all(&path) {
            error!("Can't create the mount point {}: {e}", path.display());
            return;
        }

        let opener = self.source.opener(&ident);
        let device = match opener.open(&self.rt) {
            Ok(device) => device,
            Err(e) => {
                error!("Can't open {}: {e}", ident.label);
                if let Some(hint) = open_failure_hint(&e) {
                    error!("{hint}");
                }
                let _ = std::fs::remove_dir(&path);
                return;
            }
        };

        let mtp_fs = MtpFs::new(
            device,
            opener,
            self.rt.clone(),
            MtpFsConfig {
                read_only: self.config.read_only,
                spool_dir: self.config.spool_dir.clone(),
                // Reconnect stays off here for the same reason it's off in the
                // CLI, only more so: waiting blocks every process touching the
                // mount, and the daemon has a better answer than waiting. A
                // device that comes back arrives as a fresh hotplug event and
                // gets mounted again at the same path, with nothing frozen in
                // between. Turning this on would trade a mount that reappears
                // for a desktop that hangs.
                reconnect: ReconnectPolicy::from_secs(0),
                // The pretend cable is a test seam for the CLI's reconnect path;
                // nothing here ever flips it.
                unplug: UnplugSwitch::default(),
            },
        );

        let shutdown = mtp_fs.shutdown();
        let mut fuse_config = fuser::Config::default();
        fuse_config.mount_options = mtp_fs.mount_options();

        let session = match fuser::spawn_mount(mtp_fs, &path, &fuse_config) {
            Ok(session) => session,
            Err(e) => {
                error!("Can't mount {} at {}: {e}", ident.label, path.display());
                let _ = std::fs::remove_dir(&path);
                return;
            }
        };

        // The mount raises its shutdown signal from inside a FUSE callback and
        // can't act on it (see `crate::shutdown`), so one thread per mount turns
        // that signal into a command the supervisor can act on.
        let watcher_stop = Arc::new(AtomicBool::new(false));
        {
            let watcher_stop = Arc::clone(&watcher_stop);
            let commands = self.commands.clone();
            let key = ident.key.clone();
            std::thread::spawn(move || loop {
                if let Some(reason) = shutdown.wait_timeout(Duration::from_millis(200)) {
                    let _ = commands.send(Command::GiveUp { key, reason });
                    return;
                }
                if watcher_stop.load(Ordering::Relaxed) {
                    return;
                }
            });
        }

        info!("Mounted {} at {}", ident.label, path.display());
        self.mounts.insert(
            ident.key,
            ActiveMount {
                path,
                label: ident.label,
                session,
                watcher_stop,
            },
        );
    }

    fn unmount(&mut self, key: &str, reason: &str) {
        let Some(mount) = self.mounts.remove(key) else {
            debug!("Nothing mounted for {key}; nothing to unmount ({reason})");
            return;
        };
        let ActiveMount {
            path,
            label,
            session,
            watcher_stop,
        } = mount;
        watcher_stop.store(true, Ordering::Relaxed);
        info!("Unmounting {label} from {}: {reason}", path.display());

        // The forced unmount comes first, and it's ours, not `fuser`'s.
        // `fuser`'s own unmount is a plain `umount()`, which fails with EBUSY
        // whenever anything still holds the mount, and a busy mount is the
        // normal case here: the reason the device left is usually that someone
        // yanked the cable mid-copy. [`force_unmount`] is the lazy detach
        // (`umount2(MNT_DETACH)`, or `fusermount3 -u -z` when the syscall is
        // refused), which succeeds regardless and leaves the in-flight callers
        // to get their error.
        //
        // `umount_and_join` still runs afterwards, on its own thread, to reap
        // the session: its unmount is a no-op on an already-detached mount, and
        // the *join* can take as long as the FUSE callback in progress takes to
        // notice its device is gone. The daemon must not block on that, because
        // the next device's mount is queued behind this loop.
        if let Err(e) = force_unmount(&path) {
            error!("Can't unmount {}: {e}", path.display());
        }
        let joining_path = path.clone();
        std::thread::spawn(move || match session.umount_and_join() {
            Ok(()) => debug!("The session for {} ended cleanly", joining_path.display()),
            Err(e) => debug!("The session for {} ended with {e}", joining_path.display()),
        });

        if wait_until_unmounted(&path, self.config.unmount_timeout) {
            // Only now is the directory a plain empty directory again, so this
            // is also a second check: `remove_dir` on a live mount point fails.
            if let Err(e) = std::fs::remove_dir(&path) {
                warn!(
                    "Unmounted {label}, but {} is still there: {e}",
                    path.display()
                );
            }
        } else {
            error!(
                "{} is STILL mounted {}s after unmounting it. \
                 Anything touching that path may hang; unmount it by hand with \
                 `fusermount3 -u -z {}`.",
                path.display(),
                self.config.unmount_timeout.as_secs(),
                path.display()
            );
        }
    }

    fn unmount_all(&mut self) {
        let keys: Vec<String> = self.mounts.keys().cloned().collect();
        for key in keys {
            self.unmount(&key, "the daemon is shutting down");
        }
        // Tidy, and a signal to anything watching the root that nothing is
        // mounted any more. Fails harmlessly if the root isn't empty.
        let _ = std::fs::remove_dir(&self.config.mount_root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source that's never asked for anything: the tests here only exercise
    /// the bookkeeping that happens before a device is opened.
    struct NoDevices;

    impl DeviceSource for NoDevices {
        fn opener(&self, _ident: &DeviceIdent) -> Arc<dyn DeviceOpener> {
            unreachable!("these tests never get as far as opening a device")
        }
    }

    fn ident(key: &str) -> DeviceIdent {
        DeviceIdent {
            key: key.to_string(),
            label: format!("device {key}"),
            serial: Some(key.to_string()),
        }
    }

    fn supervisor(root: PathBuf) -> (Supervisor, Sender<Command>, Receiver<Command>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();
        std::mem::forget(rt);
        let supervisor = Supervisor::new(
            SupervisorConfig::new(root, std::env::temp_dir(), false),
            Arc::new(NoDevices),
            handle,
            tx.clone(),
        );
        (supervisor, tx, rx)
    }

    #[test]
    fn a_departure_for_a_device_that_was_never_mounted_is_ignored() {
        let root = tempfile::tempdir().unwrap();
        let (mut supervisor, _tx, _rx) = supervisor(root.path().to_path_buf());
        supervisor.unmount("ABC123", "test");
        assert!(supervisor.mounted_keys().is_empty());
    }

    #[test]
    fn stopping_with_nothing_mounted_returns() {
        let root = tempfile::tempdir().unwrap();
        let (supervisor, tx, rx) = supervisor(root.path().to_path_buf());
        tx.send(Command::Stop("test".into())).unwrap();
        drop(tx);
        supervisor.run(rx);
    }

    #[test]
    fn the_mount_root_is_created_when_the_loop_starts() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("runtime").join("mtp");
        let (supervisor, tx, rx) = supervisor(root.clone());
        tx.send(Command::Stop("test".into())).unwrap();
        drop(tx);
        supervisor.run(rx);
        // `unmount_all` removes it again on the way out, so check the parent
        // chain, which is what proves `create_dir_all` ran.
        assert!(root.parent().unwrap().is_dir());
    }

    #[test]
    fn a_device_that_cannot_be_opened_leaves_no_directory_behind() {
        // The failing-open path is the one that runs when a phone is locked or
        // gvfs got there first: it must not leave an empty mount point in the
        // root for a file manager to show as a device that isn't there.
        struct NeverOpens;
        impl DeviceSource for NeverOpens {
            fn opener(&self, _ident: &DeviceIdent) -> Arc<dyn DeviceOpener> {
                struct Refuses;
                impl DeviceOpener for Refuses {
                    fn open(
                        &self,
                        _rt: &tokio::runtime::Handle,
                    ) -> Result<mtp_rs::mtp::MtpDevice, mtp_rs::Error> {
                        Err(mtp_rs::Error::ExclusiveAccess)
                    }
                    fn describe(&self) -> String {
                        "a device that won't open".into()
                    }
                }
                Arc::new(Refuses)
            }
        }

        let root = tempfile::tempdir().unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();
        let mut supervisor = Supervisor::new(
            SupervisorConfig::new(root.path().to_path_buf(), std::env::temp_dir(), false),
            Arc::new(NeverOpens),
            handle,
            tx,
        );

        supervisor.mount(ident("ABC123"));

        assert!(supervisor.mounted_keys().is_empty());
        assert!(!root.path().join("ABC123").exists());
    }
}
