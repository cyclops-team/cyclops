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
#
# The rungs run from the repo root, where the daemon's last-resort
# `./manifests` fallback would find the manifests whether or not anything
# installed them. That is the hole the first run fell through for five
# milestones, so the last section runs the whole ladder from a directory
# with nothing in it, on its own home and its own daemon.

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

# The installer section is opt-in because it runs `cargo build --release`,
# and everything else here reuses the debug binaries that are already
# built. Off by default it costs nothing; on, it is a cold release compile.
# CI runs it as its own job so it does not sit in front of the test results.
WITH_INSTALLER=0
# The real home, kept because the installer section runs with HOME pointed
# at a throwaway and still needs to find the toolchain.
CALLER_HOME="$HOME"
case "${1:-}" in
  --with-installer) WITH_INSTALLER=1 ;;
  "") ;;
  *) echo "!! unknown option: $1 (only --with-installer)" >&2; exit 2 ;;
esac

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
  # The nested rigs run their own daemon and their own tmux server in their
  # own directories. All of them die here even when a check above exited
  # early.
  for pid in "${DUO_PID:-}" "${STOCK_PID:-}"; do
    if [ -n "$pid" ]; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  cyc_tmux_teardown default
  for nested in duo stock; do
    if [ -d "$ROOT/$nested/tmux" ]; then
      TMUX_TMPDIR="$ROOT/$nested/tmux" cyc_tmux_teardown default
    fi
  done
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

# One named message has reached the record AND every delivery it carries
# has stopped moving. Badges are read from the record, so a read taken
# mid-flight shows a legal in-flight state and the transcript stops being
# reproducible.
#
# Naming the message is the load-bearing part, and two weaker versions of
# this check have already shipped and failed. "Nothing is in flight" is
# true of an empty record, so it settled before a send typed into a pane
# had reached the daemon at all. Counting lines then settled on messages
# an earlier rung had already delivered. Both are the same mistake: absence
# of evidence read as evidence of completion.
#
# Requiring at least one delivery matters too: a message exists in the
# record before any delivery attempt is written against it.
settled() {
  local subject="$1"
  "$CYC" --json history | jq -e --arg s "$subject" '
    [.lines[] | select(.subject == $s)] as $m
    | ($m | length) > 0
    and ([$m[].deliveries[]?] | length) > 0
    and ([$m[].deliveries[].state
      | select(. == "queued" or . == "gating" or . == "pasting"
               or . == "staged" or . == "submitted" or . == "retry_queued")]
      | length == 0)' >/dev/null
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

# Assert the last `run` printed NOTHING matching an extended regex. For the
# lines a command must say once and then stop saying: a note repeated on
# every run is noise, and the reader is looking for what changed.
check_absent() {
  local what="$1" pattern="$2"
  CHECKS=$((CHECKS + 1))
  if grep -qE -- "$pattern" "$OUT"; then
    printf '   FAIL  %s\n         did not want /%s/\n' "$what" "$pattern"
    FAILS=$((FAILS + 1))
  else
    printf '   ok    %s\n' "$what"
  fi
}

# Assert a file on disk matches. The seed's rule is about files, not
# output: a run that says nothing and rewrote your manifest anyway would
# pass every check above.
check_file() {
  local what="$1" path="$2" pattern="$3"
  CHECKS=$((CHECKS + 1))
  if grep -qE -- "$pattern" "$path" 2>/dev/null; then
    printf '   ok    %s\n' "$what"
  else
    printf '   FAIL  %s\n         wanted /%s/ in %s\n' "$what" "$pattern" "$path"
    FAILS=$((FAILS + 1))
  fi
}

# The file half of check_absent: assert a file does NOT match. For the
# lines whose absence is the property under test, where a later edit that
# adds one would otherwise pass silently.
check_file_absent() {
  local what="$1" path="$2" pattern="$3"
  CHECKS=$((CHECKS + 1))
  if grep -qE -- "$pattern" "$path" 2>/dev/null; then
    printf '   FAIL  %s\n         did not want /%s/ in %s\n' "$what" "$pattern" "$path"
    FAILS=$((FAILS + 1))
  else
    printf '   ok    %s\n' "$what"
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

# The stand-in's manifest. Written AFTER the first `cyclops start`, not
# before: seeding the home with the shipped manifests is that command's job
# now, and a rig that copied them in first is exactly how the first run
# stayed broken through five milestones while this gate passed.
DEMO_MANIFEST=$(cat <<'EOF'
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
)

echo
echo "#### Rung 1: one pane, persistence, history"

run "$CYC" start --plain
check "start builds the solo workspace"   '^✓ workspace ready · 1 agent$'
check "start writes the config"           'wrote .*/config\.toml$'
check "start installs the manifests"      '^  wrote 3 detection manifests to .*/manifests$'
check "and says nothing is named yet"     '^  cyclopsd isn.t running, so nothing was named yet\.'
check "step 1 starts the daemon"          '^  1  cyclopsd & +start the daemon$'
check "step 2 puts the names on"          '^  2  cyclops start +put the workspace.s names on the panes$'
check "step 3 attaches"                   '^  3  tmux attach -t main +open the workspace and start your agents$'
# No send step here. Nothing is named until the daemon is up, so a
# `cyclops send implementer` printed at this point answers "no pane for
# implementer": the one shape a first run must never be handed.
check_absent "and offers no send yet"     'cyclops send implementer'

printf '\n$ ls ~/.cyclops/manifests\n'
ls "$CYCLOPS_HOME/manifests" > "$OUT"
cat "$OUT"
check "the shipped set is on disk"        '^claude\.toml$'

# The stand-in's own manifest, written the way docs/MANIFESTS.md says to
# write one: a file in the home directory the daemon already reads.
printf '%s\n' "$DEMO_MANIFEST" > "$CYCLOPS_HOME/manifests/demo.toml"

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
check_absent "and installs nothing twice" '^  wrote [0-9]+ detection manifest'
# The rule the seed turns on: a manifest written by hand survives every
# later start, and nothing already on disk is rewritten.
check_file "the stand-in's manifest survived" \
  "$CYCLOPS_HOME/manifests/demo.toml" '^id = "demo"$'

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
# The shell name is whatever tmux reports for the rig's own `sh`, which is
# bash on macOS and dash on Linux. This check is about the trailing marker.
check "and marks a pane no hook reported" '^ +implementer +○ idle +[a-z]+ · hooks unverified$'

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

# One run, not the two rung 1 needs. The daemon is already up here, so
# `start` waits for it to re-attach to the session it just rebuilt and the
# names go back on in the same run. The heavy check is what proves the
# wait paid off: a roster cyclopsd confirmed, not one read off a file.
run "$CYC" start --plain
check "start rebuilds the session and names it" '^✔ workspace ready · 2 agents$'

wait_for "cyclopsd to re-attach" 60 daemon_attached

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
wait_for "the last delivery to settle" 50 settled "Review the rate limiter"
wait_for "both stand-ins to read idle" 50 all_idle
sleep 2.5

run "$CYC" send --all --subject "Standup in 5" --fyi --plain
check "a broadcast receipts per recipient" '^ +implementer +(✔|✓|●) '
check "one row each"                       '^ +reviewer +(✔|✓|●) '

wait_for "the broadcast to settle" 50 settled "Standup in 5"
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
wait_for "the handoff to settle" 50 settled "Burst path fix, ready for review"

run "$CYC" history --with reviewer --limit 1 --plain
check "the sender is the pane, not the caller" '^ +[0-9]+s +implementer → reviewer +Burst path fix, ready for review +✔ delivered · verified$'

HANDOFF="$("$CYC" --json history --with reviewer | jq -r '.lines[-1].id')"
printf '\n$ (typed in the reviewer pane) cyclops send implementer --reply-to %s --subject "Re: Burst path fix" --body "Approved. One nit in the retry path."\n' "$HANDOFF"
tmx send-keys -t "$N2" -l "@send implementer --reply-to $HANDOFF --subject \"Re: Burst path fix\" --body \"Approved. One nit in the retry path.\""
tmx send-keys -t "$N2" Enter
wait_for "the reply to settle" 50 settled "Re: Burst path fix"

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
echo "#### The first run docs/QUICKSTART.md walks, from outside the repo"

# A nested rig, and the one that regresses the whole first-run break.
#
# Everything above runs from the repo root, where the daemon's last-resort
# `./manifests` fallback finds the manifests whether or not anything
# installed them. An installed pair of binaries has no repo to stand in, so
# this rig runs from a directory with nothing in it, with its own home and
# its own tmux directory. It is the walk a stranger takes: start, daemon,
# status, teach a manifest, name, send, read the receipt.
mkdir -p "$ROOT/duo/home" "$ROOT/duo/tmux" "$ROOT/duo/elsewhere"
DUO_HOME="$ROOT/duo/home"
duo() { ( cd "$ROOT/duo/elsewhere" && CYCLOPS_HOME="$DUO_HOME" TMUX_TMPDIR="$ROOT/duo/tmux" "$@" ); }
duo_tmx() { duo tmux -u "$@"; }
duo_daemon_up() { duo "$CYC" --json status >/dev/null 2>&1; }
duo_attached() { duo "$CYC" --json status | jq -e '.sessions[0].attached == true' >/dev/null; }
duo_roster_has() { duo "$CYC" --json list | jq -e --arg a "$1" '[.agents[].agent] | index($a)' >/dev/null; }

# `exec env` so $! is cyclopsd's own pid. Without it the subshell is what
# gets killed on restart, the daemon under it lives on holding the socket,
# and the replacement exits on "another cyclopsd is already running" while
# the rig happily waits for a socket that was never the new one's.
start_duo_daemon() {
  ( cd "$ROOT/duo/elsewhere" && exec env CYCLOPS_HOME="$DUO_HOME" \
      TMUX_TMPDIR="$ROOT/duo/tmux" "$CYCD" ) >>"$ROOT/duo/daemon.log" 2>&1 &
  DUO_PID=$!
  wait_for "the second daemon's socket" 50 test -S "$DUO_HOME/sock"
  wait_for "the second daemon to answer" 50 duo_daemon_up
}
stop_duo_daemon() {
  [ -n "${DUO_PID:-}" ] || return 0
  kill "$DUO_PID" 2>/dev/null || true
  wait "$DUO_PID" 2>/dev/null || true
  DUO_PID=""
}

# The order scripts/install.sh and the README hand out: set the home up,
# start the daemon, then open the workspace. Started the other way round
# the first `start` has no daemon to register a name with, and naming
# takes a second run (rung 1 covers that path).
printf '\n$ cd ~/scratch && cyclops start --setup-only\n'
duo "$CYC" start --setup-only --plain > "$OUT" 2>&1
cat "$OUT"
check "setup writes the config"           'wrote .*/config\.toml$'
check "and installs the manifests"        '^  wrote 3 detection manifests to .*/manifests$'
check_absent "and opens nothing"          'workspace ready'

# The daemon runs from the same empty directory. Nothing in the config
# names a manifest directory, so it has to find the one setup just wrote.
start_duo_daemon

printf '\n$ cyclopsd &\n$ cyclops start --preset duo\n'
duo "$CYC" start --preset duo --plain > "$OUT" 2>&1
cat "$OUT"
# The heavy check: one run, with the daemon confirming every name. This is
# the whole point of the order, and the glyph is what proves it happened.
check "duo opens two panes"               '^✔ workspace ready · 2 agents$'
check "step 1 attaches"                   '^  1  tmux attach -t main +open the workspace and start your agents$'
check "step 2 names the first agent"      '^  2  cyclops send implementer --subject "hello" +send the first message$'

wait_for "the second daemon to attach" 60 duo_attached

printf '\n$ cyclops --json status | jq -r .manifests.ids[]\n'
duo "$CYC" --json status | jq -r '.manifests.ids[]' > "$OUT"
cat "$OUT"
check "the daemon found the shipped set"  '^claude$'

# Two shells, and no shipped manifest binds a shell, so both panes read
# unknown. This is the state the admin hit, and the surface has to say why
# rather than only labelling it.
read -r D1 D2 <<<"$(duo_tmx list-panes -t main -F '#{pane_id}' | tr '\n' ' ')"
duo_tmx respawn-pane -k -t "$D1" "sh '$ROOT/agent.sh' implementer '$CYC'"
duo_tmx respawn-pane -k -t "$D2" "sh '$ROOT/agent.sh' reviewer '$CYC'"
duo_tmx select-pane -t "$D1" -T ''
duo_tmx select-pane -t "$D2" -T ''
sleep 2.5

printf '\n$ cyclops status\n'
duo "$CYC" status --plain > "$OUT" 2>&1
cat "$OUT"
check "an unknown pane is on the grid"    '\? unknown'
check "and the grid says why"             'panes read unknown: none of agy, claude, codex matches what is running there'
check "and what to do about it"           'Pin one: cyclops name .* --manifest <id>'

# Teaching cyclops the CLI in those panes is one file in the home
# directory the daemon already reads (docs/MANIFESTS.md).
printf '\n$ $EDITOR ~/.cyclops/manifests/demo.toml\n$ (restart cyclopsd)\n'
printf '%s\n' "$DEMO_MANIFEST" > "$DUO_HOME/manifests/demo.toml"
stop_duo_daemon
cat >> "$DUO_HOME/config.toml" <<'EOF'
receipt_block_ms = 4000
ack_timeout_ms = 3000
EOF
start_duo_daemon
wait_for "the second daemon to re-attach" 60 duo_attached

printf '\n$ cyclops --json status | jq -r .manifests.ids[]\n'
duo "$CYC" --json status | jq -r '.manifests.ids[]' > "$OUT"
cat "$OUT"
check "the restarted daemon read the new one" '^demo$'

printf '\n$ cyclops start\n'
duo "$CYC" start --plain > "$OUT" 2>&1
cat "$OUT"
check "the second start names the panes"  '^✔ workspace ready · 2 agents$'
check_absent "and installs nothing twice" '^  wrote [0-9]+ detection manifest'
check_file "the hand-written manifest stayed" \
  "$DUO_HOME/manifests/demo.toml" '^id = "demo"$'

wait_for "the roster to hold implementer" 60 duo_roster_has implementer
sleep 2.5

printf '\n$ cyclops list\n'
duo "$CYC" list --plain > "$OUT" 2>&1
cat "$OUT"
check "both panes are named and idle"     '^ +implementer +○ idle$'
check "one row each"                      '^ +reviewer +○ idle$'

printf '\n$ cyclops send implementer --subject "hello"\n'
duo "$CYC" send implementer --subject "hello" --plain > "$OUT" 2>&1
cat "$OUT"
check "the first message is delivered"    '^✔ delivered · verified$'

printf '\n$ cyclops history\n'
duo "$CYC" history --plain > "$OUT" 2>&1
cat "$OUT"
check "and the record holds the receipt"  '^ +[0-9]+s +admin → implementer +hello +✔ delivered · verified$'

stop_duo_daemon

echo
echo "#### The shipped defaults, with no hooks wired"

# The leg this gate did not have, and the reason two receipt defects
# shipped under 81 passing checks.
#
# Every rung above raises two timing budgets and wires a hook reporter into
# its stand-in agent. Both are legitimate rig pacing, and together they
# mean the gate has only ever walked the configured-perfectly path: a
# delivery that earns "delivered · verified" inside a 4-second receipt
# window. A new user has none of that. Their config is whatever `cyclops
# start` wrote, their agent's hooks are not installed until they run
# `cyclops hooks install`, and their panes are whatever was already
# running in them.
#
# So this rig touches no config knob and reports no hook, and asserts the
# two shapes a first-timer actually gets. They are different failures with
# the same old symptom, which is why both are pinned here:
#
#   bound, unhooked   the manifest binds the pane and declares an ACK hook
#                     that nothing ever fires. The delivery is real and
#                     lands on screen evidence, so the receipt owes the
#                     screen-tier badge INSIDE the default receipt window.
#                     MEASURED before the fix: "● queued" at 2.507s, with
#                     the record reaching delivered_unverified at 3.03s,
#                     because the tier-1 window suppressed every screen
#                     checkpoint under it and the ladder resumed past the
#                     window. No unhooked agent could land a badge at all.
#
#   nothing binds it  the pane has a name and no manifest matches what runs
#                     there, so nothing can be typed into it. This one is
#                     not slow, it is impossible, and the receipt has to
#                     say so while the sender is still there: it reported
#                     "● queued · 0 ahead" and exited 0.
mkdir -p "$ROOT/stock/home" "$ROOT/stock/tmux" "$ROOT/stock/elsewhere"
STOCK_HOME="$ROOT/stock/home"
stock() { ( cd "$ROOT/stock/elsewhere" && CYCLOPS_HOME="$STOCK_HOME" \
    TMUX_TMPDIR="$ROOT/stock/tmux" "$@" ); }
stock_tmx() { stock tmux -u "$@"; }
stock_up() { stock "$CYC" --json status >/dev/null 2>&1; }
stock_attached() { stock "$CYC" --json status | jq -e '.sessions[0].attached == true' >/dev/null; }
stock_roster_has() { stock "$CYC" --json list | jq -e --arg a "$1" '[.agents[].agent] | index($a)' >/dev/null; }
stock_idle() { stock "$CYC" --json list | jq -e --arg a "$1" \
  '[.agents[] | select(.agent == $a and .state == "idle")] | length == 1' >/dev/null; }

# `run` prints and records the exit code but cannot carry this rig's env.
# Same contract, same $OUT and $ROOT/exit, so check and check_exit work
# here exactly as they do above.
stock_run() {
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
  stock "$@" >"$OUT" 2>&1
  local code=$?
  set -e
  cat "$OUT"
  printf '%s' "$code" > "$ROOT/exit"
}

start_stock_daemon() {
  ( cd "$ROOT/stock/elsewhere" && exec env CYCLOPS_HOME="$STOCK_HOME" \
      TMUX_TMPDIR="$ROOT/stock/tmux" "$CYCD" ) >>"$ROOT/stock/daemon.log" 2>&1 &
  STOCK_PID=$!
  wait_for "the defaults daemon's socket" 50 test -S "$STOCK_HOME/sock"
  wait_for "the defaults daemon to answer" 50 stock_up
}
stop_stock_daemon() {
  [ -n "${STOCK_PID:-}" ] || return 0
  kill "$STOCK_PID" 2>/dev/null || true
  wait "$STOCK_PID" 2>/dev/null || true
  STOCK_PID=""
}

stock_run "$CYC" start --preset duo --plain
check "duo opens two panes"               '^✓ workspace ready · 2 agents$'

# One manifest, shaped like a shipped one: it binds the panes AND declares
# an ACK hook. claude.toml and codex.toml both do, which is what makes an
# unwired agent the default case rather than an edge case.
printf '\n$ $EDITOR ~/.cyclops/manifests/demo.toml\n'
printf '%s\n' "$DEMO_MANIFEST" > "$STOCK_HOME/manifests/demo.toml"

# NOTHING is appended to the config. That is the whole point of this leg,
# and it is asserted rather than assumed: an override added here later
# would quietly turn this back into another configured-perfectly rung.
check_file_absent "this leg runs on untouched defaults" \
  "$STOCK_HOME/config.toml" 'receipt_block_ms|ack_timeout_ms'

start_stock_daemon
wait_for "the defaults daemon to attach" 60 stock_attached

read -r S1 S2 <<<"$(stock_tmx list-panes -t main -F '#{pane_id}' | tr '\n' ' ')"
# Pane 1 is the agent: bound by the manifest, and it never reports a hook,
# because a first run has not installed any. Plain `cat` is the honest
# stand-in for that: it takes the paste and echoes it, which is exactly the
# screen evidence tier 2 reads, and it reports nothing.
stock_tmx respawn-pane -k -t "$S1" "cat"
# Pane 2 is the pane nothing detects. `sleep` is in no manifest's
# process_names, which is the state every pane is in before its agent CLI
# starts.
stock_tmx respawn-pane -k -t "$S2" "sleep 600"
stock_tmx select-pane -t "$S1" -T ''
stock_tmx select-pane -t "$S2" -T ''
sleep 2.5

# Naming both panes, and the pair is the point: the same command says
# nothing extra about a pane a manifest binds, and warns about the one
# nothing binds. A warning on both would be noise nobody reads.
stock_run "$CYC" name "$S1" implementer --plain
check "the bound pane is named"           "^✔ named implementer · $S1\$"
check_absent "with no warning about it"   "can.t receive a message"

stock_run "$CYC" name "$S2" ghostpane --plain
check "and the pane nothing detects says so" 'nothing detects .* can.t receive a message'

wait_for "the roster to hold implementer" 60 stock_roster_has implementer
wait_for "implementer to read idle" 60 stock_idle implementer
sleep 2.5

# Shape 1. An agent with no hooks wired is delivered to on screen evidence,
# and the badge has to be on the receipt the sender reads, not only in the
# record half a second later.
stock_run "$CYC" send implementer --subject "hello" --plain
check "an unhooked agent gets the screen-tier badge" '^✓ delivered · unverified \(screen\)$'
check_exit "and a delivered send exits 0" 0

# Shape 2. The pane nothing binds. Not slow: impossible. The receipt names
# the pane and the exit code keeps a script from reading it as delivered.
stock_run "$CYC" send ghostpane --subject "hello" --plain
check "a pane nothing detects refuses the message" "^⚠ needs attention · nothing detects $S2\$"
# The badge names the pane and the reason; the line under it has to leave
# the reader somewhere to go, and for this one cause the way out is a
# command. It carries the pane as the target and the name the pane already
# answers to as the label, so it can be pasted whole: passing the label as
# the target would rename an adopted pane to a placeholder.
check "and names the command that fixes it" \
  "cyclops name $S2 ghostpane --manifest <id>"
check_exit "and it does NOT exit 0" 1

# The record agrees with both receipts. A receipt that says one thing while
# the ledger says another is the defect these two checks exist to catch.
stock_run "$CYC" history --plain
check "the delivered one is on the record" '^ +[0-9]+s +admin → implementer +hello +✓ delivered · unverified \(screen\)$'
# One cause, one wording. The receipt above named the pane because it had
# one; a folded record line carries the recipient and not the pane, so it
# says the same sentence without the id. Both come out of
# cyclops_ui::grid::cause_words, which is the only place this cause becomes
# English.
check "and the refused one too"            '^ +[0-9]+s +admin → ghostpane +hello +⚠ needs attention · nothing detects its pane$'

stop_stock_daemon

if [ "$WITH_INSTALLER" -eq 0 ]; then
  echo
  echo "== skipped: the installer section (./demos/parity-check.sh --with-installer)"
fi

# Run against a home of its own, so nothing here can reach the operator's
# profile, binaries, or cyclops home.
#
# What this covers is the shapes install.md and QUICKSTART.md quote. The
# defect it exists for is an installer that edits a shell profile in a way
# the docs describe wrongly, which is the one thing here that touches a
# file the operator wrote.
if [ "$WITH_INSTALLER" -eq 1 ]; then
echo
echo "#### The installer docs/install.md documents"

INST="$ROOT/inst"
mkdir -p "$INST"
printf '# an existing profile\nexport EXAMPLE=1\n' > "$INST/.zshrc"
cp "$INST/.zshrc" "$ROOT/zshrc.before"

# Run it with almost nothing in the environment, so an install that only
# works because of something this shell happens to export fails here.
#
# HOME is the throwaway, which is what keeps the operator's profile out of
# reach. The toolchain paths have to survive that: rustup resolves its
# default toolchain under $RUSTUP_HOME, which follows HOME when it is
# unset, so redirecting HOME alone leaves cargo unable to pick a version.
# The two below are the toolchain, not the operator's cyclops state.
#
# SHELL only picks which profile file name to look for; nothing here runs
# it, so /bin/zsh gives the same `.zshrc` assertions on both platforms.
run_installer() {
  local path="$1"; shift
  # The toolchain env, which is the part that has to survive `env -i`.
  # RUSTUP_HOME follows HOME when unset, so redirecting HOME alone hides
  # the toolchain; and CI pins a toolchain with RUSTUP_TOOLCHAIN rather
  # than setting a rustup default, so dropping it leaves cargo with no
  # version to choose. Both are the toolchain, not the operator's state.
  local keep=(
    "RUSTUP_HOME=${RUSTUP_HOME:-$CALLER_HOME/.rustup}"
    "CARGO_HOME=${CARGO_HOME:-$CALLER_HOME/.cargo}"
  )
  if [ -n "${RUSTUP_TOOLCHAIN:-}" ]; then
    keep+=("RUSTUP_TOOLCHAIN=$RUSTUP_TOOLCHAIN")
  fi
  env -i \
    PATH="$path" \
    HOME="$INST" \
    SHELL=/bin/zsh \
    TERM=dumb \
    NO_COLOR=1 \
    "${keep[@]}" \
    sh "$REPO/scripts/install.sh" "$@" > "$OUT" 2>&1 || true
}

printf '\n$ ./scripts/install.sh\n'
run_installer "$PATH"
# The build log is noise here; the checks are all on what it says it did.
grep -v '^ *\(Compiling\|Finished\|Downloaded\|Blocking\|Updating\|Adding\)' "$OUT" | tail -22

check "it says where each binary went"    "^  cyclops    $INST/.local/bin/cyclops$"
check "and the daemon too"                "^  cyclopsd   $INST/.local/bin/cyclopsd$"
check "and where the home is"             "^  home       $INST/.cyclops$"
check "it reports the version it built"   '^✔ cyclops [0-9]+\.[0-9]+\.[0-9]+ is installed$'
check "step 2 starts the daemon"          '^  2  cyclopsd & +start the daemon$'
check "step 3 opens the workspace"        '^  3  cyclops start +open your workspace; it prints what to do next$'
check "it names the profile it edited"    "^  three lines added to $INST/.zshrc:$"
check "and shows the block it added"      '^    # >>> cyclops >>>$'
check "and the PATH line inside it"       "^    export PATH=\"$INST/.local/bin:\\\$PATH\"$"
check "and where the backup went"         "^  the file as it was: $INST/.zshrc.cyclops-backup.[0-9]+$"

check_file "the binaries are executable"  "$INST/.local/bin/cyclops" '.'
check_file "and the home has a config"    "$INST/.cyclops/config.toml" '^sessions = '
check_file "and the shipped manifests"    "$INST/.cyclops/manifests/claude.toml" '^id = "claude"$'

# The profile has exactly one block, not one per run, and the second run
# says so instead of appending another.
printf '\n$ ./scripts/install.sh    # again\n'
run_installer "$PATH"
grep -c '>>> cyclops >>>' "$INST/.zshrc" > "$ROOT/blocks"
cat "$OUT" | grep 'already has the cyclops block' || true
check_file "a second run adds no second block" "$ROOT/blocks" '^1$'

printf '\n$ ./scripts/install.sh --uninstall\n'
run_installer "$INST/.local/bin:$PATH" --uninstall
cat "$OUT"
check "uninstall keeps the record"        "your record and config are untouched at $INST/.cyclops"

# The load-bearing one. A profile this touched has to come back byte for
# byte, or the installer is something an operator cannot safely undo.
if diff -q "$ROOT/zshrc.before" "$INST/.zshrc" >/dev/null 2>&1; then
  printf '   ok    the profile is byte-for-byte what it was\n'
else
  printf '   FAIL  the profile is byte-for-byte what it was\n'
  diff "$ROOT/zshrc.before" "$INST/.zshrc" || true
  FAILS=$((FAILS + 1))
fi
CHECKS=$((CHECKS + 1))
fi

echo
echo "== $((CHECKS - FAILS))/$CHECKS checks passed"
if [ "$FAILS" -ne 0 ]; then
  echo "== $FAILS shape(s) the docs claim and the binaries no longer print"
  exit 1
fi
echo "== docs and binaries agree"
