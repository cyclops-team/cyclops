#!/usr/bin/env bash
# Focused F24 evidence under a caller-selected scratch root.
#
# The full workspace does not run twice for one path property. The override
# unit test proves the selector, the syntactic lint rejects bypasses in real
# tmux fixtures, and M0 crosses the real tmux, daemon-socket, and scratch-home
# seams under the relocated root.
set -euo pipefail

if [ -z "${CYCLOPS_TEST_TMP:-}" ]; then
  echo "CYCLOPS_TEST_TMP must name the relocated scratch root" >&2
  exit 2
fi

mkdir -p "$CYCLOPS_TEST_TMP"
cargo test -p cyclopsd --test scratch_override \
  the_override_moves_the_scratch_root_and_everything_built_on_it -- --exact
cargo test -p cyclops-testrig --test scratch_path_lint \
  real_tmux_fixtures_do_not_bypass_the_scratch_root -- --exact
cargo test -p cyclopsd --test m0 m0_shadow_daemon_end_to_end -- --exact
