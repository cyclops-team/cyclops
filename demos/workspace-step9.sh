#!/usr/bin/env bash
# Step 9: mouse — split panes, scrollback lines, two tabs.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SOCK="cyc-ws-step9-demo"
SESSION="mouse"
# shellcheck source=../tests/e2e/lib/lib.sh
. "$(cd "$(dirname "$0")" && pwd)/../tests/e2e/lib/lib.sh"
CYCLOPS_HOME="$(mktemp -d "$(cyc_scratch_root)/cyclops-ws9.XXXXXX")"
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
tmux send-keys -t "$SESSION" "for i in \$(seq 1 40); do echo \"scroll line \$i\"; done" Enter
tmux split-window -h -t "$SESSION" /bin/bash --norc
tmux send-keys -t "$SESSION:0.1" "printf 'right pane — click me\n'" Enter
tmux new-window -t "$SESSION" -n logs /bin/bash --norc
tmux send-keys -t "$SESSION:1" "printf 'second tab\n'" Enter
tmux select-window -t "$SESSION:0"

echo "=== Workspace Step 9: mouse ==="
echo "Click panes/tabs/sidebar; wheel scrolls history; right-click for menu."
echo ""

export TMUX= TMUX_PANE=
exec "$CYC"
