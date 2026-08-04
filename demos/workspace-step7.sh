#!/usr/bin/env bash
# Workspace Step 7 demo: sidebar with two workspaces, switch via prefix.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SOCK="cyc-ws-step7-$$"
# shellcheck source=demos/lib.sh
. "$(cd "$(dirname "$0")" && pwd)/lib.sh"
CYCLOPS_HOME="$(mktemp -d "$(cyc_scratch_root)/cyclops-ws7.XXXXXX")"
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

mkdir -p /tmp/cyclops-proj-alpha /tmp/cyclops-proj-beta
tmux new-session -d -s cyclops-proj-alpha -c /tmp/cyclops-proj-alpha /bin/bash --norc
tmux send-keys -t cyclops-proj-alpha "printf 'alpha workspace\n'" Enter
tmux new-session -d -s cyclops-proj-beta -c /tmp/cyclops-proj-beta /bin/bash --norc
tmux send-keys -t cyclops-proj-beta "printf 'beta workspace\n'" Enter

echo "=== Workspace Step 7: sidebar and workspaces ==="
echo "Sidebar lists alpha + beta. Try: C-b ] [ to switch, C-b w for new workspace"
echo ""

export TMUX= TMUX_PANE=
exec "$CYC"
