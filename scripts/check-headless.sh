#!/usr/bin/env bash
# Prove that command-line Cyclops does not regain either interactive UI by
# Cargo feature unification, then exercise one durable contract per retained
# headless command family.

set -euo pipefail
cd "$(dirname "$0")/.."

client_graph=$(cargo tree -p cyclops --no-default-features -e features)
if printf '%s\n' "$client_graph" | grep -E 'cyclops-workspace|ratatui|crossterm|alacritty[_-]terminal|cyclops-ui feature "(presentation|watch)"'; then
    echo "headless cyclops pulled an interactive UI dependency" >&2
    exit 1
fi

daemon_graph=$(cargo tree -p cyclopsd -e features)
if ! printf '%s\n' "$daemon_graph" | grep -F 'cyclops-ui feature "presentation"' >/dev/null; then
    echo "cyclopsd restart evidence lost the reusable presentation model" >&2
    exit 1
fi
if printf '%s\n' "$daemon_graph" | grep -E 'cyclops-workspace|ratatui|crossterm|alacritty[_-]terminal|cyclops-ui feature "watch"'; then
    echo "cyclopsd test evidence pulled the interactive watch implementation" >&2
    exit 1
fi

cargo clippy -p cyclops --no-default-features --all-targets -- -D warnings
cargo test -p cyclops --no-default-features --test headless_build

for contract in \
    status_json_prints_the_raw_result \
    daemon_not_running_copy_and_exit_code \
    daemon_restart_without_a_daemon_says_so \
    watch_json_streams_events_then_reports_the_close \
    inbox_next_subscribes_before_listing_and_claims_after_one_event \
    send_happy_path_stdin_body_delivers_verified \
    default_reply_accepts_a_future_receipt_state_with_the_same_plain_warning
do
    cargo test -p cyclops --no-default-features --test e2e "$contract" -- --exact
done

cargo test -p cyclops --no-default-features --test health_cli \
    health_works_with_no_daemon_and_does_not_create_state -- --exact
cargo test -p cyclopsd --test restart_eye

echo "headless build boundary and retained command contracts passed"
