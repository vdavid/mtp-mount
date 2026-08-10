//! Integration tests for the `mtp-mountd` supervisor.
//!
//! These drive the supervisor through its command channel (the seam described
//! in `mtp_mount::daemon::supervisor`) and assert against the real filesystem:
//! a synthetic arrival produces an actual FUSE mount over an actual `mtp-rs`
//! virtual device, and a synthetic departure has to make it actually go away.
//!
//! Everything below the channel is production code. What the tests stand in for
//! is USB hotplug, which can't be simulated: no container can be made to
//! believe a phone was plugged in.
//!
//! Linux only (they need FUSE), so they're `#[ignore]` like the rest:
//!
//! ```sh
//! cargo test --test daemon -- --ignored --test-threads=1
//! ```

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mtp_rs::mtp::MtpDevice;
use mtp_rs::transport::virtual_device::config::{VirtualDeviceConfig, VirtualStorageConfig};
use tempfile::TempDir;

use mtp_mount::daemon::supervisor::{
    Command, DeviceChange, DeviceIdent, DeviceSource, Supervisor, SupervisorConfig,
};
use mtp_mount::daemon::unmount::{clean_stale_mounts, is_mountpoint};
use mtp_mount::device::DeviceOpener;

/// The single storage every test device exposes.
const STORAGE: &str = "Internal Storage";

// =============================================================================
// Harness
// =============================================================================

/// Opens the virtual device registered for a device key.
///
/// This is the production [`DeviceSource`] swapped out, and it's the only other
/// thing the tests replace: the supervisor, the filesystem, the mount, and the
/// unmount are all the real ones.
#[derive(Default)]
struct VirtualSource {
    configs: Mutex<HashMap<String, VirtualDeviceConfig>>,
}

impl DeviceSource for VirtualSource {
    fn opener(&self, ident: &DeviceIdent) -> Arc<dyn DeviceOpener> {
        let config = self
            .configs
            .lock()
            .unwrap()
            .get(&ident.key)
            .unwrap_or_else(|| panic!("no virtual device registered for {}", ident.key))
            .clone();
        Arc::new(VirtualOpener { config })
    }
}

struct VirtualOpener {
    config: VirtualDeviceConfig,
}

impl DeviceOpener for VirtualOpener {
    fn open(&self, rt: &tokio::runtime::Handle) -> Result<MtpDevice, mtp_rs::Error> {
        rt.block_on(MtpDevice::builder().open_virtual(self.config.clone()))
    }

    fn describe(&self) -> String {
        format!("virtual device {}", self.config.serial)
    }
}

/// A supervisor running on its own thread, with a channel to drive it.
struct TestDaemon {
    root: TempDir,
    _spool: TempDir,
    commands: Sender<Command>,
    source: Arc<VirtualSource>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Backing dirs for the registered devices, kept alive for the mounts.
    backing: Vec<TempDir>,
}

impl TestDaemon {
    fn new() -> Self {
        // Off unless RUST_LOG says otherwise; the supervisor's own log is the
        // only view into what an unmount did when one of these fails.
        let _ = env_logger::builder().is_test(false).try_init();
        let root = TempDir::new().expect("mount root");
        let spool = TempDir::new().expect("spool dir");
        let source = Arc::new(VirtualSource::default());
        let (commands, inbox) = mpsc::channel();

        // Leaked so the runtime outlives the FUSE threads, same as the other
        // integration suite does.
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let handle = rt.handle().clone();
        std::mem::forget(rt);

        let supervisor = Supervisor::new(
            SupervisorConfig::new(root.path().to_path_buf(), spool.path().to_path_buf(), false),
            Arc::clone(&source) as Arc<dyn DeviceSource>,
            handle,
            commands.clone(),
        );
        let thread = std::thread::spawn(move || supervisor.run(inbox));

        Self {
            root,
            _spool: spool,
            commands,
            source,
            thread: Some(thread),
            backing: Vec::new(),
        }
    }

    /// Register a device the supervisor can open, and return its ident.
    ///
    /// `setup` pre-populates the backing directory, which is what the mount
    /// then shows.
    fn add_device<F: FnOnce(&Path)>(&mut self, serial: &str, setup: F) -> DeviceIdent {
        let backing = TempDir::new().expect("backing dir");
        setup(backing.path());

        let config = VirtualDeviceConfig {
            serial: format!("{serial}-{}", std::process::id()),
            storages: vec![VirtualStorageConfig {
                description: STORAGE.into(),
                capacity: 1024 * 1024 * 1024,
                backing_dir: backing.path().to_path_buf(),
                read_only: false,
            }],
            // Reads go through `Storage::read_range`, so the device has to
            // offer a partial-read operation for the mount to serve a file.
            supports_partial_object_64: true,
            supports_rename: true,
            // The backing-dir watcher is ON, and it's load-bearing: the mount's
            // own writes land in the backing dir, the watcher re-keys the object
            // handles, and the write-then-read-back below then runs straight
            // into a stale handle. That's real Android behavior, and the mount
            // recovers from it by re-resolving, so the daemon suite runs with it
            // on rather than routing around it.
            event_poll_interval: Duration::from_millis(50),
            watch_backing_dirs: true,
            ..Default::default()
        };

        let ident = DeviceIdent {
            key: serial.to_string(),
            label: format!("test device {serial}"),
            serial: Some(serial.to_string()),
        };
        self.source
            .configs
            .lock()
            .unwrap()
            .insert(ident.key.clone(), config);
        self.backing.push(backing);
        ident
    }

    fn arrive(&self, ident: &DeviceIdent) {
        self.commands
            .send(Command::Device(DeviceChange::Arrived(ident.clone())))
            .expect("supervisor is running");
    }

    fn leave(&self, ident: &DeviceIdent) {
        self.commands
            .send(Command::Device(DeviceChange::Left(ident.clone())))
            .expect("supervisor is running");
    }

    /// Ask the supervisor to shut down and wait for it to finish.
    fn stop(&mut self) {
        let _ = self.commands.send(Command::Stop("test".into()));
        if let Some(thread) = self.thread.take() {
            thread.join().expect("supervisor thread");
        }
    }

    fn root(&self) -> &Path {
        self.root.path()
    }

    fn mount_path(&self, ident: &DeviceIdent) -> PathBuf {
        self.root.path().join(&ident.key)
    }

    /// The storage directory inside a device's mount.
    fn storage_path(&self, ident: &DeviceIdent) -> PathBuf {
        self.mount_path(ident).join(STORAGE)
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Poll until `path` lists at least one entry, so the mount is serving.
fn wait_for_mount(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(entries) = fs::read_dir(path) {
            if entries.count() > 0 {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("nothing was mounted at {} within 10s", path.display());
}

/// Poll until `path` is gone from the filesystem.
fn wait_for_removal(path: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if !path.exists() && !is_mountpoint(path) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Whether the kernel still has a mount at `path`.
///
/// This is the assertion that a mount is really gone, as opposed to a Rust
/// value having been dropped: it reads the process's own mount table, which
/// only the kernel writes.
fn kernel_has_mount(path: &Path) -> bool {
    let table = match fs::read_to_string("/proc/self/mountinfo") {
        Ok(table) => table,
        // No procfs (macOS): fall back to the `stat()`-based check.
        Err(_) => return is_mountpoint(path),
    };
    let wanted = path.to_string_lossy();
    table.lines().any(|line| {
        // Field 5 of a mountinfo line is the mount point.
        line.split_whitespace().nth(4) == Some(&wanted)
    })
}

/// Everything an unmount has to be true for. Called after every take-down.
fn assert_really_unmounted(path: &Path) {
    assert!(
        wait_for_removal(path),
        "{} is still there after the unmount",
        path.display()
    );
    assert!(
        !kernel_has_mount(path),
        "{} is still in the kernel's mount table",
        path.display()
    );
    assert!(
        !is_mountpoint(path),
        "{} still looks like a mount point",
        path.display()
    );
    assert!(
        !path.exists(),
        "the mount point {} was left behind",
        path.display()
    );
}

// =============================================================================
// 1. An arrival mounts a usable filesystem
// =============================================================================

#[test]
#[ignore]
fn arrival_mounts_a_usable_filesystem_at_the_expected_path() {
    let mut daemon = TestDaemon::new();
    let phone = daemon.add_device("ARRIVAL01", |backing| {
        fs::write(backing.join("notes.txt"), b"hello from the device").unwrap();
        fs::create_dir(backing.join("DCIM")).unwrap();
        fs::write(backing.join("DCIM").join("photo.jpg"), vec![7u8; 4096]).unwrap();
    });

    daemon.arrive(&phone);
    wait_for_mount(&daemon.mount_path(&phone));

    // The path is the one the daemon promises: <root>/<device key>.
    assert_eq!(daemon.mount_path(&phone), daemon.root().join("ARRIVAL01"));
    assert!(kernel_has_mount(&daemon.mount_path(&phone)));

    // A device with several storages gets subdirectories under one mount, not
    // one mount each, so the storage is a level down.
    let storage = daemon.storage_path(&phone);
    let mut names: Vec<String> = fs::read_dir(&storage)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["DCIM".to_string(), "notes.txt".to_string()]);

    assert_eq!(
        fs::read_to_string(storage.join("notes.txt")).unwrap(),
        "hello from the device"
    );
    assert_eq!(
        fs::read(storage.join("DCIM").join("photo.jpg")).unwrap(),
        vec![7u8; 4096]
    );
}

// =============================================================================
// 2. A departure unmounts, for real
// =============================================================================

#[test]
#[ignore]
fn departure_unmounts_and_leaves_nothing_behind() {
    let mut daemon = TestDaemon::new();
    let phone = daemon.add_device("LEAVE0001", |backing| {
        fs::write(backing.join("a.txt"), b"a").unwrap();
    });

    daemon.arrive(&phone);
    let path = daemon.mount_path(&phone);
    wait_for_mount(&path);
    assert!(kernel_has_mount(&path), "precondition: it mounted");

    daemon.leave(&phone);

    assert_really_unmounted(&path);
}

#[test]
#[ignore]
fn departure_unmounts_while_a_read_is_in_flight() {
    // The case that matters: someone is copying off the phone when the cable
    // comes out. The unmount must not wait for that read to finish, or the
    // daemon deadlocks and leaves the mount for the next process to hang on.
    let mut daemon = TestDaemon::new();
    let phone = daemon.add_device("INFLIGHT1", |backing| {
        fs::write(backing.join("big.bin"), vec![3u8; 32 * 1024 * 1024]).unwrap();
    });

    daemon.arrive(&phone);
    let path = daemon.mount_path(&phone);
    wait_for_mount(&path);

    let file = daemon.storage_path(&phone).join("big.bin");
    let reader = std::thread::spawn(move || {
        // Expected to fail partway through once the mount is detached; the
        // point is that it doesn't hold the unmount up.
        let _ = fs::read(&file);
    });
    // Let the read get going before pulling the rug out.
    std::thread::sleep(Duration::from_millis(150));

    let started = Instant::now();
    daemon.leave(&phone);
    assert_really_unmounted(&path);
    let took = started.elapsed();

    assert!(
        took < Duration::from_secs(10),
        "the unmount took {took:?}, so it waited for the in-flight read"
    );
    reader.join().expect("the reader thread panicked");
}

// =============================================================================
// 3. Several devices at once
// =============================================================================

#[test]
#[ignore]
fn two_devices_mount_at_distinct_paths_and_work_independently() {
    let mut daemon = TestDaemon::new();
    let phone = daemon.add_device("PHONE0001", |backing| {
        fs::write(backing.join("which.txt"), b"phone").unwrap();
    });
    let camera = daemon.add_device("CAMERA001", |backing| {
        fs::write(backing.join("which.txt"), b"camera").unwrap();
    });

    daemon.arrive(&phone);
    daemon.arrive(&camera);
    wait_for_mount(&daemon.mount_path(&phone));
    wait_for_mount(&daemon.mount_path(&camera));

    assert_ne!(daemon.mount_path(&phone), daemon.mount_path(&camera));
    assert_eq!(
        fs::read_to_string(daemon.storage_path(&phone).join("which.txt")).unwrap(),
        "phone"
    );
    assert_eq!(
        fs::read_to_string(daemon.storage_path(&camera).join("which.txt")).unwrap(),
        "camera"
    );

    // Writes go to the right device too, and each mount is its own session.
    fs::write(daemon.storage_path(&phone).join("new.txt"), b"written").unwrap();
    assert_eq!(
        fs::read_to_string(daemon.storage_path(&phone).join("new.txt")).unwrap(),
        "written"
    );
    assert!(!daemon.storage_path(&camera).join("new.txt").exists());

    // One device leaving doesn't disturb the other.
    daemon.leave(&phone);
    assert_really_unmounted(&daemon.mount_path(&phone));
    assert_eq!(
        fs::read_to_string(daemon.storage_path(&camera).join("which.txt")).unwrap(),
        "camera"
    );
}

#[test]
#[ignore]
fn a_second_arrival_for_the_same_device_does_not_mount_twice() {
    // Two devices reporting the same serial, or a duplicated hotplug event,
    // must not stack a second mount on top of the first.
    let mut daemon = TestDaemon::new();
    let phone = daemon.add_device("DOUBLE001", |backing| {
        fs::write(backing.join("only.txt"), b"one").unwrap();
    });

    daemon.arrive(&phone);
    wait_for_mount(&daemon.mount_path(&phone));
    daemon.arrive(&phone);
    std::thread::sleep(Duration::from_millis(300));

    let path = daemon.mount_path(&phone);
    assert_eq!(
        fs::read_to_string(daemon.storage_path(&phone).join("only.txt")).unwrap(),
        "one"
    );

    // One departure is enough to leave nothing behind, which it wouldn't be if
    // a second mount were stacked on the same path.
    daemon.leave(&phone);
    assert_really_unmounted(&path);
}

// =============================================================================
// 4. Shutdown
// =============================================================================

#[test]
#[ignore]
fn shutting_down_unmounts_every_device() {
    let mut daemon = TestDaemon::new();
    let phone = daemon.add_device("SHUTDOWN1", |backing| {
        fs::write(backing.join("f"), b"x").unwrap();
    });
    let camera = daemon.add_device("SHUTDOWN2", |backing| {
        fs::write(backing.join("f"), b"x").unwrap();
    });

    daemon.arrive(&phone);
    daemon.arrive(&camera);
    let phone_path = daemon.mount_path(&phone);
    let camera_path = daemon.mount_path(&camera);
    wait_for_mount(&phone_path);
    wait_for_mount(&camera_path);

    // What `mtp-mountd` does on SIGTERM.
    daemon.stop();

    assert_really_unmounted(&phone_path);
    assert_really_unmounted(&camera_path);
}

// =============================================================================
// 5. Stale mounts from a daemon that was killed
// =============================================================================

/// The child half of [`startup_cleans_up_a_mount_a_killed_daemon_left_behind`].
///
/// It's a test only so it can be re-run out of this same binary; it does
/// nothing unless the parent set the environment variables, which no ordinary
/// run does.
#[test]
#[ignore]
fn stale_mount_helper() {
    let (Ok(mountpoint), Ok(backing)) = (
        std::env::var("MTP_MOUNTD_TEST_STALE_MOUNTPOINT"),
        std::env::var("MTP_MOUNTD_TEST_STALE_BACKING"),
    ) else {
        return;
    };

    let rt = tokio::runtime::Runtime::new().unwrap();
    let handle = rt.handle().clone();
    let config = VirtualDeviceConfig {
        serial: format!("stale-{}", std::process::id()),
        storages: vec![VirtualStorageConfig {
            description: STORAGE.into(),
            capacity: 1024 * 1024,
            backing_dir: PathBuf::from(backing),
            read_only: false,
        }],
        supports_partial_object_64: true,
        ..Default::default()
    };
    let opener = VirtualOpener { config };
    let device = opener.open(&handle).expect("open the virtual device");
    let mtp_fs = mtp_mount::fs::MtpFs::new(
        device,
        Arc::new(opener),
        handle,
        mtp_mount::fs::MtpFsConfig {
            read_only: false,
            spool_dir: std::env::temp_dir(),
            reconnect: mtp_mount::reconnect::ReconnectPolicy::from_secs(0),
            unplug: mtp_mount::device::UnplugSwitch::default(),
        },
    );
    let mut fuse_config = fuser::Config::default();
    fuse_config.mount_options = mtp_fs.mount_options();
    let session = fuser::spawn_mount(mtp_fs, &mountpoint, &fuse_config).expect("mount");

    // Never unmount: the parent kills this process, which is exactly how a
    // daemon leaves a mount behind.
    std::mem::forget(session);
    std::mem::forget(rt);
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

#[test]
#[ignore]
fn startup_cleans_up_a_mount_a_killed_daemon_left_behind() {
    let root = TempDir::new().unwrap();
    let backing = TempDir::new().unwrap();
    fs::write(backing.path().join("f.txt"), b"x").unwrap();
    let stale = root.path().join("KILLED001");
    fs::create_dir(&stale).unwrap();

    // A separate process so it can be killed outright, without the chance to
    // unmount: SIGKILL is what an OOM stop or a crash looks like.
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["stale_mount_helper", "--exact", "--ignored", "--nocapture"])
        .env("MTP_MOUNTD_TEST_STALE_MOUNTPOINT", &stale)
        .env("MTP_MOUNTD_TEST_STALE_BACKING", backing.path())
        .spawn()
        .expect("spawn the mounting helper");

    let deadline = Instant::now() + Duration::from_secs(20);
    while !is_mountpoint(&stale) {
        assert!(
            Instant::now() < deadline,
            "the helper never mounted at {}",
            stale.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(kernel_has_mount(&stale), "precondition: it mounted");

    child.kill().expect("kill the helper");
    child.wait().expect("reap the helper");

    // With nothing serving it, the mount is still in the kernel's table and
    // every `stat()` on it now fails. This is the directory that wedges a file
    // manager, and it's what a fresh daemon has to clear.
    assert!(kernel_has_mount(&stale), "the mount survived its process");
    assert!(
        fs::read_dir(&stale).is_err(),
        "a stale mount should not be readable"
    );
    assert!(
        is_mountpoint(&stale),
        "a stale mount is still a mount point"
    );

    let cleaned = clean_stale_mounts(root.path());

    assert_eq!(cleaned, vec![stale.clone()]);
    assert!(
        !kernel_has_mount(&stale),
        "the stale mount is still in the kernel's mount table"
    );
    assert!(!stale.exists(), "the stale mount point was left behind");
}
