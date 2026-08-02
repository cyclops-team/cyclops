# Install

## Requirements

- macOS or Linux
- tmux 3.2 or newer (`tmux -V`); developed and tested on 3.6a
- Rust toolchain (`cargo`)

## Build

```bash
git clone <this repo> && cd clops
cargo build --release
```

Binaries land in `target/release/`: `cyclopsd` (the daemon) and `cyclops`
(the CLI). Put them on your PATH or call them by path.

## Configure

Cyclops keeps everything under `~/.cyclops` (override with `$CYCLOPS_HOME`).
Create `~/.cyclops/config.toml`:

```toml
# tmux sessions to watch
sessions = ["main"]

# where the per-CLI detection manifests live
manifest_dir = "/path/to/clops/manifests"
```

Optional keys (defaults shown):

```toml
tmux_socket = "name"    # tmux -L socket, for non-default servers
tmux_config = "/dev/null"  # tmux -f, mostly for tests
ack_timeout_ms = 1500        # tier-1 hook ACK window per delivery
delivery_retry_max = 1       # redelivery attempts after the first failure
receipt_block_ms = 2500      # receipt cap on the idle send path
gate_hold_notify_ms = 120000 # one admin ping when a delivery is held this long
```

Unknown keys warn and are ignored. The file is data; nothing in it executes.

## Run

```bash
cyclopsd &
cyclops status
```

Logs go to stderr; set `CYCLOPS_LOG=debug` for more. Stop with Ctrl-C or
SIGTERM; the daemon removes its socket and exits cleanly. Your tmux session
is never modified by watching it.

## Wire the hooks

Hooks turn receipts from screen-verified into hook-verified and give the
daemon instant turn edges:

```bash
cyclops hooks install claude --agent reviewer   # renders config + prints wiring
cyclops hooks selftest reviewer                 # proves the hooks actually fire
```

Install never touches vendor config directories; it prepares files under
`~/.cyclops/hooks/` and tells you the one command to wire each CLI.
Details and the codex trust caveat: [hooks.md](hooks.md).

## Verify

```bash
cyclops ping          # round trip time
cyclops status        # watched panes and their states
cyclops watch         # live events; Ctrl-C to stop
```

A pane shows `? unknown` when no manifest matches what is running in it.
The shipped manifests cover Claude Code, Codex CLI, and Antigravity CLI.

## Uninstall

Stop the daemon, then:

```bash
rm -rf ~/.cyclops
```

The ledger under `~/.cyclops/ledger/` is your message history; copy it out
first if you want the record.
