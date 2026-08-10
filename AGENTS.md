# mtp-mount

FUSE filesystem that mounts MTP devices (Android phones, cameras) as local directories. Built on `mtp-rs` for device communication and `fuser` for the FUSE layer. Translates POSIX filesystem calls into MTP operations.

Two binaries over one library: `mtp-mount` mounts one device where a person asked, `mtp-mountd` is a daemon that mounts every device as it's plugged in. Same `MtpFs` underneath.

## Quick commands

| Command                              | Description                             |
|--------------------------------------|-----------------------------------------|
| `just`                               | Run all checks: format, lint, test, doc |
| `just fix`                           | Auto-fix formatting and clippy warnings |
| `just check-all`                     | Include security audit and license check|
| `cargo run -- /mnt/phone`            | Mount first available device            |
| `cargo run -- --list`                | List connected MTP devices              |
| `cargo run --bin mtp-mountd`         | Run the auto-mounting daemon            |

## Project structure

```
src/
  main.rs          # CLI entry point (clap), the `mtp-mount` binary
  bin/mtp-mountd.rs# Daemon entry point: paths, USB watch, signals, then Supervisor::run
  lib.rs           # Module re-exports for integration tests
  fs.rs            # MtpFs: implements fuser::Filesystem
  inode.rs         # Inode table: maps FUSE inodes <-> MTP object handles
  buffer.rs        # Write buffer: temp-file-backed, flushes to MTP on close
  device.rs        # DeviceOpener trait (how the mount (re)opens its device) + UnplugSwitch
  reconnect.rs     # ReconnectPolicy: the timeout window and its backoff schedule
  shutdown.rs      # One-way "take this mount down" signal
  hints.rs         # Remedies for device-open failures, shared with --help
  spool.rs         # Resolves and prepares the disk-backed spool directory
  daemon/          # The mtp-mountd half (see "The daemon" below)
    supervisor.rs  # The mount-owning loop, driven by a Command channel (the test seam)
    usb.rs         # Production wiring: mtp-rs hotplug stream -> Commands
    dryrun.rs      # --dry-run: report what would be mounted, mount nothing
    paths.rs       # Mount root resolution, device directory naming
    unmount.rs     # Forced unmount, mountpoint detection, stale-mount sweep
  sparse_cache.rs  # Byte-range cache for on-demand partial reads
  error.rs         # MountError enum
tests/
  integration.rs   # FUSE mount tests against mtp-rs virtual device
  daemon.rs        # Daemon supervisor tests: synthetic hotplug -> real FUSE mounts
  dry_run.rs       # --dry-run reporting: key matching, verdict wording, touches-nothing
dist/
  mtp-mountd.service # systemd --user unit
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
- **Writes** buffer to a temp file in the spool dir, flushed to MTP on `release`. The flush hands `Storage::upload` a
  lazy stream over the spool file (`fs::file_stream`, 64 KiB per chunk), so an upload holds one chunk in memory
  regardless of file size. Don't collect those chunks into a `Vec` first: that puts the whole file back in RAM, which
  is the bug the lazy stream exists to prevent.
- **Spool directory**: both temp files (write buffers and sparse read caches) are created with `tempfile::tempfile_in(spool_dir)`, never plain `tempfile::tempfile()`. `$TMPDIR` is a tmpfs on most current Linux distros, so a whole-file write buffer there is RAM and a big `cp` OOMs. `spool.rs::resolve_spool_dir` is pure (env values in, path out) and picks `$XDG_CACHE_HOME/mtp-mount/spool` → `$HOME/.cache/mtp-mount/spool` on Linux, `$HOME/Library/Caches/mtp-mount/spool` on macOS; `--spool-dir` overrides. `main` resolves and prepares it *before* opening the device, and exits with the path named if it isn't writable. Never fall back to `$TMPDIR` on failure: that restores the OOM bug invisibly. The files stay **unlinked**, so a crash reclaims the space with no cleanup pass; don't switch them to named temp files.
- **Overwrites** use upload-then-delete-then-rename when the device supports rename. Falls back to delete-then-upload otherwise (with a warning log).
- **Async bridge:** fuser callbacks are sync, mtp-rs is async. Uses `tokio::runtime::Handle::block_on()` to bridge.
- **Locking:** single `Arc<Mutex<Inner>>` serializes all FUSE callbacks. Shared with the event monitor task. Acceptable because fuser already serializes per-mount: `fuser::Config::default()` leaves `n_threads` at 1, so one event-loop thread dispatches every request and doesn't read the next one until the current callback returns. That's why dropping the lock around a long upload would buy nothing today; if you ever want concurrent callbacks, raise `n_threads` first (Linux only in fuser 0.17), and only then does the lock's scope matter.
- **Event monitoring:** A background tokio task polls `MtpDevice::next_event()` on a cloned device handle (cheap, Arc-backed). On `ObjectAdded`/`ObjectRemoved`/`ObjectInfoChanged` events, it invalidates the relevant directory's cache entry in `dirs_loaded`. For unknown objects (newly added on device), it invalidates all directories. Each loop belongs to one session (`event_epoch`); a reconnect bumps the epoch and the old loop exits on its next tick.
- **Reconnect:** the mount survives a device that disconnects and comes back. See "Surviving a disconnect" below.

## Surviving a disconnect

A flaky cable drops the device and brings it back. The mount stays up, reopens the same device, and resumes.

**The flow.** Every MTP call goes through `MtpFs::with_recovery`, which runs a closure, and on a session-loss error (`device::is_link_lost`: `Disconnected`, `DeviceReset`, `NoDevice`) calls `reconnect()` and runs the closure again, up to `MAX_ATTEMPTS`. `reconnect()` sleeps through `ReconnectPolicy::schedule()` (capped exponential backoff, total exactly the window), asking the `DeviceOpener` for the device each time. Success goes to `adopt()`; running out of window calls `give_up()`. `is_link_lost` is deliberately broader than `mtp_rs::Error::is_disconnected()`, which is `Disconnected` alone: a mount asks "does this session need a reopen?", and after a `DeviceReset` it does, so don't swap ours out for the upstream predicate.

**Why the closure, not the call.** `with_recovery` re-runs the whole closure, and each closure re-resolves its own handles and storage index. A closure that captured a handle from the dead session would retry with a token the new session has never heard of.

**Handles: lazy re-resolution by path, inodes fixed.** `ObjectHandle` and `StorageId` are session-scoped opaque tokens, so a reconnect invalidates every one of them. FUSE inode numbers must NOT change: the kernel caches them and open file descriptors point at them, so renumbering breaks reads on an fd that was open across the glitch. So `adopt()` keeps the inode tree exactly as it is and only bumps `InodeTable`'s generation counter, which marks every cached handle stale. `ensure_fresh(inode)` then re-resolves on demand: it walks up to the storage root, then lists back down matching each component by name, writing fresh handles into the same inodes via `set_handle`. Lazy beats eager here because most inodes are never touched again, and a big tree would make the reconnect itself slow.

Storage IDs are re-mapped eagerly instead (there are only a few): `adopt()` matches the new `Storage` list to the existing storage inodes by name, falling back to position for devices that report no description.

**What survives.** Open file handles (`fh_to_inode`), sparse read caches (already-fetched bytes are still valid file content), and write spools. A flush interrupted by a disconnect is retried from the start of the spool file once the device is back: the spool is an unlinked temp file, so the bytes are safe. Only the *upload* step is retried; once it lands, the following delete and rename report and return `Ok`, because re-uploading would duplicate the data. A retry first clears any half-uploaded `.~tmp~<name>` left in the target directory.

**Blocking, and why reconnect is OFF by default.** A FUSE call that hits the disconnect blocks (holding the inner lock, so the mount is effectively frozen) until the device returns or the window expires. Waiting beats an `EIO` that makes `cp` abandon a 4 GB copy, which is why the behavior exists at all. But it blocks EVERY process touching the mount point, not just the one doing the transfer, so a file manager or backup job walking the mount freezes for the whole window on a device that's really gone. A frozen desktop is a worse default than a mount that disappears, so `ReconnectPolicy::DEFAULT_TIMEOUT_SECS` is `0` and users opt in per-cable. Don't flip the default back without solving the blocking (that means not holding the inner lock across the wait, which in turn needs `fuser`'s `n_threads` raised above 1).

**Giving up.** `give_up()` prints why and raises the `Shutdown` signal. It can't unmount itself: a FUSE callback holds the inode lock and still owes the kernel a reply, and `fuser` hands the unmount handle to whoever mounted the filesystem. So `main` (and the test harness) watch the signal and call `umount_and_join`. That's also why `main` uses `spawn_mount` rather than `mount`: `Session::run` is `pub(crate)`, so the only way to keep a thread free for the watch loop is the background session.

## Surviving a re-keyed handle

Android's MediaProvider re-keys object IDs across a media rescan, so a handle the mount cached when it
last listed a folder is silently invalidated. `mtp-rs` reports that as `Error::StaleHandle`
(`is_stale_handle()`, mapped from `InvalidObjectHandle`/`InvalidParentObject`) and its own docs are
explicit that a host must re-list the parent, re-resolve, and retry once rather than fail. Before this
existed, a write-then-read-back on a device with a live rescan came back as `EIO`.

**It is a sibling of the reconnect path, not a member of it.** `with_recovery` handles both, but a stale
handle takes a different branch: the session is fine and only the token is dead, so it calls
`MtpFs::invalidate_handles` (bump the `InodeTable` generation, drop the cached listings) and re-runs the
closure immediately, against the same device. **Never route it through `reconnect()`.** Reopening a
healthy device is a real regression: on Android a reopen is expensive and can wedge the device, and the
mount would freeze for the whole reconnect window over a token that a single listing would have fixed.
`is_link_lost` is what decides a reopen, and `StaleHandle` must stay out of it.

**One retry, on its own budget.** `MAX_STALE_RETRIES` is 1, per `mtp-rs`'s guidance: the first
`StaleHandle` means a re-key, and a fresh listing has the new token. A second one for the same operation
means the freshly resolved token died too, which isn't a re-key any more, so the error goes to the
caller instead of looping. The budget is separate from `MAX_ATTEMPTS` because the two failures cost
different things (a listing against a healthy session versus a reopen plus a backoff wait), and because
a stale handle must never fall through into the reconnect path when it runs out.

**Whole-table invalidation, lazily re-resolved.** `invalidate_handles` marks every cached handle stale
rather than just the one that failed. A device that re-keys re-keys in batches, so the neighbours are
suspect too, and the generation counter makes marking free: `ensure_fresh` re-resolves by path on
demand, so the only inodes that pay for a listing are the ones something touches again.

## The daemon (`mtp-mountd`)

```
mtp::watch_devices()  (mtp-rs, USB hotplug)
  |  daemon::usb::spawn_hotplug_watch  (tokio task)
  v
Sender<Command> ------------------> Supervisor::run(Receiver<Command>)   <-- SEAM
  ^                                   |            |
  |                                   |            +-- unmount: force_unmount + verify + rmdir
signal handler (SIGTERM/SIGINT)       +-- mount: DeviceSource -> DeviceOpener -> MtpFs -> spawn_mount
give-up watcher threads (one per mount, one per Shutdown signal)
```

**The seam is the command channel, and it's the whole reason this is testable.** USB hotplug can't be simulated: no
container can be made to believe a phone was plugged in. So `Supervisor` never calls `watch_devices()`. Its entire input
is `Receiver<Command>` (`Device(Arrived|Left)`, `GiveUp`, `Stop`), and `daemon::usb` is the only thing that knows about
USB. A test sends an `Arrived` and gets a real FUSE mount over a real `mtp-rs` virtual device at a real path;
everything below the channel is the production path. The second seam is `DeviceSource`, which hands back a
`DeviceOpener`: production returns `UsbOpener` (matched on serial), tests return one that opens a virtual device.
Don't move USB code into the supervisor, and don't let tests reach past the channel.

**Threading.** `Supervisor::run` blocks its thread and must NOT run inside the tokio runtime: opening a device and
mounting bridge async to sync with `Handle::block_on`, which panics on a runtime thread. `main` runs it on the main
thread; the runtime carries the hotplug watch and the signal handler.

**Mount paths.** `$XDG_RUNTIME_DIR/mtp/<key>/`, where `<key>` is the sanitized serial number, or
`usb-<vid>-<pid>-<location>` when the device reports none. That one string is both the directory name AND the
supervisor's identity for the device, so "already mounted?" and "where does it go?" can't disagree; it has to be
derivable from what BOTH an arrival and a departure report, since a departure is all the supervisor gets to know which
mount to take down. Two devices reporting the same serial therefore collide: the second is logged and ignored, not
mounted over the first. Serials are device-controlled and become a path component, so `device_dir_name` sanitizes them
(`../..` can't escape the root). No `$XDG_RUNTIME_DIR` falls back to the cache dir, never `/tmp` (world-writable).

**Unmount is the correctness property, and it's forced by our own code.** Do NOT rely on `fuser`'s unmount: built
against `libfuse3` (any distro with `libfuse3-dev`) it's a plain `umount()`, which returns `EBUSY` whenever anything
holds the mount, and a busy mount is the normal case (the cable came out mid-copy). `daemon::unmount::force_unmount`
does `umount2(MNT_DETACH)` on Linux / `unmount(MNT_FORCE)` on macOS, falling back to `fusermount3 -u -z` when the
syscall is refused. The supervisor calls that FIRST, then hands `umount_and_join` to a side thread purely to reap the
session (the join can block as long as the in-flight FUSE callback does, and the loop must not wait on it), then
CHECKS with `wait_until_unmounted` before removing the directory. A test caught the EBUSY path; keep the check.

**Stale mounts.** A daemon that was `SIGKILL`ed leaves mounts with nothing serving them: `stat()` on them fails with
`ENOTCONN` and they wedge whatever walks them. `clean_stale_mounts` sweeps the mount root one level deep at startup.
`is_mountpoint` treats both a differing `st_dev` and an `ENOTCONN` failure as "mounted", which is what makes the stale
case detectable at all.

**Reconnect is off (`ReconnectPolicy::from_secs(0)`) and must stay off.** The daemon has a better answer than waiting:
a device that comes back arrives as a fresh hotplug event and is mounted again at the same path, with nothing frozen
in between. Turning it on would trade a mount that reappears for a desktop that hangs (see "Surviving a disconnect").

**`--dry-run` is how the hotplug path gets checked at all** (`daemon/dryrun.rs`). Nothing below `spawn_hotplug_watch`
can be tested: USB can't be simulated, so `ident_of` never meets a real `MtpDeviceInfo` in CI. The silent failure that
hides there is the mount key: derive it differently for an arrival and its departure and every departure matches
nothing, so the daemon leaks mount points for as long as it runs. `--dry-run` watches real hotplug and prints the raw
fields, the derived key, the path it would mount at, and, on each departure, whether the key MATCHES the arrival, plus
a running tally and a closing summary. It creates nothing: no mount, no mount root, no mount point, no stale sweep, and
the device is never opened, so it works on a machine with no FUSE (which is the point on macOS).

Two things keep it honest. First, `usb::ident_of` goes *through* `DeviceFacts::ident()`, so the dry run reports the key
a real mount would use rather than a second derivation that could agree in a test and disagree on a cable. Second, the
reporter has the supervisor's shape: its whole input is a channel of `DryRunCommand`s, `usb::spawn_dry_run_watch` is
the only USB-aware part, and `tests/dry_run.rs` injects synthetic arrivals and departures. The commands carry
`DeviceFacts` rather than `DeviceIdent` because a dry run has to show its evidence, and because `MtpDeviceInfo` is
`#[non_exhaustive]`: nothing outside `mtp-rs` can build one, so `DeviceFacts` is what makes a real device's fields
testable data at all.

**Multiple storages**: one mount per device, storages as subdirectories. That's what `MtpFs` already does (each
`Storage` is a directory under the mount root); the daemon adds nothing. Fewer mounts means less to leak.

**Give-up watchers.** A mount raises its `Shutdown` signal from inside a FUSE callback and can't act on it, so each
mount gets a thread that turns that signal into `Command::GiveUp`. This can beat the hotplug departure: the mount
notices a dead session on its next operation, the USB watch only on its next poll.

## Testing

- **Unit tests** (99): inode table, write buffer, sparse cache, upload streaming, spool-dir resolution, device-open hints, reconnect policy, shutdown signal, mount-root resolution, device directory naming, mountpoint detection, stale-mount sweep, dry-run key derivation. Run with `cargo test`.
- **Integration tests** (29): mount a virtual MTP device via FUSE, exercise with `std::fs` operations including device event monitoring, partial reads, reconnects, and re-keyed handles. Linux only (needs `libfuse3-dev`), except the one non-ignored test below. Run with `cargo test --test integration -- --ignored --test-threads=1`
- **Daemon tests** (8, `tests/daemon.rs`): drive `Supervisor` through its command channel and assert against the real filesystem. Linux only, `cargo test --test daemon -- --ignored --test-threads=1`. See below.
- **Dry-run tests** (7, `tests/dry_run.rs`): inject arrivals and departures into the `--dry-run` reporter and assert on the verdict, the wording a person reads, and that a run leaves a temp mount root untouched. No FUSE, no device, so they run everywhere with plain `cargo test`.

### Testing a disconnect

`mtp-rs` can't simulate one. `unregister_virtual_device` only removes a device from the *discovery* registry: an already-open `MtpDevice` keeps its transport and backing dir and answers as if nothing happened (pinned down by `test_unregistering_a_virtual_device_does_not_disconnect_an_open_one`, the one test in that file that isn't `#[ignore]`). So the seam is `device::UnplugSwitch`, an `Arc<AtomicBool>` the mount and the `DeviceOpener` share: while it's set, every MTP op fails `Disconnected` and reopening fails too. Production never flips it.

The other trap is that the virtual device numbers handles from 1 in listing order, so a reopened device hands out the *same* handles and a mount that never re-resolved anything would still read the right bytes. The reconnect fixture defeats that with a decoy "Handle burner" storage: on every reopen the test's opener lists it first, burning 64 handles, so the real files come back with handles that can't collide with the dead session's. `test_open_fd_survives_reconnect` is the test that actually pins the re-resolution down (verified: it fails, reading zeros, if `bump_generation` is removed).

### Testing the daemon

`tests/daemon.rs` sends synthetic `Arrived`/`Left` commands and checks what actually happened on disk. Two things
worth keeping:

- **Proving a mount is gone** means the kernel's own record, not a dropped Rust value. `assert_really_unmounted` reads
  `/proc/self/mountinfo`, re-`stat()`s the path, and requires the directory to be removable (`rmdir` on a live
  mountpoint fails with `EBUSY`). An assertion that only checked a struct was dropped would pass over the `EBUSY`
  unmount bug this suite found.
- **A genuinely stale mount** needs a process that dies while mounted, so `startup_cleans_up_a_mount_a_killed_daemon_left_behind`
  re-runs this same test binary as a child (`stale_mount_helper`, a `#[test]` that does nothing unless the parent set
  `MTP_MOUNTD_TEST_STALE_*`), waits for it to mount, `SIGKILL`s it, and then sweeps. That's the only way to reach the
  `ENOTCONN` branch in `is_mountpoint`.

The virtual devices here run with the backing-dir watcher ON, and that's load-bearing. The mount's own writes land in
the backing dir, the watcher re-keys the object handles, and the write-then-read-back in
`two_devices_mount_at_distinct_paths_and_work_independently` runs straight into a stale handle. That's the exact bug
"Surviving a re-keyed handle" fixes, so the suite is a regression test for it: turn the watcher off and the coverage
is gone (verified by reverting the fix, which fails that test with `EIO`).

- All tests validated on Linux (Ubuntu, aarch64)

### Working on macOS without macFUSE

`fuser`'s build script hard-fails on macOS when `pkg-config` can't find macFUSE, so a plain `cargo check` won't even compile. Two ways around it, no macFUSE install needed:

- **Type-check and unit-test**: `cargo check --all-targets --features fuser/macos-no-mount` (same for `cargo clippy` and `cargo test --lib`). This compiles the FUSE layer without the mount syscalls, so it catches every API break; it just can't mount.
- **Run the real integration tests**: use a Linux container, which is also the only place FUSE mounts work here.

  ```bash
  docker run --rm --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor:unconfined \
    -v "$PWD":/w -w /w -e CARGO_TARGET_DIR=/tmp/target rust:1-slim-bookworm \
    bash -c "apt-get update -qq && apt-get install -y -qq libfuse3-dev pkg-config fuse3 && \
             cargo test --lib && \
             cargo test --test integration -- --ignored --test-threads=1 && \
             cargo test --test daemon -- --ignored --test-threads=1"
  ```

  `CARGO_TARGET_DIR=/tmp/target` keeps the container's Linux artifacts out of the host `target/`.

## Design principles

- **Minimal**: correct POSIX subset, not everything
- **No data loss**: safe flush sequence protects against upload failures
- **Well-tested**: 143 tests, virtual device integration, no hardware needed

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
