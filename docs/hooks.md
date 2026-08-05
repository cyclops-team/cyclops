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

Renders the hook config to `~/.cyclops/hooks/<label>/` and prints
copy-pasteable wiring instructions. `--dry-run` prints without writing,
`--dest <dir>` picks another directory. Cyclops refuses to write into
`~/.claude`, `~/.codex`, `~/.gemini`, or any `.agents` directory: you wire
your real setup once, deliberately. Templates live in `hooks/` in the repo.

Wiring per CLI:

- **claude**: launch with `--settings ~/.cyclops/hooks/<label>/settings.json`,
  or merge the `hooks` object into the settings file you already pass.
- **codex**: codex silently loads zero hooks in an untrusted directory, and
  `--dangerously-bypass-hook-trust` does not fix that. Either copy the
  rendered `hooks.json` to `$CODEX_HOME/hooks.json` (user level loads
  without directory trust), or seed trust in `config.toml`:
  `[projects."<dir>"]` with `trust_level = "trusted"`.
- **agy**: copy the rendered `hooks.json` to `<workspace>/.agents/hooks.json`.
  agy has no payload-matchable ack, so its deliveries stay screen-verified;
  these hooks feed liveness and turn detection.
- **cursor**: copy the rendered `hooks.json` to `<workspace>/.cursor/hooks.json`
  or `~/.cursor/hooks.json` (both load). `CURSOR_CONFIG_DIR` relocates
  `cli-config.json` but not `hooks.json`: a file placed there fires zero
  events, so never wire it that way. Unlike agy, cursor's `beforeSubmitPrompt`
  carries the full prompt text, a payload-matchable ack, so its deliveries
  reach the verified tier.

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
