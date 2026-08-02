export const meta = {
  name: 'm4-pane-ux',
  description: 'M4: cyclops name/list, live role•state titles and borders, layout presets, workspace save/restore, cyclops start',
  whenToUse: 'Launch after M3 is committed.',
  phases: [
    { title: 'Preflight' },
    { title: 'Implement', detail: 'naming/borders + workspace/layouts in parallel' },
    { title: 'Integrate' },
    { title: 'Review' },
  ],
}

const REPO = '/Users/yahirh/projects/clops'

const COMMON = `You are implementing Cyclops v2 milestone M4 in ${REPO}. Read first, binding: ${REPO}/docs/GOALS.md (pane niceness, layout presets 'designed not arranged', ladder law), the mission surface for M4 (cyclops name <target> <label>; cyclops list matching the landing-page shape: 'implementer  active  rate-limiter'; layout presets solo/duo/quad/ops where ops = 3 agents + docked stream pane at deliberate ratios; cyclops workspace save|restore persisting structure, labels, cwd, launch commands as a declarative tree restoring structure not live processes; cyclops start = restore-or-create default workspace, '✓ workspace ready — 3 agents'), findings.md, and the CURRENT code. Hard rules as ever: isolated tmux for ALL tests (-u -L unique -f /dev/null), no git, no network, zero polling, no em-dashes, fmt/clippy -D warnings/tests green.

CRITICAL SAFETY RULE for this milestone: M4 code writes INTO tmux sessions (titles, border formats, layouts). Production writes target ONLY panes/sessions the daemon watches and the admin asked to modify, and every tmux option write must be scoped (per-pane or per-session @options and format options, never server-global), reversible (cyclops name --clear restores), and recorded as a gate/system ledger line. Tests never touch the default server. The demo creates its own isolated session.

Docs are part of done (GOALS documentation rule): update every doc page your change makes stale in the same change set, and never document behavior that does not exist yet.

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
if (!pre || !pre.tests_pass || !pre.git_clean) throw new Error('M4 preflight failed: ' + (pre ? pre.notes : 'no result'))

phase('Implement')
const [naming, workspace] = await parallel([
  () => agent(`${COMMON}

TASK: naming, listing, and live pane chrome. You own the daemon registry/adoption surface, the name/list CLI verbs, and the border/title writer.
1. cyclops name <target> <label> [--manifest <id>] [--clear]: explicit pane adoption (v1 keeper): resolves target (pane id or current label), persists to the registry ($CYCLOPS_HOME, the mechanism M1 established; extend it), binds a manifest explicitly or keeps autodetect. Emits a system ledger line. --clear un-adopts and restores tmux chrome.
2. Live chrome: for adopted panes the daemon sets pane title and border format to 'role • state' with the theme's state glyph, updated on fused-state change (event-driven, no timers). Use per-pane/-session scoped options only; on daemon shutdown or --clear, restore what was there (snapshot the prior values at adoption). Respect a config switch chrome = "on"|"off".
3. cyclops list: the landing-page shape: label, state word, detail (current task hint: pane title if informative else blank), aligned grid, role colors, glyph+word. '✓ workspace ready — N agents' style summary lines belong to start, not list. --json parity.
4. Tests: adoption round trip (name, list shows it, ledger line written, --clear restores), chrome writes are scoped and reversible (read the option back, clear, read again), state change updates the border format string (isolated fixture pane), list rendering goldens.`, { label: 'naming-chrome', phase: 'Implement', schema: IMPL_SCHEMA }),
  () => agent(`${COMMON}

TASK: layouts, workspace persistence, cyclops start. You own the workspace module (daemon or CLI-side per your judgment; state lives under $CYCLOPS_HOME/workspaces/), the presets, and the start verb.
1. Presets solo/duo/quad/ops as declarative templates (data files in ${REPO}/layouts/, TOML: windows/panes tree, size ratios, roles, optional launch commands). ops docks a stream pane (running 'cyclops ui') at a deliberate ratio (document the ratio choice). Presets are tmux templates applied via the adapter crate; add a layout-apply helper to cyclops-tmux if needed (that crate is the only place tmux specifics may live).
2. cyclops workspace save [name]: capture the CURRENT watched session's structure (windows, panes, sizes as ratios, labels from the registry, cwds, running command per pane recorded as a launch hint) into a declarative TOML tree. restore [name]: recreate structure and labels in a NEW session (or the configured one if absent), set cwds, run recorded launch commands ONLY with --launch (restores structure, not live processes, per the brief), adopt panes per saved labels.
3. cyclops start: restore-or-create the default workspace: config key default_workspace; if the session already exists just ensure the daemon watches it; output '✓ workspace ready — N agents'. First run with no config: the guided three-step moment from GOALS (create session with solo preset, print the three next actions). 60s install-to-first-message is the bar this verb carries.
4. Tests: preset parse goldens; save/restore round trip on an isolated server (structure equality by ratios and labels); start idempotence (second run attaches, does not duplicate); first-run empty-state copy.`, { label: 'workspace-start', phase: 'Implement', schema: IMPL_SCHEMA }),
])

phase('Integrate')
const integration = await agent(`${COMMON}

TASK: M4 integration. NAMING: ${naming ? naming.summary : 'MISSING'} WORKSPACE: ${workspace ? workspace.summary : 'MISSING'}. Concerns: ${[naming, workspace].filter(Boolean).map(r => r.concerns || '').join(' | ')}.
1. Workspace green (fmt, clippy -D warnings, test --workspace).
2. demos/m4-workspace.sh: isolated server, cyclops start with the duo preset, name both panes, show list and status with live chrome, save the workspace, tear down, restore, show equality.
3. ARCHITECTURE.md + CHANGELOG.md. Not STATUS.md.
4. Brief check: name, list shape, presets incl. ops, save/restore, start. Gaps listed honestly.`, { label: 'integrate', phase: 'Integrate', schema: IMPL_SCHEMA })

phase('Review')
const review = await agent(
  `Adversarial review of M4 in ${REPO} (diff + untracked). Focus: tmux writes outside the adapter crate (forbidden), unscoped or irreversible option writes (server-global settings, missing restore-on-clear), chrome updates that poll instead of following events, workspace restore executing saved commands without --launch, cyclops start touching an EXISTING user session beyond watching it, ladder violations (does everything work with one unlabeled pane?). file:line evidence, verdict PASS or BLOCK.`,
  { label: 'review-safety', phase: 'Review', schema: { type: 'object', required: ['verdict', 'issues'], properties: { verdict: { type: 'string' }, issues: { type: 'array', items: { type: 'object', required: ['severity', 'claim', 'evidence'], properties: { severity: { type: 'string' }, claim: { type: 'string' }, evidence: { type: 'string' }, fix: { type: 'string' } } } } } } }
)

return { pre, naming, workspace, integration, review }
