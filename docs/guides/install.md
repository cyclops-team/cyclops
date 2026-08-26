# Install

## Requirements

- macOS or Linux
- tmux 3.2 or newer (`tmux -V`)
- Rust toolchain (`cargo`), recent stable (1.85+): Cyclops builds from
  source, so this is a hard requirement, not a contributor extra
- curl and Git (for the one-line source install)

No Rust on the machine? Install it with [rustup](https://rustup.rs):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Use rustup rather than Homebrew or apt. A package-manager Rust pins the
compiler it shipped with and cannot switch toolchains; this build tracks
recent stable, and rustup's `rustup update stable` is what keeps one
current. You rarely need to run this yourself: when `cargo` is missing,
the installer runs this same rustup step on its own (non-interactive,
profiles untouched) and continues. `CYCLOPS_NO_RUSTUP=1` declines, and
the installer stops with the command to run by hand instead.

## Install

```bash
curl -fsSL https://www.usecyclops.dev/install.sh | sh
```

The hosted script is the same `scripts/install.sh` that this repository
tests. It clones the current `main` branch into a temporary directory,
builds both binaries, puts them where your shell looks, writes the config
and detection manifests, proves the result runs, and removes the clone.
It never uses sudo.

The build step dominates the install time: an optimized compile of the
two binaries and their dependencies, a few minutes on a fast machine and
noticeably longer on older or low-power hardware. The installer builds
the `dist` profile: release optimizations without the link-time
optimization pass, which would add minutes for runtime margin this tool
does not need. That cost returns on `cyclops update`, which rebuilds the
same way. Prebuilt binaries are not published yet.

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

### Taking it back off

```bash
curl -fsSL https://www.usecyclops.dev/install.sh | sh -s -- --uninstall
```

The managed uninstall removes both public binary links, the validated pair
store, and the installer-owned PATH block. It preserves `~/.cyclops`, including
the mailbox journal and session ledgers, and refuses an altered pair store or an
ambiguous install prefix. The full safety contract is in [Uninstall](#uninstall).

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

Installing this way does no setup. Run `cyclops start --setup-only --wire-hooks`
to write the config and manifests, merge Cyclops-owned hook entries, and seed
the agent skill for installed supported consumers. Omit `--wire-hooks` when you
want config and manifests only, then wire each agent explicitly.

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
  wrote 17 themes to /Users/you/.cyclops/themes
  wrote 4 detection manifests to /Users/you/.cyclops/manifests
```

Two things, and both matter. The config says which tmux sessions to watch.
The manifests are how cyclops tells what is running in a pane; without
them every pane reads `? unknown` and no message can be delivered to one.

`--setup-only` writes them and opens nothing. A plain `cyclops start`
writes the same files on its way to opening a workspace, so a machine that
has run either one is set up.

The installer passes one more flag, `--wire-hooks`, which extends setup
into the agent CLIs installed on the machine. It safely merges Cyclops hooks
into each installed vendor's default configuration, including
`~/.claude/settings.json` for normal direct Claude launches, and it places the Cyclops skill at
the canonical destination for each installed consumer:

- Claude Code: `~/.claude/skills/cyclops/SKILL.md`
- Codex and Cursor: `~/.agents/skills/cyclops/SKILL.md`
- Antigravity CLI: `~/.gemini/antigravity-cli/skills/cyclops/SKILL.md`

Codex and Cursor share one copy. Codex is installed when `$CODEX_HOME`, or
`~/.codex` when unset, exists; Cursor is installed when `~/.cursor` exists.
The shared destination alone does not trigger either consumer. Claude Code
requires `~/.claude`, and Antigravity CLI requires
`~/.gemini/antigravity-cli`. Setup creates no home for an absent
consumer and never seeds duplicate skill locations. It keeps current
and edited copies unchanged, and upgrades an unedited copy from a known
Cyclops release. The entire wiring step is skipped when
`CYCLOPS_NO_VENDOR_HOOKS` is set.

That consent outlives the run that gave it. `--wire-hooks` records it at
`~/.cyclops/vendor-wiring-consented`, and every later `cyclops` or
`cyclops start` finishes the wiring for an agent CLI that was not there
yet. Install Cyclops before an agent CLI and its skill still lands on the
first start after that CLI arrives, with a line saying what was placed.
A boot that finds nothing new writes nothing and says nothing.
Delete the marker file to withdraw the consent; `CYCLOPS_NO_VENDOR_HOOKS`
declines the step for one run without deleting anything.

Inspect the setup without changing it:

```
$ cyclops setup check
```

The check reports all four shipped manifests plus each supported consumer's
install state, hook wiring, required receipt tier, observed acknowledgment
capability, and canonical skill destination. Claude, Codex, and Cursor require
tier 1; AGY requires tier 2. An installed tier-1 consumer whose acknowledgment
cannot match a payload is incomplete rather than silently relabeled tier 2.
Its `mailbox` line predicts the terminal
transport from the same exact-skill check the daemon uses: `doorbell` or
`direct payload`. An operator-edited skill is preserved, but cannot prove the
claim contract and therefore selects direct fallback. It exits 0 when setup is
complete for the installed consumers and 1 when setup needs repair. Add
`--json` for a stable machine-readable report. The check reads setup state
only. Use `cyclops start --setup-only --wire-hooks` to install or repair it.

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

A file you edited is never rewritten, so your measurements survive every
run. A copy still byte-identical to a version Cyclops shipped is a seed
nobody touched, and a newer shipped version replaces it on the next run,
so an upgrade reaches an untouched home without a reinstall. Themes
follow the same rule.

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

Theme files are read from `~/.cyclops/themes`, which both `cyclops
start` and bare `cyclops` fill with the shipped set: the three identity
themes (dark, light, high-contrast), four ports (catppuccin,
tokyo-night, nord, gruvbox), six bright originals (sorbet, meadow,
periwinkle, blossom, seafoam, buttercream), and four dark originals
(midnight, ember, forest, obsidian). A theme you edited is never
rewritten, the same rule the manifests follow. Detection manifests land
in `~/.cyclops/manifests` on the same paths (`start` and bare `cyclops`),
because without them every pane reads unknown and nothing can be
delivered. With no theme files at all, cyclops renders in built-in colors.

Notification-worker and legacy direct-delivery tuning, with defaults shown:

```toml
ack_timeout_ms = 1500        # hook ACK window after a notification or legacy test write
delivery_retry_max = 1       # retries only when no notification or legacy payload bytes reached the pane
receipt_block_ms = 2500      # cap for observing an immediately decidable receipt
gate_hold_notify_ms = 120000 # one admin ping when a worker is held this long
```

Standard `cyclops send` acceptance is durable before notification scheduling.
When the cached pane verdict says the FIFO head can be decided immediately,
the response observes its first durable wake disposition for at most
`receipt_block_ms`. Working and otherwise held panes return their current state
without this wait. The deadline records no fact and never decides delivery
state. Keep it under 5000 because the CLI gives a socket read five seconds
before it calls the connection lost.

`delivery_retry_max` applies only to failures proven before a notification or
legacy payload write: detach or missing manifest, a pre-write occupant rebind,
and a spool or load-buffer failure. Verification, submit, post-write rebind,
or ACK failure may follow bytes that reached the pane, so the attempt ends in
`attention_required` with an exact cause and is never written again
automatically. Inspect before taking a recovery action. The durable mailbox
message remains available for an exact claim throughout.

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
`cyclops`: both start one when none answers, so whichever way you open a
workspace there is a daemon watching it. To run the daemon under your own
supervisor instead, `cyclops start --no-daemon` leaves it alone.

`start` opens the default workspace, building it from the `solo` preset
the first time. `--preset duo|quad|ops` picks a bigger one;
[workspaces.md](workspaces.md) covers saving and restoring your own.

Normal tracing goes to the bounded `$CYCLOPS_HOME/cyclopsd.log`, including
when the daemon runs in the foreground. Set `CYCLOPS_LOG=debug` for more and
use `cyclops daemon log` to read it. Stop with Ctrl-C or SIGTERM; the daemon
removes its socket and exits cleanly. Your tmux session is never modified by
watching it.

## Wire the hooks

Hooks give the daemon authenticated turn edges for state detection and safe
one-line notification. The self-test separately reports whether a legacy
direct-delivery acknowledgement hook fired:

```bash
cyclops hooks install claude --agent reviewer   # renders config + prints wiring
cyclops hooks selftest reviewer                 # proves the hooks actually fire
```

The standalone `cyclops hooks install` command only prepares an artifact under
`~/.cyclops/hooks/` and prints the wiring step. The main installer and updater
use recorded `--wire-hooks` consent to merge Cyclops-owned entries and seed the
agent skill while preserving unrelated vendor settings. Details and the Codex
trust caveat: [hooks.md](../reference/hooks.md).

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
Cursor Agent CLI. Cursor detection has fixture coverage, but the current-version
terminal notification path has not completed live validation because no current
Cursor binary was available on the evidence host. It fails closed when exact
evidence is unavailable; socket claims remain usable. See
[Known limits](../../STATUS.md#known-limits).
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
a throwaway home, checks the shapes this page quotes, runs `cyclops
update` against a local mirror one commit ahead (and once more for the
already-current path), then uninstalls and proves the shell profile came
back byte for byte. It is opt-in because it
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

## Update

```bash
cyclops update
```

One command, from anywhere, no one-liner to re-find. It prints the build
you are running and asks the source whether there is anything newer
(`git ls-remote`, one round trip, nothing fetched). Already current
stops right there and exits 0:

```
$ cyclops update
cyclops 0.1.0 (1e16081)
  source https://github.com/cyclops-team/cyclops.git at main
✔ already the latest main · nothing to update
```

Behind a newer commit, updating clones the repository and builds a candidate
pair. The candidate CLI and daemon must report one build, then the candidate
daemon must replay a private copy of the current journals before either
installed selector changes. Durable records and operator-edited setup files in
your home are preserved. Known unedited shipped themes, manifests, hook
artifacts, and skills may advance with the installed release.

```
  activated matched pair 1e16081
  no daemon was running; the selected pair remains stopped

✔ updated · 0.1.0 (0876ed7) → 0.1.0 (1e16081)

  an open workspace stays on the old build until you detach (ctrl+b d) and run cyclops again
```

The left of the arrow is the binary that ran the update; the right is
the freshly installed one answering `--version` for itself. Each release is a
directory containing the matched pair. The public commands pass through one
`active` selector, so there is no moment where a new CLI names an old daemon.
The previous matched pair remains as `known-good`.

If a daemon was running, update restarts that exact selected generation on the
new pair. If no daemon was running, update leaves it stopped; the next
`cyclops` or `cyclops start` starts the selected daemon.

Pair activation is the update commit point. If later home setup needs repair,
the matched pair remains active and the installer prints the exact
`cyclops start --setup-only --wire-hooks` repair command. It does not report a
generic update failure that implies activation never happened.

Before changing `active`, update asks the authenticated daemon to quiesce. It
stops only when the daemon's PID, kernel start value, boot id, and socket answer
still identify the same process. The new daemon starts from the selected pair.
If startup or build identity fails, update first proves that it can stop the
exact failed candidate. It then restores the prior selector and starts the
known-good daemon. If that ownership proof fails, update leaves automatic
rollback held and reports the exact recovery action instead of signalling or
replacing an unproven process.

On the first update that installs this behavior, the daemon still running
may not know the exact-generation shutdown handshake. Update refuses to stop
that daemon and prints the manual crossing: run `cyclops daemon stop` with the
old CLI, then rerun `cyclops update`. The old direct binary bytes remain
executable while both public names move behind the selector. If the old CLI and
daemon do not both expose the same source build, they are migration-only and
are not retained as known-good. The proven candidate is retained instead.
Every later update restarts on its own and retains the previous matched pair.

A delivery that stays mid-flight refuses the selector change. Nothing is
stopped and the active pair does not move.

To restore the retained pair explicitly:

```bash
cyclops update --rollback
```

Rollback validates the retained pair and proves it can replay a private copy
of the current journals before changing the selector. It does not copy
binaries, rewrite journals, restore earlier configuration, or promise
compatibility beyond that replay proof. A running daemon is quiesced and
restarted on the retained pair. A stopped daemon stays stopped.
After a legacy first migration, rollback becomes available after the next
matched update because no unproven legacy pair is advertised as known-good.

Reopening with `cyclops` (or any `cyclops start`) repairs prepared hook
artifacts under `~/.cyclops/hooks/` when their receipts prove they are still
unedited. A file you edited, or one with no receipt, is named and left alone.
The installer and updater run setup with vendor-wiring consent: Cyclops-owned
hook entries in installed agent configs and known unedited Cyclops skills may
be refreshed, while unrelated vendor entries stay unchanged. Set
`CYCLOPS_NO_VENDOR_HOOKS=1` to skip that wiring for one run.

`CYCLOPS_REPO` and `CYCLOPS_REF` pick the source, exactly as they do for
the installer; the defaults are the public repository's `main`. A build
from edited sources (`--version` ends in `.dirty`) or from outside git
(`unknown`) can match no commit, so it says so, skips the freshness
check, and updates anyway.

`cyclops hooks install` remains the explicit repair command for vendor hook
wiring outside install and update.

## Uninstall

The installer removes the binaries, the validated managed pair store, and its
PATH block but preserves your data. Pair-store removal holds the update lease
and refuses unknown, linked, or ownership-changing entries instead of deleting
an unproven tree. Both public names are removed only from one prefix, selected
by `--prefix` or by the `cyclops` command on `PATH`. It never resolves a
`cyclopsd` from another prefix. If only `cyclopsd` can be found, uninstall
refuses and asks for an explicit prefix:

```bash
curl -fsSL https://www.usecyclops.dev/install.sh | sh -s -- --uninstall
```

If you also want to delete all Cyclops configuration and records, stop the
daemon and copy out any history you want to keep before removing
`~/.cyclops`. Canonical mailbox journals are under
`~/.cyclops/workspaces/<workspace-id>/messages.ndjson`; session state and
legacy direct-delivery records are under `~/.cyclops/ledger/`. Copy the whole
Cyclops home if you need both.
