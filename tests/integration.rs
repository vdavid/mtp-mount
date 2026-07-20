//! Integration tests for mtp-mount.
//!
//! These tests mount a virtual MTP device via FUSE and exercise the filesystem
//! with real `std::fs` operations. They require macFUSE (macOS) or FUSE (Linux).
//!
//! ```sh
//! cargo test --test integration -- --ignored --test-threads=1
//! ```

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mtp_rs::mtp::MtpDevice;
use mtp_rs::transport::virtual_device::config::{VirtualDeviceConfig, VirtualStorageConfig};
use tempfile::TempDir;

use mtp_mount::device::{DeviceOpener, UnplugSwitch};
use mtp_mount::fs::{MtpFs, MtpFsConfig};
use mtp_mount::reconnect::ReconnectPolicy;
use mtp_mount::shutdown::Shutdown;

/// Name of the extra storage the reconnect tests use to shift object handles.
const BURNER_STORAGE: &str = "Handle burner";

/// How a [`TestMount`] is put together.
struct MountSpec {
    /// Emit MTP events for changes made straight to the backing dir.
    watch_events: bool,
    /// Reconnect window in seconds (`None` keeps reconnection off entirely).
    reconnect_secs: u64,
    /// Add the decoy storage the reconnect tests need. Off by default so the
    /// other tests keep seeing a single storage in the mount root.
    handle_burner: bool,
}

impl Default for MountSpec {
    fn default() -> Self {
        Self {
            watch_events: false,
            reconnect_secs: ReconnectPolicy::DEFAULT_TIMEOUT_SECS,
            handle_burner: false,
        }
    }
}

/// Opens virtual devices for a mount, and can pretend the cable is out.
///
/// On every reopen after the first it lists the decoy storage first, which
/// burns a block of object handles so the real files come back with **different**
/// handles than the dead session handed out. The virtual device numbers handles
/// from 1 in listing order, so without this a mount that never re-resolved its
/// stale handles would still read the right bytes and the test would prove
/// nothing.
struct VirtualOpener {
    config: VirtualDeviceConfig,
    unplug: UnplugSwitch,
    opens: AtomicU32,
}

impl DeviceOpener for VirtualOpener {
    fn open(&self, rt: &tokio::runtime::Handle) -> Result<MtpDevice, mtp_rs::Error> {
        if self.unplug.is_unplugged() {
            return Err(mtp_rs::Error::Disconnected);
        }
        let device = rt.block_on(MtpDevice::builder().open_virtual(self.config.clone()))?;
        if self.opens.fetch_add(1, Ordering::SeqCst) > 0 {
            let storages = rt.block_on(device.storages())?;
            if let Some(burner) = storages
                .iter()
                .find(|s| s.info().description == BURNER_STORAGE)
            {
                rt.block_on(burner.list_objects(None))?;
            }
        }
        Ok(device)
    }

    fn describe(&self) -> String {
        "the virtual device".to_string()
    }
}

/// FUSE mount backed by a virtual MTP device.
///
/// On creation: sets up temp dirs, opens a virtual device, mounts via FUSE.
/// On drop: unmounts and cleans up.
struct TestMount {
    mount_point: TempDir,
    backing_dir: TempDir,
    fetch_counter: Arc<AtomicU64>,
    unplug: UnplugSwitch,
    shutdown: Arc<Shutdown>,
    session: Arc<Mutex<Option<fuser::BackgroundSession>>>,
}

impl TestMount {
    fn new() -> Self {
        Self::with_setup(|_| {})
    }

    /// Create a mount with device event monitoring enabled.
    /// The virtual device will emit MTP events when files change on the backing dir.
    fn with_events() -> Self {
        Self::build(
            |_| {},
            MountSpec {
                watch_events: true,
                ..Default::default()
            },
        )
    }

    /// Create a mount, calling `setup` with the backing dir path before mounting.
    /// Use this to pre-populate files in the virtual device's storage.
    fn with_setup<F: FnOnce(&Path)>(setup: F) -> Self {
        Self::build(setup, MountSpec::default())
    }

    /// A mount whose device can be unplugged, with the given reconnect window.
    fn reconnectable<F: FnOnce(&Path)>(reconnect_secs: u64, setup: F) -> Self {
        Self::build(
            setup,
            MountSpec {
                reconnect_secs,
                handle_burner: true,
                ..Default::default()
            },
        )
    }

    fn build<F: FnOnce(&Path)>(setup: F, spec: MountSpec) -> Self {
        let backing_dir = TempDir::new().expect("failed to create backing dir");
        let mount_point = TempDir::new().expect("failed to create mount point");

        setup(backing_dir.path());

        let mut storages = vec![VirtualStorageConfig {
            description: "Internal Storage".into(),
            capacity: 1024 * 1024 * 1024,
            backing_dir: backing_dir.path().to_path_buf(),
            read_only: false,
        }];
        let burner_dir = TempDir::new().expect("failed to create burner dir");
        if spec.handle_burner {
            for i in 0..64 {
                fs::write(burner_dir.path().join(format!("burn_{i}")), "x").unwrap();
            }
            storages.push(VirtualStorageConfig {
                description: BURNER_STORAGE.into(),
                capacity: 1024 * 1024,
                backing_dir: burner_dir.path().to_path_buf(),
                read_only: false,
            });
        }

        // Only the fields this suite actually exercises; the rest come from
        // `VirtualDeviceConfig::default()`, so a new mtp-rs field can't break the build.
        let config = VirtualDeviceConfig {
            serial: format!(
                "test-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ),
            storages,
            // Stated even though they match the defaults: both are load-bearing here. Reads go
            // through `Storage::read_range`, so the device must offer a partial-read op, and
            // `test_rename_file` needs rename support. If a default ever flips, this suite should
            // keep testing what it means to test.
            supports_rename: true,
            supports_partial_object_64: true,
            event_poll_interval: if spec.watch_events {
                Duration::from_millis(50)
            } else {
                Duration::ZERO
            },
            watch_backing_dirs: spec.watch_events,
            ..Default::default()
        };

        let rt = tokio::runtime::Runtime::new().expect("failed to create runtime");
        let handle = rt.handle().clone();

        let unplug = UnplugSwitch::default();
        let opener = Arc::new(VirtualOpener {
            config,
            unplug: unplug.clone(),
            opens: AtomicU32::new(0),
        });
        let device = opener.open(&handle).expect("failed to open virtual device");

        // Unlinked temp files, so the system temp dir is fine here; production
        // resolves a disk-backed spool dir under the user's cache directory.
        let mtp_fs = MtpFs::new(
            device,
            opener,
            handle,
            MtpFsConfig {
                read_only: false,
                spool_dir: std::env::temp_dir(),
                reconnect: ReconnectPolicy::from_secs(spec.reconnect_secs),
                unplug: unplug.clone(),
            },
        );
        let fetch_counter = mtp_fs.fetch_counter();
        let shutdown = mtp_fs.shutdown();
        let mount_options = mtp_fs.mount_options();

        let mut fuse_config = fuser::Config::default();
        fuse_config.mount_options = mount_options;

        // Leak the runtime and the burner dir so they stay alive for the
        // background FUSE thread.
        std::mem::forget(rt);
        std::mem::forget(burner_dir);

        let session = fuser::spawn_mount2(mtp_fs, mount_point.path(), &fuse_config)
            .expect("failed to mount FUSE filesystem");
        let session = Arc::new(Mutex::new(Some(session)));

        // Production does this in `main`: whoever owns the mount unmounts it
        // when the filesystem gives up on the device.
        {
            let shutdown = Arc::clone(&shutdown);
            let session = Arc::clone(&session);
            std::thread::spawn(move || {
                loop {
                    if shutdown.wait_timeout(Duration::from_millis(100)).is_some() {
                        break;
                    }
                    if session.lock().unwrap().is_none() {
                        return; // the test finished and unmounted already
                    }
                }
                if let Some(session) = session.lock().unwrap().take() {
                    let _ = session.umount_and_join();
                }
            });
        }

        // Wait for the mount to become ready.
        wait_for_mount(mount_point.path());

        TestMount {
            mount_point,
            backing_dir,
            fetch_counter,
            unplug,
            shutdown,
            session,
        }
    }

    /// Pretend the USB cable came out.
    fn unplug(&self) {
        self.unplug.unplug();
    }

    /// Pretend the cable goes back in after `delay`, from another thread, so the
    /// filesystem operation that's blocked on the reconnect can finish.
    fn replug_after(&self, delay: Duration) {
        let switch = self.unplug.clone();
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            switch.replug();
        });
    }

    /// Why the filesystem asked to be unmounted, if it did.
    fn shutdown_reason(&self) -> Option<String> {
        self.shutdown.reason()
    }

    /// Whether the FUSE session is still serving this mount point.
    fn is_mounted(&self) -> bool {
        self.session
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|s| !s.guard.is_finished())
    }

    /// Current count of MTP partial-read fetches.
    fn fetch_count(&self) -> u64 {
        self.fetch_counter.load(Ordering::Relaxed)
    }

    /// Path to the FUSE mount point.
    fn path(&self) -> &Path {
        self.mount_point.path()
    }

    /// Path inside the mounted storage (the virtual device exposes one storage
    /// called "Internal Storage").
    fn storage_path(&self) -> PathBuf {
        self.mount_point.path().join("Internal Storage")
    }

    /// Path to the backing directory that the virtual device serves from.
    fn backing_path(&self) -> &Path {
        self.backing_dir.path()
    }
}

/// Poll until the mount point has at least one entry (the storage dir).
fn wait_for_mount(path: &Path) {
    for _ in 0..100 {
        if let Ok(entries) = fs::read_dir(path) {
            if entries.count() > 0 {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "FUSE mount at {} did not become ready within 5 seconds",
        path.display()
    );
}

// =============================================================================
// Read operations
// =============================================================================

#[test]
#[ignore]
fn test_mount_and_list_root() {
    let mount = TestMount::new();
    let entries: Vec<_> = fs::read_dir(mount.path())
        .expect("read_dir failed")
        .filter_map(|e| e.ok())
        .collect();

    // The root should contain exactly one entry: the storage directory.
    assert_eq!(entries.len(), 1);
    let storage = &entries[0];
    assert_eq!(storage.file_name(), "Internal Storage");
    assert!(storage.file_type().unwrap().is_dir());
}

#[test]
#[ignore]
fn test_list_files() {
    let mount = TestMount::with_setup(|backing| {
        fs::write(backing.join("hello.txt"), "hello").unwrap();
        fs::write(backing.join("world.txt"), "world").unwrap();
    });

    let entries: Vec<String> = fs::read_dir(mount.storage_path())
        .expect("read_dir failed")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    assert!(entries.contains(&"hello.txt".to_string()));
    assert!(entries.contains(&"world.txt".to_string()));
}

#[test]
#[ignore]
fn test_read_file() {
    let content = "the quick brown fox jumps over the lazy dog";
    let mount = TestMount::with_setup(|backing| {
        fs::write(backing.join("test.txt"), content).unwrap();
    });

    let read_back = fs::read_to_string(mount.storage_path().join("test.txt")).expect("read failed");
    assert_eq!(read_back, content);
}

#[test]
#[ignore]
fn test_read_file_large() {
    let data: Vec<u8> = (0..1_000_000).map(|i| (i % 256) as u8).collect();
    let mount = TestMount::with_setup(|backing| {
        fs::write(backing.join("large.bin"), &data).unwrap();
    });

    let read_back = fs::read(mount.storage_path().join("large.bin")).expect("read failed");
    assert_eq!(read_back.len(), data.len());
    assert_eq!(read_back, data);
}

#[test]
#[ignore]
fn test_stat_file() {
    let content = b"stat me please";
    let mount = TestMount::with_setup(|backing| {
        fs::write(backing.join("stat.txt"), content).unwrap();
    });

    let meta = fs::metadata(mount.storage_path().join("stat.txt")).expect("metadata failed");
    assert!(meta.is_file());
    assert_eq!(meta.len(), content.len() as u64);
}

#[test]
#[ignore]
fn test_nested_directories() {
    let mount = TestMount::with_setup(|backing| {
        fs::create_dir_all(backing.join("a/b/c")).unwrap();
        fs::write(backing.join("a/b/c/deep.txt"), "deep").unwrap();
    });

    let storage = mount.storage_path();
    assert!(fs::metadata(storage.join("a")).unwrap().is_dir());
    assert!(fs::metadata(storage.join("a/b")).unwrap().is_dir());
    assert!(fs::metadata(storage.join("a/b/c")).unwrap().is_dir());

    let content =
        fs::read_to_string(storage.join("a/b/c/deep.txt")).expect("read deep file failed");
    assert_eq!(content, "deep");
}

// =============================================================================
// Write operations
// =============================================================================

#[test]
#[ignore]
fn test_create_file() {
    let mount = TestMount::new();
    let file_path = mount.storage_path().join("created.txt");

    fs::write(&file_path, "new file contents").expect("write failed");

    // Verify via the mount.
    let read_back = fs::read_to_string(&file_path).expect("read back failed");
    assert_eq!(read_back, "new file contents");

    // Verify the file landed in the backing dir.
    assert!(mount.backing_path().join("created.txt").exists());
}

#[test]
#[ignore]
fn test_mkdir() {
    let mount = TestMount::new();
    let dir_path = mount.storage_path().join("new_dir");

    fs::create_dir(&dir_path).expect("mkdir failed");

    let meta = fs::metadata(&dir_path).expect("metadata failed");
    assert!(meta.is_dir());

    assert!(mount.backing_path().join("new_dir").is_dir());
}

#[test]
#[ignore]
fn test_delete_file() {
    let mount = TestMount::with_setup(|backing| {
        fs::write(backing.join("doomed.txt"), "bye").unwrap();
    });

    let file_path = mount.storage_path().join("doomed.txt");
    assert!(file_path.exists());

    fs::remove_file(&file_path).expect("remove_file failed");
    assert!(!file_path.exists());
}

#[test]
#[ignore]
fn test_rmdir() {
    let mount = TestMount::with_setup(|backing| {
        fs::create_dir(backing.join("empty_dir")).unwrap();
    });

    let dir_path = mount.storage_path().join("empty_dir");
    assert!(dir_path.exists());

    fs::remove_dir(&dir_path).expect("rmdir failed");
    assert!(!dir_path.exists());
}

#[test]
#[ignore]
fn test_rename_file() {
    let mount = TestMount::with_setup(|backing| {
        fs::write(backing.join("old_name.txt"), "rename me").unwrap();
    });

    let storage = mount.storage_path();
    let old_path = storage.join("old_name.txt");
    let new_path = storage.join("new_name.txt");

    fs::rename(&old_path, &new_path).expect("rename failed");

    assert!(!old_path.exists());
    let content = fs::read_to_string(&new_path).expect("read renamed file failed");
    assert_eq!(content, "rename me");
}

#[test]
#[ignore]
fn test_overwrite_file() {
    let mount = TestMount::with_setup(|backing| {
        fs::write(backing.join("overwrite.txt"), "original").unwrap();
    });

    let file_path = mount.storage_path().join("overwrite.txt");

    // Overwrite with new contents.
    fs::write(&file_path, "replaced").expect("overwrite failed");

    let content = fs::read_to_string(&file_path).expect("read overwritten file failed");
    assert_eq!(content, "replaced");
}

// =============================================================================
// Edge cases
// =============================================================================

#[test]
#[ignore]
fn test_read_nonexistent() {
    let mount = TestMount::new();
    let result = fs::read(mount.storage_path().join("does_not_exist.txt"));
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().kind(), ErrorKind::NotFound);
}

#[test]
#[ignore]
fn test_empty_directory() {
    let mount = TestMount::with_setup(|backing| {
        fs::create_dir(backing.join("empty")).unwrap();
    });

    let entries: Vec<_> = fs::read_dir(mount.storage_path().join("empty"))
        .expect("read_dir failed")
        .filter_map(|e| e.ok())
        .collect();

    // std::fs::read_dir doesn't return . and .., so an empty dir yields zero entries.
    assert!(entries.is_empty());
}

#[test]
#[ignore]
fn test_concurrent_reads() {
    let mount = TestMount::with_setup(|backing| {
        fs::write(backing.join("file_a.txt"), "content A").unwrap();
        fs::write(backing.join("file_b.txt"), "content B").unwrap();
    });

    let storage = mount.storage_path();
    let path_a = storage.join("file_a.txt");
    let path_b = storage.join("file_b.txt");

    let handle_a = {
        let p = path_a.clone();
        std::thread::spawn(move || fs::read_to_string(p).expect("read A failed"))
    };
    let handle_b = {
        let p = path_b.clone();
        std::thread::spawn(move || fs::read_to_string(p).expect("read B failed"))
    };

    assert_eq!(handle_a.join().unwrap(), "content A");
    assert_eq!(handle_b.join().unwrap(), "content B");
}

// =============================================================================
// Device event monitoring (out-of-band changes on the backing dir)
// =============================================================================

/// Wait until `check` returns true, polling every 100ms for up to 5 seconds.
fn wait_until(check: impl Fn() -> bool) {
    for _ in 0..50 {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("Condition not met within 5 seconds");
}

#[test]
#[ignore]
fn test_event_file_created_on_device() {
    let mount = TestMount::with_events();
    let storage = mount.storage_path();

    // Populate the FUSE cache by listing the (empty) storage.
    let entries: Vec<_> = fs::read_dir(&storage)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(entries.len(), 0);

    // Create a file directly on the backing dir (simulating device-side change).
    fs::write(
        mount.backing_path().join("surprise.txt"),
        "hello from device",
    )
    .unwrap();

    // The file should appear in the FUSE mount after the event propagates.
    wait_until(|| {
        fs::read_dir(&storage)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name() == "surprise.txt")
    });

    let content = fs::read_to_string(storage.join("surprise.txt")).expect("read failed");
    assert_eq!(content, "hello from device");
}

#[test]
#[ignore]
fn test_event_file_deleted_on_device() {
    let mount = TestMount::with_events();
    let storage = mount.storage_path();

    // Create a file via the backing dir before the FUSE cache is populated.
    fs::write(mount.backing_path().join("doomed.txt"), "goodbye").unwrap();

    // Wait for the creation event to propagate, then verify it's visible.
    wait_until(|| {
        fs::read_dir(&storage)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name() == "doomed.txt")
    });

    // Now delete it directly on the backing dir.
    fs::remove_file(mount.backing_path().join("doomed.txt")).unwrap();

    // The file should disappear from the FUSE mount.
    wait_until(|| {
        !fs::read_dir(&storage)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name() == "doomed.txt")
    });
}

// Note: content modification events (overwriting an existing file in place) are
// intentionally not tested here. The virtual device's filesystem watcher only
// tracks file/directory creation and removal — content modifications don't change
// the MTP object tree and real MTP devices are inconsistent about emitting
// ObjectInfoChanged for content edits. See virtual_device/CLAUDE.md for details.

// =============================================================================
// Partial reads (sparse cache + download_partial_64)
// =============================================================================

/// Build a deterministic byte pattern: byte at position `i` equals `(i % 251) as u8`.
/// 251 is prime so patterns don't align with typical power-of-two boundaries,
/// making off-by-one bugs more likely to surface.
fn pattern_byte(i: u64) -> u8 {
    (i % 251) as u8
}

fn pattern_bytes(offset: u64, len: usize) -> Vec<u8> {
    (0..len as u64).map(|i| pattern_byte(offset + i)).collect()
}

#[test]
#[ignore]
fn test_read_at_arbitrary_offset() {
    const FILE_SIZE: usize = 3 * 1024 * 1024; // 3 MB
    let mount = TestMount::with_setup(|backing| {
        let data: Vec<u8> = (0..FILE_SIZE).map(|i| pattern_byte(i as u64)).collect();
        fs::write(backing.join("pattern.bin"), data).unwrap();
    });

    let path = mount.storage_path().join("pattern.bin");
    let file = fs::File::open(&path).expect("open failed");

    // Read from the middle of the file, past the first USB-chunk boundary.
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut file = file;
    file.seek(SeekFrom::Start(2_000_000)).expect("seek failed");
    let mut buf = vec![0u8; 1024];
    file.read_exact(&mut buf).expect("read failed");

    assert_eq!(buf, pattern_bytes(2_000_000, 1024));
}

#[test]
#[ignore]
fn test_seek_pattern_video_scrub() {
    // Simulate a media player scrubbing around a file: jump around, read small bursts.
    const FILE_SIZE: usize = 5 * 1024 * 1024; // 5 MB
    let mount = TestMount::with_setup(|backing| {
        let data: Vec<u8> = (0..FILE_SIZE).map(|i| pattern_byte(i as u64)).collect();
        fs::write(backing.join("video.bin"), data).unwrap();
    });

    let path = mount.storage_path().join("video.bin");
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut file = fs::File::open(&path).expect("open failed");

    let offsets: [u64; 5] = [0, 4_500_000, 1_500_000, 4_500_000, 2_500_000];
    for &offset in &offsets {
        file.seek(SeekFrom::Start(offset)).expect("seek failed");
        let mut buf = vec![0u8; 4096];
        file.read_exact(&mut buf).expect("read failed");
        assert_eq!(buf, pattern_bytes(offset, 4096), "mismatch at {offset}");
    }
}

#[test]
#[ignore]
fn test_cache_prevents_refetch() {
    const FILE_SIZE: usize = 2 * 1024 * 1024;
    let mount = TestMount::with_setup(|backing| {
        let data: Vec<u8> = (0..FILE_SIZE).map(|i| pattern_byte(i as u64)).collect();
        fs::write(backing.join("cached.bin"), data).unwrap();
    });

    let path = mount.storage_path().join("cached.bin");
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut file = fs::File::open(&path).expect("open failed");

    // First read populates the cache.
    file.seek(SeekFrom::Start(500_000)).expect("seek failed");
    let mut buf = vec![0u8; 10_000];
    file.read_exact(&mut buf).expect("read failed");
    let after_first = mount.fetch_count();
    assert!(after_first > 0, "expected at least one fetch on first read");

    // Second read of an overlapping range (fully covered by first) should not refetch.
    file.seek(SeekFrom::Start(505_000)).expect("seek failed");
    let mut buf = vec![0u8; 5000];
    file.read_exact(&mut buf).expect("read failed");
    assert_eq!(
        mount.fetch_count(),
        after_first,
        "overlapping re-read should hit cache"
    );

    // Re-reading the exact same range should also not refetch.
    file.seek(SeekFrom::Start(500_000)).expect("seek failed");
    let mut buf = vec![0u8; 10_000];
    file.read_exact(&mut buf).expect("read failed");
    assert_eq!(
        mount.fetch_count(),
        after_first,
        "identical re-read should hit cache"
    );
}

#[test]
#[ignore]
fn test_full_sequential_read() {
    // Regression check: reading a whole file sequentially (`cat`, `cp`) still works.
    const FILE_SIZE: usize = 1_000_000;
    let mount = TestMount::with_setup(|backing| {
        let data: Vec<u8> = (0..FILE_SIZE).map(|i| pattern_byte(i as u64)).collect();
        fs::write(backing.join("seq.bin"), data).unwrap();
    });

    let read = fs::read(mount.storage_path().join("seq.bin")).expect("read failed");
    assert_eq!(read.len(), FILE_SIZE);
    assert_eq!(read, pattern_bytes(0, FILE_SIZE));
}

// =============================================================================
// Reconnect (the cable comes out and goes back in)
// =============================================================================

/// Pinning down what a virtual device can and can't simulate.
///
/// `unregister_virtual_device` only removes the device from the *discovery*
/// registry: an already-open `MtpDevice` keeps its own transport and backing dir
/// and goes on answering. That's why the reconnect tests drive
/// [`UnplugSwitch`] instead: there's no way to make an open virtual device
/// report a disconnect. This test is here so the day mtp-rs grows one, it fails
/// and points at the better seam.
#[test]
fn test_unregistering_a_virtual_device_does_not_disconnect_an_open_one() {
    use mtp_rs::transport::virtual_device::registry::{
        register_virtual_device, unregister_virtual_device,
    };

    let backing_dir = TempDir::new().unwrap();
    fs::write(backing_dir.path().join("still_here.txt"), "hello").unwrap();

    let config = VirtualDeviceConfig {
        serial: format!("unregister-{}", std::process::id()),
        storages: vec![VirtualStorageConfig {
            description: "Internal Storage".into(),
            capacity: 1024 * 1024,
            backing_dir: backing_dir.path().to_path_buf(),
            read_only: false,
        }],
        ..Default::default()
    };
    let info = register_virtual_device(&config);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let device = rt
        .block_on(MtpDevice::open_by_location(info.location_id))
        .expect("failed to open the registered virtual device");

    unregister_virtual_device(info.location_id);

    let storages = rt
        .block_on(device.storages())
        .expect("an open virtual device still answers after unregistering");
    let objects = rt.block_on(storages[0].list_objects(None)).unwrap();
    assert!(
        objects.iter().any(|o| o.filename == "still_here.txt"),
        "unregistering doesn't disconnect an open device, so listings keep working"
    );

    // It does disappear from discovery, which is all it touches.
    assert!(!MtpDevice::list_devices()
        .unwrap()
        .iter()
        .any(|d| d.location_id == info.location_id));
}

#[test]
#[ignore]
fn test_reconnect_after_replug() {
    let mount = TestMount::reconnectable(30, |backing| {
        fs::write(backing.join("before.txt"), "written before the glitch").unwrap();
        fs::create_dir(backing.join("sub")).unwrap();
        fs::write(backing.join("sub/nested.txt"), "nested content").unwrap();
    });
    let storage = mount.storage_path();

    // Touch the device so the mount has cached handles to invalidate.
    assert_eq!(
        fs::read_to_string(storage.join("before.txt")).unwrap(),
        "written before the glitch"
    );

    mount.unplug();
    mount.replug_after(Duration::from_millis(400));

    // These block until the device is back, then work against the new session.
    let entries: Vec<String> = fs::read_dir(&storage)
        .expect("read_dir after replug failed")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(entries.contains(&"before.txt".to_string()));
    assert!(entries.contains(&"sub".to_string()));

    // A file inside a subdirectory exercises re-resolving a whole path chain.
    assert_eq!(
        fs::read_to_string(storage.join("sub/nested.txt")).unwrap(),
        "nested content"
    );
    assert!(mount.is_mounted(), "the mount should have survived");
    assert_eq!(mount.shutdown_reason(), None);
}

#[test]
#[ignore]
fn test_open_fd_survives_reconnect() {
    // Big enough that every read below lands on a page the kernel hasn't cached,
    // so the reads really do reach the filesystem.
    const FILE_SIZE: usize = 3 * 1024 * 1024;
    let mount = TestMount::reconnectable(30, |backing| {
        fs::create_dir(backing.join("sub")).unwrap();
        let data: Vec<u8> = (0..FILE_SIZE).map(|i| pattern_byte(i as u64)).collect();
        fs::write(backing.join("sub/movie.bin"), data).unwrap();
    });

    use std::io::{Read as _, Seek as _, SeekFrom};
    use std::os::unix::fs::MetadataExt as _;

    let path = mount.storage_path().join("sub/movie.bin");
    let mut file = fs::File::open(&path).expect("open failed");
    let inode_before = file.metadata().unwrap().ino();

    file.seek(SeekFrom::Start(0)).unwrap();
    let mut head = vec![0u8; 4096];
    file.read_exact(&mut head).expect("first read failed");
    assert_eq!(head, pattern_bytes(0, 4096));

    mount.unplug();
    mount.replug_after(Duration::from_millis(400));

    // Same file descriptor, a region that was never read: this only works if the
    // inode kept its number AND its object handle was re-resolved against the
    // new session.
    file.seek(SeekFrom::Start(2_000_000)).unwrap();
    let mut tail = vec![0u8; 8192];
    file.read_exact(&mut tail)
        .expect("read from the open fd after the reconnect failed");
    assert_eq!(tail, pattern_bytes(2_000_000, 8192));

    assert_eq!(
        file.metadata().unwrap().ino(),
        inode_before,
        "the inode number must not change across a reconnect"
    );

    // And a fresh open of the same path lands on the same inode.
    let reopened = fs::File::open(&path).expect("reopen failed");
    assert_eq!(reopened.metadata().unwrap().ino(), inode_before);
}

#[test]
#[ignore]
fn test_reconnect_disabled_unmounts_immediately() {
    let mount = TestMount::reconnectable(0, |backing| {
        fs::write(backing.join("file.txt"), "content").unwrap();
    });
    let storage = mount.storage_path();
    assert_eq!(
        fs::read_to_string(storage.join("file.txt")).unwrap(),
        "content"
    );

    let started = std::time::Instant::now();
    mount.unplug();
    // Never replugged: with reconnect off this fails right away. The iterator
    // has to be consumed: `read_dir` alone is just an `opendir`, which never
    // touches the device.
    let _ = fs::read_dir(&storage).map(Iterator::count);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a disabled reconnect must not wait around"
    );

    wait_until(|| mount.shutdown_reason().is_some());
    assert!(mount
        .shutdown_reason()
        .unwrap()
        .contains("reconnect is off"));
    wait_until(|| !mount.is_mounted());
}

#[test]
#[ignore]
fn test_reconnect_timeout_expires_and_unmounts() {
    let mount = TestMount::reconnectable(1, |backing| {
        fs::write(backing.join("file.txt"), "content").unwrap();
    });
    let storage = mount.storage_path();
    assert_eq!(
        fs::read_to_string(storage.join("file.txt")).unwrap(),
        "content"
    );

    let started = std::time::Instant::now();
    mount.unplug();
    let _ = fs::read_dir(&storage).map(Iterator::count);
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(900),
        "the mount should have waited out its window, waited {waited:?}"
    );

    wait_until(|| mount.shutdown_reason().is_some());
    assert!(mount
        .shutdown_reason()
        .unwrap()
        .contains("didn't come back within 1s"));
    wait_until(|| !mount.is_mounted());
}

#[test]
#[ignore]
fn test_write_spool_survives_reconnect() {
    let mount = TestMount::reconnectable(30, |_| {});
    let storage = mount.storage_path();
    let path = storage.join("spooled.txt");

    use std::io::Write as _;
    let mut file = fs::File::create(&path).expect("create failed");
    file.write_all(b"bytes that were spooled while the cable was fine")
        .expect("write failed");

    // The upload happens on close, and the cable is out when it starts.
    mount.unplug();
    mount.replug_after(Duration::from_millis(400));
    file.sync_all().ok();
    drop(file);

    // `create` already put an empty object on the device, so wait for the
    // spooled bytes themselves to land, not just for the name to show up.
    let landed = mount.backing_path().join("spooled.txt");
    wait_until(|| {
        fs::read_to_string(&landed).unwrap_or_default()
            == "bytes that were spooled while the cable was fine"
    });
}

// Note: files larger than 4 GB can't be tested via the virtual device because
// the virtual device's ObjectInfo builder truncates size to u32::MAX.
// The >4GB path (GetPartialObject64 with 64-bit offsets) was validated end-to-end
// on a real Pixel 9 Pro XL with an 8 GB MKV via mtp-rs's examples/test_partial_download_64.rs.
