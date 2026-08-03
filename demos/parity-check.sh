#!/usr/bin/env bash
# Every command shape README.md and docs/ show, run for real and checked
# against what the binaries print today.
#
# A doc describing output the binaries no longer print is a bug (GOALS.md,
# truth rule). This is the regression that catches it: it walks the README
# ladder rung by rung on a throwaway rig, prints each command and its
# output, and asserts the shapes the docs promise. Exit 0 means the docs
# and the binaries still agree.
#
# The transcript this prints is where the README's output blocks come from.
# Change a line the README quotes and this fails, so the two move together.
#
# Isolation is TMUX_TMPDIR, not `tmux -L`. Rung 1 is the first run with no
# config file, and the config file is the only place a tmux socket name can
# be set, so the rig cannot pass one. A private TMUX_TMPDIR gives the
# default tmux server its own directory: nothing here can reach the server
# your own sessions live on. CYCLOPS_HOME is throwaway too. Both go on the
# EXIT trap.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
# The scratch root, the tmux teardown rule and the daemon stop are
# shared, not copied.
# shellcheck source=demos/lib.sh
. "$(cd "$(dirname "$0")" && pwd)/lib.sh"

ROOT="$(mktemp -d "$(cyc_scratch_root)/cyclops-parity.XXXXXX")"
export TMUX_TMPDIR="$ROOT/tmux"
mkdir -p "$TMUX_TMPDIR"
# An inherited $TMUX would redirect a socket-less tmux call at the server
# the caller is attached to, which is the one server this must never touch.
unset TMUX
export CYCLOPS_HOME="$ROOT/home"
mkdir -p "$CYCLOPS_HOME"
CYC="$REPO/target/debug/cyclops"
CYCD="$REPO/target/debug/cyclopsd"
OUT="$ROOT/out"
DAEMON_PID=""
CHECKS=0
FAILS=0

cd "$REPO"

for dep in tmux jq; do
  command -v "$dep" >/dev/null || { echo "!! $dep is required" >&2; exit 1; }
done

tmx() { command tmux -u "$@"; }

start_daemon() {
  "$CYCD" >>"$ROOT/daemon.log" 2>&1 &
  DAEMON_PID=$!
  wait_for "the daemon socket" 50 test -S "$CYCLOPS_HOME/sock"
  wait_for "cyclopsd to attach" 50 daemon_attached
}

cleanup() {
  cyc_stop_daemon
  cyc_tmux_teardown default
  # The nested duo rig runs its own tmux server in its own directory.
  if [ -d "$ROOT/duo/tmux" ]; then
    TMUX_TMPDIR="$ROOT/duo/tmux" cyc_tmux_teardown default
  fi
  rm -rf "$ROOT"
}
trap cleanup EXIT

# Wait for a condition, checking every 200ms. Rig pacing only: the daemon
# itself never polls, and nothing here is on cyclops's own path.
wait_for() {
  local what="$1" tries="$2"; shift 2
  for _ in $(seq 1 "$tries"); do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep 0.2
  done
  echo "!! gave up waiting for $what" >&2
  return 1
}

daemon_attached() { "$CYC" --json status | jq -e '.sessions[0].attached == true' >/dev/null; }
pane_known() { "$CYC" --json status | jq -e --arg p "$1" '[.sessions[].panes[].pane_id] | index($p)' >/dev/null; }
roster_has() { "$CYC" --json list | jq -e --arg a "$1" '[.agents[].agent] | index($a)' >/dev/null; }
roster_empty() { "$CYC" --json list | jq -e '.agents | length == 0' >/dev/null; }
all_idle() { "$CYC" --json list | jq -e '[.agents[].state] | all(. == "idle")' >/dev/null; }

# No delivery still moving. Badges are read from the record, so a read
# taken mid-flight shows a legal in-flight state and the transcript stops
# being reproducible.
settled() {
  "$CYC" --json history | jq -e '[.lines[].deliveries[]?.state
    | select(. == "queued" or . == "gating" or . == "pasting"
             or . == "staged" or . == "submitted" or . == "retry_queued")]
    | length == 0' >/dev/null
}

# Run a command, show it the way the README shows it, keep the output for
# checking. stderr is folded in because half the copy under test is error
# copy, and a reader does not see the difference either.
#
# Two things are stripped from the printed line and from nothing else: the
# build directory the rig calls the binaries through, and `--plain`, which
# every call here passes to take color and glyph animation off. --plain
# prints the same words a colored terminal does (GOALS: it is the
# screen-reader path, not a reduced view), so the line printed is the line
# a reader types.
run() {
  local built="$REPO/target/debug/" shown="" a
  for a in "$@"; do
    a="${a//$built/}"
    [ "$a" = "--plain" ] && continue
    case "$a" in
      *" "*) shown="$shown \"$a\"" ;;
      *) shown="$shown $a" ;;
    esac
  done
  printf '\n$ %s\n' "${shown# }"
  set +e
  "$@" >"$OUT" 2>&1
  local code=$?
  set -e
  cat "$OUT"
  printf '%s' "$code" > "$ROOT/exit"
}

# Assert the last `run` printed something matching an extended regex.
check() {
  local what="$1" pattern="$2"
  CHECKS=$((CHECKS + 1))
  if grep -qE -- "$pattern" "$OUT"; then
    printf '   ok    %s\n' "$what"
  else
    printf '   FAIL  %s\n         wanted /%s/\n' "$what" "$pattern"
    FAILS=$((FAILS + 1))
  fi
}

# Assert the last `run` exited with this code. Exit codes are documented
# per command and scripts branch on them.
check_exit() {
  local what="$1" want="$2" got
  got="$(cat "$ROOT/exit")"
  CHECKS=$((CHECKS + 1))
  if [ "$got" = "$want" ]; then
    printf '   ok    %s\n' "$what"
  else
    printf '   FAIL  %s\n         wanted exit %s, got %s\n' "$what" "$want" "$got"
    FAILS=$((FAILS + 1))
  fi
}

echo "== rig home:   $CYCLOPS_HOME (removed on exit)"
echo "== tmux:       private TMUX_TMPDIR=$TMUX_TMPDIR (removed on exit)"

# A parity gate that ran yesterday's binaries would pass while the docs are
# already wrong, which is the one failure this script exists to catch.
echo "== building cyclops and cyclopsd"
cargo build -q -p cyclops -p cyclopsd

# The stand-in agent. It is a shell loop, not a vendor CLI, and that is the
# point of rung 3: cyclops can address it because of one manifest file.
#
# It reacts to two line shapes and ignores everything else:
#
#   [cyclops m-...]  a delivered header. It reports the line through the
#                    real `cyclops hook` receiver exactly as a wired vendor
#                    hook would, so deliveries here earn
#                    "delivered · verified" the same way a real CLI does.
#                    Both turn edges, not just the ack: a CLI that opened
#                    turns and never closed them would leave the pane
#                    looking mid-turn forever, and the next delivery would
#                    queue behind a turn that already ended.
#   @send <args>     run `cyclops send` from INSIDE this pane, so the
#                    daemon resolves the sender from the process rather
#                    than from anything the request says.
cat > "$ROOT/agent.sh" <<'EOF'
label="$1"
cyc="$2"
while IFS= read -r line; do
  case "$line" in
    "[cyclops m-"*)
      printf '%s' "$line" | jq -Rs '{prompt: .}' \
        | "$cyc" hook UserPromptSubmit --agent "$label"
      printf '{}' | "$cyc" hook Stop --agent "$label"
      ;;
    "@send "*)
      rest=${line#"@send "}
      eval "\"$cyc\" send $rest"
      ;;
  esac
done
EOF

mkdir -p "$CYCLOPS_HOME/manifests"
cp "$REPO"/manifests/*.toml "$CYCLOPS_HOME/manifests/"
cat > "$CYCLOPS_HOME/manifests/demo.toml" <<'EOF'
[agent]
id = "demo"
display_name = "Parity rig stand-in"
process_names = ["sh", "bash", "dash", "zsh", "cat"]

[hooks]
turn_start = "UserPromptSubmit"
turn_end = "Stop"
ack = "UserPromptSubmit"
ack_payload_field = "prompt"

[[rule]]
id = "title_working"
state = "working"
priority = 1100
region = "pane_title"
regex = ['^Implementing|^Reviewing']

[[rule]]
id = "title_idle"
state = "idle"
priority = 1000
region = "pane_title"
regex = ['^']

[injection]
method = "load-buffer + paste-buffer -p"
submit = "Enter"
verify_before_submit = true
verify_pattern = ["<message_id>"]
safe_states = ["idle"]
EOF

echo
echo "#### Rung 1: one pane, persistence, history"

run "$CYC" start --plain
check "start builds the solo workspace"   '^✓ workspace ready · 1 agent$'
check "start writes the config"           'wrote .*/config\.toml$'
check "step 1 starts the daemon"          '^  1  cyclopsd &  +start the daemon$'
check "step 2 attaches"                   '^  2  tmux attach -t main  +open the workspace and start your agents$'
check "step 3 sends the first message"    '^  3  cyclops send implementer --subject "hello"  +send the first message$'

P1="$(tmx list-panes -t main -F '#{pane_id}')"
# A person attaches here and starts an agent. The rig starts its stand-in,
# and clears the pane title tmux seeded with the hostname so the roster
# below shows what an agent publishes rather than what tmux did.
tmx respawn-pane -k -t "$P1" "sh '$ROOT/agent.sh' implementer '$CYC'"
tmx select-pane -t "$P1" -T ''

# Two budgets raised above their defaults, for the rig and not for cyclops.
# The stand-in answers a delivery by spawning jq and `cyclops hook`, which
# costs far more than a vendor hook already loaded in the agent's process,
# so at the shipped defaults the receipt returns "queued" and the ack lands
# after the window. Raising both makes the transcript reproducible instead
# of racing this machine. Appending is safe: `cyclops start` wrote this
# file on its first run and never edits one that exists.
#
# receipt_block_ms stays under the CLI's own 5-second socket read deadline
# (crates/cyclops/src/client.rs, READ_TIMEOUT). Past it the daemon is still
# holding the receipt when the client gives up, and a delivery that is
# going fine reports a lost connection.
cat >> "$CYCLOPS_HOME/config.toml" <<'EOF'
receipt_block_ms = 4000
ack_timeout_ms = 3000
EOF

# Only the daemon can put a name on a pane, and there was none when the
# first `start` built the session. The second run is where the name lands.
start_daemon

run "$CYC" start --plain
check "a second start is one line"        '^✔ workspace ready · 1 agent$'

wait_for "the roster to hold implementer" 50 roster_has implementer

run "$CYC" send implementer --subject "Review the rate limiter" --body "gateway.rs:120 drops the burst path" --plain
check "the receipt is a verified badge"   '^✔ delivered · verified$'
check_exit "a delivered send exits 0" 0

run "$CYC" history --plain
check "history folds the delivery badge"  '^ +[0-9]+s +admin → implementer +Review the rate limiter +✔ delivered · verified$'

echo
echo "-- the record survives a daemon restart"
cyc_stop_daemon
start_daemon
wait_for "the roster to come back" 50 roster_has implementer

run "$CYC" history --plain
check "the record is still there"         'admin → implementer +Review the rate limiter +✔ delivered · verified$'

run "$CYC" list --plain
check "the name is still there"           '^ +implementer +○ idle$'

run "$CYC" ping --plain
check "ping reports the round trip"       '^✔ cyclops is up · [0-9.]+ms$'

echo
echo "#### Rung 2: name panes"

tmx split-window -d -t main "sh '$ROOT/agent.sh' reviewer '$CYC'"
P2="$(tmx list-panes -t main -F '#{pane_id}' | tail -1)"
tmx select-pane -t "$P2" -T ''
wait_for "cyclopsd to see the new pane" 50 pane_known "$P2"

run "$CYC" status --plain
check "the header carries the eye"        '^‿ cyclops · watching main · tmux .* · up [0-9]'
check "status lists a pane with no name"  "^ +$P2 +○ idle"

run "$CYC" name "$P2" reviewer --plain
check "naming confirms name and pane"     "^✔ named reviewer · $P2\$"

wait_for "the roster to hold reviewer" 50 roster_has reviewer
tmx select-pane -t "$P1" -T 'Implementing rate limiter'
sleep 2.5

run "$CYC" list --plain
check "the roster is three columns"       '^ +implementer +● working +Implementing rate limiter$'
check "and one row per named agent"       '^ +reviewer +○ idle$'

printf '\n$ tmux display-message -p -t %s "#{E:pane-border-format}"\n' "$P2"
tmx display-message -p -t "$P2" '#{E:pane-border-format}' > "$OUT"
cat "$OUT"
check "the border reads role then state"  'reviewer.*•.*○ idle'

tmx select-pane -t "$P1" -T ''
sleep 2.5

echo
echo "#### Rung 3: any terminal agent"

run "$CYC" read reviewer --source detection --plain
check "detection names the deciding rule" '^reviewer · ○ idle · decided by title_idle$'
check "and shows the sensor that read it" '^ +title +○ idle +title_idle'

run "$CYC" name "$P2" reviewer --manifest cluade --plain
check "an unknown manifest lists the known ones" '^no manifest "cluade"; loaded: agy, claude, codex, demo$'

echo
echo "#### Rung 4: layouts"

run "$CYC" workspace save --plain
check "save reports panes, agents, path"  '^✔ workspace saved · main · 2 panes · 2 agents · .*/workspaces/main\.toml$'

printf '\n$ tmux kill-session -t main\n'
tmx kill-session -t '=main'
wait_for "the roster to empty" 50 roster_empty

# Same two runs as rung 1, for the same reason: the first rebuilds the
# session, and only a daemon that has re-attached to it can put the names
# back on. The light check is `start` declining to claim a roster it could
# not read.
run "$CYC" start --plain
check "start rebuilds the session"        '^✓ workspace ready · 2 agents$'

wait_for "cyclopsd to re-attach" 60 daemon_attached
run "$CYC" start --plain
check "and the second run names the panes" '^✔ workspace ready · 2 agents$'

wait_for "the roster to come back" 60 roster_has reviewer
# tmux titles a fresh pane with the hostname. Clear both so the roster
# shows what an agent publishes and nothing else.
read -r N1 N2 <<<"$(tmx list-panes -t main -F '#{pane_id}' | tr '\n' ' ')"
tmx select-pane -t "$N1" -T ''
tmx select-pane -t "$N2" -T ''
sleep 2.5

run "$CYC" list --plain
check "with both names on the new panes"  '^ +implementer +○ idle$'
check "and the second one too"            '^ +reviewer +○ idle$'

run "$CYC" start --workspace ops --session ops --preset ops --plain
check "a preset builds three agents"      '^✓ workspace ready · 3 agents$'
check "and says what the daemon needs"    'cyclopsd won.t watch "ops" until it.s listed in'

echo
echo "#### Rung 5: structured messages with receipts"

# The panes were rebuilt by the restore above, so the stand-ins go back in
# and the roster is re-read before anything is delivered to them.
tmx respawn-pane -k -t "$N1" "sh '$ROOT/agent.sh' implementer '$CYC'"
tmx respawn-pane -k -t "$N2" "sh '$ROOT/agent.sh' reviewer '$CYC'"
tmx select-pane -t "$N1" -T ''
tmx select-pane -t "$N2" -T ''
wait_for "both stand-ins to read idle" 50 all_idle
# The roster can read idle before fusion has recomputed the respawned pane,
# and the delivery gate reads fusion. tmux re-evaluates its subscriptions
# once a second (F23), so give it two ticks before delivering.
sleep 2.5

run "$CYC" send reviewer --subject "Review the rate limiter" --body "gateway.rs:120 drops the burst path" --plain
check "the receipt is a verified badge"   '^✔ delivered · verified$'

MID="$("$CYC" --json history --limit 1 | jq -r '.lines[-1].id')"
printf '\n$ tmux capture-pane -p -t %s\n' "$N2"
tmx capture-pane -p -t "$N2" | grep -v '^$' > "$OUT"
cat "$OUT"
check "the recipient reads a stamped header" "^\[cyclops $MID\] FROM: admin  SUBJECT: Review the rate limiter\$"
check "the body arrives verbatim"            '^gateway\.rs:120 drops the burst path$'
check "and the reply line names the sender"  '^Reply with: cyclops send admin --subject'

run "$CYC" thread "$MID" --plain
check "a thread carries the body"         '^ +gateway\.rs:120 drops the burst path$'

# Both halves, and a tick between them: the record has to hold no moving
# delivery AND fusion has to have caught up with the panes, or the
# broadcast receipt reads "queued" for whichever recipient is still
# finishing the last one. Legal, self-healing, and not reproducible.
wait_for "the last delivery to settle" 50 settled
wait_for "both stand-ins to read idle" 50 all_idle
sleep 2.5

run "$CYC" send --all --subject "Standup in 5" --fyi --plain
check "a broadcast receipts per recipient" '^ +implementer +(✔|✓|●) '
check "one row each"                       '^ +reviewer +(✔|✓|●) '

wait_for "the broadcast to settle" 50 settled
run "$CYC" history --plain
check "a broadcast is one line with N badges" '^ +[0-9]+s +admin → 2 agents +fyi +Standup in 5$'
check "and one badge row per recipient"       '^ +implementer +✔ delivered · verified$'

run "$CYC" wait reviewer --until idle --plain
check "wait reports the state and how long" '^○ idle · waited [0-9]+s$'
check_exit "a reached wait exits 0" 0

run "$CYC" send --subject "nobody" --plain
check "a send with no recipient says what to do" '^no recipient\. Name one'
check_exit "a usage error exits 2" 2

echo
echo "#### Rung 6: pipe, and what scripts can do today"

run "$CYC" pipe implementer reviewer
check "cyclops pipe is not built yet"     'unrecognized subcommand'
check_exit "so it exits on usage" 2

printf '\n$ cyclops --json history | jq -r \x27.lines[] | "\\(.from) -> \\(.to[0])  \\(.subject)"\x27\n'
"$CYC" --json history | jq -r '.lines[] | "\(.from) -> \(.to[0])  \(.subject)"' > "$OUT"
cat "$OUT"
check "every message is jq-able"          '^admin -> implementer  Review the rate limiter$'

printf '\n$ jq -c \x27select(.kind == "msg") | {ts, from, to, subject}\x27 ~/.cyclops/ledger/main.ndjson\n'
jq -c 'select(.kind == "msg") | {ts, from, to, subject}' "$CYCLOPS_HOME/ledger/main.ndjson" > "$OUT"
cat "$OUT"
check "the ledger is plain NDJSON"        '"kind"|"subject":"Review the rate limiter"'

run "$CYC" --json ui
check "ui points machine readers at watch" 'cyclops watch --json'
check_exit "and exits on usage" 2

run "$CYC" theme --plain
check "theme lists the shipped three"     '^▸ dark +● working'
check "with the ones not on beside it"    '^  high-contrast +● working'
check "and says how to switch"            '^  cyclops theme <name> to switch$'

echo
echo "#### The handoff (docs/QUICKSTART.md walks this)"

# Typed into the pane, not run from here, because the thing under test is
# who the daemon says sent it. Identity is resolved by walking the caller's
# process up to a watched pane; nothing in the request can claim a sender.
printf '\n$ (typed in the implementer pane) cyclops send reviewer --subject "Burst path fix, ready for review" --body "gateway.rs:120. Tests pass."\n'
tmx send-keys -t "$N1" -l '@send reviewer --subject "Burst path fix, ready for review" --body "gateway.rs:120. Tests pass."'
tmx send-keys -t "$N1" Enter
wait_for "the handoff to settle" 50 settled

run "$CYC" history --with reviewer --limit 1 --plain
check "the sender is the pane, not the caller" '^ +[0-9]+s +implementer → reviewer +Burst path fix, ready for review +✔ delivered · verified$'

HANDOFF="$("$CYC" --json history --with reviewer | jq -r '.lines[-1].id')"
printf '\n$ (typed in the reviewer pane) cyclops send implementer --reply-to %s --subject "Re: Burst path fix" --body "Approved. One nit in the retry path."\n' "$HANDOFF"
tmx send-keys -t "$N2" -l "@send implementer --reply-to $HANDOFF --subject \"Re: Burst path fix\" --body \"Approved. One nit in the retry path.\""
tmx send-keys -t "$N2" Enter
wait_for "the reply to settle" 50 settled

run "$CYC" thread "$HANDOFF" --plain
check "the thread holds the request"      '^ +[0-9]+s +implementer → reviewer +Burst path fix, ready for review'
check "and the reply under it"            '^ +[0-9]+s +reviewer → implementer +Re: Burst path fix'
check "with the review verdict"           '^ +Approved\. One nit in the retry path\.$'

printf '\n$ jq -c \x27select(.kind == "msg") | {from, to, subject, reply_to}\x27 ~/.cyclops/ledger/main.ndjson | tail -2\n'
jq -c 'select(.kind == "msg") | {from, to, subject, reply_to}' "$CYCLOPS_HOME/ledger/main.ndjson" | tail -2 > "$OUT"
cat "$OUT"
check "the record links the reply to it"  "\"reply_to\":\"$HANDOFF\""

echo
echo "#### The open eye (docs/troubleshooting.md quotes both of these)"

# Last, because it leaves an item on the record that nothing clears. A send
# to a name nobody holds is the cheapest way to raise one, and the point is
# the two surfaces agreeing: the receipt names the reason, and the eye
# opens because a delivery is now waiting on a human.
run "$CYC" send ghost --subject "Review this" --plain
check "the receipt names why it stopped" '^⚠ needs attention · no pane for "ghost"$'
check_exit "and exits 1 for a human" 1

run "$CYC" status --plain
check "the eye opens with a count"       '^◑ 1 cyclops · watching main · tmux .* · 1 needs attention$'
check "and the block names what it is"   '^  waiting on you$'
check "one row per open item"            '^  ghost +⚠ needs attention$'

echo
echo "#### The first run docs/QUICKSTART.md opens with"

# A nested rig, because the page starts from a machine with no config and
# no session called main, and this one has had both since rung 1. Its own
# home and its own tmux directory keep the two apart.
mkdir -p "$ROOT/duo/home" "$ROOT/duo/tmux"
printf '\n$ cyclops start --preset duo\n'
(
  export CYCLOPS_HOME="$ROOT/duo/home" TMUX_TMPDIR="$ROOT/duo/tmux"
  "$CYC" start --preset duo --plain
) > "$OUT" 2>&1
cat "$OUT"
check "duo opens two panes"               '^✓ workspace ready · 2 agents$'
check "and writes the config"             'wrote .*/config\.toml$'
check "step 3 names the first agent"      '^  3  cyclops send implementer --subject "hello"  +send the first message$'

echo
echo "== $((CHECKS - FAILS))/$CHECKS checks passed"
if [ "$FAILS" -ne 0 ]; then
  echo "== $FAILS shape(s) the docs claim and the binaries no longer print"
  exit 1
fi
echo "== docs and binaries agree"
