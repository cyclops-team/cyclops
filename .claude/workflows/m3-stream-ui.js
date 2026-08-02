export const meta = {
  name: 'm3-stream-ui',
  description: 'M3: cyclops ui stream (admin view + firehose), theme engine with semantic tokens, first theme, the eye',
  whenToUse: 'Launch after M2 is committed. The UI reads the socket and ledger only; no new daemon surface.',
  phases: [
    { title: 'Preflight' },
    { title: 'Implement', detail: 'ui core + theme engine in parallel' },
    { title: 'Integrate' },
    { title: 'Review', detail: 'GOALS frontend bar, 10k-entry fluidity' },
  ],
}

const REPO = '/Users/yahirh/projects/clops'

const COMMON = `You are implementing Cyclops v2 milestone M3 in ${REPO}. Read first, binding: ${REPO}/docs/GOALS.md (the frontend section is the spec for this milestone: the eye, strict grid, two encodings, semantic tokens, motion restraint, copy rules), ${REPO}/docs/ARCHITECTURE.md, findings.md, and the CURRENT code (daemon events, proto, the CLI style/render modules whose grid and badge voice you must match exactly). Hard rules as ever: isolated tmux only for tests (-u -L unique -f /dev/null), no git, no network, zero polling (the UI is event-driven on the subscription; ledger tail via fs watch or on-event refresh, no interval refresh), no em-dashes, fmt/clippy -D warnings/tests green. Docs are part of done (GOALS documentation rule): update every doc page your change makes stale in the same change set (docs/ui.md is yours to create when the ui verb lands), and never document behavior that does not exist yet.

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
if (!pre || !pre.tests_pass || !pre.git_clean) throw new Error('M3 preflight failed: ' + (pre ? pre.notes : 'no result'))

phase('Implement')
const [ui, themes] = await parallel([
  () => agent(`${COMMON}

TASK: crates/cyclops-ui (new lib crate; you own adding it to the workspace members/deps, ratatui + crossterm as workspace deps) plus the 'cyclops ui' subcommand wiring in crates/cyclops (main.rs dispatch only; do not restructure that crate).
1. Views: Admin stream default (ONLY messages addressed to admin/human plus attention events: blocked_*, parked, attention_required, gate holds, hook-unverified notices); Firehose on one keypress (tab or f): every message and state event live. Filter keys mirror the history flags (w with, f from, t to; a small input line). Enter on an entry jumps focus to that tmux pane (tmux select-pane/select-window via the daemon or direct tmux -u calls routed through cyclops-tmux; respect the adapter-only rule: add a tiny focus helper to cyclops-tmux if needed, nothing else).
2. Data: events.subscribe from the daemon (live) + ledger replay for backfill (cursor from the last N lines). The stream must stay fluid past 10k entries: ring buffer + windowed render, measured (write a bench-ish test rendering 10k synthetic entries and assert frame build under 16ms on this machine, report the number).
3. GOALS rendering bar: aligned timestamp gutter, hanging indents, role color + state glyph only, glyph+word pairs, badges in the exact M1 voice, density modes (c toggles comfortable/compact), no reflow jumps on arrival (new lines append below viewport unless pinned-to-tail), nothing blinks. Keypress latency under 50ms (event loop never blocks on the daemon: all IO on separate tasks feeding channels).
4. THE EYE: the signature attention indicator in the stream header: closed ( ‿ style, pick a clean glyph set and document it in the theme tokens) when calm, opening progressively when attention items exist (count shown beside it). It ticks between at most 2 frames on state change, never animates continuously (motion restraint). Also expose eye state as a plain word for --plain.
5. ? shows a cheatsheet overlay; q quits; --plain and NO_COLOR honored (degrade to a line-oriented follow mode: print events as they come, no TUI).
6. Tests: renderer unit tests with fixture entries (exact strings for a known width), the 10k fluidity measurement, filter logic, admin-vs-firehose classification (a message to admin shows in both, agent-to-agent only in firehose), and a headless integration test against the canned-daemon harness feeding events through a socket.`, { label: 'ui-core', phase: 'Implement', schema: IMPL_SCHEMA }),
  () => agent(`${COMMON}

TASK: the theme engine. You own crates/cyclops-theme (new small lib crate; add to workspace), themes/*.toml, and the migration of crates/cyclops/src/style.rs onto it (coordinate: the ui-core agent consumes your crate; land the crate API in your first hour: Theme::load(path), Theme::resolve(token) -> Color/Style, semantic token names as constants).
1. Token schema (GOALS: semantic, never raw colors in code): role.1..role.8 (stable palette slots the role hash maps into), state.idle/working/blocked/quota/dead/unknown, surface.bg/fg/dim/accent, badge.verified/unverified/queued/parked/attention, eye.calm/eye.alert, stream.gutter/subject/body. Values: truecolor hex with an explicit 256-fallback per token (fallback auto-derived if omitted; derivation documented and tested).
2. themes/dark.toml: the shipped default, matching the usecyclops.dev identity. The production landing page source is in-tree at ${REPO}/frontend/ (READ-ONLY branding reference: read its styles for the actual palette, never modify anything under it); map its colors onto the semantic tokens and document each choice in the file header.
3. Loader: data-only TOML, unknown tokens warn, missing tokens fall back to a compiled default table (the current style.rs palette becomes that table). Hot reload: a change to the active theme file applies on the next render (fs watch or mtime check on event, no interval polling; document which).
4. Migrate cyclops/src/style.rs to resolve through the theme (keeping its public fns stable so render.rs is untouched), theme selection via config key theme = "dark" plus CYCLOPS_THEME env override. cyclops-ui consumes the same crate.
5. Tests: token resolution, fallback derivation, hot-reload behavior, style.rs migration keeps every existing CLI render test green unchanged (that is the regression proof).`, { label: 'theme-engine', phase: 'Implement', schema: IMPL_SCHEMA }),
])

phase('Integrate')
const integration = await agent(`${COMMON}

TASK: M3 integration. UI: ${ui ? ui.summary : 'MISSING'} THEMES: ${themes ? themes.summary : 'MISSING'}. Concerns: ${[ui, themes].filter(Boolean).map(r => r.concerns || '').join(' | ')}.
1. Workspace green (fmt, clippy -D warnings, test --workspace).
2. demos/m3-stream.sh: isolated rig, fixture panes generating messages and a blocked state, cyclops ui in --plain follow mode captured to show the stream (TUI mode noted as manual: print the command for the admin to try in a real terminal).
3. ARCHITECTURE.md + CHANGELOG.md updates. Not STATUS.md.
4. Confirm against the brief: admin stream default, firehose toggle, filters, jump-to-pane, first theme. List gaps honestly.`, { label: 'integrate', phase: 'Integrate', schema: IMPL_SCHEMA })

phase('Review')
const review = await agent(
  `Adversarial UX + performance review of M3 in ${REPO} against docs/GOALS.md frontend section, reading the diff and running the renderer tests. Verify with evidence: two encodings only (grep for any third meaning-carrying channel), glyph+word pairing everywhere, badge voice byte-identical across CLI and UI (compare the strings), the eye present in header/status and restrained (no continuous animation code paths), 10k fluidity number, keypress path free of daemon IO, calm admin stream (classification rules), --plain completeness. file:line evidence, verdict PASS or BLOCK.`,
  { label: 'review-goals', phase: 'Review', schema: { type: 'object', required: ['verdict', 'issues'], properties: { verdict: { type: 'string' }, issues: { type: 'array', items: { type: 'object', required: ['severity', 'claim', 'evidence'], properties: { severity: { type: 'string' }, claim: { type: 'string' }, evidence: { type: 'string' }, fix: { type: 'string' } } } } } } }
)

return { pre, ui, themes, integration, review }
