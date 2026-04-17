# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0] - 2026-04-17

### Added

- **Partial reads**: FUSE reads now fetch only the requested byte ranges via MTP's `GetPartialObject64`, backed by a per-handle sparse cache. Opening a large file no longer triggers a full download. Supports files larger than 4 GB. Random-access patterns (media scrubbing, `tail -c`, seeking) work without re-downloading populated regions.
- `SparseCache` module (byte-range tracking + tempfile-backed storage, 15 unit tests)
- `MtpFs::fetch_counter()` for integration tests to verify cache behavior
- 5 new integration tests covering arbitrary-offset reads, video-scrub seek patterns, cache re-read suppression, full sequential reads, and files past the 4 GB boundary (sparse backing)

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
