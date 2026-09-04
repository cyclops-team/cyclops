#!/usr/bin/env python3
"""Check that every repo path a doc quotes actually exists.

A path in a doc is checkable, and until this existed it was not checked:
docs/development/ARCHITECTURE.md once pointed newcomers at two files that
were not in this repo, and thirty-one source paths were written without
their source-directory prefix, so copying one into an editor found nothing.

Two kinds of path get checked, because a reader uses them differently:

1. Markdown link targets, `[text](docs/install.md)`. A link that does not
   resolve is broken navigation.
2. Inline code spans that look like repo paths, `src/cyclopsd/src`.
   A reader copies these into an editor or a command.

Run it: python3 scripts/check-doc-paths.py
Exit 0 when every path resolves, 1 with a report when one does not.
"""

import json
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

# Two that no rule can express, because they are real paths in a tree this
# repo is not. Both are v1, quoted by history pages, and
# both are correct where they appear.
#
# Two more that are not paths in ANY tree: the manifest/theme runtime
# fallback is `./manifests` or `./themes` relative to whatever directory
# the process was started from, a cwd-relative convention that is
# unrelated to this repo's own layout even when the two coincide (docs and
# code comments call this out explicitly, e.g. HANDOFF.md, themes.md). A
# code span here always strips a leading `./`, so the checkable candidate
# is the bare word.
LITERAL_SKIPS = {
    "bin/commPact": "a path in the predecessor v1 tree, quoted as history in BENCHMARKS.md",
    "versions/2.1.220": "a v1 release directory, quoted as history",
    "manifests": "the daemon's cwd-relative fallback directory name, not a path this repo ships",
    "themes": "the theme engine's cwd-relative fallback directory name, not a path this repo ships",
    # Keyed without the trailing slash: `check_code_spans` normalizes a
    # candidate before it gets here, so `tests/raw/` in a doc arrives as
    # `tests/raw`.
    "tests/raw": "soak output, gitignored on purpose (/tests/raw/ in .gitignore): 1.9MB of daemon logs and pane captures from real agent CLIs",
    "tests/raw/m1-soak/summary.json": "one such run's verdict, quoted by BENCHMARKS.md as a local artifact",
    "tests/raw/m1-soak-2/summary.json": "one such run's verdict, quoted by BENCHMARKS.md as a local artifact",
    "@src/main.rs": "a reference in the OPERATOR's project, not this one: workspace-ui.md uses it to show that a file panel reference stays relative to the pane's folder after browsing elsewhere",
    ".codex/hooks.json": "a project-local hooks file in whatever project Codex CLI is running, not this repo: docs/public/reference/configuration.mdx names it to explain why project-local hooks silently never fire",
}

# A code span is a candidate when it has a slash and no whitespace. That
# is the whole shape test, deliberately.
#
# An earlier version also required the first segment to be a directory at
# the repo root, and that version skipped `cyclops-proto/src/ledger.rs`,
# which is the exact defect this gate was written for: thirty-one source
# paths missing their source-directory prefix. Measured against these docs, the
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
    """Every documentation page in the repo.

    Top level and docs/ (recursively) for `.md`; docs/ is split into
    guides/, reference/, and development/. docs/public/ is Mintlify's
    published-docs tree and holds `.mdx` instead, checked the same way:
    its links and code-span paths are read by the same reader, so a wrong
    path there is the same defect.
    """
    return sorted(
        list(REPO.glob("*.md"))
        + list((REPO / "docs").rglob("*.md"))
        + list((REPO / "docs" / "public").rglob("*.mdx"))
    )


MINTLIFY_ROOT = REPO / "docs" / "public"
MINTLIFY_CONFIG = MINTLIFY_ROOT / "docs.json"


def mintlify_page_path(slug):
    """The `.mdx` file a Mintlify site-absolute link or nav entry names.

    Mintlify addresses a page by slug from the site root, not by filesystem
    path: `/guides/recovery` in a link, or `"guides/recovery"` in
    docs.json, both mean `docs/public/guides/recovery.mdx`.
    """
    return MINTLIFY_ROOT / (slug.strip("/") + ".mdx")


def mintlify_navigation_slugs():
    """Every page slug docs.json's `navigation` tree names.

    docs.json is Mintlify's site config; `navigation` is a tree of groups,
    tabs, and page-slug strings. This walks any shape of it rather than
    assuming today's `{"pages": [...]}` layout, so a future tab or anchor
    group is still found instead of silently unindexed.
    """
    if not MINTLIFY_CONFIG.exists():
        return []
    config = json.loads(MINTLIFY_CONFIG.read_text())
    slugs = []

    def walk(node):
        if isinstance(node, str):
            slugs.append(node)
        elif isinstance(node, list):
            for item in node:
                walk(item)
        elif isinstance(node, dict):
            for value in node.values():
                walk(value)

    walk(config.get("navigation", {}))
    return slugs


def check_links(page, text, bad):
    """Link targets: anything not a URL or a bare anchor has to resolve."""
    for m in LINK.finditer(text):
        target = m.group(1)
        if target.startswith(("http://", "https://", "mailto:", "#")):
            continue
        path = target.split("#")[0]
        if not path:
            continue
        if page.suffix == ".mdx" and path.startswith("/"):
            if not mintlify_page_path(path).exists():
                bad.append((page, line_of(text, m.start()), target, "mdx link target"))
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
            # Report the normalized candidate, not the span as written. A
            # LITERAL_SKIPS entry is matched against this string, so
            # printing `tests/raw/` while the lookup wants `tests/raw`
            # sends the author to add a key that can never fire.
            bad.append((page, line_of(text, m.start()), candidate, "code span"))


def line_of(text, index):
    return text.count("\n", 0, index) + 1


# The two front doors. README.md serves users; HANDOFF.md serves contributors.
# They may link to section indexes, which in turn link to individual pages.
# Requiring every page directly in a front door made README.md an unhelpful
# flat inventory and discouraged useful hierarchy.
FRONT_DOORS = ["README.md", "docs/development/HANDOFF.md"]


def linked_markdown_pages(page):
    """Existing in-repo Markdown pages linked from one page."""
    targets = []
    text = page.read_text()
    for m in LINK.finditer(text):
        target = m.group(1).split("#")[0]
        if not target or target.startswith(("http://", "https://", "mailto:")):
            continue
        candidate = (page.parent / target).resolve()
        try:
            candidate.relative_to(REPO)
        except ValueError:
            continue
        if candidate.suffix == ".md" and candidate.exists():
            targets.append(candidate)
    return targets


def check_orphans():
    """Every page has to be reachable from a front door.

    Reachability, rather than a direct link, is the useful rule. It permits
    public, reference, and engineering indexes while still catching pages
    that no reader can navigate to. Without this, a doc set either grows
    sideways or forces its root README to become a file inventory.

    docs/public/ is a third front door, not reached by walking README.md or
    HANDOFF.md: a Mintlify page is what a reader gets to through the
    published site's own navigation, and that navigation is docs.json, not
    a Markdown link chain. A page docs.json does not name is unreachable on
    the actual published site even if some other page happens to link it.
    """
    reachable = set()
    pending = [(REPO / door).resolve() for door in FRONT_DOORS]
    while pending:
        page = pending.pop()
        if page in reachable:
            continue
        reachable.add(page)
        pending.extend(linked_markdown_pages(page))

    for slug in mintlify_navigation_slugs():
        path = mintlify_page_path(slug)
        if path.exists():
            reachable.add(path.resolve())

    # notebook.md is the maintainer's working scratchpad of raw feedback,
    # not a documentation page: indexing it from a front door would put
    # unedited notes in the reader's path, and the rule above is about
    # pages written to be read.
    orphans = sorted(
        str(p.relative_to(REPO))
        for p in docs()
        if p.resolve() not in reachable and p.name != "notebook.md"
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

Must be caught: [gone](docs/nope-{tag}.md), `src/cyclopsd/src/nope-{tag}.rs`,
and the missing-prefix form `cyclops-proto/src/ledger.rs`.

Must not be caught: `~/.cyclops/config.toml`, `$CYCLOPS_HOME/sock`,
`resources/manifests/*.toml`, `<workspace>/.agents/hooks.json`, `34/33/33`,
`0.34/0.33/0.33`, `target/release/cyclops`, `CYCLOPS_TEST_TMP=/some/dir`,
`/private/tmp`, `bin/commPact`, `versions/2.1.220`, `./manifests`, `./themes`,
`src/cyclops-proto/src/attention.rs`, `./tests/e2e/parity-check.sh`,
`.github/workflows/ci.yml`, and [a real link](guides/install.md).
"""

# A second probe, planted as `.mdx` under docs/public/, proves the
# Mintlify-specific branch in check_links: a site-absolute slug resolves
# against docs/public/, not against the page's own directory the way an
# ordinary Markdown link does.
SELFTEST_MDX_PAGE = """---
title: "selftest"
---

Must be caught: [gone](/nope-{tag}).

Must not be caught: [a real page](/introduction).
"""

MUST_CATCH = 3
MDX_MUST_CATCH = 1


def selftest():
    """Prove the gate reports what it should and nothing else."""
    page = REPO / "docs" / "zz-selftest-doc-paths.md"
    mdx_page = MINTLIFY_ROOT / "zz-selftest-doc-paths.mdx"
    if page.exists():
        print(f"!! {page} is in the way; remove it and run again")
        return 1
    if mdx_page.exists():
        print(f"!! {mdx_page} is in the way; remove it and run again")
        return 1

    page.write_text(SELFTEST_PAGE.format(tag="xyzzy"))
    mdx_page.write_text(SELFTEST_MDX_PAGE.format(tag="xyzzy"))
    try:
        bad = []
        check_links(page, page.read_text(), bad)
        check_code_spans(page, page.read_text(), bad)
        mdx_bad = []
        check_links(mdx_page, mdx_page.read_text(), mdx_bad)
    finally:
        page.unlink()
        mdx_page.unlink()

    want = MUST_CATCH + MDX_MUST_CATCH
    got = len(bad) + len(mdx_bad)
    if got == want:
        print(f"== selftest: {want} planted paths caught, no others")
        return 0
    print(f"== selftest FAILED: expected {want} findings, got {got}")
    for _, line, s, kind in bad + mdx_bad:
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
            "\nLink each page from the relevant public, reference, or engineering\n"
            "index. A page nobody can navigate to goes stale unread; if it is not\n"
            "worth indexing, delete it."
        )
    return 1


if __name__ == "__main__":
    sys.exit(main())
