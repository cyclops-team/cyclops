#!/usr/bin/env bash
set -euo pipefail
export DISPLAY="${DISPLAY:-:1}"
REPO="/workspace"

xfce4-terminal --title="Cyclops Workspace Step 5" --geometry=120x35 \
  --command="$REPO/demos/launch-step5.sh" &
TERM_PID=$!

WIN=""
for _ in $(seq 1 20); do
  WIN=$(xdotool search --name "Cyclops Workspace Step 5" 2>/dev/null | head -1 || true)
  [ -n "$WIN" ] && break
  sleep 0.5
done
[ -n "$WIN" ] || { echo "window not found" >&2; exit 1; }

sleep 3
xdotool windowactivate --sync "$WIN"
sleep 1
xdotool key ctrl+b; sleep 0.3; xdotool key n; sleep 1.5
xdotool key ctrl+b; sleep 0.3; xdotool key p; sleep 1.5
xdotool key ctrl+b; sleep 0.3; xdotool key 2; sleep 1.5
xdotool key ctrl+b; sleep 0.3; xdotool key 1; sleep 1.5
xdotool key ctrl+b; sleep 0.3; xdotool key Right; sleep 1
xdotool type --delay 60 'echo step5'
xdotool key Return
sleep 2
xdotool key ctrl+b; sleep 0.3; xdotool key d
sleep 2
kill "$TERM_PID" 2>/dev/null || true
