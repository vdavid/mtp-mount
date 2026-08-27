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
  shutdown.rs      # One-way "take this mount down" signal + the SIGTERM/SIGINT handler
  fill.rs          # FillTracker: the whole-object downloads in flight, and cancelling them
  size.rs          # Parses/formats the byte sizes --full-download-limit takes
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
- **Reads** go through one `SharedSparseCache` per open *object* (keyed by inode, not by file handle), with `SparseCache` (tempfile + sorted `Vec<Range<u64>>` of populated ranges) as the authority for which bytes are real. Partial-capable responders fetch missing ranges via `Storage::read_range` in 1 MiB chunks. A responder with neither partial-download operation takes the whole-object fallback instead. See "Reading without partial reads" below.
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

## Reading without partial reads

Some responders handle neither `GetPartialObject` nor `GetPartialObject64`: libhaze (Sphaira on the
Nintendo Switch) is the one that turned this up (#9), and simple PTP responders are the same. There is
no way to ask them for a byte range, so `Storage::read_range` fails `Unsupported` and every read of
every file used to be an `EIO`.

**The fallback.** `read_strategy(supports_partial_download, object_size, limit)` picks the path per
object. Without a partial-read op, the first `read()` starts ONE `Storage::download(ByteRange::Full)`
whose chunks are written into the same `SharedSparseCache` the ranged path uses, and each blocked read
returns the moment its own bytes land. `SparseCache` stays the authority, so a stream that dies at 60%
just leaves the rest unpopulated: a read is only ever served from bytes the device actually sent, never
from the tempfile's zero-filled hole. There is no promotion step to get wrong.

**Nothing a reader does may interrupt the fill.** A seek that jumps ahead waits for the running stream
rather than restarting it, and `close()` doesn't stop it either. Restarting would throw away the walk
and, worse, mean dropping a live `FileDownload`: `mtp-rs` marks that type `#[must_use]` because an
abandoned transfer leaves the responder mid-USB-transaction, which on Android is the failure that needs
a physical replug. The one thing that MAY stop a fill is the mount going away, and it cancels rather
than drops (see "Taking a mount down mid-transfer").

**The cache is keyed by inode so one object is downloaded once.** A thumbnailer or `file` does
open → read the header → close → reopen, and two processes reading one file overlap; with a per-handle
cache each of those is another multi-minute download of the same object. `drop_read_cache_if_unused`
therefore keeps the entry while any fh still maps to the inode **or** a fill is still running (the
filler drops it itself when it finishes). Once nothing holds it and no fill is running the entry goes,
so reopening a file later re-reads it from the device instead of serving bytes of unknown age. That's
deliberately not a cache with a policy: see "Things to avoid".

**Link loss, but not stale handles.** A fill that fails with a link-lost error may be retried once on a
new session (`recover_full_fill_link` + `reset_after_link_loss`, one retry). Nothing else resets a
fill, which is what stops a seek or a `close()` from restarting a healthy one.

**The size bound and why it's a bound, not a ban.** A fill holds the device's single MTP session for
the whole transfer, and with `fuser`'s one event-loop thread every other process on the mount queues
behind it, so a background thumbnailer that opens a 30 GB file freezes the mount for a quarter of an
hour. No number avoids that (1 GiB over a 20 MiB/s link is already ~50 seconds), so
`DEFAULT_FULL_DOWNLOAD_LIMIT` (4 GiB) caps the damage rather than preventing it, and
`--full-download-limit` (`0` lifts it) is how someone who *means* to copy a 32 GB Switch dump gets it.
Without the escape hatch the tool can't do its main job on the device that motivated the feature. Same
shape as `ReconnectPolicy::DEFAULT_TIMEOUT_SECS`: conservative default, opt in when you know what
you're waiting for.

**`EFBIG` lands on `open`, not on `read`.** Refusing at the first read would mean `cp` had already
created its destination file, and a sparse tempfile had already been allocated, for bytes that are
never coming. `open` only applies the check when the open asks for read access: an overwrite replaces
the object without reading a byte of it, so a write-only open of a huge file must still succeed.

**A device that over-returns doesn't break the ranged path.** `SparseCache::write_at` rejects a write
past the advertised size (a real invariant: it's what stops a short read from being served as zeros),
and `mtp-rs` passes a device's response through as it arrived. The ranged path therefore clamps to the
object size and warns, the same way `fill_from_stream` does, instead of turning a device quirk into an
`EIO` on the last chunk of every file.

## Taking a mount down mid-transfer

A whole-object fill can be minutes long, and the process may be asked to stop in the middle of one.
Dropping the `FileDownload` then is the mid-transaction abort described above, and dropping the tokio
runtime the fill was spawned on does exactly that, silently. So teardown is explicit:

- `MtpFs::fills()` hands out the `FillTracker` (`fill.rs`), which holds every live fill. Take it
  *before* the filesystem goes to `fuser`, which consumes it.
- `FillTracker::stop_and_wait(DEFAULT_STOP_TIMEOUT)` sets a flag every filler checks between chunks;
  the filler calls `FileDownload::cancel` (a bounded drain, a round-trip rather than a transfer) and
  exits. It also refuses to register new fills, so nothing starts during teardown.
- `main` calls it after `umount_and_join` and before the runtime drops; the daemon's `Supervisor`
  calls it in `unmount`, after `force_unmount`. Both are bounded: a device that's physically gone
  won't answer the cancel, and blocking an unmount on it would be worse than the untidy exit.

This is also why `mtp-mount` catches SIGINT/SIGTERM at all (`shutdown::spawn_signal_handler`, shared
with the daemon). It used to let Ctrl+C kill the process where it stood, which is fine when every read
is a 1 MB transaction and not fine when one holds the session for 30 GB. The `--help` text has always
said "Press Ctrl+C to unmount"; now that's what happens.

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

- **Library unit tests** (124): inode table, write buffer, sparse cache and sequential-fill coordination, fill tracking and cancellation, size parsing, upload streaming, spool-dir resolution, device-open hints, reconnect policy, shutdown signal, mount-root resolution, device directory naming, mountpoint detection, stale-mount sweep, dry-run key derivation. Run with `cargo test --lib`.
- **Binary tests** (3 for `mtp-mount`, 2 for `mtp-mountd`): the clap definitions are well-formed and `--help`'s `4G` default parses back to `DEFAULT_FULL_DOWNLOAD_LIMIT`, so the help text can't drift from what the mount does. `cargo test --bins`.
- **Integration tests** (35): mount a virtual MTP device via FUSE, exercise with `std::fs` operations including device event monitoring, partial reads, reconnects, re-keyed handles, and the whole-object fallback. Linux only (needs `libfuse3-dev`), except the one non-ignored test below. Run with `cargo test --test integration -- --ignored --test-threads=1`
- **Daemon tests** (8, `tests/daemon.rs`): drive `Supervisor` through its command channel and assert against the real filesystem. Linux only, `cargo test --test daemon -- --ignored --test-threads=1`. See below.
- **Dry-run tests** (7, `tests/dry_run.rs`): inject arrivals and departures into the `--dry-run` reporter and assert on the verdict, the wording a person reads, and that a run leaves a temp mount root untouched. No FUSE, no device, so they run everywhere with plain `cargo test`.

### Testing a disconnect

`mtp-rs` can't simulate one. `unregister_virtual_device` only removes a device from the *discovery* registry: an already-open `MtpDevice` keeps its transport and backing dir and answers as if nothing happened (pinned down by `test_unregistering_a_virtual_device_does_not_disconnect_an_open_one`, the one test in that file that isn't `#[ignore]`). So the seam is `device::UnplugSwitch`, an `Arc<AtomicBool>` the mount and the `DeviceOpener` share: while it's set, every MTP op fails `Disconnected` and reopening fails too. Production never flips it.

The other trap is that the virtual device numbers handles from 1 in listing order, so a reopened device hands out the *same* handles and a mount that never re-resolved anything would still read the right bytes. The reconnect fixture defeats that with a decoy "Handle burner" storage: on every reopen the test's opener lists it first, burning 64 handles, so the real files come back with handles that can't collide with the dead session's. `test_open_fd_survives_reconnect` is the test that actually pins the re-resolution down (verified: it fails, reading zeros, if `bump_generation` is removed).

### Testing the whole-object fallback

`MountSpec::no_partial_read` builds the virtual device with `supports_partial_object` and
`supports_partial_object_64` both `false` (mtp-rs 0.31), which is what makes any of this testable
without a Switch: the capability probe reports no partial download, ranged reads fail `Unsupported`,
and `mtp-rs` *refuses* the operations rather than serving them anyway. `TestMount::no_partial_read`
also takes the `--full-download-limit`, so a small file can stand in for one that's over the bound.

`full_fill_count()` is the assertion that matters in most of these: it counts whole-object downloads
started, so "one download, not one per read" and "a second descriptor doesn't re-download" are checks
on a number rather than on timing. Don't reach for wall-clock assertions here. The virtual device
streams from local disk at memory speed, so anything phrased as "while the download is still running"
is a race that passes on hardware and fails in CI (one earlier version of the shared-cache test did
exactly that; it's now two descriptors open at once, which is deterministic).

One more trap: FUSE `release` is fire-and-forget, and the upload happens there. A test that writes
through the mount and then reads the *backing directory* has to make one more call through the mount
first (`fs::metadata`, say) to order itself after the release, because `fuser` dispatches on a single
thread.

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
- **Well-tested**: 178 tests, virtual device integration, no hardware needed

## Things to avoid

- Complex caching strategies. Sharing one cache between the descriptors on an open object is not one:
  it exists so a device is never asked for the same object twice at once, and the entry still goes as
  soon as nothing holds the file open. A cache that outlives the last `close()` would need an eviction
  policy and a staleness story, and that's the line.
- Extended attributes, ACLs, or permission mapping
- Hardlinks, symlinks (MTP doesn't support them)

## Device-open failures

`hints.rs` maps a failed open to a remedy: `mtp_rs::Error::is_exclusive_access()` (gvfs on Linux, `ptpcamerad` on macOS holds the interface) and `is_permission_denied()` (missing udev rule) each get a hint; anything else prints the bare error. `main.rs` prints the hint under the error, and `long_help()` embeds the same `BUSY_HINT`/`PERMISSION_HINT` consts (via `hints::indent`) into the `--help` troubleshooting section, so the two wordings can't drift. Add new remedies as consts there, not inline in either place.

## CLI and --help

The `--help` output includes examples, troubleshooting tips, and notes about MTP
limitations. It's an important part of the user experience. When adding or changing
CLI flags, update the `after_long_help` text in `main.rs` to match (and in
`bin/mtp-mountd.rs` for the daemon's own). The short `-h` output is auto-generated
by clap; the long `--help` has hand-written sections.

`--full-download-limit` exists on both binaries and takes a size (`8G`, `512M`, a
plain byte count, or `0` for no limit) through `size::parse_size`. Its clap default
is the string `"4G"` so the help reads well; a unit test in each binary asserts that
string parses back to `DEFAULT_FULL_DOWNLOAD_LIMIT`, which is what stops the help
text from drifting away from the constant the code uses.

## Code style

Run `just check` before committing. `cargo fmt`, `cargo clippy -D warnings`, tests for new functionality.
