# Releasing to crates.io

Publishing is manual. One crate, two binaries (`mtp-mount` and `mtp-mountd`); they ship together, there's nothing to publish separately.

## Before you start

**Check every dependency version you're bumping to is already on crates.io.** `cargo publish` resolves them against the registry, not your disk, so a dep pinned to an unpublished version fails at the final step, after you've already tagged. `mtp-rs` is the one that matters here, and it's usually being released in the same sitting:

```bash
cargo search mtp-rs --limit 1   # must be >= the version in Cargo.toml
```

## Steps

1. **Bump version** in `Cargo.toml`
2. **Update `CHANGELOG.md`** with the new version and today's date
3. **Run `just check-all`** (formatting, clippy, tests, docs, audit, license). Fix everything. Re-run until fully clean.
4. **Commit and tag**:
   ```bash
   git commit -m "Prepare vX.Y.Z for release"
   git tag vX.Y.Z
   ```
5. **Dry run** to catch packaging issues. Do it in Docker, so the tarball is verified the way users actually build it:
   ```bash
   docker run --rm -v "$PWD":/w -w /w -e CARGO_TARGET_DIR=/tmp/target rust:1-slim-bookworm \
     bash -c "apt-get update -qq && apt-get install -y -qq libfuse3-dev pkg-config && cargo publish --dry-run"
   ```
   A bare `cargo publish --dry-run` **fails on a Mac without macFUSE**: it compiles the packaged tarball directly, so it never picks up the justfile's `fuser/macos-no-mount` workaround and `fuser`'s build script panics on the missing `fuse.pc`. `cargo publish --dry-run --features fuser/macos-no-mount` works too, but it verifies the no-mount build rather than the real one, so prefer Docker.

   This is the last chance to catch a bad tarball: **a published version can never be replaced**, only yanked.
6. **Publish**:
   ```bash
   cargo publish
   ```
   Same macFUSE problem applies. From a Mac, either add `--features fuser/macos-no-mount` (it only affects the local verification build, not what gets published) or publish from the Docker container above with your `CARGO_REGISTRY_TOKEN` passed in.
7. **Push** the commit and tag:
   ```bash
   git push && git push --tags
   ```

## Resuming an interrupted release

Steps 1 to 4 leave the repo claiming a version that isn't out yet: `Cargo.toml` and `CHANGELOG.md` say `X.Y.Z` while the newest tag and crates.io still say the version before it. That's a normal way to get interrupted, not damage.

To pick it back up: confirm the version isn't on crates.io (`cargo search mtp-mount`), fix the `CHANGELOG.md` date if the day has rolled over, re-run `just check-all`, then carry on from whichever of steps 4 to 7 hasn't happened. `git tag` and `git log --oneline "$(git tag --sort=-creatordate | head -1)"..HEAD` tell you where you stopped.

## Prerequisites

- A crates.io API token configured via `cargo login`
- The `exclude` list in `Cargo.toml` keeps the published package small (`docs/` is excluded, so doc-only commits don't change the tarball)

## Previous releases

See [CHANGELOG.md](../CHANGELOG.md) for the full release history. Git tags mark each release commit.
