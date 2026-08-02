# Status

Updated 2026-08-02.

## Done

- Repo scaffold: Cargo workspace, five crates, CI workflow (fmt, clippy,
  test on ubuntu/macos, advisory tmux-HEAD job).
- `cyclops-proto`: full protocol v1 types (hello, request/response, events,
  all method params/results), ledger schema, delivery state machine with
  legal-transition table, agent state model. 7 unit tests.
- `cyclops-manifest`: manifest schema with compiled rules, region parsing,
  priority evaluation, modal decline actions (`decline_keys`,
  `auto_dismiss`). 7 unit tests including a gate that the shipped manifests
  always parse and classify the measured hazard screens correctly.
- `cyclops-tmux`: version probe with feature gates (`bracket_paste_flag`
  absent through 3.6a, amendment b). Control client and state table pending.
- Shipped manifests for claude/codex/agy seeded verbatim from the validation
  campaign drafts, plus machine-readable decline actions (amendment g):
  claude startup modal Escape, codex update dialog "3" then Enter, agy
  survey "0". Trust and permission prompts are marked never-auto-dismiss.
- docs/GOALS.md: admin quality bar recorded verbatim.

## Next

- M0 remainder: control-mode client (attach, reply correlation, notification
  parsing, pause-after at attach), zero-polling reconciling pane table
  (refresh-client -B subscriptions + notification hints, reconcile on
  doubt), cyclopsd socket server with ping/status/pane.read/events.subscribe,
  `cyclops status`, integration tests on isolated `-L` servers, demo script.

## Risks

- tmux 3.6a subscription behavior (`refresh-client -B`) is READ evidence
  only; M0 includes a probe test before the state table depends on it.
- CI is authored but unexercised: the repo has no remote yet (pushes need
  admin).

## Open questions

- License file: README says open source; admin picks the license before
  anything publishes.
- Whether `cyclops ui` ships in the `cyclops` binary or its own crate
  (decide at M3; leaning same binary, feature-gated).

## Deviations from the brief

None yet.
