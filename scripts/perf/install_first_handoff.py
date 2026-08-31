#!/usr/bin/env python3
"""Measure a staged source install through one durable two-agent handoff.

This is scheduled and release evidence, not an ordinary correctness check. It
uses the public source installer against a fresh private prefix, home, tmux
server, and Cargo target directory. The repository checkout and Cargo registry
may already be present, so this is deliberately a staged local-source
measurement, not a claim about a network download or a clean operating system.

The machine-readable line at the end is consumed by scripts/ci-performance.py.
It contains no message body or user state and is safe to retain as a CI
artifact.
"""

from __future__ import annotations

import collections
import json
import os
import platform
import re
import shutil
import shlex
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Callable


MEASUREMENT_PREFIX = "CYCLOPS_INSTALL_FIRST_HANDOFF_JSON="
SETUP_START = "== setting up "
SETUP_COMPLETE = "✔ cyclops is set up"
HANDOFF_BODY = "The installed pair reached durable messaging."
FIXTURE_MANIFEST = "install-perf"


class WorkloadError(RuntimeError):
    """A fixture boundary or durable-handoff assertion failed."""


def confirms_fixture_manifest(response: object) -> bool:
    """Accept only the manifest confirmation the fixture asked the daemon to persist."""

    return isinstance(response, dict) and response.get("manifest") == FIXTURE_MANIFEST


def repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def command_text(command: list[str], *, cwd: Path, env: dict[str, str]) -> str:
    proc = subprocess.run(command, cwd=cwd, env=env, capture_output=True, text=True)
    if proc.returncode:
        detail = (proc.stdout or proc.stderr).strip()
        raise WorkloadError(f"{' '.join(command)} failed ({proc.returncode}): {detail}")
    return proc.stdout.strip()


def scratch_parent() -> Path:
    configured = os.environ.get("CYCLOPS_TEST_TMP")
    if configured:
        return Path(configured)
    if Path("/private/tmp").is_dir():
        return Path("/private/tmp")
    return Path(tempfile.gettempdir())


def summary(start_ns: int, end_ns: int, boundary: str) -> dict[str, object]:
    if end_ns < start_ns:
        raise WorkloadError(f"clock moved backward while measuring {boundary}")
    duration = (end_ns - start_ns) / 1_000_000_000
    # One clean staged install per retained run is intentional: three source
    # builds would turn a daily evidence lane into a build-stress test. With
    # n=1, p50, p95, and max are all the observed sample, not confidence bounds.
    return {
        "boundary": boundary,
        "samples_seconds": [round(duration, 6)],
        "sample_count": 1,
        "p50_seconds": round(duration, 6),
        "p95_seconds": round(duration, 6),
        "max_seconds": round(duration, 6),
    }


def wait_for(
    name: str,
    condition: Callable[[], bool],
    *,
    timeout_seconds: float = 20.0,
) -> None:
    """Wait only for isolated fixture evidence, never for product polling."""

    deadline = time.monotonic() + timeout_seconds
    while time.monotonic() < deadline:
        if condition():
            return
        time.sleep(0.05)
    raise WorkloadError(f"timed out waiting for {name}")


def write_cargo_wrapper(path: Path) -> None:
    """Record the installer's real `cargo build` without changing the installer."""

    path.write_text(
        """#!/usr/bin/env python3
import json
import os
import subprocess
import sys
import time

real = os.environ[\"CYCLOPS_INSTALL_PERF_REAL_CARGO\"]
trace = os.environ[\"CYCLOPS_INSTALL_PERF_CARGO_TRACE\"]
is_build = len(sys.argv) > 1 and sys.argv[1] == \"build\"
started = time.monotonic_ns()
completed = subprocess.run([real, *sys.argv[1:]], check=False)
ended = time.monotonic_ns()
if is_build:
    with open(trace, \"a\", encoding=\"utf-8\") as output:
        output.write(json.dumps({\"start_ns\": started, \"end_ns\": ended, \"status\": completed.returncode}) + \"\\n\")
raise SystemExit(completed.returncode)
""",
        encoding="utf-8",
    )
    path.chmod(0o755)


def run_installer(
    repo: Path,
    root: Path,
    env: dict[str, str],
) -> tuple[int, int, int, int, int, int, collections.deque[str]]:
    """Run the public installer and bracket its build and setup output."""

    started = time.monotonic_ns()
    process = subprocess.Popen(
        ["sh", str(repo / "scripts/install.sh"), "--prefix", env["CYCLOPS_INSTALL_PERF_PREFIX"], "--no-path"],
        cwd=repo,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
    )
    assert process.stdout is not None
    tail: collections.deque[str] = collections.deque(maxlen=80)
    setup_started: int | None = None
    setup_completed: int | None = None
    for raw_line in process.stdout:
        line = raw_line.rstrip("\n")
        tail.append(line)
        now = time.monotonic_ns()
        if SETUP_START in line and setup_started is None:
            setup_started = now
        if SETUP_COMPLETE in line and setup_completed is None:
            setup_completed = now
    status = process.wait()
    ended = time.monotonic_ns()
    if status:
        diagnostic = "\n".join(tail)
        raise WorkloadError(f"public source installer exited {status}\n{diagnostic}")
    if setup_started is None or setup_completed is None:
        diagnostic = "\n".join(tail)
        raise WorkloadError(f"installer did not expose setup boundaries\n{diagnostic}")

    trace = root / "cargo-build.jsonl"
    try:
        entries = [json.loads(line) for line in trace.read_text(encoding="utf-8").splitlines()]
    except FileNotFoundError as error:
        raise WorkloadError("installer did not invoke the Cargo timing wrapper") from error
    if len(entries) != 1 or entries[0].get("status") != 0:
        raise WorkloadError(f"expected one successful installer cargo build, got {entries!r}")
    build = entries[0]
    return (
        started,
        ended,
        int(build["start_ns"]),
        int(build["end_ns"]),
        setup_started,
        setup_completed,
        tail,
    )


def tmux(env: dict[str, str], *args: str) -> str:
    return command_text(["tmux", "-u", *args], cwd=repo_root(), env=env)


def status_json(client: Path, repo: Path, env: dict[str, str]) -> dict[str, object] | None:
    proc = subprocess.run(
        [str(client), "--json", "status"],
        cwd=repo,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    if proc.returncode:
        return None
    try:
        return json.loads(proc.stdout)
    except json.JSONDecodeError:
        return None


def as_panes(status: dict[str, object]) -> list[dict[str, object]]:
    panes: list[dict[str, object]] = []
    for session in status.get("sessions", []):
        if isinstance(session, dict):
            panes.extend(pane for pane in session.get("panes", []) if isinstance(pane, dict))
    return panes


def write_fixture_manifest(home: Path, repo: Path) -> None:
    skill = repo / "skills/cyclops/SKILL.md"
    if not skill.is_file():
        raise WorkloadError(f"shipped Cyclops skill is missing: {skill}")
    manifest = f'''[agent]
id = "{FIXTURE_MANIFEST}"
display_name = "Install performance fixture"
process_names = ["cycagent"]
argv_basenames = ["cycagent"]
launch = "cat"

[messaging]
mailbox_capability_file = {json.dumps(str(skill))}

[[rule]]
id = "fixture_idle"
state = "idle"
priority = 70
region = "bottom_non_empty_lines(4)"
lifecycle_evidence = true
regex = ['^']
'''
    (home / f"manifests/{FIXTURE_MANIFEST}.toml").write_text(manifest, encoding="utf-8")


def start_agent(
    label: str,
    pane: str,
    root: Path,
    fixture: Path,
    env: dict[str, str],
) -> Path:
    control = root / f"control.{label}"
    ready = root / f"ready.{label}"
    control.unlink(missing_ok=True)
    ready.unlink(missing_ok=True)
    os.mkfifo(control)
    command = " ".join(shlex.quote(str(value)) for value in (fixture, control, ready))
    tmux(env, "respawn-pane", "-k", "-t", pane, command)
    wait_for(f"{label} fixture agent", ready.is_file)
    return control


def run_as_agent(
    label: str,
    control: Path,
    command: list[str],
    root: Path,
    env: dict[str, str],
) -> str:
    result = root / f"result.{label}"
    exit_code = root / f"exit.{label}"
    done = root / f"done.{label}"
    for path in (result, exit_code, done):
        path.unlink(missing_ok=True)
    env_assignments = " ".join(
        f"{key}={shlex.quote(env[key])}" for key in ("HOME", "CYCLOPS_HOME", "TMUX_TMPDIR", "PATH")
    )
    script = (
        f"{env_assignments} {shlex.join(command)} > {shlex.quote(str(result))} 2>&1; "
        f"code=$?; printf '%s' \"$code\" > {shlex.quote(str(exit_code))}; "
        f": > {shlex.quote(str(done))}"
    )
    with control.open("w", encoding="utf-8") as fifo:
        fifo.write(f"run\t{script}\n")
    wait_for(f"{label} agent command", done.is_file)
    output = result.read_text(encoding="utf-8")
    if exit_code.read_text(encoding="utf-8") != "0":
        raise WorkloadError(f"agent command failed: {shlex.join(command)}\n{output}")
    return output


def name_agent(
    client: Path,
    pane: str,
    label: str,
    repo: Path,
    env: dict[str, str],
) -> bool:
    """Bind one known synthetic fixture through the public naming command."""

    last_error = ""
    manifest_pinned = False

    def named() -> bool:
        nonlocal last_error, manifest_pinned
        proc = subprocess.run(
            [
                str(client),
                "--json",
                "name",
                pane,
                label,
                "--manifest",
                FIXTURE_MANIFEST,
            ],
            cwd=repo,
            env=env,
            capture_output=True,
            text=True,
        )
        if proc.returncode:
            last_error = (proc.stdout or proc.stderr).strip()
            return False
        try:
            response = json.loads(proc.stdout)
        except json.JSONDecodeError:
            last_error = f"name returned invalid JSON: {proc.stdout.strip()}"
            return False
        if not confirms_fixture_manifest(response):
            last_error = f"name did not confirm {FIXTURE_MANIFEST!r}: {response!r}"
            return False
        manifest_pinned = True
        return True

    try:
        wait_for(f"daemon to name {label}", named)
    except WorkloadError as error:
        raise WorkloadError(f"{error}: {last_error}") from error
    return manifest_pinned


def stop_daemon(process: subprocess.Popen[str] | None) -> None:
    if process is None or process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=10)


def teardown_tmux(repo: Path, env: dict[str, str]) -> None:
    helper = repo / "tests/e2e/lib/lib.sh"
    subprocess.run(
        ["bash", "-c", '. "$1"; cyc_tmux_teardown default', "bash", str(helper)],
        cwd=repo,
        env=env,
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def fixture_name_response_requires_the_requested_manifest() -> None:
    """A successful name command is insufficient without the exact pin confirmation."""

    assert confirms_fixture_manifest({"manifest": FIXTURE_MANIFEST})
    assert not confirms_fixture_manifest({"manifest": "other-fixture"})
    assert not confirms_fixture_manifest({"pane_id": "%1"})


def selftest() -> int:
    """Run the no-daemon regression checks for the performance fixture."""

    fixture_name_response_requires_the_requested_manifest()
    print("install first handoff fixture self-test passed")
    return 0


def main() -> int:
    repo = repo_root()
    root = Path(tempfile.mkdtemp(prefix="cyc-install-handoff.", dir=scratch_parent()))
    daemon: subprocess.Popen[str] | None = None
    daemon_log = None
    cleanup_env = os.environ.copy()
    cleanup_env.pop("TMUX", None)
    cleanup_env.pop("TMUX_PANE", None)
    cleanup_env["TMUX_TMPDIR"] = str(root / "tmux")
    try:
        home = root / "home"
        prefix = root / "prefix"
        cyclops_home = home / ".cyclops"
        tmux_root = root / "tmux"
        empty_dir = root / "empty"
        wrapper_dir = root / "bin"
        for directory in (home, prefix, tmux_root, empty_dir, wrapper_dir, root / "tmp"):
            directory.mkdir(parents=True, exist_ok=True)

        caller_home = Path(os.environ.get("HOME", str(Path.home())))
        real_cargo = shutil.which("cargo")
        if real_cargo is None:
            raise WorkloadError("cargo is required for the staged source installer")
        cargo_wrapper = wrapper_dir / "cargo"
        write_cargo_wrapper(cargo_wrapper)
        env = cleanup_env.copy()
        env.update(
            {
                "HOME": str(home),
                "CYCLOPS_HOME": str(cyclops_home),
                "TMUX_TMPDIR": str(tmux_root),
                "TMPDIR": str(root / "tmp"),
                "SHELL": "/bin/sh",
                "NO_COLOR": "1",
                "CARGO_TARGET_DIR": str(root / "cargo-target"),
                "CYCLOPS_INSTALL_PERF_PREFIX": str(prefix),
                "CYCLOPS_INSTALL_PERF_REAL_CARGO": real_cargo,
                "CYCLOPS_INSTALL_PERF_CARGO_TRACE": str(root / "cargo-build.jsonl"),
                "PATH": f"{wrapper_dir}:{prefix}:{os.environ.get('PATH', '')}",
                "RUSTUP_HOME": os.environ.get("RUSTUP_HOME", str(caller_home / ".rustup")),
                "CARGO_HOME": os.environ.get("CARGO_HOME", str(caller_home / ".cargo")),
            }
        )
        if "RUSTUP_TOOLCHAIN" in os.environ:
            env["RUSTUP_TOOLCHAIN"] = os.environ["RUSTUP_TOOLCHAIN"]

        (
            install_started,
            install_finished,
            build_started,
            build_finished,
            setup_started,
            setup_finished,
            _,
        ) = run_installer(repo, root, env)
        client = prefix / "cyclops"
        daemon_binary = prefix / "cyclopsd"
        if not client.is_file() or not daemon_binary.is_file():
            raise WorkloadError("public installer did not leave both selected binaries")
        client_version = command_text([str(client), "--version"], cwd=empty_dir, env=env)
        daemon_version = command_text([str(daemon_binary), "--version"], cwd=empty_dir, env=env)
        if client_version.removeprefix("cyclops ") != daemon_version.removeprefix("cyclopsd "):
            raise WorkloadError(f"installed pair versions differ: {client_version}; {daemon_version}")

        # The public installer emits these two setup markers. Recording them
        # from its stream brackets setup without adding a private installer
        # flag or changing the supported source-install behavior.
        if not (install_started <= build_started <= build_finished <= setup_started <= setup_finished <= install_finished):
            raise WorkloadError("installer phase boundaries were not monotonic")

        fixture_setup_started = time.monotonic_ns()
        write_fixture_manifest(cyclops_home, repo)
        fixture = root / "cycagent"
        command_text(
            ["rustc", "--edition=2021", "-Dwarnings", str(repo / "tests/e2e/parity_agent.rs"), "-o", str(fixture)],
            cwd=repo,
            env=env,
        )
        fixture_setup_finished = time.monotonic_ns()

        daemon_log = (root / "daemon.log").open("w", encoding="utf-8")
        daemon_started = time.monotonic_ns()
        daemon = subprocess.Popen(
            [str(daemon_binary)],
            cwd=empty_dir,
            env=env,
            stdout=daemon_log,
            stderr=subprocess.STDOUT,
            text=True,
        )

        def daemon_ready() -> bool:
            if daemon is not None and daemon.poll() is not None:
                raise WorkloadError(f"installed daemon exited {daemon.returncode}")
            return (cyclops_home / "sock").is_socket() and status_json(client, repo, env) is not None

        wait_for("installed daemon readiness", daemon_ready)
        daemon_ready_at = time.monotonic_ns()

        adoption_started = time.monotonic_ns()
        command_text(
            [str(client), "start", "--preset", "duo", "--no-daemon", "--plain"],
            cwd=empty_dir,
            env=env,
        )

        def session_adopted() -> bool:
            status = status_json(client, repo, env)
            return bool(status and any(session.get("attached") for session in status.get("sessions", []) if isinstance(session, dict)))

        wait_for("daemon session adoption", session_adopted)
        adoption_finished = time.monotonic_ns()
        panes = tmux(env, "list-panes", "-t", "main", "-F", "#{pane_id}").splitlines()
        if len(panes) != 2:
            raise WorkloadError(f"duo preset did not expose two panes: {panes!r}")

        def daemon_sees_duo_panes() -> bool:
            status = status_json(client, repo, env)
            if status is None:
                return False
            known = {str(pane.get("pane_id")) for pane in as_panes(status)}
            return set(panes).issubset(known)

        wait_for("daemon pane discovery", daemon_sees_duo_panes)

        detection_started = time.monotonic_ns()
        sender_control = start_agent("implementer", panes[0], root, fixture, env)
        reviewer_control = start_agent("reviewer", panes[1], root, fixture, env)
        wait_for("daemon pane discovery after fixture launch", daemon_sees_duo_panes)
        implementer_manifest_pinned = name_agent(
            client, panes[0], "implementer", empty_dir, env
        )
        reviewer_manifest_pinned = name_agent(
            client, panes[1], "reviewer", empty_dir, env
        )

        def both_agents_detected() -> bool:
            status = status_json(client, repo, env)
            if status is None:
                return False
            known = {
                str(pane.get("pane_id")): str(pane.get("manifest"))
                for pane in as_panes(status)
            }
            return all(known.get(pane) == FIXTURE_MANIFEST for pane in panes)

        wait_for("both explicit fixture manifest bindings", both_agents_detected)
        detection_finished = time.monotonic_ns()

        send_started = time.monotonic_ns()
        sent = run_as_agent(
            "implementer",
            sender_control,
            [
                str(client),
                "send",
                "reviewer",
                "--subject",
                "Installed performance handoff",
                "--summary",
                "Claim the installed performance handoff. Record its durable acceptance.",
                "--body",
                HANDOFF_BODY,
                "--client-key",
                "install-first-handoff-performance",
                "--plain",
            ],
            root,
            env,
        )
        accepted = re.search(r"^accepted (m-[0-9a-f]{32})$", sent, flags=re.MULTILINE)
        if accepted is None:
            raise WorkloadError(f"installed sender did not report durable acceptance\n{sent}")
        message_id = accepted.group(1)
        journals = list((cyclops_home / "workspaces").glob("*/messages.ndjson"))
        if len(journals) != 1 or message_id not in journals[0].read_text(encoding="utf-8"):
            raise WorkloadError("accepted handoff was not present in the isolated durable journal")
        send_finished = time.monotonic_ns()

        claim_started = time.monotonic_ns()
        claimed = run_as_agent(
            "reviewer",
            reviewer_control,
            [str(client), "inbox", "claim", message_id, "--plain"],
            root,
            env,
        )
        if HANDOFF_BODY not in claimed:
            raise WorkloadError("reviewer did not claim the installed handoff body")
        claim_finished = time.monotonic_ns()
        journal_lines = journals[0].read_bytes().count(b"\n")

        report = {
            "schema": 1,
            "kind": "cyclops_install_first_durable_handoff",
            "commit": command_text(["git", "rev-parse", "HEAD"], cwd=repo, env=env),
            "dirty": bool(command_text(["git", "status", "--porcelain"], cwd=repo, env=env)),
            "environment": {
                "os": platform.platform(),
                "machine": platform.machine(),
                "cpu_count": os.cpu_count(),
                "rustc": command_text(["rustc", "-Vv"], cwd=repo, env=env),
                "cargo": command_text([real_cargo, "-V"], cwd=repo, env=env),
                "tmux": command_text(["tmux", "-V"], cwd=repo, env=env),
            },
            "installed_pair": {
                "cyclops": client_version,
                "cyclopsd": daemon_version,
                "matched": True,
            },
            "workload": {
                "install_mode": "staged local source install",
                "source_acquisition": "not measured: this invokes the public installer from the checked-out source tree",
                "cargo_target": "fresh isolated target directory; registry and toolchain caches may be warm",
                "state": "fresh isolated prefix, HOME, CYCLOPS_HOME, tmux server, daemon, fixture agents, and journal",
                "dataset": "two explicit manifest-pinned fixture agents; one durable direct message and one authenticated claim",
                "fixture": "a test-only manifest and agent executable are prepared after installation; each synthetic agent is bound with the public name --manifest command before the handoff",
                "observation_resolution": "readiness and fixture completion use bounded 50ms test-rig probes; sub-50ms phases are not latency claims",
                "sample_note": "one staged install sample per artifact; percentile fields equal the observed sample",
                "comparison_baseline": "compare only artifacts with the same staged local-source workload, fresh target shape, and recorded environment; no universal target is asserted",
                "excludes": [
                    "network source download",
                    "Rust toolchain installation",
                    "real vendor agent startup",
                    "notification or terminal-injection latency",
                ],
            },
            "phases": {
                "source_build": summary(build_started, build_finished, "installer cargo build start to return"),
                "pair_activation": summary(build_finished, setup_started, "cargo build return to installer setup step"),
                "setup": summary(setup_started, setup_finished, "installer setup step to cyclops set-up output"),
                "installer_total": summary(install_started, install_finished, "public installer process start to return"),
                "fixture_setup": summary(fixture_setup_started, fixture_setup_finished, "test-only manifest and fixture-agent preparation"),
                "daemon_readiness": summary(daemon_started, daemon_ready_at, "installed daemon process start to responding status"),
                "session_adoption": summary(adoption_started, adoption_finished, "duo workspace command to daemon attached state"),
                "agent_detection": summary(detection_started, detection_finished, "fixture agent spawn to both explicit manifest bindings"),
                "durable_send": summary(send_started, send_finished, "agent send start to accepted journal record"),
                "authenticated_claim": summary(claim_started, claim_finished, "recipient claim start to verified body"),
                "first_durable_handoff_total": summary(install_started, claim_finished, "public installer start to recipient claim, including reported test-fixture setup"),
            },
            "correctness": {
                "installed_pair_matched": True,
                "daemon_responded": True,
                "session_attached": True,
                "fixture_manifest_pinned": (
                    implementer_manifest_pinned and reviewer_manifest_pinned
                ),
                "agents_detected": 2,
                "message_durably_accepted": True,
                "recipient_claimed": True,
                "journal_line_count_after_claim": journal_lines,
            },
        }
        print(MEASUREMENT_PREFIX + json.dumps(report, sort_keys=True))
        return 0
    finally:
        stop_daemon(daemon)
        if daemon_log is not None:
            daemon_log.close()
        teardown_tmux(repo, cleanup_env)
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    if sys.argv[1:] == ["--selftest"]:
        raise SystemExit(selftest())
    try:
        raise SystemExit(main())
    except WorkloadError as error:
        print(f"install-first-handoff workload failed: {error}", file=sys.stderr)
        raise SystemExit(1)
