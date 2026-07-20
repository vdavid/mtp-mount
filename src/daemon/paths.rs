//! Where the daemon puts its mounts, and what it calls each one.
//!
//! Mounts go under one root directory, one subdirectory per device, so a file
//! manager or a script can watch a single predictable path and see devices come
//! and go. `$XDG_RUNTIME_DIR/mtp` is that root: it's per-user, it's a tmpfs the
//! session owns, and the OS clears it at logout, so a daemon that dies without
//! cleaning up can't leave anything behind past the session.

use std::path::{Path, PathBuf};

use crate::error::MountError;
use crate::spool::CacheConvention;

/// Subdirectory of `$XDG_RUNTIME_DIR` that holds the mounts.
pub const RUNTIME_SUBDIR: &str = "mtp";

/// Longest device directory name the daemon will produce.
///
/// Serial numbers are short in practice; the cap is there so a device that
/// reports something absurd can't push the mount path past `PATH_MAX`.
const MAX_DIR_NAME: usize = 64;

/// Resolve the directory that holds all the mounts.
///
/// Pure: every input is a parameter, so the fallback chain is testable without
/// touching the real environment. Empty env values count as unset, per the XDG
/// spec.
///
/// - `override_dir`: the `--mount-root` flag, wins over everything.
/// - `xdg_runtime_dir`: `$XDG_RUNTIME_DIR`, the intended home for these.
/// - `xdg_cache_home`, `home`, `convention`: the fallback, same convention as
///   the spool dir (see [`crate::spool`]).
///
/// **Why the cache dir as the fallback, not `/tmp`.** `$XDG_RUNTIME_DIR` is
/// unset outside a logind session (a plain SSH login, a container, macOS). A
/// world-writable `/tmp/mtp` would be a trap: another user could pre-create the
/// path. The user's own cache directory is private, already resolved elsewhere
/// in this crate, and stable across reboots, which costs one stale-mount sweep
/// at startup that the daemon does anyway.
pub fn resolve_mount_root(
    override_dir: Option<&Path>,
    xdg_runtime_dir: Option<&str>,
    xdg_cache_home: Option<&str>,
    home: Option<&str>,
    convention: CacheConvention,
) -> Result<PathBuf, MountError> {
    if let Some(dir) = override_dir {
        return Ok(dir.to_path_buf());
    }

    fn non_empty(v: Option<&str>) -> Option<&str> {
        v.filter(|s| !s.is_empty())
    }

    if let Some(runtime) = non_empty(xdg_runtime_dir) {
        return Ok(Path::new(runtime).join(RUNTIME_SUBDIR));
    }

    let home = non_empty(home);
    let base = match convention {
        CacheConvention::MacOs => home
            .map(|h| Path::new(h).join("Library").join("Caches"))
            .ok_or_else(|| no_root_error("neither $XDG_RUNTIME_DIR nor $HOME is set"))?,
        CacheConvention::Xdg => match non_empty(xdg_cache_home) {
            Some(cache) => PathBuf::from(cache),
            None => home.map(|h| Path::new(h).join(".cache")).ok_or_else(|| {
                no_root_error("none of $XDG_RUNTIME_DIR, $XDG_CACHE_HOME, or $HOME is set")
            })?,
        },
    };

    Ok(base.join("mtp-mount").join("mounts"))
}

fn no_root_error(what: &str) -> MountError {
    MountError::Other(format!(
        "can't find a directory to mount devices under: {what}. \
         Pass --mount-root to say where mounts should appear."
    ))
}

/// Resolve the mount root from the live environment.
pub fn mount_root_from_env(override_dir: Option<&Path>) -> Result<PathBuf, MountError> {
    let runtime = std::env::var("XDG_RUNTIME_DIR").ok();
    let cache = std::env::var("XDG_CACHE_HOME").ok();
    let home = std::env::var("HOME").ok();
    resolve_mount_root(
        override_dir,
        runtime.as_deref(),
        cache.as_deref(),
        home.as_deref(),
        CacheConvention::current(),
    )
}

/// The directory name a device gets under the mount root.
///
/// The serial number when the device reports one, because that's what people
/// recognize and what stays the same across ports and reboots. Devices that
/// report no serial (plenty of cameras, some cheap players) fall back to
/// `usb-<vendor>-<product>-<location>`, which is deterministic for as long as
/// the device stays in the same port: unplug it and move it, and it comes back
/// under a different name. That's the honest answer, since without a serial
/// there's nothing else that survives a replug.
///
/// The name doubles as the device's identity for the daemon, so two devices
/// that report the *same* serial collide. That's rare and comes from broken
/// firmware; the supervisor logs it and leaves the first one mounted rather
/// than mounting the second over it.
///
/// Anything outside `[A-Za-z0-9._-]` becomes `_`, and a name that's only dots
/// falls back to the USB form: a serial is device-controlled input, and it ends
/// up as a path component.
pub fn device_dir_name(
    serial: Option<&str>,
    vendor_id: u16,
    product_id: u16,
    location_id: u64,
) -> String {
    let usb_form = || format!("usb-{vendor_id:04x}-{product_id:04x}-{location_id}");

    let Some(serial) = serial.map(str::trim).filter(|s| !s.is_empty()) else {
        return usb_form();
    };

    let sanitized: String = serial
        .chars()
        .take(MAX_DIR_NAME)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();

    if sanitized.chars().all(|c| c == '.') {
        return usb_form();
    }
    sanitized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_dir_wins_when_set() {
        let root = resolve_mount_root(
            None,
            Some("/run/user/1000"),
            Some("/cache"),
            Some("/home/dave"),
            CacheConvention::Xdg,
        )
        .unwrap();
        assert_eq!(root, PathBuf::from("/run/user/1000/mtp"));
    }

    #[test]
    fn override_wins_over_the_runtime_dir() {
        let root = resolve_mount_root(
            Some(Path::new("/mnt/phones")),
            Some("/run/user/1000"),
            None,
            None,
            CacheConvention::Xdg,
        )
        .unwrap();
        assert_eq!(root, PathBuf::from("/mnt/phones"));
    }

    #[test]
    fn no_runtime_dir_falls_back_to_the_cache_dir_never_tmp() {
        let root = resolve_mount_root(
            None,
            None,
            Some("/cache"),
            Some("/home/dave"),
            CacheConvention::Xdg,
        )
        .unwrap();
        assert_eq!(root, PathBuf::from("/cache/mtp-mount/mounts"));

        let root = resolve_mount_root(
            None,
            Some(""),
            None,
            Some("/home/dave"),
            CacheConvention::Xdg,
        )
        .unwrap();
        assert_eq!(root, PathBuf::from("/home/dave/.cache/mtp-mount/mounts"));

        let root = resolve_mount_root(
            None,
            None,
            Some("/cache"),
            Some("/Users/dave"),
            CacheConvention::MacOs,
        )
        .unwrap();
        assert_eq!(
            root,
            PathBuf::from("/Users/dave/Library/Caches/mtp-mount/mounts")
        );
    }

    #[test]
    fn nothing_to_go_on_names_the_flag() {
        let err = resolve_mount_root(None, None, None, None, CacheConvention::Xdg)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--mount-root"), "{err}");
    }

    #[test]
    fn serial_names_the_directory() {
        assert_eq!(
            device_dir_name(Some("2A31FDH200ABC"), 0x18d1, 0x4ee1, 3),
            "2A31FDH200ABC"
        );
        assert_eq!(
            device_dir_name(Some("  ABC123  "), 0x18d1, 0x4ee1, 3),
            "ABC123"
        );
    }

    #[test]
    fn no_serial_falls_back_to_the_usb_address() {
        assert_eq!(device_dir_name(None, 0x18d1, 0x4ee1, 3), "usb-18d1-4ee1-3");
        assert_eq!(
            device_dir_name(Some("   "), 0x04e8, 0x6860, 42),
            "usb-04e8-6860-42"
        );
    }

    #[test]
    fn a_serial_can_never_escape_the_mount_root() {
        // A device-controlled string ends up as a path component, so anything
        // that could climb out of the root has to be neutered.
        assert_eq!(device_dir_name(Some("../../etc"), 0x1, 0x2, 3), ".._.._etc");
        assert_eq!(device_dir_name(Some("a/b"), 0x1, 0x2, 3), "a_b");
        assert_eq!(device_dir_name(Some(".."), 0x1, 0x2, 3), "usb-0001-0002-3");
        assert_eq!(device_dir_name(Some("."), 0x1, 0x2, 3), "usb-0001-0002-3");
    }

    #[test]
    fn an_absurd_serial_is_truncated() {
        let name = device_dir_name(Some(&"x".repeat(500)), 0x1, 0x2, 3);
        assert_eq!(name.len(), MAX_DIR_NAME);
    }
}
