# Install

## Requirements

- macOS or Linux
- tmux 3.2 or newer (`tmux -V`); developed and tested on 3.6a
- Rust toolchain (`cargo`)

## Build

```bash
git clone https://github.com/cyclops-team/cyclops.git && cd cyclops
cargo build --release
```

Binaries land in `target/release/`: `cyclopsd` (the daemon) and `cyclops`
(the CLI). Put them on your PATH or call them by path.

## Configure

Cyclops keeps everything under `~/.cyclops` (override with `$CYCLOPS_HOME`).
`cyclops start` writes the config on a first run, so the short way is:

```bash
cyclops start
```

The long way, by hand. Create `~/.cyclops/config.toml`:

```toml
# tmux sessions to watch
sessions = ["main"]

# what a bare `cyclops start` opens, see workspaces.md
default_workspace = "main"

# where the per-CLI detection manifests live
manifest_dir = "/path/to/cyclops/manifests"
```

`cyclops start` writes the first two keys and not the third: it does not
know where you cloned the repo. Without `manifest_dir`, the daemon looks
in `~/.cyclops/manifests` and then in `./manifests`, so starting cyclopsd
from the repo root works.

Four optional keys. The first two change how the daemon talks to tmux, so
add them only when you mean to. `theme` changes what every surface prints,
and `chrome` is the one switch that stops the daemon writing to your tmux
at all:

```toml
tmux_socket = "cyc"        # tmux -L socket; unset uses the default server
tmux_config = "/dev/null"  # tmux -f file; unset uses your own tmux config
theme = "dark"             # colors, see docs/themes.md; unset picks dark too
chrome = "off"             # stop writing names onto tmux borders, see panes.md
```

`cyclops theme <name>` writes the `theme` key for you and leaves the rest
of this file alone, and `cyclops theme` on its own shows what each one
looks like. Editing the key by hand does the same thing.

Theme files are read from `~/.cyclops/themes`, or from `./themes` in the
working directory (the repo layout). Copy the shipped ones in if you run
cyclops from anywhere else, otherwise it renders with built-in colors:

```bash
mkdir -p ~/.cyclops/themes && cp themes/*.toml ~/.cyclops/themes/
```

The tuning knobs, defaults shown:

```toml
ack_timeout_ms = 1500        # tier-1 hook ACK window per delivery
delivery_retry_max = 1       # redelivery attempts after the first failure
receipt_block_ms = 2500      # receipt cap on the idle send path
gate_hold_notify_ms = 120000 # one admin ping when a delivery is held this long
```

Keep `receipt_block_ms` under 5000. The CLI gives a socket read five seconds
before it calls the connection lost, so a longer receipt budget means
`cyclops send` reports a lost connection over a delivery that is going fine.
The delivery itself still completes and the record still shows it.

Unknown keys warn and are ignored. The file is data; nothing in it executes.

## Run

```bash
cyclops start   # ✓ workspace ready · 1 agent
cyclopsd &
cyclops status
```

The check is light because the daemon is not up yet, so that one agent is
a name in a file and nothing can be addressed. Run `cyclops start` again
after `cyclopsd &` and it goes heavy: the count is the roster then.

`start` opens the default workspace, building it from the `solo` preset
the first time. `--preset duo|quad|ops` picks a bigger one;
[workspaces.md](workspaces.md) covers saving and restoring your own.

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
Teaching it another one is a single TOML file:
[MANIFESTS.md](MANIFESTS.md). More symptoms and their next steps:
[troubleshooting.md](troubleshooting.md).

## Run the tests

```bash
cargo test --workspace --no-fail-fast
python3 scripts/commpact-shim/test_shim.py
./demos/parity-check.sh
```

`parity-check.sh` walks the README ladder against a throwaway tmux server
and fails if a line the docs quote is no longer what the binaries print.

`--no-fail-fast` is not optional: cargo stops at the first failing test
binary and hides every binary after it, which is how one portability bug
looked like a green build for two milestones.

Tests need tmux on PATH; the ones that need it skip cleanly without it.
Every test runs against its own tmux server (`-L cyc-<tag>-<pid>`), never
yours.

Throwaway test state goes under a short scratch root, because a Unix
socket path caps out near 104 bytes on macOS and the system temp dir there
is long. The root is `/private/tmp` on macOS and the system temp dir
elsewhere. Move it with `CYCLOPS_TEST_TMP`:

```bash
mkdir -p /private/var/tmp/cyc-relocated
CYCLOPS_TEST_TMP=/private/var/tmp/cyc-relocated cargo test --workspace --no-fail-fast
```

Use it when `/private/tmp` is not writable, and when you want to check
that nothing has hardcoded a path: a relocated run on macOS takes the same
code path Linux does. CI runs both.

## Uninstall

Stop the daemon, then:

```bash
rm -rf ~/.cyclops
```

The ledger under `~/.cyclops/ledger/` is your message history; copy it out
first if you want the record.
