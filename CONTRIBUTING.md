# Working on cyclops

Build it, change it, prove it still works. This page is the loop.

If you are installing Cyclops to use it, read [install.md](docs/guides/install.md)
instead. If you are trying to find your way around the code, read
[HANDOFF.md](docs/development/HANDOFF.md). If you are about to change delivery, the ledger,
or anything that renders, read [INVARIANTS.md](docs/development/INVARIANTS.md) first.

## Build

```bash
cargo build
```

Two binaries land in `target/debug/`: `cyclopsd` (the daemon) and `cyclops`
(the CLI). Nothing else is produced and nothing is installed.

You need tmux on PATH to run most of the tests. `tmux -V` should print 3.2
or newer; the tree is developed against 3.6a and CI also builds tmux master.
Install the same nextest release CI uses before running the full gate:

```bash
cargo install cargo-nextest --locked --version 0.9.100
```

## The loop

Five gates, in this order. They are the same five core CI gates, so a green
run here is a green run there. The Rust test gate uses nextest for executable
tests and Cargo for doctests, which nextest does not run.

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace -E 'not package(cyclopsd)' --no-fail-fast
cargo test -p cyclopsd --all-targets --no-fail-fast
cargo test --workspace --doc
python3 scripts/check-doc-paths.py
./tests/e2e/parity-check.sh
```

Clippy and the test suite take a few minutes; the rest are usually quick.
Run formatting and clippy while you work.

One command runs all five in that order, cheapest first, with per-gate
timing: `./scripts/check.sh`. Its `--fast` flag stops after the test
suite, which is the right pass while iterating; the full run is for the
moment before a push. Neither changes any flag CI uses.

While working inside one crate, the honest inner loop is smaller still:
`cargo test -p <crate>` plus clippy on save. The full workspace suite
exists to catch what crosses crate boundaries, and it only has to run
before the tree leaves your machine.

Touching `scripts/install.sh`, `website/static/install.sh`, or installer
output adds one more check, and it does a release build. The two installer
files must remain byte-for-byte identical:

```bash
cmp scripts/install.sh website/static/install.sh
./tests/e2e/parity-check.sh --with-installer
```

### `--no-fail-fast` is not optional

Nextest otherwise stops scheduling after the first failure and can hide the
remaining failures. One portability bug once passed for a green build across
two milestones because the visible failure count did not cover the full suite
(F24). Always pass the flag. CI does.

### What the parity gate is

`tests/e2e/parity-check.sh` runs every command shape the README and `docs/`
quote, for real, on a throwaway tmux server, and checks the output against
what the binaries print today. A doc describing output that no longer
exists is a bug (STYLE.md, truth rule); this is the regression that catches
it.

```
$ ./tests/e2e/parity-check.sh
== rig home:   /private/tmp/cyclops-parity.tMpeYD/home (removed on exit)
== tmux:       private TMUX_TMPDIR=/private/tmp/cyclops-parity.tMpeYD/tmux (removed on exit)
== building cyclops and cyclopsd

#### Rung 1: one pane, persistence, history

$ cyclops start
✓ workspace ready · 1 agent
  wrote /private/tmp/cyclops-parity.tMpeYD/home/config.toml
...
== 115/115 checks passed
== docs and binaries agree
```

Its transcript is where the output blocks in the docs come from. If you
change a line a doc quotes, this fails, and the fix is to copy the new
output out of the transcript into the page in the same commit. Never
hand-write output into a doc.

## Two rules that will bite you in the first hour

### Tests never touch your tmux server or your real `~/.cyclops`

Every test that needs tmux gets its own server through
`tests/testrig`. Never `tmux` directly, never the default server,
and never stop a server yourself: dropping the rig is how it stops.

```rust
use cyclops_testrig::{tmux_available, TmuxServer};

if !tmux_available() {
    return;                                // skip cleanly, never fail
}
let rig = TmuxServer::new("my-feature");   // -L cyc-my-feature-<pid>
rig.run_ok(&["new-session", "-d", "-s", "demo"]);
// teardown is Drop: stop the server, then unlink the socket file
```

The reasons are all measured, and they are written out in that crate's
header. The short version: `-L cyc-<tag>-<pid>` keeps concurrent test
binaries off each other, `-f /dev/null` keeps your tmux config from
changing behavior, `-u` stops tmux sanitizing tabs and non-ASCII to `_`
(F14, which silently destroys the title sensor), and teardown must both
stop the server and unlink the socket file, because stopping a server
unlinks nothing and a server that exits on its own leaves the file too.

Teardown lives in `Drop` so it also runs when a test panics. Tearing down
in a straight line at the end of a test body leaks a live server the first
time an assertion fails.

Homes work the same way: point `CYCLOPS_HOME` at a scratch directory. A
test that writes to the real one corrupts your own message history.

There are two guards, and they exist because this rule was fixed three
times and kept getting copied back in:

- `tests/testrig/tests/teardown_has_one_home.rs` fails if any
  other Rust file starts or kills a tmux server.
- `tests/testrig/tests/shell_teardown.rs` holds `tests/e2e/lib/lib.sh`,
  the shell home of the same rule, to the same contract.

Shell and Python go through `tests/e2e/lib/lib.sh`. Source it, never paste from it.

### Scratch paths come from `cyclops_proto::scratch`

```rust
use cyclops_proto::scratch::scratch_dir;

let home = scratch_dir("my-feature");   // <root>/my-feature-<pid>
```

Not `/private/tmp`, not `std::env::temp_dir()`, not a string literal.

Both halves of that matter. `/private/tmp` is macOS-only: it is the real
`/tmp` there and it does not exist on Linux, where `/private` is not
writable, so a hardcoded path fails every Linux test that creates a
directory. And `std::env::temp_dir()` on macOS is a long
`/var/folders/...` path, which blows the ~104-byte cap on a Unix socket
path, so a daemon socket created under it fails to bind. `scratch_root`
states that once, and `CYCLOPS_TEST_TMP` overrides it.

That is F24, and it cost two milestones and two red CI runs to learn.

Prove it still holds by relocating the root and running the suite again.
On macOS a relocated run takes the same code path Linux does:

```bash
mkdir -p /private/var/tmp/cyc-relocated
CYCLOPS_TEST_TMP=/private/var/tmp/cyc-relocated cargo nextest run --workspace -E 'not package(cyclopsd)' --no-fail-fast
CYCLOPS_TEST_TMP=/private/var/tmp/cyc-relocated cargo test -p cyclopsd --all-targets --no-fail-fast
```

CI runs the whole suite twice for this reason, once relocated.

## Demos

`demos/` holds runnable end-to-end scripts, one per milestone. Each one
builds an isolated rig, drives a real scenario, and prints what happened.

```bash
./demos/m1-send.sh        # a message from send to verified receipt
./demos/m4-workspace.sh   # build, name, save, kill, restore a workspace
./demos/m5-theme.sh       # a theme switch reaching a real pane border
```

All of them need `tmux`; the ones that read the ledger back
(`m1-send.sh`, `m2-conversation.sh`, and `m3-stream.sh`)
also need `jq`, and the first three need `python3`. They check for what
they use and say so. None of them touches your tmux server or your home,
and all are safe to run repeatedly.

Write one when you ship anything a user will do end to end. This is not
ceremony: on this codebase the demos have found defects that reading the
code did not, three times in M4 and once in M5, every one of them shipped
and working code that was wrong in a way only running it revealed. A demo
exercises the seams between the daemon, the CLI, tmux and the files on
disk, and that is where the bugs were.

`bash -n demos/<name>.sh` must always pass.

## What CI runs

`.github/workflows/ci.yml`, on ubuntu-latest and macos-latest, with
`fail-fast: false` so one platform failing cannot cancel the other and
throw away the signal that tells a portability bug from a real regression.
The current lane responsibilities, stable check names, and measured beta
baseline are recorded in [CI evidence lanes and baseline](docs/development/CI.md).

| Step | Fails when |
|---|---|
| `cargo fmt --all --check` | Formatting drifted |
| `cargo clippy --workspace --all-targets -- -D warnings` | Any lint fires, including in tests |
| `cargo nextest run --workspace -E 'not package(cyclopsd)' --no-fail-fast` | Any parallel-safe test fails, on either OS |
| `cargo test -p cyclopsd --all-targets --no-fail-fast` | Any daemon test fails under its process-isolated rig contract |
| `cargo test --workspace --doc` | A Rust doctest fails |
| `python3 scripts/check-doc-paths.py` | A doc points at a file this repo does not have, or a page exists that no front door links to. `--selftest` proves the checker still catches, so a green run cannot mean it stopped looking |
| `./tests/e2e/parity-check.sh` | A doc quotes output the binaries no longer print |
| The whole suite again with `CYCLOPS_TEST_TMP` relocated | Something hardcoded a scratch path (F24) |
| `./tests/e2e/parity-check.sh --with-installer` | `scripts/install.sh` stopped doing what install.md says, or left a shell profile changed after `--uninstall`. Its own job: it does a release build |
| `cmp scripts/install.sh website/static/install.sh`, then `npm run check` and `npm run build` in `website/` | The hosted installer drifted from the tested installer, or the website no longer type-checks or builds |

Another job builds tmux from master and runs the suite against it. It is
`continue-on-error`, so it warns rather than blocks: tmux is not this
repo's to fix. It has earned its keep once already (F25), so when it goes
red, read it rather than assuming it is the usual noise.

Local green plus red CI is not a flaky-CI story until the logs say so.

## House rules for a change

- **[STYLE.md](docs/development/STYLE.md) is binding**, on code, comments and docs. Read it
  once before your first change. The shortest version: write for a tired
  engineer who has never seen this repo.
- **A behavior fix needs a test that fails before it.** Write the test,
  watch it fail, then fix it. A test written after the fix proves the code
  passes its own test.
- **Docs ship in the same commit as the behavior.** A page describing
  something that no longer exists is a bug, not a stale doc.
- **Wire changes are additive.** New fields are optional; unknown fields
  are ignored in both directions. The daemon writes a hello line first and
  a version mismatch warns rather than rejects ([PROTOCOL.md](docs/reference/PROTOCOL.md)).
- **Vendor behavior goes in a manifest, not in Rust**
  ([MANIFESTS.md](docs/reference/MANIFESTS.md)).
- **No polling.** If you are reaching for an interval, you have not found
  the event yet ([INVARIANTS.md](docs/development/INVARIANTS.md), rule 9).
- **Record what you measured.** If you learned something about tmux, a
  vendor CLI, or a platform that contradicts what the code assumed, it goes
  in `findings.md` with the probe that proved it. Those entries are worth
  more than the fixes they caused.
