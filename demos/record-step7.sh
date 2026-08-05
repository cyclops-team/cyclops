#!/usr/bin/env bash
set -euo pipefail
export DISPLAY="${DISPLAY:-:1}"
REPO="/workspace"

xfce4-terminal --title="Cyclops Workspace Step 7" --geometry=120x35 \
  --command="$REPO/demos/launch-step7.sh" &
TERM_PID=$!

WIN=""
for _ in $(seq 1 25); do
  WIN=$(xdotool search --name "Cyclops Workspace Step 7" 2>/dev/null | head -1 || true)
  [ -n "$WIN" ] && break
  sleep 0.5
done
[ -n "$WIN" ] || { echo "window not found" >&2; exit 1; }

sleep 3
xdotool windowactivate --sync "$WIN"
sleep 1
# next workspace
xdotool key ctrl+b; sleep 0.3; xdotool key bracketright; sleep 2
xdotool type --delay 60 'pwd'; xdotool key Return; sleep 1.5
# prev workspace
xdotool key ctrl+b; sleep 0.3; xdotool key bracketleft; sleep 2
xdotool key ctrl+b; sleep 0.3; xdotool key d
sleep 2
kill "$TERM_PID" 2>/dev/null || true
