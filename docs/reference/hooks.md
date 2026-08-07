# Hooks

Vendor hooks are how a delivery earns `✔ delivered · verified`: the
recipient CLI's own hook reports the injected text back to the daemon.
Configuration does not equal subscription: a rendered config proves
nothing until an edge actually arrives, so cyclops splits the job into
prepare, wire, prove.

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
  payload-matchable ack, so its deliveries stay screen-verified; these hooks
  feed liveness and turn detection.
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

## Verify (did edges ever arrive?)

```
cyclops hooks verify reviewer
```

Prints the pane's ack tier and the last-seen age of every hook event.
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
ingested, so neither the `hooks verified` bit nor a `delivered · verified`
receipt can be forged by a process that merely shares your user id.

## Selftest (prove the round trip)

```
cyclops hooks selftest reviewer
```

The daemon sends one fyi message through the normal delivery pipeline
(subject `[cyclops] hook self-test`, body "Reply not needed.") and reports
whether the ack hook fired carrying the marker. Costs the recipient one
trivial turn; the result is also recorded in the ledger. Exit 0 when the
ack hook fired, 1 otherwise (always 1 on a screen-tier CLI like agy: there
is no ack hook to fire, the delivery state is the whole answer).

## When hooks never fire

Deliveries still land: the tier-1 ack window times out, the delivery
downgrades to screen evidence (`✓ delivered · unverified (screen)`), and the first
such delivery on a pane whose occupant has zero edges sends the admin one
`action_required` notification naming the likely cause. For codex that is
almost always the directory-trust trap above. One ping per pane occupant
(a restarted CLI without hooks earns its own), never a loop, nothing
lost.
