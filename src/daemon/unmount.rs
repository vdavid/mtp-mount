//! Taking a mount down for good, and proving it's gone.
//!
//! This is the part of the daemon that has to work when everything else has
//! failed. A FUSE mount whose device is gone answers every `stat()` with an
//! error but is still *there*: a file manager that walks it, a shell sitting in
//! it, or a backup job crawling `$HOME` all wedge on it. Leaving one behind is
//! worse than never mounting in the first place, so the unmount path is forced,
//! it doesn't wait for the filesystem's cooperation, and it's checked
//! afterwards rather than assumed.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use log::{debug, info, warn};

/// How often [`wait_until_unmounted`] re-checks the path.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Whether `path` currently has a filesystem mounted on it.
///
/// Two ways to be a mount point, and a stale FUSE mount is the second one:
///
/// 1. `stat()` works and reports a different device number than the parent
///    directory, which is what a live mount looks like.
/// 2. `stat()` fails with `ENOTCONN` ("Transport endpoint is not connected"),
///    which is what a mount looks like after its FUSE daemon died without
///    unmounting. The kernel still has the mount; nobody is serving it.
pub fn is_mountpoint(path: &Path) -> bool {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) => return e.raw_os_error() == Some(libc::ENOTCONN),
    };
    if !metadata.is_dir() {
        return false;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    match std::fs::metadata(parent) {
        Ok(parent) => parent.dev() != metadata.dev(),
        Err(_) => false,
    }
}

/// Unmount `path` whether or not anything is still using it.
///
/// The syscall comes first: `umount2(MNT_DETACH)` on Linux and
/// `unmount(MNT_FORCE)` on macOS both detach the mount immediately, so the path
/// stops being a mount point even while a read is in flight. The in-flight
/// callers get an error, which is the correct outcome for a device that isn't
/// there any more.
///
/// The syscall needs privileges the daemon usually doesn't have for a mount it
/// made through `fusermount3`, so the setuid helper is the fallback: `-z` is
/// the same lazy detach, `-q` keeps it quiet about a path that's already clean.
///
/// **Don't replace this with `fuser`'s own unmount.** When `fuser` builds
/// against `libfuse3` (what a distro with `libfuse3-dev` installed gets), its
/// unmount is a plain `umount()`, which fails with `EBUSY` while anything holds
/// the mount. A busy mount is the normal case here: the usual reason a device
/// left is that someone pulled the cable mid-copy. This is also the only way to
/// clear a mount a *previous* daemon left, which has no `fuser` session object
/// to unmount through.
pub fn force_unmount(path: &Path) -> io::Result<()> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "mount path has a NUL byte"))?;

    #[cfg(target_os = "linux")]
    let detached = unsafe { libc::umount2(c_path.as_ptr(), libc::MNT_DETACH) } == 0;
    #[cfg(target_os = "macos")]
    let detached = unsafe { libc::unmount(c_path.as_ptr(), libc::MNT_FORCE) } == 0;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let detached = false;

    if detached {
        return Ok(());
    }
    let syscall_error = io::Error::last_os_error();
    debug!(
        "Unmounting {} directly failed ({syscall_error}); falling back to the fusermount helper",
        path.display()
    );

    for helper in ["fusermount3", "fusermount"] {
        let run = Command::new(helper)
            .args(["-u", "-q", "-z", "--"])
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match run {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => debug!("{helper} exited {status} for {}", path.display()),
            Err(e) => debug!("Can't run {helper}: {e}"),
        }
    }

    if is_mountpoint(path) {
        Err(syscall_error)
    } else {
        // Something got there first: a concurrent unmount, or the syscall
        // succeeded in a way that reported failure. Either way the job is done.
        Ok(())
    }
}

/// Wait for `path` to stop being a mount point, up to `timeout`.
///
/// The return value is the daemon's proof, not a guess: it re-`stat()`s the
/// path rather than trusting that an unmount call returned.
pub fn wait_until_unmounted(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !is_mountpoint(path) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Clean up mounts a previous daemon left behind, and return what was cleaned.
///
/// A daemon that was killed (`SIGKILL`, an OOM stop, a crash) never got to
/// unmount, so its mount points are still in the kernel's mount table with
/// nothing serving them. The next daemon has to clear them before it can reuse
/// the same paths, and it should clear them even for devices that aren't back:
/// every one of them is a directory that wedges whatever touches it.
///
/// Only *this daemon's* mount root is swept, one level deep, and only
/// directories are touched. An empty leftover directory is removed too, so the
/// root reflects what's actually plugged in.
pub fn clean_stale_mounts(root: &Path) -> Vec<PathBuf> {
    let mut cleaned = Vec::new();

    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        // No root yet is the normal first-run case, not a problem.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return cleaned,
        Err(e) => {
            warn!("Can't check {} for stale mounts: {e}", root.display());
            return cleaned;
        }
    };

    for entry in entries.flatten() {
        // `file_type` here reads the directory entry, so it works even for a
        // stale mount whose `stat()` fails with ENOTCONN.
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if is_mountpoint(&path) {
            info!("Cleaning up a mount left behind at {}", path.display());
            if let Err(e) = force_unmount(&path) {
                warn!("Can't unmount the leftover at {}: {e}", path.display());
                continue;
            }
            if !wait_until_unmounted(&path, Duration::from_secs(5)) {
                warn!("{} is still mounted after unmounting it", path.display());
                continue;
            }
            cleaned.push(path.clone());
        }
        // Empty either way now: a directory with files in it isn't ours.
        if let Err(e) = std::fs::remove_dir(&path) {
            debug!("Leaving {} in place: {e}", path.display());
        }
    }

    cleaned
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_directory_is_not_a_mountpoint() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child");
        std::fs::create_dir(&child).unwrap();
        assert!(!is_mountpoint(&child));
    }

    #[test]
    fn a_missing_path_is_not_a_mountpoint() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_mountpoint(&dir.path().join("nope")));
    }

    #[test]
    fn a_file_is_not_a_mountpoint() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(!is_mountpoint(&file));
    }

    #[test]
    fn a_missing_root_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(clean_stale_mounts(&dir.path().join("never-created")).is_empty());
    }

    #[test]
    fn sweeping_removes_empty_leftovers_but_keeps_directories_with_files() {
        let root = tempfile::tempdir().unwrap();
        let empty = root.path().join("ABC123");
        let occupied = root.path().join("not-ours");
        std::fs::create_dir(&empty).unwrap();
        std::fs::create_dir(&occupied).unwrap();
        std::fs::write(occupied.join("keep"), b"x").unwrap();

        // Nothing was mounted, so nothing is reported as cleaned.
        assert!(clean_stale_mounts(root.path()).is_empty());
        assert!(!empty.exists());
        assert!(occupied.exists());
    }

    #[test]
    fn waiting_on_a_path_that_was_never_mounted_returns_at_once() {
        let dir = tempfile::tempdir().unwrap();
        assert!(wait_until_unmounted(dir.path(), Duration::from_millis(1)));
    }
}
