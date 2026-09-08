# What is worth doing next

Version `1.2.0` is being validated on branch **codex/release-1.2.0**. It is not released
until the operator approves publication. Pull requests target `main`; there is
no standing integration branch. Tagged binary publication now requires passing
release evidence for the exact commit.

The queue, roughly in order:

1. Measure a composer rule for each unverified manifest (Gemini CLI, Qwen
   Code, goose, OpenCode, Amp, Crush, aider) so a doorbell to those panes can
   detect a human draft. Until then a doorbell there is effectively raw.
2. Re-run the live vendor matrix for Claude Code, Codex CLI, Cursor Agent CLI,
   and Antigravity CLI at their current versions and refresh the evidence
   table in [STATUS.md](../../STATUS.md).
3. Decide whether `--require-wake` should accept `submitted_unverified`, and
   make the CLI, [PROTOCOL.md](../reference/PROTOCOL.md), and
   [send.md](../guides/send.md) say the same thing.
4. The doorbell pipeline still writes both the session-record `DeliveryState`
   chain and the workspace notification facts. Either retire the session
   chain or name it, in one place, as the per-attempt session record.
5. Audit the manifest `[injection]` keys the daemon no longer reads
   (`verify_pattern`, `verify_before_submit`, `safe_states`, `unsafe_states`,
   `clear_keys`, `busy_behavior`): drop them from the schema or mark them
   ignored in [MANIFESTS.md](../reference/MANIFESTS.md).
6. Keep replay-only notification states readable. Any removal needs an actual
   compatibility census and explicit migration path, not just fewer enum variants.
7. A retained benchmark harness for the doorbell lane itself, paste to
   receipt, so [BENCHMARKS.md](../reference/BENCHMARKS.md) can state that
   number from this repository.

Native tools and the optional Claude Code channel are described in
[native-apps.md](../guides/native-apps.md). Antigravity's sidecar probe still
needs verified per-conversation binding and a live round trip before it can
be advertised as a complete connector. Arbitrary existing Codex desktop chats
are not claimed as controllable through the app-server API.
