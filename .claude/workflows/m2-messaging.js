export const meta = {
  name: 'm2-messaging',
  description: 'M2: history/thread/broadcast polish, agent.wait + send-and-wait, hooks install, startup hook self-test, v1 shim PREPARED (not installed)',
  whenToUse: 'Launch after M1 is integrated, soaked, and committed. Ends with an admin decision point for the commPact v1 cutover; nothing outside the repo is modified.',
  phases: [
    { title: 'Preflight', detail: 'base green and committed, M1 soak artifacts exist' },
    { title: 'Implement', detail: 'queries + wait + hooks-install/self-test + v1 shim prep' },
    { title: 'Integrate', detail: 'workspace green, demo, docs' },
    { title: 'Review', detail: 'adversarial review, hook self-test proven live' },
  ],
}

const REPO = '/Users/yahirh/projects/clops'

const COMMON = `You are implementing part of Cyclops v2 milestone M2 in ${REPO}. FROZEN inputs, read first: ADR-001 (~/projects/cyclops-arch/deliverables/ADR-001-cyclops-architecture.md), ${REPO}/docs/DELIVERY.md, ${REPO}/docs/GOALS.md, ${REPO}/findings.md, ~/projects/COORDINATION.md (the v1 protocol your shim must keep working). Then read the CURRENT code of every crate you touch: M0 and M1 landed recently and their real APIs are the contract.

Hard rules (same as M0/M1): isolated tmux only (tmux -u -L unique -f /dev/null, killed in teardown; -u per finding F14), no git commands, no network, temp under /private/tmp, zero polling, no em-dashes, fmt/clippy -D warnings/tests green for touched crates, secrets never in the ledger. NEVER modify anything under ~/.commPact or the user's live tmux session or real CLI config dirs (~/.claude, ~/.codex, ~/.gemini, ~/.agents): M2 PREPARES artifacts in-repo; installation is a separate admin-approved step.

Return machine-consumed JSON: summary, api, tests_passed, test_output_tail, findings (MEASURED/READ), concerns.`

const IMPL_SCHEMA = {
  type: 'object',
  required: ['summary', 'tests_passed'],
  properties: {
    summary: { type: 'string' },
    api: { type: 'string' },
    tests_passed: { type: 'boolean' },
    test_output_tail: { type: 'string' },
    findings: { type: 'array', items: { type: 'object', required: ['title', 'label', 'detail'], properties: { title: { type: 'string' }, label: { type: 'string' }, detail: { type: 'string' } } } },
    concerns: { type: 'string' },
  },
}

phase('Preflight')
const pre = await agent(
  `In ${REPO}: run 'cargo test --workspace 2>&1 | tail -3', 'git status --short', 'git log --oneline -3', and 'ls tests/raw/m1-soak/ 2>/dev/null'. Report tests_pass, git_clean, notes (condensed outputs; include whether M1 soak artifacts exist and whether the latest commits mention M1). Read-only.`,
  { label: 'preflight', phase: 'Preflight', schema: { type: 'object', required: ['tests_pass', 'git_clean', 'notes'], properties: { tests_pass: { type: 'boolean' }, git_clean: { type: 'boolean' }, notes: { type: 'string' } } }, effort: 'low' }
)
if (!pre || !pre.tests_pass || !pre.git_clean) {
  throw new Error('M2 preflight failed: ' + (pre ? pre.notes : 'no result'))
}

const QUERY_PROMPT = `${COMMON}

TASK: the read side of messaging. You own the msg.history/msg.thread daemon handlers and the corresponding cyclops CLI verbs (plus their tests). Coordinate boundary: another agent owns agent.wait and a third owns hooks install; do not touch their files.

1. Daemon msg.history: read the session ledger (cyclops-ledger read_after; add an additive indexed reader to that crate ONLY if a measured need appears, and say so), filter per HistoryParams (--with X means from or to X; --from/--to; --to me resolves through the caller's identity envelope; limit; cursor), newest last, return {lines: [LedgerLine], next_cursor}. Reading is free (any agent may query the whole record, ADR messaging rule) but the method still resolves identity for '--to me'.
2. Daemon msg.thread: id -> the msg line plus every line sharing its id and every msg whose reply_to chains to it, ordered.
3. CLI: cyclops history [--with X | --from X --to Y | --to me] [--limit N] and cyclops thread <id>. Rendering per GOALS grid: timestamp gutter (relative under 24h, date beyond), role-colored from, subject, delivery badge summarizing the deliveries array (the M1 badge voice), fyi marked distinctly. --json passthrough. Empty states invite action ('No messages yet. Send one: cyclops send <target> --subject ...').
4. Broadcast/fyi polish: M1 shipped the mechanics; verify the CLI surface (--to a,b / --all / --fyi) matches the brief exactly, fix gaps, and add e2e coverage: a broadcast thread reads coherently in history (one msg fact, N delivery badges).
5. Tests: unit filters/renderers with a fixture ledger file covering all kinds; e2e history/thread against the canned-daemon harness; an integration test where two fixture panes exchange real messages through the daemon and history --with reconstructs the conversation.`

const WAIT_PROMPT = `${COMMON}

TASK: agent.wait, server-owned, plus send-and-wait composition. You own the daemon wait module and the cyclops wait CLI verb.

1. Daemon agent.wait per AgentWaitParams {target, until: idle|done|blocked, timeout_ms}: subscribe to the target's fused state internally, resolve when reached, WireError code 'timeout' on expiry (with the state it was in). Semantics: idle = fused Idle; blocked = any blocked_* or attention state; done = the CURRENT or NEXT turn ends (working -> idle edge). 'Pins pane occupant': record (pane_id, pane_pid) at wait start; if the pane dies or its pane_pid changes while waiting, resolve with WireError code 'occupant_changed' rather than a false success (the protocol spec's pinning rule).
2. Compose into msg.send: MsgSendParams.wait (WaitSpec) makes the send receipt include the wait outcome after delivery resolves: {receipt, wait: {outcome, state, waited_ms}}. Zero polling: all waiting is event-driven on the fusion broadcast.
3. CLI: cyclops wait <target> --until idle|done|blocked [--timeout 120s] (human duration parsing, default sensible), exit 0 on reached, 2 on timeout, 3 on occupant_changed, with GOALS-grade copy for each. cyclops send gains --wait done passthrough. --json shapes.
4. Tests: fixture-pane integration for each until mode including the done edge (drive a fixture 'turn' by making the pane print working-state text then idle text per its manifest), timeout, and occupant_changed (kill the pane mid-wait); e2e CLI exit codes; a send --wait done round trip.`

const HOOKS_PROMPT = `${COMMON}

TASK: hooks install + the startup self-test (amendment c: configuration does not equal subscription; Codex silently loads zero hooks in untrusted dirs, finding F1). You own crates/cyclops hooks subcommands, the daemon self-test module, and hook config templates under ${REPO}/hooks/.

1. Templates in ${REPO}/hooks/<cli>/: claude settings fragment (UserPromptSubmit, Stop, Notification, PermissionRequest -> 'cyclops hook <Event> --agent {label}'), codex hooks.json (same events, PascalCase schema), agy .agents/hooks.json (named-hooks schema, EVERY event registered with a distinct self-tagging command because payloads carry no event name, finding F7). Templates use placeholders ({label}, {cyclops_bin}) and each carries a comment header explaining the trust caveats (F1 for codex: CODEX_HOME or trust_level seeding, the flag that does NOT work).
2. cyclops hooks install <cli> --agent <label> [--dry-run] [--dest <dir>]: renders templates. Default DEST is a cyclops-owned directory: $CYCLOPS_HOME/hooks/<label>/ plus printed, copy-pasteable instructions for wiring the vendor CLI to it (claude: --settings path; codex: CODEX_HOME=... or the config.toml trust seed line, printed not applied; agy: where to place .agents/hooks.json). --dry-run prints what would be written. The command NEVER writes into vendor dot-dirs itself in M2: it prepares and instructs (the admin wires their real setup once, deliberately).
3. cyclops hooks verify <target>: asks the daemon for hook liveness (below), prints tier and last-seen edge ages.
4. Daemon self-test: per adopted pane whose manifest declares hooks: track hook liveness (last agent.state.report per event). Status gains hooks_verified: bool per pane (PaneStatus is frozen proto: put it in Detection readings or a status extension field that serde tolerates; prefer an additive optional field in PaneStatus... it IS additive-tolerant, add Option<bool> hooks_verified with skip_serializing_if, that is a compatible proto addition, note it in the report). At adoption/boot: if no hook edge has EVER been seen for a tier-1 pane, status shows the pane as hook-unverified and the first delivery that times out tier-1 ACK downgrades to tier 2 AND emits an admin.notify action_required naming the likely F1 cause. A 'cyclops hooks selftest <target>' verb triggers a daemon-driven no-op round trip: daemon sends a marker via the normal delivery pipeline flagged fyi+selftest (subject '[cyclops] hook self-test', body asking for no action: 'Reply not needed.'), and reports whether the ACK hook fired with the marker. Costs one trivial turn; document that.
5. Tests: template rendering golden files; install --dry-run e2e; self-test integration with the fixture pane (hook edge simulated by posting agent.state.report through the socket, proving liveness tracking and the unverified path); the F1 regression shape: a tier-1 pane with zero hook edges downgrades cleanly and notifies, no hang, no loss.`

const SHIM_PROMPT = `${COMMON}

TASK: PREPARE (never install) the commPact v1 deprecation shim and the cutover runbook. You own ${REPO}/scripts/commpact-shim/ and ${REPO}/docs/CUTOVER.md only. Read ~/.commPact/bin/commPact (READ-ONLY) and ~/projects/COORDINATION.md to map the real v1 surface agents use today (send with --json/--subject/--body-file -, read <target> [lines], plus whatever else bin/commPact exposes: enumerate it).

1. scripts/commpact-shim/commPact: a bash shim mapping v1 verbs to cyclops equivalents 1:1 (send -> cyclops send, read -> cyclops read, etc.), printing a one-line deprecation note to stderr (once per day per user via a stamp file), and falling back with a clear error for v1 verbs that have no v2 equivalent yet (list them honestly). bash -n clean; shellcheck-clean if shellcheck exists.
2. scripts/commpact-shim/install.sh: what the ADMIN will run later: backs up ~/.commPact/bin/commPact to commPact.v1.bak, symlinks the shim, prints rollback instructions. GUARDED: refuses to run unless CYCLOPS_CUTOVER_ACK=yes is set, so nothing installs it by accident.
3. docs/CUTOVER.md: the runbook: preconditions (M2 green, daemon running against the real session, hooks wired per pane), the install steps, the parallel-running window (v1 files stay intact), verification checklist (each COORDINATION.md messaging pattern exercised through the shim), rollback, and the explicit list of what only the admin may do. End with the ADMIN_ACTION_REQUIRED framing from COORDINATION.md.
4. Tests: a harness test running the shim with cyclops pointed at a canned daemon, asserting verb mapping and that NO file outside the test sandbox is touched (assert ~/.commPact untouched by mtime).`

phase('Implement')
const [queries, wait, hooks, shim] = await parallel([
  () => agent(QUERY_PROMPT, { label: 'history-thread', phase: 'Implement', schema: IMPL_SCHEMA }),
  () => agent(WAIT_PROMPT, { label: 'agent-wait', phase: 'Implement', schema: IMPL_SCHEMA }),
  () => agent(HOOKS_PROMPT, { label: 'hooks-selftest', phase: 'Implement', schema: IMPL_SCHEMA }),
  () => agent(SHIM_PROMPT, { label: 'v1-shim-prep', phase: 'Implement', schema: IMPL_SCHEMA }),
])

phase('Integrate')
const integration = await agent(
  `${COMMON}

TASK: M2 integration. Agent summaries: QUERIES: ${queries ? queries.summary : 'MISSING'} WAIT: ${wait ? wait.summary : 'MISSING'} HOOKS: ${hooks ? hooks.summary : 'MISSING'} SHIM: ${shim ? shim.summary : 'MISSING'}. Concerns to resolve: ${[queries, wait, hooks, shim].filter(Boolean).map(r => r.concerns || '').join(' | ')}.
1. Whole workspace green: fmt, clippy -D warnings, cargo test --workspace.
2. demos/m2-conversation.sh: isolated rig, two fixture panes, send + reply (reply_to), broadcast fyi, history --with reconstructing it, thread view, wait --until idle, hooks selftest against the fixture. Run it; capture output.
3. Update docs/ARCHITECTURE.md M2 markers, CHANGELOG.md. Do NOT edit STATUS.md.
4. Verify the M2 surface against the mission brief verbatim: history/thread/broadcast/fyi flags, wait verbs, hooks install, self-test. List any brief item not shipped with why.`,
  { label: 'integrate', phase: 'Integrate', schema: IMPL_SCHEMA }
)

phase('Review')
const reviews = await parallel([
  () => agent(
    `Adversarial review of M2 in ${REPO} (git diff HEAD + untracked). Focus: identity and ACL regressions ('--to me' resolution, wait occupant pinning, anything that lets a request impersonate a sender), hook self-test honesty (can a pane show hooks_verified without a real edge? can the F1 silent-zero-hooks state hide?), ledger reads matching what was written (history/thread completeness against a generated fixture ledger), and the shim NEVER touching real user dirs. file:line evidence, verdict PASS or BLOCK.`,
    { label: 'review-safety', phase: 'Review', schema: { type: 'object', required: ['verdict', 'issues'], properties: { verdict: { type: 'string' }, issues: { type: 'array', items: { type: 'object', required: ['severity', 'claim', 'evidence'], properties: { severity: { type: 'string' }, claim: { type: 'string' }, evidence: { type: 'string' }, fix: { type: 'string' } } } } } } }
  ),
  () => agent(
    `Review M2 UX in ${REPO} against docs/GOALS.md: history/thread grid rhythm identical to status (compare renderers side by side), badge voice consistency, empty states, error copy (what/why/next), --json parity for every new verb (anything the UI does scripts can do), and the progressive ladder: does each new verb work with ONE unlabeled pane? file:line evidence, verdict PASS or BLOCK.`,
    { label: 'review-ux', phase: 'Review', schema: { type: 'object', required: ['verdict', 'issues'], properties: { verdict: { type: 'string' }, issues: { type: 'array', items: { type: 'object', required: ['severity', 'claim', 'evidence'], properties: { severity: { type: 'string' }, claim: { type: 'string' }, evidence: { type: 'string' }, fix: { type: 'string' } } } } } } }
  ),
])

return { pre, queries, wait, hooks, shim, integration, reviews, admin_note: 'M2 ends here by design: the v1 cutover (installing scripts/commpact-shim via its guarded install.sh, wiring real hook configs into vendor CLIs) is ADMIN_ACTION_REQUIRED. Nothing outside the repo was modified.' }
