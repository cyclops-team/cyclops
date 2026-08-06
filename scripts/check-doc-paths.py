#!/usr/bin/env python3
"""Check that every repo path a doc quotes actually exists.

A path in a doc is checkable, and until this existed it was not checked:
docs/ARCHITECTURE.md opened by pointing newcomers at two files that were
not in this repo, and thirty-one source paths were written without their
`crates/` prefix, so copying one into an editor found nothing.

Two kinds of path get checked, because a reader uses them differently:

1. Markdown link targets, `[text](docs/install.md)`. A link that does not
   resolve is broken navigation.
2. Inline code spans that look like repo paths, `crates/cyclopsd/src`.
   A reader copies these into an editor or a command.

Run it: python3 scripts/check-doc-paths.py
Exit 0 when every path resolves, 1 with a report when one does not.
"""

import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# ---------------------------------------------------------------------------
# what is not a repo path
# ---------------------------------------------------------------------------

# Each rule below is a category with a reason, not a list of strings that
# happened to fail. A literal allowlist grows every time someone adds a
# doc; a rule with a reason does not, and it says what it believes.
SKIP_RULES = [
    (
        lambda s: s.startswith("~"),
        "a path in the operator's home, not in this repo",
    ),
    (
        lambda s: s.startswith("$") or "$" in s,
        "a path built from an environment variable at runtime",
    ),
    (
        lambda s: s.startswith("/"),
        "an absolute system path (/proc, /var/folders, /dev/null)",
    ),
    (
        lambda s: "<" in s or ">" in s or "{" in s or "*" in s,
        "a placeholder or a glob, not one literal file",
    ),
    (
        lambda s: "=" in s,
        "a KEY=value assignment that happens to contain a slash",
    ),
    (
        lambda s: all(seg.replace(".", "").isdigit() for seg in s.split("/") if seg),
        "all-numeric segments: a ratio like 34/33/33, not a path",
    ),
    (
        lambda s: s.split("/")[0] == "target",
        "cargo build output, which exists only after a build",
    ),
]

# The two that no rule can express, because they are real paths in a tree
# this repo is not. Both are v1, quoted by the cutover and history pages,
# and both are correct where they appear.
LITERAL_SKIPS = {
    "bin/commPact": "a path in the v1 tree, quoted by docs/CUTOVER.md",
    "versions/2.1.220": "a v1 release directory, quoted as history",
}

# A code span is a candidate when it has a slash and no whitespace. That
# is the whole shape test, deliberately.
#
# An earlier version also required the first segment to be a directory at
# the repo root, and that version skipped `cyclops-proto/src/ledger.rs`,
# which is the exact defect this gate was written for: thirty-one source
# paths missing their `crates/` prefix. Measured against these docs, the
# root test excluded 49 candidates, 43 of which every rule above already
# covers and 6 of which were real repo paths (`./demos/parity-check.sh`,
# `.github/workflows/ci.yml`). It lost more than it saved.
#
# The cost of dropping it is that a non-path with a slash written later
# fails here. That failure is loud, lands on the author, and is fixed by
# a rule or a reword. The failure it replaces was silent and reached the
# reader.

LINK = re.compile(r"\[[^\]]*\]\(([^)\s]+)\)")
CODE = re.compile(r"`([^`\n]+)`")


def skip_reason(s):
    """Why this string is not a repo path, or None if it should exist."""
    if s in LITERAL_SKIPS:
        return LITERAL_SKIPS[s]
    for test, reason in SKIP_RULES:
        if test(s):
            return reason
    return None


def docs():
    """Every markdown page in the repo, top level and docs/."""
    return sorted(list(REPO.glob("*.md")) + list((REPO / "docs").glob("*.md")))


def check_links(page, text, bad):
    """Link targets: anything not a URL or a bare anchor has to resolve."""
    for m in LINK.finditer(text):
        target = m.group(1)
        if target.startswith(("http://", "https://", "mailto:", "#")):
            continue
        path = target.split("#")[0]
        if not path:
            continue
        if not (page.parent / path).exists():
            bad.append((page, line_of(text, m.start()), target, "link target"))


def check_code_spans(page, text, bad):
    """Code spans that look like repo paths have to resolve from the root."""
    for m in CODE.finditer(text):
        s = m.group(1)
        if "/" not in s or re.search(r"\s", s):
            continue
        # A trailing slash means a directory, `file.rs:120` names a line,
        # and `./demos` is the same place as `demos`. None of the three
        # changes which file has to exist.
        candidate = s.rstrip("/").split(":")[0]
        if candidate.startswith("./"):
            candidate = candidate[2:]
        if skip_reason(candidate):
            continue
        if not (REPO / candidate).exists():
            bad.append((page, line_of(text, m.start()), s, "code span"))


def line_of(text, index):
    return text.count("\n", 0, index) + 1


# The two front doors. README.md indexes every page in a table; HANDOFF.md
# is the one a newcomer is pointed at first. A page reachable from neither
# is a page nobody finds.
FRONT_DOORS = ["README.md", "docs/HANDOFF.md"]


def check_orphans():
    """Every page has to be reachable from a front door.

    This is the rule that keeps the doc set from growing sideways. Adding
    a page is cheap and linking it is one line, so a page that is worth
    writing is worth putting in the index; one that is not worth indexing
    should not exist. Without this, a doc set gets larger and less useful
    at the same time.
    """
    linked = set()
    for door in FRONT_DOORS:
        text = (REPO / door).read_text()
        for m in LINK.finditer(text):
            target = m.group(1).split("#")[0]
            if target.endswith(".md"):
                linked.add(Path(target).name)

    # notebook.md is the maintainer's working scratchpad of raw feedback,
    # not a documentation page: indexing it from a front door would put
    # unedited notes in the reader's path, and the rule above is about
    # pages written to be read.
    orphans = sorted(
        p.name
        for p in docs()
        if p.name not in linked and p.name not in {"README.md", "notebook.md"}
    )
    if not orphans:
        return []
    return orphans


# One page holding both halves: paths that must be reported, and paths
# that must not be. A gate that reports nothing looks identical to a gate
# that is switched off, and this one was switched off once already: an
# earlier root-segment test skipped `cyclops-proto/src/ledger.rs`, the
# defect the gate was written for.
SELFTEST_PAGE = """# selftest

Must be caught: [gone](docs/nope-{tag}.md), `crates/cyclopsd/src/nope-{tag}.rs`,
and the missing-prefix form `cyclops-proto/src/ledger.rs`.

Must not be caught: `~/.cyclops/config.toml`, `$CYCLOPS_HOME/sock`,
`manifests/*.toml`, `<workspace>/.agents/hooks.json`, `34/33/33`,
`0.34/0.33/0.33`, `target/release/cyclops`, `CYCLOPS_TEST_TMP=/some/dir`,
`/private/tmp`, `bin/commPact`, `versions/2.1.220`,
`crates/cyclops-proto/src/attention.rs`, `./demos/parity-check.sh`,
`.github/workflows/ci.yml`, and [a real link](install.md).
"""

MUST_CATCH = 3


def selftest():
    """Prove the gate reports what it should and nothing else."""
    page = REPO / "docs" / "zz-selftest-doc-paths.md"
    if page.exists():
        print(f"!! {page} is in the way; remove it and run again")
        return 1
    page.write_text(SELFTEST_PAGE.format(tag="xyzzy"))
    try:
        bad = []
        text = page.read_text()
        check_links(page, text, bad)
        check_code_spans(page, text, bad)
    finally:
        page.unlink()

    if len(bad) == MUST_CATCH:
        print(f"== selftest: {MUST_CATCH} planted paths caught, no others")
        return 0
    print(f"== selftest FAILED: expected {MUST_CATCH} findings, got {len(bad)}")
    for _, line, s, kind in bad:
        print(f"  line {line}  {kind}  {s}")
    print(
        "\nFewer than expected means the gate stopped catching something.\n"
        "More means a rule stopped skipping something it should skip."
    )
    return 1


def main():
    if "--selftest" in sys.argv:
        return selftest()

    bad = []
    pages = docs()
    for page in pages:
        text = page.read_text()
        check_links(page, text, bad)
        check_code_spans(page, text, bad)

    orphans = check_orphans()

    if not bad and not orphans:
        print(f"== {len(pages)} pages checked, every path resolves and every page is indexed")
        return 0

    if bad:
        print(f"== {len(bad)} path(s) the docs quote and this repo does not have\n")
        for page, line, s, kind in bad:
            rel = page.relative_to(REPO)
            print(f"  {rel}:{line}  {kind}  {s}")
        print(
            "\nEither the path is wrong, or the file moved and the doc did not.\n"
            "A path that is deliberately not in this repo goes in LITERAL_SKIPS\n"
            "with the reason it is there."
        )

    if orphans:
        print(f"\n== {len(orphans)} page(s) no front door links to\n")
        for name in orphans:
            print(f"  {name}")
        print(
            "\nAdd a row to the table in README.md, or a link from\n"
            "docs/HANDOFF.md. A page nobody can navigate to is a page that\n"
            "goes stale unread; if it is not worth indexing, delete it."
        )
    return 1


if __name__ == "__main__":
    sys.exit(main())
