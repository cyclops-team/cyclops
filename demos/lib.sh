#!/usr/bin/env bash
# The shell home of the two rules that keep leaking when they are copied.
# Source it, do not paste it. Every demo does, and so do tests/m1_soak.py
# and tests/harness/tuikit.py, which reach this file rather than hold a
# python copy of either rule.
#
#   cyc_scratch_root      where throwaway state goes
#   cyc_tmux_teardown     stop an isolated tmux server, completely
#   cyc_stop_daemon       stop the demo's cyclopsd, idempotently
#
# The Rust side of both rules lives in cyclops_proto::scratch and in
# crates/cyclops-testrig. Restated here because bash cannot call them;
# change them together. crates/cyclops-testrig/tests/teardown_has_one_home.rs fails if
# a third home appears anywhere in the tree, and
# crates/cyclops-testrig/tests/shell_teardown.rs holds this file to the
# same teardown contract the Rust home is held to.

# Where a demo puts its throwaway CYCLOPS_HOME.
#
# Short on purpose: a unix socket path caps out near 104 bytes on macOS and
# the system temp dir there is a long /var/folders path, so a daemon socket
# under it fails to bind. /private/tmp is short and macOS-only; elsewhere
# the system temp dir is already short. CYCLOPS_TEST_TMP overrides both,
# and an exported-but-empty value reads as unset (F24).
cyc_scratch_root() {
  if [ -n "${CYCLOPS_TEST_TMP:-}" ]; then
    printf '%s\n' "$CYCLOPS_TEST_TMP"
  elif [ -d /private/tmp ]; then
    printf '%s\n' /private/tmp
  else
    printf '%s\n' "${TMPDIR:-/tmp}"
  fi
}

# Stop the isolated tmux server on socket $1 and remove its socket file.
#
# Stopping a tmux server unlinks nothing, and a server that exits on its
# own leaves the file too (both MEASURED), so a demo that only stopped the
# server left one dead socket in the tmux tmpdir on every run.
#
# In order:
#
# 1. Ask the LIVE server where its socket is. tmux derives that path from
#    TMUX_TMPDIR and the uid by a rule that has moved between versions, so
#    only a server can answer it.
# 2. Nothing left to ask does NOT mean nothing left to remove, and this is
#    the case that kept leaking: the server dies before the trap runs (a
#    crash, an operator's kill, a daemon taking it down) and the file
#    outlives it. One throwaway session brings a server back on the same
#    -L name, and therefore on the same file, so tmux can report the path.
#    `start-server` alone does not do it: tmux 3.6a exits a server with no
#    sessions before the next command reaches it (MEASURED).
# 3. Kill the server, then unlink the path from step 1 or 2.
#
# Safe to call when no server was ever started. Never fails the caller,
# so it can sit in a `set -e` EXIT trap.
cyc_tmux_teardown() {
  local sock="$1" path
  path="$(cyc_tmux_socket_path "$sock")"
  if [ -z "$path" ]; then
    command tmux -u -L "$sock" -f /dev/null \
      new-session -d -s teardown-probe /bin/sh >/dev/null 2>&1 || true
    path="$(cyc_tmux_socket_path "$sock")"
  fi
  command tmux -u -L "$sock" -f /dev/null kill-server 2>/dev/null || true
  if [ -n "$path" ]; then
    rm -f "$path"
  fi
  return 0
}

# Where the server on socket $1 keeps its socket file, asked of the server
# itself. Empty when no server is up.
cyc_tmux_socket_path() {
  command tmux -u -L "$1" -f /dev/null display-message -p '#{socket_path}' 2>/dev/null || true
}

# Stop the demo's cyclopsd and wait for it to go.
#
# Reads and clears the caller's DAEMON_PID, so it is safe to call twice:
# a demo that restarts its daemon mid-run calls it once for the restart
# and again from the EXIT trap, and the second call has nothing to do.
#
# Waiting is the point, not the kill. The daemon puts every pane border
# back on the way down (docs/panes.md), and a trap that killed and moved
# straight on to cyc_tmux_teardown raced that restore against the tmux
# server going away.
cyc_stop_daemon() {
  if [ -n "${DAEMON_PID:-}" ]; then
    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
    DAEMON_PID=""
  fi
}
