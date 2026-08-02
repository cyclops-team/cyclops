#!/usr/bin/env python3
"""M1 regression gate: 100-message mini-soak per available agent CLI.

Drives real vendor TUIs (claude, codex, agy) in an isolated tmux server and
delivers every message end-to-end through cyclopsd msg.send via the real
`cyclops send` binary. No raw tuikit paste: the daemon owns gating,
injection, verification, ACK, and the ledger. Ground truth for the verdict
is the ledger file, never client output.

Campaign pattern (ADR-001 validation soak): cheapest models, trivial
prompts, quota parking is a valid outcome. Launch-phase modal quirks are
handled tuikit-style by this driver; once the daemon starts delivering,
modal handling belongs to the manifests.

Gate: zero unrecovered loss. Every sent message must end delivered_verified
or delivered_unverified; a leg parked by vendor quota counts as complete at
the parking point. attention_required or a stuck delivery is a FAILURE.

Isolation: tmux -u -L cyc-soak-<pid> -f /dev/null (F14), scratch
CYCLOPS_HOME under /private/tmp, everything killed in teardown. Never
touches the user's tmux server. Driver-side wait loops are test-side waits,
outside the daemon's zero-polling contract.

Rerun additions (m1-soak-2): claude launches through its native versioned
symlink so manifest binding must come from the argv fallback (F21 real fix,
no hardlink shim); the final pane scrollback is scanned for duplicated
[cyclops <msg_id>] markers (double-paste is the failure mode amendment 2
forbids) and any duplicate fails the gate; daemon.log detach/reattach
events are counted so the F22 control-drop fix is proven by a zero.

Usage: python3 tests/m1_soak.py [count-per-cli]   (default 100)
CYC_SOAK_RAW overrides the artifact directory (default tests/raw/m1-soak).
"""
import json
import os
import random
import shlex
import shutil
import socket
import subprocess
import sys
import threading
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CYCLOPS = os.path.join(REPO, "target/release/cyclops")
CYCLOPSD = os.path.join(REPO, "target/release/cyclopsd")
MANIFESTS = os.path.join(REPO, "manifests")
RAW = os.environ.get("CYC_SOAK_RAW", os.path.join(REPO, "tests/raw/m1-soak"))
PID = os.getpid()
SCRATCH = f"/private/tmp/cyc-m1-soak-{PID}"
HOME = os.path.join(SCRATCH, "home")
SOCK = f"cyc-soak-{PID}"
SESSION = "soak"
TARGET = int(sys.argv[1]) if len(sys.argv) > 1 else 100
TERMINAL = {
    "delivered_verified",
    "delivered_unverified",
    "attention_required",
    "parked_blocked_quota",
}
random.seed(1729)

log_lock = threading.Lock()
run_log = None


def log(msg):
    line = f"[{time.strftime('%H:%M:%S')}] {msg}"
    with log_lock:
        print(line, flush=True)
        if run_log:
            run_log.write(line + "\n")
            run_log.flush()


# ---------------------------------------------------------------------------
# tmux (isolated server only; -u per finding F14)
# ---------------------------------------------------------------------------

def clean_env(extra=None):
    env = dict(os.environ)
    for k in ("CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "CLAUDE_CODE_SSE_PORT",
              "TMUX", "CODEX_HOME", "CYCLOPS_HOME", "CYCLOPS_AGENT"):
        env.pop(k, None)
    env["LANG"] = "en_US.UTF-8"
    if extra:
        env.update(extra)
    return env


def tmux(*args, check=False):
    r = subprocess.run(["tmux", "-u", "-L", SOCK, "-f", "/dev/null", *args],
                       capture_output=True, text=True, env=clean_env())
    if check and r.returncode != 0:
        raise RuntimeError(f"tmux {args}: {r.stderr.strip()}")
    return r


def capture(target):
    return tmux("capture-pane", "-p", "-t", target).stdout


def send_key(target, key):
    tmux("send-keys", "-t", target, key)


# ---------------------------------------------------------------------------
# Launch-phase modal handling (tuikit knowledge; per-CLI vocabulary)
# ---------------------------------------------------------------------------

MODAL_SIGNS = (
    "Enter to confirm", "Esc to keep", "Press enter to continue",
    "Do you trust", "trust this folder", "safety check",
    "Update available", "1. Yes", "2. Skip",
    "Do you want to", "❯ 1.", "› 1.",
)

# Dialog text meaning Escape aborts or exits the program (Claude 2.1.220's
# trust dialog says 'Esc to cancel' and Escape exits the CLI). Same
# vocabulary as tests/harness/tuikit.py.
ESC_EXITS_SIGNS = ("esc to cancel", "esc to exit", "esc cancels", "esc exits")


def modal_visible(cap):
    return any(s in cap for s in MODAL_SIGNS)


def dismiss_modal(cli, target, cap, modlog):
    """Never a bare Enter: default choices can be destructive (F3)."""
    modlog.write("MODAL SEEN:\n" + "\n".join(cap.rstrip().splitlines()[-16:]) + "\n---\n")
    modlog.flush()
    low = cap.lower()
    if "bypass permissions" in low and "accept" in low:
        # First-run acknowledgement for the mode the campaign (and the
        # operator's own aliases) always run with. Option 2 = accept.
        send_key(target, "2")
        time.sleep(0.4)
        send_key(target, "Enter")
    elif "update available" in low or "update now" in low:
        # Codex option 3 is the explicit skip (F3); other CLIs use 2=Skip.
        send_key(target, "3" if cli == "codex" else "2")
        time.sleep(0.4)
        send_key(target, "Enter")
    elif ("do you trust" in low or "trust this folder" in low
          or ("safety check" in low and "trust" in low)):
        # Trust prompts for our own scratch dir. Claude 2.1.220 words it
        # "Is this a project you created or one you trust?" (MEASURED in
        # this soak), so matching "Do you trust" alone misses it.
        send_key(target, "1")
        time.sleep(0.4)
        send_key(target, "Enter")
    elif "how's the cli experience" in low:
        send_key(target, "0")  # agy survey: explicit skip (F12)
    elif any(s in low for s in ESC_EXITS_SIGNS):
        # Unknown dialog where Escape aborts the program: never press it.
        # Log and stand off; wait_composer times out with evidence instead.
        modlog.write("UNKNOWN MODAL: Esc exits, not pressing it\n")
        modlog.flush()
    else:
        send_key(target, "Escape")


READY = {
    "claude": lambda c: any(l.strip().startswith("❯")
                            for l in c.rstrip().splitlines()[-6:]),
    "codex": lambda c: any(l.lstrip().startswith("›")
                           for l in c.rstrip().splitlines()[-6:]),
    "agy": lambda c: any(l.strip() == ">" or l.strip().startswith("> ")
                         for l in c.rstrip().splitlines()[-4:]),
}


def wait_composer(cli, target, modlog, timeout=180):
    """Composer ready, dismissing launch modals. Returns (ok, last_capture)."""
    end = time.monotonic() + timeout
    cap = ""
    while time.monotonic() < end:
        time.sleep(1)
        cap = capture(target)
        if modal_visible(cap):
            dismiss_modal(cli, target, cap, modlog)
            continue
        # Ready wins over a visible quota banner: with a live composer the
        # first send must flow through the daemon so its own blocked_quota
        # gate parks the leg (park-on-first proof, F11 gate rules). The
        # quota branch only covers a pane whose composer never comes up.
        if READY[cli](cap):
            return "ready", cap
        if "Individual quota reached" in cap:
            return "quota", cap
    return "timeout", cap


# ---------------------------------------------------------------------------
# NDJSON socket client
# ---------------------------------------------------------------------------

class Client:
    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(10)
        self.sock.connect(path)
        self.f = self.sock.makefile("r")
        self.hello = json.loads(self.f.readline())
        self.next_id = 1
        self.pending = []

    def request(self, method, params, timeout=15):
        rid = self.next_id
        self.next_id += 1
        self.sock.settimeout(timeout)
        line = json.dumps({"id": rid, "method": method, "params": params}) + "\n"
        self.sock.sendall(line.encode())
        while True:
            v = json.loads(self.f.readline())
            if "event" in v:
                self.pending.append(v)
                continue
            if v.get("id") == rid:
                return v

    def wait_event(self, pred, timeout):
        """Next event matching pred; buffered first. Returns None on timeout."""
        for i, e in enumerate(self.pending):
            if pred(e):
                return self.pending.pop(i)
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return None
            self.sock.settimeout(min(remaining, 30) + 0.1)
            try:
                v = json.loads(self.f.readline())
            except (socket.timeout, TimeoutError):
                continue
            if "event" not in v:
                continue
            if pred(v):
                return v
            self.pending.append(v)

    def close(self):
        try:
            self.f.close()
            self.sock.close()
        except OSError:
            pass


# ---------------------------------------------------------------------------
# Rig setup
# ---------------------------------------------------------------------------

def hook_cmd(event, agent):
    return f"{CYCLOPS} hook {event} --agent {agent}"


def build_legs():
    """One leg per CLI binary present on PATH. Absent CLIs are skipped.
    CYC_SOAK_CLIS=claude,codex narrows the fleet for debugging runs."""
    wanted = os.environ.get("CYC_SOAK_CLIS", "claude,codex,agy").split(",")
    legs = []
    for cli in ("claude", "codex", "agy"):
        if cli not in wanted:
            continue
        path = shutil.which(cli)
        if not path:
            legs.append({"cli": cli, "skipped": True})
            continue
        legs.append({"cli": cli, "skipped": False, "bin": path, "records": [],
                     "parked": False, "stop_reason": None, "sent": 0})
    return legs


def write_vendor_configs(legs):
    """Hook configs the way the validation harness did (--settings / CODEX_HOME)."""
    for leg in legs:
        if leg["skipped"]:
            continue
        cli = leg["cli"]
        proj = os.path.join(SCRATCH, f"{cli}proj")
        os.makedirs(proj, exist_ok=True)
        leg["proj"] = proj
        env = {"CYCLOPS_HOME": HOME, "CYCLOPS_AGENT": cli, "LANG": "en_US.UTF-8"}
        if cli == "claude":
            settings = os.path.join(SCRATCH, "claude-settings.json")
            hooks = {e: [{"hooks": [{"type": "command", "command": hook_cmd(e, cli)}]}]
                     for e in ("UserPromptSubmit", "Stop", "Notification")}
            with open(settings, "w") as f:
                json.dump({"hooks": hooks}, f)
            # F21 real fix in play: launch the native versioned symlink
            # directly, so pane_current_command reads "2.1.220" and the
            # manifest can only bind through the argv_basenames fallback
            # (ps argv[0] basename is still "claude"). The first soak used
            # a hardlink shim here; keeping the shim would mask the fix.
            # The binding check before spending tokens catches a miss.
            leg["cmd"] = (f"{leg['bin']} --model haiku --dangerously-skip-permissions "
                          f"--settings {settings}")
        elif cli == "codex":
            # CODEX_HOME under scratch is the F1 fix: user-level hooks load
            # without the directory-trust dialog. ONLY auth.json is copied.
            ch = os.path.join(SCRATCH, "codexhome")
            os.makedirs(ch, exist_ok=True)
            auth = os.path.expanduser("~/.codex/auth.json")
            if os.path.exists(auth):
                shutil.copy(auth, os.path.join(ch, "auth.json"))
            else:
                leg["stop_reason"] = "no ~/.codex/auth.json to copy"
            hooks = {e: [{"hooks": [{"type": "command", "command": hook_cmd(e, cli)}]}]
                     for e in ("UserPromptSubmit", "Stop")}
            with open(os.path.join(ch, "hooks.json"), "w") as f:
                json.dump({"hooks": hooks}, f)
            with open(os.path.join(ch, "config.toml"), "w") as f:
                f.write(f'model = "gpt-5.6-luna"\n\n'
                        f'[projects."{proj}"]\ntrust_level = "trusted"\n')
            env["CODEX_HOME"] = ch
            leg["cmd"] = (f"{leg['bin']} --dangerously-bypass-hook-trust "
                          f"--dangerously-bypass-approvals-and-sandbox")
        else:
            # agy: screen tier by design. No usable ACK hooks (F7); default model.
            leg["cmd"] = f"{leg['bin']} --dangerously-skip-permissions"
        leg["env"] = env
        leg["shell_cmd"] = ("env " + " ".join(f"{k}={shlex.quote(v)}"
                                              for k, v in env.items())
                            + " " + leg["cmd"])


def launch_tuis(active):
    """One window per CLI, created in leg order. Panes are mapped back by
    window index, not name: automatic-rename would race a name lookup.

    A placeholder window boots the session first so history-limit is raised
    before any CLI pane exists; the end-of-run duplicate scan needs the
    whole transcript in scrollback, and the /dev/null config default (2000
    lines) can truncate a 100-message leg. The placeholder dies once the
    leg windows are up."""
    tmux("new-session", "-d", "-s", SESSION, "-x", "120", "-y", "40",
         "-n", "rig-placeholder", "sleep 600", check=True)
    tmux("set-option", "-g", "history-limit", "20000", check=True)
    for leg in active:
        tmux("new-window", "-d", "-t", f"{SESSION}:", "-n", leg["cli"],
             "-c", leg["proj"], leg["shell_cmd"], check=True)
    tmux("kill-window", "-t", f"{SESSION}:0", check=True)
    rows = tmux("list-panes", "-s", "-t", SESSION, "-F",
                "#{window_index}\t#{pane_id}\t#{pane_current_command}",
                check=True).stdout
    by_index = {}
    for row in rows.strip().splitlines():
        widx, pane_id, cmd = (row.split("\t") + ["", ""])[:3]
        by_index[int(widx)] = (pane_id, cmd)
    for i, widx in enumerate(sorted(by_index)):
        active[i]["pane"], active[i]["launch_cmd_seen"] = by_index[widx]


def live_pane_count():
    rows = tmux("list-panes", "-s", "-t", SESSION, "-F", "#{pane_id}").stdout
    return len(rows.strip().splitlines())


def boot_daemon(pane_count):
    os.makedirs(HOME, exist_ok=True)
    with open(os.path.join(HOME, "config.toml"), "w") as f:
        f.write(f'sessions = ["{SESSION}"]\n'
                f'tmux_socket = "{SOCK}"\n'
                f'tmux_config = "/dev/null"\n'
                f'manifest_dir = "{MANIFESTS}"\n')
    dlog = open(os.path.join(SCRATCH, "daemon.log"), "w")
    proc = subprocess.Popen([CYCLOPSD], env=clean_env({"CYCLOPS_HOME": HOME}),
                            stdout=dlog, stderr=dlog)
    sock_path = os.path.join(HOME, "sock")
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        if os.path.exists(sock_path):
            try:
                c = Client(sock_path)
                st = c.request("status", {})["result"]
                s = st["sessions"][0]
                if s["attached"] and len(s["panes"]) == pane_count:
                    return proc, c, sock_path
                c.close()
            except (OSError, json.JSONDecodeError):
                pass
        if proc.poll() is not None:
            raise RuntimeError("cyclopsd exited during boot; see daemon.log")
        time.sleep(0.3)
    raise RuntimeError("daemon never attached with expected panes")


# ---------------------------------------------------------------------------
# Leg driver
# ---------------------------------------------------------------------------

def send_msg(leg, n):
    subj = f"soak {leg['cli']} {n:03d}"
    body = f"Reply with just: ok ({n})"
    env = clean_env({"CYCLOPS_HOME": HOME})
    t0 = time.time_ns()
    p = subprocess.run([CYCLOPS, "send", leg["cli"], "--fyi",
                        "--subject", subj, "--body", body, "--json"],
                       capture_output=True, text=True, timeout=60, env=env)
    wall_ms = round((time.time_ns() - t0) / 1e6, 1)
    try:
        receipt = json.loads(p.stdout)
    except json.JSONDecodeError:
        return None, {"seq": n, "error": f"send produced no receipt: "
                                         f"{p.stdout[:200]} {p.stderr[:200]}"}
    d = receipt["deliveries"][0]
    return receipt, {"seq": n, "msg_id": receipt["msg_id"],
                     "receipt_state": d["state"], "receipt_note": d.get("note"),
                     "send_wall_ms": wall_ms}


def ledger_final_state(mid, label):
    """Latest delivery state for (msg, label) straight from the ledger.
    Reconcile-on-doubt path for a dropped event subscription."""
    path = os.path.join(HOME, "ledger", f"{SESSION}.ndjson")
    final = None
    try:
        with open(path) as f:
            for line in f:
                if mid not in line:
                    continue
                try:
                    l = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if (l.get("kind") == "state" and l.get("id") == mid
                        and (l.get("data") or {}).get("to") == label):
                    final = l["data"].get("to_state", final)
    except OSError:
        pass
    return final


def save_failure(leg, n, tag):
    cap = capture(leg["pane"])
    with open(os.path.join(RAW, f"{leg['cli']}_fail_{n:03d}_{tag}.txt"), "w") as f:
        f.write(cap)
    ledger = os.path.join(HOME, "ledger", f"{SESSION}.ndjson")
    if os.path.exists(ledger):
        with open(ledger) as f:
            tail = f.readlines()[-40:]
        with open(os.path.join(RAW, f"{leg['cli']}_fail_{n:03d}_ledger_tail.ndjson"), "w") as f:
            f.writelines(tail)


def run_leg(leg, sock_path):
    ev = Client(sock_path)
    ev.request("events.subscribe", {"kinds": ["delivery-state", "admin-notify"]})
    for n in range(1, TARGET + 1):
        receipt, rec = send_msg(leg, n)
        leg["records"].append(rec)
        if receipt is None:
            leg["stop_reason"] = rec["error"]
            save_failure(leg, n, "send_error")
            break
        leg["sent"] += 1
        state = rec["receipt_state"]
        mid = rec["msg_id"]
        if state not in TERMINAL:
            # Queued or still in flight: wait on delivery-state events.
            # A non-dismissable vendor block surfaces as admin-notify
            # action_required; shorten the wait once one arrives.
            deadline, notified = 240, False
            t0 = time.monotonic()
            while True:
                # The daemon drops subscribers that stall while this thread
                # blocks inside the send subprocess (MEASURED: "client write
                # stalled; dropping connection"). Reconnect and reconcile
                # from the ledger instead of dying mid-leg.
                try:
                    e = ev.wait_event(
                        lambda e: (e["event"] == "delivery-state"
                                   and e["data"].get("id") == mid
                                   and e["data"].get("to_state") in TERMINAL)
                        or e["event"] == "admin-notify",
                        min(15, max(0.1, deadline - (time.monotonic() - t0))))
                except (OSError, ValueError):
                    ev.close()
                    time.sleep(0.5)
                    ev = Client(sock_path)
                    ev.request("events.subscribe",
                               {"kinds": ["delivery-state", "admin-notify"]})
                    final = ledger_final_state(mid, leg["cli"])
                    if final in TERMINAL:
                        state = final
                        break
                    continue
                if e is None:
                    if time.monotonic() - t0 >= deadline:
                        state = "stuck"
                        break
                    continue
                if e["event"] == "admin-notify":
                    lvl = e["data"].get("level")
                    rec.setdefault("notify", []).append(
                        {"level": lvl, "subject": e["data"].get("subject")})
                    if lvl == "action_required" and not notified:
                        notified = True
                        deadline = min(deadline, (time.monotonic() - t0) + 20)
                    continue
                state = e["data"]["to_state"]
                break
        rec["resolved_state"] = state
        if state == "parked_blocked_quota":
            leg["parked"] = True
            leg["stop_reason"] = f"vendor quota parked at seq {n}"
            log(f"[{leg['cli']}] PARKED by quota at {n}/{TARGET}")
            break
        if state in ("attention_required", "stuck"):
            leg["stop_reason"] = f"{state} at seq {n}"
            save_failure(leg, n, state)
            log(f"[{leg['cli']}] FAILURE: {state} at {n}/{TARGET}")
            break
        if n % 10 == 0:
            log(f"[{leg['cli']}] {n}/{TARGET} last={state}")
        time.sleep(random.uniform(0.1, 0.6))
    ev.close()


def scan_duplicates(leg):
    """Scan the leg's full pane scrollback for double-pasted deliveries.

    Every injected payload opens with the unique "[cyclops <msg_id>]"
    marker; after submit it stays in the transcript exactly once. A marker
    seen twice means the payload was pasted twice (the first soak's claude
    failure mode: retry after a blind delivery). -J unwraps soft-wrapped
    lines so a marker split at the pane edge still counts. Coverage is
    recorded because a truncated scrollback bounds what the scan can see."""
    cap = tmux("capture-pane", "-p", "-J", "-S", "-", "-t", leg["pane"]).stdout
    with open(os.path.join(RAW, f"{leg['cli']}_pane_final.txt"), "w") as f:
        f.write(cap)
    dups, seen = {}, 0
    for rec in leg["records"]:
        mid = rec.get("msg_id")
        if not mid:
            continue
        n = cap.count(f"[cyclops {mid}]")
        if n >= 1:
            seen += 1
        if n > 1:
            dups[rec["seq"]] = n
    leg["dup_seqs"] = dups
    leg["pane_markers_seen"] = seen


# ---------------------------------------------------------------------------
# Ledger analysis (ground truth)
# ---------------------------------------------------------------------------

def pct(xs, p):
    return xs[min(len(xs) - 1, int(round(p / 100 * (len(xs) - 1))))] if xs else None


def analyze(legs, ledger_lines):
    per_cli = {}
    for leg in legs:
        if leg.get("skipped"):
            per_cli[leg["cli"]] = {"skipped": True, "sent": 0,
                                   "delivered_verified": 0,
                                   "delivered_unverified": 0, "lost": 0}
            continue
        label = leg["cli"]
        details = []
        counts = {"delivered_verified": 0, "delivered_unverified": 0,
                  "parked_blocked_quota": 0, "lost": 0}
        ack, e2e, retries_total = [], [], 0
        for rec in leg["records"]:
            mid = rec.get("msg_id")
            if not mid:
                counts["lost"] += 1
                details.append(rec)
                continue
            steps = [l for l in ledger_lines
                     if l.get("kind") == "state" and l.get("id") == mid
                     and (l.get("data") or {}).get("to") == label
                     and "to_state" in (l.get("data") or {})]
            walk = [s["data"]["to_state"] for s in steps]
            final = walk[-1] if walk else rec.get("resolved_state", "missing")
            submitted = [s["ts"] for s in steps if s["data"]["to_state"] == "submitted"]
            delivered = [(s["ts"], s) for s in steps
                         if s["data"]["to_state"].startswith("delivered")]
            verified_by = None
            attempts = None
            for s in reversed(steps):
                if s.get("deliveries"):
                    verified_by = s["deliveries"][0].get("verified_by")
                    attempts = s["deliveries"][0].get("attempts")
                    break
            retries = sum(1 for w in walk if w == "retry_queued")
            retries_total += retries
            d = dict(rec)
            d.update({"final": final, "walk": walk, "verified_by": verified_by,
                      "attempts": attempts, "retries": retries})
            if submitted and delivered:
                # ACK latency: last submit to the first delivery evidence
                # after it (hook tier or screen tier, whichever resolved).
                after = [ts for ts, _ in delivered if ts >= submitted[-1]]
                if after:
                    d["ack_ms"] = after[0] - submitted[-1]
                    ack.append(d["ack_ms"])
            queued_ts = steps[0]["ts"] if steps else None
            if queued_ts and delivered:
                e2e.append(delivered[-1][0] - queued_ts)
            if final in counts:
                counts[final] += 1
            else:
                counts["lost"] += 1
            details.append(d)
        ack.sort()
        e2e.sort()
        per_cli[label] = {
            "skipped": False,
            "sent": leg["sent"],
            "delivered_verified": counts["delivered_verified"],
            "delivered_unverified": counts["delivered_unverified"],
            "parked_blocked_quota": counts["parked_blocked_quota"],
            "lost": counts["lost"],
            "retries": retries_total,
            "duplicates": len(leg.get("dup_seqs") or {}),
            "duplicate_seqs": leg.get("dup_seqs") or {},
            "pane_marker_coverage": f"{leg.get('pane_markers_seen', 0)}/{leg['sent']}",
            # Ledger ground truth, not thread state: a leg whose driver
            # thread died after the park still counts as parked.
            "parked": leg["parked"] or counts["parked_blocked_quota"] > 0,
            "stop_reason": leg["stop_reason"],
            "ack_ms": {"p50": pct(ack, 50), "p95": pct(ack, 95),
                       "max": ack[-1] if ack else None},
            "e2e_ms": {"p50": pct(e2e, 50), "p95": pct(e2e, 95),
                       "max": e2e[-1] if e2e else None},
        }
        with open(os.path.join(RAW, f"{label}_deliveries.json"), "w") as f:
            json.dump(details, f, indent=1)
        # Per-CLI ledger copy: every line for this leg's message ids.
        mids = {r["msg_id"] for r in leg["records"] if r.get("msg_id")}
        with open(os.path.join(RAW, f"{label}_ledger.ndjson"), "w") as f:
            for l in ledger_lines:
                if l.get("id") in mids:
                    f.write(json.dumps(l) + "\n")
    return per_cli


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    global run_log
    os.makedirs(RAW, exist_ok=True)
    os.makedirs(SCRATCH, exist_ok=True)
    run_log = open(os.path.join(RAW, "run.log"), "w")
    legs = build_legs()
    active = [l for l in legs if not l["skipped"]]
    if not active:
        print(json.dumps({"verdict": "FAIL: no agent CLIs found"}))
        return 1
    write_vendor_configs(legs)
    daemon = None
    t_run0 = time.time()
    try:
        launch_tuis(active)
        log(f"launched: {[(l['cli'], l['pane'], l['launch_cmd_seen']) for l in active]}")

        # Launch phase: composer ready per pane, modals dismissed here.
        for leg in active:
            modlog = open(os.path.join(RAW, f"{leg['cli']}_launch_modals.log"), "a")
            outcome, cap = wait_composer(leg["cli"], leg["pane"], modlog)
            leg["launch_outcome"] = outcome
            if outcome == "quota":
                leg["parked"] = True
                leg["stop_reason"] = "vendor quota exhausted at launch"
            elif outcome != "ready":
                leg["stop_reason"] = f"composer never ready ({outcome})"
                with open(os.path.join(RAW, f"{leg['cli']}_launch_fail.txt"), "w") as f:
                    f.write(cap)
            log(f"[{leg['cli']}] launch: {outcome}")
        time.sleep(2)

        runnable = [l for l in active if l["launch_outcome"] == "ready"]
        if not runnable:
            raise RuntimeError("no leg reached a ready composer")
        daemon, ctl, sock_path = boot_daemon(live_pane_count())
        log(f"daemon up, boot_id {ctl.hello.get('boot_id')}")

        # Label panes and verify manifest binding before spending tokens.
        for leg in runnable:
            r = ctl.request("pane.label", {"target": leg["pane"], "label": leg["cli"]})
            if r.get("error"):
                raise RuntimeError(f"pane.label {leg['cli']}: {r['error']}")
        st = ctl.request("status", {})["result"]
        for leg in list(runnable):
            pane = next(p for p in st["sessions"][0]["panes"]
                        if p["pane_id"] == leg["pane"])
            leg["manifest_bound"] = pane.get("manifest")
            leg["current_command"] = pane["current_command"]
            if pane.get("manifest") != leg["cli"]:
                leg["stop_reason"] = (f"manifest not bound: current_command="
                                      f"{pane['current_command']} manifest={pane.get('manifest')}")
                runnable = [l for l in runnable if l is not leg]
                continue
            det = ctl.request("pane.read",
                              {"target": leg["cli"], "source": "detection"})
            leg["initial_state"] = (det.get("result") or {}).get("detection", {}).get("state")
        log("bindings: " + json.dumps(
            {l["cli"]: {"cmd": l.get("current_command"),
                        "manifest": l.get("manifest_bound"),
                        "state": l.get("initial_state")} for l in active}))

        threads = []
        for leg in runnable:
            t = threading.Thread(target=run_leg, args=(leg, sock_path),
                                 name=f"leg-{leg['cli']}", daemon=True)
            t.start()
            threads.append(t)
        for t in threads:
            t.join()
        time.sleep(3)  # let late hook upgrades land in the ledger
        # Duplicate scan must run while the panes still exist.
        for leg in runnable:
            scan_duplicates(leg)
    finally:
        if daemon:
            daemon.terminate()
            try:
                daemon.wait(timeout=10)
            except subprocess.TimeoutExpired:
                daemon.kill()
        tmux("kill-server")
        for name in ("daemon.log",):
            src = os.path.join(SCRATCH, name)
            if os.path.exists(src):
                shutil.copy(src, os.path.join(RAW, name))
        ledger_src = os.path.join(HOME, "ledger", f"{SESSION}.ndjson")
        if os.path.exists(ledger_src):
            shutil.copy(ledger_src, os.path.join(RAW, "ledger.ndjson"))

    ledger_lines = []
    ledger_copy = os.path.join(RAW, "ledger.ndjson")
    if os.path.exists(ledger_copy):
        with open(ledger_copy) as f:
            for line in f:
                if line.strip():
                    ledger_lines.append(json.loads(line))

    # Detach evidence from the daemon's own log: "connection lost" is a
    # mid-run control drop (the F22 failure mode), "did not exit" is the
    # shutdown shadow of the same bug. Both must be zero on the fixed tree.
    detach_events, shutdown_wedges = 0, 0
    dlog_path = os.path.join(RAW, "daemon.log")
    if os.path.exists(dlog_path):
        with open(dlog_path) as f:
            for line in f:
                if "connection lost" in line:
                    detach_events += 1
                if "did not exit" in line:
                    shutdown_wedges += 1

    per_cli = analyze(legs, ledger_lines)
    losses = sum(c["lost"] for c in per_cli.values())
    duplicates = sum(c.get("duplicates", 0) for c in per_cli.values())
    incomplete = [cli for cli, c in per_cli.items()
                  if not c.get("skipped") and not c.get("parked")
                  and c["sent"] < TARGET]
    # Amendment 2 forbids double-paste, so a detected duplicate fails the
    # gate alongside loss and an incomplete leg.
    verdict = ("PASS" if losses == 0 and duplicates == 0 and not incomplete
               else "FAIL")
    summary = {
        "verdict": verdict,
        "target_per_cli": TARGET,
        "wall_s": round(time.time() - t_run0, 1),
        "detach_events": detach_events,
        "shutdown_wedges": shutdown_wedges,
        "environment": {
            "tmux_socket": SOCK,
            "legs": {l["cli"]: {k: l.get(k) for k in
                                ("launch_outcome", "current_command",
                                 "manifest_bound", "initial_state")}
                     for l in legs if not l.get("skipped")},
        },
        "per_cli": per_cli,
    }
    with open(os.path.join(RAW, "summary.json"), "w") as f:
        json.dump(summary, f, indent=1)
    shutil.rmtree(SCRATCH, ignore_errors=True)
    log(json.dumps(summary, indent=1))
    return 0 if verdict == "PASS" else 1


if __name__ == "__main__":
    sys.exit(main())
