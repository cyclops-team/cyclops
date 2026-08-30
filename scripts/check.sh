#!/usr/bin/env bash
# The five CI gates, one command, cheapest first.
#
# `./scripts/check.sh` is the full pre-push pass; `./scripts/check.sh
# --fast` stops after the compile-and-test gate for the inner loop. The
# ordering is the point: fmt and clippy answer in seconds and catch most
# of what a red CI run would have said minutes later, so they run before
# the test suite instead of after it. Every command here is the same one
# CI runs (AGENTS.md, CONTRIBUTING.md); this script adds sequencing and
# timing, never different flags.
#
# What this cannot shortcut: the tests themselves. `cargo test -p
# <crate>` while iterating on one crate is the real inner loop; this
# script is for the moment before a push, when the whole tree has to
# answer.

set -e
cd "$(dirname "$0")/.."

fast=0
[ "${1:-}" = "--fast" ] && fast=1

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
    cargo nextest run --workspace \
        -E 'not (package(cyclopsd) | binary_id(=cyclops-ui::perf) | binary_id(=cyclops-ui::queue_perf) | binary_id(=cyclops-workspace::perf_contract))' \
        --no-fail-fast || status=$?
    cargo test -p cyclopsd --all-targets --no-fail-fast || status=$?
    cargo doc --workspace --no-deps || status=$?
    return "$status"
}

stage "messaging docs" ./tests/e2e/messaging-docs-parity.sh
stage "fmt" cargo fmt --all --check
stage "clippy" cargo clippy --workspace --all-targets -- -D warnings
stage "doc paths" python3 scripts/check-doc-paths.py
stage "test" rust_tests

if [ "$fast" = "1" ]; then
    printf '== fast pass done (skipped: parity)\n'
    exit 0
fi

stage "parity" ./tests/e2e/parity-check.sh
printf '== all gates green\n'
