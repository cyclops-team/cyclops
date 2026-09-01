#!/usr/bin/env python3
"""A minimal composer that behaves like a vendor TUI, for delivery tests.

`cat` cannot stand in for a composer. It echoes a paste and never gives
the screen back, so the staged marker never leaves and no receipt can
resolve; and it paints nothing below the paste, so terminality is not
decidable and the sentinel can never be proven last. Fixtures built on it
end up claiming a representation the pane does not produce.

This draws the two things that matter and nothing else:

  <transcript rows>
  ❯ <staged text, one row per pasted line>
  ────────                     <- box rule, painted
  Model x · Ctx: NN%           <- status row, painted

Staged text stays in the composer until the submit key arrives. Telling
the two apart is the whole trick, and it is done with bracketed paste
rather than by guessing from how the bytes happened to arrive: a PTY
coalesces and splits writes as it likes, so a payload and the Enter that
follows it can land in one read, and a single pasted newline can land
alone. Advertising mode 2004 makes tmux wrap pastes in \\e[200~ and
\\e[201~, and then a carriage return means submit exactly when it falls
outside a frame.

Chrome is painted, exactly as a vendor paints it, because the escaped
half of a trailer layout is what separates a status row from prose that
merely reads like one.

`--selftest` runs the stream parser against the cases that motivated it
and prints nothing on success.

`--swallow-submit` accepts the submit key and does nothing with it: the
staged text stays in the composer and no turn runs. That is the
staged-never-sent class c shape (submit command accepted, composer not
consumed), which is what a modal or a mode does to an Enter, and it is
the case where treating send-keys success as proof of a turn would paste
a second message over the first.

`--animate-after-swallow` repaints only the status row after swallowing.
It proves that changing chrome is not a receipt while the exact staged
row remains in the composer.

`--swallow-once` keeps the first submit staged, then accepts later submits.
`--clear-staged` makes Ctrl-C clear the composer without exiting. Together
they let recovery tests distinguish submit from discard by the pane's
observable result.

`--submit-log <path>` appends one line for every submit key the fixture
receives. It lets recovery tests prove that reconciliation sends no second key.

`--submit-event-socket <path>` sends one Unix datagram after the fixture
consumes each submit key. It lets a test wait for the observed Enter rather
than polling a log file.

With that event socket, Ctrl-Q emits a `checkpoint` datagram after all earlier
terminal input. Tests use it only after shutting down the delivery worker, so
the checkpoint makes any duplicate queued Enter observable without a sleep.

`--manual-lifecycle` consumes a successful submit but stays visually idle.
Lifecycle tests then use Ctrl-T and Ctrl-Y to choose the observed start and
end without wall-clock races.

BEL (Ctrl-G) hides the composer contents without consuming them: the
staged buffer is untouched and the pane draws an empty composer. That is
the wrapped-payload shape, where a payload is really there and the screen
rules cannot see it, and it is the only way to watch the composer hold
work alone. Nothing in the delivery pipeline sends BEL.

Test-only controls make lifecycle observations deterministic. Ctrl-T keeps a
Working row visible, Ctrl-Y returns to the composer, and Ctrl-L redraws the
current frame. Cyclops itself sends none of these keys.
"""

import os
import socket
import sys
import termios
import time
import tty

RULE = "\x1b[38;5;244m────────────────────────────────────────\x1b[39m"
STATUS = "\x1b[38;5;152mModel x · Ctx: 78%\x1b[39m"
STATUS_ALT = "\x1b[38;5;152mModel x · Ctx: 77%\x1b[39m"
WORKING = "FAKETUI-WORKING"
START = b"\x1b[200~"
END = b"\x1b[201~"


def emit_event(path, event):
    with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as event_socket:
        event_socket.sendto(event, path)


class Stream:
    """Incremental reader: bytes in, (text, submit) events out.

    Holds a tail of bytes that could still be the start of a delimiter,
    so a marker split across two reads is still recognized.
    """

    def __init__(self):
        self.buf = b""
        self.in_paste = False

    def feed(self, chunk):
        self.buf += chunk
        events = []
        while self.buf:
            marker = END if self.in_paste else START
            i = self.buf.find(marker)
            if i == 0:
                self.in_paste = not self.in_paste
                self.buf = self.buf[len(marker) :]
                continue
            cut = i if i > 0 else len(self.buf)
            # Keep back anything that could still grow into a marker.
            if i < 0:
                for n in range(min(len(marker) - 1, len(self.buf)), 0, -1):
                    if marker.startswith(self.buf[-n:]):
                        cut = len(self.buf) - n
                        break
            head, self.buf = self.buf[:cut], self.buf[cut:]
            if not head:
                break
            if self.in_paste:
                # Inside a frame, a carriage return is the payload's own
                # line separator: tmux writes a pasted buffer's newlines
                # as CR. Keeping them raw would draw every row of the
                # payload on top of the first one.
                events.append(("text", head.replace(b"\r", b"\n")))
            else:
                # Outside a paste, a CARRIAGE RETURN is the submit key and
                # everything else, line feeds included, is content. tmux
                # writes a pasted buffer's newlines as LF and the Enter key
                # as CR, so the two are already distinct on the wire; the
                # bracketing above is belt to this braces, not the only
                # thing holding a multi-line payload together.
                start = 0
                for j, byte in enumerate(head):
                    if byte == 13:
                        if j > start:
                            events.append(("text", head[start:j]))
                        events.append(("submit", b""))
                        start = j + 1
                if start < len(head):
                    events.append(("text", head[start:]))
        return events


def draw(transcript, staged, working=False, hidden=False, status=STATUS):
    rows = ["\x1b[2J\x1b[H"]
    rows.extend(transcript)
    if working:
        rows.append(WORKING)
    # Hidden means the text is still staged and simply not drawn, which
    # is what a wrapped payload looks like to a bottom-region rule.
    staged_rows = [""] if hidden else staged.split("\n")
    rows.append("\x1b[39m❯ " + staged_rows[0])
    rows.extend(staged_rows[1:])
    rows.append(RULE)
    rows.append(status)
    sys.stdout.write("\r\n".join(rows) + "\r\n")
    sys.stdout.flush()


def selftest():
    # A marker split across reads is still a marker.
    s = Stream()
    assert s.feed(b"\x1b[20") == []
    assert s.feed(b"0~hello") == [("text", b"hello")]
    assert s.feed(b"\x1b[201~") == []
    assert not s.in_paste

    # A carriage return riding in the same read as the end marker still
    # submits, and only once.
    s = Stream()
    events = s.feed(START + b"payload" + END + b"\r")
    assert events == [("text", b"payload"), ("submit", b"")], events

    # Newlines INSIDE a paste are content, not a run of submits, and the
    # carriage returns tmux actually sends for them are line separators.
    # MEASURED: a three-line buffer arrives as
    # b"\x1b[200~l1\rl2\rl3\x1b[201~", and the Enter that follows arrives
    # by itself as b"\r".
    s = Stream()
    events = s.feed(START + b"a\nb\nc" + END)
    assert events == [("text", b"a\nb\nc")], events
    s = Stream()
    events = s.feed(START + b"l1\rl2\rl3" + END)
    assert events == [("text", b"l1\nl2\nl3")], events
    assert s.feed(b"\r") == [("submit", b"")]

    # Outside a paste, a lone return is the submit key.
    s = Stream()
    assert s.feed(b"\r") == [("submit", b"")]

    # A payload's line feeds are content even with no brackets in sight,
    # which is what keeps a multi-line paste in one piece when tmux does
    # not bracket it.
    s = Stream()
    events = s.feed(b"one\ntwo\nthree")
    assert events == [("text", b"one\ntwo\nthree")], events
    assert s.feed(b"\r") == [("submit", b"")]


def main():
    if "--selftest" in sys.argv:
        selftest()
        return
    swallow = "--swallow-submit" in sys.argv
    animate_after_swallow = "--animate-after-swallow" in sys.argv
    swallow_once = "--swallow-once" in sys.argv
    clear_staged = "--clear-staged" in sys.argv
    manual_lifecycle = "--manual-lifecycle" in sys.argv
    submit_log = None
    if "--submit-log" in sys.argv:
        submit_log = sys.argv[sys.argv.index("--submit-log") + 1]
    submit_event_socket = None
    if "--submit-event-socket" in sys.argv:
        submit_event_socket = sys.argv[sys.argv.index("--submit-event-socket") + 1]
    swallowed = False
    forced_working = False
    fd = sys.stdin.fileno()
    saved = termios.tcgetattr(fd)
    tty.setraw(fd)
    sys.stdout.write("\x1b[?2004h")
    sys.stdout.flush()
    transcript = []
    staged = ""
    hidden = False
    stream = Stream()
    try:
        draw(transcript, staged)
        while True:
            chunk = os.read(fd, 65536)
            if not chunk:
                break
            if manual_lifecycle and chunk == b"\x1b":
                forced_working = False
                draw(transcript, staged, hidden=hidden)
                continue
            if b"\x03" in chunk:
                if not clear_staged:
                    break
                staged = ""
                hidden = False
                chunk = chunk.replace(b"\x03", b"")
                draw(transcript, staged, working=forced_working)
                if not chunk:
                    continue
            if b"\x07" in chunk:
                # Keep the staged buffer, stop drawing it.
                hidden = True
                chunk = chunk.replace(b"\x07", b"")
                draw(transcript, staged, working=forced_working, hidden=hidden)
                if not chunk:
                    continue
            if b"\x14" in chunk:
                forced_working = True
                chunk = chunk.replace(b"\x14", b"")
                draw(transcript, staged, working=True, hidden=hidden)
                if not chunk:
                    continue
            if b"\x19" in chunk:
                forced_working = False
                chunk = chunk.replace(b"\x19", b"")
                draw(transcript, staged, hidden=hidden)
                if not chunk:
                    continue
            if b"\x0c" in chunk:
                chunk = chunk.replace(b"\x0c", b"")
                draw(transcript, staged, working=forced_working, hidden=hidden)
                if not chunk:
                    continue
            if b"\x11" in chunk:
                chunk = chunk.replace(b"\x11", b"")
                if submit_event_socket is not None:
                    emit_event(submit_event_socket, b"checkpoint")
                if not chunk:
                    continue
            for kind, payload in stream.feed(chunk):
                if kind == "text":
                    for char in payload.decode("utf-8", "replace"):
                        if char == "\x7f":
                            staged = staged[:-1]
                        else:
                            staged += char
                    hidden = False
                    draw(transcript, staged, working=forced_working)
                else:
                    if submit_log is not None:
                        with open(submit_log, "a", encoding="utf-8") as log:
                            log.write("submit\n")
                    if submit_event_socket is not None:
                        emit_event(submit_event_socket, b"submit")
                    if swallow or (swallow_once and not swallowed):
                        # The key arrived and was accepted. Nothing else
                        # happens: the composer keeps its text and no turn
                        # starts.
                        swallowed = True
                        status = STATUS_ALT if animate_after_swallow else STATUS
                        draw(
                            transcript,
                            staged,
                            working=forced_working,
                            hidden=hidden,
                            status=status,
                        )
                    elif staged:
                        transcript.extend(("\x1b[1;2m❯\x1b[0m " + staged).split("\n"))
                        staged = ""
                        if manual_lifecycle:
                            draw(transcript, staged, working=forced_working)
                        else:
                            draw(transcript, staged, working=True)
                            time.sleep(0.4)
                            draw(transcript, staged, working=forced_working)
                    else:
                        draw(transcript, staged, working=forced_working)
    finally:
        sys.stdout.write("\x1b[?2004l")
        sys.stdout.flush()
        termios.tcsetattr(fd, termios.TCSADRAIN, saved)


if __name__ == "__main__":
    main()
