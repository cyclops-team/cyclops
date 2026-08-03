#!/usr/bin/env bash
# M2 demo: a reviewed conversation end to end. Two fixture panes are
# adopted as "implementer" and "reviewer"; the implementer sends a review
# request FROM ITS OWN PANE (the daemon's fail-closed identity walk names
# the sender, nothing in the request does), the reviewer replies with
# --reply-to, admin broadcasts an fyi, and then the M2 read side
# reconstructs all of it: `history --with`, `thread`, `wait --until idle`,
# `hooks verify`, and `hooks selftest` proving the ack hook round trip.
#
# The fixture panes run a tiny sh loop that acts like a hook-wired CLI:
# on every delivered "[cyclops m-...]" header it reports the line through
# the real `cyclops hook` receiver, so deliveries here earn
# "delivered · verified" the same way a wired vendor CLI does.
#
# Never touches the default tmux server. Everything runs on a private
# server (tmux -u -L cyc-m2-demo-$$ -f /dev/null, -u per finding F14) with
# a throwaway CYCLOPS_HOME, both removed by the EXIT trap. Safe to run
# repeatedly. `bash -n demos/m2-conversation.sh` must always pass.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SOCK="cyc-m2-demo-$$"
SESSION="demo"
# The scratch root and the tmux teardown rule are shared, not copied.
# shellcheck source=demos/lib.sh
. "$(cd "$(dirname "$0")" && pwd)/lib.sh"
CYCLOPS_HOME="$(mktemp -d "$(cyc_scratch_root)/cyclops-m2-demo.XXXXXX")"
export CYCLOPS_HOME
CYC="$REPO/target/debug/cyclops"
DAEMON_PID=""

cd "$REPO"

for dep in tmux jq python3; do
  command -v "$dep" >/dev/null || { echo "!! $dep is required" >&2; exit 1; }
done

# Every tmux call in this script goes through the isolated socket.
tmx() { command tmux -u -L "$SOCK" "$@"; }

cleanup() {
  if [ -n "$DAEMON_PID" ]; then
    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
  fi
  cyc_tmux_teardown "$SOCK"
  rm -rf "$CYCLOPS_HOME"
}
trap cleanup EXIT

echo "== demo home:   $CYCLOPS_HOME (removed on exit)"
echo "== tmux server: -L $SOCK (isolated, removed on exit)"

# The fixture "agent" each pane runs. It reads its terminal like a TUI and
# reacts to two line shapes; everything else just lands on screen via tty
# echo, which is what composer verification reads.
#   @send <args>     run cyclops send from INSIDE the pane, so the
#                    daemon's identity walk resolves the sender to this
#                    pane's label
#   [cyclops m-...]  a delivered message header; report it through the
#                    real `cyclops hook` receiver exactly as a wired
#                    vendor hook would (the daemon matches it as the
#                    tier-1 ack)
cat > "$CYCLOPS_HOME/agent.sh" <<'EOF'
label="$1"
cyc="$2"
while IFS= read -r line; do
  case "$line" in
    "@send "*)
      rest=${line#"@send "}
      eval "\"$cyc\" send $rest"
      ;;
    "[cyclops m-"*)
      printf '%s' "$line" | jq -Rs '{prompt: .}' \
        | "$cyc" hook UserPromptSubmit --agent "$label"
      ;;
  esac
done
EOF

# Demo manifest bound to the fixture panes: title tier always reads idle,
# staging is verified by the message id (m1 demo lineage), plus a [hooks]
# block declaring the tier-1 ack the fixture's hook reports match.
mkdir -p "$CYCLOPS_HOME/manifests"
cat > "$CYCLOPS_HOME/manifests/fixture.toml" <<'EOF'
[agent]
id = "fixture"
display_name = "Demo agent fixture"
process_names = ["cat", "sh", "bash", "dash"]

[hooks]
turn_start = "UserPromptSubmit"
turn_end = "Stop"
ack = "UserPromptSubmit"
ack_payload_field = "prompt"

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
# loaded machine still prints delivered badges instead of a legal queued
# receipt; ack_timeout_ms gives the fixture's hook process room to spawn.
cat > "$CYCLOPS_HOME/config.toml" <<EOF
sessions = ["$SESSION"]
tmux_socket = "$SOCK"
tmux_config = "/dev/null"
manifest_dir = "$CYCLOPS_HOME/manifests"
receipt_block_ms = 10000
ack_timeout_ms = 3000
EOF

echo "== building cyclopsd and cyclops"
cargo build -q -p cyclopsd -p cyclops

# Isolated server, session "demo", two fixture panes. CYCLOPS_HOME is
# exported above, so the panes inherit it and in-pane cyclops calls find
# the demo socket.
command tmux -u -f /dev/null -L "$SOCK" new-session -d -s "$SESSION" -x 220 -y 50 \
  "sh '$CYCLOPS_HOME/agent.sh' implementer '$CYC'"
tmx split-window -d -h -t "$SESSION:0" "sh '$CYCLOPS_HOME/agent.sh' reviewer '$CYC'"

PANES=()
while IFS= read -r p; do PANES+=("$p"); done < <(tmx list-panes -t "$SESSION" -F '#{pane_id}')
echo "== panes: ${PANES[0]} (implementer)  ${PANES[1]} (reviewer)"

echo "== starting cyclopsd (log: $CYCLOPS_HOME/cyclopsd.log)"
"$REPO/target/debug/cyclopsd" >"$CYCLOPS_HOME/cyclopsd.log" 2>&1 &
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

# Adopt the panes through the registry: pane.label over the NDJSON socket.
# Retries while the daemon attaches the session; each success writes a
# pane_labeled system line.
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

# Bounded demo-side wait until the record holds n message facts and no
# delivery is still in flight, so captures and reads show settled badges.
# The daemon never polls; this loop is the demo pacing its own narration.
wait_settled() {
  want="$1"
  for _ in $(seq 1 75); do
    if out=$("$CYC" history --json 2>/dev/null); then
      got=$(printf '%s' "$out" | jq '[.lines[] | select(.kind == "msg" or .kind == "fyi")] | length')
      open=$(printf '%s' "$out" | jq '[.lines[].deliveries[]?.state
        | select(. == "queued" or . == "gating" or . == "pasting"
                 or . == "staged" or . == "submitted" or . == "retry_queued")] | length')
      if [ "$got" -ge "$want" ] && [ "$open" -eq 0 ]; then return 0; fi
    fi
    sleep 0.2
  done
  echo "!! the record never settled at $want messages" >&2
  return 1
}

# Trim trailing blank pane lines for readable captures.
show_pane() {
  tmx capture-pane -p -t "$1" | awk 'NF{n=NR} {l[NR]=$0} END{for(i=1;i<=n;i++) print "   " l[i]}'
}

echo
echo "== cyclops wait implementer --until idle (resolves immediately)"
"$CYC" wait implementer --until idle

echo
echo "== implementer sends from its own pane (identity comes from the pane)"
tmx send-keys -t "${PANES[0]}" -l '@send reviewer --subject "Review the rate limiter" --body "gateway.rs:120 drops the burst path"'
tmx send-keys -t "${PANES[0]}" Enter
wait_settled 1

echo "== reviewer pane after delivery (fixture acked it via cyclops hook):"
show_pane "${PANES[1]}"

M1_ID=$("$CYC" history --json | jq -r '.lines[0].id')
echo
echo "== reviewer replies with --reply-to $M1_ID"
tmx send-keys -t "${PANES[1]}" -l "@send implementer --reply-to $M1_ID --subject \"Re: Review the rate limiter\" --body \"Done. One nit in the retry path.\""
tmx send-keys -t "${PANES[1]}" Enter
wait_settled 2

echo "== implementer pane after the reply landed:"
show_pane "${PANES[0]}"

echo
echo "== cyclops send --all --fyi (broadcast: one record fact, N deliveries)"
"$CYC" send --all --subject "Standup in 5" --fyi
# A fixture pane that just shelled out can read as briefly busy, making the
# broadcast receipt answer queued (honest, self-healing); settle before the
# read side so history shows the resolved badges.
wait_settled 3

echo
echo "== cyclops history --with reviewer (the conversation, reconstructed)"
"$CYC" history --with reviewer

echo
echo "== cyclops thread $M1_ID (the review request and its reply)"
"$CYC" thread "$M1_ID"

echo
echo "== cyclops hooks verify reviewer (edges seen this run)"
"$CYC" hooks verify reviewer

echo
echo "== cyclops hooks selftest reviewer (one no-op turn, ack hook proven)"
"$CYC" hooks selftest reviewer

LEDGER="$CYCLOPS_HOME/ledger/$SESSION.ndjson"
echo
echo "== ledger msg facts ($LEDGER)"
jq -c 'select(.kind == "msg" or .kind == "fyi")
       | {seq, kind, id, from, to, subject, reply_to}' "$LEDGER"

echo
echo "== ledger self-test record (a system fact, never a secret)"
jq -c 'select(.kind == "system" and (.data.event? == "hook_selftest"))
       | {id, data}' "$LEDGER"

echo
echo "== done, cleaning up"
