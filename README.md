[![Crate](https://img.shields.io/crates/v/mtp-mount.svg)](https://crates.io/crates/mtp-mount)
[![License](https://img.shields.io/crates/l/mtp-mount.svg)](https://github.com/vdavid/mtp-mount)
[![CI](https://img.shields.io/github/actions/workflow/status/vdavid/mtp-mount/ci.yml?label=CI)](https://github.com/vdavid/mtp-mount/actions)

# mtp-mount

Mount MTP devices as local filesystems via FUSE. This is pure Rust, _not_ built
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

Run `mtp-mount --help` for the full list of flags, examples, and troubleshooting tips.

## What works

- **Read**: `cat`, `cp`, `head`, `less`, and random-access seeks (media scrubbing, `tail -c`, partial `dd`)
- **Write**: create files, overwrite existing files
- **Directories**: `ls`, `mkdir`, `rmdir`
- **Delete**: `rm`
- **Rename and move**: `mv`
- **Large files**: files larger than 4 GB read end-to-end (no 32-bit truncation)

## What doesn't (and why)

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
- **Writes buffer to a tempfile**, then flush to the device on close.
- **Overwrites use a safe upload-then-delete-then-rename sequence** when the device supports rename. So if the upload
  fails, the original is still there. Falls back to delete-then-upload with a warning log on devices that don't support
  rename.
- **Directory listings are cached** and refreshed on `opendir`. A background event monitor watches
  `MtpDevice::next_event()` and invalidates entries when files are added, removed, or modified on the device itself (so
  taking a photo while the phone is mounted just shows up).

## Requirements

You need a FUSE implementation:

- **Linux**: `sudo apt install libfuse3-dev` (Debian/Ubuntu) or `fuse3` (Fedora/Arch)
- **macOS**: [macFUSE](https://osxfuse.github.io/) or [FUSE-T](https://www.fuse-t.org/) (may need manual `pkg-config` wiring)

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

65 tests total (44 unit + 21 integration), all passing on Linux. The integration tests use `mtp-rs`'s virtual device
transport, so CI runs without any physical hardware.

## License

MIT OR Apache-2.0
