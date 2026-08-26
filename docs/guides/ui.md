# The stream and Messages UI

Watch the whole team live. `cyclops watch` turns the terminal into the
stream: every message and state change as it happens, on the record.
`cyclops ui` still works too, as a deprecated alias for `cyclops watch`.

## Basics

```
cyclops watch                # the admin stream: calm by default
cyclops watch --firehose     # start in the firehose instead
cyclops watch --plain        # line-oriented follow, no screen takeover
cyclops watch --with reviewer
```

Filtered watch uses current display labels. Discover them first with
`cyclops list --all`. An unknown active label fails immediately instead of
opening a stream that can never match. Display labels can be renamed, so
`--with`, `--from`, and `--to` are human view filters, not durable endpoint
selectors. Automation that needs the next message should use `cyclops inbox
next --timeout 30s`. Its optional `--from` accepts the canonical sender key
shown by `cyclops inbox list --json`, never a display label.
The JSON event stream accepts `--kinds`, not the three TUI display filters.

The admin stream shows only what is aimed at you: messages addressed to
admin, deliveries that parked or need attention, agents entering a
blocked state, deliveries held at the gate by a blocked pane, daemon
pings, and the line that ends any of those. Routine gate holds stay out
of it: a delivery queued behind a turn, or behind a human in copy-mode,
is the pipeline working. The firehose is one keypress away and shows
every message, delivery transition, state change, gate decision, session
event, ping, and clearance. A message to admin appears in both.

Tab opens a third view, Messages. It is a body-free work queue backed by
whole mailbox snapshots. Attention stays above ordinary inbox work, and
daemon FIFO order stays intact inside each group. A row keeps the exact
message, recipient, and notification attempt it represents, so a live
update cannot move an action to a neighboring row.

The `mailbox` column describes the durable body state. The `wake` column
describes only the one-line terminal notification. `claimed` and `staged` can
coexist when the recipient fetched the body while the exact doorbell still
awaits reconciliation. Resting rows never contain message bodies.

Messages has four scopes: Work, All, Inbox, and Outbound. Press `s` to
cycle them. Work is the daemon's answer about what needs this operator,
not a client-side guess. Enter opens the selected row in a full-width
detail. Message bodies appear only after the daemon authorizes the read.
Terminal actions show the daemon's evidence, require confirmation, and
name the exact notification attempt they will change.

The Messages footer always reports connection truth. While connecting or
refreshing after a change, actions are unavailable. After a lost
connection or failed snapshot read, the last authenticated snapshot stays
visible as stale data and `R` starts one explicit reconnect. Cyclops never
starts parallel reconnects or retries on a timer.

### Overload behavior

Keyboard input has its own bounded lane and starts a fair rotation across
input, action, snapshot, and event work. Every continuously ready lane is
served within four items. Ordered events apply backpressure when one frame
batch is already waiting, so a slow terminal cannot grow the queue without
bound. Every daemon frame and ledger line is limited to 1 MiB. Malformed or
oversized live input becomes a visible connection gap, keeps the last good
snapshot stale, and requires an explicit reconnect plus a fresh whole snapshot
before actions are enabled. Snapshot reads, durable follow pages, and action
answers use separate bounded lanes.

`cyclops status` remains the compact live-pane view. A pane can be runtime
idle while a notification is staged, so status prints a factual subrow when
that distinction matters: composer ownership, write readiness, notification
state, mailbox state, the next action, and the exact attempt id. This is live
barrier state, not a replacement for the durable Messages and alarm views.
Provisional and confirmed working states are labeled separately. Live pane
refresh has one request-wide budget; an incomplete row fails closed and keeps
its durable message facts. Blocked pre-write wakes are sampled to a fixed row
limit with the complete count printed below them.

A ping points at something rather than being it, so a ping saying a
human is needed shows here only while the thing it points at is still
counted (see [the eye](#the-eye)). The daemon also pings about
conditions nobody has to clear, like a delivery that fell back to screen
evidence, and those wait in the firehose. Your own `cyclops` pings name
nothing, so they always show.

### Every alarm gets its ending

A line reaches this view because it says a human is needed. The line that
says it is over reaches it for the same reason, so the two read together:

```
12:04:31  reviewer  ⚠ blocked_permission
12:04:32  reviewer  ✔ cleared · was ⚠ blocked_permission
```

The clearance quotes the alarm it answers, so you match the two by sight
without going to the firehose for the transition behind it. The same holds
for the closure of a delivery a daemon restart interrupted.

A pane that goes away while it was blocked reads differently, because
nobody answered the prompt:

```
12:04:31  reviewer  ⚠ blocked_permission
12:04:32  reviewer  ✔ pane closed · was ⚠ blocked_permission
```

Nothing is hidden, dimmed, or taken back to make this work. The stream is
a record: the alarm stays stamped where it happened, and the clearance is
a second line under it. The rule lives with the count
(`src/cyclops-proto/src/attention.rs`), so both halves and every
surface get it the same way.

## The agents panel

On a terminal 96 columns or wider, the stream shares the screen with a
panel listing every watched pane: who, which CLI the daemon detects
there, where they stand, and for how long.

```
agents               │
                     │   12:04:31  implementer → reviewer  Burst path fix
implementer · claude │               gateway.rs:120. Tests pass.
  ● working · 13m    │
                     │   12:04:35  reviewer  ⚠ blocked_permission
reviewer · claude    │
  ⚠ blocked_permission · 4s
```

The elapsed cell is the daemon's own clock for the pane's current state,
and it stays empty when nobody has said (a daemon older than the field).
It refreshes whenever anything redraws the frame; the stream never runs
a timer just to tick it, because the zero-polling contract outranks a
live clock.

`a` hides the panel and gives the stream the full width. Below 96
columns it is never drawn, so nothing the stream says gets clipped to
make room. `--plain` has no panel: that mode is a line-by-line follow.

## Mouse

Click an agent in the panel and tmux focus jumps to its pane, the same
jump `enter` makes from a stream entry. Click a stream entry to select
it; the wheel scrolls three rows a notch, and scrolling up unpins from
the tail exactly like `↑`. Everything the mouse does has a key, so a
terminal with no mouse reporting loses convenience and nothing else.

## Stream keys

```
tab      admin stream / firehose / messages
a        agents panel on / off (wide terminals)
w f t    filter with / from / to (enter applies, esc cancels, empty clears)
enter    jump tmux focus to the pane behind the selected entry
up down  scroll; scrolling up unpins from the tail
end      back to the tail
c        density: comfortable or compact
?        cheatsheet overlay
q        quit
```

## Messages keys

```
tab      next view
s        next scope: Work / All / Inbox / Outbound
enter    open the selected message
up down  move the selection; j and k work too
?        Messages cheatsheet
R        reconnect after connection loss
q        quit
```

Stream-only controls such as density, agent-panel visibility, filters,
and tail pinning do nothing in Messages. A selected row is tracked by its
durable target, not its screen position. If an update or scope change
removes that target, selection clears and Enter asks for a new selection
instead of opening the row that took its place.

Filters mirror the history flags: `with` is either direction, `from` and
`to` one each, and `with` replaces the other two. While pinned to the
tail, arrivals scroll into view; once you scroll up, the viewport holds
still and arrivals append below it. `enter` jumps to the entry's pane:
the sender of a message, the recipient of a delivery or gate line, the
agent of a state line. While pinned it takes the newest entry.

## The eye

The header carries cyclops's mark: `‿` closed when calm, `◑` opening
with one attention item, `◉` open with the count beside it.

The stream counts two things: an agent whose state is blocked, and a
delivery that parked on a quota or ran out of redelivery. Normal
`cyclops status` uses the same eye vocabulary and the same folded record:
its eye counts blocked panes, legacy delivery alarms, and durable mailbox
attention and held queue heads, exactly as the stream does, and its `waiting
on you` rows name the next action for each. The stream takes the mailbox half of
that record from every `messages.snapshot` its refresh gate accepts, stamped
by the same `workspace_seq` as the Messages view, so a durable alarm that
appears or clears while the stream is open moves its eye on that edge, with
no second read and no reconnect. Mailbox, alarm, and stream
surfaces provide the actions that resolve them. Both scopes are owned by
`src/cyclops-proto/src/attention.rs`.

Nothing else counts, pings included. A ping is the daemon telling you
about one of those two, so it names which one and the admin stream drops
it once that one is resolved. A restart ping covers a whole batch and
names every delivery it closed, so it stands while any of them still
does. The firehose keeps every ping either way, and a ping that names
nothing (your own, through the `admin.notify` verb) is never dropped.

An agent's item is tracked per pane, so the state a pane reports before
you name it and the state it reports after are the same item: adopting a
pane never strands an item nothing can clear. A delivery's item is
tracked per message, so only that message's own next transition clears
it: a later message to the same recipient, however it lands, never
closes an unresolved one.

### Where the count comes from

The stream count has two sources, and replayed history is not one of them:

- the daemon's `status` answer at startup, which carries every pane it
  watches and every delivery its fold still counts, taken over the whole
  record rather than a recent window;
- the live event push after that, one transition at a time.

`--backfill` therefore decides only what you SEE, never what is counted,
at any value including `0`. A delivery that parked on a quota hours ago
opens the eye whether or not its line is in the replayed tail, and a
pane that was blocked in the replayed tail but is gone now counts for
nothing: the answer lists the panes that exist, and a pane it does not
list stops counting.

That answer's pane half is read once, at startup, and nothing re-reads
it on a timer, because that would be polling; while the UI runs, a pane's
state moves on live events alone. The mailbox half is different: it rides
every `messages.snapshot` the refresh gate accepts, stamped by the same
`workspace_seq` as the Messages view, so it moves on the `messages.changed`
edge that invalidated the view, still never on a timer.

`cyclops status` asks for the pane roster and the open deliveries. Its eye
answers whether a live pane is blocked or anything durable waits on a human
(a legacy delivery alarm, a mailbox attempt needing attention, a held queue
head), and its admin-inbox suffix reports unread human mail. A closed eye
with a held mailbox queue behind it is no longer possible.

Anything the count knows about gets a line in the stream, timestamped
when it happened, so a park from this morning reads as this morning. It
runs both ways: a line already on screen that the daemon's current answer
no longer counts gets its clearance written under it at startup. Legacy
direct-delivery quota parks remain terminal. Standard mailbox notifications
have a separate explicit `cyclops requeue <message-id>` recovery path after the
daemon records that a quota reset was observed; it never retries on its own.

### Every count has a line

If a counted item has no line you can reach in the current view, the
frame says so under the header, in either view and under any filter:

```
implementer  ⊘ parked · quota. Hidden by the from filter · press f, empty the line, then enter.
```

It names the filter in play and the key that opens it, so `--from` and
`--to` readers are not sent to `w`. The input line opens holding the
current value, which is why clearing it means emptying the line before
enter.

Two things can strand a line: a filter hiding it, and eviction from the
10,000-entry stream. The copy says which, and `cyclops history` still
has the record.

The eye ticks through at most one intermediate frame per change and
never animates on its own. In plain mode it is a word line, printed
whenever what it counts changes, naming every item:

```
eye opening · 1 needs attention · implementer  ⊘ parked · quota
eye closed
```

The glyph set is documented with the rule
(`src/cyclops-proto/src/attention.rs`); its colors are the `eye.calm`
and `eye.alert` tokens of the active theme (see [themes.md](themes.md)).

## Data

Live entries come from the daemon's event push; nothing polls, and the
UI never blocks on the daemon (all IO runs on separate tasks). The
stream keeps the newest 10,000 entries and stays fluid there, with a
backlog of a thousand open items alongside it; the full record stays in
the ledger, where `cyclops history` reads it.

Startup runs in one order:

1. One `status` request, asking for the deliveries too. It names the
   sessions the daemon watches, where every pane stands, and the
   deliveries still waiting on a human.
2. Backfill replays the tail of THOSE sessions' ledgers under
   `~/.cyclops/ledger/` (default 200 lines, `--backfill N`). The reader
   retains at most 10,000 entries and 16 MiB across at most 256 files. Any
   malformed line or bound that truncates the requested history is shown as
   a stream gap. A ledger
   file from a session nobody watches is not replayed: the daemon counts
   the sessions it watches, so a line from anywhere else would be one no
   count owns and no event can ever clear.
3. The three groups apply oldest claim first: the replayed tail, then
   the `status` answer, then the live entries that queued behind them.
   The answer outranks history that is older by construction, and a live
   transition outranks the answer.
4. Applying the answer reconciles it against what the tail put on screen,
   both ways: a line for every counted item the tail is missing, and a
   clearance for every alarm the tail shows that the answer no longer
   counts.

With one watched session, replayed lines and the live stream dedupe
exactly by ledger seq; with several, a line landing in the exact startup
window can show twice on screen (the record itself never duplicates).

If the daemon does not answer, the backfill falls back to up to 256 ledger
files on disk so the screen is not empty, and nothing is counted. Any omitted
files are reported as a stream gap. A
daemon that predates the open-delivery field answers without it: the eye
then counts blocked panes plus whatever the live push reports, and
misses a delivery that parked before the UI started.

If the connection dies later, the full screen keeps what it has and the
header says `connection lost`.

## Plain mode

`--plain` or a piped stdout degrades to a line-oriented follow: backfill
first, then the startup reconciliation, then every admitted event as it
arrives, no color, no screen takeover; Ctrl-C exits. Content matches the
full UI's comfortable density, so a message prints its subject line and
the body's first line under it. The view and filter flags apply the same
way.

There is no header here to carry the eye, so the eye line does the whole
job: it prints whenever what it counts changes, and it names every item,
including ones whose own line the filter hid. There is no `--json`
here: the machine stream is `cyclops watch --json`.

## No color

A non-empty `NO_COLOR` turns the paint off and changes nothing else: the
eye, the firehose toggle, filters, scrolling, the cheatsheet and the
jump all stay. Every state renders as a glyph plus a word, so the screen
reads the same uncolored. Use `--plain` when you want the line-oriented
follow instead.

With color on, an agent's name wears its role color and a state cell or
delivery badge wears its group color, the same values `cyclops status`
and `cyclops history` use against the same theme (see
[themes.md](themes.md)).

## Exit codes

- `0` quit with `q`
- `1` the daemon was unreachable, or the connection died in plain mode
- `2` usage error (`--with` combined with `--from`/`--to`)
