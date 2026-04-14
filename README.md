# mtp-mount

Mount MTP devices as local filesystems via FUSE.

Connect your Android phone or camera over USB and access its storage like a regular directory. Uses `mtp-rs` (pure Rust, no libmtp) for device communication and `fuser` for the FUSE layer.

## Install

```sh
cargo install mtp-mount
```

## Usage

Mount the first available MTP device:

```sh
mtp-mount /mnt/phone
```

Pick a specific device by index:

```sh
mtp-mount -d 0 /mnt/phone
```

Mount read-only:

```sh
mtp-mount -r /mnt/phone
```

Unmount:

```sh
umount /mnt/phone       # Linux
diskutil unmount /mnt/phone  # macOS
```

## Supported operations

- Read files (`cat`, `cp`, etc.)
- Write files (create, overwrite)
- List directories (`ls`)
- Create directories (`mkdir`)
- Remove files and directories (`rm`, `rmdir`)
- Rename (`mv`)

## Not supported

MTP is a simple object-based protocol, so some POSIX features don't map:

- Hardlinks and symlinks
- File permissions (everything shows as 0644/0755)
- Extended attributes
- Partial writes (files are uploaded whole)

## Requirements

You need a FUSE implementation installed:

- **macOS**: [macFUSE](https://osxfuse.github.io/)
- **Linux**: `libfuse3-dev` (Debian/Ubuntu) or `fuse3` (Fedora/Arch)

## Build from source

```sh
git clone https://github.com/vdavid/mtp-mount.git
cd mtp-mount
cargo build --release
```

## License

MIT OR Apache-2.0
