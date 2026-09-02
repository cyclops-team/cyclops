#!/usr/bin/env bash
# Exercise the stable CLI shapes and state transitions used by the primary
# documentation against binaries built from this checkout.
#
# A doc describing output the binaries no longer print is a bug. This gate
# walks the representative README setup and messaging flows on a throwaway
# rig, prints each command and output, and asserts the documented shapes.
# It does not claim to execute every command shown anywhere in docs/.
#
# The transcript is diagnostic evidence for the assertions below. Documentation
# examples remain prose and must not treat this output as generated source.
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
COMPOSER_PATH="$REPO/src/cyclopsd/tests/common/faketui.py"
OUT="$ROOT/out"
DAEMON_PID=""
INST_DAEMON_PID=""
CHECKS=0
FAILS=0
# The install and update transcripts carry the package version. Accept the
# SemVer prerelease form Cargo exposes, not only a final X.Y.Z version.
PACKAGE_VERSION_RE='[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?'

# The installer section is opt-in because it runs the installer's dist
# build, and everything else here reuses the debug binaries that are
# already built. Off by default it costs nothing; on, it is a cold compile.
# CI runs it as its own job so it does not sit in front of the test results.
WITH_INSTALLER=0
INSTALLER_ONLY=0
# The real home, kept because the installer section runs with HOME pointed
# at a throwaway and still needs to find the toolchain.
CALLER_HOME="$HOME"
case "${1:-}" in
  --with-installer) WITH_INSTALLER=1 ;;
  --installer-only) WITH_INSTALLER=1; INSTALLER_ONLY=1 ;;
  "") ;;
  *) echo "!! unknown option: $1 (only --with-installer, --installer-only)" >&2; exit 2 ;;
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
    # The main rig starts through `cyclops start`, then later restarts under
    # this script. Print both destinations because the failure can belong to
    # either daemon generation.
    for log in "${CYCLOPS_HOME:-}/cyclopsd.log" "${ROOT:-}/daemon.log"; do
      if [ -s "$log" ]; then
        echo
        echo "== $log (last 30):"
        tail -30 "$log" | sed 's/^/   /'
      fi
    done
    # Nested daemons write structured lifecycle events under their private
    # homes; their launch redirection is still useful for an early process
    # failure. Show both when a nested journey fails.
    for nested in duo stock installed; do
      if [ "$nested" = installed ]; then
        structured_log="${INST_HOME:-}"
        [ -n "$structured_log" ] && structured_log="$structured_log/cyclopsd.log"
      else
        structured_log="$ROOT/$nested/home/cyclopsd.log"
      fi
      for log in "$structured_log" "$ROOT/$nested/daemon.log"; do
        if [ -z "$log" ] || [ ! -s "$log" ]; then
          continue
        fi
        echo
        echo "== $log (last 20):"
        tail -20 "$log" | sed 's/^/   /'
      done
    done
    if [ -s "$ROOT/notification-state.json" ]; then
      echo
      echo "== notification facts while waiting for submit:"
      jq -c 'select(.data.type == "notification_transition")
        | {id, attempt: .data.attempt_id, state: .data.state, cause: .data.cause}' \
        "$ROOT/notification-state.json" \
        | sed 's/^/   /'
    fi
  fi
  cyc_stop_daemon
  # The nested rigs run their own daemon and their own tmux server in their
  # own directories. All of them die here even when a check above exited
  # early.
  for pid in "${DUO_PID:-}" "${STOCK_PID:-}" "${INST_DAEMON_PID:-}"; do
    if [ -n "$pid" ]; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  cyc_tmux_teardown default
  for nested in duo stock installed; do
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
# The stand-in announces its command loop. tmux returns from respawn-pane after
# forking, before the fixture process is ready to receive a command.
standin_reading() { [ -f "$ROOT/ready.$1" ]; }
pane_bound_to() {
  "$CYC" --json status | jq -e --arg p "$1" --arg m "$2" \
    '[.sessions[].panes[] | select(.pane_id == $p and .manifest == $m)] | length > 0' >/dev/null
}
pane_known() { "$CYC" --json status | jq -e --arg p "$1" '[.sessions[].panes[].pane_id] | index($p)' >/dev/null; }
roster_has() { "$CYC" --json list | jq -e --arg a "$1" '[.agents[].agent] | index($a)' >/dev/null; }
roster_empty() { "$CYC" --json list | jq -e '.agents | length == 0' >/dev/null; }

pane_command_done() { [ -f "$ROOT/done.$1" ]; }
pane_has_text() { tmx capture-pane -p -t "$1" | grep -Fq -- "$2"; }
pane_matches() { tmx capture-pane -p -t "$1" | grep -Eq -- "$2"; }

start_agent() {
  local label="$1" pane="$2" control="$ROOT/control.main.$1"
  rm -f "$control" "$ROOT/ready.$label"
  mkfifo "$control"
  tmx respawn-pane -k -t "$pane" "'$ROOT/cycagent' '$control' '$ROOT/ready.$label'"
  wait_for "the $label fixture agent" 100 standin_reading "$label"
}

# Issue a Cyclops command below the watched fixture agent. The command is
# sent through a FIFO, so a composer can own the terminal at the same time.
agent_command() {
  local label="$1" command="$3" line
  rm -f "$ROOT/result.$label" "$ROOT/exit.$label" "$ROOT/done.$label"
  printf '\n$ (run by the %s agent) cyclops %s\n' "$label" "$command"
  line="\"$CYC\" $command > \"$ROOT/result.$label\" 2>&1; code=\$?; printf '%s' \"\$code\" > \"$ROOT/exit.$label\"; : > \"$ROOT/done.$label\""
  printf 'run\t%s\n' "$line" > "$ROOT/control.main.$label"
  wait_for "the $label agent command" 100 pane_command_done "$label"
  cp "$ROOT/result.$label" "$OUT"
  cp "$ROOT/exit.$label" "$ROOT/exit"
  cat "$OUT"
}

pane_write_ready() {
  "$CYC" read "$1" --source detection --plain | grep -q 'write-ready$'
}
notification_crossed_submit() {
  local journal
  journal="$(find "$CYCLOPS_HOME/workspaces" -name messages.ndjson -print -quit)"
  [ -n "$journal" ] && cp "$journal" "$ROOT/notification-state.json" &&
  jq -e --arg id "$1" '
    select(.id == $id and .data.type == "notification_transition")
    | .data.state
    | select(. == "submitted" or . == "notified")' \
    "$ROOT/notification-state.json" >/dev/null
}
pane_is_agent() {
  [ "$(tmx display-message -p -t "$1" '#{pane_current_command}')" = "cycagent" ]
}
start_composer() {
  local label="$1" pane="$2"
  tmx resize-window -t "$pane" -x 500 -y 40
  printf 'composer\t%s\n' "$COMPOSER_PATH" > "$ROOT/control.main.$label"
  wait_for "the $label composer" 100 pane_has_text "$pane" 'Model x · Ctx: 78%'
  if ! wait_for "the $label composer to be write-ready" 100 pane_write_ready "$label"; then
    "$CYC" read "$label" --source detection --raw --plain >&2 || true
    tmx capture-pane -p -t "$pane" >&2 || true
    return 1
  fi
}
stop_composer() {
  local label="$1" pane="$2"
  tmx send-keys -t "$pane" C-c
  wait_for "the $label composer to exit" 100 pane_is_agent "$pane"
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

check_same_file() {
  local what="$1" before="$2" after="$3"
  CHECKS=$((CHECKS + 1))
  if cmp -s "$before" "$after"; then
    printf '   ok    %s\n' "$what"
  else
    printf '   FAIL  %s\n         %s changed from %s\n' "$what" "$after" "$before"
    FAILS=$((FAILS + 1))
  fi
}

check_different_file() {
  local what="$1" before="$2" after="$3" result
  CHECKS=$((CHECKS + 1))
  if cmp -s "$before" "$after"; then
    printf '   FAIL  %s\n         %s did not change from %s\n' "$what" "$after" "$before"
    FAILS=$((FAILS + 1))
  else
    result=$?
    if [ "$result" -eq 1 ]; then
      printf '   ok    %s\n' "$what"
    else
      printf '   FAIL  %s\n         could not compare %s with %s\n' "$what" "$after" "$before"
      FAILS=$((FAILS + 1))
    fi
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

# The fixture has its own executable identity. Cyclops commands and the
# composer run as its children, so the gate exercises peer ancestry instead
# of granting authority from a mutable pane label. The installer-only journey
# uses the same fixture against the binaries that were actually installed.
rustc --edition=2021 -Dwarnings "$REPO/tests/e2e/parity_agent.rs" -o "$ROOT/cycagent"

# The stand-in's manifest is written only after each isolated home has seeded
# the shipped set. Doorbell transport points at the reviewed skill bytes in
# this rig, never at an agent installation in the operator's home.
DEMO_SKILL="$ROOT/cyclops-skill.md"
cp "$REPO/skills/cyclops/SKILL.md" "$DEMO_SKILL"
DEMO_MANIFEST=$(cat <<'EOF'
[agent]
id = "demo"
display_name = "Parity rig stand-in"
process_names = ["cycagent"]
argv_basenames = ["cycagent"]
launch = "cat"

[hooks]
# This stand-in exposes one event-local start and dispatch fact, not a turn key.
turn_start = "UserPromptSubmit"
ack = "UserPromptSubmit"
ack_evidence = "dispatch"
ack_payload_field = "prompt"

[messaging]
mailbox_capability_file = "__CYCLOPS_MAILBOX_CAPABILITY__"

[[rule]]
id = "title_working"
state = "working"
priority = 1100
region = "pane_title"
regex = ['^Implementing|^Reviewing']

[[rule]]
id = "title_idle"
state = "idle"
priority = 100
region = "pane_title"
regex = ['^']

[[rule]]
id = "composer_empty"
state = "idle"
composer_semantic = "clean"
priority = 90
region = "bottom_non_empty_lines(4)"
line_regex = ['^❯\s*$']
# The parity composer is fully controlled: it paints this row only while
# idle, and its working row below outranks it during a turn.
lifecycle_evidence = true

[[rule]]
id = "screen_idle"
state = "idle"
priority = 70
region = "bottom_non_empty_lines(4)"
# Before the fixture composer starts, the persistent command loop leaves a
# blank pane. This purpose-built screen rule is that loop's idle authority.
lifecycle_evidence = true
regex = ['^']

[[rule]]
id = "composer_holds_paste"
state = "idle_with_input"
composer_semantic = "human_input"
priority = 80
region = "bottom_non_empty_lines(3)"
line_regex = ['^\s*❯\s+\S']
line_regex_esc = ['^❯']

[[rule]]
id = "composer_working"
state = "working"
priority = 300
region = "bottom_non_empty_lines(5)"
line_regex = ['^FAKETUI-WORKING$']

[injection]
method = "load-buffer + paste-buffer -p"
submit = "Enter"
verify_before_submit = true
verify_pattern = ["<message_id>"]
composer_prompt_regex = '^❯ (?P<content>.*)$'
composer_continuation_regex = '^(?P<content>.*)$'
composer_trailer_regex = ['^─+$', '^Model \S+ · Ctx: \d+%$']
composer_trailer_regex_esc = ['^\x1b\[38;5;244m─', '^\x1b\[38;5;152mModel\b']
composer_trailer_required_prefix = 2
safe_states = ["idle"]
EOF
)
DEMO_MANIFEST="${DEMO_MANIFEST/__CYCLOPS_MAILBOX_CAPABILITY__/$DEMO_SKILL}"

if [ "$INSTALLER_ONLY" -eq 0 ]; then
echo "== rig home:   $CYCLOPS_HOME (removed on exit)"
echo "== tmux:       private TMUX_TMPDIR=$TMUX_TMPDIR (removed on exit)"

# A parity gate that ran yesterday's binaries would pass while the docs are
# already wrong, which is the one failure this script exists to catch.
echo "== building cyclops and cyclopsd"
cargo build -q -p cyclops -p cyclopsd

echo
echo "#### Rung 1: one pane, persistence, history"

# Setup first. This is what install.sh runs, so the rig takes the same two
# steps as an installed workspace.
run "$CYC" start --setup-only --plain
check "setup writes the config"           'wrote .*/config\.toml$'
check "setup installs the themes"         '^  wrote 17 themes to .*/themes$'
check "setup installs the sounds"         '^  wrote 2 sounds to .*/sounds$'
check "setup installs the manifests"      '^  wrote 4 detection manifests to .*/manifests$'

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
check "step 2 sends the first message"    '^  2  cyclops send implementer --subject "hello" --summary "Hello from Cyclops\. Reply when you are ready\." +send the first message$'
# The heavy check is the load-bearing one. It means cyclopsd confirmed the
# roster in this run, which is what starting the daemon here buys: before
# it, the first run named nothing and a second was needed.
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
start_agent implementer "$P1"
tmx select-pane -t "$P1" -T ''
# Wait until the daemon binds the new occupant before testing its notification.
wait_for "the daemon to bind the stand-in" 100 pane_bound_to "$P1" demo
# Allow one subscription cycle for the new occupant's readiness stamp.
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

echo
echo "-- the roster survives a daemon restart"
cyc_stop_daemon
start_daemon
wait_for "the roster to come back" 50 roster_has implementer

run "$CYC" list --plain
check "the name is still there"           '^ +implementer +○ idle$'
check "and the header says whose roster"  '^watching main · home .*/home$'

run "$CYC" ping --plain
check "ping reports the round trip"       '^✔ cyclops is up · [0-9.]+ms$'

echo
echo "#### Rung 2: name panes"

rm -f "$ROOT/ready.reviewer"
tmx split-window -d -t main "sh"
P2="$(tmx list-panes -t main -F '#{pane_id}' | tail -1)"
start_agent reviewer "$P2"
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
check "detection names the deciding rule" '^reviewer · ○ idle · decided by title_idle · '
check "and answers write-readiness too"  '^reviewer · ○ idle · decided by title_idle · (write-ready|not write-ready: [a-z_]+)$'
check "and shows the sensor that read it" '^ +title +○ idle +title_idle'

run "$CYC" read reviewer --source detection --raw --plain
check "--raw keeps the verdict"           '^reviewer · ○ idle · decided by title_idle · '
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
check "with both names on the new panes"  '^ +implementer +\? unknown$'
check "and the second one too"            '^ +reviewer +\? unknown$'

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
echo "#### Rung 5: durable mailbox acceptance and claim"

# The rebuilt workspace starts with ordinary shells. Replace them with the
# dedicated fixture agents before asserting agent-to-agent identity.
start_agent implementer "$N1"
start_agent reviewer "$N2"
tmx select-pane -t "$N1" -T ''
tmx select-pane -t "$N2" -T ''
wait_for "the daemon to bind the implementer fixture" 100 pane_bound_to "$N1" demo
wait_for "the daemon to bind the reviewer fixture" 100 pane_bound_to "$N2" demo
run "$CYC" name "$N1" implementer --plain
run "$CYC" name "$N2" reviewer --plain
start_composer reviewer "$N2"
run "$CYC" read reviewer --source detection --plain
check "the reviewer composer is write-ready" 'decided by .* · write-ready$'

agent_command implementer "$N1" 'send reviewer --subject "Release notes review" --summary "Review the release notes. Confirm the mailbox contract." --body "Check the mailbox contract." --client-key parity-review --plain'
check "acceptance is separate from notification" '^accepted m-[[:xdigit:]]{32}$'
check "the wake is a second fact" '^✓ accepted( · [0-9]+ ahead)? · wake (not started|queued|checking readiness|writing|staged|submitted|notified|withdrawn|needs attention|superseded)$'
check_exit "mailbox acceptance exits 0" 0
REVIEW_ID="$(awk '$1 == "accepted" { print $2; exit }' "$OUT")"
wait_for "the reviewer mailbox doorbell" 100 pane_matches "$N2" '^❯ \[cyclops from implementer\] Review the release notes\. Confirm the mailbox contract\. \| cyclops inbox claim m-att_[A-Za-z0-9_-]{22}$'
printf '\n$ tmux capture-pane -p -t %s\n' "$N2"
tmx capture-pane -p -t "$N2" | grep -v '^$' > "$OUT"
cat "$OUT"
check "the recipient sees the sender, summary, and exact attempt doorbell" '^❯ \[cyclops from implementer\] Review the release notes\. Confirm the mailbox contract\. \| cyclops inbox claim m-att_[A-Za-z0-9_-]{22}$'
check_absent "the pane does not receive the body" '^Check the mailbox contract\.$'
REVIEW_LOCATOR="$(awk '/^❯ \[cyclops from implementer\] Review the release notes\. Confirm the mailbox contract\. \| cyclops inbox claim m-att_[A-Za-z0-9_-]{22}$/ { print $NF; exit }' "$OUT")"
[ -n "$REVIEW_LOCATOR" ] || { echo "!! exact attempt locator was not captured" >&2; exit 1; }
wait_for "the reviewer doorbell to be submitted" 100 notification_crossed_submit "$REVIEW_ID"

# The same durable recipient agent claims the payload over the socket.
agent_command reviewer "$N2" "inbox list --plain"
check "inbox list exposes metadata" "^$REVIEW_ID implementer · Release notes review\$"
agent_command reviewer "$N2" "inbox claim $REVIEW_LOCATOR --plain"
check "the displayed locator fetches the original envelope" "^\[cyclops $REVIEW_ID\] TO: reviewer  FROM: implementer  SUBJECT: Release notes review\$"
check "the displayed locator fetches the summary" '^Summary: Review the release notes\. Confirm the mailbox contract\.$'
check "the displayed locator fetches the body" '^Check the mailbox contract\.$'
check "the displayed locator closes the exact envelope" "^\[cyclops:end $REVIEW_ID\]\$"
agent_command reviewer "$N2" "inbox claim $REVIEW_LOCATOR --plain"
check "the same locator repeat returns the same payload" '^Check the mailbox contract\.$'
agent_command reviewer "$N2" "inbox claim $REVIEW_ID --plain"
check "plain repeat claim returns the same payload" '^Check the mailbox contract\.$'
stop_composer reviewer "$N2"

# Stopping the composer changes the foreground process after Enter. The exact
# authenticated claim settles the wake before that late receipt observation can
# turn the claimed message into an operator alarm.
agent_command implementer "$N1" '--json messages'
jq -r --arg id "$REVIEW_ID" '
  .rows[] | select(.message_id == $id) | .recipients[]
  | select(.label == "reviewer")
  | "\(.mailbox.status) \(.notification.state) \(.notification.cause // "-")"' \
  "$OUT" > "$ROOT/claimed-notification-state"
cp "$ROOT/claimed-notification-state" "$OUT"
cat "$OUT"
check "an exact claim settles the wake without an alarm" '^claimed notified -$'

run "$CYC" wait reviewer --until idle --plain
check "wait reports the state and how long" '^○ idle · waited [0-9]+s$'
check_exit "a reached wait exits 0" 0

run "$CYC" send --subject "nobody" --summary "Name a recipient. Retry the send." --plain
check "a send with no recipient says what to do" '^no recipient\. Name one'
check_exit "a usage error exits 2" 2

echo
echo "#### Rung 6: pipe, and what scripts can do today"

run "$CYC" pipe implementer reviewer
check "cyclops pipe is not built yet"     'unrecognized subcommand'
check_exit "so it exits on usage" 2

printf '\n$ cyclops --json history | jq -r \x27.lines[] | "\\(.from) -> \\(.to[0])  \\(.subject)"\x27\n'
agent_command implementer "$N1" '--json history'
jq -r '.lines[] | "\(.from) -> \(.to[0])  \(.subject)"' "$OUT" > "$ROOT/history-jq"
cp "$ROOT/history-jq" "$OUT"
cat "$OUT"
check "every message is jq-able"          '^implementer -> reviewer  Release notes review$'

agent_command implementer "$N1" '--json messages'
WORKSPACE_ID="$(jq -r '.workspace_id' "$OUT")"
MESSAGE_JOURNAL="$CYCLOPS_HOME/workspaces/$WORKSPACE_ID/messages.ndjson"
printf '\n$ cyclops --json messages | jq -r .workspace_id\n'
printf '%s\n' "$WORKSPACE_ID" > "$OUT"
cat "$OUT"
check "the workspace projection names a durable UUID" \
  '^[[:xdigit:]]{8}-[[:xdigit:]]{4}-[[:xdigit:]]{4}-[[:xdigit:]]{4}-[[:xdigit:]]{12}$'
printf '\n$ jq -c \x27select(.kind == "msg") | {ts, from, to, subject}\x27 %s\n' "$MESSAGE_JOURNAL"
jq -c 'select(.kind == "msg") | {ts, from, to, subject}' "$MESSAGE_JOURNAL" > "$OUT"
cat "$OUT"
check "the workspace journal is plain NDJSON" '"subject":"Release notes review"'
check_file_absent "standard messages are not copied to the session ledger" \
  "$CYCLOPS_HOME/ledger/main.ndjson" '"subject":"Release notes review"'

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
start_composer reviewer "$N2"
agent_command implementer "$N1" 'send reviewer --subject "Burst path fix, ready for review" --summary "Review the burst path fix. Check the passing tests." --body "gateway.rs:120. Tests pass." --client-key parity-handoff --plain'
check "the handoff is accepted from the pane" '^accepted m-[[:xdigit:]]{32}$'
check_exit "the handoff exits 0" 0
HANDOFF="$(awk '$1 == "accepted" { print $2; exit }' "$OUT")"
wait_for "the handoff doorbell" 100 pane_matches "$N2" '^❯ \[cyclops from implementer\] Review the burst path fix\. Check the passing tests\. \| cyclops inbox claim m-att_[A-Za-z0-9_-]{22}$'
wait_for "the handoff doorbell to be submitted" 100 notification_crossed_submit "$HANDOFF"

stop_composer reviewer "$N2"
agent_command reviewer "$N2" 'history --with reviewer --limit 1 --plain'
check "the sender is the pane, not the caller" '^ +[0-9]+s +implementer → reviewer +Burst path fix, ready for review$'

agent_command reviewer "$N2" "inbox claim $HANDOFF --plain"
check "the reviewer claims the handoff body" '^gateway\.rs:120\. Tests pass\.$'

start_composer implementer "$N1"
agent_command reviewer "$N2" "reply $HANDOFF --summary \"Approve the burst path fix. Note one retry issue.\" --body \"Approved. One nit in the retry path.\" --client-key parity-reply --plain"
check "reply derives routing from the parent" '^accepted m-[[:xdigit:]]{32}$'
check_exit "an accepted reply exits 0" 0
REPLY_ID="$(awk '$1 == "accepted" { print $2; exit }' "$OUT")"
wait_for "the reply doorbell" 100 pane_matches "$N1" '^❯ \[cyclops from reviewer\] Approve the burst path fix\. Note one retry issue\. \| cyclops inbox claim m-att_[A-Za-z0-9_-]{22}$'
wait_for "the reply doorbell to be submitted" 100 notification_crossed_submit "$REPLY_ID"
stop_composer implementer "$N1"
agent_command implementer "$N1" "inbox claim $REPLY_ID --plain"
check "the implementer claims the verdict" '^Approved\. One nit in the retry path\.$'

agent_command reviewer "$N2" "thread $HANDOFF --plain"
check "the thread holds the request"      '^ +[0-9]+s +implementer → reviewer +Burst path fix, ready for review'
check "and the reply under it"            '^ +[0-9]+s +reviewer → implementer +Re: Burst path fix, ready for review'
check "with the review verdict"           '^ +Approved\. One nit in the retry path\.$'

printf '\n$ jq -c \x27select(.kind == "msg") | {from, to, subject, reply_to}\x27 %s | tail -2\n' "$MESSAGE_JOURNAL"
jq -c 'select(.kind == "msg") | {from, to, subject, reply_to}' "$MESSAGE_JOURNAL" | tail -2 > "$OUT"
cat "$OUT"
check "the record links the reply to it"  "\"reply_to\":\"$HANDOFF\""

echo
echo "#### The admin mailbox"

# A pane agent can address admin, but admin has no pane and receives no wake.
agent_command implementer "$N1" 'send admin --subject "Operator review" --summary "Review the release note. Respond with an operator decision." --body "The release note is ready." --client-key parity-admin --plain'
check "admin is a durable recipient" '^accepted m-[[:xdigit:]]{32}$'
check "admin receives no pane wake" '^✓ accepted · wake not started$'
check_exit "the admin send exits 0" 0
ADMIN_ID="$(awk '$1 == "accepted" { print $2; exit }' "$OUT")"

run "$CYC" status --plain
check "status reports the unread admin count" 'admin inbox 1$'

agent_command implementer "$N1" '--json messages'
jq -r --arg id "$ADMIN_ID" '
  .rows[] | select(.message_id == $id) | .recipients[]
  | select(.label == "admin") | "\(.mailbox.status) \(.notification.state)"' \
  "$OUT" > "$ROOT/admin-state"
cp "$ROOT/admin-state" "$OUT"
cat "$OUT"
check "the admin mailbox stays pending without a wake" '^pending not_started$'

echo
echo "#### docs/guides/sizing.md: handing a session's window sizing back"

# The shapes that page promises. A workspace is not running here, so this is
# the ordinary path: nothing of cyclops' making to undo, said plainly, and a
# zero exit. The refusals have their own tests in src/cyclops/tests, because
# staging a live owner or a corrupt record is not a documentation walk.
#
# The session is named because this rig drives the CLI from outside tmux,
# where there is no current session to default to. That refusal is itself a
# documented shape, so it is checked first.
run "$CYC" sizing release --plain
check "release outside tmux says which flag to use" 'name the session with --session'
check_exit "release outside tmux exits 2" 2

run "$CYC" sizing release --session main --plain
check "release names the session it acted on" '^main: cyclops sizing released$'
check "release counts the windows it put back" 'window\(s\) put back on their original policy$'
check_exit "releasing a clean session exits 0" 0

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
DUO_TMUX_CONFIG="$ROOT/duo/tmux.conf"
duo() { ( cd "$ROOT/duo/elsewhere" && CYCLOPS_HOME="$DUO_HOME" TMUX_TMPDIR="$ROOT/duo/tmux" "$@" ); }
duo_tmx() { duo tmux -u -f "$DUO_TMUX_CONFIG" "$@"; }
duo_daemon_up() { duo "$CYC" --json status >/dev/null 2>&1; }
duo_attached() { duo "$CYC" --json status | jq -e '.sessions[0].attached == true' >/dev/null; }
duo_waiting_for_session() {
  grep -Fq 'waiting for session; create it, then call session.watch' "$DUO_HOME/cyclopsd.log"
}
duo_roster_has() { duo "$CYC" --json list | jq -e --arg a "$1" '[.agents[].agent] | index($a)' >/dev/null; }
duo_pane_has_text() { duo_tmx capture-pane -p -t "$1" | grep -Fq -- "$2"; }
duo_pane_matches() { duo_tmx capture-pane -p -t "$1" | grep -Eq -- "$2"; }
duo_pane_bound_to() {
  duo "$CYC" --json status | jq -e --arg p "$1" --arg m "$2" \
    '[.sessions[].panes[] | select(.pane_id == $p and .manifest == $m)] | length > 0' >/dev/null
}
duo_agent_command() {
  local label="$1" command="$3"
  rm -f "$ROOT/result.$label" "$ROOT/exit.$label" "$ROOT/done.$label"
  printf '\n$ (run by the %s agent) cyclops %s\n' "$label" "$command"
  local line="CYCLOPS_HOME=\"$DUO_HOME\" TMUX_TMPDIR=\"$ROOT/duo/tmux\" \"$CYC\" $command > \"$ROOT/result.$label\" 2>&1; code=\$?; printf '%s' \"\$code\" > \"$ROOT/exit.$label\"; : > \"$ROOT/done.$label\""
  printf 'run\t%s\n' "$line" > "$ROOT/duo/control.$label"
  wait_for "the second rig agent command" 100 pane_command_done "$label"
  cp "$ROOT/result.$label" "$OUT"
  cp "$ROOT/exit.$label" "$ROOT/exit"
  cat "$OUT"
}
duo_agent_ready() { [ -f "$ROOT/duo/ready.$1" ]; }
duo_start_agent() {
  local label="$1" pane="$2"
  rm -f "$ROOT/duo/control.$label" "$ROOT/duo/ready.$label"
  mkfifo "$ROOT/duo/control.$label"
  duo_tmx respawn-pane -k -t "$pane" \
    "'$ROOT/cycagent' '$ROOT/duo/control.$label' '$ROOT/duo/ready.$label'"
  wait_for "the second rig $label agent" 100 duo_agent_ready "$label"
}
duo_pane_write_ready() {
  duo "$CYC" read "$1" --source detection --plain | grep -q 'write-ready$'
}
duo_notification_crossed_submit() {
  local journal
  journal="$(find "$DUO_HOME/workspaces" -name messages.ndjson -print -quit)"
  [ -n "$journal" ] && jq -e --arg id "$1" '
    select(.id == $id and .data.type == "notification_transition")
    | .data.state
    | select(. == "submitted" or . == "notified")' \
    "$journal" >/dev/null
}
duo_start_composer() {
  local label="$1" pane="$2"
  duo_tmx resize-window -t "$pane" -x 500 -y 40
  printf 'composer\t%s\n' "$COMPOSER_PATH" > "$ROOT/duo/control.$label"
  wait_for "the second rig $label composer" 100 duo_pane_has_text "$pane" 'Model x · Ctx: 78%'
  wait_for "the second rig $label composer to be write-ready" 100 duo_pane_write_ready "$label"
}

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

# The external-supervisor path: set the home up, start the daemon, then
# create its configured session with --no-daemon. This is distinct from the
# ordinary one-command journey below. The fixture config holds an empty tmux
# server open so a control attach made before the session exists is observable
# rather than disappearing before this test can inspect it.
printf '\n$ cd ~/scratch && cyclops start --setup-only\n'
duo "$CYC" start --setup-only --plain > "$OUT" 2>&1
cat "$OUT"
check "setup writes the config"           'wrote .*/config\.toml$'
check "and installs the manifests"        '^  wrote 4 detection manifests to .*/manifests$'
check_absent "and opens nothing"          'workspace ready'

printf '%s\n' 'set-option -g exit-empty off' 'set-option -g exit-unattached off' \
  > "$DUO_TMUX_CONFIG"
printf '\ntmux_config = "%s"\n' "$DUO_TMUX_CONFIG" >> "$DUO_HOME/config.toml"

# The daemon runs from the same empty directory. Nothing in the config
# names a manifest directory, so it has to find the one setup just wrote.
start_duo_daemon
wait_for "the external daemon to wait for main" 50 duo_waiting_for_session
find "$ROOT/duo/tmux" -type s -print > "$OUT"
cat "$OUT"
check_absent "waiting for main does not create a tmux server" '.'

printf '\n$ cyclopsd &\n$ cyclops start --preset duo\n'
duo "$CYC" start --preset duo --no-daemon --plain > "$OUT" 2>&1
cat "$OUT"
# The heavy check: one run, with the daemon confirming every name. This is
# the whole point of the order, and the glyph is what proves it happened.
check "duo opens two panes"               '^✔ workspace ready · 2 agents$'
check "step 1 attaches"                   '^  1  tmux attach -t main +open the workspace and start your agents$'
check "step 2 names the first agent"      '^  2  cyclops send implementer --subject "hello" --summary "Hello from Cyclops\. Reply when you are ready\." +send the first message$'

wait_for "the second daemon to attach" 60 duo_attached

printf '\n$ cyclops --json status | jq -r .manifests.ids[]\n'
duo "$CYC" --json status | jq -r '.manifests.ids[]' > "$OUT"
cat "$OUT"
check "the daemon found the shipped set"  '^claude$'

# No shipped manifest binds the fixture agent, so both panes read unknown.
# This is the state the admin hit, and the surface has to say why rather than
# only labelling it.
read -r D1 D2 <<<"$(duo_tmx list-panes -t main -F '#{pane_id}' | tr '\n' ' ')"
duo_start_agent implementer "$D1"
duo_start_agent reviewer "$D2"
duo_tmx select-pane -t "$D1" -T ''
duo_tmx select-pane -t "$D2" -T ''
sleep 2.5

printf '\n$ cyclops status\n'
duo "$CYC" status --plain > "$OUT" 2>&1
cat "$OUT"
check "an unknown pane is on the grid"    '\? unknown'
check "and the grid says why"             'read unknown: unsupported_vendor'
check "and what to do about it"           'Teach cyclops this program with a manifest'

# Teaching cyclops the CLI in those panes is one file in the home
# directory the daemon already reads (docs/reference/MANIFESTS.md).
printf '\n$ $EDITOR ~/.cyclops/manifests/demo.toml\n$ (restart cyclopsd)\n'
printf '%s\n' "$DEMO_MANIFEST" > "$DUO_HOME/manifests/demo.toml"
stop_duo_daemon
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

stop_duo_daemon
duo_start_agent implementer "$D1"
duo_start_agent reviewer "$D2"
start_duo_daemon
wait_for "the second daemon to re-attach to fixture agents" 60 duo_attached
duo "$CYC" name "$D1" implementer --plain > /dev/null
duo "$CYC" name "$D2" reviewer --plain > /dev/null
wait_for "the second daemon to bind implementer" 100 duo_pane_bound_to "$D1" demo
wait_for "the second daemon to bind reviewer" 100 duo_pane_bound_to "$D2" demo
duo_start_composer reviewer "$D2"

printf '\n$ cyclops send reviewer --subject "hello" --summary "Review the greeting. Reply when ready."\n'
duo_agent_command implementer "$D1" 'send reviewer --subject "hello" --summary "Review the greeting. Reply when ready." --client-key parity-duo --plain'
check "the first message is accepted"     '^accepted m-[[:xdigit:]]{32}$'
check "and reports notification separately" '^✓ accepted( · [0-9]+ ahead)? · wake (not started|queued|checking readiness|writing|staged|submitted|notified|withdrawn|needs attention|superseded)$'
check_exit "and accepted send exits 0" 0
DUO_MESSAGE_ID="$(awk '$1 == "accepted" { print $2; exit }' "$OUT")"
wait_for "the second rig reviewer doorbell" 100 duo_pane_matches "$D2" '^❯ \[cyclops from implementer\] Review the greeting\. Reply when ready\. \| cyclops inbox claim m-att_[A-Za-z0-9_-]{22}$'
wait_for "the second rig doorbell to be submitted" 100 duo_notification_crossed_submit "$DUO_MESSAGE_ID"

printf '\n$ cyclops history\n'
duo_agent_command implementer "$D1" 'history --plain'
check "and history holds the message fact" '^ +[0-9]+s +implementer → reviewer +hello$'

stop_duo_daemon

echo
echo "#### The shipped defaults"

# This leg keeps the generated config untouched. Standard send must accept the
# mailbox write whether hooks are wired or the target pane currently has a
# usable manifest. Those conditions affect only the one-line notification.
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
stock_pane_has_text() { stock_tmx capture-pane -p -t "$1" | grep -Fq -- "$2"; }
stock_pane_matches() { stock_tmx capture-pane -p -t "$1" | grep -Eq -- "$2"; }
stock_pane_bound_to() {
  stock "$CYC" --json status | jq -e --arg p "$1" --arg m "$2" \
    '[.sessions[].panes[] | select(.pane_id == $p and .manifest == $m)] | length > 0' >/dev/null
}
stock_agent_command() {
  local label="$1" command="$3"
  rm -f "$ROOT/result.$label" "$ROOT/exit.$label" "$ROOT/done.$label"
  printf '\n$ (run by the %s agent) cyclops %s\n' "$label" "$command"
  local line="CYCLOPS_HOME=\"$STOCK_HOME\" TMUX_TMPDIR=\"$ROOT/stock/tmux\" \"$CYC\" $command > \"$ROOT/result.$label\" 2>&1; code=\$?; printf '%s' \"\$code\" > \"$ROOT/exit.$label\"; : > \"$ROOT/done.$label\""
  printf 'run\t%s\n' "$line" > "$ROOT/stock/control.$label"
  wait_for "the defaults rig agent command" 100 pane_command_done "$label"
  cp "$ROOT/result.$label" "$OUT"
  cp "$ROOT/exit.$label" "$ROOT/exit"
  cat "$OUT"
}
stock_agent_ready() { [ -f "$ROOT/stock/ready.$1" ]; }
stock_start_agent() {
  local label="$1" pane="$2"
  rm -f "$ROOT/stock/control.$label" "$ROOT/stock/ready.$label"
  mkfifo "$ROOT/stock/control.$label"
  stock_tmx respawn-pane -k -t "$pane" \
    "'$ROOT/cycagent' '$ROOT/stock/control.$label' '$ROOT/stock/ready.$label'"
  wait_for "the defaults rig $label agent" 100 stock_agent_ready "$label"
}
stock_pane_write_ready() {
  stock "$CYC" read "$1" --source detection --plain | grep -q 'write-ready$'
}
stock_notification_crossed_submit() {
  local journal
  journal="$(find "$STOCK_HOME/workspaces" -name messages.ndjson -print -quit)"
  [ -n "$journal" ] && jq -e --arg id "$1" '
    select(.id == $id and .data.type == "notification_transition")
    | .data.state
    | select(. == "submitted" or . == "notified")' \
    "$journal" >/dev/null
}
stock_start_composer() {
  local label="$1" pane="$2"
  stock_tmx resize-window -t "$pane" -x 500 -y 40
  printf 'composer\t%s\n' "$COMPOSER_PATH" > "$ROOT/stock/control.$label"
  wait_for "the defaults rig $label composer" 100 stock_pane_has_text "$pane" 'Model x · Ctx: 78%'
  wait_for "the defaults rig $label composer to be write-ready" 100 stock_pane_write_ready "$label"
}

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

# One fixture manifest binds the first pane. Standard mailbox behavior does
# not depend on an acknowledgement hook.
printf '\n$ $EDITOR ~/.cyclops/manifests/demo.toml\n'
printf '%s\n' "$DEMO_MANIFEST" > "$STOCK_HOME/manifests/demo.toml"

# Nothing is appended to the config. The gate exercises generated defaults.
check_file_absent "this leg runs on untouched defaults" \
  "$STOCK_HOME/config.toml" 'receipt_block_ms|ack_timeout_ms'

start_stock_daemon
wait_for "the defaults daemon to attach" 60 stock_attached

read -r S1 S2 <<<"$(stock_tmx list-panes -t main -F '#{pane_id}' | tr '\n' ' ')"
# Pane 1 is bound by the fixture manifest and reports no hook. The fixture
# loop keeps its one-line doorbell visible and can issue authenticated sends.
stock_start_agent implementer "$S1"
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

# Standard defaults accept the mailbox write and send only a doorbell to a
# bound recipient. The sender remains the dedicated fixture agent.
stop_stock_daemon
stock_start_agent implementer "$S1"
stock_start_agent reviewer "$S2"
start_stock_daemon
wait_for "the defaults daemon to re-attach to fixture agents" 60 stock_attached
stock_run "$CYC" name "$S1" implementer --plain
stock_run "$CYC" name "$S2" reviewer --plain
wait_for "the defaults daemon to bind implementer" 100 stock_pane_bound_to "$S1" demo
wait_for "the defaults daemon to bind reviewer" 100 stock_pane_bound_to "$S2" demo
stock_start_composer reviewer "$S2"

stock_agent_command implementer "$S1" 'send reviewer --subject "bound hello" --summary "Review the bound greeting. Claim the private details." --body "private default body" --client-key parity-stock-bound --plain'
check "the default bound send is accepted" '^accepted m-[[:xdigit:]]{32}$'
check "its wake state is separate" '^✓ accepted( · [0-9]+ ahead)? · wake (not started|queued|checking readiness|writing|staged|submitted|notified|withdrawn|needs attention|superseded)$'
check_exit "the default bound send exits 0" 0
STOCK_BOUND_ID="$(awk '$1 == "accepted" { print $2; exit }' "$OUT")"
wait_for "the defaults reviewer doorbell" 100 stock_pane_matches "$S2" '^❯ \[cyclops from implementer\] Review the bound greeting\. Claim the private details\. \| cyclops inbox claim m-att_[A-Za-z0-9_-]{22}$'
wait_for "the defaults doorbell to be submitted" 100 stock_notification_crossed_submit "$STOCK_BOUND_ID"
stock_tmx capture-pane -p -t "$S2" > "$OUT"
check "the default pane gets the sender, summary, and exact attempt doorbell" '^❯ \[cyclops from implementer\] Review the bound greeting\. Claim the private details\. \| cyclops inbox claim m-att_[A-Za-z0-9_-]{22}$'
check_absent "the default pane does not get the body" '^private default body$'

# History owns message facts, not standard notification badges.
stock_agent_command implementer "$S1" 'history --plain'
check "the bound message is on the record" '^ +[0-9]+s +implementer → reviewer +bound hello$'
check_absent "history has no standard delivery badge" 'delivered ·|needs attention ·'

stop_stock_daemon
fi

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
if [ "$(uname -s)" = Darwin ]; then
  INST_INSTALLER_CACHE="$INST/Library/Caches/Cyclops/installer"
else
  INST_INSTALLER_CACHE="$INST/.cache/cyclops/installer"
fi

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

run_installer_in() {
  local home="$1" path="$2" out="$3" marker="$4"; shift 4
  set +e
  if [ -n "$marker" ]; then
    env -i \
      PATH="$path" \
      HOME="$home" \
      SHELL=/bin/zsh \
      TERM=dumb \
      NO_COLOR=1 \
      "${TOOLCHAIN_KEEP[@]}" \
      "CYCLOPS_TEST_COPY_FAILURE_MARKER=$marker" \
      sh "$REPO/scripts/install.sh" "$@" > "$out" 2>&1
  else
    env -i \
      PATH="$path" \
      HOME="$home" \
      SHELL=/bin/zsh \
      TERM=dumb \
      NO_COLOR=1 \
      "${TOOLCHAIN_KEEP[@]}" \
      sh "$REPO/scripts/install.sh" "$@" > "$out" 2>&1
  fi
  printf '%s' "$?" > "$ROOT/exit"
  set -e
}

run_installer() {
  local path="$1"; shift
  run_installer_in "$INST" "$path" "$OUT" "" "$@"
}

printf '\n$ ./scripts/install.sh\n'
run_installer "$PATH"
# The build log is noise here; the checks are all on what it says it did.
grep -v '^ *\(Compiling\|Finished\|Downloaded\|Blocking\|Updating\|Adding\)' "$OUT" | tail -22

check "it says where the command went"    "^  command  $INST/.local/bin/cyclops$"
check "and the daemon too"                 "^  daemon   $INST/.local/bin/cyclopsd$"
check "and where Cyclops stores state"     "^  state    $INST/.cyclops$"
check "it reports the version it built"   "^✔ cyclops $PACKAGE_VERSION_RE \\(([0-9a-f]+(\\.dirty)?|unknown)\\) is installed$"
check_absent "it gives no separate daemon step" 'cyclopsd &'
check "step 2 opens the workspace"        '^  2  cyclops +open your workspace and start your agents$'
check "it names the profile it edited"    "^  three lines added to $INST/.zshrc:$"
check "and shows the block it added"      '^    # >>> cyclops >>>$'
check "and the PATH line inside it"       "^    export PATH=\"$INST/.local/bin:\\\$PATH\"$"
check "and where the backup went"         "^  the file as it was: $INST/.zshrc.cyclops-backup.[0-9]+$"

check_file "the binaries are executable"  "$INST/.local/bin/cyclops" '.'
check_file "and the home has a config"    "$INST/.cyclops/config.toml" '^sessions = '
check_file "and the shipped manifests"    "$INST/.cyclops/manifests/claude.toml" '^id = "claude"$'
if [ -d "$INST_INSTALLER_CACHE/target" ]; then
  printf '   ok    installer keeps its build cache under the user cache\n'
else
  printf '   FAIL  installer build cache is missing: %s\n' "$INST_INSTALLER_CACHE/target"
  FAILS=$((FAILS + 1))
fi
check_exit "a clean install exits 0" 0

# The profile has exactly one block, not one per run, and the second run
# says so instead of appending another.
printf '\n$ ./scripts/install.sh    # again\n'
run_installer "$PATH"
grep -c '>>> cyclops >>>' "$INST/.zshrc" > "$ROOT/blocks"
cat "$OUT" | grep 'already has the cyclops block' || true
check_file "a second run adds no second block" "$ROOT/blocks" '^1$'
check_exit "the idempotent install exits 0" 0

# A failed macOS clone-optimized copy must not make a successful build
# un-installable. This wrapper fails only the first private client candidate
# copy; every other cp call delegates to the system implementation.
COPY_FALLBACK_HOME="$ROOT/copy-fallback"
COPY_FALLBACK_PREFIX="$COPY_FALLBACK_HOME/bin"
COPY_FALLBACK_OUT="$ROOT/copy-fallback.out"
COPY_FALLBACK_MARKER="$ROOT/copy-fallback.marker"
mkdir -p "$COPY_FALLBACK_HOME"
printf '\n$ ./scripts/install.sh --prefix ...    # private candidate copy fallback\n'
run_installer_in "$COPY_FALLBACK_HOME" \
  "$REPO/tests/e2e/fixtures/installer-copy-fails:$PATH" \
  "$COPY_FALLBACK_OUT" \
  "$COPY_FALLBACK_MARKER" \
  --prefix "$COPY_FALLBACK_PREFIX" --no-path
tail -12 "$COPY_FALLBACK_OUT"
check_exit "the private candidate copy fallback installs successfully" 0
check_file "the fallback simulation reaches the private candidate copy" \
  "$COPY_FALLBACK_MARKER" '^intentional private candidate copy failure$'
check_file "the fallback installs the client" "$COPY_FALLBACK_PREFIX/cyclops" '.'
check_file "and the fallback installs the daemon" "$COPY_FALLBACK_PREFIX/cyclopsd" '.'
check_same_file "the fallback keeps client bytes exact" \
  "$REPO/target/dist/cyclops" "$COPY_FALLBACK_PREFIX/cyclops"
check_same_file "and the fallback keeps daemon bytes exact" \
  "$REPO/target/dist/cyclopsd" "$COPY_FALLBACK_PREFIX/cyclopsd"

echo
echo "#### An installed pair completes the first durable handoff"

# The first-handoff journey uses only the pair the installer selected. It has
# its own home, tmux server, daemon process, and fixture-agent controls. It runs
# from an empty directory and borrows no Cyclops binaries or state from the
# checkout; only the test fixture and reviewed seeded bytes come from here.
INST_HOME="$INST/.cyclops"
INST_CYC="$INST/.local/bin/cyclops"
INST_CYCD="$INST/.local/bin/cyclopsd"
mkdir -p "$ROOT/installed/tmux" "$ROOT/installed/elsewhere"
printf '\n# operator note retained across update and rollback\n' >> "$INST_HOME/config.toml"
printf '%s\n' "$DEMO_MANIFEST" > "$INST_HOME/manifests/demo.toml"

installed() {
  ( cd "$ROOT/installed/elsewhere" && env \
      HOME="$INST" \
      CYCLOPS_HOME="$INST_HOME" \
      TMUX_TMPDIR="$ROOT/installed/tmux" \
      PATH="$INST/.local/bin:$PATH" \
      "$@" )
}
installed_tmx() { installed tmux -u "$@"; }
installed_daemon_up() { installed "$INST_CYC" --json status >/dev/null 2>&1; }
installed_attached() {
  installed "$INST_CYC" --json status |
    jq -e '.sessions[0].attached == true' >/dev/null
}
installed_pane_bound_to() {
  installed "$INST_CYC" --json status | jq -e --arg p "$1" --arg m "$2" \
    '[.sessions[].panes[] | select(.pane_id == $p and .manifest == $m)] | length > 0' \
    >/dev/null
}
installed_agent_ready() { [ -f "$ROOT/installed/ready.$1" ]; }
installed_command_done() { [ -f "$ROOT/installed/done.$1" ]; }

installed_start_agent() {
  local label="$1" pane="$2"
  rm -f "$ROOT/installed/control.$label" "$ROOT/installed/ready.$label"
  mkfifo "$ROOT/installed/control.$label"
  installed_tmx respawn-pane -k -t "$pane" \
    "'$ROOT/cycagent' '$ROOT/installed/control.$label' '$ROOT/installed/ready.$label'"
  wait_for "the installed $label fixture agent" 100 installed_agent_ready "$label"
}

installed_agent_command() {
  local label="$1" command="$3" line
  rm -f "$ROOT/installed/result.$label" "$ROOT/installed/exit.$label" \
    "$ROOT/installed/done.$label"
  printf '\n$ (run by the installed %s agent) cyclops %s\n' "$label" "$command"
  line="HOME=\"$INST\" CYCLOPS_HOME=\"$INST_HOME\" TMUX_TMPDIR=\"$ROOT/installed/tmux\" PATH=\"$INST/.local/bin:$PATH\" \"$INST_CYC\" $command > \"$ROOT/installed/result.$label\" 2>&1; code=\$?; printf '%s' \"\$code\" > \"$ROOT/installed/exit.$label\"; : > \"$ROOT/installed/done.$label\""
  printf 'run\t%s\n' "$line" > "$ROOT/installed/control.$label"
  wait_for "the installed $label agent command" 100 installed_command_done "$label"
  cp "$ROOT/installed/result.$label" "$OUT"
  cp "$ROOT/installed/exit.$label" "$ROOT/exit"
  cat "$OUT"
}

start_installed_daemon() {
  ( cd "$ROOT/installed/elsewhere" && exec env \
      HOME="$INST" \
      CYCLOPS_HOME="$INST_HOME" \
      TMUX_TMPDIR="$ROOT/installed/tmux" \
      PATH="$INST/.local/bin:$PATH" \
      "$INST_CYCD" ) >>"$ROOT/installed/daemon.log" 2>&1 &
  INST_DAEMON_PID=$!
  wait_for "the installed daemon socket" 50 test -S "$INST_HOME/sock"
  wait_for "the installed daemon to answer" 50 installed_daemon_up
}

stop_installed_daemon() {
  [ -n "${INST_DAEMON_PID:-}" ] || return 0
  kill "$INST_DAEMON_PID" 2>/dev/null || true
  wait "$INST_DAEMON_PID" 2>/dev/null || true
  INST_DAEMON_PID=""
}

start_installed_daemon
installed "$INST_CYC" start --preset duo --no-daemon --plain > "$OUT" 2>&1
cat "$OUT"
check "the installed pair opens the workspace" '^✔ workspace ready · 2 agents$'
wait_for "the installed daemon to attach" 60 installed_attached

read -r I1 I2 <<<"$(installed_tmx list-panes -t main -F '#{pane_id}' | tr '\n' ' ')"
installed_start_agent implementer "$I1"
installed_start_agent reviewer "$I2"
wait_for "the installed daemon to name implementer" 100 \
  installed "$INST_CYC" name "$I1" implementer --plain
wait_for "the installed daemon to name reviewer" 100 \
  installed "$INST_CYC" name "$I2" reviewer --plain
wait_for "the installed daemon to bind implementer" 100 installed_pane_bound_to "$I1" demo
wait_for "the installed daemon to bind reviewer" 100 installed_pane_bound_to "$I2" demo

installed_agent_command implementer "$I1" \
  'send reviewer --subject "Installed handoff" --summary "Review the installed handoff. Reply when complete." --body "Installed pair reached durable messaging." --client-key parity-installed-handoff --plain'
check "the installed send is durably accepted" '^accepted m-[[:xdigit:]]{32}$'
check_exit "the installed send exits 0" 0
INST_HANDOFF_ID="$(awk '$1 == "accepted" { print $2; exit }' "$OUT")"
[ -n "$INST_HANDOFF_ID" ] || { echo "!! installed handoff id was not captured" >&2; exit 1; }

installed_agent_command reviewer "$I2" "inbox claim $INST_HANDOFF_ID --plain"
check "the installed reviewer claims the body" '^Installed pair reached durable messaging\.$'
check_exit "the installed claim exits 0" 0

installed_agent_command reviewer "$I2" \
  "reply $INST_HANDOFF_ID --summary \"Confirm the installed handoff. Record the result.\" --body \"Installed handoff complete.\" --client-key parity-installed-reply --plain"
check "the installed reply is durably accepted" '^accepted m-[[:xdigit:]]{32}$'
check_exit "the installed reply exits 0" 0
INST_REPLY_ID="$(awk '$1 == "accepted" { print $2; exit }' "$OUT")"
[ -n "$INST_REPLY_ID" ] || { echo "!! installed reply id was not captured" >&2; exit 1; }

installed_agent_command implementer "$I1" "inbox claim $INST_REPLY_ID --plain"
check "the installed implementer claims the reply" '^Installed handoff complete\.$'
check_exit "the installed reply claim exits 0" 0

# Quiesce the exact test-owned daemon before preserving the user-owned files.
# Update and rollback both prove replay while the original journal stays still.
stop_installed_daemon
find "$INST_HOME/workspaces" -name messages.ndjson -print > "$ROOT/installed/journals"
INST_JOURNAL_COUNT="$(wc -l < "$ROOT/installed/journals" | tr -d ' ')"
[ "$INST_JOURNAL_COUNT" -eq 1 ] || {
  echo "!! installed journey expected one message journal, found $INST_JOURNAL_COUNT" >&2
  exit 1
}
INST_JOURNAL="$(sed -n '1p' "$ROOT/installed/journals")"
cp "$INST_HOME/config.toml" "$ROOT/installed/config.before"
cp "$INST_JOURNAL" "$ROOT/installed/messages.before"
cp -L "$INST_CYC" "$ROOT/installed/cyclops.before"
cp -L "$INST_CYCD" "$ROOT/installed/cyclopsd.before"
"$INST_CYC" --version | sed 's/^cyclops //' > "$ROOT/installed/client.before-version"
"$INST_CYCD" --version | sed 's/^cyclopsd //' > "$ROOT/installed/daemon.before-version"
check_same_file "the installed client and daemon start as one exact pair" \
  "$ROOT/installed/client.before-version" "$ROOT/installed/daemon.before-version"

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
#                     and without threading it through the clone's dist
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
    "$INST/.local/bin/cyclops" update "$@" > "$OUT" 2>&1
  printf '%s' "$?" > "$ROOT/exit"
  set -e
}

printf '\n$ cyclops update\n'
run_update
grep -v '^ *\(Compiling\|Finished\|Downloaded\|Blocking\|Updating\|Adding\)' "$OUT" | tail -24

check "update names the running build"    "^cyclops $PACKAGE_VERSION_RE \\(([0-9a-f]+(\\.dirty)?|unknown)\\)$"
check "and its source"                    "^  source $ROOT/remote at parity-update$"
check "it reran the installer"            "^✔ cyclops $PACKAGE_VERSION_RE \\([0-9a-f]+\\) is installed$"
check "and reports old build to new"      "^✔ updated · $PACKAGE_VERSION_RE \\(([0-9a-f]+(\\.dirty)?|unknown)\\) → $PACKAGE_VERSION_RE \\([0-9a-f]+\\)$"
check "and keeps a stopped daemon stopped" '^  no daemon was running; the selected pair remains stopped$'
check_absent "it never stops the daemon itself" 'stopped cyclopsd'
check_exit "an update exits 0" 0

"$INST_CYC" --version | sed 's/^cyclops //' > "$ROOT/installed/client.updated-version"
"$INST_CYCD" --version | sed 's/^cyclopsd //' > "$ROOT/installed/daemon.updated-version"
check_same_file "the update selects one exact client and daemon build" \
  "$ROOT/installed/client.updated-version" "$ROOT/installed/daemon.updated-version"
check_different_file "the update selects a new build identity" \
  "$ROOT/installed/client.before-version" "$ROOT/installed/client.updated-version"
check_different_file "the selected client bytes changed" \
  "$ROOT/installed/cyclops.before" "$INST_CYC"
check_different_file "the selected daemon bytes changed" \
  "$ROOT/installed/cyclopsd.before" "$INST_CYCD"

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
check_same_file "the operator-edited config survives the update byte for byte" \
  "$ROOT/installed/config.before" "$INST_HOME/config.toml"
check_same_file "the durable handoff survives the update byte for byte" \
  "$ROOT/installed/messages.before" "$INST_JOURNAL"

printf '\n$ cyclops update --rollback\n'
run_update --rollback
cat "$OUT"
check "rollback reports completion" '^✔ rolled back$'
check "rollback names the restored active pair" '^  active pairs/pair\.[0-9a-f]{32}$'
check "rollback keeps the displaced pair as known-good" \
  '^  known-good pairs/pair\.[0-9a-f]{32}$'
check_exit "rollback exits 0" 0

"$INST_CYC" --version | sed 's/^cyclops //' > "$ROOT/installed/client.rolled-version"
"$INST_CYCD" --version | sed 's/^cyclopsd //' > "$ROOT/installed/daemon.rolled-version"
check_same_file "rollback restores the original client identity" \
  "$ROOT/installed/client.before-version" "$ROOT/installed/client.rolled-version"
check_same_file "rollback restores the original daemon identity" \
  "$ROOT/installed/daemon.before-version" "$ROOT/installed/daemon.rolled-version"
check_same_file "the rolled-back client and daemon remain one exact pair" \
  "$ROOT/installed/client.rolled-version" "$ROOT/installed/daemon.rolled-version"
check_same_file "rollback restores the original client bytes" \
  "$ROOT/installed/cyclops.before" "$INST_CYC"
check_same_file "rollback restores the original daemon bytes" \
  "$ROOT/installed/cyclopsd.before" "$INST_CYCD"
check_same_file "rollback leaves the operator-edited config byte-identical" \
  "$ROOT/installed/config.before" "$INST_HOME/config.toml"
check_same_file "rollback leaves the durable handoff byte-identical" \
  "$ROOT/installed/messages.before" "$INST_JOURNAL"

# The update legs built the mirror's clone into the repo's own target dir
# (the shared cache that keeps this job fast), which leaves the MIRROR's
# binaries at target/dist/cyclops{,d} while cargo still counts the
# repo's own bin units fresh: a later build from this checkout would
# silently reinstall the mirror's build. Deleting the binaries
# un-freshens exactly the final link step; the dependency cache stays
# warm.
rm -f "$REPO/target/dist/cyclops" "$REPO/target/dist/cyclopsd"

# The real uninstall journey starts with a live daemon. This is the boundary
# that used to make a user run a separate `cyclops daemon stop` before
# uninstalling, so prove the installer stops only its validated daemon before
# it removes the state home.
start_installed_daemon
if installed_daemon_up; then
  printf '   ok    the daemon is live before uninstall\n'
  CHECKS=$((CHECKS + 1))
else
  printf '   FAIL  the daemon is live before uninstall\n'
  FAILS=$((FAILS + 1))
fi

printf '\n$ ./scripts/install.sh --uninstall\n'
run_installer "$INST/.local/bin:$PATH" --uninstall
cat "$OUT"
check "uninstall stops its validated daemon" '^stopped selected cyclopsd pid [0-9]+$'
check "uninstall removes the complete state home" "state removed from $INST/.cyclops"
check_exit "uninstall exits 0" 0
CHECKS=$((CHECKS + 1))
if [ ! -e "$INST_HOME" ]; then
  printf '   ok    uninstall removes the retained state files\n'
else
  printf '   FAIL  uninstall removes the retained state files\n'
  FAILS=$((FAILS + 1))
fi

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
  echo "== $FAILS documented shape check(s) failed"
  exit 1
fi
echo "== representative documentation parity checks passed"
