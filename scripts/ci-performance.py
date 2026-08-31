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

# The daemon workload crosses a process boundary. This is its retained artifact
# contract, kept beside the runner that decides whether scheduled evidence is
# honest. The Rust test emits these same facts in cold_start_replay_perf.rs.
COLD_REPLAY_SCHEMA = 1
COLD_REPLAY_KIND = "cyclops_daemon_cold_start_replay"
COLD_REPLAY_MESSAGE_COUNTS = (0, 1_000, 10_000)
COLD_REPLAY_SAMPLES_PER_COUNT = 3
COLD_REPLAY_WORKLOAD = {
    "fixture": "operator-addressed FYI messages accepted through WorkspaceMessaging",
    "measurement": "cyclopsd::boot from an already-validated config after clean daemon shutdown",
    "replay_validation": "a body-free snapshot sees every seeded message after every timed boot",
    "samples_per_message_count": COLD_REPLAY_SAMPLES_PER_COUNT,
    "excludes": [
        "config parsing",
        "executable-process launch",
        "client connection",
        "terminal notification latency",
        "post-boot snapshot verification",
    ],
}

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


def positive_int(value: object) -> bool:
    return type(value) is int and value > 0


def exact_int(value: object, expected: int) -> bool:
    return type(value) is int and value == expected


def cold_replay_report_error(report: dict[str, object]) -> str | None:
    """Reject a report that would not prove the retained replay workload ran."""
    if not exact_int(report.get("schema"), COLD_REPLAY_SCHEMA):
        return f"expected schema {COLD_REPLAY_SCHEMA}"
    if report.get("kind") != COLD_REPLAY_KIND:
        return f"expected kind {COLD_REPLAY_KIND!r}"
    build_ref = report.get("benchmark_test_build_ref")
    if not isinstance(build_ref, str) or not build_ref:
        return "missing benchmark_test_build_ref"
    version = report.get("cyclopsd_version")
    if not isinstance(version, str) or not version:
        return "missing cyclopsd_version"

    workload = report.get("workload")
    if not isinstance(workload, dict):
        return "missing workload object"
    for key, expected in COLD_REPLAY_WORKLOAD.items():
        if workload.get(key) != expected:
            return f"workload.{key} does not match the retained contract"

    measurements = report.get("measurements")
    if not isinstance(measurements, list) or len(measurements) != len(COLD_REPLAY_MESSAGE_COUNTS):
        return f"expected {len(COLD_REPLAY_MESSAGE_COUNTS)} measurement records"
    for count, record in zip(COLD_REPLAY_MESSAGE_COUNTS, measurements):
        if not isinstance(record, dict):
            return f"measurement {count} is not an object"
        if not exact_int(record.get("accepted_message_count"), count):
            return f"measurement {count} has the wrong accepted_message_count"

        journal = record.get("workspace_journal")
        if not isinstance(journal, dict):
            return f"measurement {count} is missing workspace_journal"
        bytes_count = journal.get("bytes")
        if type(bytes_count) is not int or bytes_count < 0:
            return f"measurement {count} has an invalid journal byte count"
        if not exact_int(journal.get("lines"), count):
            return f"measurement {count} has the wrong journal line count"
        if (count == 0 and bytes_count != 0) or (count > 0 and bytes_count <= 0):
            return f"measurement {count} has an implausible journal byte count"

        boot = record.get("daemon_boot")
        if not isinstance(boot, dict):
            return f"measurement {count} is missing daemon_boot"
        samples = boot.get("samples")
        if (
            boot.get("unit") != "microseconds"
            or not exact_int(boot.get("sample_count"), COLD_REPLAY_SAMPLES_PER_COUNT)
            or not isinstance(samples, list)
            or len(samples) != COLD_REPLAY_SAMPLES_PER_COUNT
            or not all(positive_int(sample) for sample in samples)
        ):
            return f"measurement {count} has incomplete boot timings"
        ordered = sorted(samples)
        p50 = ordered[(len(ordered) * 50 + 99) // 100 - 1]
        p95 = ordered[(len(ordered) * 95 + 99) // 100 - 1]
        if (
            not exact_int(boot.get("p50"), p50)
            or not exact_int(boot.get("p95"), p95)
            or not exact_int(boot.get("max"), ordered[-1])
        ):
            return f"measurement {count} has inconsistent boot timing summaries"
    return None


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
        if measurement is not None and error is None:
            error = cold_replay_report_error(measurement)
        result["required_json_marker"] = marker
        if error:
            result["status"] = proc.returncode or 1
            result["measurement_error"] = error
        else:
            result["measurement"] = measurement
    return result


def complete_cold_replay_report() -> dict[str, object]:
    """One complete small report for the runner's artifact-contract check."""
    measurements = []
    for index, count in enumerate(COLD_REPLAY_MESSAGE_COUNTS):
        samples = [100 + index, 120 + index, 110 + index]
        ordered = sorted(samples)
        measurements.append(
            {
                "accepted_message_count": count,
                "workspace_journal": {
                    "bytes": 0 if count == 0 else count * 8,
                    "lines": count,
                },
                "daemon_boot": {
                    "unit": "microseconds",
                    "samples": samples,
                    "sample_count": COLD_REPLAY_SAMPLES_PER_COUNT,
                    "p50": ordered[1],
                    "p95": ordered[-1],
                    "max": ordered[-1],
                },
            }
        )
    return {
        "schema": COLD_REPLAY_SCHEMA,
        "kind": COLD_REPLAY_KIND,
        "benchmark_test_build_ref": "selftest",
        "cyclopsd_version": "0.1.0",
        "workload": COLD_REPLAY_WORKLOAD,
        "measurements": measurements,
    }


def selftest() -> None:
    """Prove a skipped measurement cannot become a successful artifact."""
    marker = REQUIRED_JSON_MARKERS["daemon-cold-start-replay"]

    def rejected(payload: object) -> None:
        result = record_workload(
            "daemon-cold-start-replay",
            ["fixture"],
            subprocess.CompletedProcess(["fixture"], 0, f"{marker}={json.dumps(payload)}\n", ""),
            0.0,
        )
        assert result["status"] == 1
        assert "measurement_error" in result

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

    complete = complete_cold_replay_report()
    valid = record_workload(
        "daemon-cold-start-replay",
        ["fixture"],
        subprocess.CompletedProcess(["fixture"], 0, f"{marker}={json.dumps(complete)}\n", ""),
        0.0,
    )
    assert valid["status"] == 0
    assert valid["measurement"] == complete

    for incomplete in [{}, {"schema": 1}]:
        rejected(incomplete)

    wrong_workload = json.loads(json.dumps(complete))
    wrong_workload["workload"]["replay_validation"] = "not verified"
    rejected(wrong_workload)

    incomplete_measurements = json.loads(json.dumps(complete))
    incomplete_measurements["measurements"].pop()
    rejected(incomplete_measurements)

    wrong_journal = json.loads(json.dumps(complete))
    wrong_journal["measurements"][2]["workspace_journal"]["lines"] = 9_999
    rejected(wrong_journal)

    inconsistent_timing = json.loads(json.dumps(complete))
    inconsistent_timing["measurements"][1]["daemon_boot"]["p50"] = 0
    rejected(inconsistent_timing)


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
