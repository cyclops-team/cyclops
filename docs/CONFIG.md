# Configuration

Most users should not create this file by hand. `commPact-setup` generates and
validates it from a few flags, and `commPact-setup --config-only` generates one
for a safe existing-session adoption. This reference is for advanced changes.

## `team.conf`

The parser accepts a flat data file. It never sources or evaluates values.

Required keys:

```text
version=1
session=NAME
workdir=PATH
layout=tiled|columns
columns=N                 # required for columns layout
split=LABEL:WEIGHT,LABEL:WEIGHT  # optional; columns layout only
operator=ROLE
default_target=ROLE
agent_roles=ROLE[,ROLE]
role=ROLE|COMMAND
```

Roles appear in file order. Labels use lowercase letters first, followed by lowercase letters, digits, `_`, or `-`. The parser rejects duplicate roles, unknown keys, missing references, empty commands, an operator in `agent_roles`, and malformed lines before any tmux mutation. It warns, but does not fail, above eight roles.

## Runtime metadata

Initialization and adoption write these session options:

- `@commPact_config_path`
- `@commPact_roles`
- `@commPact_agent_roles`
- `@commPact_operator_role`
- `@commPact_default_target`
- `@commPact_selected_theme`

Pane `@name` is the functional role label used for lookup. Pane `@role` is the cosmetic role stamp reapplied by layout. Metadata is session-scoped. A missing agent list fails message ACL checks closed.

## Theme

`config/theme.conf` selects a static preset by `selected_theme=NAME`. Presets live in `themes/`. Reapply one with:

```sh
~/.commPact/bin/commPact-layout --config PATH --theme-only
```

## Optional weighted split

For `layout=columns`, `split=LABEL:WEIGHT,LABEL:WEIGHT` reserves the last, rightmost column for exactly two distinct declared roles. Positive integer weights set their vertical ratio and listed order is top to bottom. All other roles stay in configured order across the unchanged equal-width columns to the left. The key is invalid for `layout=tiled`; there is no position selector or N-way split.
