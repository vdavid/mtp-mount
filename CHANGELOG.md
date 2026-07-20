# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`mtp-mountd`: plug in a phone and it's just there.** A second binary, meant to run as a `systemd --user` service, that watches for MTP devices and mounts each one as it arrives, under `$XDG_RUNTIME_DIR/mtp/<serial>/`. Unplug the device and the mount goes away immediately, forcibly, even mid-copy: a FUSE mount whose device is gone wedges every process that walks it, so leaving one behind is worse than never mounting. Several devices mount at once, a device with an SD card gets one mount with the storages as subdirectories, `SIGTERM` and `SIGINT` unmount everything, and mounts a killed daemon left behind are cleared at startup. Ships `dist/mtp-mountd.service`. Closes the manual, one-device-at-a-time gap that made mtp-mount unusable as a desktop's MTP layer ([#1](https://github.com/vdavid/mtp-mount/issues/1)).
- **A cable glitch no longer has to kill the mount** (opt in with `--reconnect-timeout`). When the device disconnects, `mtp-mount` keeps the mount alive and reopens the same device (matched on serial number) as soon as it's back, then carries on: open file descriptors keep reading, cached read data stays, and a write that was still spooling is uploaded once the device returns. Filesystem calls block while the reconnect is in flight instead of failing, and never longer than the window.
- **`--reconnect-timeout <SECONDS>`** sets how long to wait for a device that went away. **Off by default** (`0`): the first disconnect unmounts right away. Waiting is opt-in because it blocks rather than fails, so on a device that's genuinely gone every process touching the mount point, a file manager or a backup job included, freezes for the whole window. When the window runs out, `mtp-mount` says so and unmounts, instead of leaving a mount that answers every call with `EIO`.

### Fixed

- **Copying a big file to a device no longer fills RAM.** The flush on `close()` read the whole spool file into memory before sending it, so `cp bigvideo.mp4 /mnt/phone/` still peaked at the file's size in RSS even after the spool itself moved to disk. Uploads now stream from the spool in 64 KiB chunks, so memory stays flat whether the file is 4 MB or 40 GB.
- **Re-listing a directory no longer changes its files' inode numbers.** Inodes are now reused for entries that are still there under the same name, so an `ls` in one terminal can't break a file another process has open.

### Changed

- **Updated to mtp-rs 0.27.0** (from 0.26.0) for `mtp::watch_devices()`, the USB hotplug stream `mtp-mountd` is built on.
- Uploading on `close()` now reports failures: `close()` returns `EIO` when the flush didn't reach the device, instead of silently succeeding while only the log knew.

- **Big writes no longer risk an out-of-memory stop.** Write buffers and read caches spooled to `$TMPDIR`, which is a tmpfs (RAM) on most current Linux distros, so `cp bigvideo.mp4 /mnt/phone/` tried to hold the whole file in memory. They now spool to a disk-backed per-user directory: `$XDG_CACHE_HOME/mtp-mount/spool` (falling back to `~/.cache/mtp-mount/spool`) on Linux, `~/Library/Caches/mtp-mount/spool` on macOS. The files stay unlinked, so their space comes back on its own after a crash.

### Added

- **`--spool-dir <PATH>`** to put the spool somewhere else, for example a big scratch disk when your home directory is small. `mtp-mount` creates the directory if it's missing and refuses to start (naming the path) if it can't write there, rather than quietly falling back to RAM.
- **Actionable errors when another process holds the device.** A failed open now says what to do: `gio mount -l` and `systemctl --user mask gvfs-mtp-volume-monitor` for gvfs on Linux and `ptpcamerad` on macOS when the interface is busy, or the udev-rule fix when the OS denies permission. The remedies are shared with the `--help` troubleshooting section, so the two can't drift apart.

### Changed

- **Updated to mtp-rs 0.26.0** (from 0.13.1), picking up 12 releases of device fixes: a 32-bit `GetPartialObject` fallback so PTP cameras that lack the 64-bit op can be read, in-session desync self-healing (an abandoned listing no longer kills the session), USB device reset recovery for wedged Samsung devices, and lenient datetime parsing for cameras that report a null date.
- Reads now call `Storage::read_range`, which replaced `download_partial_64` in mtp-rs 0.23. Same 64-bit partial-read op, same behavior, plus the camera fallback above.
- `StorageInfo` field renames from mtp-rs 0.23 (`max_capacity` → `total_capacity`, `free_space_bytes` → `free_space`) applied to the `statfs` handler.
- The integration fixture now builds `VirtualDeviceConfig` from `..Default::default()` (added in mtp-rs 0.26), stating only the fields the suite exercises. A future mtp-rs field can't break this build again.

### Added

- Renovate config (`renovate.json`), so dependency updates land as grouped weekly PRs and mtp-rs bumps land immediately.

## [0.3.1] - 2026-04-17

### Fixed

- **Files larger than 4 GB are now fully readable.** Previous releases truncated file sizes to `u32::MAX` (the standard MTP `ObjectInfo` limit), which caused the FUSE kernel to short-circuit reads past the 4 GB mark. This release picks up mtp-rs 0.13.1, which auto-resolves the real u64 size via `GetObjectPropValue(ObjectSize)`.

### Changed

- Updated to mtp-rs 0.13.1

## [0.3.0] - 2026-04-17

### Added

- **Partial reads**: FUSE reads now fetch only the requested byte ranges via MTP's `GetPartialObject64`, backed by a per-handle sparse cache. Opening a large file no longer triggers a full download. Supports files larger than 4 GB. Random-access patterns (media scrubbing, `tail -c`, seeking) work without re-downloading populated regions.
- `SparseCache` module (byte-range tracking + tempfile-backed storage, 14 unit tests)
- `MtpFs::fetch_counter()` for integration tests to verify cache behavior
- 4 new integration tests covering arbitrary-offset reads, video-scrub seek patterns, cache re-read suppression, and full sequential reads. Files larger than 4 GB are validated end-to-end via mtp-rs's real-device test (virtual device caps `ObjectInfo` size at `u32::MAX`).

### Changed

- Updated to mtp-rs 0.13.0 (gains `Storage::download_partial_64`)
- FUSE `read()` handler rewritten; `read_cache` is now `HashMap<u64, SparseCache>` instead of `HashMap<u64, std::fs::File>`

## [0.2.0] - 2026-04-16

### Added

- **Device event monitoring**: a background task polls `MtpDevice::next_event()` and automatically invalidates cached directory listings when files are added or removed on the device (for example, taking a photo while the phone is mounted). No more stale listings after device-side changes.
- **`--list` flag**: discover connected MTP devices without mounting
- **Real storage stats**: `statfs` now reports actual device capacity and free space
- Improved `--help` with examples, troubleshooting tips, and MTP limitation notes
- 2 new integration tests for event-driven cache invalidation

### Changed

- Updated to mtp-rs 0.12.0
- `Inner` state is now `Arc<Mutex<Inner>>` (shared with the event monitor task)

## [0.1.0] - 2026-04-15

Initial release.

### Added

- **FUSE filesystem** implementing read, write, mkdir, rmdir, rename, unlink, and directory listing
- **CLI** with `--device`, `--read-only`, and `--foreground` flags
- **Temp-file-backed I/O**: reads stream from MTP to disk, writes buffer to disk before flushing. No full-file RAM buffering in the FUSE layer.
- **Safe flush**: overwrites use upload-then-delete-then-rename when the device supports rename, preventing data loss if the upload fails
- **Inode table** mapping FUSE inodes to MTP object handles with cached metadata
- 28 unit tests (inode table + write buffer) and 15 integration tests (FUSE mount against virtual MTP device)
