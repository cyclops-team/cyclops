# Follow-ups from the 2026-08-07 bug-fix session

Work order for a fresh session. Each item is self-contained: what is known
(with evidence), what to change or how to investigate, and what done looks
like. Context: the 2026-08-07 session fixed four live-tested bugs (event
stream duplicate wall, event panel close/resize, keybinds dialog height,
wheel-scroll forwarding); these are the items it deliberately did not
attempt. The mechanism behind the biggest of those fixes is written up in
the session memory (`zombie-watcher-storm`) and matters here: items 1 and 2
live in the same watcher.

Baseline: the four fixes are committed on branch `fix/live-testing-bugs`
(five commits, `d535899..c1d3d88`, including this document), which is NOT
merged to `main` yet — start from that branch. All suites green there,
`tests/e2e/parity-check.sh` 115/115. The binaries in `~/.local/bin`
predate these fixes until someone installs (stop the daemon, `rm`, then
`cp` from `target/release/` — never `cp` over the running binary's inode).

Repo-wide constraints that bite in exactly this area, learned the hard way:

- `src/cyclops-proto/tests/one_place.rs` scans ALL repo file text — code,
  comments, tests. Never write an agent-state word as a string literal or
  enumerate the blocked states; use the `AgentState` enum, `is_blocked()`,
  `.to_string()`. Only full-workspace test runs execute it, so a violation
  in a crate-scoped run surfaces later, on someone else's build.
- `src/cyclops-theme/tests/vocabulary.rs` scans `.rs` text including
  comments; don't name theme token paths in prose.
- Every `cargo test --workspace` run leaks one `target/debug/cyclopsd` and
  one `/private/tmp/tmux-501/cyc-ws-inside-<pid>` socket. Sweep after.
- Never `cp` over a running binary in `~/.local/bin` (macOS rewrites the
  inode, invalidating the signature; new launches SIGKILL). Stop → `rm` →
  `cp`.

---

## 1. Watcher should follow a session rename (feature, needs a small design)

**Today's behavior** (after the 2026-08-07 fix): when a watched session is
renamed — folder-following workspaces rename sessions as a matter of course
(`src/cyclops-workspace/src/naming.rs`) — the watcher's reconcile fails,
the `has-session` probe reports the old name gone, the watcher tears down
cleanly, and the daemon's reattach loop waits for the OLD name with backoff
(`src/cyclopsd/src/lib.rs` session loop, "waiting for session"). Safe and
quiet, but the renamed session's panes stop being watched until something
re-registers the new name (reopening the workspace does, via the
`session.watch` RPC).

**Why it is not a quick change.** Session identity is the NAME everywhere
on the daemon side:

- `SessionSlot { name, .. }` (`src/cyclopsd/src/lib.rs:181`), looked up by
  `session_index(name)` (`lib.rs:227`).
- Runtime registration dedups by name: `watch_session` (`lib.rs:1311`)
  returns the existing slot only if `session_index(name)` hits; a rename
  followed by the workspace registering the new name would create a
  SECOND slot + watcher for the same tmux session (duplicate events).
- The ledger file is `ledger/<name>.ndjson` (`lib.rs:1318`). Note: the
  `LedgerWriter` holds an open handle, so appends survive a rename of the
  session; the problem is lookups and future opens, not the live file.
- `emit_state` and friends resolve `watcher.session()` → `session_index`
  (`lib.rs:245-296`); if only the watcher followed the rename, every
  ledger append would silently miss (seq `None`) — worse than today.
- `~/.cyclops/config.toml` `sessions = [...]` persists names.

**What the watcher already has.** The `%session-renamed` notification
carries the session id `$n` plus the new name and is parsed
(`src/cyclops-tmux/src/notify.rs:186`), but `handle_notification` treats it
as a bare reconcile hint (`src/cyclops-tmux/src/watcher.rs:464`). The
snapshot module already reads `#{session_id}` formats
(`src/cyclops-tmux/src/snapshot.rs:104,114`), so resolving and storing the
watcher's own `$id` at connect is established practice.

**Suggested shape** (a fresh session should sanity-check this, not treat it
as settled): resolve and store the session's `$id` in the watcher at
connect; on `SessionRenamed` whose `$id` matches, update the watcher's
internal name (what `list-panes -s -t` targets) and surface a new
`PaneEvent` upward; the daemon then renames its slot in place —
`SessionSlot.name` becomes mutable or slots get keyed by `$id` — so
`session_index(new_name)` hits the existing slot and `watch_session` from
the workspace dedups instead of duplicating. Decide explicitly what the
ledger does on rename (keep writing the open file and only open
`<new-name>.ndjson` on the next daemon boot is the minimal answer; document
whichever is chosen) and whether the config's `sessions` list is rewritten.
Watch for the name-swap edge (two sessions exchanging names) — keying by
`$id` internally is what makes that survivable.

**Done means:** rename a watched session with live panes; states keep
flowing under the new name with no gap, no duplicate slot after the
workspace re-registers, ledger appends never silently drop, and the
existing zombie test (`src/cyclops-tmux/tests/watcher_zombie_session.rs`)
still passes (a rename of a DIFFERENT session must still not resurrect a
dead watcher).

## 2. `malformed subscription value` warnings: tmux pushes truncated expansions (investigation)

**Evidence** (from `~/.cyclops/cyclopsd.log`, ~30 occurrences over
2026-08-05..07): per-pane subscription pushes arrive with FEWER than the 5
tab-separated fields `SUB_FORMAT` defines
(`src/cyclops-tmux/src/watcher.rs:45`:
title, dead, in_mode, current_command, pid). Observed values truncate
cleanly at field boundaries and to varying depth: 4 fields
(`✳ Claude Code\t0\t0\t`), 3 (`cyclops\t0\t0`), 2
(`Brioss-MacBook-Pro-3.local\t`), and 1 (empty). The warn fires in
`apply_sub_value` (`watcher.rs:524-532`).

**Ruled out already — do not re-derive:** the parser is NOT the bug. It
parses fixed fields from the right with the free-text title as remainder
(`rsplitn(5, '\t')`), so tab-bearing titles are handled; the notification
framing (`parse_subscription_changed`, `src/cyclops-tmux/src/notify.rs:300`)
splits header from value on the first `" : "`, which a title cannot fake.
The truncation exists in what tmux sends, or in how the line reaches the
parser.

**Impact:** low — the handler returns `Action::Hint` on the malformed
value, so a reconcile refreshes the same fields; the cost is log noise and
a slightly later update. That is why this is an investigation, not an
emergency.

**How to investigate** (this repo's discipline: measure, then write an
F-entry in `findings.md` — numbering continues from the top-of-file index):
isolated tmux server (`tmux -L scratch`), attach a raw control client
(`tmux -u -C -L scratch attach -t <s>`) with the same subscription
(`refresh-client -B 'cyp0:%0:<SUB_FORMAT>'`), then drive the suspects while
logging raw `%subscription-changed` lines: pane birth (subscribe racing the
process spawn — pid/cmd not yet available), pane death mid-push, title
changes carrying multibyte glyphs (`✳` appears in most occurrences), and
both tmux versions the corpus cares about (3.6a vs next-3.8 — findings
F13/F25 establish they already differ on subscription behavior). The first
question to answer: are the missing fields absent in tmux's own output, or
eaten in transit (locale sanitization is F14 territory — the daemon passes
`-u`, but confirm the failing environment did).

**Done means:** an F-entry stating the measured trigger, plus whatever
minimal code change follows (possibly none beyond demoting the warn for a
known-benign shape, or padding defaults for a known-partial push — decided
by the measurement, not guessed).

## 3. Two timing-flaky tests under parallel load (test hygiene)

Both pre-existing, both pass standalone and on re-runs; they fail only
under full-suite parallel load. Fix properly or serialize them — do not
widen tolerances to make them quiet.

- `flow_control_pause_and_resume`
  (`src/cyclops-workspace/tests/perf_contract.rs`): races tmux's
  `pause-after` clock against a real ~2s sleep. Failed once in each of two
  different agents' full-suite runs on 2026-08-07. Related history: commit
  `6da553f` ("fix(tmux): confirm flow-control resume").
- `split_opens_in_the_source_panes_directory_not_the_sessions`
  (`src/cyclops-tmux/src/ops.rs` tests): failed once with tmux "index 0 in
  use" — a window-index collision, i.e. two parallel tests driving the same
  scratch server, or index reuse racing `kill-pane`.

**Done means:** ten consecutive `cargo test --workspace` runs with no
failure from either test (run from OUTSIDE tmux, or note that the e2e
suite scrubs `TMUX`/`TMUX_PANE` itself as of 2026-08-07 — see
`run_cyclops_io` in `src/cyclops/tests/e2e.rs:82`).
