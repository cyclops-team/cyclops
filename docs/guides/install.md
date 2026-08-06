# Install

## Requirements

- macOS or Linux
- tmux 3.2 or newer (`tmux -V`)
- Rust toolchain (`cargo`)
- curl and Git (for the one-line source install)

## Install

```bash
curl -fsSL https://www.usecyclops.dev/install.sh | sh
```

The hosted script is the same `scripts/install.sh` that this repository
tests. It clones the current `main` branch into a temporary directory,
builds both binaries, puts them where your shell looks, writes the config
and detection manifests, proves the result runs, and removes the clone.
It never uses sudo.

To inspect the installer before running it, clone the repository instead:

```bash
git clone https://github.com/cyclops-team/cyclops.git
cd cyclops
./scripts/install.sh
```

Two binaries: `cyclops` (the CLI) and `cyclopsd` (the daemon).

### Where the binaries go

The installer prefers a directory your shell already searches, checking
`~/.local/bin`, `~/bin`, then `~/.cargo/bin`. Finding one means no profile
edit at all. Finding none, it uses `~/.local/bin` and adds one line to your
shell profile.

`--prefix DIR` overrides the choice:

```bash
curl -fsSL https://www.usecyclops.dev/install.sh | sh -s -- --prefix "$HOME/bin"
```

From a clone, use `./scripts/install.sh --prefix "$HOME/bin"`.

It never uses sudo. A prefix you cannot write to is an error with the fix
in it, not a password prompt.

### What it does to your shell profile

Only when the prefix is not already on your PATH, and never without saying
so. It copies the file to `<profile>.cyclops-backup.<timestamp>`, appends
three lines, and prints both:

```
== adding /Users/you/.local/bin to your PATH
  three lines added to /Users/you/.zshrc:

    # >>> cyclops >>>
    export PATH="/Users/you/.local/bin:$PATH"
    # <<< cyclops <<<

  the file as it was: /Users/you/.zshrc.cyclops-backup.20260803082117
  undo: cp "/Users/you/.zshrc.cyclops-backup.20260803082117" "/Users/you/.zshrc"    (or curl -fsSL https://www.usecyclops.dev/install.sh | sh -s -- --uninstall)
```

The markers are what make a second run a no-op instead of a second copy.
The file it picks follows `$SHELL`: `.zshrc` for zsh, `.bash_profile` or
`.bashrc` for bash, `config.fish` for fish. A shell it does not know gets
the line printed and no edit.

`--no-path` skips all of it and prints the line for you to add yourself.

### Uninstall

```bash
curl -fsSL https://www.usecyclops.dev/install.sh | sh -s -- --uninstall
```

Removes both binaries and takes the block back out of your profile,
backing it up again first. It leaves `~/.cyclops` alone and says how to
remove that too; the ledger under it is your message history.

From a clone, use `./scripts/install.sh --uninstall`.

### With cargo instead

```bash
cargo install --path src/cyclops
cargo install --path src/cyclopsd
```

Both go to `~/.cargo/bin`, and cargo warns when that is not on your PATH.
`cargo install --root ~/.local --path src/cyclops` writes
`~/.local/bin/cyclops` instead; the root is the prefix, not the directory,
and cargo appends `bin`.

Installing this way does no setup, so run `cyclops start --setup-only`
after it to write the config and the manifests.

To build without installing, `cargo build --release` puts both in
`target/release/`.

### Check your shell can find them

```bash
$ command -v cyclops cyclopsd
/Users/you/.local/bin/cyclops
/Users/you/.local/bin/cyclopsd
```

Two paths back means you are done. Nothing back means the shell has not
read the profile line yet: open a new shell, or `exec $SHELL -l`.

## Configure

Cyclops keeps everything under `~/.cyclops` (override with `$CYCLOPS_HOME`,
which has a length limit: see [below](#keep-cyclops_home-short)).
`./scripts/install.sh` already did this. The same step on its own, which is
what the installer calls and what a cargo install needs afterwards:

```
$ cyclops start --setup-only
✔ cyclops is set up
  wrote /Users/you/.cyclops/config.toml
  wrote 7 themes to /Users/you/.cyclops/themes
  wrote 4 detection manifests to /Users/you/.cyclops/manifests
```

Two things, and both matter. The config says which tmux sessions to watch.
The manifests are how cyclops tells what is running in a pane; without
them every pane reads `? unknown` and no message can be delivered to one.

`--setup-only` writes them and opens nothing. A plain `cyclops start`
writes the same files on its way to opening a workspace, so a machine that
has run either one is set up.

The long way, by hand. Create `~/.cyclops/config.toml`:

```toml
# tmux sessions to watch
sessions = ["main"]

# what `cyclops start` opens by default, see workspaces.md
default_workspace = "main"

# where the per-CLI detection manifests live; the default is
# ~/.cyclops/manifests and you rarely want anything else
manifest_dir = "/path/to/cyclops/resources/manifests"
```

`cyclops start` writes the first two keys and not the third: with nothing
there the daemon reads `~/.cyclops/manifests`, which is the directory it
just filled. Set `manifest_dir` only to point somewhere else, e.g. a
clone you are editing manifests in.

`sessions` is only the boot set. A running daemon can be asked to watch
another one -- `session.watch` on the socket, which the terminal workspace
UI calls whenever it creates a tmux session -- without touching this file
or restarting; a restart goes back to watching only what is written here.

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
theme = "dark"             # colors, see docs/guides/themes.md; unset picks dark too
chrome = "off"             # stop writing names onto tmux borders, see panes.md
```

`cyclops theme <name>` writes the `theme` key for you and leaves the rest
of this file alone, and `cyclops theme` on its own shows what each one
looks like. Editing the key by hand does the same thing.

Theme files are read from `~/.cyclops/themes`, which `cyclops start`
fills with the shipped set (dark, light, high-contrast, catppuccin,
tokyo-night, nord, gruvbox). A theme you edited is never rewritten, the
same rule the manifests follow. With no theme files at all, cyclops
renders in built-in colors.

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

`cyclops start` starts the daemon and waits for it, so it sees the exit
and reports it where you are:

```
$ cyclops start
✓ workspace ready · 1 agent
  cyclopsd could not start: bind /a/very/long/path/.cyclops/sock: path must be shorter than SUN_LEN
Its whole log is /a/very/long/path/.cyclops/cyclopsd.log.
```

and exits 1, because a workspace with no daemon can name nothing. Move
`$CYCLOPS_HOME` somewhere shorter and run it again.

This used to be a dead end: the daemon wrote that line to a stderr nobody
was reading, and every later command said `Check that cyclopsd is still
running, then retry`, which could never help because it never started.

## Run

```bash
cyclops start   # ✔ workspace ready · 1 agent
cyclops status
```

One command. `start` builds the session, starts cyclopsd when none is
running, waits for it to reach the session, and puts the workspace's names
on the panes. That is what the heavy check reports: a roster the daemon
confirmed, not a count read off a file.

The daemon it starts is detached, so it outlives the shell you typed in
and there is no tab to keep open. It logs to `$CYCLOPS_HOME/cyclopsd.log`.

```bash
cyclops daemon status   # ● cyclopsd is running · up 4m · pid 51230 · watching main
cyclops daemon log      # what it has written
cyclops daemon stop     # your tmux panes and the record are untouched
```

There is no `daemon start`: `cyclops start` is that, and so is bare
`cyclops` — both start one when none answers, so whichever way you open a
workspace there is a daemon watching it. To run the daemon under your own
supervisor instead, `cyclops start --no-daemon` leaves it alone.

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
Details and the codex trust caveat: [hooks.md](../reference/hooks.md).

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

  1 pane reads unknown: none of agy, claude, codex, cursor matches what is running there. Nothing can be delivered to an unknown pane. Pin one: cyclops name %0 <label> --manifest <id>. Teaching cyclops a new CLI is one file: docs/reference/MANIFESTS.md.
```

With no manifests at all it says that instead, because the fix is the
whole install and not one pane:

```
  1 pane reads unknown: cyclopsd loaded no detection manifests. Nothing can be delivered to an unknown pane. Install them and restart: cyclops start, then restart cyclopsd.
```

The shipped manifests cover Claude Code, Codex CLI, Antigravity CLI, and
Cursor Agent CLI.
Teaching it another one is a single TOML file:
[MANIFESTS.md](../reference/MANIFESTS.md). More symptoms and their next steps:
[troubleshooting.md](troubleshooting.md).

## Run the tests

```bash
cargo test --workspace --no-fail-fast
python3 scripts/commpact-shim/test_shim.py
./tests/e2e/parity-check.sh
```

`parity-check.sh` walks the README ladder against a throwaway tmux server
and fails if a line the docs quote is no longer what the binaries print.
`--with-installer` adds `scripts/install.sh` to the walk: it installs into
a throwaway home, checks the shapes this page quotes, then uninstalls and
proves the shell profile came back byte for byte. It is opt-in because it
does a release build, and CI runs it as its own job. The parity gate also
requires `website/static/install.sh` to be byte-for-byte identical to the
tested repository installer.

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

The installer removes the binaries and its PATH block but preserves your
data:

```bash
curl -fsSL https://www.usecyclops.dev/install.sh | sh -s -- --uninstall
```

If you also want to delete all Cyclops configuration and records, stop the
daemon, copy out any history you want to keep from `~/.cyclops/ledger/`,
then remove `~/.cyclops` yourself.
