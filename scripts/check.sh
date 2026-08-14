#!/bin/sh
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

stage "fmt" cargo fmt --all --check
stage "clippy" cargo clippy --workspace --all-targets -- -D warnings
stage "doc paths" python3 scripts/check-doc-paths.py
stage "test" cargo test --workspace --no-fail-fast

if [ "$fast" = "1" ]; then
    printf '== fast pass done (skipped: shim, parity)\n'
    exit 0
fi

stage "v1 shim" python3 scripts/commpact-shim/test_shim.py
stage "parity" ./tests/e2e/parity-check.sh
printf '== all gates green\n'
