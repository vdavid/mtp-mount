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
    _session: fuser::BackgroundSession,
}

impl TestMount {
    fn new() -> Self {
        Self::with_setup(|_| {})
    }

    /// Create a mount, calling `setup` with the backing dir path before mounting.
    /// Use this to pre-populate files in the virtual device's storage.
    fn with_setup<F: FnOnce(&Path)>(setup: F) -> Self {
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
            event_poll_interval: Duration::ZERO,
            watch_backing_dirs: false,
        };

        let rt = tokio::runtime::Runtime::new().expect("failed to create runtime");
        let handle = rt.handle().clone();

        let device = rt
            .block_on(MtpDevice::builder().open_virtual(config))
            .expect("failed to open virtual device");

        let mtp_fs = MtpFs::new(device, false, handle);
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
            _session: session,
        }
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
