## What changed and why

<!-- One or two sentences. Link an issue if there is one. -->

## Testing

- [ ] Behavior fix: added a test that fails before this change and passes after
- [ ] Docs updated in this commit, if this changes output or behavior a doc quotes

## Gates

Ran the five checks from `CONTRIBUTING.md`, in order:

- [ ] `cargo fmt --all`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace --no-fail-fast`
- [ ] `python3 scripts/check-doc-paths.py`
- [ ] `./tests/e2e/parity-check.sh`

## Scope

- [ ] No unrelated changes bundled in
