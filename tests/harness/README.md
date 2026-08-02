# Probe harness

Python helpers for driving vendor agent TUIs (Claude Code, Codex CLI, agy)
inside isolated tmux servers. Ported nearly verbatim from the ADR-001
validation campaign (`cyclops-arch/validation/scripts/`), where they ran the
563-delivery soak at zero unrecovered loss. This is the seed of the
regression suite: when a vendor CLI updates, or a manifest rule is in doubt,
a probe test here re-measures reality instead of trusting notes.

Files:

- `tuikit.py`: tmux driver. Launch a TUI in an isolated server, capture the
  screen, read `#{pane_title}`, paste via unique-named buffers, detect and
  safely dismiss modals (`MODAL_SIGNS`, `dismiss_modal`), tail hook logs.
- `hooklog.py`: the hook command wired into vendor hook configs. Appends one
  NDJSON line per hook event, with `--event <name>` self-tagging because agy
  payloads carry no event-name field (finding F7).
- `test_vocab.py`: modal-vocabulary regression test, no tmux needed. Locks
  the trust-dialog handling (explicit affirmative, never Escape on a dialog
  whose text says Esc cancels/exits) against the real Claude 2.1.220
  capture. Run directly: `python3 tests/harness/test_vocab.py`.

## The rule: never touch the live session

Probes must never interact with the user's tmux server. Non-negotiable:

- Every tmux call goes through an isolated server: `tmux -L <socket> -f
  /dev/null`. `tuikit.py` enforces this by routing everything through `SOCK`.
- One unique socket per test, including the process id, e.g.
  `cyc-probe-f13-$$`. Set it via `CYC_HARNESS_SOCK` before importing tuikit.
- Teardown kills that server (`tmux -L <socket> kill-server`), even on
  failure. A leaked server is a bug in the probe.
- Never call bare `tmux`. Bare `tmux` is the user's server.

## Writing a probe test

A probe is a small script that measures one question about a vendor TUI,
validation-campaign style. Shape:

```python
import os
os.environ["CYC_HARNESS_SOCK"] = f"cyc-probe-mytest-{os.getpid()}"
import tuikit as tk

log = open(os.path.join(tk.raw_dir(), "mytest.log"), "w")
try:
    tk.launch("probe", "claude", cwd="/private/tmp/probe-scratch")
    cap = tk.wait_composer("probe", tk.claude_ready, log)
    tk.paste("probe", "reply ok [m-test1]")
    # ... measure, assert, record ...
finally:
    tk.tmux("kill-server")
```

Conventions, inherited from the campaign:

- One question per probe. Freeze the pass criterion before collecting data.
- Timestamps in `time.time_ns()`; latencies reported as measured, with the
  method's known overhead stated (see hooklog.py's docstring).
- Raw artifacts (captures, hook logs, ledgers) go under `tests/raw/`
  (`tuikit.raw_dir()`), which is gitignored. Keep them; they are the
  evidence.
- Agent runs use scratch working directories under `/private/tmp`, never the
  repo. Note finding F8: agy walks above its cwd for context files.
- Modals are dismissed only via explicit decline options. Never a bare Enter
  (findings F3, F12).

## Where results go

Anything MEASURED that contradicts the manifests, the ADR, or the build
notes becomes a numbered entry in the repo root `findings.md` (numbering
continues from the campaign's F1-F12), labelled MEASURED or READ, naming the
probe that proved it. Manifest fixes derived from a finding go to
`manifests/*.toml` in the same change.
