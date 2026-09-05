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
tests. It clones the current `main` branch into Cyclops's user-owned cache,
builds both binaries, puts them where your shell looks, writes the config
and detection manifests, proves the result runs, and removes the source clone.
It never uses sudo. Its reusable Cargo build cache is
`~/Library/Caches/Cyclops/installer/` on macOS and
`~/.cache/cyclops/installer/` on Linux, never `/private/var`.

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

The managed uninstall stops the validated selected daemon, removes the complete
current Cyclops state home, both public binary links, the validated pair store,
and the installer-owned PATH block. Export anything you want to retain first.
It refuses an altered pair store or an ambiguous install prefix. The full safety
contract is in [Uninstall](#uninstall).

From a clone, use `./scripts/install.sh --uninstall`.

### With cargo instead

```bash
cargo install --locked --path src/cyclops
cargo install --locked --path src/cyclopsd
```

Run both commands from the same checkout. `--locked` keeps Cargo from silently
replacing that checkout's resolved dependency set while it builds either binary.

Both go to `~/.cargo/bin`, and cargo warns when that is not on your PATH.
`cargo install --locked --root ~/.local --path src/cyclops` writes
`~/.local/bin/cyclops` instead; the root is the prefix, not the directory,
and cargo appends `bin`.

Installing this way does no setup. Run `cyclops start --setup-only --wire-hooks`
to write the config and manifests, merge Cyclops-owned hook entries, and seed
the agent skill for installed supported consumers when their private canonical
skill parent already exists. Omit `--wire-hooks` when you want config and
manifests only, then wire each agent explicitly.

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
  wrote 2 sounds to /Users/you/.cyclops/sounds
  wrote 52 detection manifests to /Users/you/.cyclops/manifests
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
`~/.claude/settings.json` for normal direct Claude launches. It may add the
final Cyclops skill file at the canonical destination for each installed
consumer:

- Claude Code: `~/.claude/skills/cyclops/SKILL.md`
- Antigravity CLI: `~/.gemini/antigravity-cli/skills/cyclops/SKILL.md`
- Kimi Code CLI: `~/.kimi-code/skills/cyclops/SKILL.md`
- Qwen Code: `~/.qwen/skills/cyclops/SKILL.md`
- Codex, Cursor, Gemini CLI, goose, OpenCode, Amp, and Crush:
  `~/.agents/skills/cyclops/SKILL.md`

Every CLI that reads `~/.agents/skills` shares that one copy, because a
vendor that reads two of its skill roots warns about the duplicate (Gemini
CLI 0.45.2 prints "Skill conflict detected" when `~/.gemini/skills` and
`~/.agents/skills` hold the same skill). A consumer counts as installed when
its own config directory exists: `$CODEX_HOME` or `~/.codex`, `~/.cursor`,
`~/.config/goose`, `~/.config/opencode`, `~/.config/amp`, `~/.config/crush`.
Gemini CLI is the exception: Antigravity CLI lives under
`~/.gemini/antigravity-cli`, so `~/.gemini` exists on every AGY machine, and
Cyclops instead requires `~/.gemini/tmp` (under `$GEMINI_CLI_HOME` when set),
which Gemini CLI creates the first time it starts and AGY never does. The
shared destination alone does not trigger any of them. Claude Code requires
`~/.claude`, Antigravity CLI `~/.gemini/antigravity-cli`, Kimi `~/.kimi-code`
(or `$KIMI_CODE_HOME`), and Qwen `~/.qwen` (or `$QWEN_HOME`). Setup creates
no home for an absent
consumer and never seeds duplicate skill locations. Skill seeding never
creates `.agents`, `skills`, or `cyclops` directories: it creates only a
missing final `SKILL.md` below an existing private canonical parent. A missing
or non-private parent is reported for manual review. It keeps current and
edited copies unchanged. An existing skill that matches a known older Cyclops
release is also preserved for manual review. The entire wiring step is skipped when
`CYCLOPS_NO_VENDOR_HOOKS` is set.

That consent outlives the run that gave it. `--wire-hooks` records it at
`~/.cyclops/vendor-wiring-consented`, and every later `cyclops` or
`cyclops start` finishes the wiring for an agent CLI that was not there
yet. Once that CLI has created its private canonical skill parent, the first
later start can add the final skill file and says what was placed. Otherwise
the plan and setup report manual review; Cyclops does not create the parent.
A boot that finds nothing new writes nothing and says nothing.
Delete the marker file to withdraw the consent; `CYCLOPS_NO_VENDOR_HOOKS`
declines the step for one run without deleting anything.

Inspect the setup without changing it:

```
$ cyclops setup check
```

The check reports every shipped manifest plus each supported consumer's
install state, hook wiring, receipt tier, observed acknowledgment capability,
and canonical skill destination. An installed tier-1 consumer whose
acknowledgment cannot match a payload is reported as incomplete rather than
silently relabeled tier 2. It exits 0 when setup is
complete for the installed consumers and 1 when setup needs repair. Add
`--json` for a stable machine-readable report. The check reads setup state
only. The standard setup workflow installs or repairs it.

Review the managed setup seed decisions:

```
$ cyclops setup plan
```

The plan is a read-only, body-free report. `--json` gives the same rows for a
script. Each row names the exact manifest or installed-consumer skill target,
its observed state, and the managed-asset decision: create a missing final
file below an accepted private parent, keep the current seed, preserve an
outdated released seed, preserve an operator edit, or leave an unreadable,
unproven, or unsafe target for manual review. A shared
`~/.agents/skills/cyclops/SKILL.md` alone does not make Codex or Cursor look
installed, and the report never creates a vendor directory. During setup,
Cyclops never creates the shared `.agents` root or any consumer skill-tree
directory; a missing or unsafe parent remains manual review.

This is intentionally not a generic setup dry run. It does not plan or change
config, hook wiring, themes, sounds, binaries, updates, cleanup, or uninstall.
This first slice has no apply capability yet.

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

A file already present is never rewritten, so your measurements survive every
run. A known old Cyclops seed is reported as outdated and stays in place for
manual review. Themes follow their own update rule; the shipped sounds are
written once and then left alone.

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

Doorbell tuning, with defaults shown:

```toml
ack_timeout_ms = 1500        # hook ACK window after the doorbell is submitted
delivery_retry_max = 1       # retries only when no bytes reached the pane
receipt_block_ms = 2500      # cap for observing an immediately decidable receipt
gate_hold_notify_ms = 120000 # one admin ping when a doorbell is held this long
```

Standard `cyclops send` acceptance is durable before notification scheduling.
When the cached pane verdict says the FIFO head can be decided immediately,
the response observes its first durable wake disposition for at most
`receipt_block_ms`. Held panes return their current state without this wait.
The deadline records no fact and never decides delivery state. Keep it under
5000 because the CLI gives a socket read five seconds before it calls the
connection lost.

`delivery_retry_max` applies only to failures proven before the paste: a
detached session, a missing manifest, a pre-write occupant rebind, or a spool
failure. A paste or submit command that failed, or a pane whose occupant
changed after the paste, may follow bytes that reached the pane, so the
attempt ends in `attention_required` with an exact cause and is never written
again automatically. A line that could not be read back is still submitted
once and recorded as `submitted_unverified`. The durable mailbox message
remains available for an exact claim throughout.

The 1.0 keys `ambiguous_composer_settle_ms`, `unclaimed_reminder_ms`,
`force_notification_submit`, and `force_notification_submit_delay_ms` no
longer exist. A file that still names one boots with a warning that says so.

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
cyclops stop            # stop Cyclops; your tmux panes and messages remain
```

If an earlier session left queued wake notifications for an agent, quiet those
wakes without deleting its durable inbox entries:

```bash
cyclops clear gemmy
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

Hooks give the daemon authenticated turn edges for state detection, and the
acknowledgement hook is how a doorbell earns a verified receipt. The self-test
sends one real doorbell through the mailbox path and reports whether that hook
fired:

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

  1 pane reads unknown: none of agy, aider, amp, claude, codex, crush, cursor, gemini, goose, kimi, opencode, qwen matches what is running there. Nothing can be delivered to an unknown pane. Pin one: cyclops name %0 <label> --manifest <id>. Teaching cyclops a new CLI is one file: docs/reference/MANIFESTS.md.
```

With no manifests at all it says that instead, because the fix is the
whole install and not one pane:

```
  1 pane reads unknown: cyclopsd loaded no detection manifests. Nothing can be delivered to an unknown pane. Install them and restart: cyclops start, then restart cyclopsd.
```

Supported agents come in three tiers. The first two ship a manifest in
[`resources/manifests/`](../../resources/manifests/); the third takes the
skill and nothing else.

**Verified.** Five manifests are measured against a live CLI. Their hooks are
wired from the templates in [`resources/hooks/`](../../resources/hooks/).

| CLI | manifest id | tier | binds the pane by | hooks Cyclops wires | skill file |
|---|---|---|---|---|---|
| Claude Code | `claude` | verified | process and argv | `~/.claude/settings.json` | `~/.claude/skills/cyclops/SKILL.md` |
| Codex CLI | `codex` | verified | process | `$CODEX_HOME/hooks.json` | `~/.agents/skills/cyclops/SKILL.md` |
| Antigravity CLI | `agy` | verified | process | `~/.agents/hooks.json` | `~/.gemini/antigravity-cli/skills/cyclops/SKILL.md` |
| Cursor Agent CLI | `cursor` | verified (fixtures) | argv | `~/.cursor/hooks.json` | `~/.agents/skills/cyclops/SKILL.md` |
| Kimi Code CLI | `kimi` | verified | process and argv | `~/.kimi-code/config.toml` | `~/.kimi-code/skills/cyclops/SKILL.md` |

**Unverified terminal CLIs with a manifest.** Written from vendor
documentation and marked so (`version_tested = "unverified"`): each binds the
pane, declares the hook events the vendor documents, and seeds the skill. A
doorbell to one of them never detects a human draft, so it is effectively a
raw write with a receipt until someone measures the composer. Gemini CLI,
Qwen Code, and goose are wired from `resources/hooks/` templates; every other
hook column below is wired from the manifest's own `[hooks.wiring]` table
([MANIFESTS.md](../reference/MANIFESTS.md#hookswiring-how-cyclops-writes-the-hook-file)),
the shape in parentheses. `none` means the vendor documents no shell hook
(a JavaScript or TypeScript plugin API, or nothing), so the manifest binds the
pane and seeds the skill only.

| CLI | manifest id | launch | hooks Cyclops wires | skill file |
|---|---|---|---|---|
| Gemini CLI | `gemini` | `gemini` | `~/.gemini/settings.json` (template) | `~/.agents/skills/cyclops/SKILL.md` |
| Qwen Code | `qwen` | `qwen` | `~/.qwen/settings.json` (template) | `~/.qwen/skills/cyclops/SKILL.md` |
| goose | `goose` | `goose session` | `~/.agents/plugins/cyclops/hooks/hooks.json` (template) | `~/.agents/skills/cyclops/SKILL.md` |
| AdaL | `adal` | `adal` | `~/.adal/settings.json` (claude-settings) | `~/.adal/skills/cyclops/SKILL.md` |
| aider | `aider` | `aider` | none | none (add the skill to `read:`) |
| Amp | `amp` | `amp` | none | `~/.agents/skills/cyclops/SKILL.md` |
| Auggie | `auggie` | `auggie` | `~/.augment/settings.json` (claude-settings) | `~/.augment/skills/cyclops/SKILL.md` |
| Autohand Code | `autohand` | `autohand` | `~/.autohand/config.json` (autohand) | `~/.autohand/skills/cyclops/SKILL.md` |
| IBM Bob Shell | `bob` | `bob chat` | `~/.bob/settings/settings.json` (claude-settings) | `~/.bob/skills/cyclops/SKILL.md` |
| Cline CLI | `cline` | `cline` | none | `~/.agents/skills/cyclops/SKILL.md` |
| CodeArts Agent | `codearts` | `codearts` | none | `~/.codeartsdoer/skills/cyclops/SKILL.md` |
| CodeBuddy Code | `codebuddy` | `codebuddy` | `~/.codebuddy/settings.json` (claude-settings) | `~/.codebuddy/skills/cyclops/SKILL.md` |
| Command Code | `commandcode` | `cmd` | `~/.commandcode/settings.json` (claude-settings) | `~/.commandcode/skills/cyclops/SKILL.md` |
| Continue CLI | `continue` | `cn` | `~/.continue/settings.json` (claude-settings) | `~/.continue/skills/cyclops/SKILL.md` |
| GitHub Copilot CLI | `copilot` | `copilot` | `~/.copilot/hooks/cyclops.json` (copilot) | `~/.copilot/skills/cyclops/SKILL.md` |
| Cortex Code | `cortex` | `cortex` | `~/.snowflake/cortex/hooks.json` (claude-hooks-file) | `~/.snowflake/cortex/skills/cyclops/SKILL.md` |
| Crush | `crush` | `crush` | none (`PreToolUse` only; see hooks.md) | `~/.agents/skills/cyclops/SKILL.md` |
| Deep Agents Code | `dcode` | `dcode` | `~/.deepagents/hooks.json` (claude-hooks-file) | `~/.deepagents/agent/skills/cyclops/SKILL.md` |
| Devin for Terminal | `devin` | `devin` | `~/.config/devin/config.json` (claude-settings) | `~/.config/devin/skills/cyclops/SKILL.md` |
| Dexto | `dexto` | `dexto --mode cli` | none | `~/.agents/skills/cyclops/SKILL.md` |
| Droid | `droid` | `droid` | `~/.factory/settings.json` (claude-settings) | `~/.factory/skills/cyclops/SKILL.md` |
| ForgeCode | `forge` | `forge` | none | `~/.forge/skills/cyclops/SKILL.md` |
| Grok Build | `grok` | `grok` | `~/.grok/hooks/cyclops.json` (claude-hooks-file) | `~/.grok/skills/cyclops/SKILL.md` |
| Hermes Agent | `hermes` | `hermes` | `~/.hermes/config.yaml` (hermes-yaml) | `~/.hermes/skills/cyclops/SKILL.md` |
| iFlow CLI | `iflow` | `iflow` | `~/.iflow/settings.json` (claude-settings) | `~/.iflow/skills/cyclops/SKILL.md` |
| Jazz | `jazz` | `jazz` | none | `~/.jazz/skills/cyclops/SKILL.md` |
| Junie CLI | `junie` | `junie` | `~/.junie/config.json` (claude-settings) | `~/.junie/skills/cyclops/SKILL.md` |
| Kilo CLI | `kilo` | `kilo` | none | `~/.kilocode/skills/cyclops/SKILL.md` |
| Kimchi | `kimchi` | `kimchi` | none | `~/.config/kimchi/harness/skills/cyclops/SKILL.md` |
| Kiro CLI | `kiro` | `kiro-cli` | `~/.kiro/agents/cyclops.json` (kiro-agent) | `~/.kiro/skills/cyclops/SKILL.md` |
| Kode | `kode` | `kode` | none | `~/.kode/skills/cyclops/SKILL.md` |
| Loaf | `loaf` | `loaf` | none | `~/.agents/skills/cyclops/SKILL.md` |
| MiniMax Code CLI | `mcode` | `mcode` | none | `~/.minimax/skills/cyclops/SKILL.md` |
| Neovate | `neovate` | `neovate` | none | `~/.neovate/skills/cyclops/SKILL.md` |
| OpenClaw | `openclaw` | `openclaw tui` | none | `~/.openclaw/skills/cyclops/SKILL.md` |
| OpenCode | `opencode` | `opencode` | none | `~/.agents/skills/cyclops/SKILL.md` |
| OpenHands CLI | `openhands` | `openhands` | none (repository-level `<project>/.openhands/hooks.json` only) | `~/.openhands/skills/cyclops/SKILL.md` |
| Posit Assistant TUI | `pa` | `pa` | `~/.posit/assistant/settings.json` (claude-settings) | `~/.posit/assistant/skills/cyclops/SKILL.md` |
| Pi | `pi` | `pi` | none | `~/.pi/agent/skills/cyclops/SKILL.md` |
| Qoder CLI | `qoder` | `qoder` | `~/.qoder/settings.json` (claude-settings) | `~/.qoder/skills/cyclops/SKILL.md` |
| Qoder CN CLI | `qodercn` | `qoderclicn` | `~/.qoder-cn/settings.json` (claude-settings) | `~/.qoder-cn/skills/cyclops/SKILL.md` |
| Reasonix | `reasonix` | `reasonix` | none | `~/.reasonix/skills/cyclops/SKILL.md` |
| Rovo Dev | `rovodev` | `acli rovodev run` | none | `~/.rovodev/skills/cyclops/SKILL.md` |
| Tabnine CLI | `tabnine` | `tabnine` | `~/.tabnine/agent/settings.json` (tabnine) | `~/.tabnine/agent/skills/cyclops/SKILL.md` |
| TraeCode CLI | `traecli` | `traecli` | `~/.trae-cn/hooks.json` (claude-hooks-file) | `~/.trae-cn/skills/cyclops/SKILL.md` |
| Mistral Vibe | `vibe` | `vibe` | `~/.vibe/hooks.toml` (vibe-toml) | `~/.vibe/skills/cyclops/SKILL.md` |
| Warp Agent CLI | `warp` | `warp` | none | `~/.agents/skills/cyclops/SKILL.md` |

**Skill-only products.** IDEs, desktop apps, and bots that read a skills
directory but run in no tmux pane: no manifest, no hooks. When the directory
in the middle column exists, `--wire-hooks` seeds the skill at the path on
the right. Zed reads the shared `~/.agents/skills` copy; Zenflow reads
Zencoder's directory and is the same entry.

| Product | installed when this exists | skill file |
|---|---|---|
| AiderDesk | `~/.aider-desk` | `~/.aider-desk/skills/cyclops/SKILL.md` |
| AstrBot | `~/.astrbot` | `~/.astrbot/data/skills/cyclops/SKILL.md` |
| Codemaker | `~/.codemaker` | `~/.codemaker/skills/cyclops/SKILL.md` |
| Code Studio | `~/.codestudio` | `~/.codestudio/skills/cyclops/SKILL.md` |
| Firebender | `~/.firebender` | `~/.firebender/skills/cyclops/SKILL.md` |
| inference.sh | `~/.inferencesh` | `~/.inferencesh/skills/cyclops/SKILL.md` |
| Lingma | `~/.lingma` | `~/.lingma/skills/cyclops/SKILL.md` |
| MCPJam | `~/.mcpjam` | `~/.mcpjam/skills/cyclops/SKILL.md` |
| Moxby | `~/.moxby` | `~/.moxby/skills/cyclops/SKILL.md` |
| Mux | `~/.mux` | `~/.mux/skills/cyclops/SKILL.md` |
| Ona | `~/.ona` | `~/.ona/skills/cyclops/SKILL.md` |
| Pochi | `~/.pochi` | `~/.pochi/skills/cyclops/SKILL.md` |
| Terramind | `~/.terramind` | `~/.terramind/skills/cyclops/SKILL.md` |
| Trae | `~/.trae` | `~/.trae/skills/cyclops/SKILL.md` |
| Windsurf | `~/.codeium/windsurf` | `~/.codeium/windsurf/skills/cyclops/SKILL.md` |
| ZCode | `~/.zcode` | `~/.zcode/skills/cyclops/SKILL.md` |
| Zencoder (and Zenflow) | `~/.zencoder` | `~/.zencoder/skills/cyclops/SKILL.md` |
| Zed | `~/.config/zed` | `~/.agents/skills/cyclops/SKILL.md` |

A CLI that runs under an interpreter reports `node` or `python` as its
command; Cyclops reads the script name behind the interpreter to bind it
([MANIFESTS.md](../reference/MANIFESTS.md) explains the measurement). When
that read fails, pin the manifest by hand:
`cyclops name %N <label> --manifest <id>`. Cursor detection has fixture
coverage, but the current-version terminal notification path has not
completed live validation because no current Cursor binary was available on
the evidence host; socket claims remain usable. See
[Known limits](../../STATUS.md#known-limits).
Teaching it another one is a single TOML file:
[MANIFESTS.md](../reference/MANIFESTS.md). More symptoms and their next steps:
[troubleshooting.md](troubleshooting.md).

## Run the tests

Install the same nextest release used in CI once:

```bash
cargo install cargo-nextest --locked --version 0.9.100
```

```bash
cargo build -p cyclops -p cyclopsd --bins
cargo nextest run --workspace -E 'not package(cyclopsd)' --no-fail-fast
cargo test -p cyclopsd --all-targets --no-fail-fast
cargo test --workspace --doc
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

`--no-fail-fast` is not optional: nextest must keep scheduling after a failure
so one run reports every failing test.

Tests need tmux on PATH; the ones that need it skip cleanly without it.
Every test runs against its own tmux server (`-L cyc-<tag>-<pid>-<sequence>`), never
yours.

Throwaway test state goes under a short scratch root, because a Unix
socket path caps out near 104 bytes on macOS and the system temp dir there
is long. The root is `/private/tmp` on macOS and the system temp dir
elsewhere. Move it with `CYCLOPS_TEST_TMP`:

```bash
mkdir -p /private/var/tmp/cyc-relocated
cargo build -p cyclops -p cyclopsd --bins
CYCLOPS_TEST_TMP=/private/var/tmp/cyc-relocated cargo nextest run --workspace -E 'not package(cyclopsd)' --no-fail-fast
CYCLOPS_TEST_TMP=/private/var/tmp/cyc-relocated cargo test -p cyclopsd --all-targets --no-fail-fast
```

Use it when `/private/tmp` is not writable, and when you want to check
that nothing has hardcoded a path: a relocated run on macOS takes the same
code path Linux does. CI runs both.

## Inspect health and cleanup candidates

`cyclops health` is read-only. It reports the selected client and daemon pair,
the authenticated running daemon, same-user daemon process inventory, durable
workspace and session mappings, configured and detached sessions, duplicate
watcher slots, setup files, state permissions, caches, logs, and rollback
proof. JSON callers receive the same facts under `operational` and `rollback`.
Health reports install-time replay evidence separately from current replay
readiness. `attested_snapshot` means the exact selected binary pair booted a
private state snapshot whose content-free identity is stored in the selection
record. `current replay unproven` is still expected during ordinary health
inspection. Cyclops proves the current journals again immediately before an
operator requests rollback.

When health finds a distinct validated rollback candidate, it names the
read-only next step `cyclops update --rollback`. Health does not run the
command. The command revalidates current journals before changing the selector,
so a candidate remains distinct from a rollback already proven safe.

```bash
cyclops health
cyclops --json health
```

Cleanup accepts only named rebuildable asset classes. The default is a dry run;
`--apply` removes only candidates that still pass the descriptor-relative,
no-follow ownership checks. It never accepts an arbitrary path and never
touches message journals. Before the first deletion, Cyclops durably writes an
owner-only checkpoint and locks the cleanup journal. Each execution then
appends one content-free `completed` or `interrupted` fact to
`~/.cyclops/operations/cleanup.ndjson`. The next applied cleanup recovers a pending
checkpoint exactly once, and a torn final journal record is discarded before
replay. Invalid complete records stop cleanup before deletion instead of hiding
journal damage.

```bash
cyclops cleanup build-cache
cyclops cleanup build-cache --apply
cyclops cleanup update-scratch --apply
```

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
cyclops 1.1.0 (1e16081)
  source https://github.com/cyclops-team/cyclops.git at main
✔ already the latest main · nothing to update
```

Behind a newer commit, updating clones the repository and builds a candidate
pair. The candidate CLI and daemon must report one build, then the candidate
daemon must replay a private copy of the current journals before either
installed selector changes. A running daemon is authenticated, quiesced, and
stopped before that copy is taken, then restarted on the old pair if replay
fails. Durable records and existing setup files in your home are preserved.
Known unedited shipped themes and hook artifacts may advance with the installed
release; manifests and skills remain in place and can report outdated state.

The source clone and candidate pair are temporary and disappear when the
update ends. The only retained build artifact is Cargo's incremental cache:
`~/Library/Caches/Cyclops/` on macOS and `~/.cache/cyclops/` on Linux. Cyclops
prints its exact directory before building; it is user-owned and rebuildable.
Set `CARGO_TARGET_DIR` if you deliberately want Cargo to use another location.

The selected-pair record stores a content-free replay attestation bound to the
exact client and daemon hashes plus the private snapshot identity. Older
selection schemas remain readable and report replay as unproven. The
attestation records installation evidence only. It never replaces the current
journal replay performed by rollback.

```
  activated matched pair 1e16081
  no daemon was running; the selected pair remains stopped

✔ updated · 1.0.1 (0876ed7) → 1.1.0 (1e16081)

  an open workspace stays on the old build until you detach (ctrl+b d) and run cyclops again
```

The left of the arrow is the binary that ran the update; the right is
the freshly installed one answering `--version` for itself. Each release is a
directory containing the matched pair. The public commands pass through one
`active` selector, so there is no moment where a new CLI names an old daemon.
The previous matched pair remains as `known-good`.

The number before the parentheses is the Cargo workspace version. The value in
parentheses is the exact source build. Cyclops compares the complete pair
identity during staging and checks the candidate daemon's greeting before
activation. These internal facts do not choose a public beta tag; naming and
publishing that tag remains a separate release gate.

The pair-store lease admits one updater. A concurrent updater exits before it
can stage, select, or repair files. If an updater process stops at a filesystem
commit boundary, the next update removes only validated temporary selectors
and unselected residues, repairs managed public links, and then continues. The
public client and daemon always resolve through the same selected pair.

If a daemon was running, update restarts that exact selected generation on the
new pair. If no daemon was running, update leaves it stopped; the next
`cyclops` or `cyclops start` starts the selected daemon.

Pair activation is the update commit point. If later home setup needs repair,
the matched pair remains active and the installer prints the exact
`cyclops start --setup-only --wire-hooks` repair command. It does not report a
generic update failure that implies activation never happened.

If a selector rename is visible but its directory sync cannot be confirmed,
update names that state exactly. It does not start a daemon or delete the
candidate until it has confirmed restoration of the prior selector; a later
update repairs only validated residue.

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

Rollback validates the retained pair, quiesces and stops the authenticated
daemon, then proves the retained pair can replay a private copy of the stable
current journals before changing the selector. A failed proof restarts the
unchanged active pair. Rollback does not copy
binaries, rewrite journals, restore earlier configuration, or promise
compatibility beyond that replay proof. A running daemon restarts on the
retained pair after the selector changes. A stopped daemon stays stopped.
After a legacy first migration, rollback becomes available after the next
matched update because no unproven legacy pair is advertised as known-good.

Reopening with `cyclops` (or any `cyclops start`) repairs prepared hook
artifacts under `~/.cyclops/hooks/` when their receipts prove they are still
unedited. A file you edited, or one with no receipt, is named and left alone.
The installer and updater run setup with vendor-wiring consent: Cyclops-owned
hook entries in installed agent configs may be refreshed, while existing
Cyclops skills and unrelated vendor entries stay unchanged. Set
`CYCLOPS_NO_VENDOR_HOOKS=1` to skip that wiring for one run.

`CYCLOPS_REPO` and `CYCLOPS_REF` pick the source, exactly as they do for
the installer; the defaults are the public repository's `main`. A build
from edited sources (`--version` ends in `.dirty`) or from outside git
(`unknown`) can match no commit, so it says so, skips the freshness
check, and updates anyway.

`cyclops hooks install` remains the explicit repair command for vendor hook
wiring outside install and update.

## Uninstall

The installer stops the validated selected daemon, removes the complete current
Cyclops state home, the binaries, the validated managed pair store, its
Cyclops hook entries, unedited seeded skills, and its PATH block. Pair-store removal holds the update lease
and refuses unknown, linked, or ownership-changing entries instead of deleting
an unproven tree. Both public names are removed only from one prefix, selected
by `--prefix` or by the `cyclops` command on `PATH`. It never resolves a
`cyclopsd` from another prefix. If only `cyclopsd` can be found, uninstall
refuses and asks for an explicit prefix.

Export anything you want to retain, then run:

```bash
curl -fsSL https://www.usecyclops.dev/install.sh | sh -s -- --uninstall
```

If you want to remove only journals while keeping the rest of Cyclops state,
start with the previewed
[`cyclops data forget --all`](data.md#forget-the-retained-journal-scope)
journey. Stop the daemon before its preview and keep it stopped through the
exact confirmation. It removes only the previewed workspace and session NDJSON
journals, and deliberately leaves configuration and vendor wiring alone.
Export first if you might need the history.

`cyclops remove --all` remains available when you want its explicit preview
and confirmation without removing the installed binary pair. The installer
removes only exact Cyclops hook commands from vendor configuration and only
byte-for-byte known Cyclops skill seeds. It preserves unrelated settings,
handlers, and edited skills. If it cannot safely prove a vendor file or skill,
it leaves that file untouched and stops before removing state or binaries.
