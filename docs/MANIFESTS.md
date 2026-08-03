# Teaching cyclops a new agent CLI

Everything cyclops knows about an agent CLI is one TOML file: which
processes it runs as, how to tell working from idle by looking at the pane,
and how to type into it. No code, no plugin, no wrapper around the CLI.

Three ship in [`manifests/`](../manifests/): `claude.toml`, `codex.toml`,
`agy.toml`. A fourth is three steps away.

## Add one

1. Write the file into `~/.cyclops/manifests/`.
2. Restart `cyclopsd`. Manifests are read once at boot.
3. Name a pane running that CLI and check what cyclops reads.

```
$ cyclops read reviewer --source detection
reviewer · ○ idle · decided by title_idle

  title  ○ idle  title_idle  just now
```

`decided by` names the rule that produced the verdict. A wrong reading is
one rule to fix.

Cyclops looks for manifests in `manifest_dir` from your config, then
`~/.cyclops/manifests`, then `./manifests` relative to where you started the
daemon. First directory that exists wins; it is not a search path.

A file that fails to parse takes the whole directory with it: the daemon
logs the reason and runs with no manifests at all, so every pane reads
`? unknown`. Check `cyclopsd`'s stderr after adding one.

## A working file, end to end

This is the stand-in [`demos/parity-check.sh`](../demos/parity-check.sh)
uses. It binds a plain shell, reads its state off the pane title, and takes
deliveries. Nothing is left out:

```toml
[agent]
id = "demo"
display_name = "Parity rig stand-in"
process_names = ["sh", "bash", "dash", "zsh", "cat"]

[hooks]
turn_start = "UserPromptSubmit"
turn_end = "Stop"
ack = "UserPromptSubmit"
ack_payload_field = "prompt"

[[rule]]
id = "title_working"
state = "working"
priority = 1100
region = "pane_title"
regex = ['^Implementing|^Reviewing']

[[rule]]
id = "title_idle"
state = "idle"
priority = 1000
region = "pane_title"
regex = ['^']

[injection]
method = "load-buffer + paste-buffer -p"
submit = "Enter"
verify_before_submit = true
verify_pattern = ["<message_id>"]
safe_states = ["idle"]
```

## `[agent]`: which panes this file is about

| Key | What it does |
|---|---|
| `id` | The name you pass to `--manifest`, and the file's identity. Two files with the same id, one wins |
| `display_name` | Human name |
| `version_tested` | The CLI version the rules were measured against. Free text |
| `process_names` | Bind when the pane's foreground command is one of these |
| `argv_basenames` | Bind when `argv[0]`'s basename is one of these. The fallback for when the first list cannot work |

`argv_basenames` exists for one measured reason. tmux reports the kernel's
name for the resolved executable, so a native Claude install, where
`~/.local/bin/claude` is a symlink into `versions/2.1.220`, reports
`2.1.220` and `process_names = ["claude"]` never matches. Cyclops falls back
to reading the pane process's argv.

When neither list matches, the pane reads `? unknown` and nothing addresses
it. `cyclops name %4 reviewer --manifest demo` pins one by hand; the pin
wins over both lists and sticks with the name.

## `[[rule]]`: reading state off the pane

Each rule says: look at this part of the pane, and if this matches, the
agent is in this state. Highest `priority` that matches wins.

| Key | What it does |
|---|---|
| `id` | Rule name. This is what `cyclops read --source detection` prints |
| `state` | `unknown`, `idle`, `idle_with_input`, `working`, `blocked_modal`, `blocked_permission`, `blocked_quota`, `dead` |
| `priority` | Higher wins. Rules are sorted once at load |
| `region` | `pane_title`, or `bottom_non_empty_lines(N)` for the last N non-blank screen lines |
| `contains` | Every string must appear in the region |
| `regex` | Every pattern must match the region as one block of text |
| `line_regex` | Every pattern must match some line of the region |
| `line_regex_esc` | Same, against a capture that keeps the color codes |
| `any` | A list of alternative matchers. The rule fires if its own clauses match, or any alternative does |
| `decline_keys` | For a modal: the exact keys that decline it, e.g. `["3", "Enter"]` |
| `auto_dismiss` | Whether cyclops may press those keys itself |
| `note`, `evidence` | Free text. Write down what you measured and where |

Clauses inside one matcher are ANDed. A rule with no clauses never fires.

**Prefer the title.** A title rule costs nothing: tmux already tracks the
title, so the daemon reads it over the connection it holds and never
captures the screen. If a title rule matches, the screen sensor does not
run at all. Screen rules are for what the title cannot express, which in
practice is modals and permission prompts.

**Regex flavor** is the Rust `regex` crate. `\x{2733}` from PCRE-style
drafts is accepted and translated to `\u{2733}`; there are no backreferences
and no lookaround.

`line_regex_esc` matches against a capture that still carries the terminal's
color codes, and exists for one measured case: on Codex CLI a ghost
suggestion and text you typed are the same characters, and the only
difference is that the suggestion renders dim. A rule carrying these clauses
fails closed when no color capture was taken, rather than guessing.

A blocked state is not a state cyclops clears on its own unless the rule
says so. `auto_dismiss = false` means report and park the delivery; a human
decides. Claude's folder-trust dialog is `false` for a measured reason:
Escape on that dialog exits the CLI.

## `[hooks]`: turn edges and delivery receipts

| Key | What it does |
|---|---|
| `config_mechanism` | How this CLI is told about hooks. Free text, printed by `cyclops hooks install` |
| `turn_start`, `turn_end` | Event names for the two turn edges |
| `ack` | The event whose payload can prove a delivery arrived |
| `ack_payload_field` | The field in that payload holding the injected text |
| `available` | Every event name the CLI can fire. Documentation for whoever writes the next rule |

`ack` plus `ack_payload_field` is what earns `✔ delivered · verified`: the
hook fires, the payload carries the text, cyclops finds this message's id
inside it. A CLI with no matchable ack still works; its deliveries land as
`✓ delivered · unverified (screen)`, which means the paste left the composer
and a turn started. Both are delivered; only the evidence differs.

Wiring the hooks on the CLI side is [hooks.md](hooks.md).

## `[injection]`: how to type into it

| Key | What it does |
|---|---|
| `method` | How the text goes in. `load-buffer + paste-buffer -p` for every shipped CLI |
| `submit` | The key that sends it, usually `Enter` |
| `verify_before_submit` | Read the composer back before pressing submit |
| `verify_pattern` | What must be visible for the paste to count as staged. `<message_id>` is replaced with this delivery's marker |
| `safe_states` | Deliver only when the agent is in one of these |
| `unsafe_states` | Never deliver in these |
| `busy_behavior` | `"queues"` when text pasted mid-turn stages and runs as its own turn. Leave it out unless you measured it |

`verify_before_submit = true` with `verify_pattern = ["<message_id>"]` is the
gate that keeps a message out of the wrong pane: cyclops pastes, reads the
composer back, finds this exact message's id, re-checks that the pane still
holds the same process, and only then presses Enter. Leave both on.

`safe_states = ["idle"]` is the conservative default and the right one until
you have measured what the CLI does with text pasted mid-turn.

## Write down what you measured

Every shipped manifest carries `evidence` strings naming what was observed
and where. That is not decoration: it is how the next person knows whether a
rule is a measurement or a guess, and vendor quirks are data here, not code.
When a CLI changes, the fix is this file.

## Unknown keys are kept, not rejected

The loader tolerates keys it does not model, so a manifest can carry notes,
timings, and evidence the code has no field for. What is validated: the
regexes compile, the states are real state names, the regions parse. A
mistake in any of those names the file, the rule, and the value.
