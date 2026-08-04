//! Bare `cyclops` invocation (workspace Step 4).

use std::process::{Command, Stdio};

#[test]
fn bare_non_tty_prints_help_and_exits_zero() {
    let out = Command::new(env!("CARGO_BIN_EXE_cyclops"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("help") || text.contains("cyclops"),
        "stdout={text}"
    );
}
