#!/usr/bin/env bash
# M0 demo: cyclopsd watching an isolated tmux session, then `cyclops status`
# and `cyclops ping` against it.
#
# Never touches the default tmux server. Everything runs on a private server
# (tmux -L cyc-demo-$$ -f /dev/null) with a throwaway CYCLOPS_HOME, both
# removed by the EXIT trap. Safe to run repeatedly.
#
# cyclopsd and cyclops are implemented in parallel with this script; it is
# written against the documented M0 interfaces and may not run until
# integration lands. `bash -n demos/m0-status.sh` must always pass.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SOCK="cyc-demo-$$"
SESSION="demo"
# /private/tmp is macOS only and keeps socket paths short; elsewhere the
# system temp dir is already short. CYCLOPS_TEST_TMP overrides both.
TMPROOT="${CYCLOPS_TEST_TMP:-$([ -d /private/tmp ] && echo /private/tmp || echo "${TMPDIR:-/tmp}")}"
CYCLOPS_HOME="$(mktemp -d "$TMPROOT/cyclops-demo.XXXXXX")"
export CYCLOPS_HOME
DAEMON_PID=""

cd "$REPO"

# Every tmux call in this script goes through the isolated socket.
tmx() { command tmux -L "$SOCK" "$@"; }

cleanup() {
  if [ -n "$DAEMON_PID" ]; then
    # cargo run is the parent; kill the cyclopsd child first, then cargo.
    pkill -TERM -P "$DAEMON_PID" 2>/dev/null || true
    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
  fi
  tmx kill-server 2>/dev/null || true
  rm -rf "$CYCLOPS_HOME"
}
trap cleanup EXIT

echo "== demo home:   $CYCLOPS_HOME (removed on exit)"
echo "== tmux server: -L $SOCK (isolated, killed on exit)"

# Isolated server, session "demo", two shell panes with titles.
command tmux -f /dev/null -L "$SOCK" new-session -d -s "$SESSION" -x 180 -y 45
tmx split-window -h -t "$SESSION"
tmx select-pane -t "$SESSION.0" -T "implementer"
tmx select-pane -t "$SESSION.1" -T "reviewer"

echo "== panes:"
tmx list-panes -t "$SESSION" -F '   #{pane_id}  #{pane_title}  (#{pane_current_command})'

# Daemon config. cyclopsd was a stub when this was written; these are the
# documented M0 keys. Daemon agent: keep these keys or fix this file in the
# same change (sessions, tmux_socket, manifest_dir).
cat > "$CYCLOPS_HOME/config.toml" <<EOF
sessions = ["$SESSION"]
tmux_socket = "$SOCK"
manifest_dir = "$REPO/manifests"
EOF

echo "== building cyclopsd and cyclops"
cargo build -q -p cyclopsd -p cyclops

echo "== starting cyclopsd (log: $CYCLOPS_HOME/cyclopsd.log)"
cargo run -q -p cyclopsd >"$CYCLOPS_HOME/cyclopsd.log" 2>&1 &
DAEMON_PID=$!

# Bounded startup wait for the daemon socket ($CYCLOPS_HOME/sock). This is a
# demo script waiting for a process to boot, not the daemon's zero-polling
# contract; cyclopsd itself stays event-driven.
ok=""
for _ in $(seq 1 50); do
  if [ -S "$CYCLOPS_HOME/sock" ]; then ok=1; break; fi
  if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
    echo "!! cyclopsd exited during startup; log tail:" >&2
    tail -n 20 "$CYCLOPS_HOME/cyclopsd.log" >&2 || true
    exit 1
  fi
  sleep 0.2
done
if [ -z "$ok" ]; then
  echo "!! daemon socket never appeared at $CYCLOPS_HOME/sock; log tail:" >&2
  tail -n 20 "$CYCLOPS_HOME/cyclopsd.log" >&2 || true
  exit 1
fi

echo
echo "== cyclops ping"
cargo run -q -p cyclops -- ping

echo
echo "== cyclops status"
cargo run -q -p cyclops -- status

echo
echo "== done, cleaning up"
