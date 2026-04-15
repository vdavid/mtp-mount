[![Crate](https://img.shields.io/crates/v/mtp-mount.svg)](https://crates.io/crates/mtp-mount)
[![License](https://img.shields.io/crates/l/mtp-mount.svg)](https://github.com/vdavid/mtp-mount)
[![CI](https://img.shields.io/github/actions/workflow/status/vdavid/mtp-mount/ci.yml?label=CI)](https://github.com/vdavid/mtp-mount/actions)

# mtp-mount

Mount MTP devices as local filesystems via FUSE.

Plug in your Android phone or camera, run `mtp-mount /mnt/phone`, and use
`ls`, `cp`, `cat`, `rm`, etc. on your device's storage. Built on
[`mtp-rs`](https://crates.io/crates/mtp-rs) (pure Rust, no libmtp) and
[`fuser`](https://crates.io/crates/fuser).

## Install

```sh
cargo install mtp-mount
```

## Usage

Mount the first available MTP device:

```sh
mtp-mount /mnt/phone
```

Pick a specific device by serial number:

```sh
mtp-mount -d SERIAL /mnt/phone
```

Mount read-only:

```sh
mtp-mount -r /mnt/phone
```

Unmount:

```sh
umount /mnt/phone           # Linux
diskutil unmount /mnt/phone  # macOS
```

Run `mtp-mount --help` for the full list of options.

## Supported operations

- **Read**: `cat`, `cp`, `head`, `less`, etc.
- **Write**: create files, overwrite existing files
- **Directories**: `ls`, `mkdir`, `rmdir`
- **Delete**: `rm`
- **Rename/move**: `mv`

## Not supported

MTP is an object-based protocol, not a block device. Some POSIX features
don't map:

- Hardlinks and symlinks
- File permissions (`chmod`/`chown` are no-ops, everything shows as 0644/0755)
- Extended attributes
- Sparse files or random-access writes (files are uploaded whole on close)

## How it works

The FUSE layer translates filesystem calls into MTP operations:

- **Reads** stream from the device to a temp file, then serve from disk (no full-file RAM buffering)
- **Writes** buffer to a temp file, then flush to the device on close
- **Overwrites** use a safe upload-then-delete-then-rename sequence when the device supports rename, so data is never lost if the upload fails
- **Directory listings** are cached per session and refreshed on `opendir`

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

All 43 tests (28 unit + 15 integration) pass on Linux. The integration tests
use `mtp-rs`'s virtual device transport, so no physical device is needed.

## License

MIT OR Apache-2.0
