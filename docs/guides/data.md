# Inspect and export durable records

Use this when you want to see the retained messaging record, keep a portable
copy before changing machines, or confirm what an uninstall leaves behind.
These commands run without a daemon and do not modify the source state. Find
them under Operations in `cyclops commands`; start with `cyclops data inventory`.

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
  retention  Cyclops preserves these records until a future explicit removal journey. This command does not delete, truncate, rewrite, or repair them.
  export     cyclops data export --to <new-directory>
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

Cyclops retains these append-only records until an explicit future removal
journey is available. The data commands do not delete, truncate, rewrite, or
repair them. The current installer uninstall likewise preserves the Cyclops
state home and its journals.
