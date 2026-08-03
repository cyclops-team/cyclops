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

## F13. refresh-client -B subscriptions work in control mode on tmux 3.6a (MEASURED)

Subscribing `name:%pane:#{pane_title}\t#{pane_dead}\t#{pane_in_mode}\t#{pane_current_command}`
produces %subscription-changed for select-pane -T from outside, OSC 2 printf
from inside the pane, and copy-mode entry/exit. This makes subscriptions the
watcher's primary per-pane change signal (zero polling holds). Caveat: the
initial value push after subscribing is lazy, so bootstrap must come from
list-panes, never from the subscription's first event. Proven by
crates/cyclops-tmux/tests/subscription_probe.rs, which documents the fallback
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
as fixtures in crates/cyclops-manifest/tests/fixtures/. The esc rules fail
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
crates/cyclops-tmux/tests/control_load.rs, zero Disconnected over the 60 s
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
state across a tick. Proven by crates/cyclopsd/tests/m2_wait.rs, which
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
(`manifests/claude.toml`, `manifests/agy.toml`) carry rules over
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

What it constrains: `crates/cyclopsd/src/chrome.rs` writes
`@cyclops_role`, `@cyclops_state`, `pane-border-format` and
`pane-border-status`, and no pane title; `cyclops_tmux::layout` writes none
either. The border already displays `#{pane_title}` by default, so
replacing the border FORMAT replaces the view without touching the value
underneath. Proven by `crates/cyclopsd/tests/m4_name.rs`, which drives the
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
(`crates/cyclopsd/src/registry.rs`); it is also why a pane moving windows
needs `move_chrome` to hand the setting back to the window it left. (3) is
why every ratio in `cyclops_tmux::layout` is a share of the cells the PANES
hold and never of the window: a grid checked against the window stopped
adding up the moment the daemon named a pane, and `cyclops workspace save`
refused a session `cyclops start` had built seconds earlier
(`crates/cyclops-tmux/tests/layout_round_trip.rs::a_window_wearing_border_chrome_still_reads_as_a_grid`).
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
(`crates/cyclops/src/workspace.rs::build_size`), which is the only size
that keeps a preset looking like its design. One built with no terminal to
ask still drifts, and docs/workspaces.md says so rather than hiding it.
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
alphabet. Written down where a script writer meets it, in docs/PROTOCOL.md
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
shipped theme (`themes/light.toml`, 4410 bytes, all 22 tokens) while a
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

What it constrains: `ThemeWatch::adopt` in `crates/cyclops-theme/src/select.rs`
refuses a reload that no longer sets a token the theme on screen sets,
rather than refusing only what fails to parse. A misspelled token name
fails the same way and stays failed until it is fixed, which is the same
rule doing the same job. A theme SWITCH is exempt: that palette was asked
for, so it applies with a fresh start's tolerance. `theme.reload` on the
daemon is on the near side of that rule, which is why a half-written file
can reach a real pane border and why
`crates/cyclopsd/tests/m5_theme.rs::a_half_written_theme_leaves_the_borders_alone`
reads the border back off tmux rather than off the daemon's own belief.

Not a suite test, and deliberately: it measures a race and reports a
distribution, so asserting on it would flake. The procedure above is the
whole probe; it is 60 lines of Python and reproduces in under a minute.
