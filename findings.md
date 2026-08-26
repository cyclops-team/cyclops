# Build findings

Probe results where reality contradicted the brief or the research, found
while implementing v2. Same discipline as the validation campaign: every
entry is MEASURED (observed live on this machine) or READ (source/doc
inspection), with the probe that proved it. The validation campaign's
F1-F12 live in `~/projects/cyclops-arch/findings.md`; numbering here
continues from F13.

## Index

What each finding constrains, in one line. Search for `## F<n>` to reach
the measurement itself; the numbers are stable and never reused.

**Nothing here has expired.** Every measurement below still constrains code
in this tree today, which is why the file is long: it is not an archive.
The two exceptions are marked: F13 is still true but a later finding
changed what follows from it, and F30/F31 record a deliberate gap rather
than a measurement.

| | Constrains | Status |
|---|---|---|
| F13 | Subscriptions are the watcher's per-pane change signal, and bootstrap comes from `list-panes`, never from the subscription's first event | binds, amended by F25 |
| F14 | Every tmux invocation passes `-u`, or replies come back with tabs and non-ASCII replaced by `_` | binds |
| F15 | Anything reading pane output accepts `%extended-output` as well as `%output` | binds |
| F16 | Reply correlation accepts a terminator only when the `%begin` command number matches, so pane text cannot truncate a reply | binds |
| F17 | Bracketed paste cannot be the gate; post-paste composer verification is | binds |
| F18 | Code adjusting socket timeouts mid-connection on macOS tolerates EINVAL once the peer has closed | binds |
| F19 | Typed text versus ghost text is an SGR distinction, so the delivery gate reads escaped captures | binds |
| F20 | A dialog whose own text says Escape cancels never receives Escape; trust prompts hold for a human | binds |
| F21 | Manifest binding falls back to argv basenames when the kernel comm name is a version string | binds |
| F22 | The control-mode reader reads bytes, not UTF-8 lines | binds |
| F23 | One second is the title sensor's resolution floor, and fixtures hold a driven state across a tick | binds |
| F24 | Scratch paths come from `cyclops_proto::scratch`, and CI runs `--no-fail-fast` with `fail-fast: false` | binds |
| F25 | Pane death is reported only by the all-panes subscription, and an empty `pane_pid` parses as -1 | binds |
| F26 | Cyclops writes the pane border and never the pane title | binds |
| F27 | Border chrome is a per-pane format plus a window-scoped status, and costs each pane a row, so layout ratios measure panes and not the window | binds |
| F28 | Workspaces build at the terminal's size, because tmux spreads a resize evenly rather than in proportion | binds |
| F29 | A script matching daemon output textually matches one field, or uses `jq` | binds |
| F30, F31 | Never allocated. The gap is deliberate and the numbers stay unused | bookkeeping |
| F32 | A theme reload applies whole or not at all: a file that stops setting a token it set before is refused | binds |
| F33 | `cyclops start` derives its next steps from daemon reachability, not the pane count | binds |
| F34 | `libghostty-vt` needs Zig at build time; the corpus used `vt100` as the comparison engine instead | binds |
| F35 | `alacritty_terminal` passes the workspace VT fixture corpus 12/12; `vt100` passes 5/12 | binds |
| F36 | Cloud-agent VM stdout accepts OSC 52 clipboard writes; native fallback uses wl-copy/xclip when present | binds |
| F37 | Session rename notifications carry a stable id and can describe a background session | binds |
| F38 | Hydration replays the saved primary first, then enters the alternate screen, then the visible capture | binds |
| F39 | Width fixtures pin VS16 non-widening and SGR 21 as bold-off, so an engine bump fails loudly | binds |
| F40 | One `list-panes -a` discovers every session, window, and pane; empty sessions cannot exist | binds |
| F41 | Session fields are reachable from pane lines; the second snapshot command exists for tab-safety, not reachability | binds |
| F42 | Workspace performance measurements prove shape and scaling, not universal wall-clock budgets | binds |
| F43 | `swap-pane` focuses the destination pane unless `-d` preserves the active slot | binds |
| F44 | OSC 52 output requires terminal support; native clipboard fallback remains necessary | binds |
| F45 | Timing tests must prove their runner was scheduled before treating a timeout as product evidence | binds |
| F46 | A stalled control-mode client accumulates queued notifications before tmux can report pause | binds |
| F47 | Killing a session closes control mode without per-pane death events | binds |
| F48 | `window-size latest` lets an ordinary client change the canvas observed by a control client | binds |
| F49 | Pane applications render against tmux's reported ground, which may serve two terminals | binds |
| F50 | A malformed subscription value is a clean-prefix truncation already on tmux's own wire on tmux 3.7b, not `-u`, the line reader, or a dropped connection; the trigger did not reproduce under driving | binds |
| F51 | tmux 3.7b's own window-index and current-window bookkeeping is racy under heavy parallel fork load, even on a fully isolated `-L` server; a test creating a second window must give it an explicit index and never read it back through a session-level (current-window) target | binds |
| F52 | tmux 3.7b's `pause-after` clock only starts once a stalled reader's backlog has actually accumulated past the threshold; a host-contended producer can leave a stalled reader with nothing to pause against for seconds at a time | binds |
| F53 | `display-message -t` must resolve down to a pane, and a bare `=session` exact-match target gives it nothing to fall back to (empty output, no error); target `=session:` — other session-scoped commands accept the bare form fine | binds |
| F54 | Codex 0.147.0 can keep a multiline paste expanded; F62 later proved representation is chosen per message | binds, amended by F62 |
| F55 | Claude composer input is identified by its NBSP boundary and style, not by visible text alone | binds |
| F56 | The terminal engine accepts geometry below its documented floor, so every sizing path clamps first | binds |
| F57 | State files need one descriptor-anchored owner; path-based opens inherit unsafe permissions and cannot contain link races | binds |
| F58 | Antigravity 1.1.13 paints no idle ghost text; content after its prompt is treated as input | binds |
| F59 | Composer chrome and trailer anchoring are measured manifest data, never Rust vendor branches | binds |
| F60 | Claude can retain an idle title throughout active work, so its styled working row outranks the title | binds |
| F61 | Hook callers may have no Cyclops label; authenticated server-derived process identity is authoritative | binds |
| F62 | Raw composer, anchored sentinel, and collapsed chip are per-message evidence classes; a leading id is never completeness proof | binds |
| F63 | Detached hook origin uses the same last-known route record as report handling | binds |
| F64 | Codex paints a blank separator below the composer and the declared trailer must include it | binds |
| F65 | Whole-composer clearing is measured for Claude and Codex; Antigravity and Cursor refuse unsupported actions | binds |
| F66 | The isolated soak detected staged representations and cleared them in 100 trials each for Codex, Claude, and Antigravity; Cursor was unavailable | evidence |
| F67 | A one-line doorbell must fit the narrow lane because application wrapping is not exact composer evidence | binds |
| F68 | Codex 0.149.1 colors the prompt glyph separately and may leave its status trailer unstyled under `NO_COLOR` | binds, partial evidence |

## F13. refresh-client -B subscriptions work in control mode on tmux 3.6a (MEASURED)

Subscribing `name:%pane:#{pane_title}\t#{pane_dead}\t#{pane_in_mode}\t#{pane_current_command}`
produces %subscription-changed for select-pane -T from outside, OSC 2 printf
from inside the pane, and copy-mode entry/exit. This makes subscriptions the
watcher's primary per-pane change signal (zero polling holds). Caveat: the
initial value push after subscribing is lazy, so bootstrap must come from
list-panes, never from the subscription's first event. Proven by
src/cyclops-tmux/tests/subscription_probe.rs, which documents the fallback
if a future tmux breaks it.

## F14. tmux sanitizes control-mode replies to '_' for non-UTF-8 clients; spawn tmux -u (MEASURED, high severity)

With no LC_ALL/LC_CTYPE/LANG naming UTF-8 in the client environment, tmux
3.6a replaces tabs and non-ASCII in command reply blocks with underscores:
tab-separated list-panes formats collapse and Claude's braille spinner titles
(the F5/F6 title sensor) are destroyed. This hits exactly the environments a
daemon runs in (launchd, cron, CI). The validation campaign could not see it:
CPython's PEP 538 locale coercion exports LC_CTYPE=C.UTF-8 to child
processes, so every Python probe was silently immune. The Rust client always
passes -u; any other tmux invocation that parses formatted output needs -u
too. Route all tmux access through cyclops-tmux.

## F15. pause-after switches output notifications to %extended-output (MEASURED)

Once `refresh-client -f pause-after=300` is set (amendment a, done at
attach), pane output arrives as `%extended-output %pane age : data` instead
of %output. A consumer matching only %output goes blind right after enabling
flow control. The watcher treats both as activity.

## F16. %begin flags field separates unsolicited blocks from command replies (MEASURED)

On 3.6a the implicit post-attach block carries flags 0; replies to commands
from this client carry flags 1, and %end/%error repeat the %begin command
number. Correlation accepts a terminator only when the number matches, so
pane content that happens to look like control-mode lines cannot confuse it.
Notifications did not interleave inside reply blocks in any observed case.

## F17. paste-buffer -p brackets only when the app enabled mode 2004 (MEASURED)

A plain `cat` pane receives the payload unbracketed; the markers appear only
after the application requests bracketed paste (printf '\033[?2004h'). The
round-trip test proves byte-exact delivery (quotes, backslashes, tabs, UTF-8,
embedded newlines) with mode 2004 on. Confirms amendment b's stance: the
paste path cannot be gated on bracketing; composer verification is the gate.

## F18. macOS setsockopt(SO_RCVTIMEO) fails with EINVAL once the peer closed (MEASURED)

Reproduced in the CLI e2e tests: set_read_timeout(None) on a UnixStream
whose peer already closed fails with EINVAL while buffered lines remain
readable. The CLI swallows the error and lets the next read report the
close. Anything adjusting socket timeouts mid-connection on macOS must
tolerate this or it will hide readable data behind a misnamed error.

## F19. codex ghost text is SGR-dim; typed text is bare (MEASURED)

Probed on codex-cli 0.146.0 in an isolated tmux server, one launch, zero
turns. With capture-pane -e, the pristine composer renders the glyph bold
and the ghost suggestion dim: `ESC[1m>ESC[0m ESC[2mFind and fix a bug in
@filenameESC[0m`. After send-keys typed text the same line is
`ESC[1m>ESC[0m fix the rate limiter in gateway.rs`: no SGR wrapping at all.
The plain capture is identical in both states, which is why
composer_empty_or_ghost could never produce idle_with_input. SGR is a
reliable discriminator; the manifest carries it as line_regex_esc rules
(composer_typed_input, composer_ghost_suggestion) with the probe captures
as fixtures in src/cyclops-manifest/tests/fixtures/. The esc rules fail
closed without a -e capture; the daemon now supplies escaped captures in
fusion recompute (and therefore the delivery gate) whenever the bound
manifest carries esc rules, so typed text reads as idle_with_input
end to end.

## F20. Claude 2.1.220 trust dialog contains 'Enter to confirm' and Escape exits the CLI (MEASURED)

The folder-trust dialog ('Quick safety check: Is this a project you created
or one you trust?' / '1. Yes, I trust this folder' / 'Enter to confirm .
Esc to cancel') matched claude.toml's startup_modal, whose auto-dismiss
decline is Escape, and Escape on this dialog EXITS the CLI. Observed in the
m1 soak (tests/raw/m1-soak/claude_launch_modals.log); the harness Escape
fallback cost a soak leg. Fixed as data: a dedicated trust_dialog rule at
priority 1300 with auto_dismiss=false, and harness vocabulary that never
sends Escape to a dialog whose text says Esc cancels/exits.

## F21. Native Claude installs report pane_current_command as the version string (MEASURED)

~/.local/bin/claude is a symlink into versions/2.1.220 and macOS derives
the process comm from the resolved file, so #{pane_current_command} is
"2.1.220" and process_names=["claude"] never binds: detection silently does
nothing. Measured in the m1 soak (the driver hardlinked the binary as
"claude" to restore the tested assumption). Manifest schema now carries
agent.argv_basenames for the daemon's argv-based fallback binding via
pane_pid. Fix proven live in the rerun soak (tests/raw/m1-soak-2): claude
launched through the native versioned symlink, pane_current_command read
"2.1.220", the argv fallback bound the claude manifest, and the leg ran
100/100 delivered_verified with no hardlink shim.

## F22. Control-mode lines are not UTF-8; a decoding reader kills the connection (MEASURED, high severity)

tmux 3.6a octal-escapes control bytes in %output/%extended-output data but
passes bytes >= 0x80 through verbatim, valid UTF-8 or not: a pane printing
0xFF puts a raw 0xFF byte on the notification line, and a multi-byte
character split across two pty reads (braille U+280B written as 2+1 bytes)
produces two notification lines that are each invalid UTF-8 on their own.
The reader decoded the stream with tokio's UTF-8 next_line(); the first
such line errored it, and the while-let loop swallowed the error, closed
the pipe, and reported the live connection as dead. This is the m1 soak's
8 drops in 80 s: Claude's TUI streams braille spinner glyphs, codex output
is ASCII (zero drops in 3+ min of codex-only load), and every drop blinded
both ACK tiers while the transport was healthy. The "control child did not
exit after detach" drops are the same bug's shutdown shadow: with the pipe
already closed, detach-client was silently skipped and nobody drained the
child's stdout, so it wedged flushing until the kill. Fixed by reading
byte lines (read_until), parsing %output/%extended-output byte-faithfully,
lossy-converting only free-text fields, and killing an undetachable child
without the 2 s grace. Related measurement: control mode answers blocking
commands (wait-for, run-shell) with an immediately closed reply block, so
reply-timeout tests must freeze the server itself. Regression:
src/cyclops-tmux/tests/control_load.rs, zero Disconnected over the 60 s
sustained variant (braille + OSC title churn + split sequences + command
traffic). Field-proven in the rerun soak (tests/raw/m1-soak-2): zero
control drops and zero shutdown wedges in daemon.log across 252 s of the
same claude braille load that produced 8 drops in 80 s pre-fix.

## F23. tmux evaluates format subscriptions on a 1Hz tick; sub-second states are invisible (MEASURED)

refresh-client -B pushes %subscription-changed only when the server's
once-per-second tick re-evaluates the format, not on the change itself.
Observed while driving fixture turns for agent.wait: three title flips
landed on an exact 1s grid in the daemon log, and a working-state title
that was set and replaced within the same second produced no notification
at all: tmux pushed one net change whose value was already the final idle
title, so fusion never saw the turn. Consequences: the title sensor's
resolution is one second, a turn shorter than that can be missed entirely
by `wait --until done` on screen/title-only agents (hook-tier agents
report their edges out of band), and fixture tests must hold each driven
state across a tick. Proven by src/cyclopsd/tests/m2_wait.rs, which
holds working titles for 2s.

## F24. /private/tmp is macOS-only, and cargo test hides every failure after the first binary (MEASURED)

Both v2 pushes failed CI while the same tree was green on the developer
machine. Root cause: test scratch paths were hardcoded to `/private/tmp`,
which exists on macOS (it is the real `/tmp`) and does not exist on Linux,
where `/private` is not writable by the runner. Every Linux job died on
`control::tests::spool_file_is_owner_only_and_dir_created_0700` with
`Io(Os { code: 13, kind: PermissionDenied })` at control.rs:827
(runs 30754262517 and 30758203413, jobs 91523975003 and 91523975029).

The path itself was a deliberate choice, not an accident: a unix socket
path caps near 104 bytes on macOS and `std::env::temp_dir()` there is a
long `/var/folders/...` path, so a daemon socket created under it fails to
bind. The rule that was missing is that the constraint is macOS-specific.
`cyclops_proto::scratch::scratch_root` now states it once: `/private/tmp`
on macOS, the system temp dir elsewhere, `CYCLOPS_TEST_TMP` overriding
both. Proven by running the full suite with the root relocated to
`/private/var/tmp`, which is the same code path Linux takes.

Two masking effects made one bug look like a green build for two
milestones, and both were worth more than the fix:

- `cargo test` stops at the first failing test binary. 25 of 26 tests in
  `cyclops-tmux --lib` passed, that binary failed, and the remaining 31
  test binaries never ran. The visible failure count (1) had no relation
  to the real one, which stayed unknown until the fix landed. CI now runs
  `--no-fail-fast`.
- The job matrix defaulted to `fail-fast: true`, so the ubuntu failure
  cancelled macOS mid-run and threw away the only signal that would have
  distinguished a portability bug from a real regression. CI now sets
  `fail-fast: false`.

Local green plus red CI is not a flaky-CI story until the logs say so.
Read the failing job before assuming the environment is at fault.

## F25. A per-pane subscription can never report a pane's death; the all-panes form can (FIXED)

The advisory tmux-HEAD job went red on one test,
`short_lived_command_flips_pane_dead_with_remain_on_exit`. Two readings
were wrong before the right one. It is not that next-3.8 changed
anything: both versions emit identical notification sequences and flip
`pane_dead` within 16ms of each other. And it is not merely a race,
though it presents as one.

The cause is in tmux's source. `pane_dead` is set when the pane's pty fd
closes (format.c `format_cb_pane_dead`: `wp->fd == -1 && PANE_STATUSREADY`).
That same closed fd is the gate that makes tmux skip the pane's own
subscription (monitor.c `monitor_check_pane`: `if (wp == NULL || wp->fd
== -1) return;`). So a per-pane subscription carrying `#{pane_dead}` can
report a live pane and can never report the flip, on any version. Measured
on both: the per-pane subscription pushes once at arm time with dead=0 and
nothing afterwards, while list-panes shows dead=1.

Cyclops only ever saw a death because some unrelated event forced a
pane-table resync in the same moment, in practice tmux's automatic window
rename firing when the command exits. 3.6a won that by 23ms (flip 595ms,
resync 618ms) and next-3.8 lost it by 13ms (flip 611ms, resync 598ms).
3.6a was passing on luck, and the cost was real: pane-dead is a delivery
gate condition, so a corpse could read as a live agent indefinitely.

The fix is one more subscription, armed once per connection:
`cypdead:%*:#{pane_dead}`. The all-panes form has no fd gate, so it keeps
expanding for a dead pane. Its handler arms the existing 30ms debounced
reconcile only when the pushed flag disagrees with the table, so an
all-live session arms nothing and the zero-polling contract holds. Latency
is now bounded rather than unbounded: 1033ms for a pane dying at 500ms,
which is tmux's own 1Hz tick plus the debounce, not a timer cyclops added.
Measured over 12s on an idle session, the subscription produced three
pushes total: one per pane at arm time plus the one real flip.

A second defect had to be fixed or the first would have made things worse.
tmux master gates `#{pane_pid}` on the same fd (format.c:2465), so
next-3.8 reports an empty pid for a dead pane while 3.6a reports the stale
one. The row parser treated the empty field as a parse failure and dropped
the whole row, so on next-3.8 the reconcile the new edge triggers read the
death as a REMOVAL: `PaneRemoved("%1")` at 1033ms with the pane gone from
the table. An empty pid now parses as -1, in one place used by both parse
sites.

Verified on both binaries: exactly one `PaneChanged { changed: [Dead],
dead: true }` at ~1033ms, no PaneRemoved, 543 tests green under each.
Disarming the new subscription makes the test time out on BOTH versions,
so 3.6a's 23ms of luck is no longer what carries it.

Open, and worth knowing: a dead pane's `pane_pid` now differs by version,
stale on 3.6a and -1 on next-3.8. Not normalized, because inventing a
value tmux stopped reporting would be a lie. On 3.6a a stale pid can in
principle be recycled by an unrelated process, and sender identity walks
socket-peer ancestry to a pid; next-3.8 closes that, 3.6a leaves it as
open as it already was. Also untested: the `%*` subscription form is
measured on 3.6a and next-3.8 only. If an older tmux rejects it,
subscribing only warns and the death edge silently reverts to the old
race.

## F26. Every shipped manifest reads the pane title as a sensor, so cyclops never writes it (MEASURED)

The M4 brief asked the daemon to put `role • state` on the pane title as
well as the border. Only the border is written, and the title is the
reason.

Measured by reading what the shipped manifests bind. Two of the three
(`resources/manifests/claude.toml`, `resources/manifests/agy.toml`) carry rules over
`region = "pane_title"`, and claude's spinner rules are the title tier at
priority 1100. The third says so in its own header: codex sets no OSC
title, so `#{pane_title}` there is the static project directory name and
the manifest marks it useless. A matching title rule means the screen
sensor never runs at all (amendment h), so on claude the title IS the
detection input for the common case.

Writing it would have cost twice over, both already measured elsewhere. A
title write from outside the pane pushes a `%subscription-changed` like any
other (F13), so cyclops would be feeding its own decoration back into its
own sensor. And an agent that publishes its own title overwrites cyclops
inside tmux's one-second tick (F23), so the decoration would not even hold.

What it constrains: `src/cyclopsd/src/chrome.rs` writes
`@cyclops_role`, `@cyclops_state`, `pane-border-format` and
`pane-border-status`, and no pane title; `cyclops_tmux::layout` writes none
either. The border already displays `#{pane_title}` by default, so
replacing the border FORMAT replaces the view without touching the value
underneath. Proven by `src/cyclopsd/tests/m4_name.rs`, which drives the
title sensor from outside the pane to move a border: that test can only
exist because chrome does not write the title.

## F27. `pane-border-format` is a pane option, `pane-border-status` is not, and border text costs every pane a line (MEASURED)

Three measurements about the two options the pane chrome writes, taken on
tmux 3.6a and re-taken on 2026-08-03 (macOS 26.5.2, arm64) with a probe on
an isolated `cyclops-testrig` server holding two panes in one 80x24 window.

1. `pane-border-format` is a real pane option. `set -p -t %0
   pane-border-format AAA` then `show -p -v` reads back `AAA` on `%0`,
   empty on `%1`, and empty at window scope. So cyclops can rewrite one
   named pane's border without touching the pane beside it.
2. `pane-border-status` has no pane scope. `set -p -t %0
   pane-border-status top` succeeds, exit 0, no stderr, and afterwards the
   WINDOW option reads `top`. tmux accepted the command and wrote the
   window. There is no way to give one pane border text and not its
   neighbour.
3. Border text costs each pane one line. The same two panes measured
   `40x24` and `39x24` with `pane-border-status` unset, and `40x23` and
   `39x23` with it set to `top`.
4. A format expands ONCE. With `@cyclops_role` set to the literal
   `#{pane_id}` and the border format set to ` #{@cyclops_role} `,
   `#{E:pane-border-format}` expands to ` #{pane_id} `: the option's VALUE
   is substituted and never re-expanded.

What each constrains. (2) is why `pane-border-status` is snapshotted per
window, turned on by the first adoption in a window and put back by the
last un-adoption, and why the registry carries a `WindowChrome` at all
(`src/cyclopsd/src/registry.rs`); it is also why a pane moving windows
needs `move_chrome` to hand the setting back to the window it left. (3) is
why every ratio in `cyclops_tmux::layout` is a share of the cells the PANES
hold and never of the window: a grid checked against the window stopped
adding up the moment the daemon named a pane, and `cyclops workspace save`
refused a session `cyclops start` had built seconds earlier
(`src/cyclops-tmux/tests/layout_round_trip.rs::a_window_wearing_border_chrome_still_reads_as_a_grid`).
(4) is why the label rides `@cyclops_role` instead of being written into
the format string: labels are human input, and a label containing `#{...}`
would otherwise become a tmux directive evaluated on every border redraw.

## F28. tmux spreads a window resize evenly, not in proportion (MEASURED)

A workspace is saved as ratios, so the question is what tmux does with them
when the window changes size. It does not scale them. A two-pane window
split 70/29 at 100 columns becomes 120/79 at 200 columns, not 140/59: the
100 new columns were handed out evenly, 50 each, rather than in proportion.

The consequence has a number. tmux gives a detached session 80x24 by
default, so a workspace built by a script and attached from a 200x50
terminal arrives with its shares moved: the `ops` preset's stream dock,
designed at 30% of the height, arrives at 41%.

What it constrains: `cyclops start` and `cyclops workspace restore` build
at the size of the terminal they were run from
(`src/cyclops/src/workspace.rs::build_size`), which is the only size
that keeps a preset looking like its design. One built with no terminal to
ask still drifts, and docs/guides/workspaces.md says so rather than hiding it.
And `first_difference` deliberately does not compare ratios when matching a
live session to a saved workspace: resizing a pane moves no agent, and two
sessions built at different terminal sizes never have the same cell counts
anyway.

## F29. The daemon's JSON object keys come out alphabetical, not in the order the code writes them (MEASURED)

Every reply and every event the daemon sends is built through
`serde_json::Value`, whose object is a `BTreeMap`, so the keys are sorted
before they reach the wire. Nothing in the daemon's source order survives.

Measured twice. First in `demos/m4-workspace.sh`, which waited for
`"name":"demo","attached":true` to appear in a `--json status` answer. That
substring cannot occur: `attached` sorts before `name`. The loop fell
through all 40 of its iterations, the demo carried on regardless, and it
passed on the `sleep` inside the wait rather than on the condition.
Re-measured on 2026-08-03 by capturing raw event lines off a rig socket:
the daemon writes `{"name": name, "pane_labeled": pane_id, "label": label}`
and the wire carries
`{"event":"session","data":{"label":"reviewer","name":"main","pane_labeled":"%0"},"seq":4}`;
it writes `{"name": name, "attached": attached}` and the wire carries
`{"event":"session","data":{"attached":false,"name":"main"},"seq":7}`.

What it constrains: any script matching daemon output textually must match
ONE field, or use `jq`. A pattern spanning two keys is a pattern about the
alphabet. Written down where a script writer meets it, in docs/reference/PROTOCOL.md
under "Requests and responses", and the demo now waits on a single field
plus a specific pane id after a restore.

## F30 and F31 were never allocated

Nothing in the tree cites them, and no measurement in the M4 or M5 work is
missing a number. They are recorded here so a reader who finds F29 followed
by F32 knows the file is not truncated, and so nobody reuses the numbers
for something new: a finding number that once meant two things is worse
than a gap.

## F32. Reading a theme file while an editor saves it: about one read in five sees valid TOML defining ZERO tokens, and none sees a syntax error (MEASURED)

This is the measurement the apply-whole-or-not-at-all reload rule rests on.
The question it answers: when a hot reload stats a theme file that is being
saved right then, what does it actually read?

How. Editors save by truncating and rewriting, which is what
`open(path, "w")` does: the file goes to zero bytes and the content comes
back afterwards. The probe ran that save in a loop against a copy of a
shipped theme (`resources/themes/light.toml`, 4410 bytes, all 22 tokens) while a
second thread read the file continuously. Both the saves and the reads were
timed with `perf_counter_ns` and the overlap was computed afterwards rather
than from a flag, so a read counts as concurrent only when its own interval
overlaps a save's open-to-close interval. Every overlapping read was
classified by parsing it: all 22 tokens, some, zero, or not valid TOML.

The numbers, 5000 saves per run on macOS 26.5.2 (arm64), 2026-08-03:

| Run | reads overlapping a save | zero of 22 tokens | syntax error | partial |
|---|---|---|---|---|
| 1 | 14697 | 3045 (20.7%) | 0 | 0 |
| 2 | 14572 | 3408 (23.4%) | 0 | 0 |
| 3 | 14618 | 3415 (23.4%) | 0 | 0 |

Three things in that table matter, in this order.

**Zero syntax errors, every run.** A TOML parse error was the only failure
the loader used to treat as a failure, so the thing it guarded against is
the thing that never happens. That is the whole finding: the guard was
pointed at the wrong failure.

**There is no partial state.** A 4.4 KB write lands in one flush, so the
file is either whole or empty. "Valid TOML defining zero tokens" is not an
edge case of the save, it IS the save's visible state, and loading it
paints all 22 tokens out of the compiled default table, whose lightness has
nothing to do with the theme on screen.

**About one read in five.** Roughly a fifth of the reads that land inside a
save see it. In absolute terms it is rarer than that sounds, 3045 of the
359376 reads the first run made, because a save is 226 microseconds and the
gaps between saves are much longer. But cyclops's reads are not random:
the stat rides an event, and an event is exactly what a person editing
their theme file generates.

The milestone recorded this as 27.3% (CHANGELOG.md, STATUS.md). This
reproduction gets 20.7% to 23.4% for the same class of file, and the
difference is in what counts as "during a save": the window used here runs
from open to close, and the bytes are back before close returns. The
load-bearing half, no syntax errors at all, reproduces exactly.

What it constrains: `ThemeWatch::adopt` in `src/cyclops-theme/src/select.rs`
refuses a reload that no longer sets a token the theme on screen sets,
rather than refusing only what fails to parse. A misspelled token name
fails the same way and stays failed until it is fixed, which is the same
rule doing the same job. A theme SWITCH is exempt: that palette was asked
for, so it applies with a fresh start's tolerance. `theme.reload` on the
daemon is on the near side of that rule, which is why a half-written file
can reach a real pane border and why
`src/cyclopsd/tests/m5_theme.rs::a_half_written_theme_leaves_the_borders_alone`
reads the border back off tmux rather than off the daemon's own belief.

Not a suite test, and deliberately: it measures a race and reports a
distribution, so asserting on it would flake. The procedure above is the
whole probe; it is 60 lines of Python and reproduces in under a minute.

## F33. `cyclops start` printed a first step that could not work, because it read the count instead of the daemon (FIXED)

The symptom an operator reported: `cyclops start` says `✓ workspace ready ·
1 agent`, and its own third step answers with an error.

```
$ cyclops start
✓ workspace ready · 1 agent

Next:
  1  cyclopsd &                                  start the daemon
  2  tmux attach -t main                         open the workspace and start your agents
  3  cyclops send implementer --subject "hello"  send the first message

$ cyclopsd &
$ cyclops send implementer --subject "hello"
⚠ needs attention · no pane for "implementer"
```

Only cyclopsd holds a name. `start` ran before it, so the name went into
the workspace file and onto no pane, and step 3 addressed an agent that
did not exist. `cyclops list` right after says `No agents yet`.

Three separate holes, all on the same path.

**The count was the wrong witness.** The note that exists to say this
(`names_wait`) was gated on `agents == 0`, and `agents` falls back to the
workspace file's count when no daemon can be asked. That is exactly the
case the note is for, so the note never printed. The gate now tests
whether the daemon confirmed, which is the fact it was always about.

**The step list was built from the file too.** `first_name` came from the
layout whenever naming was allowed, whether or not anything had been
named. It now comes from the layout only when the daemon is watching;
otherwise the step is `cyclops start` again, which is the command that
puts the names on.

**The attach step was gated on the wrong thing.** `if !existed` dropped
`tmux attach` from the second run of a first setup, which is the run where
the panes still hold no agent and opening the session is the point. It
now follows `$TMUX`: offered outside tmux, not offered inside.

Measured after: daemon down, the three steps are `cyclopsd &`, `cyclops
start`, `tmux attach -t main`, and each works when run.

### The waiting half

Starting the daemon first did not help either, for a different reason: the
daemon retries its attach on a backoff (RECONNECT_MIN 200ms doubling to
RECONNECT_MAX 5s), so a session created seconds ago is one it has not
reached, and `start` saw `Watch::NotYet` and named nothing. Two runs
either way round.

`start` now waits, bounded at 6 seconds, when it just built the session
and a daemon is there to wait for. Measured: 1.3s on this machine, and the
first run reports the heavy check.

Attaching turned out not to be enough to wait for. The first version
returned as soon as `attached == true` and produced a partial adopt:

```
✔ workspace ready · 1 agent
  no such target "%1"
```

The daemon reads the pane table after it attaches, and a pane it has not
read cannot be labelled. The wait now ends when every pane the run built
is one the daemon can see, which is why `session_view` returns the pane
set alongside the names.

### The log line that stopped the operator

```
WARN cyclopsd: cannot attach; retrying with backoff session=main error=tmux spawn failed: attach handshake failed
```

That is a daemon started before `cyclops start`, waiting for a session
that does not exist yet. It clears itself: the retry three seconds later
succeeded, in the same log. Logged at WARN with "cannot attach" in front,
it reads as a dead end, and the operator stopped there rather than finding
out. It now checks whether the session actually exists and says
`INFO waiting for session; cyclops start creates it`, keeping the WARN for
an attach that failed against a session that is there.

### The shape of the mistake

The same one as the perf fixture with zero attention items, and the settle
predicate that passed on an empty record: a check pointed at a value that
is only meaningful in the case it was not testing for. Here the count is
a fallback, so reading it asks "is the file empty" while meaning to ask
"did anything actually get named".

## F34. libghostty-vt requires Zig at build time; corpus used vt100 instead (MEASURED)

The design's second VT candidate is `libghostty-vt`, which fetches Ghostty
sources and runs `zig build` in its build script (`libghostty-vt-sys` 0.2.1).
On this machine and in the default CI image, `zig` is not on PATH, so
`cargo build -p libghostty-vt` fails with "failed to execute zig build: No
such file or directory". The fixture corpus therefore compared
`alacritty_terminal` against `vt100` 0.16 as the only Rust-pure alternative
that builds without Zig. `libghostty-vt` remains the documented fallback if
a future gap appears that alacritty cannot cover and Zig is added to the
build environment.

## F35. alacritty_terminal wins the workspace VT fixture corpus 12/12 (MEASURED)

`src/cyclops-workspace/tests/corpus.rs` runs twelve fixtures covering
plain output, SGR/256/truecolor, attributes, cursor motion, wrapping, wide
characters, alternate screen, bracketed paste, and synthetic Codex/Claude
captures. `alacritty_terminal` 0.26 passes all twelve; `vt100` passes five
(missing truecolor, 256-color fg assertions, bold/dim attribute checks, and
correct wide-character spacing). Production code calls `AlacrittyVt` directly;
the `PaneVt` trait was collapsed in the same commit per the design's
"delete rather than abstract" rule.

## F36. Workspace clipboard uses OSC 52 on this VM (MEASURED)

The cloud-agent desktop terminal accepts `\x1b]52;c;<base64>\x07` written to
stdout while the workspace runs in raw mode. When OSC 52 is unavailable,
`selection::copy_native` falls back to `wl-copy`, `xclip`, or `pbcopy` if
present on PATH. Selection text is never logged or persisted; the clipboard
write is the only export (Invariant 7).

Probe: `cargo test -p cyclops-workspace selection::tests::base64_roundtrip_shape`
plus manual OSC 52 write from the workspace selection path on the agent VM.
Native fallback is best-effort and untested on every platform variant.

## F37. Session-renamed identifies background sessions on tmux 3.7b (MEASURED)

A control client attached to one session receives `%session-renamed` when a
different session is renamed. tmux 3.7b puts the renamed session's stable id
before its new name:

```text
%session-changed $2 alpha
%session-renamed $0 gamma-probe
```

Treating the whole tail as the attached session name makes the client try to
reconcile a session literally named `$0 gamma-probe`. The notification parser
therefore accepts the current `id name` shape and retains the older one-field
shape. Workspace reconciliation switches its active name only when the id is
the attached session; a background rename refreshes the sidebar without
changing sessions.

Probe: start isolated sessions `alpha` and `gamma` on one server, run
`tmux -L <socket> -C attach-session -t '=alpha'`, then run
`tmux -L <socket> rename-session -t '=gamma' gamma-probe` from another
client. The two notification lines above arrived verbatim on the control-mode
client.

## F38. `capture-pane -a` reads the saved primary screen, never the alternate screen (MEASURED)

For a pane inside the alternate screen (every full-screen agent TUI), plain
`capture-pane` reads what is in front of the user — the TUI. `capture-pane
-a` reads tmux's *saved* grid: the primary screen as it was when the TUI took
over, which is what reappears after the TUI exits. Hydration originally
replayed the `-a` capture as though it were the alternate screen, so every
attach, tab switch, and reconnect of a Claude/Codex/Cursor pane painted the
stale shell over the live TUI. The correct replay order is: lay down the
saved primary, enter the alternate screen (`\x1b[?1049h`), then replay the
visible capture — the TUI's own later exit then restores against the right
baseline. The workspace names the field `saved_primary` so the bytes cannot
be re-misread.

Probe: `src/cyclops-workspace/tests/hydration.rs::hydrating_a_pane_in_the_alternate_screen_restores_what_the_user_sees`
on an isolated server — mark the primary screen `PRIMARY_SHELL`, enter the
alternate screen and paint `ALT_TUI_SCREEN`, hydrate through the control
client. The bundle reports `alternate_on`; the plain capture holds
`ALT_TUI_SCREEN` and the `-a` capture holds `PRIMARY_SHELL`. Under the old
replay order the test shows `PRIMARY_SHELL`; under the fixed order it shows
the TUI.

## F39. alacritty 0.26: VS16 does not widen a narrow glyph, and bare SGR 21 is bold-off (MEASURED)

Two width/attribute behaviors pinned by the bridge-fidelity fixtures so an
engine bump surfaces the change as a test failure rather than a silent
one-column shift in every warning glyph:

1. `⚠\u{fe0f}` (U+FE0F VS16, emoji presentation) still occupies one column —
   the engine sizes by the character's own width class and ignores the
   variation selector's widening request.
2. Bare SGR 21 is treated as bold-off, not double underline. Double
   underline is the colon subparameter form `4:2`.

Probe: `src/cyclops-workspace/tests/fidelity.rs`,
`a_variation_selector_does_not_widen_a_narrow_glyph` and
`every_underline_style_keeps_its_own_identity`.

## F40. Killing a session's last pane leaves no server, never an empty session (MEASURED)

A tmux session cannot exist with zero windows, and a window cannot exist
with zero panes: killing the only pane in the only window of the only
session on a server does not leave a session with no windows, it leaves no
server at all. This makes one `list-panes -a` (every pane, on every
session) structurally sufficient to discover every session and window that
exists — nothing needs a per-window follow-up query just to find out what
exists, only to name it (see `ControlClient::workspace_snapshot`,
`src/cyclops-tmux/src/snapshot.rs`, task D2).

Probe, on an isolated `-L` socket:

```text
$ tmux -u -L cyc-probe -f /dev/null new-session -d -s probe -x 80 -y 24 /bin/sh
$ tmux -u -L cyc-probe -f /dev/null kill-pane -t probe:0.0
$ tmux -u -L cyc-probe -f /dev/null list-sessions
no server running on /private/tmp/tmux-501/cyc-probe
```

## F41. `list-panes -a` exposes session-level fields (`session_attached`), not only pane fields (MEASURED)

Session-, window-, and pane-scoped format variables are all reachable from
a `list-panes -a` line, not only the pane's own fields — tmux resolves a
format against the whole session/window/pane chain a pane sits in,
regardless of which `list-*` command asked for it. `#{session_attached}`
came back correctly on every pane line for a session with no attached
client (`0`) in the probe below.

The reason `ControlClient::workspace_snapshot` still issues a second
`list-sessions` command is not that `#{session_name}` is unreachable this
way — it is reachable — but that it cannot safely share a `list-panes -a`
line with `#{window_name}`. Both are arbitrary human text, and this crate's
escaping precedent (`crate::watcher`'s `PANE_FORMAT`) only makes the *last*
field on a line safe against an embedded tab; two independent free-text
fields cannot both hold that position on the same line. See the module doc
in `src/cyclops-tmux/src/snapshot.rs`.

Probe:

```text
$ tmux -u -L cyc-probe -f /dev/null new-session -d -s alpha -x 80 -y 24 /bin/sh
$ tmux -u -L cyc-probe -f /dev/null list-panes -a -F '#{session_id} #{session_attached} #{pane_id}'
$0 0 %0
```

## F42. Workspace performance contract measurements are shape evidence, not latency budgets (MEASURED)

On this machine, `send_keys_unconfirmed` took p50 3.4us and p95 6–10us
while idle (n=500), and p50 68us/p95 125us while the target pane flooded
about 8MB. The flood path is about 20x slower at the median but remained
under 800us at the observed maximum. Sustained output drained 7.0MB in 93
batches at the 8ms cadence, with a largest batch of 143 messages/84KB and no
stall longer than five empty cycles. A 100-signal decoration burst produced
one refresh 35.6ms after its first signal; a continuous 200ms stream produced
six refreshes at roughly 30–37ms gaps. Full-frame paint of mixed
ASCII/wide/SGR content had medians of 0.99ms, 3.89ms, and 7.78ms for 1, 4,
and 8 panes.

These are record-only measurements, not thresholds. The machine was noisy;
the 4- and 8-pane paint figures came from one quiet run after an earlier
loaded run was discarded. They measure control writes, backlog draining,
coalescing, and paint work, not end-to-end frame gaps under live output.
Flow control is measured separately: five normal runs recorded
pause-to-confirmed-continue / continue-to-rehydrate times of 0.30 / 1.33ms,
0.26 / 1.30ms, 0.20 / 2.57ms, 0.21 / 2.83ms, and 0.16 / 3.23ms. tmux 3.7b
accepts `refresh-client -A <pane>:continue` but omits `%continue`, so the
successful command reply is the authoritative confirmation that emits the
consumer notification.

Probe: `CARGO_INCREMENTAL=0 cargo test -p cyclops-workspace --test
perf_contract -- --nocapture`. The test file is
`src/cyclops-workspace/tests/perf_contract.rs`; the complete recorded table
and caveats are in
`.agents/planning/2026-08-03-cyclops-workspace-tui/implementation/baselines.md`.

## F43. `swap-pane` focuses the `-t` pane id at its new slot; `-d` pins the active SLOT (MEASURED)

Probed on tmux 3.6a, isolated server. After `swap-pane -s A -t B` without
`-d`, tmux leaves pane B focused wherever it now sits, so the pane named in
`-t` is the one that keeps the user's focus through a swap. With `-d` the
active SLOT is preserved instead: whatever pane now occupies the previously
active position becomes the focused pane, and the moved pane loses focus.
Cyclops therefore never passes `-d` and rides the pane that should end
focused in `-t`: the current pane for a keyboard swap, the dragged pane for
a drop. The `{left-of}` family resolves the neighbour of the current pane
at execution time, which is what makes the directional swap need no target
resolution of its own. Pinned by the swap tests in
`src/cyclops-tmux/tests/ops.rs` and the executor rig tests in
`src/cyclops-workspace/src/app/exec.rs`.

## F44. An OSC 52 write to stdout "succeeds" on terminals that ignore it (MEASURED)

macOS Terminal.app ignores the OSC 52 clipboard sequence entirely, and the
stdout write still returns Ok, so a write result can never prove a copy
happened. The workspace's copy path treated that Ok as success and skipped
the pbcopy fallback, leaving the clipboard empty on the stock macOS
terminal. The rule the fix encodes: run the native tool whenever one is on
PATH, emit OSC 52 additionally when stdout is a terminal (it is what makes
copy work on the near side of SSH), and never let either path's result gate
the other. `src/cyclops-workspace/src/selection.rs`.

## F45. A starved runner can invalidate a timing test's premise three ways, each observable (MEASURED)

Measured on GitHub's shared runners during the v3 CI runs of 2026-08-06,
where the macOS box ran the perf-contract binary at 3x its local time and
one leg's tmux server died mid-parity. Three distinct premise failures,
each with a signature the tests now check instead of a wall-clock guess:
a burst meant to land inside one debounce window stretches past it (send
duration says so); a coalescer starved past the flush window gets its
armed refresh truncated by `Closed`, which drops pending work by design,
so a tight burst draws zero rather than two; and a tmux server that never
gets CPU during a client stall never observes the blocked reader, so no
`%pause` exists to deliver while output keeps streaming afterward. The
inverse signatures are the real regressions and still fail hard: two
refreshes from a tight burst is arm-twice, and a confirmed-flowing flood
that goes silent with no `%pause` seen is a notification lost on our
side. `src/cyclops-workspace/tests/perf_contract.rs` carries the guards;
%pause emission itself is tmux's behavior, a rig prerequisite like tmux
being installed at all.

## F46. tmux master queues control-mode notifications; a stalled client never sees %pause (MEASURED)

tmux commit 6db5175e (2026-08-03, upstream issue 5458) queues control-mode
notifications rather than emitting them inside %begin/%end. Measured on
the v3 tmux-head CI job the same week: a control client whose reader
stalls against a flooding pane sees the flood confirmed flowing, then
total silence after the stall, 0 bytes in a 500ms drain and no %pause in
5s, because the queued notification waits for a flush the stalled,
command-less client never provokes. Every released tmux (3.4, 3.6a, 3.7b)
delivers %pause to the same rig. The flow-control test skips the silent
case on "next-" builds citing this finding; the reader's 3.8 adaptation
is tracked as its own task and flips that skip back to a hard fail.

## F47. Killing a tmux session delivers no per-pane deaths to control mode, only the disconnect (MEASURED)

Measured on a live rig while fixing the immortal-label bug: kill a
watched session and its control-mode client gets a disconnect, never a
death notification per pane, so the F25 all-panes subscription that
catches an individual pane dying observes nothing at all when the whole
session goes. Anything keyed on per-pane death (the adoption registry
was) silently survives session death. The daemon now releases a
session's adoptions at the two edges that CAN answer: the attach-retry
arm, where tmux positively reports the session missing, and boot, which
re-verifies resurrected bindings for sessions outside the watched set
with one has-session each. A tmux error keeps the label: could-not-ask
never releases. src/cyclopsd/src/lib.rs, registry.rs; pinned by
tests/m4_name.rs.

## F48. `window-size latest` lets any regular client out-size a control client's declared canvas (MEASURED)

Measured on tmux 3.6a while fixing the invisible-typing overflow in the
workspace. A control client only counts for window sizing after it
declares a size with `refresh-client -C` (the daemon's watcher, which
never declares, is ignored: a session with the workspace at 176 and the
daemon attached stays at 176). But under `window-size latest` the
declaration is not authority: attaching a plain 240x60 client to the
same session snapped the window from the workspace's declared 176x46 to
240x58, and a second declaring control client did the same, so panes
laid out 64 columns wider than the painted canvas and typed text ran
past the visible pane edge. A control client can never win `latest`
back, because latest follows tty input and control clients produce
none. `window-size smallest` is a fixed point instead: the window is
the minimum over declaring clients (176 with the 240 viewer attached,
150 when the viewer redeclares 150x40, back to 176 when it detaches),
so the window never exceeds the workspace canvas and a smaller viewer
only shrinks it, which the canvas absorbs as gutter. Probe: two `tmux
-C attach` coprocesses issuing `refresh-client -C` against one rig
session, plus a real client on a second tmux server. Pinned by
src/cyclops-workspace/tests/geometry.rs.

## F49. Apps dress for the ground tmux reports, and one pane serves two terminals at once (MEASURED)

Measured with codex 0.146.1 on tmux 3.6a. In a detached rig session
codex styled fg-only (bold cyan selected row, no fills): with no answer
to its OSC 11 background query it commits to no ground. Setting a pane
style (`select-pane -P 'bg=#1e1e1e'`) before launch made tmux answer
the query, and the same codex painted its full dark theme, an explicit
`48;2;57;57;57` composer fill. On a user's machine the answer comes
from whichever real terminal taught tmux its background, so agents
dress for the user's dark terminal inside a light workspace. The
tempting fix, teaching tmux a light ground so apps dress light, is
wrong by construction: the same pane is still viewed through the user's
own dark terminal, and one escape stream cannot dress for both grounds.
The restyle belongs at render, per viewer: the workspace re-grounds
neutral fills at the opposite luminance extreme to the theme's own
panel (`matched_ground` in src/cyclops-workspace/src/render/mod.rs),
the readability floor sets their text, and the same pane stays native
in the dark terminal. Pinned by render::contrast_tests.

## F50. A malformed subscription value is a clean-prefix truncation already on tmux's own wire; the trigger did not reproduce under driving (MEASURED, non-reproduction, tmux 3.7b only)

`~/.cyclops/cyclopsd.log` carries 69 "malformed subscription value"
warnings over 2026-08-04..07, all from `apply_sub_value`
(`watcher.rs:524`) failing to collect all 5 pieces of a `SUB_FORMAT` push
via `value.rsplitn(5, '\t')`. The parser itself is known-good (established
prior to this investigation). The first question: is it `-u` (F14), the
client's own line handling, or tmux's own output that is short.

**`-u`: closed by READ.** `ControlClient::spawn` (`control.rs:316`) passes
`-u` on every invocation unconditionally — it is not gated on any
environment variable, so nothing about a minimal launchd/cron environment
can omit it. The raw log bytes confirm this held for the failing
environment too: `grep 'malformed subscription value' ~/.cyclops/cyclopsd.log
| cat -tv` shows a real 3-byte UTF-8 glyph (`✳`, tmux's spinner character)
and real tab bytes (`^I`) between fields in the malformed lines — exactly
what F14's sanitization (replacement with `_`) would have destroyed. It
was not in effect when these lines were produced.

**The client's line reassembly: closed by READ.** `reader_task`
(`control.rs:706-722`) calls `read_until(b'\n', &mut line)` in a loop; that
call keeps reading from the child's stdout until it actually sees a `\n`
byte (or EOF), so a value split across two underlying reads is
reassembled before anything else touches it, and `LineRouter::feed`
(`control.rs:231`) hands the whole line straight to the notification
parser with no further splitting. Nothing on this path can hand the
parser fewer bytes than actually arrived on the wire, for a connection
that stays open.

**EOF mid-write: a real code gap, closed for THIS corpus by MEASUREMENT.**
The `Ok(_)` arm of `read_until`'s match in `reader_task` does not check
that the accumulated `line` ends in a newline before the code proceeds —
only the following `if line.last() == Some(&b'\n') { line.pop(); }` is
conditional. So a connection that dies mid-write would hand the router a
trailing partial line with no terminator, and it would be processed as if
complete — the same shape as this bug. Cross-checking all 69 malformed
timestamps against every "tmux connection lost; reattaching" line in the
same log: none falls within 5 seconds of a reconnect (closest is 231s
away; median distance is roughly 46 minutes). This corpus is not
reconnect-adjacent, so this gap — real, and still worth closing — is not
what produced it.

**The reconcile-storm angle: checked, also not correlated.** The corpus's
date range sits inside the session the zombie-watcher-storm fix came from,
whose signature is a flood of "hint-driven reconcile failed" lines
(36,608 total here, concentrated in dense bursts on 08-06 20:11-20:13 and
08-07 07:00-10:00 and 19:00). None of the 69 malformed timestamps land
inside those bursts: every one has 0 or at most 1 "reconcile failed" line
within a 5-second window either side.

**Live reproduction: driven, not reproduced. This machine has tmux 3.7b
only** (`tmux -V`); nothing here says anything about 3.6a or a
next-3.8-class build, and F13/F25 already establish those differ on
subscription behavior. Three rounds on an isolated `tmux -L
cyc-inv-scratch` server, each with a `tmux -u -C -L cyc-inv-scratch attach`
control client fed through a FIFO-backed stdin (so `refresh-client -B`
could be issued interactively) with its stdout captured in full to a log
file, driven from a second plain `tmux -L cyc-inv-scratch ...` for
everything else:

- Round 1 (one pane, one control client): a baseline title change; 60
  rapid plain `select-pane -T` changes; 80 rapid changes cycling five
  spinner glyphs including `✳`; a 200-iteration OSC title flood
  (`printf '\033]2;...\007'`) from inside the pane; 8x pane-birth races
  (subscribe the instant after `split-window`, before the shell finishes
  starting); 8x pane-death-mid-push (`sleep 0.05; exit 0`, subscribed the
  instant the pane exists); and the production flow-control flag
  (`refresh-client -f 'pause-after=300'`) enabled during a further round
  of rapid multibyte titles. 7 `%subscription-changed` lines arrived (most
  rapid changes coalesce into one push per tick, consistent with F23's
  1Hz ceiling); every one carried all 4 tabs.
- Round 2 (4 concurrently subscribed panes): simultaneous OSC-title floods
  layered with subprocess spawn/kill churn on all 4 panes at once for
  15s, repeated abrupt `kill -9` of each pane's foreground child while
  subscribed, and title changes racing a tight `current_command` churn
  loop. 132 pushes arrived, every one 4-tab.
- Round 3 (production-shaped load): `resize-pane`/`select-layout` churn
  alternated against the session; a SECOND control client subscribed
  identically to the first, then frozen mid-stream with `SIGSTOP` to model
  a stalled or duplicate attached client (the zombie-watcher-storm
  mechanism: a client tmux does not reliably drop); the PRIMARY connection
  concurrently issuing 400 `list-panes` commands in a tight loop —
  reconcile-shaped command traffic sharing the same wire as the
  notifications — while the same 4 panes kept flooding titles. 20 pushes
  arrived, every one 4-tab; all 400 command replies closed clean with no
  `%error`.

159 `%subscription-changed` pushes captured across the three rounds, all
well-formed. Zero reproductions of a truncated push, despite covering
every suspect the investigation recipe named (pane birth, pane death,
multibyte titles, rapid successive changes) plus resize churn, a stalled
second client, and reconcile-style command traffic sharing the connection.

**Shape census of the real corpus (MEASURED, log analysis).** Field-count
histogram over the 69 lines (counting the handoff's way: fields, not
tabs): 2 fields 48x, 4 fields 10x, 3 fields 5x, 1 field (value entirely
empty) 6x. Representative lines:

```text
value=✳ Claude Code\t0\t0\t                        (pane %0, 4 fields)
value=cyclops\t0\t0\t                              (pane %1, 4 fields)
value=[ . ] Action Required | cyclops-workspace\t  (pane %1, 2 fields)
value=                                              (pane %1, 1 field)
```

Every occurrence is a clean prefix — never a garbled byte mid-field —
which is what the parser being fine plus something upstream stopping
partway through the fixed suffix would produce. One precision point:
because `rsplitn(5, '\t')` assigns its five destructure slots
right-to-left (pid first, title last), ANY shortfall leaves `title`
unassigned regardless of which of the four fixed fields tmux actually
failed to emit, and shifts whatever fixed-field values did arrive into
the wrong-named slots (a 4-field push binds the `dead` variable to the
real title text, not to a dead flag). The log line's field COUNT is
diagnostic of truncation depth; it cannot say which named field was
dropped.

A genuine cross-pane burst also turned up: `05:20:29.961819` (pane %1, 1
field/empty) → `05:20:30.033197` (pane %4, 3 fields) → `05:20:30.033530`
(pane %1, 2 fields) — three malformed pushes across two different panes
within 72ms. Whatever this is, it is not purely a per-pane phenomenon; it
can land on more than one pane's subscription at essentially the same
moment.

**A censoring effect worth recording (READ, new).** `apply_sub_value`'s
destructure only fails — and only then warns — when `rsplitn(5, '\t')`
returns fewer than 5 pieces. A truncation that stops exactly one field
short of the end (`title\tdead\tin_mode\tcmd\t`, trailing tab present,
pid empty) still splits into 5 pieces: `parse_pane_pid("")` returns
`Some(-1)` by design (F25's precedent for an empty pid field), and the
row's `pane_pid` is silently set to -1 with no warning and no
`Action::Hint`. The 69-line corpus is therefore a floor: the same
phenomenon, one field shallower, is invisible in the log and silently
corrupts `pane_pid` until the next full push overwrites it — low
consequence (self-healing, and F25 already documents `pane_pid` as
unreliable around a pane's death), but the warn count understates how
often the underlying truncation actually happens.

**What follows for the code.** Every one of the 69 real occurrences is a
clean-prefix truncation the handler already treats correctly:
`Action::Hint` forces a reconcile, so the table is never stale for longer
than one debounce window. The investigation could not identify a trigger
despite three rounds of driving, so nothing here supports fabricating a
value tmux never sent — padding defaults would be exactly the lie F25
already declined to tell for a differently-shaped gap in this same
subscription mechanism. What the measurement does support: the first
`warn!` in `apply_sub_value` (the `rsplitn` shortfall) fires on a shape
that is, by construction, always a clean prefix — it can only be reached
with fewer fields, never garbled ones — so it belongs at `debug!`, not
`warn!`. The second `warn!` in the same function (an unparseable
`pane_pid` on an otherwise-complete 5-field push) is a different,
still-unobserved shape and should stay at `warn!`. Proposed diff, anchored
on the code, not applied here (concurrent edits to `watcher.rs` were in
flight during this investigation):

```rust
     let mut it = value.rsplitn(5, '\t');
     let (Some(pid), Some(cmd), Some(in_mode), Some(dead), Some(title)) =
         (it.next(), it.next(), it.next(), it.next(), it.next())
     else {
-        warn!(%pane, %value, "malformed subscription value");
+        // F50: every observed occurrence is a clean-prefix truncation
+        // tmux itself sent short (tmux 3.7b, trigger not identified);
+        // the handler already self-heals via Action::Hint below, so
+        // this is diagnostic noise, not a correctness signal.
+        debug!(%pane, %value, "malformed subscription value");
         return Action::Hint;
     };
```

Probe: isolated server `tmux -L cyc-inv-scratch`, torn down at the end
of each round (shell probes use `cyc_tmux_teardown` from
`tests/e2e/lib/lib.sh` for this); driver
scripts and raw capture logs kept under the investigation's scratch
directory. Corpus analysis: `~/.cyclops/cyclopsd.log` (read-only),
grepped for `malformed subscription value`, `tmux connection lost`, and
`reconcile failed`, cross-referenced by timestamp.

## F51. tmux 3.7b's window-index and current-window bookkeeping races under heavy parallel fork load, even on a fully isolated server (MEASURED)

`src/cyclops-tmux/tests/ops.rs`'s
`split_opens_in_the_source_panes_directory_not_the_sessions` failed once
under a full-suite parallel run with `create window failed: index 0 in
use` from its `new-window -t s -c <dir> /bin/sh` — a plain session target,
letting tmux pick the next free index itself, run immediately after
`new-session -d -s s` on the same (freshly created, single-window)
session. This looked at first like a cross-test collision (two parallel
tests sharing a scratch `-L` server), the working theory in the handoff
that sent this investigation. It is not: reproduced directly with the
`tmux` binary (no Rust harness, no `cyclops_testrig`), each of N parallel
shell workers using its own `-L cyc-<n>-$$-$RANDOM` socket, so no two
workers could ever share a server. A bare `-t s` target still failed
`index 0 in use` 3/180 tries under 60-way parallel load; a bare `-t s
-d` (suppressing focus, tried as an alternative) failed worse, 6/180.
Giving `new-window` an explicit target index (`-t s:1`, since a
from-scratch session's first window is always 0 under `-f /dev/null`'s
default `base-index`) reproduced zero `index in use` failures over 360
tries.

A second, related race showed up once the first was fixed: reading the
new window's pane back through `list-panes -t s` (a session-level target,
which resolves to the session's *current* window) intermittently returned
the *first* window's pane instead — `display-message -p -t s
"#{window_index}"` read back `0` immediately after a successful,
`-d`-less `new-window -t s:1` on 3/80 tries, even though the tmux manual
states plainly that omitting `-d` makes the new window current. Querying
the explicit window (`list-panes -t s:1`) instead of the session
reproduced zero mistargeted-pane failures over 100 tries, because it does
not depend on that current-window hand-off having landed yet.

Both races are internal to a single tmux server under load, not a
cross-process collision: every trial above used a socket name no other
process could have touched. The specific trigger inside tmux's own
session/window bookkeeping was not isolated further; that would need
reading tmux's C source or building an instrumented tmux, which was out
of scope for a test-hygiene fix. What is established is the workaround:
prefer an explicit window index over "let tmux pick", and prefer an
explicit window target over "read it back through the session's current
window", whenever a test creates a second window and immediately depends
on it.

Probe (representative; run from a plain shell, no tmux session needed):

```text
$ . tests/e2e/lib/lib.sh   # cyc_tmux_teardown
$ for i in $(seq 1 60); do
    sock="cyc-probe-$i-$$-$RANDOM"; sdir="/tmp/p-$i-s"; pdir="/tmp/p-$i-p"
    mkdir -p "$sdir" "$pdir"
    ( tmux -u -L "$sock" -f /dev/null new-session -d -s s -x 120 -y 30 -c "$sdir" /bin/sh
      tmux -u -L "$sock" -f /dev/null new-window -t s -c "$pdir" /bin/sh
      cyc_tmux_teardown "$sock" ) &
  done; wait
# observed 3/180 (three such batches): create window failed: index 0 in use
```

## F52. tmux 3.7b's `pause-after` clock only starts once a stalled reader's backlog has actually crossed the threshold, not from wall-clock elapsed alone (MEASURED)

`src/cyclops-workspace/tests/perf_contract.rs`'s
`flow_control_pause_and_resume` stalls its own single-threaded tokio
runtime for a fixed 2 seconds to reproduce a real `%pause` (see the test's
own doc comment for why a genuine executor stall is the only bounded way
to get tmux to emit one against the real `ControlClient`). Under a
same-machine 12-way parallel stress (12 copies of the same test, each
running its own isolated `yes flood`), that fixed stall failed to produce
`%pause` within a further 5-second wait 5/12 and 6/12 tries across two
batches — "never saw %pause after the stall".

Instrumenting the wait loop to log every notification it saw (not only
`%pause`) showed the failing runs draining thousands of `%extended-output`
messages the instant the stall ended, every one at `age_ms` at or near 0 —
i.e. the control connection was never actually behind once draining
resumed. This rules out the stall itself failing to block the reader
(the executor mechanism is sound and reproduces fine standalone); what it
shows is that `yes flood`'s output had not backed up enough during the
2-second stall to ever cross `pause-after`'s 1-second-behind threshold in
the first place, so tmux had nothing to pause. Twelve parallel copies of
an unbounded flood plus twelve parallel tmux servers is heavy CPU
contention system-wide; this measurement identifies where the backlog
went missing (it never accumulated) but not which specific process was
denied the CPU to produce it — that finer attribution was not chased
further.

A fixed, single stall length cannot be raised enough to cover this
reliably, because contention on a shared host is not bounded by anything
this test controls. What resolved it: retry the stall itself with
backoff (2s, 4s, 8s, 16s), each retry a fresh, *uninterrupted* block —
checking for `%pause` in between attempts and then continuing would let
the reader drain whatever little had queued and reset tmux's "how long
has this client been behind" clock to zero before the next attempt even
starts. Across 48 runs of the fixed test under the same 12-, 16-, and
20-way parallel stress that reproduced the original failure at up to 50%,
zero failed; most passed on the first (2s) attempt in about 2.3s, and the
few that needed a retry still passed, up to 24s in the worst observed
case.

Probe: `env -u TMUX -u TMUX_PANE cargo test -p cyclops-workspace --test
perf_contract flow_control_pause_and_resume -- --exact --nocapture`, run
as N parallel copies of the built test binary
(`target/debug/deps/perf_contract-*`) from a plain shell. The test file is
`src/cyclops-workspace/tests/perf_contract.rs`.

## F53. `display-message -t` needs a window/pane to fall back to; a bare `=session` exact-match target resolves to nothing (MEASURED, tmux 3.7b)

Resolving a session's stable `$id` at watcher connect (for following a
`%session-renamed` of the watcher's own session, `src/cyclops-tmux/src/watcher.rs`'s
`resolve_session_id`) tried `display-message -p -t '=<session>' '#{session_id}'`
— the same `=name` exact-match target every other session-scoped command in
this crate uses (`crate::cmd::session_target`). It came back empty: no
`%error`, exit fine, just a blank line where `$0` belongs, and every rename
test built on it timed out waiting for a `SessionRenamed` that never came.

`display-message -p -t 'session' '#{session_id}'` (no `=`) works and prints
`$0`. `display-message -p -t '=session:' '#{session_id}'` (trailing colon)
also works. The difference is what tmux falls back to: `display-message`
has to resolve `-t` down to an actual pane to evaluate any format against,
and a bare session name without a window part falls back to that session's
current window; `=session` alone apparently does not carry that fallback,
while `=session:` explicitly names the session's window list and gives
tmux something to resolve. This is the same trailing-colon shape
`ControlClient::move_window_to_session` already uses for a different
reason (naming the window list rather than colliding on a window index) —
worth remembering as the fix for `display-message -t` specifically, since
`has-session`, `rename-session`, and `list-panes -s -t` all accept a bare
`=session` target fine and do not need it.

Probe: isolated `tmux -L cyc-dbg -f /dev/null new-session -d -s before`,
then `tmux -L cyc-dbg display-message -p -t '=before' '#{session_id}'`
(empty output) vs. `tmux -L cyc-dbg display-message -p -t '=before:'
'#{session_id}'` (prints `$0`), same server, same session, only the
trailing colon differs. `src/cyclops-tmux/tests/watcher_rename.rs`'s
`a_renamed_session_keeps_flowing_under_the_new_name` is the regression
test: it failed with the bare target and passes with the trailing colon.

## F54. Codex 0.147.0 can keep a multiline bracketed paste expanded (MEASURED)

The currently installed Codex CLI no longer reproduced the collapsed
composer shape audited against 0.146.x. `codex --version` reported
`codex-cli 0.147.0`; `tmux -V` reported `tmux 3.7b`. In an isolated tmux
server, a detached 120x40 pane launched both `codex -C <scratch checkout>`
and `codex --no-alt-screen -C <scratch checkout>`. Each probe pasted
the same scrubbed 26-line payload (a Cyclops-shaped header, 24 short payload
lines, and a reply hint) with `set-buffer` followed by `paste-buffer -p`.
After one second, both `capture-pane -p` and `capture-pane -p -e` showed the
composer glyph followed by the header and all payload lines. Neither capture
contained a collapsed placeholder or a `Pasted` chip, and the escaped view
carried no collapsed-composer SGR boundary to encode.

Probe command shape (the payload text was scrubbed and is not retained):

```text
tmux -L cyc-codex-live -f /dev/null new-session -d -s codexprobe -x 120 -y 40 /bin/zsh
codex [--no-alt-screen] -C <scratch checkout>
tmux -L cyc-codex-live -f /dev/null set-buffer -b cycprobe <multiline payload>
tmux -L cyc-codex-live -f /dev/null paste-buffer -b cycprobe -t codexprobe:0.0 -d -p
tmux -L cyc-codex-live -f /dev/null capture-pane -p -t codexprobe:0.0
tmux -L cyc-codex-live -f /dev/null capture-pane -p -e -t codexprobe:0.0
```

Correction from F62: this probe established one raw representation, not a
version-wide rendering contract. Later measurements on the same Codex version
produced both raw composer content and collapsed chips depending on the
individual paste. The verifier therefore classifies each observed
representation and never selects one from the vendor version alone.

## F55. Claude Code 2.1.222 composer byte shapes: NBSP is the composer's signature (MEASURED)

A live delivery held with cause `idle_with_input` against a Claude pane
whose composer was empty except for ghost/suggestion text. The shipped
plain rule `^\s*❯\s+\S` cannot tell a human draft from anything else that
paints text after a `❯`, so the escaped capture had to carry the
discriminator, and the exact bytes had not been measured for Claude. An
isolated probe measured them: `claude` launched in a detached 140x40 pane
on its own tmux socket, captured with `capture-pane -p` and `-p -e` while
empty, with typed text, with a collapsed paste chip, and after a completed
turn.

Measured shapes (escaped capture):

```text
empty composer   ESC[39m❯<NBSP>
typed text       ESC[39m❯<NBSP>fix
paste chip       ESC[39m❯<NBSP>[Pasted text #1 +8 lines]
submitted echo   ESC[38;5;239mESC[48;5;237m❯ ESC[38;5;231mReply with…ESC[39m
```

Probe command shape:

```text
tmux -L cycprobe new-session -d -x 140 -y 40 -c <repo> "env -u CLAUDECODE claude"
tmux -L cycprobe send-keys -t %0 -l 'fix'          # then C-u, then paste-buffer -p
tmux -L cycprobe capture-pane -p -t %0             # and capture-pane -e -p
```

Three facts matter. The live composer line is `ESC[39m` + glyph + a
NO-BREAK space (U+00A0), and real input, typed or pasted, follows that
NBSP with no styling at all. The submitted-prompt echo in scrollback
repaints the glyph with its own colors and a PLAIN space, so it can never
carry the NBSP signature, which closes a second latent false positive:
that echo matches the plain staged-input regex whenever it lands in the
bottom window. The ghost line itself never rendered during this probe
(history prefixes, slash prefixes, and a completed turn all failed to
produce one on 2.1.222), so its dim-run shape is carried by the same
convention codex and cursor measured (F19); the staged-input esc clause
does not depend on it, only on real input being unstyled after the NBSP.

Fixtures: `src/cyclops-manifest/tests/fixtures/claude_{typed_composer,pasted_chip,prompt_echo}_{plain,esc}.txt`.

## F56. The VT engine's 2x1 floor is documented but not enforced (MEASURED)

A cyclops pane in which the operator ssh'd to a remote machine and
attached tmux there crashed the workspace. The pane's content is just a
full-screen program's byte stream; what killed the app was geometry, not
bytes. alacritty_terminal 0.26 declares `MIN_COLUMNS = 2` and
`MIN_SCREEN_LINES = 1` and enforces neither: `Term::new` and
`Term::resize` accept zero, and a grid sized below the floor panics on
the next cell write or resize (measured: the engine's grid/resize.rs
line 291, in the registry copy of alacritty_terminal 0.26.0, panics when
the recorded nested-tmux stream is fed through a `resize(0, 0)`; a
zero-width grid also panics on the first byte that writes a cell). Pane dimensions
reach `PaneRuntime` from tmux layout parsing and can pass through zero
while windows churn, which an attaching full-screen client provokes.

Probe: `src/cyclops-workspace/tests/fixtures/nested_tmux_client.raw` is a real pipe-pane
recording (tmux 3.7b, 140x40) of an inner `tmux -L nestinner` starting,
attaching, running seq and ls, splitting, and detaching. The regression
test `a_nested_tmux_client_byte_stream_never_panics_the_runtime` feeds it
at seven geometries down to 0x0; before the clamp it dies inside the
engine, after it every read surface stays total.

Fix shape: `PaneRuntime` clamps every path that sizes the engine
(`new`, `resize`, `reset`) to the engine's own floor. Nothing detects or
manages the nested tmux itself: the pane is an opaque terminal on
purpose, and the daemon already reads it as `? unknown` (ssh matches no
manifest).

## F57. State files need one descriptor-anchored owner (READ, high severity)

The ledger created directories and files through caller-supplied paths, so
permissions depended on the process umask and validation could not bind a
checked path to the inode later read or changed. A symlink or hard link also
made permission repair capable of changing an external target.

Ledger access now begins with one validated root descriptor supplied by the
reusable state-path crate. Descendants are relative names, directory and file
ownership is checked before repair, and links or unexpected file types are
refused before their bytes are exposed. The dedicated umask child is
`src/cyclops-state/src/lib.rs::tests::creation_is_owner_only_under_permissive_and_restrictive_umasks`.
The production-writer contract, startup repair, symlink, and hard-link probes
are in `src/cyclopsd/tests/state_permission_contract.rs`. The remaining state
writers were migrated through the same owner before the messaging candidate
was frozen.

## F58. agy paints no composer ghost text

Measured 2026-08-20, agy 1.1.13, live idle pane, `tmux capture-pane -e -p`.
An empty agy composer renders exactly `ESC[94m>ESC[39m`: the prompt glyph
is styled, and nothing follows it. No placeholder, suggestion, or ghost
text of any kind.

So `composer_has_input` needs no SGR discriminator today: any content after
that glyph is a human's. If a later version paints a suggestion there, the
rule reads it as a draft and deliveries hold forever, which is the benign
direction and is visible in `cyclops read <agent> --source detection`.

Scope limit, stated because it matters: this covers the idle state on one
version. Typed-input styling was not captured, since the only live agy
pane belonged to another agent's working session, and typing into someone
else's composer to take a measurement is the exact act the delivery gate
exists to prevent. Adding an esc clause on one observation would also
flip agy's capture flavor to escaped for every delivery, which is a
bigger behavioral change than the finding supports.

Same capture confirmed the `composer_trailer_regex` chrome shipped for
agy: the box rules de-escape to `────…` and the status row to
`Gemini 3.7 Flash · … · Ctx: 78% · …`.

## F59. Composer chrome vocabulary is vendor-specific (MEASURED)

The terminal sentinel can only prove it is the LAST payload token if
something says which rows below the composer are not payload. That
vocabulary is `injection.composer_trailer_regex`, and these are the
captures it was written from.

Procedure. For a live pane: `tmux capture-pane -e -p -t <pane>` on an
idle composer, then read the tail rows. For the fixture vendors:
`src/cyclops-manifest/tests/fixtures/*_plain.txt`, whose tails were
captured the same way in earlier delivery tests and the F55 probe.

Measured rows, 2026-08-20 unless noted:

- claude (fixtures `claude_idle_2_1_221.txt`,
  `claude_pasted_chip_plain.txt`): a box rule `────…`; a status row
  `  Opus 5 · xhigh · ~/projects/clops · Ctx: 97% · 5h: 93% · 7d: 94% ·
  1000K window · 28K used`; a hint row `  paste again to expand`; a mode
  row `  ⏸ manual mode on · ← for agents`.
- codex (fixture `codex_pasted_chip_plain.txt` plus a live capture):
  a blank line, then `  gpt-5.6-sol high · <cwd> · 258K window · 2.87M
  used`, live variant `  gpt-5.6-sol xhigh · ~ · Full Access · Context
  87% left · weekly 97% left · 258K window · 2.87M used`.
- agy (live, agy 1.1.13, `tmux capture-pane -e -p`): box rules render
  `ESC[90m────…`, the empty composer is `ESC[94m>ESC[39m`, and the
  status row de-escapes to `Gemini 3.7 Flash · High · ~ · Full · Ctx:
  78% · 93% 5h, … · 507K used`.
- cursor: NOT measured. No cursor pane exists in this fleet and none was
  installed to make one. Cursor therefore ships no trailer vocabulary,
  and no chip pattern either, since it does not collapse a paste.

  CORRECTED 2026-08-21. An earlier version of this line said that lane
  "keeps the leading-id evidence it had before". It does not, and cannot:
  `staged_representation` recognizes only a measured visible target or a
  collapsed chip, and `exact_staging_proof` authorizes Enter only for visible
  exact bytes. Cursor has neither measured representation, so
  `verify_pattern = ["<message_id>"]` is inert and every Cursor staging verify
  refuses. That is the correct fail-closed
  answer for a vendor nobody measured, and restoring leading-id
  acceptance would reintroduce the failure described in F62. It is also a
  live blocker rather than a soak-only
  concern: on this tree, every Cursor delivery ends in
  attention_required. Closing it needs a capture off a current Cursor,
  not a decision.

Consequence worth stating plainly, because it bounds what these patterns
can promise: a status row and a sentence are both text, and a pattern
loose enough to match the row can match prose. `context · budget · 128K
window` is structurally identical to codex's real row, so the codex
pattern is anchored on the model-name shape instead, and it fails closed
when the model family is renamed. `shipped_trailers_reject_adversarial_
payload_text` holds the line: every pattern is tested against prose
derived from it, and a pattern that matches payload is a bug.

## F60. Claude 2.1.236 keeps the idle sparkle through a long turn

Measured 2026-08-20 on macOS 15.5, live, Claude Code 2.1.236. A tool
execution ran continuously for over 28 minutes while `cyclops status`
reported the pane idle.

Cause, and it takes two rules agreeing to produce it. Claude sets the
terminal title to "✳ <summary>" when a turn ends and KEEPS that title
through whatever runs next, so `title_idle_sparkle` matched. The input
composer stays drawn below the streaming output, so `composer_empty`
matched too. Title and screen agreed on idle, disagreement was false,
and under INVARIANTS rule 12 that shape is write-ready: the gate would
have pasted into a pane mid-generation.

Probe: read-only sampling across animated frames. The active status row
cycles a leading glyph (·, ✶, ✽, ✳, ✢, ✻) and a gerund verb
("Kneading…", "Jitterbugging…") painted in SGR 38;5;215, followed by a
running timer envelope in 38;5;246. Captured to
`src/cyclops-manifest/tests/fixtures/claude_working_2_1_236_esc.txt`,
sha256 81f8b3b89c3c6c894ce8cfdb2b88748afe36d6d7d064e424b14d454a8e61a6ed,
a minimized fixture derived from one live escaped capture. Transcript prose
above the evidence window is neutralized while its row count and styling are
retained. The active status, composer, and chrome rows are unchanged. The test
derives the plain form from those same bytes so both forms describe one moment.

Negative separation, which is why the rule is style-bound and not a word
match: completed steps stay in the transcript forever, rendered with ⏺
or "Cooked for" in uniform 38;5;246 or 38;5;231. They never combine the
215 glyph and verb with the 246 timer envelope in the bottom region.

Fix: `composer_working_spinner_status` in
`resources/manifests/claude.toml`, priority 1150, region
`bottom_non_empty_lines(10)`, matching the escaped row. 1150 puts it
above `title_idle_sparkle` (1000) and `composer_empty` (900), so an
active turn outranks both halves of the agreement that produced the
false idle.

## F61. Hook reports required a label the hooks did not have

Measured 2026-08-20. `~/.cyclops/hook-errors.log` grew continuously with
`no agent identity; set CYCLOPS_AGENT or pass --agent`, and no hook edge
had ever reached the daemon from any pane.

What is proven: `cyclops hook` refused before connecting unless a label
was supplied, the codex and agy hook files invoke it with no `--agent`,
and the panes running them had no `CYCLOPS_AGENT` in their environment.
The claude pane had no hook wiring at all, so it contributed no errors
and no edges either.

What is NOT proven, and worth stating because it is the tempting story:
whether a vendor strips the environment for hook subshells, or the panes
were simply launched without the variable. Nobody probed that, and the
fix does not depend on the answer.

Consequence: `hooks_verified` was false fleet-wide and every receipt
degraded to the screen tier. The fix makes the label optional and has
the daemon derive the origin from the authenticated socket peer, which
it already computed in order to verify reports. A supplied label stays
an assertion, checked against that origin and denied on disagreement.

## F62. The leading id is not evidence, and chip versus sentinel is a per-message decision

DELIVERY.md states both properties as rules. The captured layouts and paste
behavior below provide their evidence.

**A visible `[cyclops <id>]` proves only that the head of the payload
arrived.** A truncated paste proves exactly the same thing, and that is
the failure the sentinel exists to catch: with a long payload the id
scrolls out of the verify region while the tail is still on screen, so
the id is simultaneously the weakest evidence and the one most likely to
be missing. Accepting it would submit half a message; requiring the
sentinel instead moves the proof to the END of the payload, where
"arrived" and "arrived whole" are the same question.

**Terminality is decided by the escaped capture, not by prose.** On every
vendor measured so far (F59), composer chrome arrives painted with SGR
attributes while a pasted payload row arrives plain, so prose shaped like
a status row fails the escaped half of the trailer layout. That is a
measured property of those three layouts, not a law about terminals. A
vendor that paints pasted text would need its own measurement, and until
somebody takes it the sentinel path refuses on that vendor rather than
guessing.

**Chip and sentinel are chosen by the render, not the vendor.** The same
CLI at the same version produces a raw-wrapped payload for one message
and a collapsed "[Pasted Content N chars]" chip for the next, depending
on size and content (F54, Codex 0.147.0). So the two evidence paths
cannot be assigned per vendor at manifest-write time; verification tries
the sentinel first and falls to the chip on what the capture actually
shows.

## F63. The in-process report origin could not place a pane during a detach

Measured 2026-08-21 by `hook_ack_during_detach_resolves_the_delivery`,
which failed with `{"applied":false,"reason":"occupant_changed"}`.

`Daemon::report_state` is the pre-trusted in-process entry for hook
reports. It states the origin the socket path would have derived rather
than trusting the caller, which is right. But it derived that origin
through `resolve_recipient`, and that function answers only from LIVE
watchers by design. During a detach it returns None, and the fallback
was `(name, 0, None)`: pane id replaced by the label, pid zero.

Downstream, `handle_report` resolves the pane through
`resolve_recipient_last_known`, so it found a real row with a real pid
and compared it against the origin's zero. Exact process-identity binding
then refused the report as an occupant change, and it refused it
BEFORE reaching the branch that names an unattributable origin, so the
reason was wrong as well.

The window this broke is the one the path exists for: a vendor hook
fires while nobody is attached, which is the case that motivated the
in-process trusted path in the first place. Refusing it re-opens the
duplicate-delivery hole the soak found.

Fix: derive the detached origin from `resolve_recipient_last_known`, the
same record `handle_report` uses, and keep the zero pid for panes that
genuinely cannot be placed. The daemon still derives the origin itself,
so nothing about the trust boundary moves.

## F64. codex paints a blank row below the composer, and the layout did not declare it

MEASURED, from rows 36 to 38 of `codex_pasted_chip_plain.txt` and
`codex_pasted_chip_esc.txt`, which are the same capture in both forms:

```
36  › [Pasted Content 2828 chars]
37
38    gpt-5.6-sol high · /private/tmp/... · ...
```

Row 37 is blank in both forms, with no SGR at all in the escaped one. The
shipped `composer_trailer_regex` listed only the model row, so a
raw-wrapped codex sentinel produced a suffix of [blank, model] whose
first row matched nothing in the layout, and `sentinel_proof` refused.
Correct behaviour for an undeclared row, and it left codex with no
sentinel path: every raw-wrapped delivery to it went to
attention_required.

The fix declares the blank as layout entry 0, with the required prefix
raised to 2. Filtering blank rows out instead would have been the wrong
repair in the same way for both vendors: an undeclared blank after the
sentinel is exactly what a truncated capture looks like, and that
refusal is load-bearing (`a_blank_row_after_the_sentinel_fails_closed`).

What is NOT proven, and matters for terminality: no real raw-wrap capture off
codex exists in this tree. The regression
(`a_codex_raw_wrap_verifies_through_its_measured_blank_separator`) uses
the real chrome rows verbatim in both forms and synthesizes only the
composer row. It proves the declared layout matches what codex paints
below the composer. It does not prove what codex does to a long paste,
including whether continuation rows carry leading indentation, which
would change whether the sentinel is a whole row. That needs a capture
off a live codex.

CORRECTED 2026-08-21. F65 adds the missing live Codex 0.149.0 raw-wrap
capture and proves its continuation indentation and styled trailer against
the generic structural extractor. The paragraph above records the evidence
boundary when F64 was written; it is no longer the current boundary.

## F65. Whole-composer clearing is measured on Claude and Codex only

Measured 2026-08-21 on macOS with tmux 3.6a, Claude Code 2.1.239,
Codex CLI 0.149.0, and Antigravity CLI 1.1.18. Each probe ran in its own
scratch tmux session under
`/private/tmp/cyclops-attention-resolution`. The trust prompts were
accepted. Probe payloads were staged but never submitted to a model.

One `C-c` cleared both a visible three-line payload and a collapsed
92-line paste on Claude and Codex. The pane process remained alive. The
candidate sequence `C-e C-u` was unsafe on all three CLIs because it
removed only the last logical line and left the header and body staged.
Antigravity also kept the staged payload after `C-c`, `Escape`,
`C-a C-k`, and `Home C-k`. It therefore has no shipped clear sequence.
Cursor was not installed and remains unmeasured.

`tmux capture-pane -e -J` reconstructed Claude and Codex physical wraps
as logical rows. Both use two spaces before later logical composer lines,
so removing the measured prompt row and continuation prefix reproduced
the exact normalized `render_payload` bytes, including the terminal
sentinel. Antigravity hard-wraps the long line into separate padded rows
inside the application, so tmux has no physical-wrap marker to join.
Its underlying composer bytes cannot be reconstructed from this capture.

The current captures also exposed three stale layout assumptions in the
existing manifest data:

- Claude 2.1.239 can paint `⏵⏵ auto mode on (shift+tab to cycle)` below
  the composer. The old optional trailer row recognized only the `⏸`
  form.
- Codex 0.149.0 emits an SGR reset before its blank separator and status
  row.
- Codex 0.149.0 carries the composer background SGR before its collapsed
  chip and may reset the chip color on the next row.

The minimized escaped fixtures preserve the measured composer and trailer
bytes. Codex fixtures use lossless hex text because their measured captures
contain meaningful trailing cells; each hash below covers the decoded bytes:

- `claude_raw_composer_2_1_239_esc.txt`, sha256
  `0410f48937a9793fa75ff15d6126780f48a3da7f41122c853d4c30240d92cf4c`
- `codex_raw_composer_0_149_0_esc.hex`, decoded sha256
  `00e3ad2daa9fcf5c0e5482fbe9232597af5eff546f72b811e2d918cc6bbd1f7f`
- `claude_collapsed_chip_2_1_239_esc.txt`, sha256
  `43d85f35dffa75a1ebc6ec38c02a18a4a4d05f2a6bb7c803429c30acae6d578d`
- `codex_collapsed_chip_0_149_0_esc.hex`, decoded sha256
  `a9cd65669abafef5c4ac149e92a06a5b5b575d61fc0ef508bf4d1070d7a44ed3`

A collapsed chip proves that hidden bytes exist, not what those bytes are.
The structural extractor reports that distinction and never reconstructs
content from a count or chip label. Antigravity and Cursor report the
separate unsupported capability. Neither result permits a terminal action.

## Manifest version evidence snapshot, 2026-08-22

Verified locally 2026-08-22. The shipped `version_tested` values still match
their authoritative full-ruleset fixtures, and an automated parity test now
keeps each claim tied to that fixture. Current installed versions are newer:

| Vendor | `version_tested` and fixture | Installed | Remaining evidence gap |
| --- | --- | --- | --- |
| Claude | 2.1.221 | 2.1.239 | Complete current idle, working, staged, modal, and quota matrix |
| Codex | 0.147.0 | 0.149.0 | Complete current matrix and live hook payload capture |
| Antigravity | 1.1.11 | 1.1.18 | Complete current matrix and exact composer recovery evidence |
| Cursor | 2026.07.23-e383d2b | unavailable | Installed current binary, complete matrix, and paired start and end hook payloads |

The `claude_raw_composer_2_1_239_esc.txt` and
`codex_raw_composer_0_149_0_esc.hex` captures prove only the extraction and
clearing behavior they measured. They do not promote the whole ruleset's
version claim. Cursor's manifest prose records one observed generation id
pair, but no raw paired hook payload fixture is checked in. Cursor therefore
keeps `turn_key_fields` empty instead of treating prose or field names as
correlation proof.

## F66. The isolated soak detected staged representations and cleared them

MEASURED 2026-08-22 on macOS with tmux 3.6a. Commit `8a93ace` produced the
reported results. The safe opt-in harness is frozen on branch
`evidence-harness-final` at `876795d`; intermediate correction `0d47f57` and
the branch tip make the live vendor run explicit and ignored by ordinary
tests. The checked result is
`validation/raw/soak/stage_and_clear_soak_report.json`.

Each installed vendor ran 100 generated payloads at 60 and 100 columns. The
corpus covered short text, Unicode, structured blank lines, code fences, long
unbroken content, and long multiline bodies. Results were:

| Vendor | Exact visible | Anchored trailer | Collapsed chip | Clean clears | Failures |
| --- | ---: | ---: | ---: | ---: | ---: |
| Codex 0.149.0 | 80 | 0 | 20 | 100 | 0 |
| Claude Code 2.1.239 | 0 | 0 | 100 | 100 | 0 |
| Antigravity 1.1.18 | 0 | 80 | 20 | 100 | 0 |

The historical JSON field `total_verified` means
`representation_detected`. It does not mean exact payload ownership, Enter
authorization, notification submission, or receipt. Collapsed-chip rows in
particular prove only the vendor representation that the harness later
cleared.

Cursor was not installed and remains `UNAVAILABLE_OFFLINE_GATE`; deterministic
fixtures do not replace the current-version live requirement. The harness
proved representation detection and clean teardown only. It deliberately did not
submit turns or claim a submitted receipt, so those guarantees come from the
separate delivery and receipt suites. The harness stays on its evidence branch
because its live vendor launches and dated artifact rewrite are opt-in
validation operations, not ordinary repository tests.

## F67. Application-wrapped doorbells cannot pass exact staging proof

MEASURED 2026-08-22 on macOS with tmux 3.6a and Claude Code 2.1.239. The
installed `14dfc91` daemon wrote its 129-character verbose doorbell to a
125-column Claude pane. Claude rendered the input as a prompt row plus a
continuation row. `tmux capture-pane -J` did not join the application-created
break. Strict exact-row verification therefore withheld Enter and opened one
`verify_failed` attention attempt. No message body reached the pane.

The earlier stage-and-clear soak did not exercise this representation. Its
Claude trials all used collapsed chips, with zero visible exact doorbells.
Reconstructing a wrapped line would be unsafe because the capture cannot
distinguish application wrapping from a newline typed by a person at the same
boundary.

The current doorbell is `cyclops inbox claim <id>`. It is 54 characters with a
full generated message id and fits beside a two-cell prompt in the validated
60-column lane. The Writing fact keeps transport `doorbell` and records
`doorbell_format: 1`. Missing format selects the legacy verbose bytes. Unknown
numeric formats replay but cannot authorize an attention recovery action.

The first installed candidate exposed a second boundary in the same live pane.
The compact row stayed on one physical line, but Claude truncated its required
status trailer. Before a turn it ended at `1000K…`; after a turn it ended at
`7d: …`. Resizing that temporary pane measured stable styled prefixes at 60,
80, 100, and 125 columns. Exact verification failed closed for every undeclared
shape. The Claude manifest now accepts the bounded model-and-effort prefix plus
its styled truncated field while retaining full-row matching, escaped-style
proof, and both required trailer rows.

The final installed-candidate check exposed the inverse layout problem on Codex
0.149.0. The active spinner was the ninth non-empty row because a queued bridge
message occupied the eight rows below it, so the bounded plain-text working rule
could not see it and status failed closed to unknown. Twenty title samples
captured the complete ten-frame Braille cycle while active; the same-version
idle title had no prefix. A separate title rule now recognizes only that exact
cycle. The screen window remains narrow, and old static titles retain the
existing screen path.

## F68. Codex 0.149.1 changed prompt styling and honors `NO_COLOR` in its trailer

MEASURED 2026-08-25 on macOS with tmux 3.6a and Codex CLI 0.149.1. Two
fresh isolated tmux servers used 187x62 panes, private `CYCLOPS_HOME` roots,
and Cyclops build `642b7d3`. One launch inherited `NO_COLOR=1`; the other
explicitly removed it. Neither probe touched the live Cyclops daemon or a
user pane.

In the colored launch, Codex painted the occupied prompt as:

```text
ESC[1m ESC[38;2;255;178;66m › ESC[0m <input>
```

The shipped rule expected the prompt glyph immediately after the bold SGR.
Cyclops therefore classified its exact staged doorbell through the plain
fallback, could not extract exact composer ownership, withheld Enter, and
raised one `verify_failed` attention attempt. Process binding, manifest
binding, and terminal-action safety all remained true. In the `NO_COLOR`
launch the prompt retained its bold SGR while the model status row carried no
SGR. The same exact doorbell again stayed staged because the trailer had no
declared unstyled proof.

The minimized captures contain compact doorbells but no message body:

| Fixture | SHA-256 |
| --- | --- |
| `src/cyclops-manifest/tests/fixtures/codex_staged_0_149_1_esc.txt` | `3892872fff4f39bafc0a79fa74d6da8d8a832e6234e9ddfce612bd5740265b50` |
| `src/cyclops-manifest/tests/fixtures/codex_staged_no_color_0_149_1.txt` | `2b3ebc0ee755ae627c4cdf41b6f2a3ab57c9101042d5ce54a58de314e23a262e` |

This evidence is narrow. It measures an occupied prompt and the two trailer
representations. The 0.149.1 ghost, slash command, working, tool, modal,
collapsed-chip, hook, restart, resumed-session, claim, and reply cases remain
unproven. It does not promote the manifest's full-ruleset version claim.

## F69. An unlinked macOS executable is still a live process

OBSERVED 2026-08-26 on macOS during a managed Cyclops update. A workspace
process kept its daemon socket open after the updater pruned the immutable pair
directory that had launched it. `LOCAL_PEERTOKEN` and
`proc_pidinfo(PROC_PIDTBSDINFO)` still identified the same execution and process
birth, but `proc_pidpath_audittoken` could no longer return an executable path.
The daemon therefore rejected the live workspace peer as if its process had
changed.

The regression launches a compiled helper from a scratch executable, captures
its socket identity, unlinks the executable while the helper remains live, and
requires the identity to remain current. Separate checks require process exit,
pid reuse, descriptor inheritance, and an in-place exec to revoke authority.
Executable path lookup is not an identity or liveness proof.

## F70. AGY 1.1.21 needs escaped prompt identity before exact doorbell submission

MEASURED 2026-08-26 on macOS with tmux 3.6a and Antigravity CLI 1.1.21.
The release-proof pane was 120 columns wide and ran the shipped AGY manifest.
The probe used `tmux capture-pane -e -p -J` against the fresh pane after
Cyclops staged one compact doorbell. A second capture from an existing AGY
conversation supplied submitted prompt echoes. The captures were read-only;
no human draft was typed or submitted for the measurement.

The active occupied composer rendered as:

```text
ESC[94m>ESC[39m cyclops inbox claim <opaque-message-key>
ESC[90m<box rule>
ESC[38;5;152mGemini 3.7 Flash<styled status fields>
```

Submitted prompts in the transcript rendered as:

```text
ESC[1mESC[34m> <submitted prompt>ESC[0m
```

Both rows reduce to `> text` after escape stripping. The old plain-only
`composer_has_input` rule could therefore treat a transcript echo as another
composer prompt. The shipped manifest now requires the active prompt's exact
escaped glyph transition and declares byte-preserving composer extraction.
Exact staging still requires the same expected doorbell bytes and the measured
styled trailer immediately below them. Unexpected text becomes extracted
content and fails equality rather than being ignored.

The initial live delivery exposed a separate missing-capability failure: the
manifest declared trailer anchors but no composer extraction patterns, so the
production submit gate returned `Unsupported` after the doorbell was visibly
staged and withheld Enter. The regression includes an earlier transcript echo,
the active 1.1.21 doorbell row, and both trailer rows, then calls the production
exact-staging proof.

This evidence is narrow. It measures the occupied and empty composer prompt,
transcript echo styling, and the two-row trailer on 1.1.21. It does not reprove
the working, modal, quota, hook, lifecycle, restart, or multiline direct-payload
rules. F71 adds current permission-modal evidence, but the full-ruleset
`version_tested` claim remains bound to 1.1.11.

## F71. AGY 1.1.21 replaces every prior state signal with a file-access modal

MEASURED 2026-08-26 on macOS with tmux 3.6a and Antigravity CLI 1.1.21.
Frozen release candidate `74ea6cc` sent one compact doorbell to pane `%40`.
The exact notification moved through queued, gating, writing, staged,
submitting, submitted, and notified with one process binding. The screen then
moved from `composer_empty` to `screen_working` and finally to `no_rule`.
Both the outcome and final detector reads returned `unknown`, no sensor
readings, and `stale: false`. The final status had held that answer for 5.8
seconds, so this was not a capture failure or a repaint between frames.

A later read-only capture of the same pane process still showed the submitted
doorbell echo and this unresolved screen:

```text
File access
Read: <external-file>
Reason: outside workspace
Allow access to this file?
> 1. Yes, allow access
  2. Yes, and always allow non-workspace access
  3. No, deny access
```

AGY removes the composer and working spinner while this decision is open. Its
pane title remains the hostname, and no hook reports the permission state. The
screen manifest is therefore the only source that can classify it. The shipped
manifest had no matching permission rule, so fusion correctly failed closed to
`unknown` but could not tell the operator what needed attention.

The fixtures `agy_file_access_permission_plain.txt` and
`agy_file_access_permission_esc.txt` preserve the measured modal structure with
the account, path, message locator, and unrelated transcript removed. The rule
requires the `File access` header, the exact question, and the selected first
choice together. It outranks a stale working spinner and never dismisses the
decision automatically. This capture proves only the 1.1.21 file-access modal;
`version_tested` remains 1.1.11 until the complete ruleset is remeasured.
