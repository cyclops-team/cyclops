#!/usr/bin/env bash
# M4 demo: one workspace, end to end. Build it, name it, save it, lose it,
# get it back.
#
# This is where M4's two halves meet. `cyclops start` builds the duo
# preset, `cyclops name` puts the names on and tmux paints the borders,
# `cyclops workspace save` writes the shape and the roster to a file, the
# session is killed outright, and `restore` plus `start` bring back the
# panes, the ratios, the directories, the names and the border chrome.
#
# The last section proves it by comparison rather than by claim: the pane
# rectangles, the roster, and a second save of the restored session are
# diffed against what was there before. A mismatch prints the diff and
# exits non-zero, so this doubles as a smoke test.
#
# Never touches the default tmux server. Everything runs on a private
# server (tmux -u -L cyc-demo-$$ -f /dev/null, -u per finding F14) with a
# throwaway CYCLOPS_HOME, both removed by the EXIT trap. Safe to run
# repeatedly.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SOCK="cyc-demo-$$"
SESSION="demo"

# The scratch root and the tmux teardown rule are shared, not copied.
# shellcheck source=../tests/e2e/lib/lib.sh
. "$(cd "$(dirname "$0")" && pwd)/../tests/e2e/lib/lib.sh"
CYCLOPS_HOME="$(mktemp -d "$(cyc_scratch_root)/cyclops-demo.XXXXXX")"
export CYCLOPS_HOME
WORK="$CYCLOPS_HOME/compare"
mkdir -p "$WORK"
DAEMON_PID=""
DRIFT=0

cd "$REPO"

# Every tmux call in this script goes through the isolated socket.
tmx() { command tmux -u -L "$SOCK" -f /dev/null "$@"; }

cleanup() {
  if [ -n "$DAEMON_PID" ]; then
    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
  fi
  cyc_tmux_teardown "$SOCK"
  rm -rf "$CYCLOPS_HOME"
}
trap cleanup EXIT

# Wait for a condition the daemon reaches on its own backoff. The daemon
# stays event-driven; this is a script waiting for a process to boot, the
# way it would wait for any other server.
wait_for() {
  local what="$1" tries="$2"
  shift 2
  for _ in $(seq 1 "$tries"); do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep 0.25
  done
  echo "!! gave up waiting for $what; daemon log tail:" >&2
  tail -n 20 "$CYCLOPS_HOME/daemon.log" >&2 || true
  exit 1
}

# True once the daemon's control connection to the session is up, which is
# when it can resolve a pane and therefore name one.
#
# Matched on the field alone rather than on "this session's field": the
# daemon builds its reply through serde_json::Value, so the keys come out
# in alphabetical order, not declaration order (F29), and a pattern
# spanning two keys is really a pattern about the alphabet. This config
# watches one session, so one field answers the question.
daemon_attached() {
  "$CYC" --json status | grep -q '"attached":true'
}

# True once the daemon can see one specific pane. The condition after a
# restore: the daemon was attached to the session that just died, so
# "attached" alone can still be answering about the old one.
daemon_sees() {
  "$CYC" --json status | grep -q "\"pane_id\":\"$1\""
}

# True once the daemon has dropped every name, which is what a pane closing
# does. Waited on rather than slept through, so the roster printed after
# the kill is the settled one and not a stale read.
roster_empty() {
  "$CYC" --json list | grep -q '"agents":\[\]'
}

# The session's pane rectangles, position-sorted and without pane ids. Ids
# are re-issued by tmux when a session is rebuilt, so comparing them would
# report drift that is not there; the rectangles are the thing a workspace
# claims to restore.
geometry() {
  tmx list-panes -s -t "=$SESSION" \
    -F '#{pane_left},#{pane_top}  #{pane_width}x#{pane_height}' | sort
}

# The same rectangles with their pane ids, for reading rather than diffing.
show_panes() {
  tmx list-panes -s -t "=$SESSION" \
    -F '   #{pane_id}  at #{pane_left},#{pane_top}  #{pane_width}x#{pane_height}'
}

# Who is on the roster. States are live and belong to the agents, not to
# the workspace, so only the names are compared.
roster_names() {
  "$CYC" list --plain | awk '{print $1}' | sort
}

# A workspace file without the lines that name it: the comment header and
# the workspace's own name. Everything else is the arrangement.
ws_body() {
  grep -v -e '^#' -e '^name = ' "$1"
}

# One comparison, reported either way. Prints the diff when they differ
# and remembers it for the exit code.
same() {
  local what="$1" before="$2" after="$3"
  if diff -u "$before" "$after" >/dev/null; then
    printf '   %-22s same\n' "$what"
  else
    printf '   %-22s DIFFERENT\n' "$what"
    diff -u "$before" "$after" | sed 's/^/      /'
    DRIFT=1
  fi
}

echo "== demo home:   $CYCLOPS_HOME (removed on exit)"
echo "== tmux server: -L $SOCK (isolated, removed on exit)"

echo "== building cyclops and cyclopsd"
cargo build --quiet -p cyclops -p cyclopsd
CYC="$REPO/target/debug/cyclops"
CYCD="$REPO/target/debug/cyclopsd"

# The panes in this demo run `cat` as stand-in agents: a real workspace
# would run claude or codex here. This manifest reads their pane titles
# the way claude.toml reads claude's, so the states below are the real
# detection path and not a fixture.
mkdir -p "$CYCLOPS_HOME/manifests"
cat > "$CYCLOPS_HOME/manifests/demo.toml" <<'EOF'
[agent]
id = "demo"
display_name = "Demo agent"
process_names = ["cat"]

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
submit = "Enter"
EOF

cat > "$CYCLOPS_HOME/config.toml" <<EOF
sessions = ["$SESSION"]
default_workspace = "$SESSION"
tmux_socket = "$SOCK"
tmux_config = "/dev/null"
manifest_dir = "$CYCLOPS_HOME/manifests"
EOF

echo
echo "== cyclops start --preset duo   (no daemon yet: panes now, names later)"
"$CYC" start --preset duo --plain

echo
echo "== what tmux built"
show_panes

read -r P1 P2 <<<"$(tmx list-panes -s -t "=$SESSION" -F '#{pane_id}' | tr '\n' ' ')"

# Stand-in agents. respawn-pane replaces the pane's shell with `cat`;
# cyclops sends no keys to a pane and neither does this.
tmx respawn-pane -k -t "$P1" cat
tmx respawn-pane -k -t "$P2" cat

echo
echo "== starting cyclopsd (log: $CYCLOPS_HOME/daemon.log)"
"$CYCD" >"$CYCLOPS_HOME/daemon.log" 2>&1 &
DAEMON_PID=$!
wait_for "the daemon socket" 40 test -S "$CYCLOPS_HOME/sock"
wait_for "cyclopsd to attach to $SESSION" 40 daemon_attached

# A title the way an agent publishes one. tmux re-evaluates its format
# subscriptions once a second (F23), so give it a tick.
tmx select-pane -t "$P1" -T "Implementing rate limiter"
sleep 2

echo
# The preset already carries these two names, and `start` would have put
# them on by itself. It could not: no daemon was running when it built the
# session, and only the daemon can adopt a pane. So this is the other way
# in, and the one a person uses on a session they arranged themselves.
echo "== cyclops name: two panes, two names"
"$CYC" name "$P1" implementer --plain
"$CYC" name "$P2" reviewer --plain
sleep 1.5

echo
echo "== cyclops list"
"$CYC" list --plain

echo
echo "== cyclops status"
"$CYC" status --plain

echo
echo "== the border each named pane wears (tmux expands it, not cyclops)"
for p in "$P1" "$P2"; do
  printf '   %s\n' "$(tmx display-message -p -t "$p" '#{E:pane-border-format}')"
done
echo "   window pane-border-status = [$(tmx show-options -w -t "$P1" -v pane-border-status)]"

echo
echo "== and what that costs: tmux draws a border line where the text goes"
show_panes

echo
echo "== cyclops workspace save"
"$CYC" workspace save --plain

echo
echo "== the file"
sed 's/^/   /' "$CYCLOPS_HOME/workspaces/$SESSION.toml"

# Everything the workspace claims to bring back, recorded before it is
# destroyed.
geometry > "$WORK/geometry.before"
roster_names > "$WORK/roster.before"
ws_body "$CYCLOPS_HOME/workspaces/$SESSION.toml" > "$WORK/file.before"

echo
echo "== now lose it: tmux kill-session -t $SESSION"
tmx kill-session -t "=$SESSION"
wait_for "the roster to empty" 40 roster_empty
"$CYC" list --plain

echo
echo "== cyclops workspace restore --launch   (panes, sizes, directories, commands)"
"$CYC" workspace restore --launch --plain

# This session was the only one on the demo's server, so killing it took
# the server down and the new one hands out pane ids from %0 again. The
# names below come back from the workspace file; nothing carries them
# across on a pane id.
read -r R1 R2 <<<"$(tmx list-panes -s -t "=$SESSION" -F '#{pane_id}' | tr '\n' ' ')"
echo "   rebuilt as $R1 and $R2"
wait_for "cyclopsd to see the rebuilt $SESSION" 60 daemon_sees "$R1"

echo
echo "== cyclops start   (the line restore printed: the names go back on)"
"$CYC" start --plain
sleep 1.5

echo
echo "== cyclops list"
"$CYC" list --plain

echo
echo "== the borders came back too, on the new panes"
for p in "$R1" "$R2"; do
  printf '   %s  %s\n' "$p" "$(tmx display-message -p -t "$p" '#{E:pane-border-format}')"
done

echo
echo "== the session, pane for pane"
show_panes

echo
echo "== save the restored session again, and compare the three things"
"$CYC" workspace save restored --plain
geometry > "$WORK/geometry.after"
roster_names > "$WORK/roster.after"
ws_body "$CYCLOPS_HOME/workspaces/restored.toml" > "$WORK/file.after"

echo
same "pane rectangles" "$WORK/geometry.before" "$WORK/geometry.after"
same "roster names" "$WORK/roster.before" "$WORK/roster.after"
same "workspace file" "$WORK/file.before" "$WORK/file.after"

echo
if [ "$DRIFT" -ne 0 ]; then
  echo "== the workspace did not round-trip; the diffs are above"
  exit 1
fi
echo "== done: the session was destroyed and came back the same shape, the"
echo "   same roster, and the same file. What the agents were doing is"
echo "   theirs, not the workspace's."
