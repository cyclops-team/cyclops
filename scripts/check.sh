#!/usr/bin/env bash
# The complete required CI gate, one command, cheapest first.
#
# `./scripts/check.sh` is the full pre-push pass; `./scripts/check.sh
# --fast` stops after the compile-and-test gate for the inner loop;
# `./scripts/check.sh --quick` stops after formatting, clippy, and the
# pure unit tests, a one-to-two-minute sanity pass for the middle of
# editing. The ordering is the point: fmt and clippy answer in seconds and
# catch most of what a red CI run would have said minutes later, so they
# run before the test suite instead of after it. Every command here is the
# same one CI runs (AGENTS.md, CONTRIBUTING.md); this script adds
# sequencing and timing, never different flags.
#
# What this cannot shortcut: the tests themselves. `cargo test -p
# <crate>` while iterating on one crate is the real inner loop; this
# script is for the moment before a push, when the whole tree has to
# answer. `--quick` is a coarser version of that same inner loop across
# the whole workspace: it catches lint and unit-test regressions fast, not
# the tmux-backed and daemon integration contracts `--fast` still runs.

set -e
cd "$(dirname "$0")/.."

mode=full
case "${1:-}" in
    --fast)  mode=fast ;;
    --quick) mode=quick ;;
esac

stage() {
    name=$1
    shift
    printf '== %s\n' "$name"
    start=$(date +%s)
    "$@"
    printf '   %ss\n' "$(( $(date +%s) - start ))"
}

# All parts of the Rust gate run even when an earlier part fails, so one pass
# reports every failure; the stage fails if any did. Same as CI. Performance
# executables belong to the retained scheduled and release evidence lanes.
rust_tests() {
    status=0
    # workspace_cli's real-daemon start assertion needs its sibling binary.
    # workspace_boot_sizing's sizing assertion does not require a daemon and
    # tolerates daemon-start failure.
    cargo build -p cyclops -p cyclopsd --bins || status=$?
    cargo nextest run --workspace \
        -E 'not (package(cyclopsd) | binary_id(=cyclops-ui::perf) | binary_id(=cyclops-ui::queue_perf) | binary_id(=cyclops-workspace::perf_contract))' \
        --no-fail-fast || status=$?
    cargo test -p cyclopsd --all-targets --no-fail-fast || status=$?
    cargo doc --workspace --no-deps || status=$?
    return "$status"
}

# Every integration-test binary, including the three retained performance
# ones, is a separate `kind(test)` target discovered from a crate's tests/
# directory; this filter selects only the unit tests compiled into each
# crate's own `kind(lib)` or `kind(bin)` target, so nothing tmux-backed or
# daemon-backed runs here. That is what keeps this tier under two minutes.
if [ "$mode" = "quick" ]; then
    stage "fmt" cargo fmt --all --check
    stage "clippy" cargo clippy --workspace --all-targets -- -D warnings
    stage "unit tests" cargo nextest run --workspace -E 'kind(lib) | kind(bin)' --no-fail-fast
    printf '== quick pass done (skipped: messaging docs, installer parity, headless build, doc paths, integration tests, cargo doc, parity)\n'
    exit 0
fi

stage "messaging docs" ./tests/e2e/messaging-docs-parity.sh
stage "fmt" cargo fmt --all --check
stage "clippy" cargo clippy --workspace --all-targets -- -D warnings
stage "installer parity" cmp -s scripts/install.sh website/static/install.sh
stage "headless build" ./scripts/check-headless.sh
stage "doc paths" python3 scripts/check-doc-paths.py
stage "test" rust_tests

if [ "$mode" = "fast" ]; then
    printf '== fast pass done (skipped: parity)\n'
    exit 0
fi

stage "parity" ./tests/e2e/parity-check.sh
printf '== all gates green\n'
