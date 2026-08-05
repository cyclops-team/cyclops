#!/usr/bin/env bash
# Step 11: drag — nested split for divider and tab drag.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SOCK="cyc-ws-step11-demo"
SESSION="drag"
# shellcheck source=demos/lib.sh
. "$(cd "$(dirname "$0")" && pwd)/lib.sh"
CYCLOPS_HOME="$(mktemp -d "$(cyc_scratch_root)/cyclops-ws11.XXXXXX")"
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

tmux new-session -d -s "$SESSION" -x 110 -y 30 /bin/bash --norc
tmux send-keys -t "$SESSION" "printf 'left-top\n'" Enter
tmux split-window -h -t "$SESSION" /bin/bash --norc
RIGHT=$(tmux list-panes -t "$SESSION" -F '#{pane_id}' | sed -n '2p')
tmux send-keys -t "$RIGHT" "printf 'right-top\n'" Enter
tmux select-pane -t "$SESSION:0.0"
tmux split-window -v -t "$SESSION:0.0" /bin/bash --norc
BOTTOM=$(tmux list-panes -t "$SESSION" -F '#{pane_id}' | sed -n '2p')
tmux send-keys -t "$BOTTOM" "printf 'left-bottom\n'" Enter
tmux new-window -t "$SESSION" -n extra /bin/bash --norc
tmux send-keys -t "$SESSION:1" "printf 'tab two\n'" Enter
tmux select-window -t "$SESSION:0"

echo "=== Workspace Step 11: drag ==="
echo "Drag dividers to resize; drag tabs to reorder."
echo ""

export TMUX= TMUX_PANE=
exec "$CYC"
