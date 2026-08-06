#!/usr/bin/env bash
# M3 demo: the stream UI on a live record. Three fixture panes are adopted
# as "implementer", "reviewer", and "builder"; two `cyclops ui --plain`
# followers run the whole time (the calm admin stream and the firehose)
# while the panes generate traffic: an agent-to-agent review request, a
# builder pane driven into blocked_permission and back, and a completion
# note sent to admin. The captured logs then show the M3 contract live:
#
#   - the admin stream stays calm: agent-to-agent chatter never appears,
#     only the blocked state, the message to admin, the attention_required
#     delivery (admin has no pane), and the daemon's ping
#   - the firehose shows everything, and the message to admin appears in
#     BOTH views
#   - the eye degrades to word lines in plain mode, opening and closing
#     with the attention count
#   - a late viewer catches up from the ledger tail (--backfill) and the
#     filter flags (--with) mirror history
#   - killing the daemon ends the follow with honest copy and exit 1
#
# The full-screen TUI (raw mode, the eye glyphs, tab/w/f/t/enter/c/?)
# cannot run headless; the script prints the command to try it manually.
#
# Never touches the default tmux server. Everything runs on a private
# server (tmux -u -L cyc-m3-demo-$$ -f /dev/null, -u per finding F14) with
# a throwaway CYCLOPS_HOME, both removed by the EXIT trap. Safe to run
# repeatedly. `bash -n demos/m3-stream.sh` must always pass.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SOCK="cyc-m3-demo-$$"
SESSION="demo"
# The scratch root and the tmux teardown rule are shared, not copied.
# shellcheck source=../tests/e2e/lib/lib.sh
. "$(cd "$(dirname "$0")" && pwd)/../tests/e2e/lib/lib.sh"
CYCLOPS_HOME="$(mktemp -d "$(cyc_scratch_root)/cyclops-m3-demo.XXXXXX")"
export CYCLOPS_HOME
CYC="$REPO/target/debug/cyclops"
DAEMON_PID=""
ADMIN_UI_PID=""
FIREHOSE_UI_PID=""

cd "$REPO"

for dep in tmux jq python3; do
  command -v "$dep" >/dev/null || { echo "!! $dep is required" >&2; exit 1; }
done

# Every tmux call in this script goes through the isolated socket.
tmx() { command tmux -u -L "$SOCK" "$@"; }

cleanup() {
  for pid in "$ADMIN_UI_PID" "$FIREHOSE_UI_PID" "$DAEMON_PID"; do
    if [ -n "$pid" ]; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  cyc_tmux_teardown "$SOCK"
  rm -rf "$CYCLOPS_HOME"
}
trap cleanup EXIT

echo "== demo home:   $CYCLOPS_HOME (removed on exit)"
echo "== tmux server: -L $SOCK (isolated, removed on exit)"

# The fixture "agent" the implementer and reviewer panes run, same shape
# as the m2 demo: @send runs cyclops send from INSIDE the pane (identity
# resolves from the pane, nothing in the request names the sender), and a
# delivered "[cyclops m-...]" header is reported through the real
# `cyclops hook` receiver, earning the tier-1 verified ack.
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

# The m2 fixture manifest plus one rule for this demo: a pane title
# carrying NEEDS APPROVAL reads as blocked_permission at priority 900,
# beating the always-idle floor. Title flips must hold across the 1Hz
# subscription tick (finding F23); the waits below ride the real events.
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
id = "needs_approval"
state = "blocked_permission"
priority = 900
region = "pane_title"
regex = ['NEEDS APPROVAL']

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

# Daemon config, m2 demo values: receipt_block_ms raised so a loaded
# machine prints delivered badges, ack_timeout_ms gives the fixture's
# hook process room to spawn.
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

# Isolated server, session "demo", three fixture panes. Pane ids are
# captured at creation, no ordering assumptions. The builder pane is a
# plain cat: state comes from its title, it never messages.
IMPL=$(command tmux -u -f /dev/null -L "$SOCK" new-session -d -P -F '#{pane_id}' \
  -s "$SESSION" -x 220 -y 50 "sh '$CYCLOPS_HOME/agent.sh' implementer '$CYC'")
REVW=$(tmx split-window -d -P -F '#{pane_id}' -h -t "$SESSION:0" \
  "sh '$CYCLOPS_HOME/agent.sh' reviewer '$CYC'")
BULD=$(tmx split-window -d -P -F '#{pane_id}' -v -t "$SESSION:0" "cat")
echo "== panes: $IMPL (implementer)  $REVW (reviewer)  $BULD (builder)"

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
python3 - "$CYCLOPS_HOME/sock" \
  "$IMPL" implementer "$REVW" reviewer "$BULD" builder <<'EOF'
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
# delivery is still in flight. The daemon never polls; this loop is the
# demo pacing its own narration.
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

# Bounded wait until a follower's log contains a fixed string; the
# followers are push-fed, this only paces the narration.
wait_log() {
  file="$1"; pat="$2"
  for _ in $(seq 1 75); do
    if grep -qF "$pat" "$file" 2>/dev/null; then return 0; fi
    sleep 0.2
  done
  echo "!! never saw \"$pat\" in $file; content so far:" >&2
  sed 's/^/   /' "$file" >&2 || true
  return 1
}

show_log() { sed 's/^/   /' "$1"; }

# Both stream views follow from the start. Plain mode is line-oriented:
# backfill first, then one line per admitted event, eye changes as word
# lines. The processes are the same `cyclops ui`, just headless.
echo
echo "== starting the followers: cyclops ui --plain (admin stream + firehose)"
"$CYC" ui --plain \
  >"$CYCLOPS_HOME/admin-stream.log" 2>"$CYCLOPS_HOME/admin-stream.err" &
ADMIN_UI_PID=$!
"$CYC" ui --plain --firehose >"$CYCLOPS_HOME/firehose.log" 2>&1 &
FIREHOSE_UI_PID=$!
# The firehose backfill replays the labeling lines; once one shows, both
# followers are past backfill and live entries flow through.
wait_log "$CYCLOPS_HOME/firehose.log" "labeled"

echo
echo "== beat 1: implementer asks reviewer for a review (agent to agent)"
tmx send-keys -t "$IMPL" -l '@send reviewer --subject "Review the rate limiter" --body "gateway.rs:120 drops the burst path"'
tmx send-keys -t "$IMPL" Enter
wait_settled 1

echo "== beat 2: the builder pane hits a permission wall (title-driven)"
tmx select-pane -t "$BULD" -T 'NEEDS APPROVAL: delete build cache'
"$CYC" wait builder --until blocked --timeout 15s
echo "== ...and the admin clears it; builder goes back to work"
tmx select-pane -t "$BULD" -T 'compiling'
"$CYC" wait builder --until idle --timeout 15s

echo "== beat 3: reviewer reports to admin (lands in BOTH views)"
tmx send-keys -t "$REVW" -l '@send admin --subject "Review done, one nit" --body "retry path could double-fire"'
tmx send-keys -t "$REVW" Enter
wait_settled 2

# Admin has no pane, so that delivery honestly resolves attention_required
# and the daemon pings; both lines belong in the admin stream. Wait until
# the ping reached the followers before reading the logs.
wait_log "$CYCLOPS_HOME/admin-stream.log" "delivery to admin needs attention"
wait_log "$CYCLOPS_HOME/firehose.log" "delivery to admin needs attention"

echo
echo "== the firehose saw everything:"
kill "$FIREHOSE_UI_PID" 2>/dev/null || true
wait "$FIREHOSE_UI_PID" 2>/dev/null || true
FIREHOSE_UI_PID=""
show_log "$CYCLOPS_HOME/firehose.log"

echo
echo "== a late viewer catches up from the record and filters it"
echo "== (cyclops ui --plain --firehose --with reviewer --backfill 50)"
"$CYC" ui --plain --firehose --with reviewer --backfill 50 \
  >"$CYCLOPS_HOME/late.log" 2>&1 &
LATE_PID=$!
wait_log "$CYCLOPS_HOME/late.log" "Review done, one nit"
kill "$LATE_PID" 2>/dev/null || true
wait "$LATE_PID" 2>/dev/null || true
show_log "$CYCLOPS_HOME/late.log"

echo
echo "== stopping cyclopsd; the admin follower ends honestly (exit 1)"
kill "$DAEMON_PID" 2>/dev/null || true
wait "$DAEMON_PID" 2>/dev/null || true
DAEMON_PID=""
rc=0
wait "$ADMIN_UI_PID" || rc=$?
ADMIN_UI_PID=""

echo
echo "== the admin stream stayed calm (full session):"
show_log "$CYCLOPS_HOME/admin-stream.log"
echo
echo "== connection-loss copy (stderr), exit code $rc:"
show_log "$CYCLOPS_HOME/admin-stream.err"

echo
echo "== checking the M3 contract against the captures"
fail=0
must() {
  if grep -qF "$2" "$1"; then echo "   ok: $3"; else
    echo "!! MISSING in ${1##*/}: $3" >&2; fail=1
  fi
}
must_not() {
  if grep -qF "$2" "$1"; then echo "!! LEAKED into ${1##*/}: $3" >&2; fail=1
  else echo "   ok: $3"; fi
}
A="$CYCLOPS_HOME/admin-stream.log"
F="$CYCLOPS_HOME/firehose.log"
must_not "$A" "Review the rate limiter" "agent-to-agent chatter stays out of the admin stream"
must "$F" "Review the rate limiter" "the firehose carries the agent-to-agent message"
must "$A" "blocked_permission" "the blocked builder reaches the admin stream"
must "$A" "eye opening" "the eye opened (word line, plain mode)"
must "$A" "eye closed" "the eye closed again when builder unblocked"
must "$A" "Review done, one nit" "the message to admin is in the admin stream"
must "$F" "Review done, one nit" "the message to admin is in the firehose too"
must "$A" "needs attention · no pane" "the pane-less admin delivery shows its honest state"
must "$CYCLOPS_HOME/late.log" "Review done, one nit" "backfill replays the record for a late viewer"
must_not "$CYCLOPS_HOME/late.log" "builder" "--with reviewer filters the builder's lines out"
must "$CYCLOPS_HOME/admin-stream.err" "lost the connection to cyclops" "connection loss ends the follow with honest copy"
if [ "$rc" -ne 1 ]; then
  echo "!! expected exit 1 from the severed follower, got $rc" >&2; fail=1
else
  echo "   ok: the severed follower exited 1"
fi
[ "$fail" -eq 0 ] || { echo "!! the demo contract did not hold" >&2; exit 1; }

echo
echo "== done, cleaning up"
echo
echo "The full-screen TUI is the manual half of this demo. Against your"
echo "own running daemon, in a real terminal:"
echo
echo "   ./target/debug/cyclops ui"
echo
echo "tab flips to the firehose, w/f/t filter, enter jumps tmux focus to"
echo "the entry's pane, c flips density, ? shows the cheatsheet, q quits."
