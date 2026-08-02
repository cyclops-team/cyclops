#!/usr/bin/env python3
"""Modal vocabulary regression test for the harness (no tmux, no TUIs).

Locks the m1-soak fix: Claude 2.1.220's folder-trust dialog ('Quick safety
check: Is this a project you created or one you trust?', 'Esc to cancel')
must be answered with the explicit affirmative for harness-owned scratch
dirs, and Escape must NEVER be sent to a dialog whose text says Esc
cancels/exits, because Escape exits the CLI there (cost a soak leg).

Key sends are stubbed; the test asserts on the exact key sequence. Run:
python3 tests/harness/test_vocab.py
"""
import io
import os
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(REPO, "tests"))
os.environ.setdefault("CYC_HARNESS_SOCK", f"cyc-vocab-test-{os.getpid()}")

import tuikit  # noqa: E402
import m1_soak  # noqa: E402

# The real dialog capture, same evidence the manifest tests lock against.
TRUST_DIALOG = open(os.path.join(
    REPO, "crates/cyclops-manifest/tests/fixtures/claude_trust_dialog.txt")).read()

UNKNOWN_ESC_EXITS = "Strange new dialog\n1. Continue\nEnter to confirm . Esc to cancel"
STARTUP_MODAL = "Claude in Chrome extension detected\n❯ 1. Yes\nEnter to confirm . Esc to keep"
# The codex update dialog whose preselected option runs a package upgrade
# (F3). The measured decline is 3 = Skip until next version, never 2.
UPDATE_DIALOG = ("✨ Update available! 0.145.0 -> 0.146.0\n"
                 "› 1. Update now (runs `brew upgrade --cask codex`)\n"
                 "3. Skip until next version\nPress enter to continue")

time.sleep = lambda s: None  # keys are recorded, spacing is irrelevant


def record_tuikit(cap, cli=None):
    sent = []
    tuikit.send_key = lambda target, key: sent.append(key)
    tuikit.send_enter = lambda target: sent.append("Enter")
    tuikit.dismiss_modal("t", cap, io.StringIO(), cli=cli)
    return sent


def record_soak(cli, cap):
    sent = []
    m1_soak.send_key = lambda target, key: sent.append(key)
    m1_soak.dismiss_modal(cli, "t", cap, io.StringIO())
    return sent


def main():
    # Detection: the trust wording is modal vocabulary in both copies.
    assert tuikit.modal_visible(TRUST_DIALOG), "tuikit misses the trust dialog"
    assert m1_soak.modal_visible(TRUST_DIALOG), "m1_soak misses the trust dialog"
    assert "trust this folder" in tuikit.MODAL_SIGNS
    assert "safety check" in tuikit.MODAL_SIGNS

    # Trust dialog: explicit affirmative, never Escape (Escape exits the CLI).
    for name, sent in (("tuikit", record_tuikit(TRUST_DIALOG)),
                       ("m1_soak", record_soak("claude", TRUST_DIALOG))):
        assert sent == ["1", "Enter"], f"{name} trust dialog sent {sent}"
        assert "Escape" not in sent, f"{name} sent Escape into the trust dialog"

    # Unknown dialog saying Esc cancels/exits: stand off, send nothing.
    for name, sent in (("tuikit", record_tuikit(UNKNOWN_ESC_EXITS)),
                       ("m1_soak", record_soak("claude", UNKNOWN_ESC_EXITS))):
        assert sent == [], f"{name} pressed keys into an Esc-exits dialog: {sent}"

    # Regression guard: 'Esc to keep' dialogs still decline with Escape.
    assert record_tuikit(STARTUP_MODAL) == ["Escape"]
    assert record_soak("claude", STARTUP_MODAL) == ["Escape"]

    # Codex update dialog: the measured decline is 3 (Skip until next
    # version, F3); other CLIs keep 2=Skip. Same vocabulary in both copies.
    assert record_tuikit(UPDATE_DIALOG, cli="codex") == ["3", "Enter"], \
        "tuikit must decline the codex update dialog with 3"
    assert record_tuikit(UPDATE_DIALOG) == ["2", "Enter"]
    assert record_soak("codex", UPDATE_DIALOG) == ["3", "Enter"]
    assert record_soak("claude", UPDATE_DIALOG) == ["2", "Enter"]

    print("test_vocab: all assertions passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
