#!/usr/bin/env bash
# Remove build artifacts untouched for more than 7 days from the
# configured Cargo target directory. Safe any time: cargo rebuilds
# whatever this deletes. See CONTRIBUTING.md, "worktrees and build cache".
set -euo pipefail

TARGET_DIR="${CARGO_TARGET_DIR:-$(cd "$(dirname "$0")/.." && pwd)/target}"

if ! command -v cargo-sweep >/dev/null 2>&1; then
    echo "cargo-sweep is not installed; nothing was removed." >&2
    echo "  install it once:  cargo install cargo-sweep --locked" >&2
    echo "  then rerun:       $0" >&2
    exit 1
fi

if [ ! -d "$TARGET_DIR" ]; then
    echo "no target directory at $TARGET_DIR; nothing to sweep"
    exit 0
fi

echo "sweeping artifacts older than 7 days from $TARGET_DIR"
cargo sweep --time 7 "$TARGET_DIR"
