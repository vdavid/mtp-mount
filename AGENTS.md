# mtp-mount

FUSE filesystem that mounts MTP devices (Android phones, cameras) as local directories. Built on `mtp-rs` for device communication and `fuser` for the FUSE layer. Translates POSIX filesystem calls into MTP operations.

## Quick commands

| Command                              | Description                             |
|--------------------------------------|-----------------------------------------|
| `just`                               | Run all checks: format, lint, test, doc |
| `just fix`                           | Auto-fix formatting and clippy warnings |
| `just check-all`                     | Include security audit and license check|
| `cargo run -- /mnt/phone`            | Mount first available device            |
| `cargo run -- --list`                | List connected MTP devices              |

## Project structure

```
src/
  main.rs          # CLI entry point (clap)
  lib.rs           # Module re-exports for integration tests
  fs.rs            # MtpFs: implements fuser::Filesystem
  inode.rs         # Inode table: maps FUSE inodes <-> MTP object handles
  buffer.rs        # Write buffer: temp-file-backed, flushes to MTP on close
  hints.rs         # Remedies for device-open failures, shared with --help
  sparse_cache.rs  # Byte-range cache for on-demand partial reads
  error.rs         # MountError enum
tests/
  integration.rs   # FUSE mount tests against mtp-rs virtual device
```

## Architecture

```
CLI (clap)
  |
MtpFs (fuser::Filesystem)
  |                      \
InodeTable + WriteBuffer  Event monitor (tokio task)
  |                        |
mtp-rs (MtpDevice, Storage + next_event)
```

**Entry point:** `main.rs` parses CLI args, opens the MTP device via `mtp-rs`, and starts the FUSE session via `fuser`.

**Key design choices:**
- **Reads** are byte-range on-demand via `Storage::read_range`. Each open file handle has a `SparseCache` (tempfile + sorted `Vec<Range<u64>>` of populated ranges). FUSE `read(offset, size)` asks the cache for missing ranges, fetches them in 1 MB chunks, writes them into the tempfile, and serves the requested slice. No full-file download on open; supports files > 4 GB.
- **Writes** buffer to a temp file (`tempfile::tempfile()`), flushed to MTP on `release`.
- **Overwrites** use upload-then-delete-then-rename when the device supports rename. Falls back to delete-then-upload otherwise (with a warning log).
- **Async bridge:** fuser callbacks are sync, mtp-rs is async. Uses `tokio::runtime::Handle::block_on()` to bridge.
- **Locking:** single `Arc<Mutex<Inner>>` serializes all FUSE callbacks. Shared with the event monitor task. Acceptable because fuser already serializes per-mount.
- **Event monitoring:** A background tokio task polls `MtpDevice::next_event()` on a cloned device handle (cheap, Arc-backed). On `ObjectAdded`/`ObjectRemoved`/`ObjectInfoChanged` events, it invalidates the relevant directory's cache entry in `dirs_loaded`. For unknown objects (newly added on device), it invalidates all directories.

## Testing

- **Unit tests** (48): inode table, write buffer, sparse cache, device-open hints. Run with `cargo test`.
- **Integration tests** (21): mount a virtual MTP device via FUSE, exercise with `std::fs` operations including device event monitoring and partial reads. Linux only (needs `libfuse3-dev`). Run with `cargo test --test integration -- --ignored --test-threads=1`
- All tests validated on Linux (Ubuntu, aarch64)

### Working on macOS without macFUSE

`fuser`'s build script hard-fails on macOS when `pkg-config` can't find macFUSE, so a plain `cargo check` won't even compile. Two ways around it, no macFUSE install needed:

- **Type-check and unit-test**: `cargo check --all-targets --features fuser/macos-no-mount` (same for `cargo clippy` and `cargo test --lib`). This compiles the FUSE layer without the mount syscalls, so it catches every API break; it just can't mount.
- **Run the real integration tests**: use a Linux container, which is also the only place FUSE mounts work here.

  ```bash
  docker run --rm --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor:unconfined \
    -v "$PWD":/w -w /w -e CARGO_TARGET_DIR=/tmp/target rust:1-slim-bookworm \
    bash -c "apt-get update -qq && apt-get install -y -qq libfuse3-dev pkg-config fuse3 && \
             cargo test --lib && cargo test --test integration -- --ignored --test-threads=1"
  ```

  `CARGO_TARGET_DIR=/tmp/target` keeps the container's Linux artifacts out of the host `target/`.

## Design principles

- **Minimal**: correct POSIX subset, not everything
- **No data loss**: safe flush sequence protects against upload failures
- **Well-tested**: 69 tests, virtual device integration, no hardware needed

## Things to avoid

- Complex caching strategies
- Extended attributes, ACLs, or permission mapping
- Hardlinks, symlinks (MTP doesn't support them)

## Device-open failures

`hints.rs` maps a failed open to a remedy: `mtp_rs::Error::is_exclusive_access()` (gvfs on Linux, `ptpcamerad` on macOS holds the interface) and `is_permission_denied()` (missing udev rule) each get a hint; anything else prints the bare error. `main.rs` prints the hint under the error, and `long_help()` embeds the same `BUSY_HINT`/`PERMISSION_HINT` consts (via `hints::indent`) into the `--help` troubleshooting section, so the two wordings can't drift. Add new remedies as consts there, not inline in either place.

## CLI and --help

The `--help` output includes examples, troubleshooting tips, and notes about MTP
limitations. It's an important part of the user experience. When adding or changing
CLI flags, update the `after_long_help` text in `main.rs` to match. The short `-h`
output is auto-generated by clap; the long `--help` has hand-written sections.

## Code style

Run `just check` before committing. `cargo fmt`, `cargo clippy -D warnings`, tests for new functionality.
