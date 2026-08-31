#!/usr/bin/env python3
"""Run stable Cyclops performance workloads and retain comparable metadata."""

from __future__ import annotations

import argparse
import datetime
import json
import os
import platform
import subprocess
import sys
import time
from pathlib import Path


WORKLOADS = (
    ("stream-frame-10k", ["cargo", "test", "-p", "cyclops-ui", "--test", "perf", "--", "--nocapture"]),
    ("message-queue-10k", ["cargo", "test", "-p", "cyclops-ui", "--test", "queue_perf", "--", "--nocapture"]),
    (
        "workspace-control-and-flood",
        ["cargo", "test", "-p", "cyclops-workspace", "--test", "perf_contract", "--", "--nocapture"],
    ),
    (
        "daemon-cold-start-replay",
        [
            "cargo",
            "test",
            "-p",
            "cyclopsd",
            "--test",
            "cold_start_replay_perf",
            "--",
            "--ignored",
            "--nocapture",
        ],
    ),
)

# Most performance executables enforce a budget and need only a successful
# exit. This workload also supplies a retained measurement. A clean test exit
# after an environmental skip is not performance evidence, so the runner
# requires the report marker before it can call this workload successful.
REQUIRED_JSON_MARKERS = {
    "daemon-cold-start-replay": "CYCLOPS_DAEMON_COLD_START_REPLAY_JSON",
}


def command_text(command: list[str]) -> str:
    proc = subprocess.run(command, check=False, capture_output=True, text=True)
    return (proc.stdout or proc.stderr).strip()


def package_version() -> str:
    metadata = json.loads(command_text(["cargo", "metadata", "--no-deps", "--format-version", "1"]))
    return next(package["version"] for package in metadata["packages"] if package["name"] == "cyclops")


def output_of(proc: subprocess.CompletedProcess[str]) -> str:
    return "\n".join(part.strip() for part in (proc.stdout, proc.stderr) if part.strip())


def required_json(output: str, marker: str) -> tuple[dict[str, object] | None, str | None]:
    """Read one retained measurement marker, or explain why it is unusable."""
    prefix = f"{marker}="
    payloads = [line.removeprefix(prefix) for line in output.splitlines() if line.startswith(prefix)]
    if len(payloads) != 1:
        return None, f"expected one {marker} report, found {len(payloads)}"
    try:
        parsed = json.loads(payloads[0])
    except json.JSONDecodeError as error:
        return None, f"{marker} is not valid JSON: {error.msg}"
    if not isinstance(parsed, dict):
        return None, f"{marker} must contain a JSON object"
    return parsed, None


def record_workload(
    name: str,
    command: list[str],
    proc: subprocess.CompletedProcess[str],
    duration: float,
) -> dict[str, object]:
    """Turn a completed command into retained evidence with an honest status."""
    output = output_of(proc)
    result: dict[str, object] = {
        "name": name,
        "command": command,
        "duration_seconds": round(duration, 6),
        # `status` is the evidence result. Keep the raw command status too:
        # an otherwise-successful command may still fail to provide a required
        # measurement, and hiding that distinction makes diagnosis harder.
        "command_status": proc.returncode,
        "status": proc.returncode,
        "output": output.splitlines(),
    }
    if marker := REQUIRED_JSON_MARKERS.get(name):
        measurement, error = required_json(output, marker)
        result["required_json_marker"] = marker
        if error:
            result["status"] = proc.returncode or 1
            result["measurement_error"] = error
        else:
            result["measurement"] = measurement
    return result


def selftest() -> None:
    """Prove a skipped measurement cannot become a successful artifact."""
    marker = REQUIRED_JSON_MARKERS["daemon-cold-start-replay"]
    missing = record_workload(
        "daemon-cold-start-replay",
        ["fixture"],
        subprocess.CompletedProcess(["fixture"], 0, "test skipped\n", ""),
        0.0,
    )
    assert missing["command_status"] == 0
    assert missing["status"] == 1
    assert "measurement_error" in missing
    assert "measurement" not in missing

    valid = record_workload(
        "daemon-cold-start-replay",
        ["fixture"],
        subprocess.CompletedProcess(["fixture"], 0, f"{marker}={{\"schema\":1}}\n", ""),
        0.0,
    )
    assert valid["status"] == 0
    assert valid["measurement"] == {"schema": 1}

    malformed = record_workload(
        "daemon-cold-start-replay",
        ["fixture"],
        subprocess.CompletedProcess(["fixture"], 0, f"{marker}=not-json\n", ""),
        0.0,
    )
    assert malformed["status"] == 1
    assert "measurement_error" in malformed


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("output", nargs="?", type=Path)
    parser.add_argument("--selftest", action="store_true")
    args = parser.parse_args()

    if args.selftest:
        selftest()
        print("CI performance runner self-test passed")
        return 0
    if args.output is None:
        parser.error("output is required unless --selftest is used")

    results = []
    failed = False
    for name, command in WORKLOADS:
        start = time.monotonic()
        proc = subprocess.run(command, check=False, capture_output=True, text=True)
        duration = time.monotonic() - start
        result = record_workload(name, command, proc, duration)
        output = "\n".join(result["output"])
        print(f"== {name} ({duration:.3f}s)")
        print(output)
        if error := result.get("measurement_error"):
            print(f"!! {name}: {error}", file=sys.stderr)
        results.append(result)
        failed |= result["status"] != 0

    report = {
        "schema": 2,
        "generated_at_utc": datetime.datetime.now(datetime.UTC).isoformat(),
        "commit": command_text(["git", "rev-parse", "HEAD"]),
        "dirty": bool(command_text(["git", "status", "--porcelain"])),
        "cyclops_version": package_version(),
        "environment": {
            "os": platform.platform(),
            "machine": platform.machine(),
            "cpu_count": os.cpu_count(),
            "rustc": command_text(["rustc", "-Vv"]),
            "cargo": command_text(["cargo", "-V"]),
            "tmux": command_text(["tmux", "-V"]),
            "runner_os": os.environ.get("RUNNER_OS"),
            "runner_arch": os.environ.get("RUNNER_ARCH"),
            "image_os": os.environ.get("ImageOS"),
            "image_version": os.environ.get("ImageVersion"),
        },
        "workloads": results,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return int(failed)


if __name__ == "__main__":
    raise SystemExit(main())
