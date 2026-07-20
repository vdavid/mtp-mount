[![Crate](https://img.shields.io/crates/v/mtp-mount.svg)](https://crates.io/crates/mtp-mount)
[![License](https://img.shields.io/crates/l/mtp-mount.svg)](https://github.com/vdavid/mtp-mount)
[![CI](https://img.shields.io/github/actions/workflow/status/vdavid/mtp-mount/ci.yml?label=CI)](https://github.com/vdavid/mtp-mount/actions)

# mtp-mount

Mount MTP devices as local filesystems via FUSE, with _write_ support! This is pure Rust, _not_ built
on [libmtp](https://github.com/libmtp/libmtp/).

To use it, plug in your Android phone or camera, run `mtp-mount /mnt/phone`, and use `ls`, `cp`, `cat`, `rm`, `mv`
on the device's storage like you would on any local directory. Or run the `mtp-mountd` daemon and skip the mounting
step: every device you plug in shows up under `$XDG_RUNTIME_DIR/mtp/` on its own.

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

Ride out a worn-out cable that keeps dropping the link (off by default, see below):

```sh
mtp-mount --reconnect-timeout 30 /mnt/phone  # wait half a minute for the device to come back
mtp-mount --reconnect-timeout 60 /mnt/phone  # wait a minute
```

Run `mtp-mount --help` for the full list of flags, examples, and troubleshooting tips.

## When the cable glitches

Old and worn USB cables drop the device for a second and bring it back. `mtp-mount` can ride that out, but you have to
ask for it: pass `--reconnect-timeout SECONDS`.

- The mount stays up, and the device is reopened by serial number, so a replug can't land you on a different phone.
- Filesystem calls made while the device is away **wait** for it instead of failing, and they resume once it's back.
  Files you have open keep working, bytes already read stay cached, and a file you were writing still uploads.
- If the device doesn't come back in time, `mtp-mount` prints why and unmounts, rather than leaving a mount point that
  answers every command with an I/O error.

**It's off by default because waiting blocks.** Calls don't fail during the window, they hang, and that includes
anything else touching the mount point: a file manager listing it, a backup job walking it. On a device that's really
gone, a 30-second freeze across the desktop is a worse default than a mount that goes away. Turn it on when you're
fighting a specific cable, and pick a window you'd be happy waiting out.

MTP identifies files by session-scoped handles, so all of them go stale the moment the device reconnects. `mtp-mount`
looks each one up again by path, keeping inode numbers unchanged, which is what lets already-open files carry on.

## Mount devices automatically: `mtp-mountd`

Running `mtp-mount` by hand, per device, is fine for a one-off copy. For a desktop that just works, there's a second
binary: `mtp-mountd` watches for MTP devices and mounts each one as it's plugged in.

```sh
# Try it in a terminal first
RUST_LOG=info mtp-mountd

# Then leave it running as a user service
mkdir -p ~/.config/systemd/user
cp dist/mtp-mountd.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now mtp-mountd
journalctl --user -fu mtp-mountd
```

Mounts appear under `$XDG_RUNTIME_DIR/mtp/`, one directory per device, named after the device's serial number:

```
/run/user/1000/mtp/2A31FDH200ABC/Internal shared storage/DCIM/
/run/user/1000/mtp/2A31FDH200ABC/SD card/
```

- **One mount per device**, with each storage (internal memory, SD card) as a subdirectory. One phone is one place to
  look, and one thing to clean up if anything goes wrong.
- **Devices that report no serial number** get `usb-<vendor>-<product>-<port>` instead. That name lasts as long as the
  device stays in the same port; without a serial there's nothing else that survives a replug.
- **Unplug a device and its mount goes away immediately**, forcibly, even mid-copy. A FUSE mount whose device is gone
  wedges anything that walks it, so leaving one behind is worse than never mounting.
- **Stopping the service unmounts everything.** `systemctl --user stop mtp-mountd` leaves nothing behind. If a previous
  daemon was killed outright and did leave mounts, the next one clears them at startup.
- **No reconnect window.** A device that drops off is unmounted, and hotplug mounts it again when it comes back, which
  beats freezing every process on the mount point while waiting.
- `--mount-root PATH` moves the mounts, `--spool-dir PATH` moves the spool, `-r` mounts everything read-only. Run
  `mtp-mountd --help` for the rest.

### Checking a device without mounting it: `--dry-run`

```sh
mtp-mountd --dry-run
```

Watches for devices and prints what it *would* do, mounting nothing: the fields each device reports, the mount key
derived from them, and the full path it would mount at. No directory is created and the device is never opened, so this
runs anywhere, including a machine with no FUSE.

It's there to answer one question about your device: plug it in, wait for the `PLUGGED IN` block, then unplug it, and
the `UNPLUGGED` block says whether the key matches the arrival's. It has to, because that key is how a departure finds
the mount to take down; a device whose two keys disagree would leave its mount behind every time. Do that a few times
and read the summary at the end. If it says `PROBLEM`,
[open an issue](https://github.com/vdavid/mtp-mount/issues) with the output: that's a device we need to know about.

If `$XDG_RUNTIME_DIR` isn't set (an SSH login without a logind session, a container), mounts go under
`$XDG_CACHE_HOME/mtp-mount/mounts` instead, or `~/.cache/mtp-mount/mounts`. Never `/tmp`, which other users can write to.

Running gvfs at the same time? It grabs the USB interface first and `mtp-mountd` then can't open the device. Stop it
grabbing devices with `systemctl --user mask gvfs-mtp-volume-monitor`.

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
cargo test --test daemon -- --ignored --test-threads=1
```

128 tests total (93 unit, 27 integration, 8 daemon), all passing on Linux. They use `mtp-rs`'s virtual device
transport, so CI runs without any physical hardware.

## License

MIT OR Apache-2.0
