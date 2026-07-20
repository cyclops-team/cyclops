# commPact

commPact is a small Bash and tmux toolkit for coordinating several terminal panes, including AI agents, inside one tmux session. It labels panes by role, routes structured messages between them, applies a repeatable layout, and enforces a same-session permission boundary so messages cannot cross into unrelated sessions.

It works with any program that runs in a pane: a plain shell, Claude Code, Codex, a REPL, or a long-running process.

## Why it exists

Raw tmux can send keystrokes to any pane, but it has no notion of who a pane is, who may talk to it, or how a message should be framed. commPact adds three things on top of tmux without replacing it:

- **Roles.** Every pane carries a label (`operator`, `lead`, `reviewer`, or whatever you choose). Messages target the label, not a volatile pane id.
- **A permission boundary.** A pane may only message declared roles in its own session. The allow list comes from configuration, not from hard-coded names, and an unconfigured session cannot send at all.
- **Structured, verified delivery.** `send` pastes a framed message, verifies it landed, and submits it, returning a machine-readable result instead of raw scrollback.

## Prerequisites

- macOS or Linux
- Bash
- tmux 3.2 or newer recommended

The installer checks these and stops with an actionable message if tmux is missing. It never runs a package manager, downloads from the network, edits your shell startup files, or edits your `tmux.conf`. Installing tmux and adding to `PATH` stay under your control.

## Quick start

The panes open in whatever directory you run the bootstrap from, so clone commPact somewhere separate and invoke it by path from your project directory:

```sh
git clone <repo-url> ~/commPact
cd /path/to/your/project
~/commPact/install.sh
```

That single command installs commPact into `~/.commPact`, generates a validated configuration, and starts a `commpact` tmux session with generic shell panes labeled `operator`, `lead`, `worker`, and `reviewer`. The panes open in the current directory; pass `--workdir PATH` to choose another. No configuration file has to be written by hand.

The labels above are defaults only. To choose your own from the start, pass flags; they are forwarded to the generator:

```sh
~/commPact/install.sh \
  --session project \
  --roles driver,implementer,reviewer \
  --command codex
```

Attach to the session when it is ready:

```sh
tmux attach -t project
```

To install the tooling without starting a session, use `~/commPact/install.sh --install-only`.

## Install only, then generate later

If you prefer the two steps separately, install from the release tree:

```sh
~/commPact/bin/commPact-install install
```

The default home is `~/.commPact`. Use `--destination PATH` for a staged or test install. An existing home requires `update` or explicit `--replace`; a replacement keeps a timestamped backup. `update` preserves an existing generated `config/team.conf` after re-validating it.

Then create a working session with no file editing:

```sh
~/.commPact/bin/commPact-setup
```

### Add to PATH (optional, recommended)

Every command works with its full path. To use the short names day to day, add the bin directory once:

```sh
echo 'export PATH="$HOME/.commPact/bin:$PATH"' >> ~/.zshrc
source ~/.zshrc
```

The rest of this document uses full paths so the examples work with or without this step.

## Customize without hand-writing team.conf

`commPact-setup` generates and validates the configuration for you, then starts the session. Run it with no flags for generic defaults (current directory, current shell, tiled layout), or set only the values you care about:

```sh
~/.commPact/bin/commPact-setup \
  --session project \
  --workdir "$PWD" \
  --operator facilitator \
  --roles builder,checker,writer \
  --default-target builder \
  --command sh
```

| Flag | Meaning | Default |
| :---- | :---- | :---- |
| `--session NAME` | tmux session name | `commpact` |
| `--workdir PATH` | working directory for every pane | current directory |
| `--roles CSV` | agent labels, excluding the operator | `lead,worker,reviewer` |
| `--operator LABEL` | operator label | `operator` |
| `--default-target LABEL` | default message target | first `--roles` label |
| `--command COMMAND` | command launched in every pane | `$SHELL` or `sh` |
| `--layout tiled\|columns` | pane layout | `tiled` |
| `--columns N` | width for columns layout | `2` when columns |
| `--config PATH` | where to write the config | `~/.commPact/config/team.conf` |

Useful modes:

- `--dry-run` prints the configuration it would generate and exits, writing nothing and starting nothing.
- `--config-only` writes and validates the configuration but does not start tmux. Use it when preparing to adopt an existing session.
- `--replace-config` backs up and replaces an existing generated config instead of refusing.
- `--attach` attaches to the new session after setup.

The generator refuses to overwrite an existing config unless asked, writes with a restrictive umask, and validates the result with the same parser the rest of the toolkit uses before anything starts. The operator label is always kept out of the agent roles, so the operator is never an ordinary message target.

## Adopt an existing session safely

If your panes are already running, label the live session instead of creating a new one. Nothing is restarted.

**1. Generate a config for the session (no session is started):**

```sh
~/.commPact/bin/commPact-setup --config-only \
  --session your-session \
  --roles lead,worker,reviewer \
  --command sh
```

The per-role command is required by the file format but is ignored by adoption; it is used only when `commPact-init` starts fresh panes.

**2. Read the current pane ids immediately before adopting**, because they change if a pane restarts:

```sh
~/.commPact/bin/commPact list
```

**3. Adopt, mapping each role to a live pane id:**

```sh
~/.commPact/bin/commPact-adopt --config ~/.commPact/config/team.conf --session your-session \
  --map operator=%1 --map lead=%2 --map worker=%3 --map reviewer=%4
```

Adoption stamps each pane's label and the session permission metadata together. It refuses a mapping that points outside the session or reuses a pane, and it will not silently leave a role unmapped.

**4. Smoke test:**

```sh
~/.commPact/bin/commPact-msg --session your-session @lead "smoke test"
```

If you rename roles on a live session, tell every pane the new labels first, quiesce sending during the brief re-adoption, and reply by pane id rather than label until the new labels are confirmed.

## Messaging basics

The `commPact` command is the pane interface:

```sh
~/.commPact/bin/commPact list                       # panes: target, pid, command, size, label
~/.commPact/bin/commPact resolve reviewer            # print the pane id for a label
~/.commPact/bin/commPact read lead 100               # read the last 100 lines of a pane
~/.commPact/bin/commPact id                          # print this pane's id
```

Send a structured message. `send` frames it with a subject and sender header, verifies it pasted, submits it, and returns a JSON result:

```sh
printf 'Please review the auth change.' \
  | ~/.commPact/bin/commPact send reviewer --json --subject 'Review request' --body-file -
```

Pass only the message body on `--body-file -`; the `SUBJECT:` and `FROM:` header lines are generated for you. The recipient sees who sent it and the exact pane id to reply to, and replies to that pane id.

For a short one-off line to the default target or a named role, `commPact-msg` is a convenience wrapper:

```sh
~/.commPact/bin/commPact-msg @reviewer "Please take a look"
~/.commPact/bin/commPact-msg @status
```

To surface a formatted notice to the operator pane, use `commPact-notice` (levels: `info`, `success`, `action`, `urgent`; `--popup` also shows a tmux popup):

```sh
~/.commPact/bin/commPact-notice --level action "Staging deploy needs approval"
```

Lower-level `type` and `keys` are also available for driving a non-agent pane (a prompt, a REPL). They require reading the target first, so you always see the current state before you type into it:

```sh
~/.commPact/bin/commPact read worker 10
~/.commPact/bin/commPact type worker "y"
~/.commPact/bin/commPact read worker 10
~/.commPact/bin/commPact keys worker Enter
```

## Safety boundaries

- **Same-session only.** `send` reads the allow list from the sender session's metadata and permits only targets in the same session. A target in another session is denied.
- **Config-driven allow list, fail closed.** The permitted roles come from the session's `agent_roles`, never from hard-coded names. A session with no commPact metadata cannot send at all; it reports an unconfigured error rather than guessing. Use `send --allow-label LABEL` for a deliberate one-off override and `--expect-label LABEL` to assert the target is who you think it is.
- **Operator is not a message target.** The operator role is excluded from `agent_roles` by the config parser, so ordinary agent messaging cannot address it.
- **Operator-gated setup.** `commPact-init` and `commPact-setup` refuse to run from inside an existing commPact session unless invoked from that session's operator pane. From a normal shell they run freely.
- **Read before you type.** `type` and `keys` require a prior `read` of the target pane, so manual keystrokes are never sent blind. `send` performs its own guarded preflight internally.
- **`name` is unguarded.** `commPact name` sets a pane label with no permission check. It is for operator recovery of a pane that lost its label, not routine use; re-labeling a working pane can desync it from the enforced roster.
- **No surprise system changes.** The installer never touches your shell startup files, package manager, or `tmux.conf`.

## Update and uninstall

```sh
~/.commPact/bin/commPact-install update                 # re-install, preserving a validated config
~/.commPact/bin/commPact-install uninstall              # remove the install home
~/.commPact/bin/commPact-install uninstall --restore PATH   # restore a specific backup
```

`update` requires an existing install and keeps your generated `config/team.conf`. A replace-install (`install --replace`) and every `update` create a timestamped backup under `~/.commPact.backup.*`. `uninstall` does not create a backup: it removes the install home and reports any existing backups, which are never auto-deleted. `--restore PATH` restores one of those backups.

`commPact-state-watchdog --state-dir PATH` is a manual local state check you can run on demand.

## Advanced configuration

Most users never edit the config; `commPact-setup` writes it. When you need advanced options, the format is a flat, data-only file (the parser never executes it):

```conf
version=1
session=project
workdir=/absolute/path
layout=tiled
operator=operator
default_target=lead
agent_roles=lead,worker,reviewer
role=operator|sh
role=lead|sh
role=worker|sh
role=reviewer|sh
```

Rules the parser enforces:

- `version` must be `1`; `session`, `workdir`, `layout`, `operator`, `default_target`, and `agent_roles` are required.
- `layout` is `tiled` or `columns`; a `columns` layout also needs `columns=N`.
- Each `role=LABEL|COMMAND` declares a pane. Labels match `^[a-z][a-z0-9_-]*$`. The command is what `commPact-init` launches; adoption ignores it.
- `operator` and `default_target` must be declared roles, `default_target` must be in `agent_roles`, and `operator` must not be in `agent_roles`.
- An optional `split=LABEL:WEIGHT,LABEL:WEIGHT` gives a weighted rightmost column with the `columns` layout.

Apply or refresh a layout on a running session without changing labels or metadata:

```sh
~/.commPact/bin/commPact-layout --config ~/.commPact/config/team.conf
~/.commPact/bin/commPact-layout --config ~/.commPact/config/team.conf --theme-only
```

The generated `config/team.conf` is local user state. It is not part of the release source; only `config/team.conf.example` ships. `update` preserves your local copy across upgrades.

## Reference

- [Configuration and runtime metadata](docs/CONFIG.md)
- [Operator cutover and rollback](docs/CUTOVER.md)
- [Vendor and attribution boundary](docs/VENDOR.md)

The final commPact project license and distribution status are pending.
