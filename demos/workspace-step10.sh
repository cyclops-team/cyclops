#!/usr/bin/env bash
# Step 10: selection — pane filled with copyable text.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SOCK="cyc-ws-step10-demo"
SESSION="select"
# shellcheck source=../tests/e2e/lib/lib.sh
. "$(cd "$(dirname "$0")" && pwd)/../tests/e2e/lib/lib.sh"
CYCLOPS_HOME="$(mktemp -d "$(cyc_scratch_root)/cyclops-ws10.XXXXXX")"
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

cyc_tmux_teardown "$SOCK"

tmux new-session -d -s "$SESSION" -x 100 -y 28 /bin/bash --norc
tmux send-keys -t "$SESSION" "printf 'COPY_DEMO the quick brown fox jumps over the lazy dog\n'; printf 'COPY_DEMO second line for selection\n'; printf 'COPY_DEMO third line here\n'" Enter

echo "=== Workspace Step 10: selection ==="
echo "Drag across COPY_DEMO lines in the pane to select and copy."
echo ""

export TMUX= TMUX_PANE=
exec "$CYC"
