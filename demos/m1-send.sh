#!/usr/bin/env bash
# M1 demo: the delivery pipeline end to end. cyclopsd watches an isolated
# tmux session with two cat panes adopted as "implementer" and "reviewer"
# via pane.label, then `cyclops send` pastes a message into a pane (visible
# in the capture), a broadcast fans out to every labeled pane, and jq reads
# the delivery record straight from the session ledger.
#
# Never touches the default tmux server. Everything runs on a private server
# (tmux -u -L cyc-m1-demo-$$ -f /dev/null, -u per finding F14) with a
# throwaway CYCLOPS_HOME, both removed by the EXIT trap. Safe to run
# repeatedly. `bash -n demos/m1-send.sh` must always pass.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SOCK="cyc-m1-demo-$$"
SESSION="demo"
# /private/tmp is macOS only and keeps socket paths short; elsewhere the
# system temp dir is already short. CYCLOPS_TEST_TMP overrides both.
TMPROOT="${CYCLOPS_TEST_TMP:-$([ -d /private/tmp ] && echo /private/tmp || echo "${TMPDIR:-/tmp}")}"
CYCLOPS_HOME="$(mktemp -d "$TMPROOT/cyclops-m1-demo.XXXXXX")"
export CYCLOPS_HOME
DAEMON_PID=""

cd "$REPO"

for dep in tmux jq python3; do
  command -v "$dep" >/dev/null || { echo "!! $dep is required" >&2; exit 1; }
done

# Every tmux call in this script goes through the isolated socket.
tmx() { command tmux -u -L "$SOCK" "$@"; }

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

# Isolated server, session "demo", two cat panes. cat echoes whatever the
# pipeline pastes, so the delivery is visible in a capture.
command tmux -u -f /dev/null -L "$SOCK" new-session -d -s "$SESSION" -x 200 -y 50 cat
tmx split-window -d -h -t "$SESSION:0" cat

PANES=()
while IFS= read -r p; do PANES+=("$p"); done < <(tmx list-panes -t "$SESSION" -F '#{pane_id}')
echo "== panes: ${PANES[0]} (implementer)  ${PANES[1]} (reviewer)"

# Demo manifest bound to cat panes: title tier always reads idle, staging
# is verified by the message id, no hook ACK (screen tier). The shipped
# manifests bind claude/codex/agy; a demo cat pane needs its own.
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

[injection]
method = "load-buffer + paste-buffer -p"
submit = "Enter"
verify_before_submit = true
verify_pattern = ["<message_id>"]
safe_states = ["idle"]
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

# Adopt the panes through the registry: pane.label over the NDJSON socket
# (the CLI grows a verb for this in M2). Retries while the daemon attaches
# the session; each success writes a pane_labeled system line.
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
echo "== cyclops send implementer (watch the paste land)"
cargo run -q -p cyclops -- send implementer \
  --subject "Review the rate limiter" \
  --body $'Please look at retry.rs before the next run.\nBoth lines paste as one message.'

echo
echo "== implementer pane (${PANES[0]}) after delivery:"
tmx capture-pane -p -t "${PANES[0]}" | awk 'NF{n=NR} {l[NR]=$0} END{for(i=1;i<=n;i++) print "   " l[i]}'

echo
echo "== cyclops send --all (broadcast: one ledger fact, N deliveries)"
cargo run -q -p cyclops -- send --all \
  --subject "standup" \
  --body "Broadcast to every labeled pane."

LEDGER="$CYCLOPS_HOME/ledger/$SESSION.ndjson"
echo
echo "== ledger msg lines ($LEDGER)"
jq -c 'select(.kind == "msg") | {seq, id, from, to, subject}' "$LEDGER"

echo
echo "== ledger state lines (every delivery transition, causes never screens)"
# kind=state also carries fused pane-state changes; to_state marks the
# delivery transitions.
jq -c 'select(.kind == "state" and .data.to_state != null)
       | {seq, id, to: .data.to, from: .data.from, to_state: .data.to_state, cause: .data.cause}' \
  "$LEDGER"

echo
echo "== done, cleaning up"
