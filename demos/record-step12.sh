#!/usr/bin/env bash
set -euo pipefail
export DISPLAY="${DISPLAY:-:1}"
REPO="/workspace"
# shellcheck source=demos/lib.sh
. "$REPO/demos/lib.sh"
CYCLOPS_HOME="$(cyc_scratch_root)/cyclops-ws12-demo"
CYC="$REPO/target/debug/cyclops"

xfce4-terminal --title="Cyclops Workspace Step 12" --geometry=120x35 \
  --command="$REPO/demos/launch-step12.sh" &
TERM_PID=$!

WIN=""
for _ in $(seq 1 40); do
  WIN=$(xdotool search --name "Cyclops Workspace Step 12" 2>/dev/null | head -1 || true)
  [ -n "$WIN" ] && break
  sleep 0.5
done
[ -n "$WIN" ] || { echo "window not found" >&2; exit 1; }

# Wait for cyclopsd socket and workspace to boot.
for _ in $(seq 1 40); do
  [ -S "$CYCLOPS_HOME/sock" ] && break
  sleep 0.5
done

sleep 3
xdotool windowactivate --sync "$WIN"
sleep 1

# Send a message so the ledger has events for the panel.
export CYCLOPS_HOME
"$CYC" send implementer --subject "Review the patch" --body "Check workspace-step12.sh" 2>/dev/null || true
sleep 2

# Toggle event panel.
xdotool key ctrl+b; sleep 0.3; xdotool key e
sleep 2.5

# Focus other pane to show badges on both.
xdotool key ctrl+b; sleep 0.3; xdotool key Right
sleep 1.5

# Click implementer in sidebar (named agent row).
eval "$(xdotool getwindowgeometry --shell "$WIN")"
SX=$((WIDTH * 10 / 100))
SY=$((HEIGHT * 28 / 100))
xdotool mousemove --window "$WIN" --sync "$SX" "$SY"
xdotool click 1
sleep 2

xdotool key ctrl+b; sleep 0.3; xdotool key e
sleep 1
xdotool key ctrl+b; sleep 0.3; xdotool key d
sleep 2
kill "$TERM_PID" 2>/dev/null || true
