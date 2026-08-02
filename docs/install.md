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

Optional keys:

```toml
tmux_socket = "name"    # tmux -L socket, for non-default servers
tmux_config = "/dev/null"  # tmux -f, mostly for tests
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
