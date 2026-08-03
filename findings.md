# Build findings

Probe results where reality contradicted the brief or the research, found
while implementing v2. Same discipline as the validation campaign: every
entry is MEASURED (observed live on this machine) or READ (source/doc
inspection), with the probe that proved it. The validation campaign's
F1-F12 live in `~/projects/cyclops-arch/findings.md`; numbering here
continues from F13.

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

## F25. Cyclops has no reliable dead-pane edge on any tmux version; 3.6a wins the race by 23ms (MEASURED)

The advisory tmux-HEAD job went red on one test,
`short_lived_command_flips_pane_dead_with_remain_on_exit`. The first
reading, that next-3.8 had stopped emitting a dead-pane notification, was
wrong. Measured against a local tmux built from master, next-3.8 and 3.6a
emit byte-identical control-mode notification sequences for this scenario,
and both flip `pane_dead` at the same moment (595ms on 3.6a, 611ms on
next-3.8, for a child exiting at 500ms).

What actually happens is a race that nothing in the design arbitrates.
There is no notification for a pane's death. The watcher only learns of it
because some OTHER event forces a pane-table resync, and here that event is
tmux's automatic window rename firing when the command exits:

    3.6a       pane_dead flips 595ms   resync 618ms   sees dead=1   PASS
    next-3.8   pane_dead flips 611ms   resync 598ms   sees dead=0   FAIL

Both margins are around 20ms and neither is guaranteed. 3.6a passes by
luck. Proven by dumping every PaneEvent the watcher emits under each
binary: on 3.6a the resync yields `PaneChanged %1 changed=[WindowName,
Dead] dead=true`; on next-3.8 the same resync yields `changed=[WindowName]
dead=false`, and because no further event ever arrives, the watcher's
table holds `dead=false` permanently. Final state read back from the
watcher confirms it: `%1 dead=Some(false)` with the pane long dead.

The reason the subscription does not save us is the substantive finding.
SUB_FORMAT already carries `#{pane_dead}`, and subscriptions re-evaluate on
tmux's 1Hz tick (F23), so a 0-to-1 flip should push. It does not. Measured
on BOTH versions: subscribe to a pane while alive, let it die, watch eight
seconds and four-plus ticks, and the count of pushes for that pane after
death is zero. tmux stops re-evaluating a per-pane format subscription once
the pane's process exits. Subscribing to an already-dead pane likewise
pushes nothing, so there is no recovery path through the subscription
either.

Consequence, and it applies to the shipping version, not just to HEAD: a
pane that dies without a coincident second event stays `dead=false` in the
pane table forever. `cyclops status` will show a corpse as a live agent,
and the delivery gate's dead check, which is one of the guards standing
between a delivery and the wrong destination, silently stops guarding. The
occupant_unchanged re-check before paste and submit still catches the worse
case where a NEW process rebinds the pane id, so this degrades a layer
rather than opening the door.

Scope, measured rather than assumed: this is specific to
`remain-on-exit on`, which is not tmux's default. With the default off, an
exiting command takes its pane with it, and that emits a real
`%layout-change` (measured at 5.65s for a child exiting at 5.5s), which
resyncs the table and produces PaneRemoved normally. So the exposure is
users who turn remain-on-exit on, plus the test rig that must turn it on
to make pane_dead observable at all. That is why this is recorded and
carried rather than fixed hot.

Not fixed here. The fix has to give death its own edge rather than relying
on a coincident rename: a bounded re-read of the pane row triggered by the
pane's process exiting, or a window-level subscription that keeps ticking
after the pane's process is gone. Recorded as a risk in STATUS.md and
carried into the pane work, where the pane table is already being touched.
The tmux-HEAD job stays continue-on-error: it did its job, which was to
surface this at all.

## F26. Every shipped manifest reads the pane title, so cyclops cannot write it (MEASURED)

M4's brief asked the daemon to put `role • state` on both the pane title
and the pane border. The title is not available, for three reasons that
compound.

It is a sensor. claude.toml carries `title_working_spinner` at priority
1100 and `title_idle_sparkle` at 1000, both `region = "pane_title"`, and
they are the title tier of fusion: on a pane where a title rule matches,
screen capture never runs at all (amendment h). agy.toml and codex.toml
carry pane_title rules too. Overwriting the title with decoration would
replace the highest-priority evidence cyclops has about that pane with a
string cyclops wrote itself.

It is a fusion trigger. F13 measured that `select-pane -T` from outside
the pane pushes `%subscription-changed` exactly like an in-pane OSC 2
write, so every chrome write would wake a recompute of the pane it just
overwrote.

It does not stick. Claude rewrites its title continuously through a turn
and tmux re-evaluates subscriptions on a 1Hz tick (F23), so a title
cyclops writes is gone before a reader sees it.

The border is the surface that was actually wanted. tmux's default
`pane-border-format` is `#{?pane_active,#[reverse],}#{pane_index}#[default]
"#{pane_title}"`, so the border already DISPLAYS the title: replacing the
format replaces the view of the pane's identity without touching the value
underneath, and the title sensor keeps reading what the agent publishes.
Proven by crates/cyclopsd/tests/m4_name.rs, where the border follows the
fused state while the test drives that state by writing the pane title
from outside; both would be impossible if cyclops owned the title.

Recorded as a deviation from the brief in STATUS.md.

## F27. pane-border-format is a pane option; pane-border-status is not (MEASURED)

Probed on tmux 3.6a in an isolated server. The two options behind border
chrome scope differently, and the difference decides how much of a user's
tmux a daemon has to touch to name a pane.

`set-option -p -t %0 pane-border-format ...` is accepted and stored AT PANE
SCOPE: `show-options -p -t %0 -v pane-border-format` reads it back, a
sibling pane in the same window still resolves the inherited default, and
`set-option -p -t %0 -u` removes it. So per-pane chrome affects exactly one
pane and reverses exactly.

`set-option -p -t %0 pane-border-status top` is also accepted, and it is
NOT a pane option: `show-options -w -t @0 -v pane-border-status` reads
"top" straight afterwards, i.e. the `-p` write went to the window. There is
no pane scope for it. Cyclops therefore sets it with an explicit `-w`,
snapshots the window's prior value once (the first adoption in that window
takes the snapshot, the last un-adoption puts it back), and never touches
the server-global scope.

Two more measurements that shaped the design:

- `show-options -p -t %0 -v <opt>` prints an empty line both for "unset at
  this scope" and for "set here to the empty string". The value-less form
  (`show-options -p -t %0 <opt>`) prints nothing for the first and
  `<opt> ''` for the second, so the snapshot asks twice: once whether the
  option is set here, once what it is.
- A format string is expanded ONCE, so an option's value is substituted
  literally and never re-expanded: `@cyclops_state` holding
  `a#{pane_id},b"c` renders as those characters. That is why the chrome
  text lives in per-pane `@cyclops_role` / `@cyclops_state` options while
  the `#[fg=...]` runs live in the format cyclops owns. A label can then
  never become a tmux directive.
- `#[fg=#d19a66]` in a border format renders as SGR `38;5;173` on a
  256-color client, so tmux does the truecolor-to-256 mapping per client.
  The daemon writes one border for every client that may attach, and only
  tmux knows what each one supports, so chrome writes hex and lets tmux
  map it rather than picking the theme's own c256 fallback.
- `pane-border-status top` costs the pane one row (a 20-row pane becomes
  19), including in a single-pane window where tmux then draws a border
  that was not there. That is the visible price of border chrome and the
  reason `chrome = "off"` exists.

## F28. tmux spreads a resize evenly, not proportionally, so cell layouts drift (MEASURED)

A layout expressed as ratios has to be turned into cells at some window
size, and tmux does not keep those cells in proportion when the window
changes size. It hands the delta out EVENLY, one pane at a time.

Measured on tmux 3.6a, isolated server, two panes side by side at 100
columns split 70 | 29:

    resize-window -x 200   ->  120 | 79      (each +50; proportional is 140 | 59)
    resize-window -x 60    ->   50 |  9      (each -70)

Vertically the same, and this is the case that matters. `cyclops start`
built the `ops` preset into a detached session, which tmux sizes with
`default-size`, 80x24. The dock landed at 7 of 23 usable rows, 30.4%, as
designed. Attaching from a 200x50 terminal grew both rows by 13, so the
dock became 20 of 49 rows: 40.8% of the screen for a dock designed at 30%,
and the three agents lost a third of their height between them.

Consequence: a preset only looks like its design if it is built at the
size it will be looked at. `cyclops start` and `cyclops workspace restore`
therefore size the new session to the terminal they were run from
(`cyclops_ui::terminal_size`, the ioctl the stream already uses), and fall
back to letting tmux choose only when stdout is not a terminal.

Not fixed, and it cannot be fixed from here: nothing re-applies the ratios
when a client attaches later at a different size, because there is no
event to hang that on without polling. Building at the operator's own size
covers the case that happens, which is a person running `cyclops start` in
the terminal they are about to work in. Building one inside a small pane,
or in a script, and attaching elsewhere still drifts. Recorded in
docs/workspaces.md under "Sizes and resizing" rather than hidden.

## F29. The daemon's JSON keys come out alphabetically, so scripts must not match on key order (MEASURED)

`cyclops --json status` prints one compact line whose object keys are in
ALPHABETICAL order, not the order the structs declare:

    {"boot_id":"...","daemon_version":"0.1.0","proto":1,
     "sessions":[{"attached":true,"name":"demo","panes":[...]}],
     "tmux_version":"3.6a","uptime_ms":2017}

`SessionStatus` declares `name` then `attached`; the wire says `attached`
then `name`. The daemon answers through `serde_json::Value`, whose object
is a `BTreeMap` unless the `preserve_order` feature is on, and it is not.

This is not cosmetic. `demos/m4-workspace.sh` waited for the daemon to
attach with

    grep -q "\"name\":\"$SESSION\",\"attached\":true"

which matches nothing the daemon can ever send. The loop ran out its 40
iterations every time and the script continued anyway, so the demo passed
on a ten-second sleep and would have failed on a slower machine for a
reason nobody would have looked for. A wait that cannot succeed is worse
than no wait: it looks like a wait in the diff.

Two consequences, both applied:

- A shell consumer matches ONE field (`"attached":true`) or uses jq. A
  pattern spanning two keys is a pattern about the alphabet.
- A wait keyed on `attached` alone is still not enough right after a
  session is rebuilt, because the daemon can still be reporting the
  session that just died. `demos/m4-workspace.sh` waits on the new pane
  id instead.

## F30. Killing a session kills a one-session tmux server, and the next one re-issues %0 (MEASURED)

Probed on tmux 3.6a, isolated server, one session `demo` with two panes:

    kill-session -t =demo   ->  has-session: "no server running on <path>"
    new-session -d -s demo  ->  panes %0 %1 again, server pid 60557 -> 60571

With a second session on the same server keeping it alive, the same
kill-and-rebuild hands out `%2` instead: the id counter is the server's
and dies with it.

So pane id reuse is not an exotic case reachable only by a crash. It is
what happens when a person kills the session they were working in and
builds it again, which is the ordinary `cyclops workspace restore` path.
Nothing may carry a name across on a pane id alone. The adoption registry
already refuses to (an entry is restored only when the pane exists AND its
root pid matches the one recorded at adoption); this is the measurement
showing the case it defends against is the common one, not the rare one.
`demos/m4-workspace.sh` runs exactly this: the rebuilt session comes back
as `%0` and `%1`, and the names go back on from the workspace file rather
than from the ids.

## F31. Three of the four shipped preset labels get the same role color (MEASURED)

Role color is one of the two encodings GOALS says carry meaning, and
`cyclops_theme::role_slot` picks it by FNV-hashing the label into eight
slots. Run over the labels Cyclops itself ships in `layouts/`:

    implementer -> slot 1
    reviewer    -> slot 2
    tests       -> slot 2
    docs        -> slot 2
    admin       -> slot 4

So the `quad` preset, the arrangement with the most agents in it, paints
three of its four agents in one color. Seen live in `demos/m4-name.sh`,
where `reviewer` and `tests` both render `#[fg=#96aac3]` on their borders
and in `cyclops list`.

Nothing is lost, because color is never alone: the name is spelled out in
the same cell on every surface, so a `--plain` or NO_COLOR reader and a
color reader get the same information. What is lost is the encoding's
value. Role color exists so a person can tell panes apart at a glance, and
across the shipped ladder it mostly cannot.

Not fixed here, because every fix is a design decision rather than a
correction:

- Hashing differently just moves the collision to a different set of
  labels; eight slots and a hash will always collide somewhere.
- Assigning slots in adoption order would make them distinct, but the
  color then depends on the order panes were named, and it has to be
  recorded (the registry is durable now, so it could be) or it changes on
  every daemon restart.
- Assigning over the whole registry at render time keeps them distinct
  without new state, but an agent's color would then change when a
  different agent is named.

M4 raises the stakes rather than causing this: it ships the four labels
that collide, and it puts role color onto tmux borders, so the same
collision is now visible on the border, in `list`, in `status` and in the
stream at once.
