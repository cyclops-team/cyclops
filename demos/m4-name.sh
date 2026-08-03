#!/usr/bin/env bash
# M4 demo: naming panes, the roster, and the border a named pane wears.
#
# Three panes get names, `cyclops list` prints the roster, one agent goes
# to work and its border follows, the name survives a daemon restart, and
# `--clear` puts tmux back the way it was found.
#
# Never touches the default tmux server. Everything runs on a private
# server (tmux -u -L cyc-demo-$$ -f /dev/null, -u per finding F14) with a
# throwaway CYCLOPS_HOME, both removed by the EXIT trap. Safe to run
# repeatedly.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SOCK="cyc-demo-$$"
SESSION="demo"
# The scratch root, the tmux teardown rule and the daemon stop are
# shared, not copied.
# shellcheck source=demos/lib.sh
. "$(cd "$(dirname "$0")" && pwd)/lib.sh"
CYCLOPS_HOME="$(mktemp -d "$(cyc_scratch_root)/cyclops-demo.XXXXXX")"
export CYCLOPS_HOME
DAEMON_PID=""

cd "$REPO"

tmx() { command tmux -u -L "$SOCK" -f /dev/null "$@"; }

cleanup() {
  cyc_stop_daemon
  cyc_tmux_teardown "$SOCK"
  rm -rf "$CYCLOPS_HOME"
}
trap cleanup EXIT

echo "== demo home:   $CYCLOPS_HOME (removed on exit)"
echo "== tmux server: -L $SOCK (isolated, removed on exit)"

cargo build --quiet
CYC="$REPO/target/debug/cyclops"
CYCD="$REPO/target/debug/cyclopsd"

# A detection manifest for this demo's panes. Real manifests live in
# manifests/ and bind real agent CLIs; these panes run `cat`, so the demo
# ships rules that read the pane title the way claude's do.
mkdir -p "$CYCLOPS_HOME/manifests"
cat > "$CYCLOPS_HOME/manifests/demo.toml" <<'EOF'
[agent]
id = "demo"
display_name = "Demo agent"
process_names = ["cat"]

[[rule]]
id = "title_blocked"
state = "blocked_permission"
priority = 1200
region = "pane_title"
regex = ['^APPROVE']

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
tmux_socket = "$SOCK"
tmux_config = "/dev/null"
manifest_dir = "$CYCLOPS_HOME/manifests"
EOF

command tmux -u -f /dev/null -L "$SOCK" new-session -d -s "$SESSION" -x 140 -y 40 cat
tmx split-window -t "$SESSION" -d cat
tmx split-window -t "$SESSION" -d cat
sleep 0.5
read -r P1 P2 P3 <<<"$(tmx list-panes -t "$SESSION" -F '#{pane_id}' | tr '\n' ' ')"

"$CYCD" >"$CYCLOPS_HOME/daemon.log" 2>&1 &
DAEMON_PID=$!
sleep 1.5

# Titles the way an agent publishes them. tmux re-evaluates its format
# subscriptions once a second (F23), so give it a tick.
tmx select-pane -t "$P1" -T "Implementing rate limiter"
tmx select-pane -t "$P3" -T "APPROVE: write to gateway.rs?"
sleep 2

echo
echo "== name three panes"
"$CYC" name "$P1" implementer --plain
"$CYC" name "$P2" reviewer --plain
"$CYC" name "$P3" tests --plain
sleep 1.5

echo
echo "== cyclops list"
"$CYC" list --plain

echo
echo "== what each pane's border renders (tmux expands it, not cyclops)"
for p in "$P1" "$P2" "$P3"; do
  printf '   %s\n' "$(tmx display-message -p -t "$p" '#{E:pane-border-format}')"
done

echo
echo "== reviewer starts a turn; the border follows the state, on that edge alone"
tmx select-pane -t "$P2" -T "Reviewing the burst path"
sleep 2.5
"$CYC" list --plain
printf '   %s\n' "$(tmx display-message -p -t "$P2" '#{E:pane-border-format}')"

echo
echo "== the roster is a file, so it survives a restart"
cat "$CYCLOPS_HOME/registry.json"
cyc_stop_daemon
sleep 0.5
"$CYCD" >>"$CYCLOPS_HOME/daemon.log" 2>&1 &
DAEMON_PID=$!
sleep 2
"$CYC" list --plain

echo
echo "== --clear gives the pane and its window back to tmux"
echo "   before: pane-border-format = [$(tmx show-options -p -t "$P1" pane-border-format)]"
"$CYC" name implementer --clear --plain
"$CYC" name reviewer --clear --plain
"$CYC" name tests --clear --plain
echo "   after:  pane-border-format = [$(tmx show-options -p -t "$P1" pane-border-format)]"
echo "   after:  pane-border-status = [$(tmx show-options -w -t "$SESSION" pane-border-status)]"

echo
echo "== the record"
grep -o '"event":"pane_labeled"[^}]*}' "$CYCLOPS_HOME/ledger/$SESSION.ndjson"

echo
echo "== done"
