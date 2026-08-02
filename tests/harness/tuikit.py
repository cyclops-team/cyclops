#!/usr/bin/env python3
"""Shared helpers for driving agent TUIs inside an isolated tmux server.

Ported nearly verbatim from the ADR-001 validation campaign
(cyclops-arch/validation/scripts/tuikit.py, 2026-08-01). Lives here as the
seed of the regression suite: probe tests against vendor TUIs, run
validation-campaign style, with MEASURED results recorded in the repo's
findings.md. Changes from the source, and nothing else: the socket name
comes from CYC_HARNESS_SOCK (default "cyc-harness"), RAW is the repo's
tests/raw (created on demand, gitignored), campaign-absolute paths removed.

Every function that talks to tmux goes through SOCK. Nothing here may ever touch
the default tmux server.
"""
import subprocess, os, time, json, tempfile

SOCK = os.environ.get("CYC_HARNESS_SOCK", "cyc-harness")
RAW = os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))),
    "tests", "raw",
)

def raw_dir():
    """Probe artifact directory, created on demand. Gitignored."""
    os.makedirs(RAW, exist_ok=True)
    return RAW

def tmux(*args, check=False):
    r = subprocess.run(["tmux", "-L", SOCK, *args], capture_output=True, text=True)
    if check and r.returncode != 0:
        raise RuntimeError(f"tmux {args}: {r.stderr.strip()}")
    return r

def ensure_server():
    tmux("start-server")

def clean_env():
    env = dict(os.environ)
    for k in ("CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "CLAUDE_CODE_SSE_PORT", "TMUX"):
        env.pop(k, None)
    return env

def launch(session, cmd, cwd, w=120, h=40):
    """Create session running cmd. First call creates the isolated server with -f /dev/null."""
    r = subprocess.run(["tmux", "-f", "/dev/null", "-L", SOCK, "new-session", "-d",
                        "-s", session, "-x", str(w), "-y", str(h), "-c", cwd, cmd],
                       capture_output=True, text=True, env=clean_env())
    if r.returncode != 0:
        raise RuntimeError(f"launch failed: {r.stderr.strip()}")

def kill(session):
    tmux("kill-session", "-t", session)

def capture(target, tail=None):
    r = tmux("capture-pane", "-p", "-t", target)
    out = r.stdout
    if tail:
        out = "\n".join(out.rstrip().splitlines()[-tail:])
    return out

def title(target):
    return tmux("display-message", "-p", "-t", target, "#{pane_title}").stdout.strip()

def pane_in_mode(target):
    return tmux("display-message", "-p", "-t", target, "#{pane_in_mode}").stdout.strip() == "1"

def send_enter(target):
    tmux("send-keys", "-t", target, "Enter")

def send_key(target, key):
    tmux("send-keys", "-t", target, key)

_bufseq = 0

def paste(target, text, bracketed=True):
    """Deliver text via load-buffer + paste-buffer. Returns wall ts_ns just before paste.

    The buffer name MUST be unique per delivery. tmux buffer names are global
    server state, so two concurrent senders sharing one name race: the first
    loads, the second pastes with -d and deletes it, and the first then fails
    with "no buffer <name>". Observed live during the parallel soak. A single
    serializing daemon avoids this by construction; independent senders cannot.
    """
    global _bufseq
    _bufseq += 1
    buf = f"cycval_{os.getpid()}_{_bufseq}"
    f = tempfile.NamedTemporaryFile("w", delete=False, suffix=".cycval")
    f.write(text)
    f.close()
    tmux("load-buffer", "-b", buf, f.name, check=True)
    args = ["paste-buffer", "-b", buf, "-d", "-t", target]
    if bracketed:
        args.insert(1, "-p")
    t0 = time.time_ns()
    tmux(*args, check=True)
    os.unlink(f.name)
    return t0

class HookTail:
    """Tail an NDJSON hook log written by hooklog.py (or codex notify wrapper)."""
    def __init__(self, path):
        self.path = path
        self.pos = 0
        open(path, "a").close()

    def new(self):
        out = []
        with open(self.path) as f:
            f.seek(self.pos)
            chunk = f.read()
            self.pos = f.tell()
        for line in chunk.splitlines():
            line = line.strip()
            if line:
                try:
                    out.append(json.loads(line))
                except Exception:
                    out.append({"event": "PARSE_ERROR", "raw": line[:200]})
        return out

    def wait_for(self, pred, timeout):
        """Wait for an entry matching pred; returns (entry, arrival_ts_ns) or (None, None)."""
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            for e in self.new():
                if pred(e):
                    return e, time.time_ns()
            time.sleep(0.05)
        return None, None

MODAL_SIGNS = (
    "Enter to confirm", "Esc to keep", "Press enter to continue",
    "Do you trust", "Update available", "1. Yes", "2. Skip",
    "Do you want to", "❯ 1.", "› 1.",
)

def modal_visible(cap):
    """Any numbered-choice or confirm dialog. Deliberately broad: a false
    positive costs a wait, a false negative costs a blind Enter into a dialog.
    An update dialog whose default action is `brew upgrade --cask codex` was
    observed during this campaign, which is why 'Press enter to continue'
    alone must never be treated as a composer."""
    return any(s in cap for s in MODAL_SIGNS)

def dismiss_modal(target, cap, log):
    """Dismiss safely. NEVER a bare Enter: the default choice can be destructive.
    Choose the explicit decline option by number when the dialog offers one."""
    log.write("MODAL SEEN:\n" + "\n".join(cap.rstrip().splitlines()[-16:]) + "\n---\n")
    log.flush()
    low = cap.lower()
    if "update available" in low or "update now" in low:
        send_key(target, "2")          # Skip, never "Update now"
        time.sleep(0.4); send_enter(target)
    elif "do you trust" in low:
        send_key(target, "1")          # our own scratch dir
        time.sleep(0.4); send_enter(target)
    elif "hook" in low and "trust" in low:
        send_key(target, "1")
        time.sleep(0.4); send_enter(target)
    else:
        send_key(target, "Escape")

def wait_composer(target, ready_pred, log, timeout=90):
    """Wait until ready_pred(capture) is true, dismissing modals along the way."""
    end = time.monotonic() + timeout
    cap = ""
    while time.monotonic() < end:
        time.sleep(1)
        cap = capture(target)
        if modal_visible(cap):
            dismiss_modal(target, cap, log)
            continue
        if ready_pred(cap):
            return cap
    raise TimeoutError(f"composer not ready; last capture tail:\n" +
                       "\n".join(cap.rstrip().splitlines()[-12:]))

def claude_ready(cap):
    lines = cap.rstrip().splitlines()
    return any(l.strip().startswith("❯") for l in lines[-6:]) and not modal_visible(cap)
