#!/usr/bin/env python3
"""Check that the release aggregate owns every same-commit evidence lane.

GitHub Actions runs the commands. This small syntactic check protects the
workflow topology: a green release aggregate must wait for tmux HEAD and the
bounded race, cleanup, soak, and long-history run at the candidate's SHA.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path


DEFAULT_WORKFLOW = Path(".github/workflows/release-evidence.yml")
PUBLISH_WORKFLOW = Path(".github/workflows/release-binaries.yml")


def candidate_evidence(runs: object, candidate: str) -> str:
    """Require a completed release-evidence run for the exact tagged commit.

    The caller obtains this response from this repository's workflow-specific
    GitHub API endpoint. A green PR, older SHA, or queued run is not release evidence.
    """
    if not re.fullmatch(r"[a-f0-9]{40}", candidate):
        raise ValueError("candidate must be a full 40-character commit SHA")
    if not isinstance(runs, dict) or not isinstance(runs.get("workflow_runs"), list):
        raise ValueError("GitHub did not return a workflow-runs response")
    for run in runs["workflow_runs"]:
        if (
            isinstance(run, dict)
            and run.get("head_sha") == candidate
            and run.get("path") == str(DEFAULT_WORKFLOW)
            and run.get("event") in {"workflow_dispatch", "push"}
            and run.get("status") == "completed"
            and run.get("conclusion") == "success"
            and isinstance(run.get("html_url"), str)
        ):
            return run["html_url"]
    raise ValueError(f"no successful release evidence for exact candidate {candidate}; run release-evidence.yml at this commit before publishing")


def validate_publishing(workflow: str) -> None:
    jobs = job_blocks(workflow)
    evidence = jobs.get("check-evidence", "")
    require_text(evidence, "actions: read", "release evidence lookup lacks Actions permission")
    require_text(evidence, "actions/workflows/release-evidence.yml/runs", "publication does not query release evidence")
    require_text(evidence, 'git rev-parse HEAD', "publication does not resolve the tagged commit")
    require_text(evidence, '--candidate "$candidate"', "publication does not verify the exact candidate")
    require_text(evidence, "shell: bash", "evidence validation needs pipefail so tee cannot hide a refusal")
    require_text(jobs.get("build", ""), "needs: [check-version, check-evidence]", "release builds must wait for candidate evidence")
    require_text(jobs.get("publish", ""), "needs: [check-version, check-evidence, build]", "release publishing must wait for candidate evidence")


def job_blocks(workflow: str) -> dict[str, str]:
    """Return the top-level job blocks without requiring a YAML dependency."""

    jobs_marker = "\njobs:\n"
    marker_index = workflow.find(jobs_marker)
    if marker_index < 0:
        return {}
    pattern = re.compile(
        r"^  (?P<name>[a-z][a-z0-9-]*):\n(?P<body>.*?)(?=^  [a-z][a-z0-9-]*:\n|\Z)",
        re.MULTILINE | re.DOTALL,
    )
    jobs_section = workflow[marker_index + len(jobs_marker) :]
    return {match["name"]: match["body"] for match in pattern.finditer(jobs_section)}


def require_text(block: str, text: str, error: str) -> None:
    if text not in block:
        raise ValueError(error)


def validate(workflow: str) -> None:
    """Require the release workflow's candidate-wide responsibilities."""

    jobs = job_blocks(workflow)

    tmux_head = jobs.get("tmux-head")
    if tmux_head is None:
        raise ValueError("missing release tmux-head job")
    require_text(tmux_head, "name: release tmux HEAD", "tmux-head has the wrong stable name")
    require_text(tmux_head, "Build tmux from master", "tmux-head does not build tmux HEAD")
    require_text(tmux_head, "./scripts/check.sh --fast", "tmux-head lacks the full fast gate")

    reliability = jobs.get("reliability")
    if reliability is None:
        raise ValueError("missing release reliability job")
    require_text(
        reliability,
        "name: release race and long-history evidence",
        "reliability has the wrong stable name",
    )
    require_text(reliability, "timeout-minutes: 90", "reliability lost its bounded timeout")
    require_text(
        reliability,
        "CYCLOPS_CI_REPEAT: 10",
        "reliability lost its bounded repetition count",
    )
    require_text(
        reliability,
        "./scripts/ci-reliability.sh",
        "reliability does not run the retained evidence",
    )

    aggregate = jobs.get("beta-release-gate")
    if aggregate is None:
        raise ValueError("missing beta release evidence aggregate")
    if not re.search(r"^    if: \$\{\{ always\(\) \}\}$", aggregate, re.MULTILINE):
        raise ValueError("beta-release-gate must run after failed dependencies")
    for dependency in (
        "clean-validation",
        "historical-journals",
        "installer-and-journeys",
        "performance",
        "tmux-head",
        "reliability",
    ):
        require_text(
            aggregate,
            f"      - {dependency}",
            f"beta-release-gate must need {dependency}",
        )
    require_text(
        aggregate,
        "TMUX_HEAD: ${{ needs.tmux-head.result }}",
        "beta-release-gate does not read the tmux-head result",
    )
    require_text(
        aggregate,
        "RELIABILITY: ${{ needs.reliability.result }}",
        "beta-release-gate does not read the reliability result",
    )
    require_text(
        aggregate,
        'test "$TMUX_HEAD" = success',
        "beta-release-gate does not require tmux-head success",
    )
    require_text(
        aggregate,
        'test "$RELIABILITY" = success',
        "beta-release-gate does not require reliability success",
    )


def selftest(workflow: str) -> None:
    """Show that the aggregate rejects missing evidence or failure handling."""

    validate(workflow)
    publishing = PUBLISH_WORKFLOW.read_text(encoding="utf-8")
    validate_publishing(publishing)
    candidate = "a" * 40
    verified = {
        "head_sha": candidate,
        "path": str(DEFAULT_WORKFLOW),
        "event": "workflow_dispatch",
        "status": "completed",
        "conclusion": "success",
        "html_url": "https://github.com/example/cyclops/actions/runs/123",
    }
    assert candidate_evidence({"workflow_runs": [verified]}, candidate) == verified["html_url"]
    for field, value in (
        ("head_sha", "b" * 40),
        ("path", ".github/workflows/ci.yml"),
        ("event", "pull_request"),
        ("status", "in_progress"),
        ("conclusion", "failure"),
        ("conclusion", "cancelled"),
        ("conclusion", "skipped"),
    ):
        try:
            candidate_evidence({"workflow_runs": [{**verified, field: value}]}, candidate)
        except ValueError:
            pass
        else:
            raise AssertionError(f"publication accepted {field}={value}")
    try:
        validate_publishing(publishing.replace("needs: [check-version, check-evidence, build]", "needs: [check-version, build]"))
    except ValueError:
        pass
    else:
        raise AssertionError("publication accepted a missing evidence dependency")
    try:
        validate_publishing(publishing.replace("shell: bash", "shell: sh"))
    except ValueError:
        pass
    else:
        raise AssertionError("publication accepted a pipeline without pipefail")
    for dependency in ("tmux-head", "reliability"):
        missing_dependency = workflow.replace(f"      - {dependency}\n", "", 1)
        try:
            validate(missing_dependency)
        except ValueError as error:
            expected = f"beta-release-gate must need {dependency}"
            if str(error) != expected:
                raise AssertionError(f"expected {expected!r}, got {error!r}") from error
        else:
            raise AssertionError(f"missing {dependency} dependency passed validation")

    aggregate_prefix = (
        "  beta-release-gate:\n"
        "    name: beta release evidence complete\n"
        "    if: ${{ always() }}\n"
    )
    missing_always = workflow.replace(
        aggregate_prefix,
        aggregate_prefix.removesuffix("    if: ${{ always() }}\n"),
        1,
    )
    try:
        validate(missing_always)
    except ValueError as error:
        expected = "beta-release-gate must run after failed dependencies"
        if str(error) != expected:
            raise AssertionError(f"expected {expected!r}, got {error!r}") from error
    else:
        raise AssertionError("missing aggregate always condition passed validation")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workflow", type=Path, default=DEFAULT_WORKFLOW)
    parser.add_argument("--selftest", action="store_true")
    parser.add_argument("--candidate", help="exact tagged commit to authorize for packaging")
    parser.add_argument("--runs-file", type=Path, help="GitHub workflow-specific runs API response")
    args = parser.parse_args()

    try:
        if args.candidate or args.runs_file:
            if not args.candidate or not args.runs_file:
                raise ValueError("--candidate and --runs-file are required together")
            url = candidate_evidence(json.loads(args.runs_file.read_text()), args.candidate)
            print(f"Release evidence for {args.candidate}: {url}")
            return 0
        workflow = args.workflow.read_text(encoding="utf-8")
        if args.selftest:
            selftest(workflow)
            print("Release evidence workflow self-test passed")
        else:
            validate(workflow)
            validate_publishing(PUBLISH_WORKFLOW.read_text(encoding="utf-8"))
            print("Release evidence workflow contract passed")
    except (OSError, ValueError, AssertionError) as error:
        print(f"release evidence workflow contract failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
