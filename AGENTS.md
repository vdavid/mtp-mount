# mtp-mount

FUSE filesystem that mounts MTP devices (Android phones, cameras) as local directories. Built on `mtp-rs` for device communication and `fuser` for the FUSE layer. Translates POSIX filesystem calls into MTP operations.

## Quick commands

| Command                              | Description                             |
|--------------------------------------|-----------------------------------------|
| `just`                               | Run all checks: format, lint, test, doc |
| `just fix`                           | Auto-fix formatting and clippy warnings |
| `just check-all`                     | Include security audit and license check|
| `cargo run -- /mnt/phone`            | Mount first available device            |

## Project structure

```
src/
  main.rs    # CLI entry point (clap)
  lib.rs     # Module re-exports for integration tests
  fs.rs      # MtpFs: implements fuser::Filesystem
  inode.rs   # Inode table: maps FUSE inodes <-> MTP object handles
  buffer.rs  # Write buffer: temp-file-backed, flushes to MTP on close
  error.rs   # MountError enum
tests/
  integration.rs  # FUSE mount tests against mtp-rs virtual device
```

## Architecture

```
CLI (clap)
  |
MtpFs (fuser::Filesystem)
  |
InodeTable + WriteBuffer
  |
mtp-rs (MtpDevice, Storage)
```

**Entry point:** `main.rs` parses CLI args, opens the MTP device via `mtp-rs`, and starts the FUSE session via `fuser`.

**Key design choices:**
- **Reads** stream from MTP to a temp file via `download_stream`, then serve FUSE reads from disk. No full-file RAM buffering.
- **Writes** buffer to a temp file (`tempfile::tempfile()`), flushed to MTP on `release`.
- **Overwrites** use upload-then-delete-then-rename when the device supports rename. Falls back to delete-then-upload otherwise (with a warning log).
- **Async bridge:** fuser callbacks are sync, mtp-rs is async. Uses `tokio::runtime::Handle::block_on()` to bridge.
- **Locking:** single `Mutex<Inner>` serializes all FUSE callbacks. Acceptable because fuser already serializes per-mount.

## Testing

- **Unit tests** (28): inode table + write buffer, run with `cargo test`
- **Integration tests** (15): mount a virtual MTP device via FUSE, exercise with `std::fs` operations. Linux only (needs `libfuse3-dev`). Run with `cargo test --test integration -- --ignored --test-threads=1`
- All tests validated on Linux (Ubuntu, aarch64)

## Design principles

- **Minimal**: correct POSIX subset, not everything
- **No data loss**: safe flush sequence protects against upload failures
- **Well-tested**: 43 tests, virtual device integration, no hardware needed

## Things to avoid

- Complex caching strategies
- Extended attributes, ACLs, or permission mapping
- Hardlinks, symlinks (MTP doesn't support them)
- Background polling threads

## Code style

Run `just check` before committing. `cargo fmt`, `cargo clippy -D warnings`, tests for new functionality.
