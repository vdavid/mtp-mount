# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-04-15

Initial release.

### Added

- **FUSE filesystem** implementing read, write, mkdir, rmdir, rename, unlink, and directory listing
- **CLI** with `--device`, `--read-only`, and `--foreground` flags
- **Temp-file-backed I/O**: reads stream from MTP to disk, writes buffer to disk before flushing. No full-file RAM buffering in the FUSE layer.
- **Safe flush**: overwrites use upload-then-delete-then-rename when the device supports rename, preventing data loss if the upload fails
- **Inode table** mapping FUSE inodes to MTP object handles with cached metadata
- 28 unit tests (inode table + write buffer) and 15 integration tests (FUSE mount against virtual MTP device)
