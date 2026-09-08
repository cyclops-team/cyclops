#!/usr/bin/env python3
"""Exercise installer source selection without network, compilation, or activation."""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


REPO = Path(__file__).resolve().parents[1]


class InstallerSelection(unittest.TestCase):
    def run_selection(
        self, *, options: tuple[str, ...] = (), failure: str = "the build failed", **selection: str
    ) -> str:
        # Substitute only external fetch/build boundaries. Run the actual shell
        # installer, then stop at Cargo before any binary can be activated.
        with tempfile.TemporaryDirectory(
            prefix="cyclops-selection-", dir=os.environ.get("CYCLOPS_TEST_TMP")
        ) as temporary:
            root = Path(temporary)
            clone = root / "checkout"
            (clone / "scripts").mkdir(parents=True)
            shutil.copyfile(REPO / "scripts/install.sh", clone / "scripts/install.sh")
            (clone / "Cargo.toml").write_text("# existing local checkout\n")
            commands = root / "bin"
            commands.mkdir()
            fixtures = {
                "tmux": 'printf "tmux 3.5\\n"\n',
                "curl": '''printf 'curl %s\\n' "$*" >> "$TRACE"
case "$*" in
  *url_effective*) printf 'https://example.invalid/cyclops/releases/tag/v9.8.7'; exit 0 ;;
esac
exit 22
''',
                "git": '''printf 'git %s\\n' "$*" >> "$TRACE"
for destination do :; done
mkdir -p "$destination"
printf '# fetched candidate\\n' > "$destination/Cargo.toml"
''',
                "cargo": '''case "$1" in
  --version) printf 'cargo 1.90.0\\n'; exit 0 ;;
esac
printf 'cargo %s at %s\\n' "$*" "$PWD" >> "$TRACE"
if [ -n "${TEST_CANDIDATE_BUILD:-}" ]; then
    mkdir -p "$CARGO_TARGET_DIR/dist"
    for binary in cyclops cyclopsd; do
        printf '#!/bin/sh\\nif [ "$1" = --version ]; then echo "%s 9.8.7 (%s)"; else echo activation >> "$TRACE"; exit 74; fi\\n' \
            "$binary" "$TEST_CANDIDATE_BUILD" > "$CARGO_TARGET_DIR/dist/$binary"
        chmod 755 "$CARGO_TARGET_DIR/dist/$binary"
    done
    exit 0
fi
exit 73
''',
            }
            for name, body in fixtures.items():
                command = commands / name
                command.write_text("#!/bin/sh\n" + body)
                command.chmod(0o755)
            home = root / "home"
            home.mkdir()
            trace = root / "trace"
            env = {
                "PATH": f"{commands}:/usr/bin:/bin:/usr/sbin:/sbin",
                "HOME": str(home),
                "XDG_CACHE_HOME": str(home / ".cache"),
                "SHELL": "/bin/sh",
                "NO_COLOR": "1",
                "CYCLOPS_NO_RUSTUP": "1",
                "CYCLOPS_REPO": "https://example.invalid/cyclops.git",
                "TRACE": str(trace),
                **selection,
            }
            result = subprocess.run(
                ["sh", str(clone / "scripts/install.sh"), "--no-path", *options],
                env=env, capture_output=True, text=True, timeout=10,
            )
            self.assertNotEqual(result.returncode, 0, "fixture must stop before activation")
            self.assertIn(failure, result.stderr, result.stdout + result.stderr)
            self.assertFalse((home / ".local/bin/cyclops").exists())
            commands_run = trace.read_text()
            self.assertNotIn("activation", commands_run)
            return commands_run

    def test_unavailable_pinned_release_builds_that_tag_not_the_local_checkout(self) -> None:
        trace = self.run_selection(CYCLOPS_VERSION="9.8.7")
        self.assertIn("git clone --depth 1 --branch v9.8.7 ", trace)
        self.assertNotIn("--branch main ", trace)

    def test_latest_fallback_keeps_the_resolved_release_identity(self) -> None:
        trace = self.run_selection()
        self.assertIn("git clone --depth 1 --branch v9.8.7 ", trace)

    def test_explicit_source_ref_skips_releases_and_fetches_requested_ref(self) -> None:
        trace = self.run_selection(CYCLOPS_REF="beta/fix/example")
        self.assertNotIn("curl ", trace)
        self.assertIn("git clone --depth 1 --branch beta/fix/example ", trace)

    def test_explicit_source_builds_the_supplied_checkout(self) -> None:
        trace = self.run_selection(options=("--source",))
        self.assertNotIn("curl ", trace)
        self.assertNotIn("git clone ", trace)
        self.assertIn("/checkout\n", trace)
        self.assertIn("cargo build --locked ", trace)

    def test_source_update_rejects_wrong_commit_before_activation(self) -> None:
        self.run_selection(
            options=("--source",), failure="does not match requested",
            CYCLOPS_EXPECTED_BUILD="a" * 40, TEST_CANDIDATE_BUILD="b" * 7,
        )

    def test_relative_cargo_target_is_read_from_the_source_checkout(self) -> None:
        # Cargo writes relative targets inside the checkout, not the caller's
        # directory. Finding that candidate must reach the identity check.
        self.run_selection(
            options=("--source",), failure="does not match requested",
            CARGO_TARGET_DIR="relative-target", CYCLOPS_EXPECTED_BUILD="a" * 40,
            TEST_CANDIDATE_BUILD="b" * 7,
        )

    def test_source_update_rejects_unknown_or_dirty_build_before_activation(self) -> None:
        for build in ("unknown", "aaaaaaa.dirty"):
            with self.subTest(build=build):
                self.run_selection(
                    options=("--source",), failure="has no clean commit identity",
                    CYCLOPS_EXPECTED_BUILD="a" * 40, TEST_CANDIDATE_BUILD=build,
                )


if __name__ == "__main__":
    unittest.main()
