#!/usr/bin/env bash
# Step 14 demo: command surface — bare cyclops, watch rename, ui deprecation.
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
CYC="$REPO/target/debug/cyclops"

echo "=== Step 14: command surface ==="
echo ""
echo '$ cyclops --help | head -5'
"$CYC" --help | head -5
echo ""
echo '$ cyclops </dev/null   # non-TTY → help, exit 0'
"$CYC" </dev/null
echo "exit: $?"
echo ""
echo '$ cyclops ui --json 2>&1 | head -2'
"$CYC" ui --json 2>&1 | head -2 || true
echo ""
echo '$ cyclops ui 2>&1 | head -1   # deprecation on stderr'
"$CYC" ui --plain 2>&1 | head -3 || true
echo ""
echo "cyclops watch opens the stream TUI (same as ui without deprecation)."
echo "cyclops watch --json is the machine-readable event stream."
echo ""
read -r -p "Press Enter to close…" _
