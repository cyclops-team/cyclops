#!/usr/bin/env bash
set -euo pipefail
export DISPLAY="${DISPLAY:-:1}"
REPO="/workspace"
SOCK="cyc-ws-step8-demo"

xfce4-terminal --title="Cyclops Workspace Step 8" --geometry=120x35 \
  --command="$REPO/demos/launch-step8.sh" &
TERM_PID=$!

WIN=""
for _ in $(seq 1 25); do
  WIN=$(xdotool search --name "Cyclops Workspace Step 8" 2>/dev/null | head -1 || true)
  [ -n "$WIN" ] && break
  sleep 0.5
done
[ -n "$WIN" ] || { echo "window not found" >&2; exit 1; }

sleep 4
xdotool windowactivate --sync "$WIN"
sleep 1

# Kill the tmux control client (not the server) to trigger reconnect dimming.
CLIENT_PID=""
for _ in $(seq 1 20); do
  CLIENT_PID=$(pgrep -f "tmux.*-L ${SOCK}.*-C" 2>/dev/null | head -1 || true)
  [ -n "$CLIENT_PID" ] && break
  sleep 0.5
done
if [ -n "$CLIENT_PID" ]; then
  kill "$CLIENT_PID" 2>/dev/null || true
  sleep 3
fi

# Let the reconnect chain finish and panes recover.
sleep 4
xdotool key ctrl+b; sleep 0.3; xdotool key Right; sleep 1.5
xdotool key ctrl+b; sleep 0.3; xdotool key d
sleep 2
kill "$TERM_PID" 2>/dev/null || true
