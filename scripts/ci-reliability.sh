#!/usr/bin/env bash
# Scheduled race, forced-cleanup, and long-history evidence. Every repetition
# observes an exact outcome; there is no timing sleep between attempts.

set -e
cd "$(dirname "$0")/.."

repeat="${CYCLOPS_CI_REPEAT:-10}"
case "$repeat" in
  ''|*[!0-9]*|0) echo "CYCLOPS_CI_REPEAT must be a positive integer" >&2; exit 2 ;;
esac

i=1
while [ "$i" -le "$repeat" ]; do
  cargo test -p cyclopsd --test messaging_coordinator \
    a_visible_human_draft_cleared_by_backspace_releases_the_same_attempt -- --exact
  cargo test -p cyclops-testrig --test interrupted_owner \
    killing_an_owner_removes_only_its_exact_tmux_resources -- --exact
  i=$((i + 1))
done

cargo test -p cyclopsd --lib \
  mailbox::tests::ten_thousand_message_snapshot_uses_the_mailbox_lookup_index -- --exact
cargo test -p cyclopsd --lib \
  mailbox::tests::follow_pages_every_settled_message_beyond_the_snapshot_tail -- --exact
cargo test -p cyclopsd --test stage_and_clear_soak -- --nocapture
