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
"""

import os
import sys
import termios
import time
import tty

RULE = "\x1b[38;5;244m────────────────────────────────────────\x1b[39m"
STATUS = "\x1b[38;5;152mModel x · Ctx: 78%\x1b[39m"
WORKING = "FAKETUI-WORKING"
START = b"\x1b[200~"
END = b"\x1b[201~"


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


def draw(transcript, staged, working=False):
    rows = ["\x1b[2J\x1b[H"]
    rows.extend(transcript)
    if working:
        rows.append(WORKING)
    staged_rows = staged.split("\n")
    rows.append("\x1b[39m❯ " + staged_rows[0])
    rows.extend(staged_rows[1:])
    rows.append(RULE)
    rows.append(STATUS)
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
    fd = sys.stdin.fileno()
    saved = termios.tcgetattr(fd)
    tty.setraw(fd)
    sys.stdout.write("\x1b[?2004h")
    sys.stdout.flush()
    transcript = []
    staged = ""
    stream = Stream()
    try:
        draw(transcript, staged)
        while True:
            chunk = os.read(fd, 65536)
            if not chunk:
                break
            if b"\x03" in chunk:
                break
            for kind, payload in stream.feed(chunk):
                if kind == "text":
                    staged += payload.decode("utf-8", "replace")
                    draw(transcript, staged)
                elif staged:
                    transcript.extend(("\x1b[39m❯ " + staged).split("\n"))
                    staged = ""
                    draw(transcript, staged, working=True)
                    time.sleep(0.4)
                    draw(transcript, staged)
                else:
                    draw(transcript, staged)
    finally:
        sys.stdout.write("\x1b[?2004l")
        sys.stdout.flush()
        termios.tcsetattr(fd, termios.TCSADRAIN, saved)


if __name__ == "__main__":
    main()
