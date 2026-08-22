# Hooks

Vendor hooks report authenticated lifecycle edges. Standard mailbox messaging
uses them with pane detection to decide whether a content-free notification is
safe to write. The legacy direct-delivery self-test also uses an acknowledgement
hook to prove that its injected test payload arrived. A rendered config proves
nothing until an edge actually arrives, so Cyclops splits the job into prepare,
wire, and prove.

## Install (prepare; vendor config is never touched)

```
cyclops hooks install claude --agent reviewer
```

Renders a neutral artifact and prints wiring instructions. The default paths
are vendor-isolated, so preparing one vendor cannot replace another vendor's
same-named file:

```
$CYCLOPS_HOME/hooks/claude/<label>/settings.json
$CYCLOPS_HOME/hooks/codex/<label>/hooks.json
$CYCLOPS_HOME/hooks/agy/<label>/hooks.json
$CYCLOPS_HOME/hooks/cursor/<label>/hooks.json
```

For example, `cyclops hooks install codex --agent reviewer` prepares
`$CYCLOPS_HOME/hooks/codex/reviewer/hooks.json`. `--dry-run` prints without
writing, and `--dest <dir>` remains an explicit directory: the selected
vendor file is written directly inside that directory. Cyclops refuses to
write into `~/.claude`, `~/.codex`, `~/.gemini`, or any `.agents` or `.cursor`
directory: you wire your real setup once, deliberately. The prepare uses a
same-directory temporary file and atomic rename, so an interrupted write
cannot leave partial JSON. Templates live in `resources/hooks/` in the repo.

Wiring per CLI:

- **claude**: launch with `--settings
  $CYCLOPS_HOME/hooks/claude/<label>/settings.json`, or merge its `hooks`
  object into the settings file you already pass. Preserve every unrelated
  setting and handler; do not replace an existing shared file.
- **codex**: codex silently loads zero hooks in an untrusted directory, and
  `--dangerously-bypass-hook-trust` does not fix that. If
  `$CODEX_HOME/hooks.json` does not exist, copy the prepared artifact there.
  If it exists, merge only Cyclops' event entries and preserve every
  unrelated key and handler; never overwrite it. After merging, open Codex's
  `/hooks` and review and trust the exact Cyclops command definition; new or
  changed commands are skipped until that exact definition is trusted. For
  project-local hooks, also trust the project config layer in
  `$CODEX_HOME/config.toml`: `[projects."<dir>"]` with
  `trust_level = "trusted"`. Reload behavior depends on the Codex version; if
  the running process does not pick up the merged file or trust decision,
  restart or reload Codex, then run the selftest.
- **agy**: if `<workspace>/.agents/hooks.json` does not exist, copy the
  rendered artifact there. If it exists, merge only Cyclops' event entries,
  preserving every unrelated key and handler; never overwrite it. agy has no
  payload-matchable acknowledgement, so its legacy self-test stays
  screen-verified; these hooks feed liveness and turn detection.
- **cursor**: if `<workspace>/.cursor/hooks.json` or `~/.cursor/hooks.json`
  does not exist, copy the rendered artifact there. If it exists, merge only
  Cyclops' event entries, preserving every unrelated key and handler; never
  overwrite it. `CURSOR_CONFIG_DIR` relocates `cli-config.json` but not
  `hooks.json`: a file placed there fires zero events, so never wire it that
  way. Unlike agy, cursor's `beforeSubmitPrompt` carries the full prompt text,
  a payload-matchable ack, so its deliveries reach the verified tier.

For every vendor, a missing destination can receive the prepared file. An
existing destination must be merged by hand so existing handlers and unrelated
configuration survive. Configuration alone is not proof: finish by running
`cyclops hooks verify <label>` or `cyclops hooks selftest <label>`. If a vendor
reload is needed, run the selftest after reloading or restarting it.

## Install-time wiring

`scripts/install.sh` ends with `cyclops start --setup-only --wire-hooks`,
the one opt-in that lets cyclops do the wiring above itself: it merges
cyclops' hook entries into the config each installed vendor CLI reads on
its own (`$CODEX_HOME/hooks.json`, `~/.agents/hooks.json`,
`~/.cursor/hooks.json`). It also seeds the same agent skill at each canonical
destination: `~/.claude/skills/cyclops/SKILL.md`, one shared
`~/.agents/skills/cyclops/SKILL.md` for Codex and Cursor, and
`~/.gemini/antigravity-cli/skills/cyclops/SKILL.md` for AGY. It never creates
duplicate vendor copies. A vendor directory that does not exist is never
created, your own entries are merged around rather than replaced, and the
original file is copied aside before the first edit.

The consent is recorded at `~/.cyclops/vendor-wiring-consented`, so an
agent CLI installed after cyclops is not stranded: the next `cyclops` or
`cyclops start` finds it, wires it the same way, and prints one line
saying so: silence means nothing needed writing. Delete the marker to
withdraw the consent; `CYCLOPS_NO_VENDOR_HOOKS=1` declines the whole
step, at install time and after.

## Verify (did edges ever arrive?)

```
cyclops hooks verify reviewer
```

Prints the pane's legacy acknowledgement tier and the last-seen age of every
hook event.
`cyclops status` carries the same bit: `hooks unverified` marks an adopted
pane whose configured hooks have never fired this daemon run. Exit 1 while
unverified, so scripts can gate on it.

Liveness belongs to the pane's current occupant, not the pane: edges are
recorded against the process that produced them, so restarting the CLI in
a pane reverts it to `hooks unverified` until the new process fires an
edge. A predecessor's edges never vouch for its replacement. An unlabeled
pane can show declared hooks without a verdict: hook tracking starts when
the pane has a label.

## Only the pane can report

`agent.state.report` over the socket is accepted only from a process
inside the very pane it reports for, verified against the connection's
kernel peer credentials the same way send identity is. Real hooks pass by
construction: `cyclops hook` runs as a child of the vendor CLI inside the
pane. Anything else, the admin shell included, is denied and nothing is
ingested, so neither the `hooks verified` bit nor a legacy
`delivered · verified` self-test receipt can be forged by a process that merely
shares your user id.

The daemon resolves that process to the exact watched session, pane id, and
current process generation. A client-provided agent label is optional and is
never authority. Reusing a pane id in another tmux session or replacing the
occupant cannot inherit hook liveness from the original route.

## Selftest (prove the round trip)

```
cyclops hooks selftest reviewer
```

The daemon sends one fyi message through the legacy direct-delivery pipeline
(subject `[cyclops] hook self-test`, body "Reply not needed.") and reports
whether the ack hook fired carrying the marker. Costs the recipient one
trivial turn; the result is also recorded in the ledger. Exit 0 when the
ack hook fired, 1 otherwise (always 1 on a screen-tier CLI like agy: there
is no ack hook to fire, the delivery state is the whole answer).

## When hooks never fire

The legacy self-test can still land without an acknowledgement. Its tier-1
window times out and the result downgrades to screen evidence
(`✓ delivered · unverified (screen)`). This does not describe standard
`cyclops send`, which accepts a mailbox message first and reports notification
state separately.
