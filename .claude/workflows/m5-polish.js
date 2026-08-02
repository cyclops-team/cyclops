export const meta = {
  name: 'm5-polish',
  description: 'M5: three themes with hot reload, docs polish, README quickstart on the progressive ladder, landing-page command parity',
  whenToUse: 'Launch after M4 is committed.',
  phases: [
    { title: 'Preflight' },
    { title: 'Implement', detail: 'themes + docs in parallel' },
    { title: 'Integrate' },
    { title: 'Review', detail: 'parity audit against usecyclops.dev shapes' },
  ],
}

const REPO = '/Users/yahirh/projects/clops'

const COMMON = `You are implementing Cyclops v2 milestone M5 in ${REPO}. Read first, binding: ${REPO}/docs/GOALS.md (themes as semantic tokens, copy rules, ladder law, 'default output should look like a product screenshot'), README.md (the landing-page command shapes recorded there: cyclops start '✓ workspace ready — 3 agents', cyclops send receipt '✓ delivered · verified', cyclops list three-column shape), findings.md, and the CURRENT code including crates/cyclops-theme and themes/. Hard rules as ever: isolated tmux for tests, no git, no network, zero polling, no em-dashes, fmt/clippy -D warnings/tests green. Return JSON: summary, api, tests_passed, test_output_tail, findings, concerns.`

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
if (!pre || !pre.tests_pass || !pre.git_clean) throw new Error('M5 preflight failed: ' + (pre ? pre.notes : 'no result'))

phase('Implement')
const [themes, docs] = await parallel([
  () => agent(`${COMMON}

TASK: ship the full theme set and prove hot reload. You own themes/, crates/cyclops-theme, and 'cyclops theme <name>'.
1. themes/light.toml and themes/high-contrast.toml joining dark.toml: every semantic token defined, 256-color fallbacks explicit, WCAG-ish contrast documented in each file header (high-contrast targets 7:1 for text tokens; state which pairs were checked and how). Same site identity family.
2. cyclops theme <name>: switches the config key, applies live (daemon notifies subscribers; UI hot-reloads per the M3 mechanism; CLI picks it up next invocation). cyclops theme with no arg lists themes with a one-line swatch preview per theme (glyph+word rows rendered in that theme).
3. Hot reload hardening: malformed theme edits fall back to the last good theme with one warning line, never a crash or a color explosion (test this).
4. Tests: token completeness across all three themes (a completeness test enumerating required tokens), fallback derivation, malformed-reload recovery, swatch renderer goldens.`, { label: 'themes', phase: 'Implement', schema: IMPL_SCHEMA }),
  () => agent(`${COMMON}

TASK: documentation to the done-well bar ('a stranger runs cyclops start, wires two agents, passes reviewed work between them, auditable months later'). You own README.md, docs/ (except GOALS.md and ARCHITECTURE.md structure), and demos/ additions.
1. README quickstart following the progressive ladder EXACTLY, one rung at a time, each rung showing a real command and its real output (run them against an isolated rig and paste actual output; the outputs must match what the binaries print today, this is a parity gate): 1 one pane + persistence + history, 2 name panes, 3 any terminal agent, 4 layouts, 5 structured messages with receipts, 6 pipe (mark rung 6 'coming in M6' if not yet shipped).
2. docs/QUICKSTART.md (expanded walk with the two-agent review-gate handoff flow), docs/MANIFESTS.md (how a new agent CLI becomes one TOML file: schema reference from cyclops-manifest source, the extensibility promise), docs/PROTOCOL.md (socket methods with request/response examples straight from cyclops-proto types; scripts can do anything the UI does).
3. Command parity audit: every command shape the README/landing shows must exist and print what the docs claim. Build a small script demos/parity-check.sh asserting each (grep-level assertions against --plain output on an isolated rig); it becomes a CI-able regression.
4. CHANGELOG.md entries current. Keep copy to GOALS rules: sentence case, plain verbs, no jargon facing humans, errors what/why/next.`, { label: 'docs-parity', phase: 'Implement', schema: IMPL_SCHEMA }),
])

phase('Integrate')
const integration = await agent(`${COMMON}

TASK: M5 integration. THEMES: ${themes ? themes.summary : 'MISSING'} DOCS: ${docs ? docs.summary : 'MISSING'}. Concerns: ${[themes, docs].filter(Boolean).map(r => r.concerns || '').join(' | ')}.
1. Workspace green; run demos/parity-check.sh and every existing demo script; all must pass.
2. ARCHITECTURE.md pointers current. Not STATUS.md.
3. Report the ladder walk yourself: follow README rung by rung on a fresh isolated rig, literally; every deviation between doc and behavior is a finding.`, { label: 'integrate', phase: 'Integrate', schema: IMPL_SCHEMA })

phase('Review')
const review = await agent(
  `Review M5 in ${REPO}: docs truthfulness (spot-check 10 documented outputs against real binary output on an isolated rig), theme completeness/contrast claims (verify two claimed contrast pairs by computing them), hot-reload recovery test quality, and GOALS copy rules across every new doc (no em-dashes, sentence case, no jargon facing humans, errors what/why/next). file:line evidence, verdict PASS or BLOCK.`,
  { label: 'review-parity', phase: 'Review', schema: { type: 'object', required: ['verdict', 'issues'], properties: { verdict: { type: 'string' }, issues: { type: 'array', items: { type: 'object', required: ['severity', 'claim', 'evidence'], properties: { severity: { type: 'string' }, claim: { type: 'string' }, evidence: { type: 'string' }, fix: { type: 'string' } } } } } } }
)

return { pre, themes, docs, integration, review }
