#!/usr/bin/env bash
set -euo pipefail
export DISPLAY="${DISPLAY:-:1}"
REPO="/workspace"

xfce4-terminal --title="Cyclops Step 14 CLI" --geometry=100x28 \
  --command="bash -lc '$REPO/demos/workspace-step14.sh'" &
TERM_PID=$!

sleep 12
kill "$TERM_PID" 2>/dev/null || true
