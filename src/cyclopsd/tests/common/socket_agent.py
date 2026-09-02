"""Authenticated pane fixture with measured Codex screen states.

The initial escaped row is Codex's empty ghost suggestion. Before executing
each socket client command received through the pane, the fixture paints the
measured working marker so a newly released FIFO attempt cannot write.
"""

import shlex
import subprocess
import sys


GHOST = (
    "\x1b[1m\x1b[38;2;255;178;66m›\x1b[0m "
    "\x1b[2mSummarize recent commits\x1b[0m"
)
WORKING = "• Working (0s • esc to interrupt)"

sys.stdout.write(GHOST)
sys.stdout.flush()

for line in sys.stdin:
    sys.stdout.write(f"\x1b[2J\x1b[H{WORKING}\n")
    sys.stdout.flush()
    try:
        subprocess.run(shlex.split(line), check=False)
    except Exception:
        pass
