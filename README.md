[![Crate](https://img.shields.io/crates/v/mtp-mount.svg)](https://crates.io/crates/mtp-mount)
[![License](https://img.shields.io/crates/l/mtp-mount.svg)](https://github.com/vdavid/mtp-mount)
[![CI](https://img.shields.io/github/actions/workflow/status/vdavid/mtp-mount/ci.yml?label=CI)](https://github.com/vdavid/mtp-mount/actions)

# mtp-mount

Mount MTP devices as local filesystems via FUSE, with _write_ support! This is pure Rust, _not_ built
on [libmtp](https://github.com/libmtp/libmtp/).

To use it, plug in your Android phone or camera, run `mtp-mount /mnt/phone`, and use `ls`, `cp`, `cat`, `rm`, `mv`
on the device's storage like you would on any local directory.

Built on [`mtp-rs`](https://crates.io/crates/mtp-rs) (pure-Rust MTP stack) and [
`fuser`](https://crates.io/crates/fuser).

## Install

```sh
cargo install mtp-mount
```

## Usage

List connected devices:

```sh
mtp-mount --list
```

Mount the first available device:

```sh
mtp-mount /mnt/phone
```

Or pick a specific device by serial number:

```sh
mtp-mount -d SERIAL /mnt/phone
```

Mount read-only (safer for browsing, you'll make no accidental deletes):

```sh
mtp-mount -r /mnt/phone
```

Unmount:

```sh
umount /mnt/phone            # Linux
diskutil unmount /mnt/phone  # macOS
```

Give a worn-out cable more (or less) room to recover:

```sh
mtp-mount --reconnect-timeout 60 /mnt/phone  # wait a minute for the device to come back
mtp-mount --reconnect-timeout 0 /mnt/phone   # don't wait at all, unmount on the first drop
```

Run `mtp-mount --help` for the full list of flags, examples, and troubleshooting tips.

## When the cable glitches

Old and worn USB cables drop the device for a second and bring it back. `mtp-mount` rides that out:

- The mount stays up, and the device is reopened by serial number, so a replug can't land you on a different phone.
- Filesystem calls made while the device is away **wait** for it instead of failing, and they resume once it's back.
  Files you have open keep working, bytes already read stay cached, and a file you were writing still uploads.
- The wait is capped by `--reconnect-timeout` (30 seconds by default), so nothing hangs forever. Set it to `0` to fail
  fast instead.
- If the device doesn't come back in time, `mtp-mount` prints why and unmounts, rather than leaving a mount point that
  answers every command with an I/O error.

MTP identifies files by session-scoped handles, so all of them go stale the moment the device reconnects. `mtp-mount`
looks each one up again by path, keeping inode numbers unchanged, which is what lets already-open files carry on.

## What works

- **Read**: `cat`, `cp`, `head`, `less`, and random-access seeks (media scrubbing, `tail -c`, partial `dd`)
- **Write**: create files, overwrite existing files
- **Directories**: `ls`, `mkdir`, `rmdir`
- **Delete**: `rm`
- **Rename and move**: `mv`
- **Large files**: files larger than 4 GB read end-to-end (no 32-bit truncation)
- **Flaky cables**: the mount survives a device that drops off the bus and comes back

## What doesn't work (and why)

MTP is an object-based protocol, not a block device, so some POSIX features just don't map:

- Hardlinks and symlinks (MTP has no concept of them)
- File permissions: `chmod` and `chown` are no-ops, everything shows as `0644`/`0755`
- Extended attributes
- Sparse files and random-access writes: files are uploaded whole on close

## How it works

The FUSE layer translates filesystem calls into MTP operations:

- **Reads are byte-range on-demand.** Each FUSE `read(offset, size)` fetches only the missing bytes via MTP's
  `GetPartialObject64`, writes them into a sparse tempfile, and serves the requested slice. Repeated reads of the same
  region hit the local cache. Scrubbing a 10 GB video only downloads what you actually touch.
- **Writes spool to disk**, then upload to the device on close. The spool lives in your cache directory, not `/tmp`:
  on most current Linux distros `/tmp` is a tmpfs (RAM), and a 4 GB upload buffered there would fill memory. The upload
  streams from the spool file in 64 KiB chunks, so memory stays flat no matter how big the file is.
- **Overwrites use a safe upload-then-delete-then-rename sequence** when the device supports rename. So if the upload
  fails, the original is still there. Falls back to delete-then-upload with a warning log on devices that don't support
  rename.
- **Directory listings are cached** and refreshed on `opendir`. A background event monitor watches
  `MtpDevice::next_event()` and invalidates entries when files are added, removed, or modified on the device itself (so
  taking a photo while the phone is mounted just shows up).

## Why gvfs can't write (and this can)

On most Linux desktops, MTP goes through gvfs, and gvfs can't write. This happens:

```
$ cp photo.jpg "/run/user/1000/gvfs/mtp:host=Pixel_9/Internal shared storage/DCIM/"
cp: cannot create regular file '...': Operation not supported
```

That's not really a gvfs bug, it's a mismatch between two designs. POSIX opens a file and starts writing without knowing
how big it will be. MTP wants the size up front, in `SendObjectInfo`, before you send a single byte. gvfs gives up and
returns `EOPNOTSUPP`. People have been hitting this for ~20 years.

`mtp-mount` spools the file locally while you write, then uploads it on `close()`, when the size is finally known. So
`cp`, `rsync`, and "save as" from any app work the way you'd expect.

### Where the spool lives

Writes (and the read cache) go to unlinked temp files under:

- **Linux**: `$XDG_CACHE_HOME/mtp-mount/spool`, or `~/.cache/mtp-mount/spool` when `XDG_CACHE_HOME` is unset
- **macOS**: `~/Library/Caches/mtp-mount/spool`

Point it somewhere else with `--spool-dir /path/to/dir`, for example when your home directory is small and you're
copying a 60 GB video off a phone. The directory is created if missing, and `mtp-mount` won't start if it can't write
there. The files are unlinked, so their space comes back on its own if the process stops, and nothing is left behind
to clean up.

## Requirements

Disk space for the spool: copying a file to or from the device buffers it under your cache directory first, so keep
roughly as much free space there as the largest file you'll transfer (or pass `--spool-dir`).

You need a FUSE implementation:

- **Linux**: `sudo apt install libfuse3-dev` (Debian/Ubuntu) or `fuse3` (Fedora/Arch)
- **macOS**: [macFUSE](https://osxfuse.github.io/) or [FUSE-T](https://www.fuse-t.org/) (may need manual `pkg-config`
  wiring)

## Build from source

```sh
git clone https://github.com/vdavid/mtp-mount.git
cd mtp-mount
cargo build --release
```

## Testing

Unit tests run without FUSE:

```sh
cargo test
```

Integration tests mount a virtual MTP device via FUSE (Linux only, needs `libfuse3-dev`):

```sh
cargo test --test integration -- --ignored --test-threads=1
```

99 tests total (72 unit + 27 integration), all passing on Linux. The integration tests use `mtp-rs`'s virtual device
transport, so CI runs without any physical hardware.

## License

MIT OR Apache-2.0
