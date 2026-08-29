# Archived: Cyclops Demo Day Public-Readiness Checklist

**Status:** Historical archive, not a current release gate

This checklist records the time-boxed public-readiness pass that preceded the
current release baseline. It is retained for historical context and is not an
active plan, release gate, or onboarding document.

## Execution policy and timebox

This checklist is being executed under a hard **2.5-hour / 150-minute wall-clock timebox**.

The objective is **not to complete 100% of checklist items**. The objective is to produce the strongest safe, understandable, installable, and demoable public Cyclops release possible within 150 minutes.

Prioritize by Demo Day impact, not checklist order.

### Role of the parent agent

The parent agent is the lead engineer/coordinator, not the default implementer.

Keep high-value reasoning in the parent agent:

- understand the checklist and repository architecture
- determine critical path and ordering
- make cross-cutting product/UX decisions
- enforce the already-decided Cyclops positioning
- identify dependencies and conflicts
- review all delegated changes
- run final integration and verification
- handle human checkpoints
- decide whether the final combined state is safe to freeze

### Delegation

Delegate implementation aggressively to cheaper subagents when work can be cleanly scoped and independently verified.

Good delegation targets include:

- documentation audits
- scoped README/docs edits
- website/docs consistency checks
- internal-material inventory
- secret/privacy audit
- community files
- link/path checks
- installer audit
- isolated cleanup

Each delegated task should include:

- narrow scope
- explicit files or ownership boundaries where possible
- acceptance criteria
- relevant repository context
- instruction not to modify unrelated files

Do not delegate the entire checklist to one subagent.

Parallelize independent tasks where safe.

Do not assign multiple subagents to edit the same files simultaneously.

Keep these primarily with the parent agent:

- final product/UX judgment
- final cross-repo consistency review
- verification semantics
- conflict resolution
- integration of concurrent branches
- release/freeze decisions

After delegated work returns, the parent must:

1. inspect the diff
2. verify it against current code and this checklist
3. fix it or send it back if necessary
4. run relevant targeted checks

Never accept a subagent's claim that something works without verification.

### Priority levels

#### P0 — must finish before Demo Day

Anything that can make a visitor fail to install, launch, understand, or see the core product:

- public clean install
- bare `cyclops` launch
- README first-run clarity
- natural-language / Cyclops skill onboarding
- Quickstart teaching the intended workflow
- docs matching actual behavior
- planned demo agents working
- real agent-to-agent handoff
- natural-language handoff where supported
- accurate verified vs unverified delivery semantics
- launch-blocking bugs
- actual secrets/private-information blockers
- license correctness
- required gates on the final candidate

#### P1 — do if time remains

High-value public readiness that must not jeopardize P0:

- `SECURITY.md`
- issue forms
- PR template
- critical link/path cleanup
- competitor/internal docs cleanup
- GitHub metadata recommendations
- release/tag preparation
- branch-protection preparation
- private vulnerability reporting

#### P2 — defer unless trivial

- analytics improvements
- new install-request tracking
- social-preview polish
- exhaustive advanced documentation
- nonessential cleanup
- aesthetic refactors
- governance polish
- new telemetry
- unrelated optimization
- anything explicitly deferred elsewhere in this checklist

Never spend significant time on P1/P2 while a P0 item remains unresolved.

### Wall-clock plan

Use elapsed wall-clock time, not checklist completion percentage.

#### T+0–10 minutes

- inspect repository state and authoritative instructions
- triage checklist
- identify P0 critical path
- delegate independent tasks
- immediately begin install / launch / demo-path verification

#### T+10–70 minutes

Focus overwhelmingly on P0:

- fix install/launch issues
- README
- Quickstart
- natural-language onboarding
- Cyclops skill/setup clarity
- demo handoff behavior
- review delegated changes as they return

#### T+70–105 minutes

- integrate P0 changes
- run targeted tests
- fix P0 failures
- allow high-value P1 work in parallel only if P0 is healthy

#### T+105–130 minutes

Build the release candidate:

- clean-install smoke test
- natural-language handoff smoke test
- live-demo smoke test
- broader required gates
- README/docs/site consistency review

#### T+130–140 minutes

Concurrent bug-fix integration window.

Do not guess which active branches should be included.

Ask the human which completed branches/PRs are approved.

Integrate only approved work and run affected tests.

If those branches are not ready, do not wait indefinitely for them.

#### T+140–150 minutes

No new non-blocking work.

Only:

- resolve actual Demo Day blockers
- run highest-value final checks
- review final diff
- produce final report
- establish freeze candidate

The human needs the remaining ~30 minutes before the event for physical setup and rehearsal.

### Deadline behavior

If work threatens the 150-minute deadline:

- reduce scope
- explicitly defer noncritical work
- prioritize correctness over completeness
- prioritize:

  `install → cyclops → agents → natural-language coordination → handoff → visible receipt`

- do not allow one difficult P1/P2 task to consume the remaining window
- do not delay freeze merely to make the checklist appear complete

At the end of the timebox, report incomplete work rather than continuing indefinitely.

---

## Goal

Prepare Cyclops for real public users arriving from Demo Day, GitHub, usecyclops.dev, or the public installer.

This is a release-readiness pass, not a product-expansion pass.

The target is:

- coherent positioning
- obvious first-run UX
- accurate public documentation
- reliable clean installation
- reliable live demo behavior
- safe open-source defaults
- a known-good release
- no obviously private/internal material exposed
- a frozen, tested Demo Day commit

Do not expand scope into new product features, architectural refactors, or speculative improvements unless they fix a genuine launch blocker.

---

# 0. Decisions already made

Do not reopen these decisions.

## Positioning

Product name:

> Cyclops

Current one-liner:

> One eye. Many agents. A single coordinated team.

Cyclops is an open-source coordination layer/workspace for coding agents running in the terminal.

The important product story is:

- run the coding agents you already use
- see their state in one workspace
- address agents by identity
- hand work between agents
- verify message delivery
- retain an auditable record of coordination

## Primary interactive entrypoint

The canonical interactive command is:

```bash
cyclops
```

Bare `cyclops` opens the full-screen workspace.

Do not redesign the public flow around `cyclops start`.

`cyclops start`, `cyclops send`, `cyclops wait`, `cyclops history`, etc. remain important underlying CLI/programmatic primitives, but they should not be presented as the primary day-to-day human experience.

## Human interaction model

A normal human user should understand that they can talk to their coding agents in natural language.

Example:

> Implement this change. When you're done, send it to the reviewer and ask them to review it.

The coding agent should be able to use Cyclops underneath to perform that handoff.

The docs must clearly distinguish:

1. the human interface
2. the agent/skill interface
3. the lower-level CLI/automation interface

## License

Replace the incorrect current copyright holder with:

```text
Copyright (c) 2026 Cyclops contributors
```

Keep the MIT License terms otherwise unchanged.

---

# 1. Parallel-work safety

Other agents may be fixing bugs on separate branches while this task is running.

* Work only in the dedicated Demo Day readiness branch/worktree.
* Do not checkout, reset, rebase, delete, or modify other agents' branches.
* Do not assume `main` is static.
* Preserve unrelated human and agent work.
* Avoid overlapping ownership where possible.
* Before editing high-conflict files, inspect whether another active branch is likely modifying them.
* Treat bug-fix branches as incoming dependencies, not work to overwrite.
* Do not force-push.
* Do not rewrite history.
* Do not delete branches.
* Do not discard uncommitted changes.

Likely high-conflict files include:

* `README.md`
* installer scripts
* docs navigation/config
* shared workspace code
* `Cargo.lock`
* website configuration

Before final verification/freeze:

1. identify approved bug-fix branches/PRs
2. integrate them into the readiness branch
3. resolve conflicts carefully
4. rerun the full required test suite
5. rerun public-readiness smoke tests on the combined state

---

# 2. Protect existing work before touching anything

Before editing:

* Read `AGENTS.md`.
* Read every authoritative development document it points to.
* Read current repo invariants/style/handoff documentation.
* Run `git status`.
* Identify the current branch/worktree.
* Inspect uncommitted changes.
* Inspect recent commits relevant to the launch.
* Determine which docs are authoritative vs generated summaries/planning material.
* Do not overwrite unrelated changes.
* Do not reset, clean, force checkout, or destructively rebase.

Create a short execution plan ordered by dependency and risk, then begin.

---

# 3. README: make the first 30 seconds excellent

The top of the README should immediately answer:

1. What is Cyclops?
2. Why would I use it?
3. What does it look like?
4. How do I install it?
5. What do I run?
6. How do I actually use it?

## Required above-the-fold content

Include:

* Cyclops logo/name using an existing canonical asset if available
* current one-liner:

  * `One eye. Many agents. A single coordinated team.`
* concise 1–2 sentence description
* strong current screenshot or short GIF if a real current asset exists
* public install command
* `cyclops` as the primary launch command
* `usecyclops.dev`
* docs link
* GitHub/source context where appropriate

Suggested product explanation:

> Cyclops is an open-source coordination layer for coding agents running in your terminal. Run the agents you already use, see what each is doing, hand work between them, verify message delivery, and keep the workflow on an auditable record.

Do not blindly use that sentence if current implementation details contradict any wording.

## First-use explanation

Immediately after install/open instructions, make this mental model obvious:

> Start the coding agents you already use and talk to them normally. Cyclops gives those agents a shared way to identify one another, exchange structured handoffs, and track delivery.

Do not make a first-time user believe they must personally type `cyclops send` every time they want agents to coordinate.

## README audit

Review the entire README for:

* stale commands
* stale screenshots
* old shell/Python implementation references
* claims that no longer match current Rust `main`
* `cyclops start` presented as the default interactive UI entrypoint
* planned features presented as built
* duplicate or overly technical onboarding
* broken links
* internal implementation details dominating the user story

---

# 4. Natural-language / agent-first onboarding

This is a launch blocker.

A real first-time user read the current documentation and concluded that they needed to manually use Cyclops terminal commands to coordinate agents.

Fix that mental model.

## Inspect the actual Cyclops skill implementation

Determine from the repository:

* where the Cyclops skill/instructions live
* how supported agents discover them
* whether installation is automatic
* whether the user must manually install/configure anything
* which agent tools currently support the skill
* what capabilities the skill teaches the agent
* how an agent learns to:

  * send a message
  * address another agent
  * reply
  * wait
  * inspect history
  * verify delivery

Do not invent behavior.

If skill setup is incomplete, awkward, or non-automatic, document the exact reality.

## Teach three interfaces explicitly

### A. Human interface

The human:

* installs Cyclops
* runs `cyclops`
* opens or arranges agents in the workspace
* talks to those agents normally in natural language
* watches agent state and coordination from the workspace

Example human instructions:

> Send your findings to the implementer.

> When you're done, ask the reviewer to review this.

> Have Codex review this change and report back.

> Send this context to the planner.

### B. Agent interface

Explain that the Cyclops skill/instructions teach the coding agent how to translate human intent into Cyclops operations.

Conceptually:

```text
Human:
"Finish this implementation, then ask reviewer to review it."

Implementer
    ↓
finishes task
    ↓
uses Cyclops
    ↓
Reviewer receives structured handoff
    ↓
Cyclops records delivery / receipt / state
```

The exact capabilities shown must match what is actually implemented.

### C. CLI / automation interface

Explain that:

```bash
cyclops send
cyclops wait
cyclops history
cyclops thread
...
```

are important primitives for:

* agents
* scripts
* CI
* debugging
* advanced users
* understanding what happens underneath

They are not required to be the normal human interaction model.

---

# 5. Rewrite the Quickstart around the real first-time experience

The Quickstart should begin with the path a normal user actually takes.

It should answer these questions in order:

1. How do I install Cyclops?
2. What command do I run?
3. How do I open my existing project?
4. How do I start Claude Code / Codex / Cursor / another supported agent?
5. How does Cyclops recognize the agents?
6. How do I name/address them?
7. What is the Cyclops skill?
8. Do I need to manually type `cyclops send`?
9. How do I ask one agent to hand work to another?
10. How can I tell the handoff arrived?
11. What does verified vs unverified mean?
12. When would I personally use the CLI commands?
13. Where do I go when something fails?

## Required first workflow

Include one natural-language-first end-to-end example.

For example:

### Human to implementer

> Implement the rate limiter change. When you're done, send it to reviewer and ask for a review.

Then explain:

```text
implementer works
        ↓
implementer uses Cyclops to send structured handoff
        ↓
reviewer receives task
        ↓
Cyclops records delivery evidence
        ↓
reviewer works
        ↓
reviewer can reply through Cyclops
```

Then optionally show:

> What happened underneath

with the relevant CLI primitives.

Do not lead with a wall of terminal commands.

---

# 6. Canonical public user flow

Make the following surfaces tell the same story:

* README
* Quickstart
* install guide
* website
* public docs
* installer completion output
* examples
* troubleshooting
* booth/demo instructions where stored in repo

Canonical interactive flow:

```text
Install
  ↓
cyclops
  ↓
open/start the coding agents you already use
  ↓
talk to those agents normally
  ↓
agents coordinate through Cyclops
  ↓
human watches state / handoffs / receipts
```

Document `cyclops start` separately where it is useful for:

* explicit workspace construction
* presets
* scripting
* automation
* session management
* advanced CLI workflows

Do not change working CLI behavior merely to make docs easier to write.

---

# 7. Public documentation audit

Review all public docs covering:

* installation
* first run
* Quickstart
* project setup
* workspaces
* tabs
* panes
* supported agents
* manifests
* agent detection
* unknown state
* naming/addressing
* Cyclops skill
* natural-language coordination
* sending/receiving handoffs
* structured messages
* verified delivery
* screen-inferred/unverified delivery
* hooks
* waiting for completion
* history
* threads
* audit record
* layouts
* themes
* troubleshooting
* uninstall
* known limitations

Every claim must match current code or tested behavior.

## Explicitly prevent false claims

Do not imply these exist if they remain unbuilt:

* automatic attention routing
* `cyclops pipe`
* generic autonomous DAG/project orchestration
* automatic arbitrary task routing
* skill dependency graph orchestration
* anything present only in planning documents

Future ideas must be clearly labeled as future work.

---

# 8. Public/internal document audit

Search the entire tracked repository for material that should probably not be part of the public product surface.

Search for:

* Herdr
* Smux
* other competitor names
* `competitor`
* `competitive`
* `alternative`
* `vs.`
* market analysis
* internal strategy
* private planning
* fundraising notes
* personal notes
* recruiting/job-search notes
* customer information
* private meeting notes
* stale planning documents that can be mistaken for current product docs
* internal design debates
* credentials
* tokens
* secrets
* internal URLs
* personal data

## Competitor/internal research

Do not automatically delete useful research.

Produce an inventory containing:

* file path
* what it contains
* whether it is linked from public docs
* whether a normal GitHub visitor could reasonably encounter it
* recommendation:

  * keep public
  * remove from public tree
  * move to a private/internal repo later

## HUMAN CHECKPOINT A

Before deleting or moving competitor/internal research, show the exact proposed file list and recommendation.

Wait for approval.

Important:

Removing a tracked file from `main` does not erase it from Git history.

Do not rewrite Git history today merely to hide ordinary internal research.

If an actual credential/secret is found, stop immediately and report its location/type without echoing the secret.

---

# 9. License

The current license attribution is incorrect.

Change only the copyright notice to:

```text
Copyright (c) 2026 Cyclops contributors
```

Keep MIT License text unchanged.

---

# 10. Installer audit

Audit the exact public path:

```bash
curl -fsSL https://www.usecyclops.dev/install.sh | sh
```

Verify:

* hosted script is the intended current installer
* website asset and repository installer are consistent where required
* no sudo is used
* expected binaries are installed
* PATH modification behaves as documented
* backups are made where documented
* config is seeded correctly
* manifests are seeded correctly
* existing user edits are not unexpectedly overwritten
* reinstall behaves safely
* uninstall behaves as documented
* installer output tells a first-time interactive user to run `cyclops`
* installer does not reinforce an outdated CLI-first mental model

---

# 11. Clean-install test

Perform the strongest safe clean-install test available.

Prefer:

* throwaway HOME
* isolated tmux server
* temp directories
* test harness
* disposable VM/container where appropriate

Do not damage the real local Cyclops state.

Test:

1. curl install
2. shell/PATH behavior
3. run `cyclops`
4. workspace renders
5. config/resources exist
6. agent manifests exist
7. reinstall
8. uninstall
9. reinstall again if useful

Record exactly what was tested.

Do not claim testing against a vendor CLI that was not actually installed.

---

# 12. Demo Day functional smoke test

Test the exact things visitors will see.

## Workspace

Verify:

* bare `cyclops` launches successfully
* workspace sidebar renders
* tabs work
* panes work
* pane splitting works if part of demo
* selection/focus works
* text remains readable
* mouse behavior used in demo works
* no obvious crash during normal interaction

## Agent detection

Where installed locally, test:

* Claude Code
* Codex
* Cursor Agent
* other agents actually planned for the demo

Verify:

* detection occurs
* idle/working transitions are plausible
* unknown is not silently presented as idle
* named agents remain addressable

## Handoff

Run at least one real end-to-end handoff.

Test:

* sender → recipient
* structured message
* message arrives
* receipt is recorded
* history/thread reflects it

## Verification semantics

Verify both conceptual cases:

```text
✔ delivered · verified
```

only when recipient hooks genuinely confirmed that exact message.

And:

```text
✓ delivered · unverified (screen)
```

when delivery is inferred from screen evidence.

Never make demo/docs imply verification when only screen evidence exists.

## Natural-language demo

Where the skill supports it, test the user-facing path:

Human tells one agent naturally:

> Send this to reviewer and ask them to review it.

Confirm the agent actually invokes the relevant Cyclops functionality and the recipient receives the handoff.

This is the most important onboarding smoke test.

---

# 13. Security/privacy audit

Audit tracked files and obvious recent history for:

* `.env` files
* API keys
* tokens
* passwords
* credentials
* private URLs
* secrets in test fixtures
* personal data
* internal credentials in docs
* accidental local paths revealing sensitive information
* CI secrets hard-coded into workflows
* frontend env values that should not be public

Use local/repo tooling where available.

Do not upload repository contents to a third-party scanner.

If no secrets are found, report what was actually checked rather than claiming a mathematically exhaustive scan.

If a real secret is discovered:

STOP.

Do not continue ordinary launch work until the secret has been addressed.

---

# 14. Open-source community basics

Add if absent or improve if inadequate.

## `SECURITY.md`

Include:

* current/pre-release status
* how to report a vulnerability privately
* what not to put in a public issue

Do not invent an email address.

If GitHub private vulnerability reporting is the intended path, document that appropriately once enabled.

## Issue forms

Add:

```text
.github/ISSUE_TEMPLATE/bug_report.yml
.github/ISSUE_TEMPLATE/feature_request.yml
```

Bug form should ask for:

* OS
* terminal
* tmux version
* Cyclops version/commit
* agent CLI involved
* reproduction steps
* expected behavior
* actual behavior
* logs where appropriate
* reminder to remove secrets

Feature request should be lightweight.

## PR template

Add:

```text
.github/pull_request_template.md
```

Include concise checks for:

* tests
* docs
* user-facing behavior
* backwards compatibility where relevant
* no unrelated changes

Do not overbuild governance today.

---

# 15. GitHub public surface

Audit repo metadata:

* repository description
* website URL
* topics
* social preview
* visibility
* default branch

Suggested topic categories where appropriate:

* ai-agents
* coding-agents
* developer-tools
* terminal
* tmux
* rust
* claude-code
* codex
* multi-agent

Do not spam irrelevant topics.

Check the GitHub landing experience as a stranger would see it.

---

# 16. Website / public docs consistency

Audit:

* `usecyclops.dev`
* install CTA
* docs CTA
* GitHub link
* screenshots
* one-liner
* product description
* supported-agent claims
* install command
* first-run command
* natural-language/skill explanation
* unbuilt feature claims

The website, README, and docs should not describe three different products.

---

# 17. Demo Day traffic tracking

Do not add broad product telemetry to the Cyclops binary today.

## QR traffic

Create a Demo-Day-specific destination or attribution mechanism.

Prefer something simple such as:

```text
https://usecyclops.dev/?utm_source=founders_inc&utm_medium=demo_day&utm_campaign=offseason_11
```

Use the actual canonical domain/path structure.

Make sure the QR destination works before displaying it.

## Installer tracking

Inspect current hosting/CDN infrastructure.

Determine whether `/install.sh` requests can already be counted through:

* hosting analytics
* CDN logs
* edge analytics
* server access logs

Only implement new request tracking today if it is:

* trivial
* server-side/edge-level
* privacy-respecting
* low-risk
* non-breaking
* does not modify installer semantics

Otherwise create a follow-up issue.

## Product telemetry

Defer PostHog or CLI telemetry until after Demo Day unless it already exists cleanly.

Any future telemetry should be:

* disclosed
* minimal
* privacy-conscious
* easy to opt out of

---

# 18. Release pinning

The public installer should not track an arbitrary moving `main` for public launch traffic.

Once a known-good commit exists:

* record exact SHA
* ensure blocking CI is green
* ensure clean install succeeds
* ensure Demo Day smoke test succeeds
* ensure docs correspond to that code
* ensure approved bug-fix branches are integrated

Then propose:

* release/tag name
* exact SHA
* release notes
* installer ref change

Likely release:

```text
v0.1.0
```

but derive the correct release strategy from current repo state.

## HUMAN CHECKPOINT B

Before:

* creating a tag
* pushing a release
* changing the public installer from `main` to a release/tag
* publishing a GitHub Release

show:

* proposed tag
* SHA
* installer diff
* test status
* concise release notes

Wait for approval.

After approval:

* pin installer
* rerun installer tests
* ensure hosted installer consistency
* create/push release only as approved

---

# 19. CI and required gates

Read the authoritative repository instructions and run all required gates.

At minimum, verify the current equivalents of:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace -E 'not package(cyclopsd)' --no-fail-fast
cargo test -p cyclopsd --all-targets --no-fail-fast
cargo test --workspace --doc
python3 scripts/check-doc-paths.py
./tests/e2e/parity-check.sh
```

Also run installer/website checks required by the current repo.

Do not blindly follow this copied list if authoritative repo docs have changed.

Record:

* command
* result
* failures
* fixes
* rerun result

No known failing blocking gate at freeze.

---

# 20. Main branch protection

Inspect actual workflow/check names before configuring anything.

Prepare rules for `main`:

* require pull request
* require at least 1 approval
* require blocking CI checks
* require conversation resolution if appropriate
* re-require approval after material updates if reasonable
* prevent force pushes
* prevent branch deletion
* preserve admin escape hatch only if intentionally desired
* keep intentionally advisory jobs non-blocking

Identify exact checks for:

* Linux
* macOS
* installer
* docs
* website
* required compatibility/E2E gates

Do not guess check names.

## HUMAN CHECKPOINT C

Before changing GitHub branch protection/rulesets, show:

* exact proposed settings
* exact required check names
* implications for a small team

Wait for approval.

Do not change:

* repository visibility
* collaborator permissions
* merge strategy
* branch protections
* rulesets

without approval.

---

# 21. Private vulnerability reporting

Check whether GitHub private vulnerability reporting is enabled.

If not:

* list it as a manual/admin action
* enable only with human approval if tooling permits

Do not invent a security email merely to satisfy `SECURITY.md`.

---

# 22. Accuracy pass for future/unbuilt features

Search README/docs/site for language implying currently available:

* pipe orchestration
* automatic attention routing
* generic graph execution
* autonomous workflow DAGs
* automatic skill dependency graphs
* automatic agent assignment/routing
* other roadmap ideas

Either:

* remove the claim
* correct it
* label it clearly as planned/future

Current public docs should describe what someone can actually use today.

---

# 23. Link/path validation

Check:

* README links
* docs links
* website → docs
* website → GitHub
* install link
* asset URLs
* screenshots
* navigation
* deprecated/old command references
* stale frontend paths after repository restructuring

Run automated path/link validation already present in repo.

Manually click the most important first-run links.

---

# 24. Stranger test

Simulate a visitor who knows nothing about Cyclops.

Starting from GitHub, they should be able to answer within roughly one minute:

* What is this?
* Why would I use it?
* Which coding agents does it work with?
* How do I install it?
* What command do I run?
* How do I open my project?
* How do I run agents?
* Do I have to manually type coordination commands?
* How do I tell agents to work together?
* How do I know a handoff arrived?
* Where do I go when something breaks?

If the docs require knowledge that only the authors currently have, fix that.

---

# 25. Integrate concurrent bug fixes

Before final freeze:

* identify active bug-fix branches/PRs
* determine which are approved for Demo Day
* inspect their diffs
* integrate them into the readiness branch
* resolve conflicts without discarding either side
* rerun the relevant targeted tests
* rerun the full required gates
* rerun clean install if installer/onboarding behavior changed
* rerun the live-demo smoke test

Do not freeze an earlier readiness commit while known approved Demo Day bug fixes remain unintegrated.

---

# 26. Final diff review

Before freeze, review every changed file.

Check for:

* accidental generated content
* AI-written fluff
* stale commands
* inconsistent terminology
* broken Markdown
* broken links
* excessive docs
* internal notes accidentally exposed
* unrelated refactors
* changed behavior without tests
* duplicated instructions
* copy that oversells the product
* incorrect agent support claims
* screenshots that no longer match UI
* `cyclops start` accidentally reintroduced as the primary interactive mental model
* natural-language/skill behavior described more broadly than actual implementation
* accidental conflict-resolution damage from concurrent branches

---

# 27. Final Demo Day freeze criteria

The repo is ready to freeze only when:

* README first screen is strong
* one-liner is correct
* bare `cyclops` is the canonical interactive entrypoint
* natural-language/skill workflow is clearly documented
* Quickstart works for a new user
* public install succeeds
* workspace launches
* planned demo agents are detected where available
* real handoff succeeds
* natural-language handoff succeeds where supported
* receipt semantics are accurate
* secrets/privacy audit has no unresolved blocker
* internal/competitor material has been reviewed
* license is corrected
* SECURITY/issue/PR basics exist
* docs/site/repo agree
* approved bug fixes are integrated
* known-good release decision is complete
* installer is pinned as approved
* blocking CI is green
* branch-protection plan is handled
* no launch-blocking known bug remains

At that point:

STOP MAKING NON-BLOCKING CHANGES.

Treat the repo as frozen for Demo Day.

Only fix a genuine launch/demo blocker after freeze.

---

# 28. Explicitly deferred until after Demo Day

Do not let these delay today's freeze:

* Homebrew
* Nix
* broad package-manager support
* PostHog/product telemetry
* sophisticated analytics
* generic graph orchestration
* skill dependency graph system
* automatic attention routing
* pipe orchestration
* larger architecture refactors
* extensive contributor governance
* perfect docs for every advanced command
* aesthetic refactors
* unrelated performance optimization

Create issues/follow-ups where useful.

---

# 29. Final report

When finished, report in exactly these sections:

## COMPLETED

List the meaningful changes made.

## VERIFIED

List:

* test/gate results
* clean-install result
* natural-language handoff result
* demo smoke-test result
* link/path checks
* concurrent bug-fix integrations

## PUBLIC UX

State exactly what a new user now does:

```text
install → cyclops → start agents → talk naturally → agents coordinate through Cyclops
```

Mention any setup required for the Cyclops skill.

## NEEDS HUMAN ACTION

List only unresolved items such as:

* competitor/internal file approval
* release/tag approval
* branch protection
* vulnerability reporting
* social preview
* QR destination
* external hosting analytics

## DEFERRED

List intentionally postponed work.

## KNOWN RISKS

List anything a first-time user could plausibly hit at Demo Day.

## FREEZE

State:

* exact commit SHA
* release/tag if created
* installer ref
* whether repository is safe to freeze

Once frozen, do not make further non-blocking changes.
