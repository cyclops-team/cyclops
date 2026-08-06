# commPact v1 cutover

Runbook for retiring the commPact v1 bash CLI and serving its verbs through
cyclops. Everything here is PREPARED in this repo and tested; nothing is
installed until the admin runs the guarded installer. The shim lives in
`scripts/commpact-shim/`.

## What the shim does

`scripts/commpact-shim/commPact` keeps the v1 calling surface working:

| v1 call | Served by |
|---|---|
| `send <t> --json --subject S --body-file -` | `cyclops send <t> --subject S --body-file - --json` |
| `read <t> [lines]` | `cyclops read <t> --source recent --lines N` (default 50, like v1) |
| `list` | `cyclops status` |
| `resolve <label>` | pane id from `cyclops read --json` |
| `doctor` | `cyclops ping` + `cyclops status` |
| `id`, `hash`, `version` | local, v1 behavior kept |
| `type`, `keys`, `message`, `name` | refused with a clear error (no v2 equivalent yet) |

Send flags `--max-bytes`, `--lock-timeout-ms`, and `--verify marker-hash`
are accepted and ignored with a note: the daemon owns pacing and
verification now. `--no-submit`, `--allow-label`, and `--expect-label` are
refused. A one-line deprecation note prints to stderr once per day per user
(stamp under `$CYCLOPS_HOME`).

## Known differences

- Receipts: v1's `{"status":"SUBMITTED",...}` becomes the cyclops receipt
  (`msg_id`, `deliveries[].state`). Exit 0 still means delivered or
  queued; 1 means parked or needs attention; 2 means usage error.
- The recipient sees one `[cyclops m-...] FROM: x  SUBJECT: y` header line
  instead of v1's `SUBJECT:` and `FROM:` lines. Both fields are still
  there; replying to the FROM label works unchanged.
- v1 stripped leading `SUBJECT:`/`FROM:`/`PANE:` lines from bodies;
  cyclops does not (the envelope cannot be forged; see
  [DELIVERY.md](DELIVERY.md)).
- `COMMPACT_SOCKET` is not consulted; the daemon owns the tmux connection.

## Preconditions (all must hold before install)

- [ ] Current core gates pass, including clippy and
      `cargo test --workspace --no-fail-fast`.
- [ ] Shim tests green: `python3 scripts/commpact-shim/test_shim.py`.
- [ ] `cyclopsd` running against the real session; `cyclops status` shows
      every pane you intend to preserve with a sane state.
- [ ] Panes adopted: each label resolves and receives a test send.
- [ ] Hooks wired per pane by the admin; a test send to each agent comes
      back `delivered · verified` (hook ACK), not just screen-tier.

## Install (ADMIN ONLY)

```bash
cd /path/to/cyclops
CYCLOPS_CUTOVER_ACK=yes scripts/commpact-shim/install.sh
```

The installer refuses without the ack variable. It moves
`~/.commPact/bin/commPact` to `commPact.v1.bak`, symlinks the shim in its
place, and prints the rollback. It refuses if a backup already exists or
the target is already a foreign symlink.

## Parallel window

v1 stays intact during the window: only `bin/commPact` becomes a symlink.
`commPact.v1.bak` is the byte-identical v1 binary, the other
`~/.commPact/bin/*` helpers and all v1 config and docs are untouched, and
rollback is two commands. Every shimmed send lands in the cyclops ledger,
so the whole window is auditable afterward.

## Verification checklist (COORDINATION.md patterns via the shim)

- [ ] Routine message, the exact protocol line:
      `printf 'Body text only' | ~/.commPact/bin/commPact send claude --json --subject 'Review request' --body-file -`
      returns a delivered receipt and the claude pane shows the
      `[cyclops m-...]` header with FROM and SUBJECT.
- [ ] Reply flow: the recipient replies to the FROM label and the reply
      arrives as a structured message, no polling.
- [ ] Assignment format (REQUEST/CONTEXT/DONE WHEN/STOP IF) and completion
      format pass through bodies unchanged.
- [ ] Status inspection: `~/.commPact/bin/commPact read codex 50` prints
      the pane tail.
- [ ] Unshimmed verb declines cleanly: `commPact type claude hi` exits 2
      and points at cyclops send.
- [ ] Deprecation note appeared once on stderr, silent on the next call.
- [ ] The ledger recorded each message above:
      `jq 'select(.kind=="msg")' ~/.cyclops/ledger/<session>.ndjson`.

## Rollback

```bash
rm ~/.commPact/bin/commPact
mv ~/.commPact/bin/commPact.v1.bak ~/.commPact/bin/commPact
```

v1 works again immediately. Nothing else changed during the window, so
there is nothing else to undo.

## Only the admin may

- run `install.sh` (or set `CYCLOPS_CUTOVER_ACK`)
- modify anything under `~/.commPact`, including deleting the backup when
  the window ends
- wire hook configs into vendor dirs (`~/.claude`, `~/.codex`, `~/.agents`)
- run `cyclopsd` against the live session
- update `COORDINATION.md` to teach the native cyclops verbs
- roll back

ADMIN_ACTION_REQUIRED: the cutover itself. Agents prepared and tested the
shim but must never install it. Admin decides when, runs the installer,
walks the checklist above, and owns rollback.
