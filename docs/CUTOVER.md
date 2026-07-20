# Cutover boundary

This tree is a staged package. The default local home is `~/.commPact`. Package commands do not modify the live tmux server, `~/.smux`, shell startup files, PATH, or tmux configuration.

## Operator sequence

1. Read the current team configuration and operational docs.
2. Broadcast the proposed labels and record acknowledgement from every pane whose label will change.
3. Take a fresh integrity backup before any live metadata change. Keep the prior bridge and metadata as the rollback path when one exists.
4. Create or adopt the live session, stamp `@commPact_*` metadata, and run an absolute-path smoke test in the intended session.
5. If verification fails, restore the saved metadata and prior bridge or configuration. Do not overwrite unrelated tooling.

The package installer does not change live tmux state. The configured operator owns adoption, rollback, and distribution decisions.
