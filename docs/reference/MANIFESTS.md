# Teaching cyclops a new agent CLI

Everything cyclops knows about an agent CLI is one TOML file: which
processes it runs as, how to tell working from idle by looking at the pane,
and how to type into it. No code, no plugin, no wrapper around the CLI.

Twelve ship inside the `cyclops` binary and land in `~/.cyclops/manifests`
on your first `cyclops start`. Five are measured against a live CLI:
`claude.toml`, `codex.toml`, `agy.toml`, `cursor.toml`, `kimi.toml`. Seven
are written from vendor documentation alone and say so with
`version_tested = "unverified"`: `gemini.toml`, `qwen.toml`, `goose.toml`,
`opencode.toml`, `amp.toml`, `crush.toml`, `aider.toml`. An unverified file
binds the pane, declares the hook events the vendor documents, and carries
no idle, working, or composer rule, so a doorbell to that pane never detects
a human draft: it is written and submitted like a raw write with a receipt,
until someone measures the composer and edits the file.
Each header names its sources and what was and was not observed. Their
source is [`resources/manifests/`](../../resources/manifests/). A
thirteenth is three steps away.

## Add one

1. Write the file into `~/.cyclops/manifests/`.
2. Restart `cyclopsd`. Manifests are read once at boot.
3. Name a pane running that CLI and check what cyclops reads.

```
$ cyclops read reviewer --source detection
reviewer · ○ idle · decided by title_idle · write-ready

  title   ○ idle  title_idle      just now
  screen  ○ idle  composer_empty  just now
```

`decided by` names the rule that produced the verdict. A wrong reading is
one rule to fix.

`write-ready` is a second answer to a different question: was the composer
positively proven empty by this capture. Only a screen rule can answer it,
because only the screen can see the composer. A manifest with no idle screen
rule reads `not write-ready: no_write_safe_composer_evidence` however
confidently its title rule says idle. Delivery does not gate on it: a
doorbell holds only on a positively observed human draft (a rule with
`composer_semantic = "human_input"`) or a named block, so a manifest with no
composer rule never holds a doorbell for a person's typing.

Add `--raw` and the same answer carries the pane capture the sensors
read, under the readings. One answer means one moment: a separate
`--source visible` read can straddle a state change, and then the screen
you are staring at contradicts the verdict it is supposed to explain.

```
$ cyclops read reviewer --source detection --raw
reviewer · ○ idle · decided by title_idle · write-ready

  title   ○ idle  title_idle      just now
  screen  ○ idle  composer_empty  just now

what the sensors read (%1):
...the pane, verbatim...
```

Cyclops looks for manifests in `manifest_dir` from your config, then
`~/.cyclops/manifests`, then `./manifests` relative to where you started the
daemon. First directory that exists wins; it is not a search path. With no
`manifest_dir` set, that means the directory `cyclops start` filled, which
is where your own file goes too. A later `cyclops start` writes only the
shipped files it does not find, so nothing you put there is touched.

A file that fails to parse takes the whole directory with it: the daemon
logs the reason and runs with no manifests at all, so every pane reads
`? unknown`. Check `cyclopsd`'s stderr after adding one.

## A working file, end to end

This is the stand-in [`tests/e2e/parity-check.sh`](../../tests/e2e/parity-check.sh)
uses. It binds a plain shell, reads its state off the pane title, and takes
deliveries. Nothing is left out:

```toml
[agent]
id = "demo"
display_name = "Parity rig stand-in"
process_names = ["sh", "bash", "dash", "zsh", "cat"]
launch = "cat"

[hooks]
turn_start = "UserPromptSubmit"
turn_start_evidence = "confirmed"
turn_end = "Stop"
turn_end_evidence = "confirmed"
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
| `launch` | The command that starts this CLI, for `cyclops start --agents <id>`. Optional |

Each shipped `version_tested` value is parity-tested against an authoritative
live fixture from that version. The value identifies the newest tested vendor
version, not proof that every state was remeasured. Each rule's `evidence` and
the current release record state the narrower coverage boundary.

`argv_basenames` exists for one measured reason. tmux reports the kernel's
name for the resolved executable, so a native Claude install, where
`~/.local/bin/claude` is a symlink into `versions/2.1.220`, reports
`2.1.220` and `process_names = ["claude"]` never matches. Cyclops falls back
to reading the pane process's argv.

A CLI that runs under an interpreter defeats both lists today. MEASURED on
Gemini CLI 0.45.2: `gemini` is a Node script, so `#{pane_current_command}`
reads `node` and `ps -o args=` on the pane's foreground process reads
`node /.../bin/gemini`; the daemon takes only the first argv token. Qwen
Code and Amp are Node scripts and aider is a Python script, so the same
holds for them. Never list the interpreter name: `node` would claim every
Node pane on the machine. Pin those panes by hand until the daemon reads
the script path behind an interpreter.

When neither list matches, the pane reads `? unknown` and nothing addresses
it. `cyclops name %4 reviewer --manifest demo` pins one by hand; the pin
wins over both lists and sticks with the name.

`launch` is the only key here that is not about detection. It is what
`cyclops start --preset duo --agents claude,codex` runs in each named pane:
the id comes from this file, the command comes from this key. Leave it out
and cyclops still detects that CLI perfectly; `--agents` refuses the id
rather than guessing at a binary name, because a wrong guess fails inside a
pane where nobody reads the error. Write the bare command. Hook wiring is
`cyclops hooks install` and is deliberately not composed in here, so a pane
started this way runs on the title and screen tiers until you wire it.

## `[[rule]]`: reading state off the pane

Each rule says: look at this part of the pane, and if this matches, the
agent is in this state. Highest `priority` that matches wins.

| Key | What it does |
|---|---|
| `id` | Rule name. This is what `cyclops read --source detection` prints |
| `state` | `unknown`, `idle`, `idle_with_input`, `working`, `blocked_modal`, `blocked_permission`, `blocked_quota`, `dead` |
| `composer_semantic` | Optional measured composer meaning: `clean`, `human_input`, `ghost_suggestion`, or `ambiguous` |
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

`composer_semantic` describes only the composer shape matched by that rule.
It does not replace runtime `state`. Leave it absent when the rule does not
read the composer or when evidence does not support a classification. Use
`ambiguous` when one rule matches multiple meanings, such as an empty composer
and a ghost suggestion. Runtime code must not infer this meaning from rule ids.

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
does not match when no color capture was taken, rather than guessing.

A blocked state is not a state cyclops clears on its own unless the rule
says so. `auto_dismiss = false` means report and park the delivery; a human
decides. Claude's folder-trust dialog is `false` for a measured reason:
Escape on that dialog exits the CLI.

## `[hooks]`: turn edges and the ack receipt

| Key | What it does |
|---|---|
| `config_mechanism` | How this CLI is told about hooks. Free text, printed by `cyclops hooks install` |
| `turn_start`, `turn_end` | Event names for the two turn edges |
| `ack` | The event whose payload can prove a doorbell arrived |
| `ack_payload_field` | The field in that payload holding the injected text |
| `available` | Every event name the CLI can fire. Documentation for whoever writes the next rule |

`ack` plus `ack_payload_field` is what earns a doorbell `verified_by: hook`:
the hook fires, the payload carries the line, and Cyclops finds the attempt
inside it. Without them a doorbell settles on screen evidence, or on nothing
at all, and the journal says which.

Wiring the hooks on the CLI side is [hooks.md](hooks.md).

## `[injection]`: how to type into it

| Key | What it does |
|---|---|
| `method` | How the text goes in. `load-buffer + paste-buffer -p` for every shipped CLI |
| `submit` | The key that sends it, usually `Enter` |
| `clear_keys`, `verify_before_submit`, `verify_pattern`, `safe_states`, `unsafe_states`, `busy_behavior` | Accepted from older files and ignored. The 1.1.0 daemon reads the composer back once after every paste and holds only on a seen draft or a named block; none of these keys changes that |
| `composer_prompt_regex` | Whole joined-capture row that starts the active composer, with a named `content` capture |
| `composer_continuation_regex` | Whole joined-capture row for each later logical payload line, with a named `content` capture |

After the paste, cyclops reads the composer back once and compares the exact
doorbell row, re-checks that the pane still holds the same process, and then
presses `submit`. A row that did not read back exactly is still submitted,
once, and recorded as `submitted_unverified`. That readback is not
configurable per manifest.

The two composer patterns are declared together and are matched after
`capture-pane -J -e` joins tmux physical wraps and Cyclops removes SGR codes.
They remove only measured prompt chrome. Extraction still refuses unless one
same-id header reaches one terminal sentinel followed immediately by the
vendor's styled trailer. A duplicate header, duplicate sentinel, transcript
echo, undeclared trailing row, or collapsed chip is not visible payload.

`clear_keys` is accepted from older files. Each entry must still be a named
non-text key or a modified chord, but nothing invokes it.

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
