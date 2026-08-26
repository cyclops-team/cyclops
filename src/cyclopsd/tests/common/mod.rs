//! Shared rig for the cyclopsd integration tests: isolated tmux server,
//! scratch home, in-process daemon, NDJSON test clients.
//!
//! The tmux server and its teardown come from `cyclops-testrig`, which is
//! where the isolation flags and the kill-then-unlink rule are stated. Do
//! not re-implement either here; that is how the socket leak survived
//! three rounds of fixes. Bounded poll loops in this file are test-side
//! waits, explicitly outside the daemon's zero-polling contract.
// One rig serving every test binary in this crate: no single binary uses
// every helper or every re-export, and each compiles this file on its own.
#![allow(dead_code, unused_imports)]

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use cyclops_proto::{DeliveryState, Kind, LedgerLine};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const MAX_CONCURRENT_RIGS: usize = 8;

fn rig_slots() -> &'static Arc<Semaphore> {
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SLOTS.get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_RIGS)))
}

// ---------------------------------------------------------------------------
// Fixture manifests
// ---------------------------------------------------------------------------

/// Screen-tier fixture bound to plain cat/sh panes: title tier always says
/// idle, staging is verified by the message id, no hook ACK (tier 2).
/// A deterministic composer process for the lifecycle fixtures.
///
/// `cat` cannot model a composer: it echoes a paste and never gives the
/// screen back, so the staged marker never leaves and the screen-evidence
/// ACK tier can never resolve. This read loop models the two things a real
/// composer does.
///
/// A paste arrives as one burst, so the tty echoes every row of it at once,
/// including the final sentinel row, which carries no newline and therefore
/// stays in the input buffer as staged text. The loop consumes the
/// newline-terminated rows silently, leaving that echo on screen for the
/// staging check to find. Only when the daemon's Enter finally terminates
/// the sentinel row does the loop repaint: screen cleared, prompt back,
/// marker gone, which is the evidence the screen ACK tier waits for.
/// Repainting on every row instead would erase the sentinel's echo before
/// anything could verify it.
///
/// Lifecycle only. It proves receipts and liveness, not INVARIANTS rule 12
/// and not sentinel completeness; a pane that paints no chrome under a
/// paste cannot decide terminality at all.
/// The composer process behind the lifecycle fixtures: a small fake TUI
/// that stages a paste, paints chrome below it, and consumes the staged
/// text when the submit key arrives.
///
/// It exists because `cat` cannot model a composer. `cat` echoes a paste
/// and never gives the screen back, so the staged marker never leaves
/// and no receipt can resolve, and it paints nothing below the paste, so
/// terminality is undecidable and the sentinel can never be proven
/// last. Fixtures built on it end up claiming a representation the pane
/// does not produce.
///
/// See `common/faketui.py` for what it draws.
pub fn composer_pane() -> String {
    format!("python3 {}", faketui_path())
}

/// A composer whose lifecycle is controlled by explicit test-only keys.
pub fn manual_lifecycle_composer_pane() -> String {
    format!("python3 {} --manual-lifecycle", faketui_path())
}

/// The composer that ACCEPTS the submit key and does nothing with it:
/// the staged text stays put and no turn runs.
///
/// The staged-never-sent class c shape, which is what a modal or a mode
/// does to an Enter. `send-keys` succeeds, so any inference from that
/// success is wrong here, and the payload is still in the composer.
pub fn swallowing_composer_pane() -> String {
    format!("python3 {} --swallow-submit", faketui_path())
}

/// The swallowing composer also repaints its status row after Enter.
/// The exact staged row remains, so the repaint is not a receipt.
pub fn swallowing_animated_composer_pane() -> String {
    format!(
        "python3 {} --swallow-submit --animate-after-swallow",
        faketui_path()
    )
}

/// The fake TUI's script path, for fixtures that start it themselves
/// (a pane whose root is a shell types the command instead of tmux
/// exec'ing it).
pub fn faketui_path() -> String {
    format!("{}/tests/common/faketui.py", env!("CARGO_MANIFEST_DIR"))
}

/// The same process after a fixture's own setup has run, for panes that
/// must first show a modal or a banner and then behave like a composer.
pub fn composer_tail() -> String {
    format!(
        "exec python3 {}/tests/common/faketui.py",
        env!("CARGO_MANIFEST_DIR")
    )
}

pub const CAT_MANIFEST: &str = r#"
[agent]
id = "fix"
display_name = "Cat fixture"
# Keep the GitHub Actions driver out of the fixture's vendor set. Otherwise
# socket requests from the test process look like an agent outside the rig.
process_names = ["python3", "python", "Python", "cat", "sh", "dash"]

[[rule]]
id = "always_idle"
state = "idle"
priority = 100
region = "pane_title"
regex = ['^']

# The screen sensor must be able to answer for these panes, because a
# write needs positive clean-composer evidence from the sensor that sees
# the composer (INVARIANTS rule 12). A blank fixture pane IS a clean
# composer, so the empty bottom region is the evidence.
[[rule]]
id = "composer_empty"
state = "idle"
composer_semantic = "clean"
priority = 90
region = "bottom_non_empty_lines(4)"
line_regex = ['^❯\s*$']

# The FINAL payload row, which is the one row a fixed bottom region can
# never lose: `cat` echoes every pasted line, so the header scrolls away
# while the sentinel stays. Paired with the generic '[cyclops:end '
# pattern, this is what lets marker_in_composer reach the submit step.
#
# These shared fixtures exercise delivery LIFECYCLE only. They do not
# prove INVARIANTS rule 12 and they do not prove sentinel completeness:
# a blank pane paints no chrome under a paste, so terminality is not
# decidable here at all. Those claims belong to the dedicated tests in
# m1_blockers.rs and the sentinel unit tests in delivery.rs.
[[rule]]
id = "composer_holds_paste"
state = "idle_with_input"
composer_semantic = "human_input"
priority = 80
# The active composer only. The escaped matcher rejects the dim transcript
# prompt that the fixture paints after consuming a turn.
region = "bottom_non_empty_lines(3)"
line_regex = ['^\s*❯\s+\S']
line_regex_esc = ['^❯']

# The transient row the composer paints after consuming the sentinel.
# Screen ACK evidence needs a turn, and a generic-marker staging does not
# license "the window changed" as proof of one.
[[rule]]
id = "composer_working"
state = "working"
priority = 300
region = "bottom_non_empty_lines(5)"
line_regex = ['^FAKETUI-WORKING$']

[injection]
method = "load-buffer + paste-buffer -p"
submit = "Enter"
verify_before_submit = true
verify_pattern = ["<message_id>"]
# The staged sentinel row as this pane renders it. A shell composer echoes
# the paste unstyled, so the two halves of the pair are the same shape
# here; a real vendor's chip is painted and its escaped half says so.
# What the fake TUI paints below the composer, in order and in both
# forms. Two rows, both required, both painted: this is the authentic
# sentinel lane rather than a chip declared over raw text.
composer_trailer_regex = ['^─+$', '^Model \S+ · Ctx: \d+%$']
composer_trailer_regex_esc = ['^\x1b\[38;5;244m─', '^\x1b\[38;5;152mModel\b']
composer_trailer_required_prefix = 2
composer_prompt_regex = '^❯ (?P<content>.*)$'
composer_continuation_regex = '^(?P<content>.*)$'
safe_states = ["idle"]
"#;

/// Tier-1 fixture: same pane behavior plus a payload-matchable hook ACK.
pub const HOOK_MANIFEST: &str = r#"
[agent]
id = "fix"
display_name = "Hook fixture"
# Keep the GitHub Actions driver out of the fixture's vendor set. Otherwise
# socket requests from the test process look like an agent outside the rig.
process_names = ["python3", "python", "Python", "cat", "sh", "dash"]

[hooks]
turn_start = "UserPromptSubmit"
turn_start_evidence = "confirmed"
turn_end = "Stop"
turn_end_evidence = "confirmed"
ack = "UserPromptSubmit"
ack_payload_field = "prompt"

[[rule]]
id = "always_idle"
state = "idle"
priority = 100
region = "pane_title"
regex = ['^']

# The screen sensor must be able to answer for these panes, because a
# write needs positive clean-composer evidence from the sensor that sees
# the composer (INVARIANTS rule 12). A blank fixture pane IS a clean
# composer, so the empty bottom region is the evidence.
[[rule]]
id = "composer_empty"
state = "idle"
composer_semantic = "clean"
priority = 90
region = "bottom_non_empty_lines(4)"
line_regex = ['^❯\s*$']

# The FINAL payload row, which is the one row a fixed bottom region can
# never lose: `cat` echoes every pasted line, so the header scrolls away
# while the sentinel stays. Paired with the generic '[cyclops:end '
# pattern, this is what lets marker_in_composer reach the submit step.
#
# These shared fixtures exercise delivery LIFECYCLE only. They do not
# prove INVARIANTS rule 12 and they do not prove sentinel completeness:
# a blank pane paints no chrome under a paste, so terminality is not
# decidable here at all. Those claims belong to the dedicated tests in
# m1_blockers.rs and the sentinel unit tests in delivery.rs.
[[rule]]
id = "composer_holds_paste"
state = "idle_with_input"
composer_semantic = "human_input"
priority = 80
# The active composer only. The escaped matcher rejects the dim transcript
# prompt that the fixture paints after consuming a turn.
region = "bottom_non_empty_lines(3)"
line_regex = ['^\s*❯\s+\S']
line_regex_esc = ['^❯']

# The transient row the composer paints after consuming the sentinel.
# Screen ACK evidence needs a turn, and a generic-marker staging does not
# license "the window changed" as proof of one.
[[rule]]
id = "composer_working"
state = "working"
priority = 300
region = "bottom_non_empty_lines(5)"
line_regex = ['^FAKETUI-WORKING$']

[injection]
submit = "Enter"
verify_before_submit = true
verify_pattern = ["<message_id>"]
# The staged sentinel row as this pane renders it. A shell composer echoes
# the paste unstyled, so the two halves of the pair are the same shape
# here; a real vendor's chip is painted and its escaped half says so.
# What the fake TUI paints below the composer, in order and in both
# forms. Two rows, both required, both painted: this is the authentic
# sentinel lane rather than a chip declared over raw text.
composer_trailer_regex = ['^─+$', '^Model \S+ · Ctx: \d+%$']
composer_trailer_regex_esc = ['^\x1b\[38;5;244m─', '^\x1b\[38;5;152mModel\b']
composer_trailer_required_prefix = 2
composer_prompt_regex = '^❯ (?P<content>.*)$'
composer_continuation_regex = '^(?P<content>.*)$'
"#;

/// Modal fixture: one auto-dismissable rule with explicit decline keys and
/// one rule (trust-style) that must never be dismissed by the daemon.
pub const MODAL_MANIFEST: &str = r#"
[agent]
id = "fix"
display_name = "Modal fixture"
process_names = ["python3", "python", "Python", "cat", "sh", "bash", "dash"]

[[rule]]
id = "fake_update_modal"
state = "blocked_modal"
priority = 1300
region = "bottom_non_empty_lines(8)"
contains = ["FAKE-UPDATE-AVAILABLE"]
decline_keys = ["3", "Enter"]
auto_dismiss = true

[[rule]]
id = "fake_trust_modal"
state = "blocked_modal"
priority = 1300
region = "bottom_non_empty_lines(8)"
contains = ["FAKE-TRUST-PROMPT"]
auto_dismiss = false

[[rule]]
id = "always_idle"
state = "idle"
priority = 100
region = "pane_title"
regex = ['^']

# The screen sensor must be able to answer for these panes, because a
# write needs positive clean-composer evidence from the sensor that sees
# the composer (INVARIANTS rule 12). A blank fixture pane IS a clean
# composer, so the empty bottom region is the evidence.
[[rule]]
id = "composer_empty"
state = "idle"
composer_semantic = "clean"
priority = 90
region = "bottom_non_empty_lines(4)"
line_regex = ['^❯\s*$']

# The FINAL payload row, which is the one row a fixed bottom region can
# never lose: `cat` echoes every pasted line, so the header scrolls away
# while the sentinel stays. Paired with the generic '[cyclops:end '
# pattern, this is what lets marker_in_composer reach the submit step.
#
# These shared fixtures exercise delivery LIFECYCLE only. They do not
# prove INVARIANTS rule 12 and they do not prove sentinel completeness:
# a blank pane paints no chrome under a paste, so terminality is not
# decidable here at all. Those claims belong to the dedicated tests in
# m1_blockers.rs and the sentinel unit tests in delivery.rs.
[[rule]]
id = "composer_holds_paste"
state = "idle_with_input"
composer_semantic = "human_input"
priority = 80
# The active composer only. The escaped matcher rejects the dim transcript
# prompt that the fixture paints after consuming a turn.
region = "bottom_non_empty_lines(3)"
line_regex = ['^\s*❯\s+\S']
line_regex_esc = ['^❯']

# The transient row the composer paints after consuming the sentinel.
# Screen ACK evidence needs a turn, and a generic-marker staging does not
# license "the window changed" as proof of one.
[[rule]]
id = "composer_working"
state = "working"
priority = 300
region = "bottom_non_empty_lines(5)"
line_regex = ['^FAKETUI-WORKING$']

[injection]
submit = "Enter"
verify_before_submit = true
verify_pattern = ["<message_id>"]
# The staged sentinel row as this pane renders it. A shell composer echoes
# the paste unstyled, so the two halves of the pair are the same shape
# here; a real vendor's chip is painted and its escaped half says so.
# What the fake TUI paints below the composer, in order and in both
# forms. Two rows, both required, both painted: this is the authentic
# sentinel lane rather than a chip declared over raw text.
composer_trailer_regex = ['^─+$', '^Model \S+ · Ctx: \d+%$']
composer_trailer_regex_esc = ['^\x1b\[38;5;244m─', '^\x1b\[38;5;152mModel\b']
composer_trailer_required_prefix = 2
composer_prompt_regex = '^❯ (?P<content>.*)$'
composer_continuation_regex = '^(?P<content>.*)$'
"#;

/// Quota fixture: the agy quota phrase parks the recipient.
pub const QUOTA_MANIFEST: &str = r#"
[agent]
id = "fix"
display_name = "Quota fixture"
process_names = ["python3", "python", "Python", "cat", "sh", "bash", "dash"]

[[rule]]
id = "quota_exhausted"
state = "blocked_quota"
priority = 1400
region = "bottom_non_empty_lines(12)"
contains = ["Individual quota reached"]

[[rule]]
id = "always_idle"
state = "idle"
priority = 100
region = "pane_title"
regex = ['^']

# The screen sensor must be able to answer for these panes, because a
# write needs positive clean-composer evidence from the sensor that sees
# the composer (INVARIANTS rule 12). A blank fixture pane IS a clean
# composer, so the empty bottom region is the evidence.
[[rule]]
id = "composer_empty"
state = "idle"
composer_semantic = "clean"
priority = 90
region = "bottom_non_empty_lines(4)"
line_regex = ['^❯\s*$']

# The FINAL payload row, which is the one row a fixed bottom region can
# never lose: `cat` echoes every pasted line, so the header scrolls away
# while the sentinel stays. Paired with the generic '[cyclops:end '
# pattern, this is what lets marker_in_composer reach the submit step.
#
# These shared fixtures exercise delivery LIFECYCLE only. They do not
# prove INVARIANTS rule 12 and they do not prove sentinel completeness:
# a blank pane paints no chrome under a paste, so terminality is not
# decidable here at all. Those claims belong to the dedicated tests in
# m1_blockers.rs and the sentinel unit tests in delivery.rs.
[[rule]]
id = "composer_holds_paste"
state = "idle_with_input"
composer_semantic = "human_input"
priority = 80
# The active composer only. The escaped matcher rejects the dim transcript
# prompt that the fixture paints after consuming a turn.
region = "bottom_non_empty_lines(3)"
line_regex = ['^\s*❯\s+\S']
line_regex_esc = ['^❯']

# The transient row the composer paints after consuming the sentinel.
# Screen ACK evidence needs a turn, and a generic-marker staging does not
# license "the window changed" as proof of one.
[[rule]]
id = "composer_working"
state = "working"
priority = 300
region = "bottom_non_empty_lines(5)"
line_regex = ['^FAKETUI-WORKING$']

[injection]
submit = "Enter"
verify_before_submit = true
verify_pattern = ["<message_id>"]
# The staged sentinel row as this pane renders it. A shell composer echoes
# the paste unstyled, so the two halves of the pair are the same shape
# here; a real vendor's chip is painted and its escaped half says so.
# What the fake TUI paints below the composer, in order and in both
# forms. Two rows, both required, both painted: this is the authentic
# sentinel lane rather than a chip declared over raw text.
composer_trailer_regex = ['^─+$', '^Model \S+ · Ctx: \d+%$']
composer_trailer_regex_esc = ['^\x1b\[38;5;244m─', '^\x1b\[38;5;152mModel\b']
composer_trailer_required_prefix = 2
composer_prompt_regex = '^❯ (?P<content>.*)$'
composer_continuation_regex = '^(?P<content>.*)$'
"#;

/// Busy fixture: a screen marker keeps the pane working until released.
pub const BUSY_MANIFEST: &str = r#"
[agent]
id = "fix"
display_name = "Busy fixture"
process_names = ["python3", "python", "Python", "cat", "sh", "bash", "dash"]

[[rule]]
id = "busy_marker"
state = "working"
priority = 1200
region = "bottom_non_empty_lines(8)"
contains = ["BUSY-MARKER"]

[[rule]]
id = "always_idle"
state = "idle"
priority = 100
region = "pane_title"
regex = ['^']

# The screen sensor must be able to answer for these panes, because a
# write needs positive clean-composer evidence from the sensor that sees
# the composer (INVARIANTS rule 12). A blank fixture pane IS a clean
# composer, so the empty bottom region is the evidence.
[[rule]]
id = "composer_empty"
state = "idle"
composer_semantic = "clean"
priority = 90
region = "bottom_non_empty_lines(4)"
line_regex = ['^❯\s*$']

# The FINAL payload row, which is the one row a fixed bottom region can
# never lose: `cat` echoes every pasted line, so the header scrolls away
# while the sentinel stays. Paired with the generic '[cyclops:end '
# pattern, this is what lets marker_in_composer reach the submit step.
#
# These shared fixtures exercise delivery LIFECYCLE only. They do not
# prove INVARIANTS rule 12 and they do not prove sentinel completeness:
# a blank pane paints no chrome under a paste, so terminality is not
# decidable here at all. Those claims belong to the dedicated tests in
# m1_blockers.rs and the sentinel unit tests in delivery.rs.
[[rule]]
id = "composer_holds_paste"
state = "idle_with_input"
composer_semantic = "human_input"
priority = 80
# The active composer only. The escaped matcher rejects the dim transcript
# prompt that the fixture paints after consuming a turn.
region = "bottom_non_empty_lines(3)"
line_regex = ['^\s*❯\s+\S']
line_regex_esc = ['^❯']

# The transient row the composer paints after consuming the sentinel.
# Screen ACK evidence needs a turn, and a generic-marker staging does not
# license "the window changed" as proof of one.
[[rule]]
id = "composer_working"
state = "working"
priority = 300
region = "bottom_non_empty_lines(5)"
line_regex = ['^FAKETUI-WORKING$']

[injection]
submit = "Enter"
verify_before_submit = true
verify_pattern = ["<message_id>"]
# The staged sentinel row as this pane renders it. A shell composer echoes
# the paste unstyled, so the two halves of the pair are the same shape
# here; a real vendor's chip is painted and its escaped half says so.
# What the fake TUI paints below the composer, in order and in both
# forms. Two rows, both required, both painted: this is the authentic
# sentinel lane rather than a chip declared over raw text.
composer_trailer_regex = ['^─+$', '^Model \S+ · Ctx: \d+%$']
composer_trailer_regex_esc = ['^\x1b\[38;5;244m─', '^\x1b\[38;5;152mModel\b']
composer_trailer_required_prefix = 2
composer_prompt_regex = '^❯ (?P<content>.*)$'
composer_continuation_regex = '^(?P<content>.*)$'
"#;

/// Verification-failure fixture: the pattern can never appear on screen,
/// so every attempt fails composer verification.
pub const BAD_VERIFY_MANIFEST: &str = r#"
[agent]
id = "fix"
display_name = "Bad verify fixture"
process_names = ["python3", "python", "Python", "cat", "sh", "bash", "dash"]

[[rule]]
id = "always_idle"
state = "idle"
priority = 100
region = "pane_title"
regex = ['^']

# Screen-idle evidence only: this fixture exists to make verification
# FAIL, so it deliberately carries no staged-payload marker. Without the
# idle rule the gate could never admit a write at all and the test would
# prove nothing.
[[rule]]
id = "composer_empty"
state = "idle"
composer_semantic = "clean"
priority = 90
region = "bottom_non_empty_lines(4)"
line_regex = ['^❯\s*$']

[injection]
submit = "Enter"
verify_before_submit = true
verify_pattern = ["CYC-NOPE-<message_id>-NOPE"]
"#;

// ---------------------------------------------------------------------------
// Rig
// ---------------------------------------------------------------------------

/// The isolated tmux server these tests drive, plus the skip check.
/// Naming, isolation flags, and teardown are `cyclops-testrig`'s: this
/// crate's tests add helpers on top, never a second way to kill a server.
pub use cyclops_testrig::{tmux_available, TmuxServer as TmuxGuard};

/// Scratch CYCLOPS_HOME under the scratch root, removed on drop. The root
/// is `cyclops_proto::scratch`'s, never a hardcoded /private/tmp (F24).
pub struct HomeGuard(pub PathBuf);

impl Drop for HomeGuard {
    fn drop(&mut self) {
        // Debug aid: CYC_TEST_KEEP=1 preserves the scratch home.
        if std::env::var("CYC_TEST_KEEP").is_err() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// Minimal NDJSON test client (the m0 pattern): responses by id, events
/// buffered for next_event.
pub struct TestClient {
    lines: tokio::io::Lines<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
    pending_events: VecDeque<Value>,
    next_id: u64,
}

impl TestClient {
    pub async fn connect(path: &Path) -> TestClient {
        let stream = UnixStream::connect(path).await.expect("connect daemon");
        let (r, writer) = stream.into_split();
        let mut lines = BufReader::new(r).lines();
        // Consume the hello line.
        tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .expect("hello within 5s")
            .expect("read hello")
            .expect("hello line");
        TestClient {
            lines,
            writer,
            pending_events: VecDeque::new(),
            next_id: 1,
        }
    }

    pub async fn next_line(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(10), self.lines.next_line())
            .await
            .expect("line within 10s")
            .expect("read line")
            .expect("connection open");
        serde_json::from_str(&line).expect("line parses")
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({"id": id, "method": method, "params": params}).to_string();
        self.writer
            .write_all(format!("{line}\n").as_bytes())
            .await
            .expect("write to daemon");
        loop {
            let v = self.next_line().await;
            if v.get("event").is_some() {
                self.pending_events.push_back(v);
                continue;
            }
            if v["id"] == json!(id) {
                return v;
            }
        }
    }

    async fn expect_closed(&mut self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match self.lines.next_line().await.expect("read shutdown EOF") {
                    Some(_) => continue,
                    None => return,
                }
            }
        })
        .await
        .expect("connection closes within 5s");
    }

    /// Next event matching `pred`, scanning buffered then incoming lines.
    pub async fn wait_event<F: Fn(&Value) -> bool>(&mut self, within: Duration, pred: F) -> Value {
        if let Some(i) = self.pending_events.iter().position(&pred) {
            return self.pending_events.remove(i).unwrap();
        }
        let deadline = Instant::now() + within;
        loop {
            assert!(
                Instant::now() < deadline,
                "no matching event within {within:?}"
            );
            let v = self.next_line().await;
            if v.get("event").is_some() {
                if pred(&v) {
                    return v;
                }
                self.pending_events.push_back(v);
            }
        }
    }
}

/// One booted rig: isolated tmux server, scratch home, in-process daemon,
/// a request client, and an events-subscribed client.
pub struct Rig {
    _descriptor_slot: OwnedSemaphorePermit,
    pub tmux: TmuxGuard,
    pub home: PathBuf,
    home_guard: HomeGuard,
    pub daemon: cyclopsd::Daemon,
    pub ctl: TestClient,
    pub ev: TestClient,
    pub sessions: Vec<String>,
}

impl Rig {
    /// Boot with one session "main" and one pane running `pane_cmd`.
    /// `cfg_extra` appends config lines (timing knobs per scenario).
    pub async fn new(tag: &str, manifest: &str, pane_cmd: &str, cfg_extra: &str) -> Rig {
        Rig::new_multi(tag, manifest, &[("main", pane_cmd)], cfg_extra).await
    }

    /// Boot without installed mailbox capability evidence.
    pub async fn new_without_mailbox_capability(
        tag: &str,
        manifest: &str,
        pane_cmd: &str,
        cfg_extra: &str,
    ) -> Rig {
        Rig::new_multi_inner(tag, manifest, &[("main", pane_cmd)], cfg_extra, false).await
    }

    /// Boot with several watched sessions, each with one starting pane.
    pub async fn new_multi(
        tag: &str,
        manifest: &str,
        sessions: &[(&str, &str)],
        cfg_extra: &str,
    ) -> Rig {
        Rig::new_multi_inner(tag, manifest, sessions, cfg_extra, true).await
    }

    async fn new_multi_inner(
        tag: &str,
        manifest: &str,
        sessions: &[(&str, &str)],
        cfg_extra: &str,
        install_mailbox_capability: bool,
    ) -> Rig {
        // A rig owns a tmux server, daemon, ledgers, and socket clients.
        // Bound per-process concurrency so the suite also passes with the
        // macOS default 256-file descriptor limit.
        let descriptor_slot = rig_slots()
            .clone()
            .acquire_owned()
            .await
            .expect("rig semaphore remains open");
        // Debug aid: CYC_TEST_LOG=debug turns daemon tracing on.
        if let Ok(filter) = std::env::var("CYC_TEST_LOG") {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .try_init();
        }
        // The server exists from here on, so its teardown is already armed
        // when the config below names it.
        let tmux = TmuxGuard::new(&format!("m1-{tag}"));
        let socket = tmux.socket();
        let home = cyclops_proto::scratch::scratch_dir(&format!("cyc-m1-{tag}"));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("manifests")).expect("create scratch home");
        let home_guard = HomeGuard(home.clone());
        let manifest = if install_mailbox_capability && !manifest.contains("[messaging]") {
            let capability = home.join("cyclops-skill.md");
            std::fs::write(
                &capability,
                include_bytes!("../../../../skills/cyclops/SKILL.md"),
            )
            .expect("write mailbox capability evidence");
            format!(
                "{manifest}\n[messaging]\nmailbox_capability_file = {:?}\n",
                capability.display().to_string()
            )
        } else {
            manifest.to_string()
        };
        std::fs::write(home.join("manifests/fix.toml"), manifest).expect("write manifest");
        let names: Vec<String> = sessions.iter().map(|(n, _)| n.to_string()).collect();
        write_config(&home, socket, &names, cfg_extra);

        for (name, pane_cmd) in sessions {
            tmux.run_ok(&[
                "new-session",
                "-d",
                "-s",
                name,
                "-x",
                "160",
                "-y",
                "40",
                pane_cmd,
            ]);
        }

        let (cfg, _) = cyclopsd::Config::load(&home).expect("config loads");
        let daemon = cyclopsd::boot(cfg).await.expect("daemon boots");
        let sock = daemon.socket_path();
        let ctl = TestClient::connect(&sock).await;
        let mut ev = TestClient::connect(&sock).await;
        let ack = ev.request("events.subscribe", json!({})).await;
        assert_eq!(ack["result"]["subscribed"], true);
        let mut rig = Rig {
            _descriptor_slot: descriptor_slot,
            tmux,
            home,
            home_guard,
            daemon,
            ctl,
            ev,
            sessions: sessions.iter().map(|(n, _)| n.to_string()).collect(),
        };
        for idx in 0..sessions.len() {
            rig.wait_attached_session(idx, 1).await;
        }
        rig
    }

    /// Change the config this rig boots on. Takes effect at the next
    /// [`Rig::reboot`], which is the only time the daemon reads the file.
    pub fn rewrite_config(&self, cfg_extra: &str) {
        write_config(&self.home, self.tmux.socket(), &self.sessions, cfg_extra);
    }

    /// Shut the daemon down and boot a fresh one on the same home and tmux
    /// server, with fresh clients. The ledger persists across the two runs.
    pub async fn reboot(self) -> Rig {
        let Rig {
            _descriptor_slot,
            tmux,
            home,
            home_guard,
            daemon,
            mut ctl,
            mut ev,
            sessions,
        } = self;
        daemon.shutdown().await;
        ctl.expect_closed().await;
        ev.expect_closed().await;
        let (cfg, _) = cyclopsd::Config::load(&home).expect("config loads");
        let daemon = cyclopsd::boot(cfg).await.expect("daemon reboots");
        let sock = daemon.socket_path();
        let ctl = TestClient::connect(&sock).await;
        let mut ev = TestClient::connect(&sock).await;
        let ack = ev.request("events.subscribe", json!({})).await;
        assert_eq!(ack["result"]["subscribed"], true);
        Rig {
            _descriptor_slot,
            tmux,
            home,
            home_guard,
            daemon,
            ctl,
            ev,
            sessions,
        }
    }

    /// Wait until the daemon is attached and sees `panes` panes (session 0).
    pub async fn wait_attached(&mut self, panes: usize) {
        self.wait_attached_session(0, panes).await;
    }

    pub async fn wait_attached_session(&mut self, idx: usize, panes: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let resp = self.ctl.request("status", json!({})).await;
            let session = &resp["result"]["sessions"][idx];
            if session["attached"] == json!(true)
                && session["panes"].as_array().map(Vec::len) == Some(panes)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "daemon never attached with {panes} panes in session {idx}: {resp}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub async fn pane_ids(&mut self) -> Vec<String> {
        self.pane_ids_session(0).await
    }

    pub async fn pane_ids_session(&mut self, idx: usize) -> Vec<String> {
        let resp = self.ctl.request("status", json!({})).await;
        resp["result"]["sessions"][idx]["panes"]
            .as_array()
            .expect("panes array")
            .iter()
            .map(|p| p["pane_id"].as_str().unwrap().to_string())
            .collect()
    }

    pub async fn label(&mut self, target: &str, label: &str) {
        let resp = self
            .ctl
            .request("pane.label", json!({"target": target, "label": label}))
            .await;
        assert_eq!(resp["result"]["label"], label, "{resp}");
    }

    /// msg.send with the sender already resolved, for tests about
    /// DELIVERY rather than about who sent it.
    ///
    /// Deliberately not the socket path. Socket sends resolve the sender
    /// from the calling process's ancestry, and a test runner's ancestry
    /// is not the daemon's business: a suite run from inside an agent CLI
    /// is a vendor descendant outside every watched pane, which the
    /// daemon correctly refuses to call the operator. Routing delivery
    /// tests through that would make them pass or fail on where the suite
    /// happened to be started.
    ///
    /// This is the documented embedding entry point, not a test-only
    /// door, and it resolves nothing. Attribution has its own tests,
    /// over the real socket with real process trees, in
    /// `tests/sender_identity.rs`.
    pub async fn send(&mut self, params: Value) -> (Value, Duration) {
        let params = serde_json::from_value(params).expect("msg.send params");
        let t = Instant::now();
        let result = self.daemon.deliver_payload("admin", params).await;
        let elapsed = t.elapsed();
        let result = result.unwrap_or_else(|e| panic!("msg.send failed: {}", e.message));
        (result, elapsed)
    }

    pub fn ledger_path(&self) -> PathBuf {
        self.ledger_path_for("main")
    }

    pub fn ledger_path_for(&self, session: &str) -> PathBuf {
        self.home.join(format!("ledger/{session}.ndjson"))
    }

    pub fn ledger_lines(&self) -> Vec<Value> {
        self.ledger_lines_for("main")
    }

    pub fn ledger_lines_for(&self, session: &str) -> Vec<Value> {
        const READ_ATTEMPTS: usize = 50;
        const READ_RETRY: Duration = Duration::from_millis(20);

        // A concurrent append can expose its final JSON before the newline.
        // Retry only that unfinished tail. Completed malformed lines fail now.
        for attempt in 0..READ_ATTEMPTS {
            let text =
                std::fs::read_to_string(self.ledger_path_for(session)).expect("ledger readable");
            let lines: Vec<_> = text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .collect();
            let mut parsed = Vec::with_capacity(lines.len());

            for (index, line) in lines.iter().enumerate() {
                match serde_json::from_str(line) {
                    Ok(value) => parsed.push(value),
                    Err(error) => {
                        let final_line_is_in_flight =
                            index + 1 == lines.len() && !text.ends_with('\n');
                        if final_line_is_in_flight && attempt + 1 < READ_ATTEMPTS {
                            std::thread::sleep(READ_RETRY);
                            break;
                        }
                        panic!("ledger line is valid JSON: {error}");
                    }
                }
            }

            if parsed.len() == lines.len() {
                return parsed;
            }
        }

        unreachable!("the final bounded ledger read either returns or reports its parse error")
    }

    /// The whole-run ledger contract: every line is jq-parseable and
    /// schema-legal, every delivery transition is legal and chains
    /// continuously from queued, and screen-only text never leaked in.
    pub fn assert_ledger_legal(&self, forbidden_screen_text: &[&str]) {
        self.assert_ledger_legal_for("main", forbidden_screen_text);
    }

    pub fn assert_ledger_legal_for(&self, session: &str, forbidden_screen_text: &[&str]) {
        let text = std::fs::read_to_string(self.ledger_path_for(session)).expect("ledger readable");
        let mut chains: HashMap<(String, String), Vec<(DeliveryState, DeliveryState)>> =
            HashMap::new();
        let mut saw_boot = false;
        let mut last_seq = 0u64;
        for raw in text.lines().filter(|l| !l.trim().is_empty()) {
            let _: Value = serde_json::from_str(raw).expect("jq-parseable line");
            let line: LedgerLine = serde_json::from_str(raw).expect("schema-legal line");
            assert!(line.seq > last_seq, "seq not monotonic at {raw}");
            last_seq = line.seq;
            for f in forbidden_screen_text {
                assert!(
                    !raw.contains(f),
                    "screen text leaked into the ledger: {f:?} in {raw}"
                );
            }
            if let Some(data) = &line.data {
                if data["event"] == "boot" {
                    saw_boot = true;
                }
                // Delivery-state lines carry to/from/to_state; fusion
                // state lines carry pane_id/state instead.
                if matches!(line.kind, Kind::State) && data.get("to_state").is_some() {
                    let from: DeliveryState =
                        serde_json::from_value(data["from"].clone()).expect("from state");
                    let to_state: DeliveryState =
                        serde_json::from_value(data["to_state"].clone()).expect("to state");
                    assert!(
                        from.can_transition_to(to_state),
                        "illegal transition {from:?} -> {to_state:?} in ledger: {raw}"
                    );
                    let to = data["to"].as_str().unwrap_or_default().to_string();
                    chains
                        .entry((line.id.clone(), to))
                        .or_default()
                        .push((from, to_state));
                }
            }
        }
        assert!(saw_boot, "no boot system line in ledger");
        for ((id, to), steps) in chains {
            assert_eq!(
                steps[0].0,
                DeliveryState::Queued,
                "delivery {id}->{to} does not start from queued"
            );
            for pair in steps.windows(2) {
                assert_eq!(
                    pair[0].1, pair[1].0,
                    "delivery {id}->{to} chain breaks between {:?} and {:?}",
                    pair[0], pair[1]
                );
            }
        }
    }

    /// Final delivery state for (msg id, recipient), from the ledger.
    pub fn final_state(&self, msg_id: &str, to: &str) -> Option<String> {
        self.final_state_in("main", msg_id, to)
    }

    pub fn final_state_in(&self, session: &str, msg_id: &str, to: &str) -> Option<String> {
        self.ledger_lines_for(session)
            .iter()
            .rev()
            .find(|l| l["kind"] == "state" && l["id"] == msg_id && l["data"]["to"] == to)
            .map(|l| l["data"]["to_state"].as_str().unwrap().to_string())
    }

    pub async fn shutdown(self) {
        self.daemon.shutdown().await;
    }
}

/// Write the daemon config file. One place, so a test that changes a knob
/// mid-run writes the same file the boot wrote.
fn write_config(home: &Path, socket: &str, sessions: &[String], cfg_extra: &str) {
    let names: Vec<String> = sessions.iter().map(|n| format!("\"{n}\"")).collect();
    std::fs::write(
        home.join("config.toml"),
        format!(
            "sessions = [{}]\n\
             tmux_socket = \"{socket}\"\n\
             tmux_config = \"/dev/null\"\n\
             manifest_dir = \"{}\"\n\
             {cfg_extra}",
            names.join(", "),
            home.join("manifests").display()
        ),
    )
    .expect("write config");
}

/// Poll daemon status until the first pane reports `want`.
///
/// Status is the daemon's own view, which is the one that matters: tmux
/// can already be in a state the watcher table has not consumed yet.
pub async fn wait_pane_state(rig: &mut Rig, want: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let resp = rig.ctl.request("status", json!({})).await;
        let state = resp["result"]["sessions"][0]["panes"][0]["state"].clone();
        if state == json!(want) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "pane never reached state {want}: {resp}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Pane command: print a marker, hold until any input line, clear the
/// screen, then behave like the composer for the rest of its life.
///
/// The tail matters as much as the marker. These fixtures exist to hold a
/// delivery (a modal, a busy pane) and then release it, and the release
/// only proves anything if what follows can actually take a message:
/// admit a write, hold the staged sentinel, and clear it on Enter.
pub fn hold_script(marker: &str) -> String {
    let tail = composer_tail();
    format!("sh -c 'echo {marker}; read x; printf \"\\033[2J\\033[H\"; {tail}'")
}

/// Hold on a marker, then consume submits without emitting a lifecycle edge.
/// Tests use this to prove that a pre-release Working phase cannot satisfy a
/// wait that starts after delivery resolves.
pub fn hold_then_manual_lifecycle_script(marker: &str) -> String {
    let tail = format!("{} --manual-lifecycle", composer_tail());
    format!("sh -c 'echo {marker}; read x; printf \"\\033[2J\\033[H\"; {tail}'")
}
