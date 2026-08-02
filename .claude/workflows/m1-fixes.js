export const meta = {
  name: 'm1-fixes',
  description: 'M1 gate-fix round: control-drop root cause, review BLOCK issues, manifest safety, soak rerun, re-review',
  whenToUse: 'Runs on the M1 pre-gate checkpoint (416056d). Closes the M1 gate.',
  phases: [
    { title: 'Fix-adapter-data', detail: 'tmux drop root cause + manifest/data safety, parallel' },
    { title: 'Fix-daemon', detail: 'all cyclopsd review fixes on top' },
    { title: 'Soak', detail: 'rerun the gate: claude+codex 100 each' },
    { title: 'Re-review', detail: 'verify every named issue' },
  ],
}

const REPO = '/Users/yahirh/projects/clops'

const COMMON = `You are fixing Cyclops v2 M1 gate blockers in ${REPO}. Base: commit 416056d (M1 pre-gate checkpoint), workspace fully green. Binding docs: docs/DELIVERY.md, docs/GOALS.md (invariants + docs truth rule), findings.md. Soak artifacts with failure evidence: ${REPO}/tests/raw/m1-soak/ (daemon.log, ledger copies, failure captures). Read the CURRENT code first; it changed heavily in M1.

Hard rules: isolated tmux only for tests (tmux -u -L unique-with-pid -f /dev/null, killed in teardown), no git commands, no network, temp under /private/tmp, zero polling, no em-dashes, secrets never in the ledger. Gates: cargo fmt --all, cargo clippy --workspace --all-targets -- -D warnings, cargo test on touched crates green, plus the whole workspace at the end of your task. Every fix gets a test that fails before and passes after where feasible. Docs rule: update any doc your fix makes stale in the same change.

Return JSON: summary, fixes (array: {issue, fix, test}), tests_passed, test_output_tail, findings (MEASURED/READ), concerns.`

const FIX_SCHEMA = {
  type: 'object',
  required: ['summary', 'tests_passed'],
  properties: {
    summary: { type: 'string' },
    fixes: { type: 'array', items: { type: 'object', required: ['issue', 'fix'], properties: { issue: { type: 'string' }, fix: { type: 'string' }, test: { type: 'string' } } } },
    tests_passed: { type: 'boolean' },
    test_output_tail: { type: 'string' },
    findings: { type: 'array', items: { type: 'object', required: ['title', 'label', 'detail'], properties: { title: { type: 'string' }, label: { type: 'string' }, detail: { type: 'string' } } } },
    concerns: { type: 'string' },
  },
}

phase('Fix-adapter-data')
const [adapter, data] = await parallel([
  () => agent(`${COMMON}

TASK: root-cause and fix the tmux control-connection drops, plus adapter hygiene. You own crates/cyclops-tmux only.

THE BUG (soak, MEASURED): 8x "tmux connection lost; reattaching" in 80s, strictly correlated with an active Claude TUI pane (zero drops during 3+ minutes of codex-only load). Twice preceded by "tmux control child did not exit after detach, killing". Each drop blinds both delivery ACK tiers. Evidence in ${REPO}/tests/raw/m1-soak/daemon.log.

Investigate in this order:
1. PRIME SUSPECT: ControlConfig.command_timeout. If a command reply (capture-pane during heavy %extended-output traffic) exceeds the timeout, does the client treat it as fatal and tear down the connection? A busy TUI streaming output can starve reply parsing. Reproduce on an isolated server: a pane running a fast output generator (yes or a script printing 100 lines/10ms with OSC title churn to simulate Claude), then issue capture-pane/display commands in a tight sequence and observe whether the client errors. Also check: does the reader task apply backpressure or can the notification channel (unbounded) plus reply routing deadlock or starve under burst?
2. Verify %pause handling under load: pause-after=300 is seconds of AGE; confirm %pause storms are not causing the teardown, and that auto-continue is not fighting tmux.
3. The "control child did not exit after detach" message is OUR shutdown path killing the child: find why detach hangs (stdin closed while output still streaming?) and make shutdown robust regardless.
Fix so that: command replies are correlated correctly under arbitrary %output/%extended-output interleaving load; slow replies under load do not kill the connection (raise/segment the timeout, or make timeout per-command-class, or treat timeout as command failure WITHOUT connection teardown when the transport is demonstrably alive); reconnect remains for real transport death. Write a sustained-load integration test: 60s+ compressed variant (10s) of heavy-output pane + concurrent command traffic with ZERO Disconnected events required.
Also fix (review, low): ControlClient::load_buffer writes payloads to std::env::temp_dir() with default perms (control.rs:444-458). Give it a caller-supplied spool dir option or create files 0600; coordinate: the daemon currently duplicates this logic to avoid the shared temp dir; expose the safe path so cyclopsd/src/delivery.rs can switch to it (a follow-up agent will consume it, document the new signature clearly in your summary).
Record the root cause as a numbered finding (F19+) in ${REPO}/findings.md (append; the file is shared, edit surgically).`, { label: 'adapter-drops', phase: 'Fix-adapter-data', schema: FIX_SCHEMA }),
  () => agent(`${COMMON}

TASK: manifest data safety + harness vocabulary + docs. You own manifests/*.toml, crates/cyclops-manifest, tests/harness/tuikit.py, tests/m1_soak.py (vocabulary only), docs/send.md (new), docs/DELIVERY.md (spec clarifications only where named below), docs/MANIFESTS content if it exists.

Issues, all from the M1 reviews/soak (fix every one):
1. HIGH (review): manifests/claude.toml lets title_idle_sparkle (priority 1000) outrank composer_has_staged_input (950): fused state reads idle while a human draft sits in the composer, and the gate would paste over it and auto-submit. Fix in data: composer_has_staged_input priority above the idle title (1050). Add a manifest-crate test locking the ordering: title ✳ + composer '❯ draft text' must evaluate to idle_with_input.
2. HIGH (review): codex.toml cannot produce idle_with_input at all: composer_empty_or_ghost matches '^\\s*›' whether the composer holds ghost suggestions or real typed text. PROBE FIRST (MEASURED, isolated tmux, codex binary is on this machine; one short launch, no turns needed): capture the codex composer with escapes (tmux capture-pane -e) in two states: pristine ghost suggestion, and after send-keys typing literal text. Determine whether ghost text is distinguishable by SGR (dim/color) from typed text. If yes: add the discriminator as manifest data (new optional rule field the schema already tolerates or a small schema addition in cyclops-manifest: e.g. an escaped_region matcher; keep it data-driven and document it) plus rules: typed text => idle_with_input (priority 1050), ghost => idle. If SGR does not discriminate, implement the honest fallback and say so loudly: any '›' line with content => idle_with_input EXCEPT lines matching a data allowlist of observed ghost phrasings, and record the residual risk as a finding. Either way add manifest tests with the probed real captures as fixtures.
3. SAFETY (soak, MEASURED): Claude 2.1.220's folder-trust dialog reads 'Quick safety check: Is this a project you created or one you trust?' with '1. Yes, I trust this folder / Enter to confirm . Esc to cancel', and Escape EXITS the CLI. Today claude.toml startup_modal (contains 'Enter to confirm', decline Escape, auto_dismiss=true) would match it and kill the session. Add a dedicated claude trust_dialog rule ABOVE startup_modal (contains 'safety check' / 'trust this folder'), auto_dismiss=false (trust is a human decision, same policy as codex trust_dialog); make sure startup_modal cannot shadow it; test with the real dialog text as fixture.
4. BINDING (soak, MEASURED): native Claude installs report pane_current_command as the VERSION STRING ('2.1.220': ~/.local/bin/claude symlinks to versions/2.1.220 and macOS derives comm from the resolved file), so process_names=['claude'] never binds and detection silently does nothing. Data side: document in claude.toml that comm may be a semver string on native installs. The CODE fallback (argv-based binding via pane_pid) belongs to the daemon agent, not you, but add the manifest schema field they need if any (e.g. argv_basenames = ['claude']) with schema support + test.
5. Harness: tuikit.py MODAL_SIGNS/dismiss_modal miss the new trust wording and its Escape fallback exits claude (cost a soak leg). Add 'trust this folder' and 'safety check' handling mirroring the soak driver's fix; NEVER Escape on a dialog whose text says Esc cancels/exits: prefer the explicit affirmative for harness-owned scratch dirs, and add the same vocabulary to tests/m1_soak.py if it carries its own copy.
6. docs/send.md (new, truth rule: only shipped behavior): sending, receipt states with the exact badge strings, broadcast, --fyi, exit codes, what verified vs unverified means, quota parking and what to do about it. 60-second skimmable.
7. docs/DELIVERY.md: two spec clarifications the fixes introduce (mark them as v1.1 amendments at the bottom, do not rewrite history): tier-2 evidence wording stays as spec (marker-left-composer AND turn evidence) and detach-aware ACK deadlines (deadlines freeze while the control connection is down; post-reattach the pipeline re-verifies before any retry). The daemon agent implements; you document.`, { label: 'manifest-safety', phase: 'Fix-adapter-data', schema: FIX_SCHEMA }),
])

phase('Fix-daemon')
const daemon = await agent(`${COMMON}

TASK: every cyclopsd fix from the M1 reviews and soak. You own crates/cyclopsd (and may adjust crates/cyclops only for the hook seq fix below). Two agents just landed ahead of you; READ THEIR CHANGES FIRST: adapter (${adapter ? adapter.summary : 'MISSING: investigate crates/cyclops-tmux diff'}) and manifest/data (${data ? data.summary : 'MISSING: investigate manifests diff'}). Their fix lists: ADAPTER: ${adapter && adapter.fixes ? JSON.stringify(adapter.fixes).slice(0, 1500) : 'n/a'} DATA: ${data && data.fixes ? JSON.stringify(data.fixes).slice(0, 1500) : 'n/a'}

Fix all of these, each with a test:

A. HIGH (review): send-and-wait ordering violates DELIVERY.md: msg_send starts wait_until immediately after the receipt phase without awaiting delivery resolution, and until:done counts working phases that predate the delivery (delivery.rs:647-658, :1654-1659). Fix: wait starts only after the delivery reaches a resolved state, and done counts only working seen after this delivery's Submitted.

B. MEDIUM (review): post-paste verification can pass on stale screen text: patterns_hit any-matches non-id patterns anywhere in a 15-line window (delivery.rs:1344-1361, :1390-1392), so 'Pasted text' from a PREVIOUS message can verify a paste that never staged. Fix: the substituted <message_id> pattern counts anywhere; non-id patterns count only on a composer line (reuse the marker_in_composer machinery).

C. MEDIUM (review): restart limbo: non-terminal deliveries at daemon stop are never resolved (lib.rs:250-253, :426-433). Limbo is a bug (GOALS). Fix: at boot, scan each session ledger for deliveries whose latest state is non-terminal; append a state line to attention_required (cause: daemon_restart) plus ONE aggregated admin.notify listing them. Test: kill a daemon mid-queue, reboot, assert the ledger closes every chain.

D. MEDIUM (review): tier-2 screen ACK accepts a changed-window alone as evidence (delivery.rs:1517-1523), weaker than spec (marker-left-composer AND turn evidence). Fix per spec: window change alone only counts when the earlier verification demonstrably staged the id pattern; otherwise require working_seen or output_seen. Lengthen the tier-2 deadline if needed rather than weakening evidence.

E. SOAK (the gate killer): detach-blind ACK. During a control-connection drop, agent.state.report is rejected (session_detached) and captures fail, but ACK deadlines keep running; a delivery submitted just before two drops exhausted submit+retry into attention_required while the recipient answered BOTH copies (duplicate delivery, tests/raw/m1-soak evidence). Fix: (1) ACK deadlines freeze while the session watcher is detached and extend by the detach duration on reattach. (2) After reattach, before ANY retry of an in-flight delivery, run an evidence pass (screen capture for the marker, late-ACK window) and resolve as delivered rather than resubmitting when the evidence shows the payload arrived. (3) Hook reports arriving during detach should be accepted if the pane still exists in the last-known table (the report does not need the tmux connection). Test with a scripted watcher detach/reattach around a delivery.

F. BINDING (soak): pane_current_command can be a version string on native installs (claude '2.1.220'), silently never binding a manifest. Fix in fusion binding: when comm matches no manifest, fall back to the argv basename of pane_pid (ps -o comm=/args= or sysctl; you have pane_pid in PaneRow) matched against process_names plus any new manifest field the data agent added. Cache per (pane, pid). Test with a renamed binary in an isolated pane.

G. LOWS (review, all of them):
- delete-buffer best-effort when paste-buffer fails after load-buffer succeeded (delivery.rs:1321-1340).
- switch inject() to the adapter's new safe load_buffer spool path (the adapter agent exposed it; if absent, keep the local 0700 spool and note it).
- hook seq reset: a seq lower than max-seen means the counter reset; clear that agent's window instead of dropping real ACKs as duplicates (ack.rs:44-57 vs cyclops/src/hook.rs:135).
- hook readings TTL: age out (or invalidate on N disagreeing rules-tier recomputes) so a stale hook edge cannot pin fused state; plus admin.notify once when a delivery has been held in gating past a generous threshold (make it config, default ~120s) so a wedged hold is at least visible.
- cross-session consistency: the unresolvable-recipient state line goes to session_idx 0 while the msg line goes to involved sessions (delivery.rs:574 vs :536-551); make every per-session file a complete stream for the deliveries it hosts.
- modal decline TOCTOU: re-capture and require the same modal rule to still match before the FINAL confirming key of a multi-key decline sequence; abort to the gate loop if the screen changed.
- subscriber buffer: raise the per-subscriber event buffer so a briefly-stalled client survives soak-rate events (soak measured drops at ~2.5s stall); keep the drop for truly wedged clients.

Amendment i while you are in there (brief requires it, currently implicit): ensure the injector is behind a trait (Injector or similar) with the tmux paste path as its first impl, so a headless protocol backend can slot in per agent without touching layers above. If M1 already shaped it this way, state where; if not, refactor minimally.

Finish with the whole workspace green: fmt, clippy -D warnings, cargo test --workspace.`, { label: 'daemon-fixes', phase: 'Fix-daemon', schema: FIX_SCHEMA })

phase('Soak')
const soak = await agent(`${COMMON}

TASK: rerun the M1 gate soak on the fixed tree using the existing rig ${REPO}/tests/m1_soak.py (read it first; extend rather than rewrite). Gate: 100 messages per available CLI, zero unrecovered loss, ledger as ground truth.
- claude: 100 messages. The previous run died to control-connection drops (now fixed) and a binding workaround (hardlink); use the real fix (argv binding) instead of the hardlink if it landed, else keep the hardlink and say so.
- codex: 100 messages (regression: was 100/100; also watch whether ack p95 improves now that detach churn is fixed).
- agy: expect an immediate valid quota park (vendor quota was exhausted with a multi-day reset). One message proving park-on-first is a complete leg per F11 gate rules; if quota recovered, run the full 100.
- Cheapest models as before (haiku / gpt-5.6-luna / agy default). Same isolation rules. Artifacts to ${REPO}/tests/raw/m1-soak-2/.
- Report per-CLI: sent, delivered_verified, delivered_unverified, lost, retries, duplicates (check pane/ledger for double-delivery of any seq: the previous failure mode), ack p50/p95, detach events during the run (must be 0 for the drop fix to be proven), verdict PASS/FAIL with one line.`, { label: 'soak-rerun', phase: 'Soak', schema: {
  type: 'object',
  required: ['verdict', 'per_cli', 'notes'],
  properties: {
    verdict: { type: 'string' },
    per_cli: { type: 'array', items: { type: 'object', required: ['cli', 'sent', 'delivered_verified', 'delivered_unverified', 'lost'], properties: { cli: { type: 'string' }, sent: { type: 'number' }, delivered_verified: { type: 'number' }, delivered_unverified: { type: 'number' }, lost: { type: 'number' }, retries: { type: 'number' }, duplicates: { type: 'number' }, detach_events: { type: 'number' }, ack_p50_ms: { type: 'number' }, ack_p95_ms: { type: 'number' }, parked: { type: 'boolean' }, skipped: { type: 'boolean' } } } },
    notes: { type: 'string' },
    findings: { type: 'array', items: { type: 'object', required: ['title', 'label', 'detail'], properties: { title: { type: 'string' }, label: { type: 'string' }, detail: { type: 'string' } } } },
  },
} })

phase('Re-review')
const REVIEW_SCHEMA = {
  type: 'object',
  required: ['verdict', 'issues'],
  properties: {
    verdict: { type: 'string' },
    issues: { type: 'array', items: { type: 'object', required: ['severity', 'claim', 'evidence'], properties: { severity: { type: 'string' }, claim: { type: 'string' }, evidence: { type: 'string' }, fix: { type: 'string' } } } },
  },
}
const reviews = await parallel([
  () => agent(`Re-review Cyclops M1 fixes in ${REPO} (diff since commit 416056d plus untracked). The original review BLOCKED on: (1) claude.toml idle-sparkle outranking staged-composer (human draft injectable), (2) codex missing idle_with_input discrimination, (3) no pane-rebind re-check between gate and paste/submit, (4) modal decline TOCTOU on multi-key sequences, (5) tmux buffer left server-global on paste failure, (6) adapter load_buffer world-readable temp. Verify EACH is genuinely fixed with file:line evidence and a test, hunt regressions the fixes introduced, and re-run the invariants sweep: resolve+gate+verify+submit on every typed-into-pane path, manifest-only modal keys, no quota retry, no secrets in ledger appends, tmux -u everywhere outside the adapter. Verdict PASS or BLOCK.`, { label: 'reverify-invariants', phase: 'Re-review', schema: REVIEW_SCHEMA }),
  () => agent(`Re-review Cyclops M1 fixes in ${REPO} (diff since commit 416056d plus untracked). The original review BLOCKED on: (1) send-and-wait starting before delivery resolution and counting pre-existing working phases, (2) stale-text composer verification via any-match, (3) restart limbo for non-terminal deliveries, (4) tier-2 window-change-alone evidence, (5) cross-session state-line inconsistency, (6) hook seq-reset dropping real ACKs, (7) stale hook readings pinning state with no operator signal. Verify EACH against docs/DELIVERY.md (including its new amendments section) with file:line evidence and a test; check the detach-freeze ACK deadline logic for new races (deadline freeze vs late ACK vs retry evidence pass); confirm every delivery still ends in a named terminal state across daemon restart mid-flight. Verdict PASS or BLOCK.`, { label: 'reverify-correctness', phase: 'Re-review', schema: REVIEW_SCHEMA }),
])

return { adapter, data, daemon, soak, reviews }
