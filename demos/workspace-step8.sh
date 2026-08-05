#!/usr/bin/env bash
# Step 8: resilience — workspace on isolated tmux; record script kills control client.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SOCK="cyc-ws-step8-demo"
SESSION="resil"
# shellcheck source=demos/lib.sh
. "$(cd "$(dirname "$0")" && pwd)/lib.sh"
CYCLOPS_HOME="$(mktemp -d "$(cyc_scratch_root)/cyclops-ws8.XXXXXX")"
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
tmux send-keys -t "$SESSION" "printf 'reconnect demo — control client may drop\n'" Enter
tmux split-window -h -t "$SESSION" /bin/bash --norc
tmux send-keys -t "$SESSION:0.1" "printf 'pane two\n'" Enter

echo "=== Workspace Step 8: resilience ==="
echo "If the control client drops, panes dim with 'reconnecting…' then recover."
echo ""

export TMUX= TMUX_PANE=
exec "$CYC"
