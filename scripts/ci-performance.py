#!/usr/bin/env python3
"""Run stable Cyclops performance workloads and retain comparable metadata."""

from __future__ import annotations

import argparse
import datetime
import json
import os
import platform
import subprocess
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


def command_text(command: list[str]) -> str:
    proc = subprocess.run(command, check=False, capture_output=True, text=True)
    return (proc.stdout or proc.stderr).strip()


def package_version() -> str:
    metadata = json.loads(command_text(["cargo", "metadata", "--no-deps", "--format-version", "1"]))
    return next(package["version"] for package in metadata["packages"] if package["name"] == "cyclops")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("output", type=Path)
    args = parser.parse_args()

    results = []
    failed = False
    for name, command in WORKLOADS:
        start = time.monotonic()
        proc = subprocess.run(command, check=False, capture_output=True, text=True)
        duration = time.monotonic() - start
        output = "\n".join(part.strip() for part in (proc.stdout, proc.stderr) if part.strip())
        print(f"== {name} ({duration:.3f}s)")
        print(output)
        results.append(
            {
                "name": name,
                "command": command,
                "duration_seconds": round(duration, 6),
                "status": proc.returncode,
                "output": output.splitlines(),
            }
        )
        failed |= proc.returncode != 0

    report = {
        "schema": 1,
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
