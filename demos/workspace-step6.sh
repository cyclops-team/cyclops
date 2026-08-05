#!/usr/bin/env bash
# Workspace Step 6 demo: structural intents — split, zoom, rename tab, new tab.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SOCK="cyc-ws-step6-$$"
SESSION="struct"
# shellcheck source=demos/lib.sh
. "$(cd "$(dirname "$0")" && pwd)/lib.sh"
CYCLOPS_HOME="$(mktemp -d "$(cyc_scratch_root)/cyclops-ws6.XXXXXX")"
export CYCLOPS_HOME
printf 'tmux_socket = "%s"\ntmux_config = "/dev/null"\n' "$SOCK" >"$CYCLOPS_HOME/config.toml"
CYC="$REPO/target/debug/cyclops"

cleanup() {
  cyc_tmux_teardown "$SOCK"
  rm -rf "$CYCLOPS_HOME"
}
trap cleanup EXIT

cd "$REPO"
tmux() { command tmux -u -L "$SOCK" -f /dev/null "$@"; }

tmux new-session -d -s "$SESSION" -x 100 -y 28 -c /tmp /bin/bash --norc
tmux send-keys -t "$SESSION" "printf 'structural demo\n'" Enter

echo "=== Workspace Step 6: structural intents ==="
echo "Try: C-b % \" x z , c & (split, close, zoom, rename, new/close tab)"
echo ""

export TMUX= TMUX_PANE=
exec "$CYC"
