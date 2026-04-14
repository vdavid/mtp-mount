# mtp-mount

FUSE filesystem that mounts MTP devices (Android phones, cameras) as local directories. Built on `mtp-rs` for device communication and `fuser` for the FUSE layer. Translates POSIX filesystem calls into MTP operations so you can use `ls`, `cp`, `cat`, etc. on your phone's storage.

## Quick commands

| Command          | Description                                 |
|------------------|---------------------------------------------|
| `just`           | Run all checks: format, lint, test, doc     |
| `just fix`       | Auto-fix formatting and clippy warnings     |
| `just check-all` | Include security audit and license check    |
| `cargo run -- /mnt/phone` | Mount first available device       |

## Project structure

```
src/
  main.rs    # CLI entry point (clap)
  fs.rs      # MtpFs: implements fuser::Filesystem
  inode.rs   # Inode table: maps FUSE inodes <-> MTP object handles
  buffer.rs  # Write buffer: coalesces small writes into MTP uploads
  error.rs   # MountError enum
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

## Design principles

- **Minimal**: Implement a correct POSIX subset, not everything
- **Correct**: Handle edge cases (stale handles, disconnects) gracefully
- **Well-tested**: Virtual device tests via `mtp-rs`'s `virtual-device` feature

## Testing

- **Unit**: `cargo test` (mock-based)
- **Virtual device**: Tests against `mtp-rs`'s virtual device (filesystem-backed, no hardware needed)
- **Integration**: Manual testing with real devices

## Things to avoid

- Complex caching strategies (keep it simple first)
- Extended attributes, ACLs, or permission mapping
- Hardlinks, symlinks (MTP doesn't support them)
- Spawning background threads for polling (use FUSE callbacks)

## Code style

Run `just check` before committing. `cargo fmt`, `cargo clippy -D warnings`, tests for new functionality, doc comments for public APIs.
