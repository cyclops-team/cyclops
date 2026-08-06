#!/usr/bin/env bash
# Workspace Step 4 demo: bare cyclops on a TTY renders the active pane live,
# typing reaches the shell, prefix-C-b d detaches without killing tmux.
#
# Run inside a graphical terminal on the agent desktop for screen recording.
# Never touches the default tmux server.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SOCK="cyc-ws-step4-$$"
SESSION="demo"
# shellcheck source=../tests/e2e/lib/lib.sh
. "$(cd "$(dirname "$0")" && pwd)/../tests/e2e/lib/lib.sh"
CYCLOPS_HOME="$(mktemp -d "$(cyc_scratch_root)/cyclops-ws4.XXXXXX")"
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

tmux new-session -d -s "$SESSION" -x 100 -y 28 \
  "printf 'Cyclops workspace Step 4 demo\nType here after cyclops attaches.\n'; exec /bin/bash --norc"
tmux send-keys -t "$SESSION" "printf 'hello from tmux pane\n'" Enter

echo "=== Workspace Step 4: bare cyclops ==="
echo "Launching cyclops (attach to session $SESSION on socket $SOCK)..."
echo "Demo: type 'echo step4 works', then press C-b d to detach."
echo ""

export TMUX= TMUX_PANE=
exec "$CYC"
