#!/usr/bin/env python3
"""Run stable Cyclops performance workloads and retain comparable metadata."""

from __future__ import annotations

import argparse
import datetime
import json
import math
import os
import platform
import subprocess
import sys
import time
from pathlib import Path


WORKLOADS = (
    (
        "stream-frame-10k",
        ["cargo", "test", "-p", "cyclops-ui", "--test", "perf", "--", "--nocapture"],
        None,
    ),
    (
        "message-queue-10k",
        ["cargo", "test", "-p", "cyclops-ui", "--test", "queue_perf", "--", "--nocapture"],
        None,
    ),
    (
        "workspace-control-and-flood",
        ["cargo", "test", "-p", "cyclops-workspace", "--test", "perf_contract", "--", "--nocapture"],
        None,
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
        "CYCLOPS_DAEMON_COLD_START_REPLAY_JSON",
    ),
    (
        "install-first-durable-handoff",
        ["python3", "scripts/perf/install_first_handoff.py"],
        "CYCLOPS_INSTALL_FIRST_HANDOFF_JSON",
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

# The install journey is deliberately a staged local-source measurement, not a
# claim about downloading source or provisioning a clean operating system.
INSTALL_HANDOFF_SCHEMA = 1
INSTALL_HANDOFF_KIND = "cyclops_install_first_durable_handoff"
INSTALL_HANDOFF_PHASES = {
    "source_build",
    "pair_activation",
    "setup",
    "installer_total",
    "fixture_setup",
    "daemon_readiness",
    "session_adoption",
    "agent_detection",
    "durable_send",
    "authenticated_claim",
    "first_durable_handoff_total",
}
INSTALL_HANDOFF_WORKLOAD = {
    "install_mode": "staged local source install",
    "source_acquisition": "not measured: this invokes the public installer from the checked-out source tree",
    "cargo_target": "fresh isolated target directory; registry and toolchain caches may be warm",
    "state": "fresh isolated prefix, HOME, CYCLOPS_HOME, tmux server, daemon, fixture agents, and journal",
    "dataset": "two detected fixture agents; one durable direct message and one authenticated claim",
    "fixture": "a test-only manifest and agent executable are prepared after installation; fixture setup is reported separately",
    "observation_resolution": "readiness and fixture completion use bounded 50ms test-rig probes; sub-50ms phases are not latency claims",
    "sample_note": "one staged install sample per artifact; percentile fields equal the observed sample",
    "comparison_baseline": "compare only artifacts with the same staged local-source workload, fresh target shape, and recorded environment; no universal target is asserted",
    "excludes": [
        "network source download",
        "Rust toolchain installation",
        "real vendor agent startup",
        "notification or terminal-injection latency",
    ],
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


def nonnegative_number(value: object) -> bool:
    return type(value) in (int, float) and math.isfinite(value) and value >= 0


def nonempty_string(value: object) -> bool:
    return isinstance(value, str) and bool(value.strip())


def cold_replay_report_error(report: dict[str, object]) -> str | None:
    """Reject a report that would not prove the retained replay workload ran."""
    if not exact_int(report.get("schema"), COLD_REPLAY_SCHEMA):
        return f"expected schema {COLD_REPLAY_SCHEMA}"
    if report.get("kind") != COLD_REPLAY_KIND:
        return f"expected kind {COLD_REPLAY_KIND!r}"
    build_ref = report.get("benchmark_test_build_ref")
    if not nonempty_string(build_ref):
        return "missing benchmark_test_build_ref"
    version = report.get("cyclopsd_version")
    if not nonempty_string(version):
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


def install_handoff_report_error(report: dict[str, object]) -> str | None:
    """Reject an install record that does not prove a durable two-agent handoff."""
    if not exact_int(report.get("schema"), INSTALL_HANDOFF_SCHEMA):
        return f"expected schema {INSTALL_HANDOFF_SCHEMA}"
    if report.get("kind") != INSTALL_HANDOFF_KIND:
        return f"expected kind {INSTALL_HANDOFF_KIND!r}"
    if not nonempty_string(report.get("commit")):
        return "missing commit"
    if type(report.get("dirty")) is not bool:
        return "missing dirty state"

    environment = report.get("environment")
    if not isinstance(environment, dict):
        return "missing environment"
    for key in ("os", "machine", "rustc", "cargo", "tmux"):
        if not nonempty_string(environment.get(key)):
            return f"environment.{key} is missing"
    if not positive_int(environment.get("cpu_count")):
        return "environment.cpu_count is invalid"

    installed_pair = report.get("installed_pair")
    if not isinstance(installed_pair, dict):
        return "missing installed_pair"
    if not nonempty_string(installed_pair.get("cyclops")) or not nonempty_string(installed_pair.get("cyclopsd")):
        return "installed pair is missing a version"
    if installed_pair.get("matched") is not True:
        return "installed pair is not matched"

    workload = report.get("workload")
    if workload != INSTALL_HANDOFF_WORKLOAD:
        return "install handoff record does not match the retained workload contract"

    phases = report.get("phases")
    if not isinstance(phases, dict):
        return "install handoff record has no phase map"
    missing_phases = INSTALL_HANDOFF_PHASES.difference(phases)
    if missing_phases:
        return f"install handoff record is missing phases: {', '.join(sorted(missing_phases))}"
    for name in INSTALL_HANDOFF_PHASES:
        phase = phases.get(name)
        if not isinstance(phase, dict):
            return f"install handoff phase {name} is not an object"
        samples = phase.get("samples_seconds")
        if (
            not nonempty_string(phase.get("boundary"))
            or not exact_int(phase.get("sample_count"), 1)
            or not isinstance(samples, list)
            or len(samples) != 1
            or not nonnegative_number(samples[0])
        ):
            return f"install handoff phase {name} has incomplete samples"
        if any(phase.get(key) != samples[0] for key in ("p50_seconds", "p95_seconds", "max_seconds")):
            return f"install handoff phase {name} has inconsistent summaries"

    correctness = report.get("correctness")
    required_proofs = {
        "installed_pair_matched",
        "daemon_responded",
        "session_attached",
        "message_durably_accepted",
        "recipient_claimed",
    }
    if not isinstance(correctness, dict) or not all(correctness.get(proof) is True for proof in required_proofs):
        return "install handoff record is missing a durable-handoff proof"
    if not exact_int(correctness.get("agents_detected"), 2):
        return "install handoff record did not detect two agents"
    if not positive_int(correctness.get("journal_line_count_after_claim")):
        return "install handoff record has no durable journal proof"
    return None


def report_error(name: str, report: dict[str, object]) -> str | None:
    if name == "daemon-cold-start-replay":
        return cold_replay_report_error(report)
    if name == "install-first-durable-handoff":
        return install_handoff_report_error(report)
    return f"{name} has no retained-report validator"


def record_workload(
    name: str,
    command: list[str],
    proc: subprocess.CompletedProcess[str],
    duration: float,
    marker: str | None,
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
    if marker:
        measurement, error = required_json(output, marker)
        if measurement is not None and error is None:
            error = report_error(name, measurement)
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


def complete_install_handoff_report() -> dict[str, object]:
    """One complete staged handoff record for the runner's contract check."""
    sample = 0.1
    phase = {
        "boundary": "self-test boundary",
        "samples_seconds": [sample],
        "sample_count": 1,
        "p50_seconds": sample,
        "p95_seconds": sample,
        "max_seconds": sample,
    }
    return {
        "schema": INSTALL_HANDOFF_SCHEMA,
        "kind": INSTALL_HANDOFF_KIND,
        "commit": "selftest",
        "dirty": False,
        "environment": {
            "os": "selftest",
            "machine": "selftest",
            "cpu_count": 1,
            "rustc": "selftest",
            "cargo": "selftest",
            "tmux": "selftest",
        },
        "installed_pair": {
            "cyclops": "cyclops 0.1.0",
            "cyclopsd": "cyclopsd 0.1.0",
            "matched": True,
        },
        "workload": INSTALL_HANDOFF_WORKLOAD,
        "phases": {name: phase.copy() for name in INSTALL_HANDOFF_PHASES},
        "correctness": {
            "installed_pair_matched": True,
            "daemon_responded": True,
            "session_attached": True,
            "agents_detected": 2,
            "message_durably_accepted": True,
            "recipient_claimed": True,
            "journal_line_count_after_claim": 1,
        },
    }


def selftest() -> None:
    """Prove incomplete retained reports cannot become successful artifacts."""

    def fixture_result(name: str, report: object | None) -> dict[str, object]:
        marker = next(marker for workload, _, marker in WORKLOADS if workload == name)
        output = "test skipped\n" if report is None else f"{marker}={json.dumps(report)}\n"
        return record_workload(
            name,
            ["fixture"],
            subprocess.CompletedProcess(["fixture"], 0, output, ""),
            0.0,
            marker,
        )

    def rejected(name: str, report: object) -> None:
        result = fixture_result(name, report)
        assert result["status"] == 1
        assert "measurement_error" in result

    for name in ("daemon-cold-start-replay", "install-first-durable-handoff"):
        missing = fixture_result(name, None)
        assert missing["command_status"] == 0
        assert missing["status"] == 1
        assert "measurement_error" in missing
        assert "measurement" not in missing

    cold = complete_cold_replay_report()
    valid_cold = fixture_result("daemon-cold-start-replay", cold)
    assert valid_cold["status"] == 0
    assert valid_cold["measurement"] == cold
    rejected("daemon-cold-start-replay", {})

    wrong_journal = json.loads(json.dumps(cold))
    wrong_journal["measurements"][2]["workspace_journal"]["lines"] = 9_999
    rejected("daemon-cold-start-replay", wrong_journal)

    install = complete_install_handoff_report()
    valid_install = fixture_result("install-first-durable-handoff", install)
    assert valid_install["status"] == 0
    assert valid_install["measurement"] == install
    rejected("install-first-durable-handoff", {})

    missing_phase = json.loads(json.dumps(install))
    missing_phase["phases"].pop("durable_send")
    rejected("install-first-durable-handoff", missing_phase)

    missing_proof = json.loads(json.dumps(install))
    missing_proof["correctness"]["recipient_claimed"] = False
    rejected("install-first-durable-handoff", missing_proof)

    changed_isolation = json.loads(json.dumps(install))
    changed_isolation["workload"]["state"] = "shared operator HOME"
    rejected("install-first-durable-handoff", changed_isolation)

    inconsistent_summary = json.loads(json.dumps(install))
    inconsistent_summary["phases"]["setup"]["p95_seconds"] = 1.0
    rejected("install-first-durable-handoff", inconsistent_summary)

    nonfinite_phase = json.loads(json.dumps(install))
    nonfinite_phase["phases"]["setup"] = {
        "boundary": "non-finite sample",
        "samples_seconds": [float("inf")],
        "sample_count": 1,
        "p50_seconds": float("inf"),
        "p95_seconds": float("inf"),
        "max_seconds": float("inf"),
    }
    rejected("install-first-durable-handoff", nonfinite_phase)


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
    for name, command, marker in WORKLOADS:
        start = time.monotonic()
        proc = subprocess.run(command, check=False, capture_output=True, text=True)
        duration = time.monotonic() - start
        result = record_workload(name, command, proc, duration, marker)
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
