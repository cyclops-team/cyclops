export const meta = {
  name: 'm6-flow',
  description: 'M6: cyclops pipe, attention routing (admin notify on blocked/parked/done), --wait composition ergonomics',
  whenToUse: 'Launch after M5 is committed.',
  phases: [
    { title: 'Preflight' },
    { title: 'Implement' },
    { title: 'Integrate' },
    { title: 'Review' },
  ],
}

const REPO = '/Users/yahirh/projects/clops'

const COMMON = `You are implementing Cyclops v2 milestone M6 in ${REPO}. Read first, binding: ${REPO}/docs/GOALS.md (human layer: ping on blocked/done/parked, silent otherwise; background worker + attention routing flow), docs/DELIVERY.md, findings.md, CURRENT code. Hard rules as ever: isolated tmux for tests, no git, no network, zero polling, no em-dashes, fmt/clippy -D warnings/tests green. Docs are part of done (GOALS documentation rule): update every doc page your change makes stale in the same change set (docs/send.md gains pipe and --wait sections), and never document behavior that does not exist yet.

Return JSON: summary, api, tests_passed, test_output_tail, findings, concerns.`

const IMPL_SCHEMA = {
  type: 'object',
  required: ['summary', 'tests_passed'],
  properties: { summary: { type: 'string' }, api: { type: 'string' }, tests_passed: { type: 'boolean' }, test_output_tail: { type: 'string' }, findings: { type: 'array', items: { type: 'object', required: ['title', 'label', 'detail'], properties: { title: { type: 'string' }, label: { type: 'string' }, detail: { type: 'string' } } } }, concerns: { type: 'string' } },
}

phase('Preflight')
const pre = await agent(
  `In ${REPO}: 'cargo test --workspace 2>&1 | tail -3', 'git status --short', 'git log --oneline -3'. Report tests_pass, git_clean, notes. Read-only.`,
  { label: 'preflight', phase: 'Preflight', schema: { type: 'object', required: ['tests_pass', 'git_clean', 'notes'], properties: { tests_pass: { type: 'boolean' }, git_clean: { type: 'boolean' }, notes: { type: 'string' } } }, effort: 'low' }
)
if (!pre || !pre.tests_pass || !pre.git_clean) throw new Error('M6 preflight failed: ' + (pre ? pre.notes : 'no result'))

phase('Implement')
const [flow, attention] = await parallel([
  () => agent(`${COMMON}

TASK: cyclops pipe <from> <to> [--lines N] [--subject s] plus --wait ergonomics polish.
1. pipe: capture the tail of <from>'s pane (default N sensible, via pane.read recent), wrap it as a normal message ('[cyclops] FROM: <from> SUBJECT: <s or generated>' + the tail in a fenced block with an honest header line saying what it is and how many lines), deliver to <to> through the normal pipeline (all gates apply). Receipt semantics identical to send. The captured tail is message BODY (it enters the ledger like any message body; pane tails are conversation the admin already sees, not secrets; note this reasoning in the code comment).
2. --wait ergonomics per the brief: send --wait done default timeout sane and overridable; wait exit codes documented; a compact combined receipt ('✓ delivered · verified, then idle after 34s').
3. Tests: pipe round trip between fixture panes (content integrity, header honesty), pipe to a busy target queues, receipt strings, wait ergonomics e2e.`, { label: 'pipe-wait', phase: 'Implement', schema: IMPL_SCHEMA }),
  () => agent(`${COMMON}

TASK: attention routing: the background-worker flow (start it, get pinged only when blocked or done).
1. Routing rules (config, data-only): notify_admin = ["blocked", "parked", "attention", "done"?] default blocked+parked+attention. On a matching fused-state or delivery transition the daemon emits admin.notify (ledger system line + event). 'done' is per-request: cyclops send --notify-done marks that delivery so its turn-end pings once.
2. Delivery of pings to the human: the admin stream (M3 UI) already shows attention events; additionally a configurable notify_command (e.g. terminal bell into the admin pane, or osascript for macOS notification) executed with the subject as argv (data-only config, command allowlist documented, never shell-interpolated: use argv array). Off by default.
3. Rate discipline (GOALS: silent otherwise): dedupe repeat notifications for the same (pane,state) until it clears; a quota park pings once, not per retry attempt (there are no retries on quota anyway; prove with a test).
4. Tests: routing matrix (each transition class to notified-or-silent), dedupe behavior, notify_command argv safety (a subject containing shell metacharacters arrives as one argv element; assert no shell is involved), --notify-done round trip on a fixture pane.`, { label: 'attention-routing', phase: 'Implement', schema: IMPL_SCHEMA }),
])

phase('Integrate')
const integration = await agent(`${COMMON}

TASK: M6 integration. FLOW: ${flow ? flow.summary : 'MISSING'} ATTENTION: ${attention ? attention.summary : 'MISSING'}. Concerns: ${[flow, attention].filter(Boolean).map(r => r.concerns || '').join(' | ')}.
1. Workspace green; demos/m6-flow.sh: fixture worker pane, pipe its output to a reviewer pane, then a blocked state pinging the admin stream once.
2. ARCHITECTURE.md + CHANGELOG.md. Not STATUS.md.
3. Brief check with honest gaps.`, { label: 'integrate', phase: 'Integrate', schema: IMPL_SCHEMA })

phase('Review')
const review = await agent(
  `Adversarial review of M6 in ${REPO} (diff + untracked): pipe content passing through every delivery gate (no bypass paths), notify_command injection safety (argv only, no sh -c anywhere), notification dedupe (no ping storms), silence discipline (nothing pings on healthy transitions unless opted in), ledger completeness for pipes. file:line evidence, verdict PASS or BLOCK.`,
  { label: 'review-safety', phase: 'Review', schema: { type: 'object', required: ['verdict', 'issues'], properties: { verdict: { type: 'string' }, issues: { type: 'array', items: { type: 'object', required: ['severity', 'claim', 'evidence'], properties: { severity: { type: 'string' }, claim: { type: 'string' }, evidence: { type: 'string' }, fix: { type: 'string' } } } } } } }
)

return { pre, flow, attention, integration, review }
