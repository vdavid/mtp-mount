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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mtp_rs::mtp::MtpDevice;
use mtp_rs::transport::virtual_device::config::{VirtualDeviceConfig, VirtualStorageConfig};
use tempfile::TempDir;

use mtp_mount::fs::MtpFs;

/// FUSE mount backed by a virtual MTP device.
///
/// On creation: sets up temp dirs, opens a virtual device, mounts via FUSE.
/// On drop: unmounts and cleans up.
struct TestMount {
    mount_point: TempDir,
    backing_dir: TempDir,
    fetch_counter: Arc<AtomicU64>,
    _session: fuser::BackgroundSession,
}

impl TestMount {
    fn new() -> Self {
        Self::with_setup(|_| {})
    }

    /// Create a mount with device event monitoring enabled.
    /// The virtual device will emit MTP events when files change on the backing dir.
    fn with_events() -> Self {
        Self::build(|_| {}, true)
    }

    /// Create a mount, calling `setup` with the backing dir path before mounting.
    /// Use this to pre-populate files in the virtual device's storage.
    fn with_setup<F: FnOnce(&Path)>(setup: F) -> Self {
        Self::build(setup, false)
    }

    fn build<F: FnOnce(&Path)>(setup: F, watch_events: bool) -> Self {
        let backing_dir = TempDir::new().expect("failed to create backing dir");
        let mount_point = TempDir::new().expect("failed to create mount point");

        setup(backing_dir.path());

        let config = VirtualDeviceConfig {
            manufacturer: "Test".into(),
            model: "Virtual Device".into(),
            serial: format!("test-{}", std::process::id()),
            storages: vec![VirtualStorageConfig {
                description: "Internal Storage".into(),
                capacity: 1024 * 1024 * 1024,
                backing_dir: backing_dir.path().to_path_buf(),
                read_only: false,
            }],
            supports_rename: true,
            event_poll_interval: if watch_events {
                Duration::from_millis(50)
            } else {
                Duration::ZERO
            },
            watch_backing_dirs: watch_events,
        };

        let rt = tokio::runtime::Runtime::new().expect("failed to create runtime");
        let handle = rt.handle().clone();

        let device = rt
            .block_on(MtpDevice::builder().open_virtual(config))
            .expect("failed to open virtual device");

        let mtp_fs = MtpFs::new(device, false, handle);
        let fetch_counter = mtp_fs.fetch_counter();
        let mount_options = mtp_fs.mount_options();

        let mut fuse_config = fuser::Config::default();
        fuse_config.mount_options = mount_options;

        // Leak the runtime so it stays alive for the background FUSE thread.
        // The runtime is needed for blocking MTP calls inside fuser callbacks.
        std::mem::forget(rt);

        let session = fuser::spawn_mount2(mtp_fs, mount_point.path(), &fuse_config)
            .expect("failed to mount FUSE filesystem");

        // Wait for the mount to become ready.
        wait_for_mount(mount_point.path());

        TestMount {
            mount_point,
            backing_dir,
            fetch_counter,
            _session: session,
        }
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

#[test]
#[ignore]
fn test_read_large_file_past_4gb() {
    // Files larger than 4 GB require GetPartialObject64. We create a sparse file
    // on the backing dir (takes almost no disk space) and read bytes from near
    // the start and past the 4 GB boundary.
    const FILE_SIZE: u64 = 5 * 1024 * 1024 * 1024; // 5 GB
    const BOUNDARY: u64 = 4 * 1024 * 1024 * 1024 + 1024; // just past the 32-bit limit

    // Write a known pattern at two specific offsets in the otherwise-sparse file.
    let mount = TestMount::with_setup(|backing| {
        let path = backing.join("big.bin");
        let file = fs::File::create(&path).unwrap();
        file.set_len(FILE_SIZE).unwrap();

        use std::io::{Seek as _, SeekFrom, Write as _};
        let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();

        file.seek(SeekFrom::Start(1000)).unwrap();
        file.write_all(&pattern_bytes(1000, 256)).unwrap();

        file.seek(SeekFrom::Start(BOUNDARY)).unwrap();
        file.write_all(&pattern_bytes(BOUNDARY, 256)).unwrap();
    });

    let path = mount.storage_path().join("big.bin");
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut file = fs::File::open(&path).expect("open failed");

    // Read near the start.
    file.seek(SeekFrom::Start(1000)).expect("seek failed");
    let mut buf = vec![0u8; 256];
    file.read_exact(&mut buf).expect("read near start failed");
    assert_eq!(buf, pattern_bytes(1000, 256));

    // Read past the 32-bit boundary — this is what GetPartialObject64 enables.
    file.seek(SeekFrom::Start(BOUNDARY)).expect("seek failed");
    let mut buf = vec![0u8; 256];
    file.read_exact(&mut buf)
        .expect("read past 4 GB boundary failed");
    assert_eq!(buf, pattern_bytes(BOUNDARY, 256));
}
