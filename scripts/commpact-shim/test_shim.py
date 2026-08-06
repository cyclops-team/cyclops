#!/usr/bin/env python3
"""commPact shim tests: verb mapping against a canned cyclops daemon.

Runs the real cyclops binary (target/debug) against a canned NDJSON daemon
on a sandbox socket, drives the shim exactly the way v1 agents call
commPact, and asserts:

  - each mapped verb reaches the daemon as the right method and params
  - unshimmed verbs refuse cleanly without touching the daemon
  - the deprecation note prints once per day (stamp file), stderr only
  - install.sh refuses without CYCLOPS_CUTOVER_ACK=yes and installs
    correctly into a sandbox HOME
  - nothing outside the sandbox is touched: the real ~/.commPact is
    snapshotted by mtime before and after (the M2 brief's assertion)

No tmux, no network, no real home. Everything lives in a sandbox under the
scratch root: CYCLOPS_TEST_TMP if set, else /private/tmp on macOS, else the
system temp dir (see scratch_root below and F24).
Run: python3 scripts/commpact-shim/test_shim.py
"""

import hashlib
import json
import os
import pathlib
import shutil
import socket
import subprocess
import sys
import tempfile
import threading

HERE = pathlib.Path(__file__).resolve().parent
REPO = HERE.parents[1]
SHIM = HERE / "commPact"
INSTALL = HERE / "install.sh"
CYCLOPS = REPO / "target" / "debug" / "cyclops"

# cyclops_proto::scratch::SCRATCH_ENV. One name, both languages.
SCRATCH_ENV = "CYCLOPS_TEST_TMP"

# Same canned status shape src/cyclops/tests/e2e.rs uses, so the human
# status rendering (list, doctor) parses it.
CANNED_STATUS = {
    "daemon_version": "0.1.0",
    "proto": 1,
    "boot_id": "b-shim",
    "uptime_ms": 120_000,
    "tmux_version": "3.6a",
    "sessions": [{
        "name": "main",
        "attached": True,
        "panes": [{
            "pane_id": "%1", "window_id": "@1", "window_name": "agents",
            "agent": "reviewer", "manifest": "claude",
            "title": "reviewer", "current_command": "claude",
            "dead": False, "in_mode": False, "width": 120, "height": 40,
            "state": "idle",
        }],
    }],
}

CHECKS = 0


def ok(name):
    global CHECKS
    CHECKS += 1
    print(f"ok {CHECKS} - {name}")


def fail(name, detail):
    print(f"FAIL - {name}\n{detail}", file=sys.stderr)
    sys.exit(1)


def expect(cond, name, detail=""):
    if not cond:
        fail(name, detail)
    ok(name)


class CannedDaemon:
    """Accepts NDJSON connections like cyclopsd: hello line first, then one
    reply per request line. Records every request for assertions."""

    def __init__(self, sock_path):
        self.requests = []
        self.lock = threading.Lock()
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(str(sock_path))
        self.listener.listen(8)
        threading.Thread(target=self._serve, daemon=True).start()

    def _serve(self):
        while True:
            try:
                conn, _ = self.listener.accept()
            except OSError:
                return
            threading.Thread(target=self._handle, args=(conn,), daemon=True).start()

    def _handle(self, conn):
        f = conn.makefile("rwb")
        hello = {"cyclops": "0.1.0", "proto": 1, "boot_id": "b-shim"}
        f.write((json.dumps(hello) + "\n").encode())
        f.flush()
        for line in f:
            req = json.loads(line)
            with self.lock:
                self.requests.append(req)
            resp = {"id": req["id"], "result": self._result(req)}
            f.write((json.dumps(resp) + "\n").encode())
            f.flush()
        conn.close()

    def _result(self, req):
        m = req["method"]
        p = req.get("params") or {}
        if m == "ping":
            return {"pong": True}
        if m == "status":
            return CANNED_STATUS
        if m == "pane.read":
            return {"target": p.get("target", ""), "pane_id": "%1", "text": "alpha\nbeta"}
        if m == "msg.send":
            to = p["to"][0]
            if p.get("subject") == "park":
                return {"msg_id": "m-park", "seq": 2, "deliveries": [{
                    "to": to, "state": "parked_blocked_quota", "note": "resets in 135h",
                }]}
            return {"msg_id": "m-shim01", "seq": 1, "deliveries": [{
                "to": to, "state": "delivered_verified",
            }]}
        return {}

    def count(self):
        with self.lock:
            return len(self.requests)

    def last(self):
        with self.lock:
            return self.requests[-1]

    def close(self):
        self.listener.close()


def snapshot(root):
    """Recursive (path -> mtime_ns, size) map, directories included."""
    root = pathlib.Path(root)
    if not root.exists():
        return None
    snap = {}
    for dirpath, _dirnames, filenames in os.walk(root):
        snap[dirpath] = os.stat(dirpath).st_mtime_ns
        for n in filenames:
            p = os.path.join(dirpath, n)
            st = os.stat(p)
            snap[p] = (st.st_mtime_ns, st.st_size)
    return snap


def scratch_root():
    """Where throwaway test state goes. None means the system temp dir.

    The same rule as cyclops_proto::scratch::scratch_root, restated here
    because Python cannot call it: CYCLOPS_TEST_TMP wins, then /private/tmp
    on macOS (short, so a unix socket path fits the ~104 byte cap), then
    the system temp dir. Hardcoding /private/tmp broke every Linux run
    (F24), and ignoring the override made this suite the one place the
    relocated-root CI leg could not reach. Change it with scratch.rs.
    """
    override = os.environ.get(SCRATCH_ENV)
    if override:
        return override
    if sys.platform == "darwin":
        return "/private/tmp"
    return None


def main():
    if not CYCLOPS.exists():
        subprocess.run(["cargo", "build", "-p", "cyclops"], cwd=REPO, check=True)

    # Both scripts must parse before anything runs.
    for script in (SHIM, INSTALL):
        r = subprocess.run(["bash", "-n", str(script)], capture_output=True, text=True)
        expect(r.returncode == 0, f"bash -n {script.name}", r.stderr)
    if shutil.which("shellcheck"):
        for script in (SHIM, INSTALL):
            r = subprocess.run(["shellcheck", str(script)], capture_output=True, text=True)
            expect(r.returncode == 0, f"shellcheck {script.name}", r.stdout + r.stderr)
    else:
        print("# shellcheck not installed, skipping")

    real_commpact = pathlib.Path.home() / ".commPact"
    before = snapshot(real_commpact)

    root = scratch_root()
    sb = pathlib.Path(tempfile.mkdtemp(prefix="cyc-shim-", dir=root))
    # The sandbox has to land where the rule says, or CI running with the
    # root relocated proves nothing about this suite.
    expect(root is None or str(sb).startswith(str(root)),
           f"sandbox lands under the scratch root {root}", str(sb))
    home = sb / "home"
    ch = sb / "ch"
    home.mkdir()
    ch.mkdir()

    def env(extra=None):
        e = os.environ.copy()
        e["HOME"] = str(home)
        e["CYCLOPS_HOME"] = str(ch)
        e["CYCLOPS_BIN"] = str(CYCLOPS)
        e.pop("CYCLOPS_AGENT", None)
        e.pop("TMUX_PANE", None)
        e.pop("CYCLOPS_CUTOVER_ACK", None)
        if extra:
            e.update(extra)
        return e

    def shim(args, stdin=None, extra_env=None):
        return subprocess.run(
            [str(SHIM)] + args, input=stdin, capture_output=True,
            text=True, env=env(extra_env),
        )

    daemon = CannedDaemon(ch / "sock")
    try:
        # send: the exact COORDINATION.md pattern. First shim call, so the
        # deprecation note must be the only stderr line.
        r = shim(["send", "claude", "--json", "--subject", "Review request",
                  "--body-file", "-"], stdin="Body text only\n")
        expect(r.returncode == 0, "send --json exits 0", r.stderr)
        expect("delivered_verified" in r.stdout and "m-shim01" in r.stdout,
               "send --json prints the raw receipt", r.stdout)
        expect("deprecated" in r.stderr and "\n" not in r.stderr.strip(),
               "first call prints the one-line deprecation note", r.stderr)
        req = daemon.last()
        expect(req["method"] == "msg.send"
               and req["params"]["to"] == ["claude"]
               and req["params"]["subject"] == "Review request"
               and req["params"]["body"] == "Body text only"
               and req["params"]["fyi"] is False,
               "send maps to msg.send with v1 params", json.dumps(req))

        # Stamp: the note does not repeat within the day.
        stamp = ch / "commpact-shim.stamp"
        expect(stamp.exists(), "stamp file written under CYCLOPS_HOME")
        r = shim(["send", "claude", "--subject", "park", "--body-file", "-"],
                 stdin="please")
        expect("deprecated" not in r.stderr, "note not repeated same day", r.stderr)
        expect(r.returncode == 1 and "parked" in r.stdout
               and "out of quota" in r.stderr,
               "parked receipt passes exit 1 and the reset copy through",
               f"rc={r.returncode} out={r.stdout} err={r.stderr}")

        # Human path without --json renders the badge.
        r = shim(["send", "claude", "--subject", "human", "--body-file", "-"],
                 stdin="b")
        expect(r.returncode == 0 and "delivered" in r.stdout,
               "send without --json renders the badge", r.stdout)

        # Ignored v1 tuning flags: noted, then delivered anyway.
        n = daemon.count()
        r = shim(["send", "claude", "--json", "--subject", "s", "--body-file", "-",
                  "--max-bytes", "4096", "--lock-timeout-ms", "2000",
                  "--verify", "marker-hash"], stdin="b")
        expect(r.returncode == 0 and r.stderr.count("ignored") == 3,
               "tuning flags ignored with one note each",
               f"rc={r.returncode} err={r.stderr}")
        expect(daemon.count() == n + 1, "ignored flags still deliver")

        # Refused v1 flags: usage error, daemon never contacted.
        n = daemon.count()
        r = shim(["send", "claude", "--json", "--subject", "s", "--body-file", "-",
                  "--no-submit"], stdin="b")
        expect(r.returncode == 2 and "no cyclops equivalent" in r.stderr,
               "--no-submit refused with exit 2", r.stderr)
        r = shim(["send", "claude", "--json", "--subject", "s", "--body-file", "-",
                  "--expect-label", "claude"], stdin="b")
        expect(r.returncode == 2, "--expect-label refused with exit 2", r.stderr)
        expect(daemon.count() == n, "refused flags never reach the daemon")

        # Empty body refused like v1.
        r = shim(["send", "claude", "--json", "--subject", "s", "--body-file", "-"],
                 stdin="")
        expect(r.returncode == 2 and "body must not be empty" in r.stderr,
               "empty body refused like v1", r.stderr)

        # read with and without the v1 default of 50 lines.
        r = shim(["read", "claude", "30"])
        expect(r.returncode == 0 and r.stdout == "alpha\nbeta\n",
               "read prints pane text verbatim", r.stdout)
        req = daemon.last()
        expect(req["method"] == "pane.read"
               and req["params"]["target"] == "claude"
               and req["params"]["lines"] == 30
               and req["params"]["source"] == "recent",
               "read maps to pane.read source=recent", json.dumps(req))
        shim(["read", "claude"])
        expect(daemon.last()["params"]["lines"] == 50,
               "read defaults to 50 lines like v1")

        # list -> status, resolve -> pane_id, doctor -> ping + status.
        r = shim(["list"])
        expect(r.returncode == 0 and "reviewer" in r.stdout
               and daemon.last()["method"] == "status",
               "list maps to cyclops status", r.stdout + r.stderr)
        r = shim(["resolve", "claude"])
        expect(r.returncode == 0 and r.stdout == "%1\n",
               "resolve prints the pane id", r.stdout + r.stderr)
        n = daemon.count()
        r = shim(["doctor"])
        expect(r.returncode == 0 and "doctor" in r.stdout
               and daemon.count() == n + 2,
               "doctor runs ping then status", r.stdout + r.stderr)

        # Local verbs need no daemon.
        n = daemon.count()
        r = shim(["id"], extra_env={"TMUX_PANE": "%7"})
        expect(r.returncode == 0 and r.stdout == "%7\n", "id prints TMUX_PANE")
        r = shim(["id"])
        expect(r.returncode == 1 and "TMUX_PANE" in r.stderr,
               "id outside tmux errors like v1", r.stderr)
        want = hashlib.sha256(b"abc").hexdigest()[:12]
        r = shim(["hash", "abc"])
        expect(r.returncode == 0 and r.stdout.strip() == want,
               "hash matches v1 sha256_12", r.stdout)
        r = shim(["version"])
        expect(r.returncode == 0 and "shim" in r.stdout, "version names the shim")
        r = shim([])
        expect(r.returncode == 0 and "commPact" in r.stdout, "bare usage exits 0")
        expect(daemon.count() == n, "local verbs never touch the daemon")

        # Unshimmed verbs refuse honestly and never reach the daemon.
        n = daemon.count()
        for args in (["type", "claude", "hi"], ["keys", "claude", "Enter"],
                     ["message", "claude", "yo"], ["name", "claude", "claude"]):
            r = shim(args)
            expect(r.returncode == 2 and "not shimmed" in r.stderr,
                   f"{args[0]} refused with exit 2", r.stderr)
        expect(daemon.count() == n, "unshimmed verbs never reach the daemon")

        # install.sh, entirely inside the sandbox HOME. CYCLOPS_BIN points
        # nowhere so the installer takes its warning branch and can never
        # talk to a real daemon.
        bin_dir = home / ".commPact" / "bin"
        bin_dir.mkdir(parents=True)
        target = bin_dir / "commPact"
        backup = bin_dir / "commPact.v1.bak"
        dummy = "#!/bin/bash\necho v1\n"
        target.write_text(dummy)
        target.chmod(0o755)
        ienv = {"CYCLOPS_BIN": "/nonexistent/cyclops"}

        r = subprocess.run([str(INSTALL)], capture_output=True, text=True,
                           env=env(ienv))
        expect(r.returncode == 1 and "CYCLOPS_CUTOVER_ACK" in r.stderr,
               "install refuses without the ack", r.stderr)
        expect(target.is_file() and not target.is_symlink()
               and not backup.exists(),
               "refused install changed nothing")

        r = subprocess.run([str(INSTALL)], capture_output=True, text=True,
                           env=env({**ienv, "CYCLOPS_CUTOVER_ACK": "yes"}))
        expect(r.returncode == 0, "acked install succeeds", r.stderr)
        expect(target.is_symlink() and os.readlink(target) == str(SHIM),
               "target is now a symlink to the shim")
        expect(backup.read_text() == dummy, "backup is the original binary")
        expect("rollback" in r.stdout, "install prints the rollback", r.stdout)

        r = subprocess.run([str(INSTALL)], capture_output=True, text=True,
                           env=env({**ienv, "CYCLOPS_CUTOVER_ACK": "yes"}))
        expect(r.returncode == 0 and "already installed" in r.stdout,
               "reinstall is a no-op", r.stdout + r.stderr)

        target.unlink()
        target.write_text(dummy)
        r = subprocess.run([str(INSTALL)], capture_output=True, text=True,
                           env=env({**ienv, "CYCLOPS_CUTOVER_ACK": "yes"}))
        expect(r.returncode == 1 and "backup already exists" in r.stderr,
               "existing backup is never overwritten", r.stderr)
    finally:
        daemon.close()
        shutil.rmtree(sb, ignore_errors=True)

    after = snapshot(real_commpact)
    if before is None:
        print("# no real ~/.commPact on this machine, mtime check skipped")
    else:
        expect(before == after, "real ~/.commPact untouched (mtime + size)",
               "diff: " + json.dumps(sorted(
                   set(before.items()) ^ set(after.items()), key=str)[:10], default=str))

    print(f"\nall {CHECKS} checks passed")


if __name__ == "__main__":
    main()
