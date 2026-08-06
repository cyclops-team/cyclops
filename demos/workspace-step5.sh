#!/usr/bin/env bash
# Workspace Step 5 demo: tab bar, nested splits, both panes live, prefix tab
# navigation and pane focus.
#
# Run inside a graphical terminal on the agent desktop for screen recording.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SOCK="cyc-ws-step5-$$"
SESSION="wsdemo"
# shellcheck source=../tests/e2e/lib/lib.sh
. "$(cd "$(dirname "$0")" && pwd)/../tests/e2e/lib/lib.sh"
CYCLOPS_HOME="$(mktemp -d "$(cyc_scratch_root)/cyclops-ws5.XXXXXX")"
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

tmux new-session -d -s "$SESSION" -x 100 -y 28 /bin/bash --norc
tmux rename-window -t "$SESSION:0" main
tmux send-keys -t "$SESSION:0" "printf 'pane-left\n'" Enter
tmux split-window -h -t "$SESSION:0" /bin/bash --norc
tmux send-keys -t "$SESSION:0.1" "printf 'pane-right\n'" Enter
tmux new-window -t "$SESSION" -n review /bin/bash --norc
tmux send-keys -t "$SESSION:1" "printf 'review tab\n'" Enter
tmux select-window -t "$SESSION:0"

echo "=== Workspace Step 5: tabs and layout ==="
echo "Session has: window 'main' (split) + window 'review'"
echo "Try: C-b n/p (tabs), C-b 1/2, C-b arrows (pane focus), C-b d (detach)"
echo ""

export TMUX= TMUX_PANE=
exec "$CYC"
