# commPact shim

The commPact v1 calling surface, served by cyclops. PREPARED here;
installed only by the admin via `install.sh`, which refuses to run unless
`CYCLOPS_CUTOVER_ACK=yes` is set. Runbook:
[docs/development/CUTOVER.md](../../docs/development/CUTOVER.md).

- `commPact`: the shim. Forwards send/read/list/resolve/doctor to cyclops,
  keeps id/hash/version local with v1 behavior, refuses type/keys/message/
  name with a clear error (no v2 equivalent yet).
- `install.sh`: admin-only installer. Moves `~/.commPact/bin/commPact` to
  `commPact.v1.bak` and symlinks the shim in its place; prints rollback.
- `test_shim.py`: self-contained tests. Canned NDJSON daemon on a sandbox
  socket, the real cyclops binary, sandbox HOME; asserts verb mapping,
  refusals, the once-per-day deprecation note, installer guard behavior,
  and that the real `~/.commPact` is untouched (mtime + size snapshot).

Run the tests:

```bash
python3 scripts/commpact-shim/test_shim.py
```

CI runs this suite on ubuntu and macos after the Rust tests (`test` job in
`.github/workflows/ci.yml`); the Rust test gate (nextest and the doctests)
does not cover the shim.
