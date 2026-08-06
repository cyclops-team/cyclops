#!/usr/bin/env bash
# M5 demo: switching themes, and what a half-written theme file does not do.
#
# Three things, none of which can be checked by reading code. A switch
# reaches a pane border that tmux, not cyclops, expands. An edit to the
# live file reaches the same border. And a file caught mid-save leaves the
# border exactly where it was, with one line said about it.
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
# shellcheck source=../tests/e2e/lib/lib.sh
. "$(cd "$(dirname "$0")" && pwd)/../tests/e2e/lib/lib.sh"
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

# This demo's panes run `cat`, so they need a manifest of their own. Real
# manifests live in resources/manifests/ and bind real agent CLIs.
mkdir -p "$CYCLOPS_HOME/manifests"
cat > "$CYCLOPS_HOME/manifests/demo.toml" <<'EOF'
[agent]
id = "demo"
display_name = "Demo agent"
process_names = ["cat"]

[[rule]]
id = "title_idle"
state = "idle"
priority = 1000
region = "pane_title"
regex = ['^']

[injection]
submit = "Enter"
EOF

# The shipped themes, in this home rather than the repo's, so the demo can
# edit one without touching the tree.
mkdir -p "$CYCLOPS_HOME/themes"
cp "$REPO"/resources/themes/*.toml "$CYCLOPS_HOME/themes/"

cat > "$CYCLOPS_HOME/config.toml" <<EOF
# A config with comments and keys the switch must leave alone.
sessions = ["$SESSION"]
tmux_socket = "$SOCK"
tmux_config = "/dev/null"
manifest_dir = "$CYCLOPS_HOME/manifests"
EOF

command tmux -u -f /dev/null -L "$SOCK" new-session -d -s "$SESSION" -x 140 -y 40 cat
sleep 0.5
PANE="$(tmx list-panes -t "$SESSION" -F '#{pane_id}' | head -1)"

"$CYCD" >"$CYCLOPS_HOME/daemon.log" 2>&1 &
DAEMON_PID=$!
sleep 1.5

"$CYC" name "$PANE" reviewer --plain >/dev/null
sleep 1

# The colors the daemon wrote, not the words: tmux expands the text, the
# format carries the styling, and the styling is the whole question here.
border_colors() {
  tmx show-options -p -t "$PANE" -v pane-border-format |
    grep -o '#\[fg=#[0-9a-f]*\]' | tr '\n' ' '
}

echo
echo "== what is there"
"$CYC" theme

echo
echo "== reviewer's border, on the default dark theme"
echo "   $(border_colors)"

echo
echo "== switch"
"$CYC" theme light
sleep 0.5
echo "   border now: $(border_colors)"

echo
echo "== the config kept its comment and its other keys"
cat "$CYCLOPS_HOME/config.toml"

echo
echo "== edit the live file, saved the ordinary way"
echo "   reviewer hashes to role.3, so that is the slot to move; the"
echo "   separator takes surface.dim, so move that too"
sed -i.bak -e 's/#5c6e8a/#c01010/g' -e 's/#6e6e6e/#101010/g' \
  "$CYCLOPS_HOME/themes/light.toml"
rm -f "$CYCLOPS_HOME/themes/light.toml.bak"
# Any daemon event re-stats the file. Nothing is happening on this rig, so
# ask for the same repaint `cyclops theme` asks for.
"$CYC" theme light >/dev/null
sleep 0.5
echo "   border now: $(border_colors)"

echo
echo "== now catch the file mid-save: valid TOML, none of its tokens"
echo "   (F32 in findings.md: about one read in five landing during a save"
echo "   sees exactly this, and none of them sees a syntax error)"
BEFORE="$(border_colors)"
: > "$CYCLOPS_HOME/themes/light.toml"
"$CYC" theme light 2>&1 | sed 's/^/   /' || true
sleep 0.5
echo "   border before: $BEFORE"
echo "   border after:  $(border_colors)"
# The CLI refuses a theme that would render built-in colors, so on this
# path the daemon is never asked and says nothing. That is the outer half
# of the rule. The daemon's half, a file going wrong under a daemon that
# IS asked to reload, is src/cyclopsd/tests/m5_theme.rs, which reads
# the border back off tmux the same way this does.
if grep -q 'theme: ' "$CYCLOPS_HOME/daemon.log"; then
  echo "   what the daemon said about it:"
  grep -o 'theme: .*' "$CYCLOPS_HOME/daemon.log" | tail -1 | sed 's/^/     /'
else
  echo "   the CLI refused it before the daemon was asked; the daemon's own"
  echo "   half of the rule is src/cyclopsd/tests/m5_theme.rs"
fi

echo
echo "== done"
