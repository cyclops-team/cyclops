#!/usr/bin/env bash
# Installs the commPact -> cyclops shim over the v1 CLI. ADMIN ONLY.
#
# Prepared by Cyclops M2; nothing runs this automatically. It refuses to
# act unless CYCLOPS_CUTOVER_ACK=yes is set, so no agent or script can
# install it by accident. Runbook: clops/docs/CUTOVER.md.
#
# What it does, in order:
#   1. moves ~/.commPact/bin/commPact to commPact.v1.bak (the backup IS
#      the original binary, byte-identical)
#   2. symlinks this directory's shim in its place
#   3. prints the rollback
# Everything else under ~/.commPact stays untouched.
set -euo pipefail

die() { echo "error: $*" >&2; exit 1; }

SHIM_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SHIM="$SHIM_DIR/commPact"
COMMPACT_HOME="${COMMPACT_HOME:-$HOME/.commPact}"
TARGET="$COMMPACT_HOME/bin/commPact"
BACKUP="$COMMPACT_HOME/bin/commPact.v1.bak"

print_rollback() {
  cat <<EOF
rollback (restores v1 exactly):
  rm "$TARGET"
  mv "$BACKUP" "$TARGET"
EOF
}

# The guard: refuse unless the admin acknowledged the cutover explicitly.
if [[ "${CYCLOPS_CUTOVER_ACK:-}" != "yes" ]]; then
  cat >&2 <<EOF
This replaces the live commPact v1 CLI at
  $TARGET
with the cyclops shim. It is the admin cutover step from
clops/docs/CUTOVER.md and must not run by accident. Nothing was changed.

To proceed deliberately:
  CYCLOPS_CUTOVER_ACK=yes $0
EOF
  exit 1
fi

# Preflight: the shim must exist, be executable, and parse.
[[ -x "$SHIM" ]] || die "shim not found or not executable: $SHIM"
bash -n "$SHIM" || die "shim does not parse: $SHIM"

# Idempotence and conflict handling before anything moves.
if [[ -L "$TARGET" ]]; then
  current="$(readlink "$TARGET")"
  if [[ "$current" == "$SHIM" ]]; then
    echo "already installed: $TARGET -> $SHIM"
    print_rollback
    exit 0
  fi
  die "$TARGET is already a symlink to $current; resolve that by hand first"
fi
[[ -f "$TARGET" ]] || die "no commPact v1 binary at $TARGET; nothing to shim"
if [[ -e "$BACKUP" ]]; then
  die "backup already exists: $BACKUP. Refusing to overwrite the v1 backup; restore or remove it first."
fi

# Warnings only: install can precede fixing PATH, but say so plainly.
CYX="${CYCLOPS_BIN:-cyclops}"
if command -v "$CYX" >/dev/null 2>&1; then
  if ! "$CYX" ping >/dev/null 2>&1; then
    echo "warning: cyclopsd is not answering; shim send/read will fail until it runs" >&2
  fi
else
  echo "warning: cyclops not on PATH; the shim will fail until it is (or set CYCLOPS_BIN)" >&2
fi

mv "$TARGET" "$BACKUP"
ln -s "$SHIM" "$TARGET"

echo "installed: $TARGET -> $SHIM"
echo "v1 preserved: $BACKUP"
print_rollback
