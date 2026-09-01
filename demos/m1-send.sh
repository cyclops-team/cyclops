#!/usr/bin/env bash
# Current mailbox demo: durable messaging without either Cyclops UI. cyclopsd
# watches an isolated tmux session with two cat panes adopted as
# "implementer" and "reviewer". `cyclops send` durably accepts a private
# message before its optional terminal wake, a broadcast fans out to every
# labeled pane, and the body-free projection and workspace journal provide
# independent evidence of the result.
#
# Never touches the default tmux server. Everything runs on a private server
# (tmux -u -L cyc-m1-demo-$$ -f /dev/null, -u per finding F14) with a
# throwaway CYCLOPS_HOME, both removed by the EXIT trap. Safe to run
# repeatedly. `bash -n demos/m1-send.sh` must always pass.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SOCK="cyc-m1-demo-$$"
SESSION="demo"
# The scratch root and the tmux teardown rule are shared, not copied.
# shellcheck source=../tests/e2e/lib/lib.sh
. "$(cd "$(dirname "$0")" && pwd)/../tests/e2e/lib/lib.sh"
CYCLOPS_HOME="$(mktemp -d "$(cyc_scratch_root)/cyclops-m1-demo.XXXXXX")"
export CYCLOPS_HOME
DAEMON_PID=""
ADMIN_COMMAND_INDEX=0

cd "$REPO"

for dep in tmux jq python3; do
  command -v "$dep" >/dev/null || { echo "!! $dep is required" >&2; exit 1; }
done

# Every tmux call in this script goes through the isolated socket.
tmx() { command tmux -u -L "$SOCK" "$@"; }

# Run an operator command from a short-lived tmux pane. This keeps the demo
# valid when its parent shell belongs to a detected agent: the socket peer is
# then an ordinary non-vendor pane, which Cyclops correctly authenticates as
# the workspace administrator. tmux wait-for is the completion event, so the
# demo never guesses how long the command needs.
admin_command() {
  ADMIN_COMMAND_INDEX=$((ADMIN_COMMAND_INDEX + 1))
  local output="$CYCLOPS_HOME/admin-command-$ADMIN_COMMAND_INDEX.out"
  local status="$CYCLOPS_HOME/admin-command-$ADMIN_COMMAND_INDEX.status"
  local signal="cyc-m1-admin-$ADMIN_COMMAND_INDEX-$$"
  local command_line="$1"

  tmx new-window -d -t "$SESSION" \
    "env CYCLOPS_HOME='$CYCLOPS_HOME' $command_line >'$output' 2>&1; result=\$?; printf '%s\\n' \"\$result\" >'$status'; command tmux -u -L '$SOCK' wait-for -S '$signal'"
  tmx wait-for "$signal"
  cat "$output"
  test "$(cat "$status")" -eq 0
}

cleanup() {
  if [ -n "$DAEMON_PID" ]; then
    # cargo run is the parent; kill the cyclopsd child first, then cargo.
    pkill -TERM -P "$DAEMON_PID" 2>/dev/null || true
    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
  fi
  cyc_tmux_teardown "$SOCK"
  rm -rf "$CYCLOPS_HOME"
}
trap cleanup EXIT

echo "== demo home:   $CYCLOPS_HOME (removed on exit)"
echo "== tmux server: -L $SOCK (isolated, removed on exit)"

# Isolated server, session "demo", two cat panes. They provide a visible
# terminal wake target without either Cyclops UI.
command tmux -u -f /dev/null -L "$SOCK" new-session -d -s "$SESSION" -x 200 -y 50 cat
tmx split-window -d -h -t "$SESSION:0" cat

PANES=()
while IFS= read -r p; do PANES+=("$p"); done < <(tmx list-panes -t "$SESSION" -F '#{pane_id}')
echo "== panes: ${PANES[0]} (implementer)  ${PANES[1]} (reviewer)"

# Demo manifest bound to cat panes: title tier always reads idle, but the
# fixture intentionally declares no injection capability. This proves that
# durable messaging still works when terminal notification is unavailable.
# The shipped manifests bind real agent CLIs; a demo cat pane needs its own.
mkdir -p "$CYCLOPS_HOME/manifests"
cat > "$CYCLOPS_HOME/manifests/cat.toml" <<'EOF'
[agent]
id = "cat"
display_name = "Cat demo fixture"
process_names = ["cat", "sh", "bash", "dash"]

[[rule]]
id = "always_idle"
state = "idle"
priority = 100
region = "pane_title"
regex = ['^']
EOF

# Daemon config. receipt_block_ms is raised above the 2500 default so a
# slow machine still prints delivered badges instead of a legal queued
# receipt. It must stay under the CLI's 5s socket read timeout.
cat > "$CYCLOPS_HOME/config.toml" <<EOF
sessions = ["$SESSION"]
tmux_socket = "$SOCK"
tmux_config = "/dev/null"
manifest_dir = "$CYCLOPS_HOME/manifests"
receipt_block_ms = 4000
EOF

echo "== building cyclopsd and cyclops"
cargo build -q -p cyclopsd -p cyclops

echo "== starting cyclopsd (log: $CYCLOPS_HOME/cyclopsd.log)"
cargo run -q -p cyclopsd >"$CYCLOPS_HOME/cyclopsd.log" 2>&1 &
DAEMON_PID=$!

# Bounded startup wait for the daemon socket. Demo-script waiting, not the
# daemon's zero-polling contract; cyclopsd itself stays event-driven.
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

# Adopt the panes through the registry over the NDJSON socket. Retries while
# the daemon attaches the session; each success writes a pane_labeled system
# line.
echo "== labeling panes via pane.label"
python3 - "$CYCLOPS_HOME/sock" "${PANES[0]}" implementer "${PANES[1]}" reviewer <<'EOF'
import json, socket, sys, time

sock_path = sys.argv[1]
pairs = list(zip(sys.argv[2::2], sys.argv[3::2]))

def rpc(method, params):
    s = socket.socket(socket.AF_UNIX)
    s.settimeout(5)
    s.connect(sock_path)
    f = s.makefile("rw")
    f.readline()  # hello line
    f.write(json.dumps({"id": 1, "method": method, "params": params}) + "\n")
    f.flush()
    resp = json.loads(f.readline())
    s.close()
    return resp

deadline = time.time() + 20
for pane, label in pairs:
    while True:
        resp = rpc("pane.label", {"target": pane, "label": label})
        if "error" not in resp or resp["error"] is None:
            print(f"   {pane} -> {label}")
            break
        # no_such_target until the session watcher attaches; keep trying.
        if time.time() > deadline:
            print(f"!! labeling {pane} failed: {resp['error']}", file=sys.stderr)
            sys.exit(1)
        time.sleep(0.2)
EOF

echo
echo "== cyclops send implementer (durable acceptance precedes the optional wake)"
admin_command "'$REPO/target/debug/cyclops' send implementer --subject 'Review the rate limiter' --summary 'The rate limiter is ready for review. Check retry.rs before the next run.' --body 'Please look at retry.rs before the next run. Both lines remain one private message.'"

echo
echo "== cyclops send --all (broadcast: one ledger fact, N deliveries)"
admin_command "'$REPO/target/debug/cyclops' send --all --subject 'standup' --summary 'Standup is ready for both agents. Join after reaching a safe stopping point.' --body 'Broadcast to every labeled pane.'"

echo
echo "== body-free authenticated projection (no UI required)"
admin_command "'$REPO/target/debug/cyclops' messages --plain"

JOURNAL=""
for candidate in "$CYCLOPS_HOME"/workspaces/*/messages.ndjson; do
  if [ -f "$candidate" ]; then JOURNAL="$candidate"; break; fi
done
[ -n "$JOURNAL" ] || { echo "!! workspace journal was not created" >&2; exit 1; }

echo
echo "== workspace message facts ($JOURNAL; bodies intentionally omitted)"
jq -c 'select(.kind == "msg") | {seq, id, from, to, subject}' "$JOURNAL"

echo
echo "== optional wake facts (content-free and honestly classified)"
jq -c 'select(.data.type? == "notification_transition")
       | {seq, id, state: .data.state, cause: .data.cause}' "$JOURNAL"

echo
echo "== done, cleaning up"
