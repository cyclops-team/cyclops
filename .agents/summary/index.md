# Cyclops Knowledge Base Index

This directory is a generated knowledge base for AI assistants working on
the Cyclops codebase. Add **this file** to context first; it says which file
answers which kind of question so you can pull in only what you need.

## How to use this knowledge base

1. Start here. Each entry below says what its file contains and when to read
   it.
2. **The repo's own docs outrank these summaries.** Cyclops maintains
   authoritative, CI-enforced pages: `docs/development/HANDOFF.md` (the newcomer map),
   `docs/development/ARCHITECTURE.md`, `docs/reference/PROTOCOL.md`, `docs/development/DELIVERY.md`,
   `docs/development/INVARIANTS.md`, `CONTRIBUTING.md`, `docs/development/STYLE.md`, and one
   page per user-facing feature. These summaries condense and index them;
   when in doubt, follow the pointer to the real page.
3. Before *changing* anything, read `AGENTS.md` at the repo root — it lists
   the repo-specific gates a change must pass (several are unusual: docs are
   CI-verified against binary output, and doc paths are mechanically
   checked).

## Files in this knowledge base

| File | Contains | Read it when |
|---|---|---|
| [codebase_info.md](codebase_info.md) | What Cyclops is, languages, repo layout, the nine crates at a glance, runtime footprint, license, version status | You need basic orientation or a directory map |
| [architecture.md](architecture.md) | System diagram, crate dependency graph, ownership boundaries, the six deliberate design decisions and their rejected alternatives, concurrency model, error-handling philosophy | You're deciding *where* a change belongs, or why something is built the way it is |
| [components.md](components.md) | Per-crate detail: modules, key types, responsibilities, and what each crate deliberately does NOT own; plus non-crate components (frontend, manifests, themes, layouts, demos, scripts) | You're working inside a specific crate or asset directory |
| [interfaces.md](interfaces.md) | The NDJSON socket protocol and its methods, CLI verbs and exit codes, the vendor hook contract, the manifest TOML schema, on-disk file formats, environment variables | You're calling, extending, or debugging a boundary |
| [data_models.md](data_models.md) | `AgentState`, the 10-state delivery machine (with diagram), the ledger schema, wire envelope types, the attention register, config structs, identifier conventions | You're reasoning about state, the ledger, or what a field means |
| [workflows.md](workflows.md) | Runtime flows (boot, session watching, fusion, delivery send-to-receipt with sequence diagram, hooks, UI, workspace start, theme switch) and the dev/CI loops | You're tracing behavior end-to-end or setting up to contribute |
| [dependencies.md](dependencies.md) | External Rust crates and why each exists, system dependencies (tmux/python/jq), frontend packages, version-compatibility posture, what's deliberately absent | You're adding a dependency or checking compatibility constraints |
| [review_notes.md](review_notes.md) | Known gaps in this documentation, maintainer-relevant observations, and the consolidation decisions made (why README and CONTRIBUTING were preserved) | You're refreshing this knowledge base or auditing its accuracy |

## Relationships between files

- `architecture.md` is the hub: components, interfaces, data models, and
  workflows each expand one of its facets.
- `data_models.md` and `interfaces.md` describe the same types from two
  angles (what they mean vs. how they're spoken); both point into
  `src/cyclops-proto`, the single source of truth.
- `workflows.md` shows the types from `data_models.md` moving through the
  components from `components.md`.

## Example queries and where they resolve

- "Where does the delivery state machine live?" → data_models.md → `src/cyclops-proto/src/ledger.rs`.
- "Why isn't there a file watcher / interval timer?" → architecture.md (zero-polling decision).
- "How do I add support for a new agent CLI?" → interfaces.md (manifest schema) → `docs/reference/MANIFESTS.md`; no Rust required.
- "Why did my test kill the user's tmux?" → workflows.md (dev loop) → `CONTRIBUTING.md` testrig rules.
- "What must pass before a PR is green?" → workflows.md (CI section) or `AGENTS.md`.
- "Can I edit the frontend?" → components.md / review_notes.md: it's a read-only branding reference; don't, without an explicit request.

## Maintenance

Generated 2026-08-03 by static analysis. Regenerate after milestones land —
`STATUS.md`, `CHANGELOG.md`, and crate surfaces move together. The
`Custom Instructions` section of `AGENTS.md` is human-maintained and must be
preserved verbatim on regeneration.
