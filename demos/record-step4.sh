#!/usr/bin/env bash
set -euo pipefail
export DISPLAY="${DISPLAY:-:1}"
REPO="/workspace"

xfce4-terminal --title="Cyclops Workspace Step 4" --geometry=120x35 \
  --command="$REPO/demos/launch-step4.sh" &
TERM_PID=$!

WIN=""
for _ in $(seq 1 20); do
  WIN=$(xdotool search --name "Cyclops Workspace Step 4" 2>/dev/null | head -1 || true)
  [ -n "$WIN" ] && break
  sleep 0.5
done
[ -n "$WIN" ] || { echo "window not found" >&2; exit 1; }

sleep 2
xdotool windowactivate --sync "$WIN"
sleep 1
xdotool type --delay 80 'echo step4 works'
xdotool key Return
sleep 2
xdotool key ctrl+b
sleep 0.3
xdotool key d
sleep 2
kill "$TERM_PID" 2>/dev/null || true
