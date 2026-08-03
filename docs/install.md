# Install

## Requirements

- macOS or Linux
- tmux 3.2 or newer (`tmux -V`); developed and tested on 3.6a
- Rust toolchain (`cargo`)

## Install

```bash
git clone https://github.com/cyclops-team/cyclops.git && cd cyclops
cargo install --path crates/cyclops
cargo install --path crates/cyclopsd
```

Two binaries: `cyclops` (the CLI) and `cyclopsd` (the daemon). Both go to
`~/.cargo/bin`. Cargo says so on the last line of each install, and warns
when that directory is not on your PATH.

To build without installing, `cargo build --release` puts the same two in
`target/release/`.

## Put them on your PATH

Check:

```bash
$ command -v cyclops cyclopsd
/Users/you/.cargo/bin/cyclops
/Users/you/.cargo/bin/cyclopsd
```

Two paths back means you are done. Nothing back means the shell cannot see
them, and `~/.cargo/bin` is on plenty of machines without being on the
PATH of any of them. Add it, then open a new shell:

```bash
# zsh
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.zshrc

# bash
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.bashrc
```

If you already keep binaries somewhere on your PATH, `cargo install
--root` puts them there instead of in `~/.cargo/bin`:

```bash
cargo install --root ~/.local --path crates/cyclops
cargo install --root ~/.local --path crates/cyclopsd
```

`--root ~/.local` writes `~/.local/bin/cyclops`. The root is the prefix,
not the directory; cargo appends `bin`.

## Configure

Cyclops keeps everything under `~/.cyclops` (override with `$CYCLOPS_HOME`,
which has a length limit: see [below](#keep-cyclops_home-short)).
`cyclops start` sets it up, so the short way is:

```
$ cyclops start
✓ workspace ready · 1 agent
  wrote /Users/you/.cyclops/config.toml
  wrote 3 detection manifests to /Users/you/.cyclops/manifests
```

Two things, and both matter. The config says which tmux sessions to watch.
The manifests are how cyclops tells what is running in a pane; without
them every pane reads `? unknown` and no message can be delivered to one.

The long way, by hand. Create `~/.cyclops/config.toml`:

```toml
# tmux sessions to watch
sessions = ["main"]

# what a bare `cyclops start` opens, see workspaces.md
default_workspace = "main"

# where the per-CLI detection manifests live; the default is
# ~/.cyclops/manifests and you rarely want anything else
manifest_dir = "/path/to/cyclops/manifests"
```

`cyclops start` writes the first two keys and not the third: with nothing
there the daemon reads `~/.cyclops/manifests`, which is the directory it
just filled. Set `manifest_dir` only to point somewhere else, e.g. a
clone you are editing manifests in.

### When the shipped set gains a manifest

Every `cyclops start` writes the manifests it ships and does not already
find, so a new one lands on your next start and says so:

```
  wrote 1 detection manifest to /Users/you/.cyclops/manifests
```

A file already there is never read, compared, or rewritten, so your edits
survive every run. The other side of that: a shipped manifest that changes
does not reach a copy you already have. Delete yours and run `cyclops
start` again to take the new one.

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

### Keep `$CYCLOPS_HOME` short

The daemon binds its socket at `$CYCLOPS_HOME/sock`, and a Unix socket path
caps out near 104 bytes on macOS. Measured there: a `$CYCLOPS_HOME` of 98
bytes binds, 99 does not. The default `~/.cyclops` is nowhere near it; a
home under a deep project directory or a macOS `/var/folders` temp path
can be.

Past the cap `cyclopsd` exits at boot:

```
boot failed: bind /a/very/long/path/.cyclops/sock: path must be shorter than SUN_LEN
```

`cyclopsd &` puts that on a stderr nobody is reading, so what you actually
see is the next command:

```
$ cyclops status
lost the connection to cyclops: path must be shorter than SUN_LEN. Check that cyclopsd is still running, then retry.
```

Checking cyclopsd does not help, because it never started. Move
`$CYCLOPS_HOME` somewhere shorter and start the daemon again. `cyclops
start` will not catch this for you: it writes the config and the manifests
and prints its usual next steps, because nothing it does needs a socket.

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

A pane shows `? unknown` when no manifest matches what is running in it,
and `cyclops status` says which of the two reasons it is:

```
$ cyclops status
‿ cyclops · watching main · tmux 3.6a · up 1s

  %0  ? unknown  zsh

  1 pane reads unknown: none of agy, claude, codex matches what is running there. Nothing can be delivered to an unknown pane. Pin one: cyclops name %0 <label> --manifest <id>. Teaching cyclops a new CLI is one file: docs/MANIFESTS.md.
```

With no manifests at all it says that instead, because the fix is the
whole install and not one pane:

```
  1 pane reads unknown: cyclopsd loaded no detection manifests. Nothing can be delivered to an unknown pane. Install them and restart: cyclops start, then restart cyclopsd.
```

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
