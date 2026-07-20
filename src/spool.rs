//! Where write buffers and read caches are spooled.
//!
//! Both the write path and the read path back their temp files with real disk,
//! never `$TMPDIR`: on most current Linux distros `/tmp` is a tmpfs, so spooling
//! a multi-gigabyte upload there fills RAM and gets the process stopped by the
//! OOM killer. The spool lives under the user's cache directory instead.
//!
//! The files stay **unlinked** (`tempfile::tempfile_in`), so a crash reclaims
//! their space with no cleanup pass and no leftovers.

use std::io;
use std::path::{Path, PathBuf};

use crate::error::MountError;

/// Cache-directory convention to follow when no explicit spool dir is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheConvention {
    /// XDG base directories: `$XDG_CACHE_HOME`, else `$HOME/.cache`.
    Xdg,
    /// macOS: `$HOME/Library/Caches`.
    MacOs,
}

impl CacheConvention {
    /// The convention for the platform this binary was built for.
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Xdg
        }
    }
}

/// Resolve the spool directory from an optional override and the environment.
///
/// Pure: every input is a parameter, so this is testable without touching the
/// real environment. Empty env values count as unset, per the XDG spec.
///
/// - `override_dir`: the `--spool-dir` flag, wins over everything.
/// - `xdg_cache_home`: `$XDG_CACHE_HOME` (ignored under [`CacheConvention::MacOs`]).
/// - `home`: `$HOME`.
pub fn resolve_spool_dir(
    override_dir: Option<&Path>,
    xdg_cache_home: Option<&str>,
    home: Option<&str>,
    convention: CacheConvention,
) -> Result<PathBuf, MountError> {
    if let Some(dir) = override_dir {
        return Ok(dir.to_path_buf());
    }

    // Empty env values count as unset, per the XDG spec.
    fn non_empty(v: Option<&str>) -> Option<&str> {
        v.filter(|s| !s.is_empty())
    }
    let home = non_empty(home);

    let base = match convention {
        CacheConvention::MacOs => home
            .map(|h| Path::new(h).join("Library").join("Caches"))
            .ok_or_else(|| {
                MountError::Other(
                    "can't find a cache directory: $HOME is not set. \
                     Pass --spool-dir to say where writes should be spooled."
                        .into(),
                )
            })?,
        CacheConvention::Xdg => match non_empty(xdg_cache_home) {
            Some(xdg) => PathBuf::from(xdg),
            None => home.map(|h| Path::new(h).join(".cache")).ok_or_else(|| {
                MountError::Other(
                    "can't find a cache directory: neither $XDG_CACHE_HOME nor $HOME is set. \
                     Pass --spool-dir to say where writes should be spooled."
                        .into(),
                )
            })?,
        },
    };

    Ok(base.join("mtp-mount").join("spool"))
}

/// Resolve the spool directory from the live environment.
pub fn spool_dir_from_env(override_dir: Option<&Path>) -> Result<PathBuf, MountError> {
    let xdg = std::env::var("XDG_CACHE_HOME").ok();
    let home = std::env::var("HOME").ok();
    resolve_spool_dir(
        override_dir,
        xdg.as_deref(),
        home.as_deref(),
        CacheConvention::current(),
    )
}

/// Create the spool directory if needed and prove it's writable.
///
/// Fails loudly, naming the path: silently falling back to `$TMPDIR` would put
/// the spool back in RAM where nobody would notice.
pub fn prepare_spool_dir(dir: &Path) -> Result<(), MountError> {
    std::fs::create_dir_all(dir).map_err(|e| spool_error(dir, e))?;
    // The write path only ever makes unlinked temp files here, so making one is
    // both the honest permission check and a no-op on success.
    tempfile::tempfile_in(dir).map_err(|e| spool_error(dir, e))?;
    Ok(())
}

fn spool_error(dir: &Path, source: io::Error) -> MountError {
    MountError::Other(format!(
        "can't use the spool directory {}: {source}. \
         Pass --spool-dir to point it at a writable directory on disk.",
        dir.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_cache_home_wins_when_set() {
        let dir = resolve_spool_dir(
            None,
            Some("/cache"),
            Some("/home/dave"),
            CacheConvention::Xdg,
        )
        .unwrap();
        assert_eq!(dir, PathBuf::from("/cache/mtp-mount/spool"));
    }

    #[test]
    fn xdg_unset_falls_back_to_home_cache() {
        let dir = resolve_spool_dir(None, None, Some("/home/dave"), CacheConvention::Xdg).unwrap();
        assert_eq!(dir, PathBuf::from("/home/dave/.cache/mtp-mount/spool"));
    }

    #[test]
    fn empty_xdg_counts_as_unset() {
        let dir =
            resolve_spool_dir(None, Some(""), Some("/home/dave"), CacheConvention::Xdg).unwrap();
        assert_eq!(dir, PathBuf::from("/home/dave/.cache/mtp-mount/spool"));
    }

    #[test]
    fn override_wins_over_env() {
        let dir = resolve_spool_dir(
            Some(Path::new("/mnt/scratch")),
            Some("/cache"),
            Some("/home/dave"),
            CacheConvention::Xdg,
        )
        .unwrap();
        assert_eq!(dir, PathBuf::from("/mnt/scratch"));
    }

    #[test]
    fn macos_uses_library_caches_and_ignores_xdg() {
        let dir = resolve_spool_dir(
            None,
            Some("/cache"),
            Some("/Users/dave"),
            CacheConvention::MacOs,
        )
        .unwrap();
        assert_eq!(
            dir,
            PathBuf::from("/Users/dave/Library/Caches/mtp-mount/spool")
        );
    }

    #[test]
    fn no_home_and_no_xdg_errors() {
        let err = resolve_spool_dir(None, None, None, CacheConvention::Xdg).unwrap_err();
        assert!(err.to_string().contains("--spool-dir"));

        let err =
            resolve_spool_dir(None, Some("/cache"), None, CacheConvention::MacOs).unwrap_err();
        assert!(err.to_string().contains("--spool-dir"));
    }

    #[test]
    fn prepare_creates_missing_directory() {
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("mtp-mount").join("spool");
        prepare_spool_dir(&dir).unwrap();
        assert!(dir.is_dir());
    }

    #[test]
    fn prepare_reports_the_path_it_could_not_use() {
        // A regular file where the directory should be: `create_dir_all` fails.
        let parent = tempfile::tempdir().unwrap();
        let blocked = parent.path().join("not-a-dir");
        std::fs::write(&blocked, b"x").unwrap();
        let err = prepare_spool_dir(&blocked).unwrap_err().to_string();
        assert!(err.contains(&blocked.display().to_string()), "{err}");
    }
}
