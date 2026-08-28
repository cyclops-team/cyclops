## What changed and why

<!-- One or two sentences. Link an issue if there is one. -->

## Reliability roadmap authority

<!-- Required for reliability implementation PRs. Delete this section only
     when the change is outside the reliability release. -->

- Baseline commit: `ab0ccd98576445c32035d60d5e547c235fc1c8b2`
- Roadmap SHA-256: `efa7b2f88eac7c25868a5b48aa354709d7fdc499b586a74085aba43cd1c79126`

## Testing

- [ ] Behavior fix: added a test that fails before this change and passes after
- [ ] Docs updated in this commit, if this changes output or behavior a doc quotes

## Gates

Ran the five checks from `CONTRIBUTING.md`, in order:

- [ ] `cargo fmt --all`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo nextest run --workspace -E 'not package(cyclopsd)' --no-fail-fast`, `cargo test -p cyclopsd --all-targets --no-fail-fast`, and `cargo test --workspace --doc`
- [ ] `python3 scripts/check-doc-paths.py`
- [ ] `./tests/e2e/parity-check.sh`

## Scope

- [ ] No unrelated changes bundled in
