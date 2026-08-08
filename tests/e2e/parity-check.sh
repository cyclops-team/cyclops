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
# The rungs run from the repo root. A daemon with no `manifest_dir`
# configured and no `$CYCLOPS_HOME/manifests` yet falls back to
# `./manifests` relative to its own working directory, which let a broken
# `cyclops start` seed step pass anyway on a rig that happened to run from
# a directory holding a `manifests/` folder. That is the hole the first
# run fell through for five milestones, so the last section still runs the
# whole ladder from a directory with nothing in it, on its own home and
# its own daemon.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
# The scratch root, the tmux teardown rule and the daemon stop are
# shared, not copied.
# shellcheck source=lib/lib.sh
. "$(cd "$(dirname "$0")" && pwd)/lib/lib.sh"

ROOT="$(mktemp -d "$(cyc_scratch_root)/cyclops-parity.XXXXXX")"
export TMUX_TMPDIR="$ROOT/tmux"
mkdir -p "$TMUX_TMPDIR"
# An inherited $TMUX would redirect a socket-less tmux call at the server
# the caller is attached to, which is the one server this must never touch.
# TMUX_PANE goes with it: `cyclops list` scopes to the caller's session by
# matching that pane id, and an id inherited from the operator's own tmux
# can collide with a rig pane id and scope the roster mid-walk.
unset TMUX
unset TMUX_PANE
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

if ! cmp -s "$REPO/scripts/install.sh" "$REPO/website/static/install.sh"; then
  echo "!! website/static/install.sh must match scripts/install.sh" >&2
  exit 1
fi

# `$0` is `sh` when the hosted script is piped. Help must therefore be
# self-contained rather than trying to reread the source through `$0`.
PIPE_HELP="$(sh -s -- --help < "$REPO/website/static/install.sh")"
case "$PIPE_HELP" in
  *"--prefix DIR"*"--uninstall"*) ;;
  *) echo "!! the hosted installer's piped --help is incomplete" >&2; exit 1 ;;
esac

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

# Take over the daemon `cyclops start` spawned, so the restart and the
# teardown below work on it. Its pid comes from the daemon itself, which
# is the only thing that can say it without guessing.
adopt_daemon() {
  DAEMON_PID="$("$CYC" --json daemon status | jq -r '.pid // empty')"
  [ -n "$DAEMON_PID" ] || { echo "!! cyclops start did not leave a daemon running" >&2; exit 1; }
}

cleanup() {
  local code=$?
  # Say why, before tearing down the evidence.
  #
  # Most commands here run as `cmd > "$OUT" 2>&1` and are read by `check`
  # afterwards. Under `set -e` a command that exits nonzero takes the script
  # with it BEFORE the line that prints $OUT, so the one thing worth seeing,
  # the failing command's own message, is captured to a file and then
  # deleted with $ROOT three lines down. That produced three CI reds in a
  # row whose entire content was "exit code 1", and cost more time guessing
  # than every real defect this script has caught.
  #
  # Only on failure: a green run has already printed everything it read.
  if [ "$code" -ne 0 ]; then
    if [ -s "${OUT:-}" ]; then
      echo
      echo "== the last command's output, which set -e would otherwise discard:"
      sed 's/^/   /' "$OUT"
    fi
    # The nested rigs log their daemon to a file nobody prints. When the
    # failure is "the daemon never came up", this is the only place that
    # says what it said on the way down.
    for nested in duo stock; do
      if [ -s "$ROOT/$nested/daemon.log" ]; then
        echo
        echo "== $nested daemon.log (last 20):"
        tail -20 "$ROOT/$nested/daemon.log" | sed 's/^/   /'
      fi
    done
  fi
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
# The daemon has bound this pane to this manifest, which is what decides the
# ACK tier of anything sent to it. Not "the pane exists" and not "the roster
# knows the name": both are true while `manifest` is still unset, and a
# delivery issued in that window is classified screen-tier at send time and
# never upgrades.
# The stand-in's read loop is running in this pane.
#
# tmux returns from respawn-pane when it has FORKED, not when the new
# process is reading. A delivery issued in that gap is typed at whatever is
# still in the pane, which is the login shell: the stand-in never sees it,
# nothing acks it, and the delivery stays screen-tier permanently.
#
# MEASURED here: the pane still reports zsh for ~1.2s after respawn, and
# demo.toml lists zsh in process_names, so the pane is already bound to the
# manifest and already reads idle throughout the gap. Neither `manifest`
# nor `state` can distinguish it. The stand-in writing its own flag is the
# only unambiguous signal, and unlike a sleep it is not a guess about how
# slow the machine is.
standin_reading() { [ -f "$ROOT/ready.$1" ]; }
pane_bound_to() {
  "$CYC" --json status | jq -e --arg p "$1" --arg m "$2" \
    '[.sessions[].panes[] | select(.pane_id == $p and .manifest == $m)] | length > 0' >/dev/null
}
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

# Stronger than settled: every delivery for the subject is the heavy check.
# Needed when the live receipt legally prints ● submitted under load
# (docs/guides/send.md) while the durable badge is still becoming
# delivered_verified — restarting the daemon in that window marks the
# delivery needs-attention and poisons every later eye check.
delivery_verified() {
  local subject="$1"
  "$CYC" --json history | jq -e --arg s "$subject" '
    [.lines[] | select(.subject == $s) | .deliveries[]?] as $d
    | ($d | length) > 0
    and ([$d[].state] | all(. == "delivered_verified"))' >/dev/null
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
# Announce the read loop BEFORE entering it. tmux returns from respawn-pane
# when it has forked, not when the new process is reading, and the rig has
# no other way to tell those apart: demo.toml binds the login shell too.
: > "$3/ready.$label"
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
launch = "cat"

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

# Setup first, so the tuning below is in the config before the daemon
# that `cyclops start` spawns reads it. This is what install.sh runs, so
# the rig takes the same two steps a person installing does.
run "$CYC" start --setup-only --plain
check "setup writes the config"           'wrote .*/config\.toml$'
check "setup installs the themes"         '^  wrote 17 themes to .*/themes$'
check "setup installs the manifests"      '^  wrote 4 detection manifests to .*/manifests$'

# Two budgets raised above their defaults, for the rig and not for cyclops.
# The stand-in answers a delivery by spawning jq and `cyclops hook`, which
# costs far more than a vendor hook already loaded in the agent's process,
# so at the shipped defaults the receipt returns "queued" and the ack lands
# after the window. Raising both makes the transcript reproducible instead
# of racing this machine.
#
# receipt_block_ms stays under the CLI's own 5-second socket read deadline
# (src/cyclops/src/client.rs, READ_TIMEOUT). Past it the daemon is still
# holding the receipt when the client gives up, and a delivery that is
# going fine reports a lost connection.
cat >> "$CYCLOPS_HOME/config.toml" <<'EOF'
receipt_block_ms = 4800
ack_timeout_ms = 4500
EOF

# The stand-in's own manifest, written the way docs/reference/MANIFESTS.md says to
# write one: a file in the home directory the daemon reads at boot. It
# goes in before the daemon starts, which is the order a person teaching
# cyclops a new CLI takes too.
printf '%s\n' "$DEMO_MANIFEST" > "$CYCLOPS_HOME/manifests/demo.toml"

# The first run a person makes, with nothing running. One command: it
# builds the session, starts a daemon, waits for it to reach the session,
# and puts the workspace's names on the panes.
run "$CYC" start --plain
check "start builds the solo workspace"   '^✔ workspace ready · 1 agent$'
check "and starts a daemon"               '^  started cyclopsd, logging to .*/cyclopsd\.log$'
check "step 1 attaches"                   '^  1  tmux attach -t main +open the workspace and start your agents$'
check "step 2 sends the first message"    '^  2  cyclops send implementer --subject "hello" +send the first message$'
# The heavy check is the load-bearing one. It means cyclopsd confirmed the
# roster in this run, which is what starting the daemon here buys: before
# it, the first run named nothing and a second was needed (F33).
check_absent "and needs no second run"    'nothing was named yet'
check_absent "and no daemon step"         'cyclopsd &'

# Everything below restarts and stops this daemon, so the rig adopts the
# one `cyclops start` made rather than starting a second.
adopt_daemon

printf '\n$ ls ~/.cyclops/manifests\n'
ls "$CYCLOPS_HOME/manifests" > "$OUT"
cat "$OUT"
check "the shipped set is on disk"        '^claude\.toml$'

P1="$(tmx list-panes -t main -F '#{pane_id}')"
# A person attaches here and starts an agent. The rig starts its stand-in,
# and clears the pane title tmux seeded with the hostname so the roster
# below shows what an agent publishes rather than what tmux did.
rm -f "$ROOT/ready.implementer"
tmx respawn-pane -k -t "$P1" "sh '$ROOT/agent.sh' implementer '$CYC' '$ROOT'"
wait_for "the implementer stand-in to be reading" 100 standin_reading implementer
tmx select-pane -t "$P1" -T ''
# Let the daemon see the new occupant before anything is delivered to it.
#
# It was already running when the pane changed hands, so it learns the new
# occupant from a tmux subscription, and those tick at 1Hz (F23). Deliver
# before that lands and the delivery is classified screen-tier AT SEND TIME
# and never upgrades: the receipt is `✓ delivered · unverified (screen)`
# instead of the heavy check, permanently. Nothing the wait below the send
# does can rescue it, which is why raising THAT wait's budget changed
# nothing on either platform.
#
# This was `sleep 4`, from "at 0s the receipt is screen-tier every run; at
# 4s it is hook-verified every run". True on the machine that measured it,
# and the reason this rung has failed on loaded CI runners on both
# platforms. The two conditions it was standing in for are both observable,
# so they are waited for instead: the stand-in is reading (above, and it is
# the slower of the two), and the daemon has bound the pane.
wait_for "the daemon to bind the stand-in" 100 pane_bound_to "$P1" demo
# One subscription period, so the daemon has ticked at least once since the
# occupant changed.
#
# This is the last constant here and it is the only one tied to a property
# of the system rather than to machine speed: the subscription runs at 1Hz
# (F23), so two seconds is two ticks. It cannot be waited for instead. The
# obvious observable, current_command changing, is not one: the incoming
# process is the platform's /bin/sh, and on a runner whose login shell is
# already bash the before and after strings are identical, so "it changed"
# never becomes true. That predicate is what turned this rung red on macos
# while ubuntu passed.
#
# What used to be here was `sleep 4` covering this AND the stand-in's
# startup at once. Startup is the part that scaled with load and it is now
# waited for above, which is the half that was failing.
sleep 2

run "$CYC" start --plain
check "a second start is one line"        '^✔ workspace ready · 1 agent$'
check_absent "and installs nothing twice" '^  wrote [0-9]+ detection manifest'
check_absent "and starts no second daemon" 'started cyclopsd'
# The rule the seed turns on: a manifest written by hand survives every
# later start, and nothing already on disk is rewritten.
check_file "the stand-in's manifest survived" \
  "$CYCLOPS_HOME/manifests/demo.toml" '^id = "demo"$'

wait_for "the roster to hold implementer" 50 roster_has implementer

run "$CYC" send implementer --subject "Review the rate limiter" --body "gateway.rs:120 drops the burst path" --plain
# The stand-in pays for two process spawns (jq + cyclops hook) per ack.
# On a loaded Ubuntu runner that can finish just after receipt_block_ms,
# so the live receipt prints ● submitted while the delivery is still
# becoming verified. Wait for the durable badge before any check that
# needs it — and before the daemon-restart rung below, which would mark a
# still-submitted delivery as needs-attention and open the eye.
wait_for "the first delivery to verify" 50 delivery_verified "Review the rate limiter"
if grep -qE '^✔ delivered · verified$' "$OUT"; then
  check "the receipt is a verified badge" '^✔ delivered · verified$'
else
  # Mid-confirmation receipt; history now holds the heavy check.
  CHECKS=$((CHECKS + 1))
  if delivery_verified "Review the rate limiter"; then
    printf '   ok    the receipt is a verified badge\n'
  else
    printf '   FAIL  the receipt is a verified badge\n         wanted /^✔ delivered · verified$/ (or settled delivered_verified)\n'
    FAILS=$((FAILS + 1))
  fi
fi
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
check "and the header says whose roster"  '^watching main · home .*/home$'

run "$CYC" ping --plain
check "ping reports the round trip"       '^✔ cyclops is up · [0-9.]+ms$'

echo
echo "#### Rung 2: name panes"

rm -f "$ROOT/ready.reviewer"
tmx split-window -d -t main "sh '$ROOT/agent.sh' reviewer '$CYC' '$ROOT'"
wait_for "the reviewer stand-in to be reading" 100 standin_reading reviewer
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

run "$CYC" read reviewer --source detection --raw --plain
check "--raw keeps the verdict"           '^reviewer · ○ idle · decided by title_idle$'
check "and adds the capture it read"      '^what the sensors read \(%[0-9]+\):$'

run "$CYC" read reviewer --raw --plain
check "--raw without detection is a usage error" 'pairs with --source detection'
check_exit "and exits 2" 2

run "$CYC" name "$P2" reviewer --manifest cluade --plain
check "an unknown manifest lists the known ones" '^no manifest "cluade"; loaded: agy, claude, codex, cursor, demo$'

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
run "$CYC" start --no-daemon --plain
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

run "$CYC" start --workspace ops --session ops --preset ops --no-daemon --plain
check "a preset builds three agents"      '^✓ workspace ready · 3 agents$'
check "and says what the daemon needs"    'cyclopsd won.t watch "ops" until it.s listed in'

# The blank a shipped preset leaves on purpose: which CLI runs in which
# pane. It is named per run, against the manifests this home holds, and the
# only CLI on this rig is the stand-in (launch = "cat" above). A vendor CLI
# is never started here: the gate has to pass on a machine with none
# installed.
run "$CYC" start --workspace fleet --session fleet --preset duo --agents demo,demo --no-daemon --plain
check "--agents opens two named panes"    '^✓ workspace ready · 2 agents$'
check_file "and writes the fleet into the workspace" \
  "$CYCLOPS_HOME/workspaces/fleet.toml" '^command = "cat"$'

fleet_running() { [ "$(tmx list-panes -t '=fleet' -F '#{pane_current_command}' | sort -u)" = "cat" ]; }
wait_for "the fleet to be running" 25 fleet_running
printf '\n$ tmux list-panes -t fleet -F "#{pane_current_command}"\n'
tmx list-panes -t '=fleet' -F '#{pane_current_command}' > "$OUT"
cat "$OUT"
check "both panes run what --agents named" '^cat$'
check_absent "and neither fell back to a shell" '^[a-z]*sh$'

# The other half of the rule: naming CLIs runs them now, and a command
# written into a workspace still needs --launch on every later run.
printf '\n$ tmux kill-session -t fleet\n'
tmx kill-session -t '=fleet'
run "$CYC" start --workspace fleet --session fleet --no-daemon --plain
check "the workspace comes back"          '^✓ workspace ready · 2 agents$'
printf '\n$ tmux list-panes -t fleet -F "#{pane_current_command}"\n'
tmx list-panes -t '=fleet' -F '#{pane_current_command}' > "$OUT"
cat "$OUT"
check_absent "with empty panes, not the fleet" '^cat$'

run "$CYC" start --workspace fleet2 --session fleet2 --preset duo --agents demo --no-daemon --plain
check "a fleet that does not fit is refused" 'preset duo has 2 named panes \(implementer, reviewer\)'
check "and names the arrangement that fits"  '\-\-preset solo, which has 1'
check_exit "and exits 2" 2

echo
echo "#### Rung 5: structured messages with receipts"

# The panes were rebuilt by the restore above, so the stand-ins go back in
# and the roster is re-read before anything is delivered to them.
rm -f "$ROOT/ready.implementer"
tmx respawn-pane -k -t "$N1" "sh '$ROOT/agent.sh' implementer '$CYC' '$ROOT'"
wait_for "the implementer stand-in to be reading" 100 standin_reading implementer
rm -f "$ROOT/ready.reviewer"
tmx respawn-pane -k -t "$N2" "sh '$ROOT/agent.sh' reviewer '$CYC' '$ROOT'"
wait_for "the reviewer stand-in to be reading" 100 standin_reading reviewer
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
check "and the reply line names the sender"  '^Reply: cyclops send admin --subject'

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
check "theme lists the shipped default"   '^▸ dark +● working'
check "and the seeded palettes"           '^  catppuccin +● working'
check "all seven of them"                 '^  tokyo-night +● working'
check "with the ones not on beside it"    '^  high-contrast +● working'
check "and says how to switch"            '^  cyclops theme <name> to switch$'

echo
echo "#### The handoff (docs/guides/QUICKSTART.md walks this)"

# Typed into the pane, not run from here, because the thing under test is
# who the daemon says sent it. Identity is resolved by walking the caller's
# process up to a watched pane; nothing in the request can claim a sender.
printf '\n$ (typed in the implementer pane) cyclops send reviewer --subject "Burst path fix, ready for review" --body "gateway.rs:120. Tests pass."\n'
tmx send-keys -t "$N1" -l '@send reviewer --subject "Burst path fix, ready for review" --body "gateway.rs:120. Tests pass."'
tmx send-keys -t "$N1" Enter
# `delivery_verified`, not `settled`: the check below asserts the heavy
# check, and `settled` returns the moment nothing is in flight, which
# includes `delivered_unverified`. Screen tier is a legal resting state, so
# waiting for "stopped moving" and then demanding "hook verified" is a race
# the script runs against itself. It loses on a loaded runner, twice
# observed: once here reading the light check, and once downstream, exactly
# as `delivery_verified`'s own comment warns, because the un-upgraded
# delivery poisons the later eye count.
wait_for "the handoff to verify" 100 delivery_verified "Burst path fix, ready for review"

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
echo "#### The blocking gate docs/guides/QUICKSTART.md section 6 hands to scripts"

# That section tells scripts to gate on `.wait[0].outcome`, and explains
# why the exit code is not enough. Both halves are checked here, because
# the explanation is the load-bearing part: a reader who trusts `set -e`
# alone ships a gate that passes when no review happened.

# The turn has to be driven, and the order is the whole difficulty.
#
# Two rules constrain it, and between them the stand-in's own answer is
# unusable. `done` deliberately refuses a working phase that predates the
# delivery (src/cyclopsd/tests/m1_fixes.rs, fix A), so the turn cannot
# be started before the send. And a working phase shorter than tmux's 1Hz
# subscription tick is invisible (F23), so the stand-in's instant
# hook-fired turn leaves no edge to return on.
#
# So: wait for the delivery to resolve, then run a turn long enough to
# see, then end it. That is a real agent's shape, slowed down.
( wait_for "the gate delivery to resolve" 100 settled "Review the burst path fix"
  tmx select-pane -t "$N2" -T 'Reviewing the burst path'
  sleep 2.5
  tmx select-pane -t "$N2" -T '' ) &
EDGE_DRIVER=$!

printf '\n$ cyclops send reviewer --subject "Review the burst path fix" --wait done --timeout 30s --json\n'
"$CYC" send reviewer --subject "Review the burst path fix" \
  --wait done --timeout 30s --json > "$ROOT/receipt.json" 2>&1
GATE_EXIT=$?
wait "$EDGE_DRIVER" 2>/dev/null || true
jq -c '.wait[0]' "$ROOT/receipt.json" > "$OUT"
cat "$OUT"
check "there is one wait entry per recipient" '"to":"reviewer"'
# The five fields the section names. A gate that branches on `outcome`
# breaks the moment one of them is renamed, and renaming one is easy.
for field in to outcome state waited_ms delivery; do
  check "the wait entry carries $field"      "\"$field\":"
done

# The exact predicate the documented script runs. Not a paraphrase of it:
# this is the line a reader copies.
if jq -e '.wait[0].outcome == "reached"' "$ROOT/receipt.json" >/dev/null; then
  printf '   ok    the documented jq gate passes on a finished turn\n'
else
  printf '   FAIL  the documented jq gate passes on a finished turn\n'
  FAILS=$((FAILS + 1))
fi
CHECKS=$((CHECKS + 1))
[ "$GATE_EXIT" -eq 0 ] || { printf '   FAIL  a reached wait exits 0 (got %s)\n' "$GATE_EXIT"; FAILS=$((FAILS + 1)); }
CHECKS=$((CHECKS + 1))

# The other half, and the reason that jq line exists. A recipient that
# takes the message and never finishes a turn: `cat` is in the stand-in
# manifest's process_names, so the pane stays detected and the delivery
# still lands, but nothing fires a hook and no turn ever ends.
printf '\n$ cyclops send reviewer --subject "Never answered" --wait done --timeout 3s --json    # reviewer is not answering\n'
tmx respawn-pane -k -t "$N2" "cat"
tmx select-pane -t "$N2" -T ''
sleep 2.5
"$CYC" send reviewer --subject "Never answered" \
  --wait done --timeout 3s --json > "$ROOT/timeout.json" 2>&1
STALL_EXIT=$?
jq -c '{exit: '"$STALL_EXIT"', outcome: .wait[0].outcome, delivered: .deliveries[0].state}' \
  "$ROOT/timeout.json" > "$OUT"
cat "$OUT"
check "a wait that runs out says timeout"  '"outcome":"timeout"'
# The claim the whole section is built on. If this ever exits non-zero the
# warning in the docs is wrong, and if it ever stops being 0 silently, a
# documented gate starts passing on an unreviewed change.
[ "$STALL_EXIT" -eq 0 ] \
  && printf '   ok    but the delivery landed, so it still exits 0\n' \
  || { printf '   FAIL  but the delivery landed, so it still exits 0 (got %s)\n' "$STALL_EXIT"; FAILS=$((FAILS + 1)); }
CHECKS=$((CHECKS + 1))

if jq -e '.wait[0].outcome == "reached"' "$ROOT/timeout.json" >/dev/null 2>&1; then
  printf '   FAIL  and the documented jq gate stops it\n'
  FAILS=$((FAILS + 1))
else
  printf '   ok    and the documented jq gate stops it\n'
fi
CHECKS=$((CHECKS + 1))

echo
echo "#### The open eye (docs/guides/troubleshooting.md quotes both of these)"

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
check "one row per open item"            '^  ghost +⚠ needs attention · no pane with that name · m-[[:xdigit:]]+ · (just now|[0-9]+(s|m|h|d)( [0-9]+(m|h))? ago)$'

echo
echo "#### The first run docs/guides/QUICKSTART.md walks, from outside the repo"

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
check "and installs the manifests"        '^  wrote 4 detection manifests to .*/manifests$'
check_absent "and opens nothing"          'workspace ready'

# The daemon runs from the same empty directory. Nothing in the config
# names a manifest directory, so it has to find the one setup just wrote.
start_duo_daemon

printf '\n$ cyclopsd &\n$ cyclops start --preset duo\n'
duo "$CYC" start --preset duo --no-daemon --plain > "$OUT" 2>&1
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
rm -f "$ROOT/ready.implementer"
duo_tmx respawn-pane -k -t "$D1" "sh '$ROOT/agent.sh' implementer '$CYC' '$ROOT'"
wait_for "the implementer stand-in to be reading" 100 standin_reading implementer
rm -f "$ROOT/ready.reviewer"
duo_tmx respawn-pane -k -t "$D2" "sh '$ROOT/agent.sh' reviewer '$CYC' '$ROOT'"
wait_for "the reviewer stand-in to be reading" 100 standin_reading reviewer
duo_tmx select-pane -t "$D1" -T ''
duo_tmx select-pane -t "$D2" -T ''
sleep 2.5

printf '\n$ cyclops status\n'
duo "$CYC" status --plain > "$OUT" 2>&1
cat "$OUT"
check "an unknown pane is on the grid"    '\? unknown'
check "and the grid says why"             'panes read unknown: none of agy, claude, codex, cursor matches what is running there'
check "and what to do about it"           'Pin one: cyclops name .* --manifest <id>'

# Teaching cyclops the CLI in those panes is one file in the home
# directory the daemon already reads (docs/reference/MANIFESTS.md).
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
duo "$CYC" start --no-daemon --plain > "$OUT" 2>&1
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
# The fix for the invisible second daemon: this rig IS the second daemon
# on a second home, so its roster must open by naming that home and not
# the main rig's.
check "the header names the second home"  '^watching main · home .*/duo/home$'

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
# delivery that earns "delivered · verified" inside a raised receipt
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

stock_run "$CYC" start --preset duo --no-daemon --plain
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
  echo "== skipped: the installer section (./tests/e2e/parity-check.sh --with-installer)"
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
echo "#### The installer docs/guides/install.md documents"

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
# The toolchain env, which is the part that has to survive `env -i`.
# RUSTUP_HOME follows HOME when unset, so redirecting HOME alone hides
# the toolchain; and CI pins a toolchain with RUSTUP_TOOLCHAIN rather
# than setting a rustup default, so dropping it leaves cargo with no
# version to choose. Both are the toolchain, not the operator's state.
# One list, because the installer runs and the update leg below strip
# the environment the same way.
TOOLCHAIN_KEEP=(
  "RUSTUP_HOME=${RUSTUP_HOME:-$CALLER_HOME/.rustup}"
  "CARGO_HOME=${CARGO_HOME:-$CALLER_HOME/.cargo}"
)
if [ -n "${RUSTUP_TOOLCHAIN:-}" ]; then
  TOOLCHAIN_KEEP+=("RUSTUP_TOOLCHAIN=$RUSTUP_TOOLCHAIN")
fi

run_installer() {
  local path="$1"; shift
  env -i \
    PATH="$path" \
    HOME="$INST" \
    SHELL=/bin/zsh \
    TERM=dumb \
    NO_COLOR=1 \
    "${TOOLCHAIN_KEEP[@]}" \
    sh "$REPO/scripts/install.sh" "$@" > "$OUT" 2>&1 || true
}

printf '\n$ ./scripts/install.sh\n'
run_installer "$PATH"
# The build log is noise here; the checks are all on what it says it did.
grep -v '^ *\(Compiling\|Finished\|Downloaded\|Blocking\|Updating\|Adding\)' "$OUT" | tail -22

check "it says where each binary went"    "^  cyclops    $INST/.local/bin/cyclops$"
check "and the daemon too"                "^  cyclopsd   $INST/.local/bin/cyclopsd$"
check "and where the home is"             "^  home       $INST/.cyclops$"
check "it reports the version it built"   '^✔ cyclops [0-9]+\.[0-9]+\.[0-9]+ \(([0-9a-f]+(\.dirty)?|unknown)\) is installed$'
check_absent "it gives no separate daemon step" 'cyclopsd &'
check "step 2 opens the workspace"        '^  2  cyclops +open your workspace and start your agents$'
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

echo
echo "#### The update docs/guides/install.md documents"

# `cyclops update` clones its source and reruns the installer, so this
# leg needs a source one commit past the installed build. A throwaway
# mirror of this repo gets exactly that: one empty commit on a named
# branch. Built with init+fetch rather than clone, because a CI checkout
# can be a detached HEAD with no named branch to clone from.
#
# The mirror is the last COMMIT, not the working tree: fetch is the one
# capture that cannot write into this repo. So this leg tests `cyclops
# update` as last committed, and a change to the verb that exists only
# as uncommitted work fails the second run below until it lands. CI runs
# on the commit, where the two are the same tree.
# --depth=1 is not an optimization: a CI checkout is a shallow clone, and
# a full fetch from one is refused ("shallow roots are not allowed to be
# updated"), which is why this passed on a developer's full clone and
# failed on both CI runners. A shallow fetch works against either.
git init -q "$ROOT/remote" 2>/dev/null
git -C "$ROOT/remote" fetch -q --depth=1 "$REPO" HEAD
git -C "$ROOT/remote" checkout -q -B parity-update FETCH_HEAD
git -C "$ROOT/remote" -c user.name=parity -c user.email=parity@invalid \
  commit -q --allow-empty -m "parity: one commit past the installed build"

# The same env -i discipline as run_installer, plus three overrides:
#
#   CYCLOPS_REPO      the mirror above: git clones a local path, and the
#                     network is never touched
#   CYCLOPS_REF       the mirror's one branch
#   CARGO_TARGET_DIR  the build cache the install above just filled. env -i
#                     strips it, the clone sits in a different directory,
#                     and without threading it through the clone's release
#                     build starts cold and this job's time doubles.
run_update() {
  set +e
  env -i \
    PATH="$INST/.local/bin:$PATH" \
    HOME="$INST" \
    SHELL=/bin/zsh \
    TERM=dumb \
    NO_COLOR=1 \
    CYCLOPS_REPO="$ROOT/remote" \
    CYCLOPS_REF=parity-update \
    CARGO_TARGET_DIR="$REPO/target" \
    "${TOOLCHAIN_KEEP[@]}" \
    "$INST/.local/bin/cyclops" update > "$OUT" 2>&1
  printf '%s' "$?" > "$ROOT/exit"
  set -e
}

printf '\n$ cyclops update\n'
run_update
grep -v '^ *\(Compiling\|Finished\|Downloaded\|Blocking\|Updating\|Adding\)' "$OUT" | tail -24

check "update names the running build"    '^cyclops [0-9]+\.[0-9]+\.[0-9]+ \(([0-9a-f]+(\.dirty)?|unknown)\)$'
check "and its source"                    "^  source $ROOT/remote at parity-update$"
check "it reran the installer"            '^✔ cyclops [0-9]+\.[0-9]+\.[0-9]+ \([0-9a-f]+\) is installed$'
check "and reports old build to new"      '^✔ updated · [0-9]+\.[0-9]+\.[0-9]+ \(([0-9a-f]+(\.dirty)?|unknown)\) → [0-9]+\.[0-9]+\.[0-9]+ \([0-9a-f]+\)$'
check "then the restart steps"            '^Restart:$'
check "the workspace goes first"          '^  1  q +quit any open workspace; it is still on the old build$'
check "the daemon second"                 '^  2  cyclops daemon stop +the daemon is too$'
check "and start comes back up"           '^  3  cyclops start +come back up on the new build$'
check_absent "it never stops the daemon itself" 'stopped cyclopsd'
check_exit "an update exits 0" 0

# A second update against the same ref: the binary just installed IS the
# mirror's commit, so the freshness check answers and nothing rebuilds.
printf '\n$ cyclops update    # again\n'
run_update
cat "$OUT"
check "a repeat update says already current" '^✔ already the latest parity-update · nothing to update$'
check_exit "and exits 0 saying so" 0

# What an update must never cost: the home the first install set up. The
# installer's seed rule (files already there are never rewritten) is what
# this rides on, and this is where it is proven from the update side.
check_file "the config survived the update" "$INST/.cyclops/config.toml" '^sessions = '

# The update legs built the mirror's clone into the repo's own target dir
# (the shared cache that keeps this job fast), which leaves the MIRROR's
# binaries at target/release/cyclops{,d} while cargo still counts the
# repo's own bin units fresh: a later build from this checkout would
# silently reinstall the mirror's build. Deleting the binaries
# un-freshens exactly the final link step; the dependency cache stays
# warm.
rm -f "$REPO/target/release/cyclops" "$REPO/target/release/cyclopsd"

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
