#!/usr/bin/env python3
"""Classify a Git diff into Cyclops evidence responsibilities.

The classifier is deliberately path-based. It decides which expensive lane can
honestly report "not applicable"; it never claims that a path proves runtime
behavior. Workflow and classifier changes select every lane so routing changes
prove themselves before merge.
"""

from __future__ import annotations

import argparse
import subprocess
from pathlib import Path


LANES = (
    "rust",
    "docs",
    "parity",
    "website",
    "installer",
    "tmux",
    "platform",
    "tmux_head",
)


def under(path: str, *prefixes: str) -> bool:
    return any(path == prefix.rstrip("/") or path.startswith(prefix) for prefix in prefixes)


def classify(paths: list[str]) -> dict[str, bool]:
    result = {lane: False for lane in LANES}
    # A deleted or renamed source, script, resource, or fixture can invalidate
    # a path quoted by documentation. The checker is cheap, and a name-only
    # diff cannot prove that an arbitrary removed target was unreferenced.
    result["docs"] = True
    for path in paths:
        control = under(path, ".github/workflows/") or path.startswith("scripts/ci-") or path in {
            ".config/nextest.toml",
            "scripts/check.sh",
            "scripts/test-relocated-scratch.sh",
        }
        markdown = path.endswith(".md")
        cargo = path in {"Cargo.toml", "Cargo.lock"} or path.endswith("/Cargo.toml")

        result["rust"] |= control or cargo or path == ".config/nextest.toml" or under(
            path,
            "src/",
            "tests/testrig/",
        ) or path in {"tests/e2e/parity_agent.rs", "scripts/check.sh"}
        result["docs"] |= control or markdown or path == "scripts/check-doc-paths.py"
        result["parity"] |= control or markdown or cargo or under(
            path,
            "src/",
            "resources/",
            "tests/e2e/",
            "demos/",
        )
        result["website"] |= control or under(path, "website/") or path in {
            "scripts/install.sh",
            "README.md",
        }
        result["installer"] |= control or cargo or under(
            path,
            "resources/",
            "skills/",
            "scripts/install.sh",
            "website/static/install.sh",
            "src/cyclops/src/skillseed.rs",
            "src/cyclops/src/hookset.rs",
        ) or path == "docs/guides/install.md"
        result["tmux"] |= control or under(
            path,
            "src/cyclops-tmux/",
            "src/cyclops-workspace/",
            "src/cyclopsd/",
            "tests/testrig/",
            "resources/manifests/",
            "resources/layouts/",
            "tests/e2e/",
        )
        result["platform"] |= control or cargo or under(
            path,
            "src/cyclops-state/",
            "src/cyclops-tmux/",
            "tests/testrig/",
            "scripts/install.sh",
            "website/static/install.sh",
        ) or path in {
            "src/cyclops-proto/src/scratch.rs",
            "src/cyclops-client/src/lib.rs",
            "src/cyclopsd/src/server.rs",
            "src/cyclops/src/cleanup.rs",
            "src/cyclops/src/daemon.rs",
            "src/cyclops/src/health.rs",
            "src/cyclops/src/update.rs",
        } or under(path, "src/cyclops/tests/")
        result["tmux_head"] |= control or cargo or under(
            path,
            "src/cyclops-tmux/",
            "tests/testrig/",
            "resources/manifests/",
            "resources/layouts/",
        )
    return result


def changed_paths(base: str | None, head: str | None) -> list[str] | None:
    if not base or not head or set(base) == {"0"}:
        return None
    proc = subprocess.run(
        ["git", "diff", "--name-only", "--diff-filter=ACDMRTUXB", base, head],
        check=True,
        capture_output=True,
        text=True,
    )
    return [line for line in proc.stdout.splitlines() if line]


def selftest() -> None:
    assert classify(["website/src/routes/+page.svelte"])["website"]
    assert not classify(["website/src/routes/+page.svelte"])["installer"]
    assert classify(["scripts/install.sh"])["website"]
    assert classify(["scripts/install.sh"])["installer"]
    daemon = classify(["src/cyclopsd/src/messaging.rs"])
    assert daemon["rust"] and daemon["tmux"] and daemon["parity"]
    assert not daemon["platform"]
    client = classify(["src/cyclops-client/src/lib.rs"])
    assert client["platform"]
    assert classify(["src/cyclops/src/daemon.rs"])["platform"]
    assert classify(["src/cyclops/tests/e2e.rs"])["platform"]
    assert classify(["src/cyclops/tests/workspace_cli.rs"])["platform"]
    docs = classify(["docs/development/CI.md"])
    assert docs["docs"] and docs["parity"]
    assert not docs["rust"] and not docs["installer"]
    ordinary = classify(["src/cyclops-proto/src/attention.rs"])
    assert ordinary["rust"] and ordinary["docs"] and ordinary["parity"]
    assert not ordinary["website"] and not ordinary["installer"]
    assert not ordinary["tmux"] and not ordinary["platform"]
    assert not ordinary["tmux_head"]
    assert all(classify(["scripts/ci-performance.py"]).values())
    control = classify([".github/workflows/ci.yml"])
    assert all(control.values())


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base")
    parser.add_argument("--head")
    parser.add_argument("--github-output", type=Path)
    parser.add_argument("--selftest", action="store_true")
    args = parser.parse_args()

    if args.selftest:
        selftest()
        print("CI path classifier self-test passed")
        return 0

    paths = changed_paths(args.base, args.head)
    selection = {lane: True for lane in LANES} if paths is None else classify(paths)
    for path in paths or ["<manual run: all lanes>"]:
        print(path)
    print("selected:", ", ".join(lane for lane, selected in selection.items() if selected))

    if args.github_output:
        with args.github_output.open("a", encoding="utf-8") as output:
            for lane in LANES:
                output.write(f"{lane}={'true' if selection[lane] else 'false'}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
