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
  device.rs        # DeviceOpener trait (how the mount (re)opens its device) + UnplugSwitch
  reconnect.rs     # ReconnectPolicy: the timeout window and its backoff schedule
  shutdown.rs      # One-way "take this mount down" signal
  hints.rs         # Remedies for device-open failures, shared with --help
  spool.rs         # Resolves and prepares the disk-backed spool directory
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
- **Writes** buffer to a temp file in the spool dir, flushed to MTP on `release`.
- **Spool directory**: both temp files (write buffers and sparse read caches) are created with `tempfile::tempfile_in(spool_dir)`, never plain `tempfile::tempfile()`. `$TMPDIR` is a tmpfs on most current Linux distros, so a whole-file write buffer there is RAM and a big `cp` OOMs. `spool.rs::resolve_spool_dir` is pure (env values in, path out) and picks `$XDG_CACHE_HOME/mtp-mount/spool` → `$HOME/.cache/mtp-mount/spool` on Linux, `$HOME/Library/Caches/mtp-mount/spool` on macOS; `--spool-dir` overrides. `main` resolves and prepares it *before* opening the device, and exits with the path named if it isn't writable. Never fall back to `$TMPDIR` on failure: that restores the OOM bug invisibly. The files stay **unlinked**, so a crash reclaims the space with no cleanup pass; don't switch them to named temp files.
- **Overwrites** use upload-then-delete-then-rename when the device supports rename. Falls back to delete-then-upload otherwise (with a warning log).
- **Async bridge:** fuser callbacks are sync, mtp-rs is async. Uses `tokio::runtime::Handle::block_on()` to bridge.
- **Locking:** single `Arc<Mutex<Inner>>` serializes all FUSE callbacks. Shared with the event monitor task. Acceptable because fuser already serializes per-mount.
- **Event monitoring:** A background tokio task polls `MtpDevice::next_event()` on a cloned device handle (cheap, Arc-backed). On `ObjectAdded`/`ObjectRemoved`/`ObjectInfoChanged` events, it invalidates the relevant directory's cache entry in `dirs_loaded`. For unknown objects (newly added on device), it invalidates all directories. Each loop belongs to one session (`event_epoch`); a reconnect bumps the epoch and the old loop exits on its next tick.
- **Reconnect:** the mount survives a device that disconnects and comes back. See "Surviving a disconnect" below.

## Surviving a disconnect

A flaky cable drops the device and brings it back. The mount stays up, reopens the same device, and resumes.

**The flow.** Every MTP call goes through `MtpFs::with_reconnect`, which runs a closure, and on a session-loss error (`device::is_link_lost`: `Disconnected`, `DeviceReset`, `NoDevice`) calls `reconnect()` and runs the closure again, up to `MAX_ATTEMPTS`. `reconnect()` sleeps through `ReconnectPolicy::schedule()` (capped exponential backoff, total exactly the window), asking the `DeviceOpener` for the device each time. Success goes to `adopt()`; running out of window calls `give_up()`.

**Why the closure, not the call.** `with_reconnect` re-runs the whole closure, and each closure re-resolves its own handles and storage index. A closure that captured a handle from the dead session would retry with a token the new session has never heard of.

**Handles: lazy re-resolution by path, inodes fixed.** `ObjectHandle` and `StorageId` are session-scoped opaque tokens, so a reconnect invalidates every one of them. FUSE inode numbers must NOT change: the kernel caches them and open file descriptors point at them, so renumbering breaks reads on an fd that was open across the glitch. So `adopt()` keeps the inode tree exactly as it is and only bumps `InodeTable`'s generation counter, which marks every cached handle stale. `ensure_fresh(inode)` then re-resolves on demand: it walks up to the storage root, then lists back down matching each component by name, writing fresh handles into the same inodes via `set_handle`. Lazy beats eager here because most inodes are never touched again, and a big tree would make the reconnect itself slow.

Storage IDs are re-mapped eagerly instead (there are only a few): `adopt()` matches the new `Storage` list to the existing storage inodes by name, falling back to position for devices that report no description.

**What survives.** Open file handles (`fh_to_inode`), sparse read caches (already-fetched bytes are still valid file content), and write spools. A flush interrupted by a disconnect is retried from the start of the spool file once the device is back: the spool is an unlinked temp file, so the bytes are safe. Only the *upload* step is retried; once it lands, the following delete and rename report and return `Ok`, because re-uploading would duplicate the data. A retry first clears any half-uploaded `.~tmp~<name>` left in the target directory.

**Blocking.** A FUSE call that hits the disconnect blocks (holding the inner lock, so the mount is effectively frozen) until the device returns or the window expires. That's deliberate: waiting a few seconds beats an `EIO` that makes `cp` abandon a 4 GB copy. It's bounded by `--reconnect-timeout`, so it can't hang forever.

**Giving up.** `give_up()` prints why and raises the `Shutdown` signal. It can't unmount itself: a FUSE callback holds the inode lock and still owes the kernel a reply, and `fuser` hands the unmount handle to whoever mounted the filesystem. So `main` (and the test harness) watch the signal and call `umount_and_join`. That's also why `main` uses `spawn_mount2` rather than `mount2`: `Session::run` is `pub(crate)`, so the only way to keep a thread free for the watch loop is the background session.

## Testing

- **Unit tests** (72): inode table, write buffer, sparse cache, spool-dir resolution, device-open hints, reconnect policy, shutdown signal. Run with `cargo test`.
- **Integration tests** (27): mount a virtual MTP device via FUSE, exercise with `std::fs` operations including device event monitoring, partial reads, and reconnects. Linux only (needs `libfuse3-dev`), except the one non-ignored test below. Run with `cargo test --test integration -- --ignored --test-threads=1`

### Testing a disconnect

`mtp-rs` can't simulate one. `unregister_virtual_device` only removes a device from the *discovery* registry: an already-open `MtpDevice` keeps its transport and backing dir and answers as if nothing happened (pinned down by `test_unregistering_a_virtual_device_does_not_disconnect_an_open_one`, the one test in that file that isn't `#[ignore]`). So the seam is `device::UnplugSwitch`, an `Arc<AtomicBool>` the mount and the `DeviceOpener` share: while it's set, every MTP op fails `Disconnected` and reopening fails too. Production never flips it.

The other trap is that the virtual device numbers handles from 1 in listing order, so a reopened device hands out the *same* handles and a mount that never re-resolved anything would still read the right bytes. The reconnect fixture defeats that with a decoy "Handle burner" storage: on every reopen the test's opener lists it first, burning 64 handles, so the real files come back with handles that can't collide with the dead session's. `test_open_fd_survives_reconnect` is the test that actually pins the re-resolution down (verified: it fails, reading zeros, if `bump_generation` is removed).
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
- **Well-tested**: 77 tests, virtual device integration, no hardware needed

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
