# Dependencies

External dependencies and what each is for. The Rust set is small and
deliberate; versions are pinned once in the workspace root `Cargo.toml`.

## Rust workspace dependencies

| Dependency | Used by | For |
|---|---|---|
| `tokio` 1 (full) | cyclops-tmux, cyclops-ui, cyclops-workspace, cyclopsd | Async runtime: control-mode child processes, Unix socket server, tasks, channels, one-shot timers, and signals. `test-util` is enabled in cyclopsd dev-deps |
| `serde` / `serde_json` | nearly all crates | Wire, ledger, registry, and layout (de)serialization — NDJSON everywhere |
| `toml` 0.8 | manifest, theme, cyclopsd, cyclops | Manifests, themes, daemon config, workspace layouts |
| `thiserror` 2 | library crates | One typed error enum per crate |
| `anyhow` 1 | cyclopsd, cyclops | Error context at application edges only |
| `tracing` / `tracing-subscriber` | manifest, tmux, ledger, cyclopsd | Structured logging; daemon stderr honors `CYCLOPS_LOG` |
| `regex` 1 | cyclops-manifest | Compiled detection-rule matchers |
| `clap` 4 (derive) | cyclopsd, cyclops | CLI parsing |
| `uuid` 1 (v4) | cyclopsd | boot_id and message/event ids |
| `libc` 0.2 | cyclops-ui, cyclopsd | termios raw mode, window-size ioctl, socket peer credentials |
| `ratatui` 0.30 / `crossterm` 0.29 | cyclops-workspace | Full-screen workspace layout, buffered rendering, terminal input, and terminal lifecycle |
| `alacritty_terminal` 0.26 | cyclops-workspace | Embedded pane VT state and escape-sequence processing |
| `unicode-width` 0.2 | cyclops-workspace | Terminal-cell width calculations |
| `tempfile` 3 | dev-deps only | Scratch dirs in tests |

The stream UI deliberately hand-rolls its small terminal layer, while the
full workspace uses Ratatui/Crossterm. There is no filesystem-watcher crate or
interval-timer machinery (the zero-polling contract), and no database (the
ledger is NDJSON and a 10k-line scan is single-digit milliseconds).

## System dependencies

| Tool | Needed for |
|---|---|
| tmux ≥ 3.2 | The product itself and most tests (developed on 3.6a; CI also builds tmux master as an advisory canary) |
| Rust stable toolchain | Building; CI uses `dtolnay/rust-toolchain@stable` with rustfmt + clippy |
| Python 3 | `scripts/check-doc-paths.py`, `scripts/commpact-shim/test_shim.py`, `tests/e2e/m1_soak.py`, `tests/e2e/test_vocab.py`, and three demos |
| jq | Demos that read the ledger back (`demos/m1-send.sh`, `demos/m2-conversation.sh`, `demos/m3-stream.sh`, `tests/e2e/parity-check.sh`) |
| POSIX shell | `demos/`, `scripts/install.sh` (`bash -n` must always pass on demos) |

## Website dependencies (`website/`, outside the workspace)

Dev-dependencies only — zero runtime dependencies, no UI library, no
Tailwind:

| Package | For |
|---|---|
| `@sveltejs/kit` 2 / `svelte` 5 | The framework |
| `vite` 8 + `@sveltejs/vite-plugin-svelte` | Build |
| `@sveltejs/adapter-auto` | Deploy adapter; the host is configured outside the repo |
| `typescript` + `svelte-check` | Static checking (`npm run check`) |

External services the site touches: the GitHub API (star count, 4s timeout,
graceful fallback) and Google Fonts (JetBrains Mono, Silkscreen).

## Version-compatibility posture

- tmux: version-specific behavior is a named predicate in
  `src/cyclops-tmux/src/version.rs` (e.g. bracket-paste flag ≥3.8,
  pause-after ≥3.2), never a call-site comparison. Unknown control-mode
  notification lines are data, not errors, for forward compatibility with
  tmux HEAD.
- Wire protocol: additive-only. New fields optional, unknown fields ignored
  in both directions, version mismatch warns.
- Vendor CLIs: all knowledge lives in `resources/manifests/` data files, so a vendor
  change is a manifest edit, not a release.
