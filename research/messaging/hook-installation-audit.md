# Hook installation audit

Date: 2026-08-07

## Decision

Cyclops should automate hook wiring, but **the source installer should not
silently edit vendor configuration in the current design**.

The safe sequence is:

1. Make hook identity dynamic so one installed hook can report whichever
   watched pane actually invoked it. Today every generated command hard-codes
   one Cyclops label.
2. Add a separate, explicit `cyclops hooks setup` or `cyclops hooks wire`
   workflow that previews, merges, backs up, verifies, and can uninstall only
   the entries Cyclops owns.
3. Run that workflow after `cyclops start`, when panes have labels and the
   daemon can perform a real self-test. Keep the source installer's default
   behavior unchanged; it may advertise the command or expose an explicit
   opt-in later.
4. Prefer vendor plugin packaging where it gives Cyclops an isolated hook
   file and a vendor-owned trust/uninstall flow. Claude and Codex now both
   support plugin-bundled hooks; Cursor and Antigravity need fresh live probes
   before Cyclops relies on their plugin behavior.

Direct auto-install from `scripts/install.sh` is mechanically possible, but it
would currently guess labels, overwrite shared JSON files, leave orphaned
commands on uninstall, and be unable to prove that the vendor loaded them.

## Scope and evidence

This is a read-only audit of the current installer, hook renderer and receiver,
daemon ingestion, manifests, templates, documentation, and tests. No product or
installer code was changed.

The strongest local evidence is:

- `src/cyclops/src/hookset.rs:1-10,94-153,164-233` — prepare-only contract,
  wiring instructions, vendor-directory refusal, and write behavior.
- `resources/hooks/{claude,codex,agy,cursor}/*` — exact generated commands and
  vendor JSON dialects.
- `src/cyclops/src/hook.rs:28-44,50-84,111-163` — three-second budget, silent
  exit-zero behavior, payload forwarding, sequence file, and error log.
- `src/cyclopsd/src/server.rs:344-370,659-699` — reports must name a label or
  pane, and the socket peer must originate inside that exact pane.
- `src/cyclopsd/src/ack.rs:175-257` — liveness, duplicate suppression, exact
  message-ID ACK matching, and hook-driven state fusion.
- `resources/manifests/claude.toml:14-42`,
  `resources/manifests/codex.toml:14-31`,
  `resources/manifests/agy.toml:9-50`, and
  `resources/manifests/cursor.toml:16-78` — tested versions and hook
  capabilities.
- `scripts/install.sh:175-203,383-389,395-425` — current uninstall, setup-only
  home creation, and handoff to `cyclops start`.
- `tests/e2e/parity-check.sh:69-79,1046-1145` — the two installers must remain
  byte-identical, and installer output/profile restoration are parity gates.
- `src/cyclops/tests/hooks_cli.rs:31-209` — existing install behavior is pinned
  as neutral rendering plus refusal to enter vendor directories.

Current external facts were checked against the official
[Claude hooks reference](https://code.claude.com/docs/en/hooks),
[Claude settings reference](https://code.claude.com/docs/en/settings), and
[Codex hooks reference](https://developers.openai.com/codex/hooks). Cursor's
current public hooks page did not expose usable text to this audit tool, so
Cursor conclusions below rely on the repository's measured manifest and must
be re-probed. A recent official
[Google Antigravity codelab](https://codelabs.developers.google.com/secure-agentic-coding?hl=en)
was used only to identify an Antigravity schema-drift risk, not to replace the
repository's live measurement.

Audit-time version inventory on this machine:

| CLI | Installed here | Manifest tested against |
|---|---:|---:|
| Claude Code | 2.1.224 | 2.1.220 |
| Codex CLI | 0.146.1 | 0.145.0 |
| Cursor Agent | 2026.08.04-aaa8809 | 2026.07.23-e383d2b |
| Antigravity (`agy`) | not installed | 1.1.6 |

The gaps are small for Claude and Codex but large enough that an automated
writer needs version-aware smoke tests. Cursor is already on a later monthly
build than the one whose payload and config-location behavior Cyclops measured.

## What Cyclops does today

The intended flow is correctly split into three different claims:

1. **Prepare:** `cyclops hooks install <cli> --agent <label>` renders a JSON
   artifact under `$CYCLOPS_HOME/hooks/<label>/`.
2. **Wire:** the user copies or merges that artifact into vendor configuration,
   or launches Claude with it.
3. **Prove:** `cyclops hooks verify` shows whether any edge arrived;
   `cyclops hooks selftest` sends a real marker and proves whether the ACK event
   carried that marker.

That distinction is sound. A file on disk is not evidence that a running CLI
loaded it. The daemon only marks hook liveness after an event resolves to the
current pane occupant (`src/cyclopsd/src/ack.rs:175-181`), and only marks a
delivery verified when the configured ACK event's payload field contains the
waiting message ID (`src/cyclopsd/src/ack.rs:197-218`).

The receiver is intentionally harmless to the vendor process: it remains
silent and exits zero even if reporting fails (`src/cyclops/src/hook.rs:38-44`).
However, a command hook can still delay a prompt for up to Cyclops's
three-second budget when the daemon or socket is unhealthy
(`src/cyclops/src/hook.rs:28-36`). Global automatic installation increases the
blast radius from watched Cyclops panes to every session of that vendor CLI.

## Findings

### 1. Static labels block safe global installation

Every template embeds:

```text
<absolute-cyclops-path> hook <event> --agent <label>
```

The receiver requires `--agent` or `CYCLOPS_AGENT`
(`src/cyclops/src/hook.rs:50-54`), the wire type requires `agent`
(`src/cyclops-proto/src/wire.rs:468-481`), and the daemon rejects a report when
the socket peer is not inside the pane that name resolves to
(`src/cyclopsd/src/server.rs:659-699`).

This is secure, but it makes a global hook file awkward:

- A global file containing `--agent reviewer` is wrong in an `implementer`
  pane.
- Installing several label-specific entries makes every matching event spawn
  every entry. Wrong-label reports are denied and written to
  `hook-errors.log`; process cost grows with the number of labels.
- Renaming or replacing a pane leaves a stale command even though hook
  liveness correctly resets for the new occupant.
- The source installer runs `start --setup-only`; it has no running daemon or
  adopted panes from which to learn labels (`scripts/install.sh:383-389`).

The daemon already has the right primitive: it derives sender identity from
Unix-socket peer credentials and process ancestry. Hook reports should use the
same resolved identity rather than trusting or requiring an identity in the
JSON body. An additive wire change can make `agent` optional for new clients,
fill it from the verified peer, and retain the current field for compatibility.

### 2. The printed `cp` commands can destroy existing hook configuration

The command itself refuses vendor directories, but three of its instructions
tell the user to copy a complete `hooks.json` over the vendor's shared file:

- Codex: `cp ... $CODEX_HOME/hooks.json`
  (`src/cyclops/src/hookset.rs:108-120`).
- Antigravity: `cp ... <workspace>/.agents/hooks.json`
  (`src/cyclops/src/hookset.rs:122-129`).
- Cursor: `cp ... ~/.cursor/hooks.json` or the project file
  (`src/cyclops/src/hookset.rs:131-141`).

Those files may already contain unrelated user, team, or security hooks. The
current renderer has no read/merge/backup/uninstall path; `std::fs::write`
also replaces the neutral prepared artifact without an atomic rename
(`src/cyclops/src/hookset.rs:219-230`). The safe boundary in the CLI therefore
pushes the destructive operation into a copy-paste instruction.

This risk is concrete on the audit machine: `~/.codex/hooks.json`,
`~/.claude/settings.json`, and `~/.cursor/hooks.json` all already exist. None
contains a Cyclops hook, while the Claude and Cursor files already contain
their vendors' hook event keys. There is no prepared `~/.cyclops/hooks`
directory. Following the current Codex `cp` instruction would replace the
existing user hook file rather than merge Cyclops into it.

Codex makes this more subtle: all matching hooks from all active layers run;
higher-precedence layers do not replace lower ones. Duplicate registration is
therefore possible even when no file is overwritten. Cyclops dedupes matching
vendor events by sequence and, where available, session/turn/event
(`src/cyclopsd/src/ack.rs:183-195`), but duplicate subprocesses and config
confusion remain.

### 3. Installer-time configuration cannot establish verification

The source installer only has enough context to install binaries and seed a
Cyclops home. It does not know:

- which vendor CLIs the user will run;
- which panes and labels will exist;
- which scope the user wants (global, project, local, or plugin);
- whether an enterprise policy disables non-managed hooks;
- whether Codex has trusted the hook definition;
- whether the running CLI hot-reloaded the change or needs a restart.

Even a syntactically perfect write cannot run `hooks selftest` without a live,
labeled recipient. Reporting "hooks installed" at this point would collapse
configuration and subscription into one misleading success state—the exact
distinction `hookset.rs:8-10` and `docs/reference/hooks.md:1-10` preserve.

### 4. Current uninstall would strand vendor commands

`scripts/install.sh --uninstall` removes the two binaries and the marked PATH
block, then deliberately keeps `$CYCLOPS_HOME` (`scripts/install.sh:175-200`).
Today this is consistent because the installer never edits vendor config.

If it starts wiring hooks, uninstall must either remove only Cyclops-owned
entries or receive explicit consent to leave them. Otherwise every subsequent
vendor event invokes a missing absolute binary. Restoring an entire backup is
also unsafe when the user has edited the file since installation; uninstall
needs a surgical ownership record, not a blind rollback.

### 5. Absolute command paths are robust until the prefix changes

The renderer uses `current_exe` so hooks do not depend on the vendor's `PATH`
(`src/cyclops/src/hookset.rs:155-162`). That is the correct default. A reinstall
at the same prefix atomically replaces the binary and keeps the command valid.

Installing to a new prefix or uninstalling leaves the old absolute path in
vendor config. Codex also trusts an exact hook-definition hash; changing the
command path causes a new trust review. A setup manager needs to detect and
migrate stale Cyclops commands and explain any required re-trust.

### 6. Vendor and enterprise policy drift is a normal state

Hook support is not one common interface:

- vendor JSON shapes differ;
- event names and casing differ;
- config scopes and reload rules differ;
- only some ACK payloads carry prompt text;
- policies may disable user hooks even when files are correct;
- vendor releases can change schema or payloads independently of Cyclops.

This validates the current principle that vendor quirks remain data, but an
automatic writer must be capability- and version-aware. It must fail closed on
unknown JSON or an untested major schema, without damaging the existing file.

### 7. Global hooks broaden the privacy and reliability boundary

The `UserPromptSubmit`/`beforeSubmitPrompt` payload contains the full prompt.
Cyclops forwards the vendor payload through the local socket
(`src/cyclops/src/hook.rs:56-83`) so the daemon can find the message marker.
The payload is not written to the delivery ledger by this path, but installing
the hook globally means Cyclops sees prompt events from non-Cyclops sessions as
well.

An automatic flow should state this before consent, ignore events from
unwatched panes quickly and silently, and never treat an absent daemon as a
persistent error worth growing `hook-errors.log` indefinitely. The installed
handler must stay observation-only: exit zero, emit no stdout, and never return
a vendor decision that could block or modify the user's prompt.

## Vendor-by-vendor feasibility

### Claude Code: feasible after identity and merge work

Current Cyclops capability:

- `UserPromptSubmit` carries `prompt` and provides tier-1 ACK evidence.
- `Stop` is turn end; `Notification` and `PermissionRequest` provide attention
  edges (`resources/hooks/claude/settings.json.tmpl:10-29`).
- Tested version is 2.1.220; 2.1.224 is installed on this machine.

Current official behavior improves the available design space:

- User hooks can live in `~/.claude/settings.json`; project and local files are
  also supported.
- Hook entries merge across settings levels rather than replacing one another.
- Plugin `hooks/hooks.json` is supported.
- Direct settings edits are normally picked up by a file watcher.
- `allowManagedHooksOnly` or strict plugin-only customization can prevent user
  settings hooks from running.

The current Cyclops instructions overstate the need to launch with
`--settings`; that is one non-destructive option, but a surgical merge into
user settings or a plugin is now viable. A plugin is especially attractive
because it avoids rewriting the user's shared settings object and gives the
vendor a recognizable source. It still needs dynamic Cyclops identity.

Uninstall must remove only the exact Cyclops handler from each event, leaving
the user's matcher groups and other handlers intact. Claude has no per-hook
disable flag in JSON: deleting the owned entry is the correct removal.

### Codex CLI: feasible, but trust is an unavoidable user step

Current Cyclops capability:

- `UserPromptSubmit.prompt` supplies a tier-1 ACK and `Stop` supplies turn end
  (`resources/hooks/codex/hooks.json.tmpl:21-35`).
- The daemon already handles duplicate events when user and project layers
  both fire (`src/cyclopsd/src/ack.rs:183-195`).
- Tested version is 0.145.0; 0.146.1 is installed here.

Current official behavior:

- Codex reads user and project `hooks.json`, inline config hooks, and
  plugin-bundled hooks. All matching sources run.
- Project hooks require the project config layer to be trusted; user hooks are
  independent of project trust.
- Every non-managed command definition must be reviewed and trusted by exact
  hash through `/hooks`. A new or changed command is skipped until reviewed.
- `[features] hooks = false` disables hooks; enterprise
  `allow_managed_hooks_only = true` skips user, project, session, and plugin
  hooks.

Therefore an automatic writer may prepare or merge the entry, but it must
report `configured; awaiting Codex trust` rather than `installed`. It should
open with the exact next step (`/hooks`) and self-test only after the user has
trusted it. It must not set project trust or use
`--dangerously-bypass-hook-trust` on the user's behalf.

User scope is the least surprising default once identity is dynamic because it
avoids per-project directory trust. Plugin packaging improves isolation but
does not remove Codex's hook trust review.

### Cursor Agent: plausible, but require a new live probe first

Current measured capability:

- `beforeSubmitPrompt.prompt` is a tier-1 ACK; `stop` is turn end.
- User and project `hooks.json` worked, while `CURSOR_CONFIG_DIR` did not
  relocate hooks (`resources/manifests/cursor.toml:41-78`).
- The flat Cursor JSON shape is not the Claude/Codex nested shape
  (`resources/hooks/cursor/hooks.json.tmpl:1-30`).

The measured build is 2026.07.23; this machine has 2026.08.04. Before
automatic writing, repeat the config-location, event, prompt-field, reload,
duplicate-layer, hook-failure, and uninstall probes on the current CLI. Do not
infer Cursor semantics from Claude or Codex. In particular, Cyclops has not
documented a trust browser, disable policy, or deterministic merge behavior
for the current Cursor CLI.

If the probe holds, a user-level merge is viable after dynamic identity. The
writer must preserve `version`, unrelated events, matcher fields, and unknown
keys, and must never place the file under `CURSOR_CONFIG_DIR`.

### Antigravity (`agy`): automate last, and do not sell it as verification

Current measured capability is only lifecycle support:

- `PreInvocation` fires near turn start but carries no prompt text.
- There is no payload-matchable ACK, so messages remain screen-verified.
- Payloads did not carry their own event name, requiring a distinct command per
  event (`resources/manifests/agy.toml:16-50`).

Automating these hooks can improve liveness and turn-edge accuracy, but it
cannot change `✓ delivered · unverified (screen)` into the verified tier. It is
therefore lower priority than the other three vendors for the stated goal.

There is also a schema-drift warning: Cyclops measured a named-hooks structure
against agy 1.1.6, while a recent official Google Antigravity codelab shows a
different flat `.agents/hooks.json` example with an `enabled` field. This may
be a product/version distinction, but it is enough to prohibit automatic
merging until a current `agy` binary is installed and probed. Project scope
also means the setup command must know the actual workspace; the source
installer does not.

## Design options

| Option | Safety | Ease | Verification | Recommendation |
|---|---|---|---|---|
| Keep prepare-and-copy exactly as-is | High for Cyclops, but printed `cp` may overwrite user config | Low | Manual self-test | Improve immediately; not the end state |
| Installer silently writes global files | Low | Superficially high | Cannot self-test at install time | Reject |
| Installer opt-in writes global files | Medium only after identity, merge, ownership, and policy work | Medium | Deferred until agents run | Possible later, not first implementation |
| Post-start `hooks setup` with preview and self-test | High | High | Immediate, pane-specific proof | Recommended near-term |
| Vendor plugin packages | High isolation; vendor trust still applies | Potentially highest | Self-test still required | Recommended parallel experiment for Claude/Codex |

## Recommended staged approach

### Stage 0: make the current manual path non-destructive

Before adding any automatic write:

1. Replace raw `cp` advice with merge-aware guidance and a warning when the
   destination already exists.
2. Separate prepared artifacts by vendor as well as label, for example
   `$CYCLOPS_HOME/hooks/<vendor>/<label>/...`. Codex, Cursor, and Antigravity
   all currently render `hooks.json` into the same per-label directory
   (`src/cyclops/src/hookset.rs:58-63,182-196`).
3. Add a read-only doctor/plan view that reports active paths, duplicate
   Cyclops entries, stale binary paths, disabled feature/policy state where it
   can be detected, and the exact reload/trust step.
4. Document how a manually wired entry is removed when Cyclops is uninstalled.

### Stage 1: remove static identity from installed hook definitions

Use the kernel-authenticated socket peer as the authority:

1. New hook templates invoke `cyclops hook <event>` without `--agent`.
2. Make `StateReportParams.agent` optional additively for wire compatibility.
3. On `agent.state.report`, resolve the peer's pane from process ancestry and
   assign its current label in the daemon. An explicit legacy agent field must
   still equal that result.
4. Drop reports from unwatched or unlabeled panes quietly. They are expected
   when a global vendor hook is installed; they are not a verified Cyclops
   edge and should not grow an error log.
5. Keep the existing same-UID and inside-the-pane checks. Dynamic identity must
   strengthen the "sender is whoever connected" invariant, not create a
   caller-selected name.

This produces one stable hook definition per vendor instead of one per pane
label, survives rename/restart, and makes global installation tractable.

### Stage 2: add an explicit reversible setup manager

Proposed UX (not currently built):

```text
cyclops hooks setup reviewer --dry-run
cyclops hooks setup reviewer --scope user
cyclops hooks setup --all
cyclops hooks remove codex --scope user
cyclops hooks doctor reviewer
```

The command should:

1. Resolve the live pane and bound manifest; never ask the user to repeat the
   vendor kind Cyclops already knows.
2. Print the target path, scope, events, full command, prompt-payload privacy
   fact, reload/restart requirement, and vendor trust step.
3. Require explicit consent before the first vendor-config write. JSON mode
   must be non-interactive and require an explicit `--apply` equivalent.
4. Refuse invalid JSON, unsupported schema versions, symlink surprises, files
   not owned by the current user, and enterprise policy it cannot override.
5. Preserve unknown keys and every non-Cyclops entry. Add only exact
   Cyclops-owned handlers and dedupe old Cyclops paths.
6. Preserve file mode and ownership, write a same-directory temporary file,
   parse it again, fsync it, then atomically rename it. Create a timestamped
   backup before the first change.
7. Record an installation receipt under `$CYCLOPS_HOME` containing vendor,
   scope, target path, entry fingerprint, prior and resulting hashes, binary
   path, and timestamp. Do not record unrelated configuration values.
8. Make a second run a no-op. If the user edited the Cyclops entry, stop and
   show the diff rather than overwriting it.
9. Tell the user to restart/reload or trust the definition when required, then
   run `hooks selftest`. Report separate states such as `prepared`,
   `configured`, `awaiting trust/restart`, and `verified`.
10. On removal, delete only handlers matching the recorded Cyclops fingerprint.
    If the file changed, perform a surgical removal; never restore the whole
    backup over newer user edits. Keep the backup as recovery evidence.

### Stage 3: integrate with onboarding, not silently with installation

After `cyclops start` has adopted panes, show one actionable summary:

```text
2 agents can use verified delivery; hooks are not active.
Preview safe wiring: cyclops hooks setup --all --dry-run
```

An eventual installer flag such as `--with-hooks` should only schedule or
invoke the explicit setup manager and should explain that verification remains
pending until a live agent trusts/reloads the hook. It must never become the
default for `curl | sh`.

If either installer changes, keep `scripts/install.sh` and
`website/static/install.sh` byte-for-byte identical and run
`./tests/e2e/parity-check.sh --with-installer` as required by the repository.

### Stage 4: evaluate plugin distribution

Build a small proof for Claude and Codex that contains only the lifecycle hook
definitions and calls the stable Cyclops binary path. Evaluate:

- install, enable, trust, update, disable, and uninstall UX;
- whether the vendor exposes a stable plugin command path;
- how absolute Cyclops binary relocation affects trust;
- policy behavior under managed-hook-only environments;
- whether hooks run in subagents and unwanted non-Cyclops sessions;
- whether one plugin package can be versioned independently from Cyclops.

Do not assume the plugin is "installed" until the same marker self-test passes.

## Required tests before shipping automatic writes

### Pure merge and ownership tests

- Empty/missing file, valid unrelated file, malformed JSON, unknown fields,
  unsupported schema, read-only file, symlink target, and interrupted write.
- Existing Cyclops entry at the same path, old binary path, modified entry,
  duplicate entry across layers, and several unrelated hooks in the same
  event.
- Install twice is byte-for-byte stable.
- Remove restores the original file byte-for-byte when no outside edit
  occurred; after outside edits, it removes only the owned entry.
- File permissions and ownership survive install and remove.

### Vendor probes

For each supported version, test user and project scope, reload/restart, full
prompt field, event casing, duplicate layers, hook failure/timeout, disabled
hooks, and uninstall. Record new measured facts with the probe. Antigravity
must also re-establish its JSON schema; Cursor must re-establish current CLI
event coverage.

### Cyclops integration

- Two same-vendor panes with different labels through one global hook file.
- Rename and process replacement without rewriting vendor config.
- Hook invoked from an unwatched pane, admin shell, wrong UID, and stale pane.
- Daemon absent, socket stale, daemon restarting, and ACK arriving while tmux
  is detached.
- `hooks verify` remains unverified after configuration alone and flips only
  after a real edge from the current occupant.
- `hooks selftest` proves the exact message marker; screen-tier vendors remain
  honestly screen-tier.

### Installer and documentation gates

- Existing `hooks_cli` tests must be replaced or extended deliberately; they
  currently require every vendor directory to be refused.
- Installer parity, second-run idempotency, uninstall recovery, profile
  byte-restoration, docs path checks, and parity transcripts must all cover the
  new opt-in output.
- Never run these tests against the user's real home or vendor configuration.

## Bottom line

The highest-leverage improvement is not "copy four JSON files during install."
It is **one dynamically identified, vendor-isolated hook plus a reversible
post-start setup and proof loop**.

That approach keeps the strongest part of the current design—configuration is
not called verified until the agent emits the exact message marker—while
removing the manual wiring and overwrite hazards that make hooks feel brittle
today.
