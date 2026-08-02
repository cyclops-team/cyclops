export const meta = {
  name: 'm2-fixes',
  description: 'M2 gate-fix round: authenticate hook reports, liveness pid-keying, cursor paging, UX copy fixes, re-review',
  whenToUse: 'Runs on the uncommitted M2 tree. Closes the M2 gate.',
  phases: [
    { title: 'Fix', detail: 'security/daemon + UX/copy in parallel' },
    { title: 'Verify', detail: 're-verify both BLOCK lists' },
  ],
}

const REPO = '/Users/yahirh/projects/clops'

const COMMON = `You are fixing Cyclops v2 M2 gate blockers in ${REPO}. Base: uncommitted M2 tree on top of commit f1b0811, workspace green (258 tests). Binding docs: docs/GOALS.md, docs/DELIVERY.md, docs/{send,history,wait,hooks}.md, findings.md. Read the CURRENT code of every file you touch first.

Hard rules: isolated tmux only for tests (tmux -u -L unique-with-pid -f /dev/null, killed in teardown), no git commands, no network, temp under /private/tmp, zero polling, no em-dashes, secrets never in the ledger. Gates: cargo fmt --all, cargo clippy --workspace --all-targets -- -D warnings, cargo test --workspace green at the end. Every fix gets a test that fails before where feasible. Docs truth rule: fix stale docs in the same change.

Return JSON: summary, fixes (array {issue, fix, test}), tests_passed, test_output_tail, findings, concerns.`

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

phase('Fix')
const [security, ux] = await parallel([
  () => agent(`${COMMON}

TASK: the security/correctness blockers. You own crates/cyclopsd, crates/cyclops-proto (additive only), and the daemon-side tests. Do not touch crates/cyclops (the parallel agent owns it).

1. HIGH: agent.state.report is unauthenticated. Evidence: ack.rs:153-180 resolves the pane from params.agent with no peer check; server.rs:300-314 dispatches handle_report without peer creds. A same-uid process can forge hook liveness (hooks_verified) AND tier-1 ACK evidence (delivered · verified), so the record can lie. Fix: pin hook reports exactly like senders: the SOCKET path resolves identity::resolve_sender(uid, peer_pid, panes) and requires the ancestry to land in the pane params.agent names (label or pane id). Mismatch or Admin or unresolvable: WireError code denied, and the report is NOT ingested (fail closed; document that admin cannot post hook reports, hooks come from inside the pane by construction since cyclops hook runs as a child of the vendor CLI). The IN-PROCESS Daemon::report_state API keeps a pre-resolved trusted path for tests, mirroring msg_send's design; existing integration tests that post reports through the socket from the test process must switch to the in-process path or spawn their reporter inside the fixture pane (prefer fixing the tests the honest way; at least one NEW test must post a forged report over the socket from outside the pane and assert denied + no liveness recorded + no ACK matched).
2. MEDIUM: F1 can hide behind stale liveness: hook edges are keyed by pane_id and forgotten only on PaneRemoved (lib.rs:832-835), so an occupant restart without hooks keeps showing hooks_verified. Fix: record pane_pid with each edge; seen_any/hooks_verified_for discard edges whose recorded pid no longer matches the current row (occupant change invalidates liveness). Test: fixture pane gains liveness, occupant swapped (respawn), assert hooks_verified reverts and the F1 downgrade notification fires on the next tier-1 delivery.
3. MEDIUM: msg.history cursor paging silently skips messages with multiple watched sessions (history.rs:281-298 pages on raw per-file seq; docs/history.md promises a gapless walk). Fix properly: an opaque composite cursor (e.g. base64 of a {session: seq} map or paging on the (ts, seq, id) sort key); keep the wire field a string/u64-compatible Value additively (HistoryParams.cursor is Option<u64> in proto: add an additive cursor2/opaque field OR encode the composite losslessly into the u64 only when a single session is watched and refuse cross-session paging with a clear error listing the workaround). Choose the design that keeps one-session behavior byte-identical and multi-session honest; document in docs/history.md; test with two watched sessions and a paged walk that must not skip.
4. LOW: --to me / --with miss alias forms: recipients are ledgered as typed ('%1' vs the label 'reviewer'). Fix at the send end: canonicalize each resolved recipient to its label (pane id when unlabeled) before the msg line is written (delivery.rs:952-983 expand_recipients / :648-653); history then matches naturally. Test: send to '%N' of a labeled pane, assert history --with label finds it.
5. LOW: send-and-wait pins the occupant at wait start, not delivery submit (delivery.rs ~881-890). Fix: capture pane_pid on the DeliveryHandle at submit and pass it into wait_pinned for the send-and-wait path. Test: swap occupant between delivery resolution and wait start, assert occupant_changed rather than a report about the impostor.
6. Note-level, additive: agent.wait success payload gains "outcome": "reached" for shape symmetry with send-and-wait entries.
Update docs/{history,wait,hooks}.md where behavior changed, same change.`, { label: 'security-fixes', phase: 'Fix', schema: FIX_SCHEMA }),
  () => agent(`${COMMON}

TASK: the UX/copy blockers. You own crates/cyclops (hookset.rs, render.rs, copy.rs, their tests), docs/{hooks,send}.md, docs/GOALS-adjacent notes in STATUS only if asked (do NOT edit GOALS.md; it is admin text), scripts/commpact-shim (CI wiring only), .github/workflows/ci.yml.

1. HIGH: hooks selftest failure copy recommends a command that always fails: hookset.rs:300-307 renders "cyclops hooks install {target}" where target is the label/pane id, but install takes a CLI kind. The daemon already computes the bound manifest id (selftest.rs:294-302): add it to the selftest result additively (coordinate: the parallel agent owns cyclopsd; the field may already exist by the time you finish; if you need the one-line daemon change, make it ONLY in the selftest result assembly and note it, the parallel agent is told you may) and render "cyclops hooks install <manifest> --agent <target>". Test locks the exact string.
2. MEDIUM: hooks verify on an unlabeled pane says "no hooks declared" while listing declared hooks below (hookset.rs mapping None). Split the copy: manifest-bound-but-unadopted reads "hook tracking starts when the pane has a label" with the next step; truly no manifest reads "no hooks declared". Tests for both.
3. MEDIUM: selftest human output prints raw wire state ("delivered_unverified") instead of the badge voice: deserialize into DeliveryState and render via the same receipt badge path as send. Test.
4. LOW (GOALS alignment): GOALS says hollow check = unverified; today both badges use the identical solid ✓. Implement a weight pair: verified "✔ delivered · verified" (heavy check), unverified "✓ delivered · unverified (screen)" (light check), consistently across receipt_badge, history/thread badges, selftest, docs/send.md, docs/hooks.md, and the e2e goldens. Add one line to STATUS.md deviations noting: "hollow check" implemented as heavy-vs-light check weight because no portable hollow check glyph exists in terminal fonts; flagged for admin. Do NOT edit GOALS.md.
5. LOW: docs/hooks.md:61 writes "✓ delivered · unverified" missing "(screen)"; fix with the new glyph in the same pass.
6. LOW: reserved-label dead end: hookset.rs:152-155 refuses "%4" with no next step. Extend the copy with the naming step (config/registry path, same pointer render_status uses) then rerun install.
7. Shim CI: python3 scripts/commpact-shim/test_shim.py is not run by cargo test; add a CI job step (ubuntu+macos matrix job or a small dedicated job) running it, and a line in scripts/commpact-shim/README.
Keep every existing test green; goldens change only where the glyph pair lands.`, { label: 'ux-fixes', phase: 'Fix', schema: FIX_SCHEMA }),
])

phase('Verify')
const verify = await agent(
  `Final M2 gate verification in ${REPO} (read-only plus running tests; no edits, no git). Two fix agents just closed the M2 review BLOCKs. Their reports: SECURITY: ${security ? JSON.stringify(security.fixes || security.summary).slice(0, 2000) : 'MISSING'} UX: ${ux ? JSON.stringify(ux.fixes || ux.summary).slice(0, 2000) : 'MISSING'}

Verify ruthlessly with file:line evidence, running the relevant tests:
1. Forged agent.state.report over the socket from outside the pane: denied, no liveness, no ACK match; legitimate in-pane report still works (trace the socket dispatch to the peer-pinned resolution; confirm the in-process test path cannot be reached from the socket).
2. Liveness invalidated on occupant pid change; F1 downgrade notification fires post-swap.
3. Multi-session history paging: gapless or honestly refused; one-session behavior unchanged (compare against the pre-fix fixture test).
4. Alias canonicalization: send to pane id of a labeled pane, history --with label finds it, ledger carries the canonical name.
5. Send-and-wait pins the submit-time pid.
6. Selftest copy: install hint carries the manifest id; badge voice everywhere; verify-on-unlabeled split copy; reserved-label next step.
7. Glyph pair ✔/✓ consistent across send receipts, history, thread, selftest, and docs; STATUS.md carries the deviation note; GOALS.md untouched.
8. cargo test --workspace, clippy -D warnings, fmt --check: clean. Report totals.
9. Regression hunt in the diff since f1b0811 for anything the fixes broke.
Verdict PASS or BLOCK with per-item confirmation.`,
  { label: 'final-verify', phase: 'Verify', schema: {
    type: 'object',
    required: ['verdict', 'issues'],
    properties: {
      verdict: { type: 'string' },
      issues: { type: 'array', items: { type: 'object', required: ['severity', 'claim', 'evidence'], properties: { severity: { type: 'string' }, claim: { type: 'string' }, evidence: { type: 'string' }, fix: { type: 'string' } } } },
      test_totals: { type: 'string' },
    },
  } }
)

return { security, ux, verify }
