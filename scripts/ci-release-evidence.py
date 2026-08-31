#!/usr/bin/env python3
"""Check that the release aggregate owns every same-commit evidence lane.

GitHub Actions runs the commands. This small syntactic check protects the
workflow topology: a green release aggregate must wait for tmux HEAD and the
bounded race, cleanup, soak, and long-history run at the candidate's SHA.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path


DEFAULT_WORKFLOW = Path(".github/workflows/release-evidence.yml")


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
    args = parser.parse_args()

    try:
        workflow = args.workflow.read_text(encoding="utf-8")
        if args.selftest:
            selftest(workflow)
            print("Release evidence workflow self-test passed")
        else:
            validate(workflow)
            print("Release evidence workflow contract passed")
    except (OSError, ValueError, AssertionError) as error:
        print(f"release evidence workflow contract failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
