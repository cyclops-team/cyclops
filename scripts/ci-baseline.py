#!/usr/bin/env python3
"""Render a reproducible GitHub Actions cost baseline for one CI run.

The report uses first-attempt job timestamps for wall and runner time, then
samples recent pull-request runs of the same workflow for failure and explicit
rerun frequency. It distinguishes workflow failures from flaky tests: the API
does not say why a run failed.

Run it from a checked-out Cyclops repository:

    python3 scripts/ci-baseline.py <run-id> --sample 30
"""

from __future__ import annotations

import argparse
import json
import subprocess
from datetime import datetime
from typing import Any
from urllib.parse import urlencode


def gh_json(repo: str, endpoint: str) -> Any:
    result = subprocess.run(
        ["gh", "api", f"repos/{repo}/{endpoint}"],
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(result.stdout)


def repository_name() -> str:
    result = subprocess.run(
        ["gh", "repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner"],
        check=True,
        capture_output=True,
        text=True,
    )
    return result.stdout.strip()


def timestamp(value: str) -> datetime:
    return datetime.fromisoformat(value.replace("Z", "+00:00"))


def elapsed_seconds(start: str, end: str) -> int:
    return round((timestamp(end) - timestamp(start)).total_seconds())


def duration(seconds: int) -> str:
    minutes, remainder = divmod(seconds, 60)
    return f"{minutes}m {remainder:02d}s"


def percent(count: int, total: int) -> str:
    return "0.0%" if total == 0 else f"{count * 100 / total:.1f}%"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run_id", type=int, help="GitHub Actions workflow run id")
    parser.add_argument("--attempt", type=int, default=1, help="attempt to baseline")
    parser.add_argument("--sample", type=int, default=30, choices=range(1, 101))
    parser.add_argument("--repo", help="owner/name; defaults to the current checkout")
    args = parser.parse_args()

    repo = args.repo or repository_name()
    run = gh_json(repo, f"actions/runs/{args.run_id}")
    jobs = gh_json(
        repo,
        f"actions/runs/{args.run_id}/attempts/{args.attempt}/jobs?per_page=100",
    )["jobs"]
    complete_jobs = [job for job in jobs if job["started_at"] and job["completed_at"]]
    if not complete_jobs:
        raise SystemExit("the selected attempt has no completed jobs")

    first_start = min(timestamp(job["started_at"]) for job in complete_jobs)
    last_finish = max(timestamp(job["completed_at"]) for job in complete_jobs)
    wall_seconds = round((last_finish - first_start).total_seconds())
    job_durations = [
        (job["name"], job["conclusion"], elapsed_seconds(job["started_at"], job["completed_at"]))
        for job in complete_jobs
    ]
    runner_seconds = sum(item[2] for item in job_durations)

    query = urlencode(
        {
            "per_page": args.sample,
            "event": "pull_request",
            "created": f"<={run['created_at']}",
        }
    )
    recent = gh_json(
        repo,
        f"actions/workflows/{run['workflow_id']}/runs?{query}",
    )["workflow_runs"]
    same_workflow = recent
    failures = sum(item["conclusion"] == "failure" for item in same_workflow)
    successes = sum(item["conclusion"] == "success" for item in same_workflow)
    cancelled = sum(item["conclusion"] == "cancelled" for item in same_workflow)
    reruns = sum(item["run_attempt"] > 1 for item in same_workflow)

    print("# CI baseline")
    print()
    print(f"- Repository: `{repo}`")
    print(f"- Workflow: `{run['name']}`")
    print(f"- Run: [{args.run_id}]({run['html_url']})")
    print(f"- Head: `{run['head_sha']}`")
    print(f"- Attempt: {args.attempt}")
    print(f"- Wall time: {duration(wall_seconds)}")
    print(f"- Runner time: {duration(runner_seconds)}")
    print()
    print("| Job | Conclusion | Duration |")
    print("|---|---:|---:|")
    for name, conclusion, seconds in sorted(job_durations, key=lambda item: item[0]):
        print(f"| {name} | {conclusion} | {duration(seconds)} |")
    print()
    total = len(same_workflow)
    print(f"## Recent pull-request runs ({total})")
    print()
    print(f"- Success: {successes} ({percent(successes, total)})")
    print(f"- Failure: {failures} ({percent(failures, total)})")
    print(f"- Cancelled: {cancelled} ({percent(cancelled, total)})")
    print(f"- Explicit rerun: {reruns} ({percent(reruns, total)})")
    print()
    print(
        "Failure frequency is a workflow outcome, not a flake classification. "
        "Explicit reruns count runs whose latest GitHub attempt is greater than one."
    )


if __name__ == "__main__":
    main()
