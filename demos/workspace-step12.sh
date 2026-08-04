#!/usr/bin/env bash
# Step 12: agent decoration — cyclopsd with labeled cat panes, workspace TUI.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SOCK="cyc-ws-step12-demo"
SESSION="agents"
# shellcheck source=demos/lib.sh
. "$(cd "$(dirname "$0")" && pwd)/lib.sh"
CYCLOPS_HOME="$(cyc_scratch_root)/cyclops-ws12-demo"
export CYCLOPS_HOME
DAEMON_PID=""
CYC="$REPO/target/debug/cyclops"

cleanup() {
  cyc_stop_daemon DAEMON_PID
  cyc_tmux_teardown "$SOCK"
  rm -rf "$CYCLOPS_HOME"
}
trap cleanup EXIT

cd "$REPO"
tmux() { command tmux -u -L "$SOCK" -f /dev/null "$@"; }

cyc_tmux_teardown "$SOCK"
rm -rf "$CYCLOPS_HOME"
mkdir -p "$CYCLOPS_HOME/manifests"
cat >"$CYCLOPS_HOME/manifests/cat.toml" <<'EOF'
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

cat >"$CYCLOPS_HOME/config.toml" <<EOF
sessions = ["$SESSION"]
tmux_socket = "$SOCK"
tmux_config = "/dev/null"
manifest_dir = "$CYCLOPS_HOME/manifests"
receipt_block_ms = 4000
EOF

tmux new-session -d -s "$SESSION" -x 110 -y 30 cat
tmux split-window -h -t "$SESSION:0" cat

PANES=()
while IFS= read -r p; do PANES+=("$p"); done < <(tmux list-panes -t "$SESSION" -F '#{pane_id}')

cargo build -q -p cyclopsd -p cyclops

cargo run -q -p cyclopsd >"$CYCLOPS_HOME/cyclopsd.log" 2>&1 &
DAEMON_PID=$!

ok=""
for _ in $(seq 1 50); do
  if [ -S "$CYCLOPS_HOME/sock" ]; then ok=1; break; fi
  if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
    tail -n 20 "$CYCLOPS_HOME/cyclopsd.log" >&2 || true
    exit 1
  fi
  sleep 0.2
done
[ -n "$ok" ] || { tail -n 20 "$CYCLOPS_HOME/cyclopsd.log" >&2; exit 1; }

python3 - "$CYCLOPS_HOME/sock" "${PANES[0]}" implementer "${PANES[1]}" reviewer <<'EOF'
import json, socket, sys, time

sock_path = sys.argv[1]
pairs = list(zip(sys.argv[2::2], sys.argv[3::2]))

def rpc(method, params):
    s = socket.socket(socket.AF_UNIX)
    s.settimeout(5)
    s.connect(sock_path)
    f = s.makefile("rw")
    f.readline()
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
            break
        if time.time() > deadline:
            sys.exit(1)
        time.sleep(0.2)
EOF

echo "=== Workspace Step 12: agent decoration ==="
echo "Badges on pane borders; C-b e opens the event panel."
echo ""

export TMUX= TMUX_PANE=
exec "$CYC"
