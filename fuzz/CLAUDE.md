# fuzz: cargo-fuzz targets over the mount's own logic

Device bytes reach the mount through `mtp-rs`, which fuzzes its parsers itself. These targets cover what the mount adds
on top, the parts with no threads or I/O to get in the way, each as a small program the input spells out and a model
it's checked against:

- `fuzz_size`: `parse_size` and `format_size` round-trip.
- `fuzz_range_set`: `ranges::RangeSet` (the sparse read cache's bookkeeping) against a bitmap.
- `fuzz_file_buffer`: `buffer::FileBuffer` on a `Cursor<Vec<u8>>` against a `Vec`.
- `fuzz_inode_table`: `inode::InodeTable`, driven the way `fs.rs` drives it, with its indexes checked after every step.

Each target's header says how it reads its input.

## Files

- `fuzz_targets/*.rs`: the targets, self-contained
- `corpus/<target>/seed_*.bin`: hand-written seed programs, one or a few per target, that run every operation at least
  once. Everything else a run adds is gitignored.
- `Cargo.lock`: its own workspace. On macOS it builds `fuser` with `macos-no-mount`, so no macFUSE is needed.

## Running

- `just fuzz <target> [seconds]` locally.
- `.github/workflows/fuzz.yml` runs every `[[bin]]` for five minutes each on Mondays, reading the list from
  `Cargo.toml`, and builds every target on Rust nightly-2025-07-16.

## Rules for a target (OSS Verify)

These targets are written to qualify for [OSS Verify](https://theorem.dev/oss-verify/), which proves that a target
can't crash on any input. Its model is one Linux process whose only data is the input, so a target must:

- start no thread and no async runtime, read no clock, use no network or environment, and open no file. That's why
  `FileBuffer` is generic over its storage and `InodeTable::add_storage_at` takes the time;
- take its "random" choices from the input bytes;
- bound every loop and size that comes from the input;
- express a property as `assert!` (kept in release builds; the panic aborts), never `debug_assert!`;
- not install a panic hook or catch panics.

`SparseCache` and `SharedSparseCache` stay out: they own a temp file and a `Condvar` that a fill thread signals.
`RangeSet` is the logic inside them that can go wrong quietly.

OSS Verify builds with rustc 1.90 (nightly-2025-07-16), below our 1.91 MSRV, and lowers `rust-version` with a build
patch. Keep the fuzzed modules compiling on 1.90; the weekly `oss-verify-toolchain` job catches a slip.
