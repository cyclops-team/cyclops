# How a message gets from one pane to another

Cyclops v2 is a tmux-backed coordination daemon for terminal coding agents.
This page follows one message end to end and names, at every fork, the file
that decides. Read it once and you can put your finger on where any
behavior comes from.

The shape is frozen by ADR-001 and by the validation campaign's amendments.
Neither document is in this repository. Both live in the separate
`cyclops-arch` design repo, which this clone does not carry, and you do not
need either to read this page: every frozen decision and every lettered
amendment is tabled at the end of it against the file that implements it,
and the measurements they rest on are [findings.md](../../findings.md).
Everything described here is in the tree and under test.

## The shape of it

```mermaid
flowchart LR
    A["agent CLI<br/>in a tmux pane"] -->|"cyclops send, over the socket"| C["cyclops<br/>(thin NDJSON client)"]
    C -->|"one JSON object per line"| D["cyclopsd"]
    D -->|"who connected, walked up<br/>to a watched pane"| I["sender identity"]
    D -->|"every fact, before it is acted on"| L[("ledger<br/>one NDJSON file<br/>per session")]
    D -->|"load-buffer, paste-buffer, send-keys<br/>on the control connection"| T(["tmux server"])
    T -->|"the payload, as if pasted"| B["recipient's pane"]
    B -->|"the vendor hook runs cyclops hook,<br/>which posts back through the same socket"| D
    D -->|"one receipt per recipient"| C
    D -->|"and an event line to every subscriber"| S(["cyclops watch"])
```

Four rules hold the whole design up.

- **tmux owns the terminal.** Cyclops never hosts a pty. It asks tmux and
  tmux answers, over the interface tmux calls control mode.
- **The ledger is the record.** Append-only NDJSON, one file per watched
  session, written before anything downstream believes it.
- **Nothing polls.** Every recompute rides an event that already happened.
- **Every tmux specific lives in `src/cyclops-tmux`.** Control mode is
  unversioned and moves between releases, so one crate absorbs that.

The sender is never in the request. The daemon resolves it from the peer
credentials on the socket and walks that pid up to a watched pane, so
nothing in a body can forge the header a recipient reads.

## Watching: how the daemon knows what a pane is doing

```mermaid
flowchart TD
    X(["tmux server"]) -->|"%subscription-changed on cyp&lt;n&gt;:<br/>title, dead, in_mode, command, pid"| W["control client<br/>one tmux -C per watched session"]
    X -->|"%subscription-changed on cypdead:%*,<br/>the only edge a death can arrive on (F25)"| W
    X -->|"%output / %extended-output"| W
    X -->|"window add, close, rename, layout change,<br/>pane moved, pane mode changed"| W
    W -->|"a subscribed field moved"| RW["write the row"]
    W -->|"structural hint, or output from<br/>a pane the table does not know"| RC["reconcile, debounced 30ms"]
    W -->|"activity on a known pane"| EV(["OutputActivity event"])
    RC -->|"list-panes: the authoritative answer"| P["pane table, keyed by pane_id"]
    RW --> P
    P -->|"a row moved, a hook edge arrived,<br/>or a caller asked"| F["fusion"]
    F -->|"the verdict moved"| O(["state event · ledger line · border repaint"])
    F -->|"a caller asked"| Q(["status · pane.read · agent.wait · the gate"])
```

Notifications are hints, not truth. Truth comes from `list-panes`, and the
daemon re-asks whenever a hint arrives or a doubt exists, so a missed event
costs freshness and never correctness (ADR revision 1, level-triggered
core). `src/cyclops-tmux/src/watcher.rs` owns this loop.

### Fusion: which sensor decides

`src/cyclopsd/src/fusion.rs`, `recompute_pane`. Manifest rules are sorted by
priority once at load; every tier below picks the first rule that matches,
and the fused verdict is whichever tier's winner sits earlier in that one
order.

```mermaid
flowchart TD
    rc(["recompute"]) --> dead{"pane dead?"}
    dead -->|yes| isdead["verdict: dead"]
    dead -->|no| mode{"pane in copy-mode?"}
    mode -->|"yes: not an agent state,<br/>status exposes in_mode per row"| prior["verdict: the prior one, kept"]
    mode -->|no| bound{"manifest bound?<br/>1. the explicit pin from cyclops name --manifest<br/>2. the pane's kernel comm name<br/>3. the launched argv basename (F21)"}
    bound -->|"none of the three"| unknown["verdict: unknown"]
    bound -->|yes| title["title tier:<br/>first pane_title rule that matches"]
    title -->|"a rule matched and nobody forced a capture"| fuse
    title -->|"no rule matched, or the caller forced<br/>the full set (the gate, pane.read detection)"| screen["screen tier: capture-pane,<br/>plus the -e capture when the<br/>manifest carries esc rules (F19)"]
    screen -->|"the capture failed"| doubt["keep the prior verdict;<br/>with no prior, decided_by is sensor_error"]
    screen -->|"captured"| fuse["fuse: whichever tier's winning rule sits<br/>earlier in priority order decides"]
    fuse -->|"one tier fired, or both agree"| verdict(["verdict, decided_by names what won"])
    fuse -->|"both tiers fired and disagree"| flag["disagreement exposed;<br/>the higher-priority rule still wins"]
    fuse -->|"the rules tier came back unknown<br/>and a live hook reading exists"| hook["the hook reading decides,<br/>decided_by is hook:&lt;event&gt;"]
    fuse -->|"a rule decided and the hook reading differs"| flag
    flag --> verdict
    hook --> verdict
```

Screen capture is evidence of last resort (amendment h): a pane_title rule
that decides alone skips `capture-pane` entirely. A sensor that fails is
doubt, not evidence, which is why a failed capture keeps the prior verdict
instead of flipping the pane.

A hook reading counts as live for 300s, and is dropped after three
consecutive recomputes where the rules tier decided against it, so a stale
edge cannot pin fused state. Blocked states always come from rules: no
tested CLI hooks its modals or its quota.

## Sending: how a message becomes a verified receipt

Two halves, joined by one queue. `msg.send` writes the fact and fans out;
one FIFO worker per recipient pane then carries each chain on its own.
Both are `src/cyclopsd/src/delivery.rs`; the semantics are docs/development/DELIVERY.md.

### The call: what the sender gets back

```mermaid
flowchart TD
    s1{"1. any session watched?"} -->|no| e1["error: no_such_target"]
    s1 -->|yes| s2{"2. expand the to-list:<br/>* is every labeled pane"}
    s2 -->|"no names left after dedupe"| e2["error: bad_request"]
    s2 -->|"one or more names"| s3["3. resolve each name to a pane,<br/>canonicalize it to its ledger name"]
    s3 -->|"every name resolved or marked unresolvable"| s4["4. one msg line into each involved session<br/>file, N delivery chains under it"]
    s4 -->|"the msg fact is on disk"| s5{"5. per recipient:<br/>did the name resolve to a pane?"}
    s5 -->|no| gone["attention_required, cause no_such_pane,<br/>admin pinged once"]
    s5 -->|yes| s6{"6. is that pane's worker parked?"}
    s6 -->|"yes: blocked_quota never auto-retries"| parked["parked_blocked_quota,<br/>note carries the reset hint"]
    s6 -->|no| s7["7. push the chain onto the queue for<br/>that worker and wake it"]
    s7 -->|"the chain is queued"| s8{"8. was the worker free at send:<br/>not busy, queue empty,<br/>fused state idle?"}
    s8 -->|"no: busy, or chains ahead of it"| moving
    s8 -->|"yes: the idle path"| s9["9. block on this chain until it reaches<br/>delivered_verified, delivered_unverified,<br/>attention_required or parked_blocked_quota"]
    s9 -->|"it reached one of those four"| settled
    s9 -->|"receipt_block_ms elapsed first"| moving
    gone -->|"resolved at send"| settled
    parked -->|"resolved at send"| settled
    settled["10. receipt: that state, plus the note<br/>or cause the chain recorded"] -->|"one per recipient"| out
    moving["10. receipt: queued, with the number of<br/>chains ahead of it on that worker"] -->|"one per recipient"| out
    out(["msg.send result: msg_id, seq, receipts"])
```

Steps 5 to 9 run per recipient, so one broadcast returns mixed receipts:
delivered_verified for the idle pane, queued with a position for the busy
one, parked for the one out of quota. `receipt_block_ms` (default 2500) is
one deadline for the whole receipt phase, not one per recipient, so a
broadcast with several idle recipients shares the cap.

A queued receipt is honest, not a failure. The chain keeps moving after the
call returns and every transition still lands in the ledger; the sender is
told where it stood when the daemon answered, which is the only thing the
daemon knew.

`send --wait` composes `agent.wait` onto the same call after step 10: it
appends a `wait` array and never changes the receipts.

### The chain: gate, paste, submit, prove

```mermaid
flowchart TD
    q(["the worker takes the chain"]) --> g["gate<br/>(next section)"]
    g -->|"admits, and hands back the pane_pid it admitted"| r1{"occupant check: still that pid,<br/>alive, same manifest?"}
    r1 -->|"no: a shell occupant would EXECUTE the text"| rb["retry_queued, cause pane_rebound"]
    r1 -->|yes| paste["load-buffer into cyc-&lt;daemon pid&gt;-&lt;seq&gt;,<br/>then paste-buffer -p -d (amendment e)"]
    paste --> ver{"did the paste stage?<br/>re-read the screen on a ladder of delays"}
    ver -->|"no capture proved it"| vf["retry_queued, cause verify_failed"]
    ver -->|"the message id is in the verify region,<br/>or a generic marker sits on<br/>a manifest composer line"| staged["staged, and registered<br/>for hook ACK matching"]
    staged --> r2{"occupant check, second time"}
    r2 -->|no| rb
    r2 -->|yes| sub["send the manifest's submit key"]
    sub --> t1{"does this CLI declare an ack hook<br/>with a payload field?"}
    t1 -->|"yes: tier 1, for ack_timeout_ms"| hookw["wait for an agent.state.report<br/>whose payload carries this message id"]
    hookw -->|"it arrived"| dv(["delivered_verified · verified_by hook"])
    hookw -->|"the window closed. On a pane no hook edge<br/>has EVER reached, that is the F1 signature<br/>and the admin hears it once"| t2
    t1 -->|"no: screen evidence is the best available"| t2{"tier 2, at fixed checkpoints:<br/>marker gone from the composer<br/>AND turn evidence?"}
    t2 -->|"working state, output activity,<br/>or a changed composer with the id staged"| du(["delivered_unverified · verified_by screen"])
    t2 -->|"nobody could look: session detached,<br/>or the capture failed"| fz["freeze the clock; a reattach or<br/>pane activity looks again and resumes it"]
    fz --> t2
    t2 -->|"looked, saw nothing, deadline spent"| ao["retry_queued, cause ack_timeout"]
    du -->|"a late hook ACK matches"| dv
    rb --> retry{"attempts left?"}
    vf --> retry
    ao --> retry
    retry -->|yes| g
    retry -->|no| att(["attention_required, admin pinged"])
```

Three things in that diagram are the whole reason it is longer than "paste
and press Enter".

The occupant check runs twice, before the paste and again before the submit
key, against the `pane_pid` the gate admitted. A pane whose occupant changed
in between (the agent exited to a shell, another CLI took over) must never
receive the payload or the Enter, because a shell would run it.

Freezing beats expiring. While the control connection is down the daemon
cannot see the pane, so a deadline that ran anyway would fail a delivery
that in fact landed. Every remaining instant shifts by the outage, and the
reattach re-reads the screen before any deadline may fire.

Tier 1 never stops being possible. The registration made at `staged` stays
live until the chain resolves, so a hook ACK that arrives after the window
closed still verifies, whether the chain is still waiting or already
settled on screen evidence.

### The gate

The gate is the guard between a delivery and the wrong pane. It decides in
this order and only in this order (`delivery.rs`, `gate`):

```mermaid
flowchart TD
    s1{"1. session attached?"} -->|"no: session_detached"| hold
    s1 -->|yes| s2{"2. pane still in the table?"}
    s2 -->|no| gone["attention_required: no_such_pane"]
    s2 -->|yes| s3{"3. pane dead?"}
    s3 -->|yes| deadout["attention_required: pane_dead"]
    s3 -->|no| s4{"4. pane in copy-mode?"}
    s4 -->|"yes: pane_in_mode"| hold
    s4 -->|no| s5{"5. manifest bound?"}
    s5 -->|no| nomanifest["attention_required: no_manifest"]
    s5 -->|yes| s6["6. recompute fused state,<br/>screen sensor forced"]
    s6 --> s7{"7. fused state"}
    s7 -->|idle| ok["proceed: paste"]
    s7 -->|dead| deadout
    s7 -->|blocked_quota| park["park: quota never auto-retries"]
    s7 -->|"modal or permission, the rule auto-dismisses<br/>and declines remain"| decline["send that rule's decline keys,<br/>then re-read the screen"]
    s7 -->|"modal or permission, otherwise:<br/>hold on the rule id, admin pinged once"| hold
    s7 -->|working| hold
    s7 -->|"idle_with_input: human typing wins"| hold
    s7 -->|unknown| hold
    decline --> s1
    hold["8. hold: wait for a pane or state event,<br/>never a timer"] --> s1
```

Steps 1 to 5 read the pane table and the manifest binding; step 6 is the
only one that touches the screen, and it runs immediately before pasting so
the snapshot is fresher than any human keystroke round trip.

Holds wake on events, never on a clock. Two timers live in this loop and
neither of them polls: a one-shot settle after a decline, so the dismissal
renders before the screen is re-read, and a one-shot admin ping after
`gate_hold_notify_ms`, which reports a wedged hold without ending it.

### The record of it

Every transition above is a ledger line. The table of legal moves is
`src/cyclops-proto/src/ledger.rs`, `DeliveryState::can_transition_to`:

```mermaid
stateDiagram-v2
    [*] --> queued: msg.send recorded
    queued --> gating: the recipient's worker takes it
    gating --> pasting: gate admits, fused idle
    pasting --> staged: composer holds the message id
    staged --> submitted: Enter sent
    submitted --> delivered_verified: hook ACK matches
    submitted --> delivered_unverified: screen evidence, no ack tier
    delivered_unverified --> delivered_verified: a late hook ACK matches
    delivered_verified --> [*]

    gating --> retry_queued: pane rebound after admit
    pasting --> retry_queued: step failed, attempts left
    staged --> retry_queued: step failed, attempts left
    submitted --> retry_queued: step failed, attempts left
    retry_queued --> gating: retry, back through the gate

    queued --> attention_required: no pane for that name
    gating --> attention_required: pane dead, gone, or no manifest
    pasting --> attention_required: attempts spent
    staged --> attention_required: attempts spent
    submitted --> attention_required: attempts spent
    retry_queued --> attention_required: attempts spent

    gating --> parked_blocked_quota: fused blocked_quota
    queued --> parked_blocked_quota: recipient parked, queue drained
    retry_queued --> parked_blocked_quota: legal, unreachable today

    attention_required --> queued: re-queue, no verb ships yet
    parked_blocked_quota --> queued: re-queue, no verb ships yet
```

Three edges are legal and driven by nothing today. The two re-queues have
no verb: an operator resends the message instead, which starts a new chain.
The third cannot happen at all, and the reason is worth knowing, because
its absence is what keeps a park from creating limbo: a chain in
`retry_queued` is being carried inside the worker's own retry loop and is
therefore not in the queue that a park drains.

One transition is missing from the diagram because it has no single source.
A daemon restart closes every chain still in flight to `attention_required`
with cause `daemon_restart`, whatever state it was in (`delivery.rs`,
`close_limbo`). Limbo is a bug, so a restart never leaves a chain open.

## What needs a human, and who owns it

The eye is the signature device, and it appears on three surfaces: the
stream header, the `--plain` eye line, and `cyclops status`. All three read
one register and none of them recomputes it. That register is
`src/cyclops-proto/src/attention.rs`, and it is the only file allowed to answer
this question.

```mermaid
flowchart TD
    S["the daemon's status answer:<br/>the pane roster as it is now, plus<br/>open_deliveries folded from the whole record"] -->|"REPLACES both halves wholesale,<br/>once, at startup"| R
    E["live events: state, delivery-state, pane-removed"] -->|"one item at a time; each one IS<br/>that item's next transition"| R
    H["a replayed ledger tail"] -->|"nothing. A window over the record cannot<br/>answer 'right now', and letting it try means<br/>--backfill decides the count"| R
    R(["the register"]) --> A{"AGENT half: the pane's fused state"}
    R --> D{"DELIVERY half: the chain's latest state"}
    A -->|"blocked_modal, blocked_permission or<br/>blocked_quota: nothing downstream clears them"| CT["counted"]
    A -->|"anything else"| NC["not counted"]
    D -->|"attention_required or parked_blocked_quota:<br/>the pipeline cannot move it on its own"| CT
    D -->|"in flight, or delivered"| NC
    CT -->|"the count is what the eye answers to"| EYE(["the eye opens, and every number<br/>it shows has a line the reader can reach"])
    NC --> EYE
    CT -->|"the item's next transition leaves that set"| RES(["a clearance: the surface that showed<br/>the alarm is the one that shows it end"])
    CT -->|"the pane leaves the tmux table"| RES
```

Two failures shaped that picture. Deriving the count from the displayed
stream made `--backfill 200` and `--backfill 400` disagree about whether
anything was wrong. And a blocked pane that later closed stayed counted
forever, because the snapshot is taken once and re-taking it on a timer
would be polling: `pane-removed` is that pane's last transition and the
only thing that can drop it.

The four states a receipt waits for are not this rule's two. A receipt is
settled once the pipeline will not move the chain on its own, which
includes both delivered states. Whether one still needs a human is this
separate question with this separate owner.

`src/cyclops-proto/tests/one_place.rs` is a tripwire against a second
implementation. Read its header before trusting a green run: it catches
the copies people write without meaning to, and it says plainly that it
cannot catch a determined one.

## Workspaces: how a session comes back

A name lives in exactly two places. The daemon's registry holds it while
the pane is alive; a workspace file holds it when the panes are gone.
`cyclops start` and `cyclops workspace restore` carry names from the file
to the registry, `cyclops workspace save` reads them back out of `status`,
and `pane.label` is the one call where the two paths meet.

```mermaid
flowchart TD
    st(["cyclops start"]) --> l{"1. a saved workspace file for this name?"}
    l -->|yes| lay["the layout is that file"]
    l -->|"no: take a preset, and write the file<br/>once a session has been built from it"| lay
    lay --> ex{"2. does the session exist?"}
    ex -->|no| build["build it through cyclops-tmux layout:<br/>new-session, split-window, resize-pane,<br/>at the size of this terminal (F28)"]
    ex -->|"yes: left exactly as it is,<br/>so start is safe to run twice"| cfg
    build --> cfg["3. make sure config.toml names the session,<br/>so the daemon will watch it"]
    cfg --> ask["4. ask the daemon: watching it, connecting,<br/>watching something else, or down?"]
    ask --> gate{"5. may this workspace name these panes?"}
    gate -->|"the session was just built from this layout"| yes["allowed"]
    gate -->|"a preset meeting a session it never built"| no
    gate -->|"the live grid no longer has this shape,<br/>window for window and row for row"| no
    gate -->|"a name the daemon holds sits on a pane<br/>the file does not put it on"| no
    gate -->|"the shape matches and no name is misplaced"| yes
    no["refused. Nothing is renamed, and the<br/>copy says what would fix it"] --> say
    yes --> adopt
    rs(["cyclops workspace restore"]) -->|"apply refuses a session that already exists, so<br/>this always builds a fresh one and the gates<br/>above have nothing left to protect"| build2["build it, the same way"]
    build2 --> ask2["ask the daemon the same question"]
    ask2 --> adopt{"6. is the daemon watching this session?"}
    adopt -->|"yes: pane.label per named pane,<br/>in the layout's position order"| say
    adopt -->|"no: a session built a second ago is normally<br/>still connecting, so say which of the four<br/>daemon states this was"| say
    say(["7. what is ready, and what is left to do"])
```

The gates exist because a workspace file holds no pane ids and cannot: the
panes it describes are usually gone by the time it is read, and tmux hands
a freed id to the next pane it makes. All the file has to point at a pane
with is WHERE the pane sits, and that points at something only while the
grid still matches and every name already on a pane is where the file puts
it. Pane count is not that check: `ops` and `quad` both hold four panes,
and mapping either onto the other renames every agent onto a pane it does
not own. Never type into the wrong pane (GOALS).

Restore rebuilds panes, sizes, directories and labels. It starts no
process unless you pass `--launch`, and even then tmux runs the command as
the pane's own; no keys are sent anywhere. Details:
`docs/guides/workspaces.md`.

## Writing into tmux

M4 is the first milestone that writes INTO tmux, and it writes on two paths
that never touch each other. Keeping them apart is the whole design: one is
the daemon's, on a control-mode connection it already owns, and one is a
client's, on one-shot invocations against a server no daemon need be
watching.

```mermaid
flowchart TD
    N["cyclops name %4 reviewer"] --> D["cyclopsd: pane.label"]
    D -->|"the roster, so a restart keeps it"| R["registry.json<br/>whole-file temp+rename, 0600"]
    D -->|"how it got here"| L["ledger: system line, pane_labeled"]
    D -->|"written on eight edges and no others: adoption,<br/>a fused state change, a clear, a session attach,<br/>a window move, a pane close, daemon shutdown,<br/>and a theme switch"| C["chrome.rs, over the control-mode client:<br/>@cyclops_role, @cyclops_state,<br/>pane-border-format (this pane),<br/>pane-border-status (this window)"]
    W["cyclops start · workspace save and restore"] -->|"labels, and only when a daemon answers"| D
    W -->|"structure"| T["cyclops-tmux layout, one-shot invocations:<br/>new-session, split-window, resize-pane"]
    C --> X(["tmux"])
    T --> X
```

Each of those eight edges is fired by one function, and no function fires
two:

| Edge | Fired by |
|---|---|
| adoption | `adopt_pane` |
| a fused state change | `fusion::recompute_pane` |
| a clear | `unadopt_pane` |
| a session attach | `reconcile_adoptions` |
| a window move | `move_chrome` |
| a pane close | `handle_pane_event` |
| daemon shutdown | `restore_all_chrome` |
| a theme switch | `reload_theme` |

The four that paint a set of panes (adoption, session attach, window move,
theme switch) all paint through one function, `paint_adoptions`.
`src/cyclopsd/src/chrome.rs` holds the same table and a test reads this
page against it, so a ninth caller cannot leave these three pages
describing the old set.

Chrome sets options and takes them back. The pane's prior
`pane-border-format` and the window's prior `pane-border-status` are
snapshotted at adoption and put back on `--clear`, on pane close, on a
window move (the window the pane left), and at shutdown. F27 says why the
window scope is unavoidable, F26 why the pane title is never written. The
`chrome = "off"` switch lives in `chrome.rs` and nowhere else, and the
snapshot is taken even when it is off, so a restore never unsets an option
cyclops did not read.

The layout path sets no option, writes no pane title, and sends no keys.
Nothing it does needs undoing, and it cannot collide with the chrome the
daemon owns.

## Where each decision lives

One line per crate. Each crate's own `//!` header states what it owns and
what it deliberately does not; read that before changing one.

| Crate | Owns |
|---|---|
| `cyclops-proto` | Wire types, ledger schema, the delivery state machine, the attention rule. Data only, no IO. Everything compiles against it (docs/reference/PROTOCOL.md). |
| `cyclops-manifest` | Per-CLI detection rules as TOML data: regions, priorities, modal decline keys, injection contract (docs/reference/MANIFESTS.md). |
| `cyclops-tmux` | The tmux adapter and the blast wall: nothing outside it speaks to tmux. Control mode, the reconciling pane table, version parsing, one-shot focus and layout. |
| `cyclopsd` | The daemon: watcher, fusion, delivery pipeline, ledger writer, socket server, adoption registry, pane chrome. |
| `cyclops` | The CLI: a thin NDJSON client plus the human-facing renderers, `cyclops hook`, and the workspace verbs. |
| `cyclops-ui` | The stream behind `cyclops watch`: admin view, firehose, the eye, jump-to-pane, windowed rendering over a 10k ring (docs/guides/ui.md). |
| `cyclops-workspace` | The full-screen workspace behind bare `cyclops`: Ratatui/Crossterm chrome, embedded pane VT runtimes, direct manipulation, dialogs, and persistence. |
| `cyclops-ledger` | Crash-safe append-only NDJSON writer and cursor reader. Fsync before acknowledging; torn final lines are sealed, never rewritten. |
| `cyclops-theme` | The semantic token vocabulary, theme files, 256-color fallback, selection and hot reload (docs/guides/themes.md). |
| `cyclops-testrig` | Test-only. The isolated tmux server and the one statement of its teardown rule. |

Non-crate directories: `resources/manifests/` (shipped detection data for Claude,
Codex, Antigravity, and Cursor), `resources/hooks/` (vendor hook config templates `cyclops hooks install`
renders), `resources/layouts/` (the four workspace presets, compiled in with
`include_str!` so a fresh install has them before it has a config file),
`resources/themes/` (the seven shipped token files), `demos/` (runnable end-to-end
scripts; `tests/e2e/lib/lib.sh` holds the two rules the scripts must not copy, the
scratch root and the tmux teardown), `tests/e2e/lib/` (the Python probe
harness), `scripts/commpact-shim/` (the prepared v1 shim and its guarded
installer; nothing installs it, see docs/development/CUTOVER.md), `website/` (the
landing page for usecyclops.dev, outside the Cargo workspace and checked by
its own CI job).

### The frozen decisions

| ADR-001 decision | Lives at |
|---|---|
| Single daemon, one `tmux -C` client per session (T3) | `src/cyclops-tmux/src/control.rs`, owned by `cyclopsd` |
| Level-triggered reconciling core, not an event mirror (revision 1, C2) | `src/cyclops-tmux/src/watcher.rs` |
| Sensor fusion with per-sensor readings and observable disagreement (revision 2) | Types in `src/cyclops-proto/src/state.rs`; engine in `src/cyclopsd/src/fusion.rs`; hook sensor fed from `src/cyclopsd/src/ack.rs` |
| Detection rules are per-CLI data, not code (H2) | `cyclops-manifest`, `resources/manifests/{claude,codex,agy,cursor}.toml` |
| NDJSON Unix socket, hello line first, version mismatch warns never rejects (S2) | `src/cyclops-proto/src/wire.rs`; server in `src/cyclopsd/src/server.rs` |
| Append-only NDJSON ledger, monotonic seq plus boot_id, replayable by cursor (C6) | Schema in `src/cyclops-proto/src/ledger.rs`; writer in `cyclops-ledger`. The stream client backfills by reading session files directly; server-side cursor replay on `events.subscribe` is accepted and ignored, with no client that needs it |
| Delivery pipeline: queue, gate, paste, verify, submit, ACK; failures are queued states | `src/cyclops-proto/src/ledger.rs` for the machine, `src/cyclopsd/src/delivery.rs` for the pipeline |
| Turn detection from hooks via a `cyclops hook` receiver | `wire.rs` (`agent.state.report`), `src/cyclops/src/hook.rs`, `src/cyclopsd/src/ack.rs` |
| Agent surface: thin CLI speaking NDJSON to the socket | `src/cyclops` |
| v1 keepers: fail-closed ACL, data-only config, explicit pane adoption, identity from socket peer | `src/cyclopsd/src/identity.rs` (peer creds plus a pid-ancestry walk to a watched pane), `src/cyclopsd/src/registry.rs` |
| tmux specifics confined to one adapter, CI against tmux HEAD | `src/cyclops-tmux`; advisory tmux-HEAD CI job. One invocation is outside it: `cyclopsd::probe_tmux` runs `tmux -V` and parses through the adapter, which the adapter's own header names as the exception |

The MCP front-door on the same daemon (option D absorbed) is not built and
is not a dependency of anything shipped. The v1 shim and its runbook remain
available for an explicit migration; nothing installs them automatically
(`docs/development/CUTOVER.md`).

### The validation amendments

Letters follow the admin's build brief. The related change number from the
validation report's section 8 is in parentheses; that report is in
`cyclops-arch` and not in this tree, so the table below is the whole of it
you need. The report's change 1
(per-agent ACK capability tiers) is a frozen decision in the brief rather
than a lettered amendment; it lives in `cyclops-manifest` `Hooks.ack`
(None means the screen tier), `ledger.rs` `DeliveredUnverified` plus
`VerifiedBy`, and the manifests (`agy.toml` declares no ack).

| | Amendment | Lives at |
|---|---|---|
| a | `pause-after` set on the control connection at attach (2) | `src/cyclops-tmux/src/control.rs` attach handshake; F15 covers the `%extended-output` consequence |
| b | `bracket_paste_flag` unavailable through tmux 3.6a, so post-paste composer verification is the gate (3) | `src/cyclops-tmux/src/version.rs`; verification with `<message_id>` substitution in `src/cyclopsd/src/delivery.rs` |
| c | Daemon self-test proving hooks actually fire, F1: Codex loads zero hooks in untrusted dirs, silently (4) | `src/cyclopsd/src/selftest.rs`: per-pane edge liveness, `hooks.verify` / `hooks.selftest`, the `hooks_verified` bit, one F1 ping per zero-edge pane |
| d | Dedupe hook events on (session_id, turn_id, event), F2: Codex double-fires across config layers (5) | `src/cyclopsd/src/ack.rs`, plus the reporter's own seq |
| e | Unique tmux buffer name per delivery, F4: named buffers are global and concurrent senders race (6) | `src/cyclopsd/src/delivery.rs`: `cyc-<pid>-<seq>` loaded from a 0600 spool file in a 0700 directory |
| f | Terminal `blocked_quota`: park and alert, never auto-retry, F11 (9) | `state.rs` `BlockedQuota`, `ledger.rs` `ParkedBlockedQuota` (terminal in the record; the operator resends after the reset), parking and the urgent notify in `delivery.rs` |
| g | Modal vocabulary is per-CLI data with explicit decline options, never a generic Enter or Escape, F3, F12, F20 (8) | `cyclops-manifest` `decline_keys` plus `auto_dismiss`; `resources/manifests/*.toml` |
| h | Fusion is rare-blocked-state coverage, not steady-state accuracy (7) | `src/cyclops-proto/src/state.rs` module header; the tier order in `src/cyclopsd/src/fusion.rs` |
| i | Delivery behind a trait so per-agent backends can swap without touching the layers above | `src/cyclopsd/src/delivery.rs`: the `Injector` trait (paste, submit, capture) with `TmuxInjector` as the M1 backend; gate, verify and ACK call through the seam only |

## The zero-polling contract

Idle CPU near zero is a hard goal (GOALS.md). Concretely:

- No interval timers re-querying tmux, panes, or files. State changes
  arrive as control-mode notifications and `refresh-client -B` subscription
  reports; hook reports arrive as socket requests.
- Reconciliation is event-triggered: a notification, a socket request, or a
  delivery gate check may trigger authoritative queries. A clock may not.
- Screen capture runs only at gate time or when a hint puts fused state in
  doubt, never on a schedule.
- Daemon stalls are in-band, not watchdog-polled: `pause-after` converts
  falling behind into `%pause` and `%continue` notifications (amendment a).
- Clients never poll the daemon: `events.subscribe` pushes. The stream UI
  backfills by reading the session ledger tails once at startup and rides
  the push from there.

Timers do exist; none of them is an interval. Each is a one-shot tied to
one thing that already happened: the paste verification re-reads, the
tier-1 ACK window, the tier-2 checkpoints, the decline spacing, the gate's
single wedged-hold ping, the per-pane output settle debounce, the watcher's
reconnect backoff, and the deadlines a caller asked for.

Sanctioned exceptions, none in the product: the Python probe harness
(`tests/e2e/lib/`) polls because it is a measuring instrument, the test
rigs wait in bounded loops for things a test has no edge to await
(`cyclops-testrig`'s `wait_screen`), and demo scripts may wait in a bounded
loop for process startup or for the record to settle between narration
steps.
