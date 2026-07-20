# mtp-mount Development Commands
# ===============================
#
# Available commands (run `just --list` for details):
#
#   Individual checks:
#     fmt         - Format code with cargo fmt
#     fmt-check   - Check formatting (CI mode)
#     clippy      - Run clippy with -D warnings
#     test        - Run tests
#     doc         - Build documentation
#     audit       - Security audit (requires cargo-audit)
#     deny        - License/dependency check (requires cargo-deny)
#
#   Composite commands:
#     check       - Run fast checks: fmt-check, clippy, test, doc (default)
#     check-all   - Run all checks including audit and deny
#     fix         - Auto-fix formatting and clippy warnings
#
#   Utility commands:
#     clean       - Remove build artifacts
#     install-tools - Install required development tools

set shell := ["bash", "-uc"]

# Extra cargo flags so the checks run on a Mac without macFUSE.
#
# `fuser`'s build script calls `.unwrap()` on its pkg-config probe for macFUSE,
# so when macFUSE isn't installed EVERY cargo command that builds `fuser` panics
# out, including `cargo clippy` and `cargo doc`, not only the tests. The
# `macos-no-mount` feature builds the FUSE layer without the mount syscalls:
# everything type-checks and lints, it only loses the ability to mount, which a
# Mac without macFUSE can't do anyway.
#
# Empty on Linux, and on a Mac that does have macFUSE, so CI keeps building the
# real mount path. That means local checks here are marginally weaker than CI;
# the Linux CI job is what covers the mount implementation.
fuse_features := if os() == "macos" {
    if path_exists("/Library/Filesystems/macfuse.fs") == "true" { "" } else { "--features fuser/macos-no-mount" }
} else { "" }

# Default recipe - run fast checks
default: check

# ==============================================================================
# Individual checks
# ==============================================================================

# Format code with cargo fmt
fmt:
    @echo "[*] Formatting..."
    @cargo fmt
    @echo "[+] Formatted"

# Check formatting without modifying files (for CI)
fmt-check:
    @echo "[*] Checking formatting..."
    @cargo fmt --check
    @echo "[+] Formatting OK"

# Run clippy with strict warnings
clippy:
    @echo "[*] Running clippy..."
    @cargo clippy --all-targets {{ fuse_features }} --quiet -- -D warnings
    @echo "[+] Clippy passed"

# Run tests
test:
    @echo "[*] Running tests..."
    @cargo test {{ fuse_features }} --quiet
    @echo "[+] Tests passed"

# Build documentation
doc:
    @echo "[*] Building docs..."
    @cargo doc --no-deps {{ fuse_features }} --quiet
    @echo "[+] Docs built"

# Run security audit (requires cargo-audit)
audit:
    @echo "[*] Running security audit..."
    @if ! command -v cargo-audit &> /dev/null; then \
        echo "[!] cargo-audit not found. Install with: just install-tools"; \
        exit 1; \
    fi
    @cargo audit --deny warnings
    @echo "[+] Security audit passed"

# Run cargo-deny checks (requires cargo-deny)
deny:
    @echo "[*] Running cargo-deny..."
    @if ! command -v cargo-deny &> /dev/null; then \
        echo "[!] cargo-deny not found. Install with: just install-tools"; \
        exit 1; \
    fi
    @cargo deny --log-level error check
    @echo "[+] Cargo deny passed"

# ==============================================================================
# Composite commands
# ==============================================================================

# Run fast checks: fmt-check, clippy, test, doc
check: fmt-check clippy test doc
    @echo ""
    @echo "[+] All fast checks passed!"

# Run all checks including slow ones: check + audit + deny
check-all: check audit deny
    @echo ""
    @echo "[+] All checks passed!"

# Auto-fix formatting and clippy warnings
fix: fmt
    @echo "[*] Running clippy --fix..."
    @cargo clippy --all-targets {{ fuse_features }} --fix --allow-dirty --allow-staged --quiet -- -D warnings
    @echo "[+] Fixed"

# ==============================================================================
# Utility commands
# ==============================================================================

# Remove build artifacts
clean:
    @echo "[*] Cleaning build artifacts..."
    cargo clean
    @echo "[+] Clean complete"

# Install required development tools
install-tools:
    @echo "[*] Installing development tools..."
    @echo ""
    @echo "Installing cargo-audit..."
    cargo install cargo-audit
    @echo ""
    @echo "Installing cargo-deny..."
    cargo install cargo-deny
    @echo ""
    @echo "[+] All tools installed"
