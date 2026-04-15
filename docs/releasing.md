# Releasing to crates.io

Publishing is manual.

## Steps

1. **Bump version** in `Cargo.toml`
2. **Update `CHANGELOG.md`** with the new version and date
3. **Run `just check-all`** (formatting, clippy, tests, docs, audit, license). Fix everything. Re-run until fully clean.
4. **Commit and tag**:
   ```bash
   git commit -m "Prepare vX.Y.Z for release"
   git tag vX.Y.Z
   ```
5. **Dry run** to catch packaging issues:
   ```bash
   cargo publish --dry-run
   ```
6. **Publish**:
   ```bash
   cargo publish
   ```
7. **Push** the commit and tag:
   ```bash
   git push && git push --tags
   ```

## Prerequisites

- A crates.io API token configured via `cargo login`
- The `exclude` list in `Cargo.toml` keeps the published package small

## Previous releases

See [CHANGELOG.md](../CHANGELOG.md) for the full release history. Git tags mark each release commit.
