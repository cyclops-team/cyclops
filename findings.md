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
