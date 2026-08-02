export const meta = {
  name: 'm1-delivery',
  description: 'M1: ledger integration, msg.send end-to-end, delivery state machine, ACK tiers, quota parking, modal vocabulary, mini-soak gate',
  whenToUse: 'Launch after M0 is integrated, green, and committed. Preflight refuses a dirty or red base.',
  phases: [
    { title: 'Preflight', detail: 'base must be green and committed' },
    { title: 'Implement', detail: 'delivery core + identity + CLI send/hook in parallel' },
    { title: 'Integrate', detail: 'workspace green, demo, docs' },
    { title: 'Soak', detail: '100-message mini-soak per available CLI, zero unrecovered loss' },
    { title: 'Review', detail: 'adversarial review vs DELIVERY.md, amendments, GOALS' },
  ],
}

const REPO = '/Users/yahirh/projects/clops'

const COMMON = `You are implementing part of Cyclops v2 milestone M1 in the Cargo workspace at ${REPO}. The architecture is FROZEN: ADR-001 (~/projects/cyclops-arch/deliverables/ADR-001-cyclops-architecture.md), the validation amendments, ${REPO}/docs/DELIVERY.md (the M1 spec, binding), ${REPO}/docs/GOALS.md (quality bar, binding). Read all of those plus ${REPO}/findings.md (F13-F18 are load-bearing for this milestone) and the CURRENT code of every crate you touch or call: the M0 implementation landed recently and its actual APIs override anything you assume.

Hard rules:
- NEVER touch the user's live tmux session or default tmux server. Tests use isolated servers: tmux -u -L <unique-per-test-with-pid> -f /dev/null, killed in teardown (see F14 for why -u).
- No git commands. No network. Temp files under /private/tmp only.
- Zero polling: event-driven, reconcile on doubt. Debounce reset-timers are fine, interval re-queries are not.
- Comment style: behavior and non-obvious decisions, concise, no em-dashes, no filler.
- Gates for your work: cargo fmt --all, cargo clippy --workspace --all-targets -- -D warnings, cargo test -p <crates you touched>. Do not leave the workspace red.
- Secrets never enter the ledger: gate/state lines carry rule ids and causes, never raw screen captures.
- Only touch the files your task lists. Coordinate boundaries are strict because other agents run in parallel with you.

Return machine-consumed JSON: summary, api (public surface you added/changed), tests_passed, test_output_tail, findings (MEASURED/READ surprises, validation style), concerns.`

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

const PRE_SCHEMA = {
  type: 'object',
  required: ['tests_pass', 'git_clean', 'notes'],
  properties: { tests_pass: { type: 'boolean' }, git_clean: { type: 'boolean' }, notes: { type: 'string' } },
}

phase('Preflight')
const pre = await agent(
  `In ${REPO}: run 'cargo test --workspace 2>&1 | tail -3', 'git status --short' and 'git log --oneline -3'. Report tests_pass (all green), git_clean (no uncommitted changes besides untracked scratch), notes (the outputs, condensed). Read-only, do not fix anything.`,
  { label: 'preflight', phase: 'Preflight', schema: PRE_SCHEMA, effort: 'low' }
)
if (!pre || !pre.tests_pass || !pre.git_clean) {
  throw new Error('M1 preflight failed, base is not green/committed: ' + (pre ? pre.notes : 'no result'))
}

const CORE_PROMPT = `${COMMON}

TASK: the delivery core in crates/cyclopsd, plus adopting crates/cyclops-ledger into the workspace. This is the heart of M1; docs/DELIVERY.md is your spec, implement it exactly.

1. Workspace adoption of cyclops-ledger (you own these edits): remove the temporary [workspace] opt-out table from crates/cyclops-ledger/Cargo.toml, add the crate to the root Cargo.toml members and workspace.dependencies, add it to cyclopsd's dependencies. Its API (LedgerWriter::open/append, read_after) is frozen; extend only with additive helpers if genuinely needed.
2. Ledger wiring: one LedgerWriter per watched session at $CYCLOPS_HOME/ledger/<session>.ndjson. Daemon boot writes a kind=system line (boot, tmux version, manifest set). Fused state CHANGES write kind=state lines. Everything the delivery pipeline does writes lines per DELIVERY.md.
3. msg.send: full pipeline per DELIVERY.md: per-recipient FIFO worker, gate order exactly as specified (dead, in_mode hold, quota park, modal manifest-decline-or-hold, working hold, idle_with_input hold because human typing always wins, idle proceed with a fresh pre-paste re-check), inject (unique buffer name per delivery, temp file under a 0700 private dir per the tmux crate concern, paste-buffer -p, composer verification with <message_id> substitution, manifest submit key), ACK tiers (tier 1 hook match through the AckMatcher described below, tier 2 screen with later upgrade), bounded single retry, attention_required + admin notify on exhaustion, parked_blocked_quota parking that never auto-retries. Broadcast: one msg line, N delivery records advancing independently. Receipt semantics and timing budgets per DELIVERY.md (block max 2.5s on the idle path, immediate queued/parked receipts otherwise).
4. AckMatcher: consumes agent.state.report submissions (the socket method; implement its handler now): normalize event names, dedupe on (session_id, turn_id, event) where payloads carry them (codex duplicates, amendment d), match the manifest hooks.ack event whose ack_payload_field contains the delivery's message id, route to the waiting delivery worker. Out-of-order tolerance via the report seq field. Unmatched reports still update fusion (a hook edge is a sensor reading, wire it into the fusion state as the hook sensor).
5. Events: subscribers receive msg, delivery-state, gate, and admin-notify events as they happen (the M3 stream consumes these).
6. admin.notify handler: writes a kind=system ledger line and broadcasts an event. Delivery pipeline calls it internally for parked/attention transitions.
7. Config additions (warn-not-error on unknown keys stays): ack_timeout_ms (default 1500), delivery_retry_max (default 1), receipt_block_ms (default 2500).
8. Identity: crates/cyclopsd/src/identity.rs is being written IN PARALLEL by another agent with the exact surface: resolve_sender(uid: u32, pid: i32, panes: &[(String, Option<String>, i32)]) -> Sender where panes tuples are (pane_id, label, pane_pid) and Sender is an enum { Agent(String), Pane(String), Admin } plus a fn peer_of(&UnixStream) -> io::Result<(u32, i32)>. Code against that exact signature; declare the module; if the file is absent when you finish, leave a compiling stub with todo!() marked clearly and note it in concerns. Do NOT implement the walk yourself and do NOT edit identity.rs.
9. Tests (isolated tmux only): unit-test the state machine wiring against cyclops_proto::DeliveryState::can_transition_to (every transition your code performs must be legal; add a debug assertion in the transition helper). Integration: fixture manifest bound to a cat pane (screen tier: paste, verify, submit, delivered_unverified via marker-left-composer plus output activity), a modal fixture (write a fixture manifest whose modal rule has decline_keys and one without auto_dismiss; fake the modal by running a script that prints the modal text and holds; assert decline keys sent in one case, attention hold plus admin notify event in the other), quota fixture (screen shows the agy quota phrase: assert parking, admin notify, and that NOTHING retries it), ordering (three sends to a busy fixture arrive FIFO after it goes idle), broadcast fan-out records, receipt shapes for idle/busy/parked paths, ledger lines legal and jq-parseable for the whole run.
Do not touch crates/cyclops (another agent owns it this phase) or crates/cyclops-tmux except: nothing. If you need a tmux helper that does not exist, note it in concerns instead of adding it.`

const IDENTITY_PROMPT = `${COMMON}

TASK: fail-closed sender identity. Files you own, exclusively: crates/cyclopsd/src/identity.rs (new), crates/cyclops-tmux/src/watcher.rs and its snapshot format ONLY for adding the pane_pid field, plus that crate's affected tests.

1. cyclops-tmux: add pane_pid (i32) to PaneRow, sourced from #{pane_pid} in the list-panes snapshot format and the per-pane subscription format. Keep parsing anchored per the crate's existing tab-ambiguity discipline (ids left, fixed fields right, title as remainder). Update PaneRow::to_status only if PaneStatus grows a field (it does not; pane_pid is daemon-internal).
2. crates/cyclopsd/src/identity.rs with EXACTLY this public surface (the delivery core agent is coding against it in parallel):
   pub enum Sender { Agent(String), Pane(String), Admin }
   pub fn peer_of(stream: &tokio::net::UnixStream) -> std::io::Result<(u32, i32)>  // (uid, pid), macOS LOCAL_PEERCRED via getsockopt(SOL_LOCAL, LOCAL_PEERCRED) with xucred (pid via LOCAL_PEERPID), Linux SO_PEERCRED ucred. libc is already a dependency.
   pub fn resolve_sender(uid: u32, pid: i32, panes: &[(String, Option<String>, i32)]) -> Sender
   Resolution: if uid != daemon uid the CALLER denies (peer_of only reports; document it). Walk the process ancestry of pid (macOS: sysctl KERN_PROC_PID for ppid, loop with a depth cap of 32 and a visited set; Linux: /proc/<pid>/stat field 4) until a pid equals some pane_pid: that pane's label if labeled, else Pane(pane_id). No pane match: Admin (a same-uid shell outside watched panes is the human, per COORDINATION).
3. Tests: unit-test resolve_sender with synthetic ancestry (inject a parent-lookup closure so the walk is testable without real processes; keep the public fn signature by making the closure an internal seam). Integration (isolated tmux): spawn a session, run a child process inside a pane that connects... simulating the full socket round trip is the core agent's job; yours proves that walking from a real child process spawned inside an isolated tmux pane reaches that pane's pane_pid (spawn 'sleep 5' via send-keys, find its pid via pgrep -P chain from the pane pid downward, then resolve upward and assert the match). Also prove peer_of returns your own uid/pid over a socketpair.`

const CLI_PROMPT = `${COMMON}

TASK: crates/cyclops additions for M1: send and the hook receiver. Read the crate's existing client/style/render/copy modules first and match their idioms exactly. You own crates/cyclops only.

1. cyclops send <target> --subject <s> [--body <b> | --body-file -] [--to a,b] [--all] [--fyi] [--reply-to id]: positional target merges into the to-list; --body-file - reads stdin (the v1 habit, COORDINATION.md shows it). Calls msg.send. Renders the receipt in the landing-page badge voice:
   delivered_verified:   ✓ delivered · verified
   delivered_unverified: ✓ delivered · unverified (screen)
   queued:               ● queued · 2 ahead
   parked_blocked_quota: ⛔ parked · quota, resets in 135h
   attention_required:   ⚠ needs attention · <cause>
   Broadcast prints one line per recipient, aligned grid. --json passthrough. Exit 0 on delivered/queued, 1 on parked/attention (scripts branch on it).
2. cyclops hook <event-name>: the receiver vendor hook configs invoke. Reads stdin JSON (tolerant of the three vendor shapes; agy payloads have NO event-name field which is exactly why the event name is an argument, findings F7), wraps it as agent.state.report { agent: from $CYCLOPS_AGENT env or --agent flag, event, seq: from a per-process monotonic file counter under $CYCLOPS_HOME/hookseq/<agent>, payload: raw }, posts it. MUST be fast and silent (it runs inside vendor hook budgets): no color, no output on success, exit 0 even if the daemon is down (a hook must never break the agent CLI; log to $CYCLOPS_HOME/hook-errors.log instead). 3s total budget.
3. Error copy per GOALS (what happened, why, next step) for: unknown recipient (list known), daemon down, parked target (show the reset hint from the receipt note).
4. Tests: unit renderers for every receipt state (exact strings, plain mode); e2e against the existing canned-daemon harness pattern: send happy path, broadcast grid, parked exit code, hook subcommand posts a well-formed report and stays silent, hook with daemon down exits 0 and writes the error log.`

phase('Implement')
const [core, identity, cli] = await parallel([
  () => agent(CORE_PROMPT, { label: 'delivery-core', phase: 'Implement', schema: IMPL_SCHEMA }),
  () => agent(IDENTITY_PROMPT, { label: 'identity', phase: 'Implement', schema: IMPL_SCHEMA }),
  () => agent(CLI_PROMPT, { label: 'cli-send-hook', phase: 'Implement', schema: IMPL_SCHEMA }),
])

phase('Integrate')
const integration = await agent(
  `${COMMON}

TASK: integration. The three M1 agents just finished: delivery core (${core ? core.summary : 'MISSING, investigate'}), identity (${identity ? identity.summary : 'MISSING, investigate'}), CLI (${cli ? cli.summary : 'MISSING, investigate'}). Their concerns: CORE: ${core ? core.concerns || 'none' : 'n/a'} IDENTITY: ${identity ? identity.concerns || 'none' : 'n/a'} CLI: ${cli ? cli.concerns || 'none' : 'n/a'}.
1. Make the whole workspace green: cargo fmt --all, clippy --workspace --all-targets -D warnings, cargo test --workspace. Resolve any stub seams (identity module wiring) and API drift between the three.
2. Write demos/m1-send.sh in the style of demos/m0-status.sh (isolated server, self-cleaning): two cat panes labeled implementer/reviewer via the registry mechanism the core implemented, one cyclops send producing a visible paste plus '✓ delivered · unverified (screen)' receipt, one broadcast, then jq over the ledger file showing the msg and state lines. bash -n clean, run it, capture output.
3. Update docs/ARCHITECTURE.md M1 markers to reflect what now exists, and append a concise M1 section to CHANGELOG.md [Unreleased]. Do NOT edit STATUS.md (the orchestrator owns it).
4. Report any behavior that contradicts docs/DELIVERY.md rather than silently adapting the doc: the doc is the spec; deviations need a finding.`,
  { label: 'integrate', phase: 'Integrate', schema: IMPL_SCHEMA }
)

phase('Soak')
const soak = await agent(
  `${COMMON}

TASK: the M1 regression gate: a 100-message mini-soak per AVAILABLE agent CLI, driven end-to-end through cyclopsd msg.send (not raw tuikit paste). Zero unrecovered loss required.
- Which CLIs: probe for claude, codex, agy binaries. Soak each that exists; report absent ones as skipped.
- Cheapest models, trivial prompts (the validation campaign pattern): claude --model haiku via --settings hook config; codex with CODEX_HOME under scratch (copy ONLY auth.json from ~/.codex, hooks.json written fresh; model gpt-5.6-luna; directory-trust gating is finding F1, CODEX_HOME is the fix); agy default (screen tier, no usable ACK hooks, findings F7/F11).
- Rig: isolated tmux server (tmux -u -L cyc-soak-$$ -f /dev/null), one pane per CLI under test, temp CYCLOPS_HOME, config watching that session, real manifests from ${REPO}/manifests, hook configs invoking the real 'cyclops hook' binary (cargo build --release first; use target/release binaries for realistic latency). Register labels for the panes.
- Reuse ${REPO}/tests/harness/tuikit.py knowledge for launching the vendor TUIs and their modal quirks, but ALL message delivery goes through cyclops send / the socket. Messages: sequenced markers, trivial content ('Reply with just: ok (<n>)').
- 100 messages per CLI, sent when idle (the pipeline handles pacing; send the next after the previous delivery reaches a terminal state or queued-then-delivered). Record per-delivery: seq, states walked, verified_by, ack latency, retries. Pull ground truth from the ledger file, not client output.
- Vendor quota exhaustion (F11) is a valid campaign outcome, not a failure: if a CLI parks, record it, stop that leg, continue others.
- Gate: zero unrecovered loss (every message ends delivered_verified or delivered_unverified; parked-by-quota legs count as complete at the parking point). Any attention_required or lost message is a FAILURE: capture the ledger tail and pane state for diagnosis.
- Artifacts: ${REPO}/tests/raw/m1-soak/ (gitignored): per-CLI ledger copies, summary.json (per-CLI counts, latency p50/p95, verdict).
- Kill everything you started (tmux server, daemon, CLIs) even on failure. Never touch the live session. Watch token spend: trivial prompts only, stop a leg at the first sign of a vendor-side block other than quota (login prompts etc.) and report it.`,
  { label: 'mini-soak', phase: 'Soak', schema: {
    type: 'object',
    required: ['verdict', 'per_cli', 'notes'],
    properties: {
      verdict: { type: 'string', description: 'PASS or FAIL with one-line reason' },
      per_cli: { type: 'array', items: { type: 'object', required: ['cli', 'sent', 'delivered_verified', 'delivered_unverified', 'lost'], properties: { cli: { type: 'string' }, sent: { type: 'number' }, delivered_verified: { type: 'number' }, delivered_unverified: { type: 'number' }, lost: { type: 'number' }, parked: { type: 'boolean' }, ack_p50_ms: { type: 'number' }, ack_p95_ms: { type: 'number' }, skipped: { type: 'boolean' } } } },
      notes: { type: 'string' },
      findings: { type: 'array', items: { type: 'object', required: ['title', 'label', 'detail'], properties: { title: { type: 'string' }, label: { type: 'string' }, detail: { type: 'string' } } } },
    },
  } }
)

phase('Review')
const REVIEW_SCHEMA = {
  type: 'object',
  required: ['verdict', 'issues'],
  properties: {
    verdict: { type: 'string', description: 'PASS or BLOCK' },
    issues: { type: 'array', items: { type: 'object', required: ['severity', 'claim', 'evidence'], properties: { severity: { type: 'string' }, claim: { type: 'string' }, evidence: { type: 'string', description: 'file:line proof' }, fix: { type: 'string' } } } },
  },
}
const reviews = await parallel([
  () => agent(
    `Adversarial review of the M1 delivery implementation in ${REPO} against ${REPO}/docs/DELIVERY.md and the DeliveryState::can_transition_to table in crates/cyclops-proto/src/ledger.rs. Read the diff (git diff HEAD, plus untracked files) and the pipeline code in full. Hunt: illegal state transitions, paths where a delivery can end without a terminal ledger state (limbo is a bug), receipt states that lie (conflating verified/unverified), ordering violations, the 2.5s receipt cap, retry beyond the bound, quota auto-retry. Every issue needs file:line evidence and a concrete failure scenario; no style nits. Verdict BLOCK only for correctness or invariant breaks.`,
    { label: 'review-correctness', phase: 'Review', schema: REVIEW_SCHEMA }
  ),
  () => agent(
    `Adversarial review of M1 in ${REPO} for the GOALS.md reliability invariants and safety. Read docs/GOALS.md invariants section, the manifests in manifests/, and the delivery/gate code in full. Hunt: any path that types into a pane without resolve+gate+verify+submit, any generic modal dismissal (Enter/Escape not sourced from manifest decline_keys), auto_dismiss=false rules that still get keys sent, blocked_quota retry paths, human-typing races (idle_with_input handling, the pre-paste freshness re-check), secrets leaking into ledger lines (grep every ledger append for screen content), buffer-name uniqueness, tmux -u discipline (finding F14) in every new tmux invocation outside the adapter crate. file:line evidence, no nits.`,
    { label: 'review-invariants', phase: 'Review', schema: REVIEW_SCHEMA }
  ),
])

return { pre, core, identity, cli, integration, soak, reviews }
