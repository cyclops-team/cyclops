# Architecture review and deep audit method

**Status:** general, reusable method
**Basis:** `UNIFIED-CONTEXT.md`
**Applies to:** codebases, systems, features, integrations, migrations,
refactors, reliability work, performance work, and other substantial technical
decisions

This method helps answer a broad question:

> Does this design make sense as a correct, understandable, maintainable
> system, and what is the smallest safe path to improve it?

It is deliberately project-agnostic. It does not assume a programming
language, framework, repository shape, deployment model, or architectural
style. It is a thinking method, not a mandatory ceremony.

Use only the depth the work needs. A small, well-understood change may require
a short outcome statement, one trace, and a focused check. A consequential
change involving persistent state, several callers, external effects,
security, or difficult recovery may justify the full method.

## The method in one sentence

Define the desired outcome independently from the current implementation,
derive the guarantees and ownership needed to produce it, inspect the current
system as evidence, challenge it at its change and failure seams, and recommend
only improvements whose value earns their complexity.

## What a review should produce

A useful architecture review leaves behind enough clarity to act:

- the user or operator outcome;
- the important guarantees and accepted non-guarantees;
- the authoritative state and its owner;
- the main domain concepts and their responsibilities;
- the important modules, interfaces, seams, and adapters;
- representative success, failure, and recovery traces;
- verified findings separated from proposals and unknowns;
- a small, ordered change sequence;
- the tests or measurements that would prove the change; and
- explicit complexity that should not be added yet.

The result does not need to be a large document. It needs to make the important
decisions understandable to someone who was not already inside the system.

## Governing principles

### Current implementation is evidence, not authority

Existing code, diagrams, documents, tests, and workflows reveal current
behavior and past decisions. Their existence does not prove that the current
shape is correct or ideal.

Review in this order:

1. Define the desired outcome without using current source names.
2. Derive the minimum guarantees required for that outcome.
3. Describe the smallest plausible design that could provide them.
4. Inspect the implementation and compare it with that independent model.
5. Preserve existing machinery only when it protects a real guarantee,
   handles a credible fault, or reduces meaningful maintenance cost.

This prevents the review from becoming a tour of the current code.

### User experience is part of correctness

A technically valid operation can still be wrong for the user when the system:

- reports more confidence than the evidence supports;
- hides failure, delay, or uncertainty;
- makes recovery unclear or unsafe;
- presents stale state as current truth;
- requires users to understand internal machinery for ordinary work; or
- allows two official views to disagree about the same fact.

Review what people observe and decide, not only what internal functions return.

### Complexity must answer a named concern

Every retry, fallback, cache, queue, compatibility path, background worker,
configuration option, interface, and state adds another operating mode.

Ask:

> Which legal input, invariant, credible fault, security concern, measured
> need, or observed failure requires this machinery?

If the concern cannot be named, do not add the machinery preemptively. If the
concern is real, confirm that the proposed mechanism owns and verifies the
promised outcome.

### Modularity is about knowledge, not file count

Use these terms consistently:

- A **module** has an **interface** and an **implementation**.
- A module is **deep** when a small interface provides substantial behavior.
- **Depth** gives callers **leverage** and maintainers **locality**.
- A **seam** is a place where behavior can vary through an interface.
- An **adapter** is an implementation selected at a seam.

Many small files are not automatically modular. A large file is not
automatically wrong. The important question is how much knowledge a caller or
maintainer must carry.

Apply the deletion test:

> If this module disappeared, would its complexity disappear, or would the
> same knowledge spread across its callers?

A pass-through module adds naming without depth. A deep module earns its place
by keeping decisions, invariants, and change local.

### Domain language guides design without dictating structure

Domain-driven design is useful for finding stable names, responsibilities,
state owners, rules, and relationships. It is not a requirement to create a
public module for every noun.

Separate responsibilities when they have different owners, invariants,
dependencies, failure modes, or reasons to change. Keep behavior together when
separation would scatter one invariant or force callers to coordinate several
interfaces for one action.

The house analogy is a useful test:

- each room has a recognizable purpose;
- related activity may overlap without erasing ownership;
- movement between rooms is visible; and
- one room should not contain mutable access to the entire house merely for
  convenience.

Ask: which room owns this fact or decision, and why would a new engineer expect
to find it there?

### Reliability requires honest outcomes

Do not collapse every result into success or failure when the system may know
something more precise:

- rejected before work began;
- accepted or committed;
- partially completed;
- external effect requested;
- completed and verified;
- completed but not verified;
- outcome unknown; or
- recovery requires a person.

The exact states depend on the domain. The principle is general: public wording,
retry rules, persistence, and recovery must reflect what the system can really
prove.

## Evidence discipline

Label claims so readers can distinguish observation from recommendation:

| Label | Meaning | Support |
|---|---|---|
| Verified | Confirmed in current behavior, code, tests, or authoritative documentation | Exact source, trace, or reproducer |
| Measured | Observed under a named workload and environment | Revision, setup, method, and result |
| Historical | True for an older recorded state | Date or revision and scope |
| Inferred | Best explanation of verified facts | Reasoning and a plausible falsifier |
| Proposed | Desired future design or change | Outcome, tradeoff, and acceptance criterion |
| Unverified | Important assumption not yet established | Cheapest meaningful experiment |

Reviewer agreement is evidence of review, not proof. A passing test is evidence
about the behavior it exercises, not proof of the entire architecture.

## Review process

The phases below form a loop. A code trace may disprove the initial model. A
failure trace may reveal missing state. A prototype may show that a proposed
seam has no value. Revise the model when evidence changes it.

### 1. Calibrate the review

Before reading deeply, state:

- the decision or question being reviewed;
- affected users, operators, and maintainers;
- desired outcome;
- scope and non-goals;
- cost of failure;
- expected change horizon;
- available time and evidence; and
- whether the work is explanation, diagnosis, proposal, or implementation.

Increase review depth when the work involves irreversible data changes,
security, privacy, money, several teams, many callers, concurrency, external
effects, difficult recovery, or high migration cost.

Do not perform a deep audit merely because a checklist exists.

### 2. Establish authority and working state

Identify which sources are:

- governing instructions;
- current contracts;
- current implementation;
- measurements;
- proposals; and
- historical records.

Record the revision or artifact version being reviewed. Inspect uncommitted or
in-progress work before making changes. Preserve unrelated work.

When sources disagree, report the disagreement. Do not silently promote the
most convenient source to authority.

### 3. Define the outcome independently from the implementation

Describe representative journeys before mapping source files. Include the
ordinary path and the most important failure or recovery path.

For each journey, ask:

- What is the person trying to accomplish?
- What result do they need to trust?
- What does the system need to remember?
- Which actions are optional?
- What can fail independently?
- What feedback prevents confusion?
- What happens when the preferred path is unavailable?

Then derive the minimum guarantees. Anything beyond them must justify its
cost.

### 4. Model the domain and responsibility

Use stable domain language rather than current class or file names.

For each important concept, write a compact responsibility card:

| Field | Question |
|---|---|
| Purpose | Why does this concept exist? |
| Operational story | What sequence demonstrates its value? |
| Owned state | Which facts belong here? |
| Actions | Which changes may it perform? |
| Invariants | What must always remain true? |
| Does not own | Which nearby responsibilities belong elsewhere? |

Then describe important syncs between concepts:

| Trigger | Participants | Conditions | Facts exchanged | Result | User-visible outcome |
|---|---|---|---|---|---|
| What happened? | Who coordinates? | What must be true? | What crosses the seam? | What changes? | What does the person observe? |

A sync coordinates independent capabilities. It should not reach into their
internal representation or take over their responsibilities.

Before separating concepts, ask whether any facts must change atomically. Do
not split one invariant merely to make the diagram look cleaner.

### 5. Map the current system

Search before reading large files. Build an evidence map around questions, not
directories.

Locate:

- public entry points;
- authoritative state and derived views;
- mutations and commit points;
- external effects;
- important interfaces and adapters;
- shared mutable state;
- queues, caches, retries, and fallbacks;
- user-visible output and recovery actions;
- legacy callers and persisted history;
- regression tests and fault controls; and
- performance claims and their workloads.

The map should show where behavior is enforced, not merely where types are
declared.

### 6. Trace important behavior end to end

For each important journey, follow the complete path:

1. user or caller action;
2. input validation and authorization;
3. domain decision;
4. state mutation or durable commit;
5. requested external effect;
6. result and uncertainty;
7. user-visible feedback;
8. recovery or retry; and
9. verification.

At each step ask:

- What fact is authoritative?
- Who owns it?
- What can the caller observe?
- What can fail here?
- What becomes uncertain after waiting or crossing a process?
- Which test or measurement proves the claim?

Compare public contracts with enforcement. Wording that promises more than the
implementation can prove is an architectural defect.

### 7. Challenge failure, concurrency, and recovery

Apply this only to faults relevant to the system. Consider delay, loss,
duplication, reordering, timeout, partial execution, stale state, restart,
resource exhaustion, corruption, and independent failure.

For multi-step work, inspect these cuts when they exist:

- before commit;
- after commit but before response;
- before an external effect;
- after an external effect with an unknown result;
- after observation but before mutation;
- during restart or replay; and
- after a consumer misses an update.

Ask:

- May the caller retry?
- Is retry idempotent or protected from duplicate effects?
- Which state survives failure?
- Which uncertainty must stay visible?
- What detects and contains the fault?
- What returns the system to a defined state?
- Can useful work continue while one path is blocked?
- Can recovery itself introduce another fault?

Evaluate both safety and liveness. A system that avoids every risk by making no
progress is not reliable.

### 8. Evaluate modularity and simplification

For each module, inspect its interface rather than counting files:

- What must callers know?
- Does the module own the state and decisions required for its purpose?
- Are invariants local or reconstructed by callers?
- Does its interface expose internal representation?
- Would a change require edits across unrelated callers?
- Is the seam real, with meaningful variation, or hypothetical?
- Does the deletion test show real depth?

Strong deepening opportunities delete knowledge from callers. Weak refactors
only move lines, rename types, add pass-through interfaces, or split one
coherent rule across several modules.

Prefer the smallest change that improves locality and leverage. One adapter
usually means a seam is hypothetical. Two meaningful adapters make the seam
real.

### 9. Review user experience and operability

Use the same state and outcome model for implementation and presentation.
Avoid a second set of meanings invented only for display.

For every important result, ask:

- What happened?
- What is proven?
- What remains unknown?
- What is the consequence?
- Does the person need to act?
- What exact action is safe?

Ordinary work should expose domain concepts, not implementation machinery.
Advanced details should appear when they help explain or recover from a real
problem.

Check whether the system remains understandable when optional presentation,
automation, monitoring, or integration paths are unavailable. An optional
adapter should not become another source of truth.

### 10. Review tests and performance as evidence

The interface is the primary test surface. Choose the least expensive honest
seam for each claim:

- pure domain traces for rules and state transitions;
- adapter contracts for interchangeable implementations;
- focused integration traces for real process, storage, or network behavior;
- a small number of complete user journeys;
- performance and soak measurements for risks that require time or load; and
- manual or observational evidence when automation would be brittle or weaker.

A regression test should reproduce the defect, fail for the expected reason
before the fix, and protect a durable contract without disproportionate cost.
More tests are not automatically more confidence.

For performance work:

1. define the user-relevant outcome;
2. compare equivalent outcomes;
3. record the revision, environment, workload, and sample method;
4. measure latency distributions, throughput, resources, and incorrect
   outcomes as relevant;
5. change one meaningful variable; and
6. repeat the same workload while checking non-performance behavior.

Do not optimize an unmeasured path or compare a fast weak guarantee with a
slower strong guarantee as though they were equivalent.

### 11. Form findings and sequence change

Each finding should be understandable without the entire audit:

1. **Priority and recommendation strength.** Why now, later, or not yet?
2. **Evidence.** What is verified or measured?
3. **Consequence.** What can a user, operator, or maintainer experience?
4. **Explanation.** Which owner, invariant, interface, or seam is involved?
5. **Smallest coherent change.** What is the least change that improves the
   outcome?
6. **Regression proof.** What would prevent recurrence?
7. **Non-goal.** Which tempting extra work is not justified?

Use recommendation strength separately from priority:

- **Strong:** evidence shows a current correctness, comprehension, or
  maintenance problem.
- **Worth exploring:** likely useful, but a measurement, caller census, or
  prototype must precede commitment.
- **Speculative:** retain as a question, not planned work.

Sequence work so each step produces evidence and leaves the system coherent:

1. correct misleading contracts and confirmed defects;
2. create the smallest useful seam;
3. deepen one behavior family at a time;
4. migrate callers while preserving behavior;
5. remove superseded paths after a caller and history census; and
6. optimize only after measurement.

Keep a simple complexity budget:

- **Keep:** complexity that protects a proven guarantee.
- **Deepen now:** knowledge that is duplicated or spread across callers.
- **Measure first:** changes whose value depends on workload or scale.
- **Avoid now:** machinery without evidence that it is needed.

### 12. Validate the review

Match verification to the artifact and decision.

Recheck:

- every material finding against primary evidence;
- every current-behavior claim against the reviewed revision;
- every measurement against its workload and environment;
- every proposed seam against real variation or concentrated correctness;
- every removal against current callers and persisted history;
- every recommendation against the desired user outcome;
- contradictions between contracts, code, tests, examples, and presentation;
- document links, terminology, formatting, and worktree scope; and
- which important questions remain unverified.

An independent reviewer is useful when the decision is consequential. Ask
them to challenge assumptions, missing failure modes, over-splitting, and
unsupported confidence. Recheck their claims against primary evidence.

## Review stop conditions

A review has enough evidence to conclude when:

- the important user journeys have success, failure, and recovery traces;
- each important mutable fact has an understood owner;
- each claimed guarantee has an enforcement and verification path;
- retry and unknown outcomes are explicit where relevant;
- important interactions and atomic invariants are understood;
- simplification candidates pass the deletion test;
- performance claims compare equivalent outcomes;
- recommendations distinguish evidence, proposals, and unknowns;
- the change sequence can proceed in coherent, verifiable steps; and
- an engineer outside the work can explain the design without reading the
  entire implementation.

The review does not need a perfect final diagram before work begins. It needs a
stable outcome model, ownership model, invariant set, and safe way to learn
through implementation.

## Compact reusable checklist

### Frame

- [ ] State the outcome, users, scope, non-goals, and cost of failure.
- [ ] Calibrate review depth to risk and change surface.
- [ ] Record the reviewed revision and governing sources.
- [ ] Separate current contracts, implementation, proposals, and history.

### Model

- [ ] Describe ordinary, failure, and recovery journeys.
- [ ] Derive the minimum guarantees independently from current code.
- [ ] Name domain concepts, owners, actions, invariants, and non-ownership.
- [ ] Identify syncs and facts crossing each seam.
- [ ] Keep atomic invariants together.

### Inspect

- [ ] Trace important behavior end to end.
- [ ] Map authoritative, derived, cached, and ephemeral state.
- [ ] Compare public claims with enforcement and evidence.
- [ ] Inspect failure cuts, retry, recovery, safety, and liveness.
- [ ] Apply the house test and deletion test.
- [ ] Look for knowledge spread, shallow modules, and speculative seams.
- [ ] Review user feedback and operator recovery.

### Prove

- [ ] Use the least expensive honest verification seam.
- [ ] Compare performance at equivalent outcomes.
- [ ] Label verified, measured, historical, inferred, proposed, and unverified
  claims.
- [ ] Seek independent challenge when risk justifies it.

### Act

- [ ] Give each finding evidence, consequence, smallest change, proof, and
  non-goal.
- [ ] Separate priority from recommendation strength.
- [ ] Sequence correctness, deepening, migration, removal, then optimization.
- [ ] State what should be kept, deepened, measured, and avoided.
- [ ] Validate the final artifact and report remaining uncertainty.

## What this method deliberately avoids

- Treating the current design as correct because it exists.
- Starting with a class, crate, package, or deployment diagram.
- Creating one public module per domain noun.
- Equating many files with modularity.
- Adding interfaces, retries, states, or configuration without a named need.
- Splitting an invariant across modules to make responsibilities look tidy.
- Hiding uncertainty behind broad success or failure wording.
- Treating presentation problems as unrelated to correctness.
- Writing regression tests that cannot reproduce the original defect.
- Comparing performance across unequal outcomes.
- Deleting old behavior without understanding callers and persisted history.
- Treating reviewer agreement or passing checks as proof.
- Producing a large report when a short decision record would be enough.

## Final standard

The review succeeds when a person outside the immediate work can answer:

- What outcome does the system provide?
- What does it promise, and what does it not promise?
- Which facts are authoritative, and who owns them?
- Which module owns each important rule?
- What crosses each seam?
- What can fail independently?
- What does retry or recovery mean?
- What does the user observe and need to understand?
- Which complexity is necessary, provisional, or unjustified?
- What is the smallest safe change sequence?
- Which tests and measurements would prove the change?

If those answers require reading the entire implementation, understanding every
internal state, or trusting a diagram without evidence, the architecture review
is not finished.
