# Inspect, export, and forget durable records

Use this when you want to see the retained messaging record, keep a portable
copy before changing machines, explicitly remove the journal scope, or confirm
what an uninstall leaves behind. Inventory and export run without a daemon and
do not modify the source state. To forget, stop and quiesce the daemon first,
then preview without changing anything and use that exact confirmation while it
remains stopped.
Find these commands under Operations in `cyclops commands`; start with
`cyclops data inventory`.

## See what Cyclops retains

Run `cyclops data inventory`.

It reports the exact file count and byte total for two durable categories:

- workspace journals under `workspaces/<workspace-id>/messages.ndjson`;
- session journals under `ledger/<session>.ndjson`.

Both are owner-only, append-only NDJSON records. Workspace journals can contain
message bodies. The inventory intentionally does not treat preferences, setup
files, or managed installation assets as durable message records; their
lifecycles have separate owners.

For an empty state home, it prints:

```text
durable record inventory
  workspace journals  0 files · 0 bytes
  session journals  0 files · 0 bytes
  scope      workspace and session NDJSON journals only; preferences, setup files, and managed installation assets are outside this export.
  ownership  Cyclops owns these append-only journals below its state home; workspace journals can contain message bodies.
  retention  Cyclops preserves these records until an explicit confirmed forget operation. Inventory and export never delete, truncate, rewrite, or repair them.
  export     cyclops data export --to <new-directory>
  forget     cyclops data forget --all
```

An incomplete inventory exits nonzero and names the affected category. Export
is refused in that state rather than silently omitting a record. Cyclops also
refuses both commands when its state root itself is not owner-only, because a
different local user could otherwise replace or add records during inspection.

## Make a portable copy

Choose a direct path whose parent already exists, is owned by you, and is not
writable by another user. The parent path must not contain a symbolic link, and
the new export path itself must not already exist. Then run:

```bash
cyclops data export --to <new-directory>
```

Cyclops creates that private directory and copies the selected journal bytes
without parsing, repairing, truncating, or changing their sources. It holds the
destination directories open while writing, so a later pathname replacement
cannot redirect copied records. If the selected destination no longer names
that held directory before completion, the command fails rather than trust the
replacement path. The result contains raw files in a directory named `records`,
preserving their relative paths, and a `manifest.json` that lists the category,
path, and copied byte count for each record.

The source is live, not paused for export. Cyclops captures each selected
file's identity and modification evidence, verifies it while copying, and
rechecks the complete selected set before completion. It refuses completion if
that evidence no longer matches. It is not a daemon-paused atomic cut: a daemon
can still append after the final recheck and before the command returns.

Before copying, Cyclops writes and syncs an `INCOMPLETE` marker and its export
directory. It removes that marker only after the copied files and manifest are
synced, the destination still names the held directory, and the marker itself
is still the one it created. It checks the destination name again after the
marker-removal sync, so a detected replacement in that completion window is
reported as uncertain rather than as success. A failure before marker removal
leaves the marker.
If the directory sync after removal fails, Cyclops reports completion as
uncertain rather than claiming either a complete export or a retained marker.
Do not rely on an export while the marker exists or while completion is
uncertain; inspect it or retry with another new destination. Source records
are never modified by Cyclops either way.

The current installer uninstall preserves the Cyclops state home and its
journals. Use the explicit journey below when you want to remove the journal
scope, or follow the [uninstall guide](install.md#uninstall) for the wider
manual removal of settings and vendor hooks.

## Forget the retained journal scope

`cyclops data forget --all` is an explicit, narrow removal operation. It
selects exactly the workspace and session NDJSON journals shown in its preview.
It does not remove preferences, layouts, setup files, logs, sockets, managed
assets, installed binaries, or vendor configuration. Export first if you may
need the records again.

First stop and quiesce the daemon. A graceful stop can append final journal
facts, so a preview made before it stops has a stale confirmation token:

```bash
cyclops daemon stop
```

Then preview the exact paths and byte total:

```bash
cyclops data forget --all
```

The preview changes nothing and prints a command containing its exact
confirmation token. Keep the daemon stopped and paste that command unchanged:

```bash
cyclops data forget --all --confirm <token-from-preview>
```

The confirmation applies only to the previewed files. Cyclops records a
private, content-free checkpoint before the first deletion and verifies every
file's descriptor-bound identity again before removing it. A running daemon,
an incomplete inventory, a journal locked by another writer, a changed path, a
replaced journal, or a mismatched token stops the operation rather than
broadening the scope.

The daemon and confirmed removal hold the same journal lease. Once the removal
has its lease, a daemon cannot start and write journals in the gap before
deletion; do not start it between the preview and confirmation, and keep it
stopped until the command reports its result.

If the process is interrupted after confirmation, run `cyclops data forget
--all` again. If it reports a pending checkpoint or a partial result, resolve
the reported condition and rerun the same exact confirmation. If it does not
report a pending checkpoint, make a fresh preview before confirming. Recovery
considers only the original previewed paths: files created after the preview
are left in place, and a changed planned file is left in place and reported as
partial. A recovery report distinguishes files removed by that invocation from
planned files that were already absent. Empty parent directories may remain.
