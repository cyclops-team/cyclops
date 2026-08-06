//! `cyclops hooks`: install (render vendor hook configs), verify (hook
//! liveness), selftest (one no-op round trip through the delivery
//! pipeline).
//!
//! Install PREPARES artifacts and prints wiring instructions; it never
//! writes into vendor dot-dirs (~/.claude, ~/.codex, ~/.gemini, .agents,
//! .cursor).
//! Configuration does not equal subscription (amendment c, finding F1):
//! a rendered config proves nothing until `hooks verify` or
//! `hooks selftest` shows edges actually arriving.

use std::path::Path;
use std::time::Duration;

use clap::ValueEnum;
use cyclops_proto::{DeliveryReceipt, DeliveryState};
use serde_json::json;

use crate::client::Client;
use crate::render::{display_width, human_duration, pad, receipt_badge};
use crate::style::Style;
use crate::{copy, EXIT_USAGE};

/// Templates ship inside the binary; the files under resources/hooks/ in
/// the repo are the source of truth and the golden tests hold the two
/// together.
const CLAUDE_TMPL: &str = include_str!("../../../resources/hooks/claude/settings.json.tmpl");
const CODEX_TMPL: &str = include_str!("../../../resources/hooks/codex/hooks.json.tmpl");
const AGY_TMPL: &str = include_str!("../../../resources/hooks/agy/hooks.json.tmpl");
const CURSOR_TMPL: &str = include_str!("../../../resources/hooks/cursor/hooks.json.tmpl");

/// Read timeout for hooks.selftest: the daemon waits up to 10s for the
/// delivery to resolve, so the client waits a little longer.
const SELFTEST_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// Path components that mark a vendor CLI's own config tree. Install
/// refuses to write anywhere inside one, whatever the --dest says.
const VENDOR_DIRS: &[&str] = &[".claude", ".codex", ".gemini", ".agents", ".cursor"];

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CliKind {
    Claude,
    Codex,
    Agy,
    Cursor,
}

impl CliKind {
    fn template(self) -> &'static str {
        match self {
            CliKind::Claude => CLAUDE_TMPL,
            CliKind::Codex => CODEX_TMPL,
            CliKind::Agy => AGY_TMPL,
            CliKind::Cursor => CURSOR_TMPL,
        }
    }

    /// Rendered file name in the destination directory.
    fn file_name(self) -> &'static str {
        match self {
            CliKind::Claude => "settings.json",
            CliKind::Codex | CliKind::Agy | CliKind::Cursor => "hooks.json",
        }
    }

    fn name(self) -> &'static str {
        match self {
            CliKind::Claude => "claude",
            CliKind::Codex => "codex",
            CliKind::Agy => "agy",
            CliKind::Cursor => "cursor",
        }
    }
}

/// Render a template: substitute {label} and {cyclops_bin}, strip the '#'
/// comment header so the output is plain vendor JSON.
pub fn render(kind: CliKind, label: &str, cyclops_bin: &str) -> String {
    let body: String = kind
        .template()
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    let mut out = body
        .replace("{label}", label)
        .replace("{cyclops_bin}", cyclops_bin);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Copy-pasteable wiring instructions. The admin runs these once,
/// deliberately; cyclops never touches vendor config itself.
fn instructions(kind: CliKind, rendered: &Path, label: &str) -> String {
    let p = rendered.display();
    match kind {
        CliKind::Claude => format!(
            "Wire it (claude reads hooks only from settings it is launched with):\n\
             \n\
             \x20 claude --settings {p}\n\
             \n\
             Already passing your own --settings file? Merge the \"hooks\" object\n\
             from {p} into it.\n\
             Then prove it fires: cyclops hooks selftest {label}"
        ),
        CliKind::Codex => format!(
            "Wire it. Codex loads ZERO hooks in an untrusted directory (finding F1),\n\
             and --dangerously-bypass-hook-trust does NOT fix that. Two setups work:\n\
             \n\
             1. User-level hooks, no directory trust needed:\n\
             \x20    cp {p} \"${{CODEX_HOME:-$HOME/.codex}}/hooks.json\"\n\
             \n\
             2. Keep project-local hooks and pre-seed trust in\n\
             \x20  ${{CODEX_HOME:-$HOME/.codex}}/config.toml (edit the path first):\n\
             \x20    [projects.\"/path/to/your/project\"]\n\
             \x20    trust_level = \"trusted\"\n\
             \n\
             Then prove it fires: cyclops hooks selftest {label}"
        ),
        CliKind::Agy => format!(
            "Wire it (agy reads .agents/hooks.json in the workspace it runs in):\n\
             \n\
             \x20 mkdir -p <workspace>/.agents && cp {p} <workspace>/.agents/hooks.json\n\
             \n\
             agy has no payload-matchable ACK (finding F7): deliveries stay on the\n\
             screen-verified tier; these hooks feed liveness and turn detection.\n\
             Then check edges arrive: cyclops hooks verify {label}"
        ),
        CliKind::Cursor => format!(
            "Wire it (cursor reads hooks.json from the workspace it runs in, or\n\
             from your home directory):\n\
             \n\
             \x20 mkdir -p <workspace>/.cursor && cp {p} <workspace>/.cursor/hooks.json\n\
             \x20 # or, user level:\n\
             \x20 mkdir -p ~/.cursor && cp {p} ~/.cursor/hooks.json\n\
             \n\
             CURSOR_CONFIG_DIR does NOT work for hooks: it relocates\n\
             cli-config.json but hooks.json placed there fires zero events.\n\
             Then prove it fires: cyclops hooks selftest {label}"
        ),
    }
}

/// True when `dest` points inside a vendor CLI's own config tree.
fn inside_vendor_dir(dest: &Path) -> bool {
    dest.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|s| VENDOR_DIRS.contains(&s))
    })
}

/// The path of this cyclops binary, for hook commands that must work from
/// any vendor CLI's environment without PATH assumptions.
fn cyclops_bin() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "cyclops".to_string())
}

pub fn run_install(
    kind: CliKind,
    label: &str,
    dry_run: bool,
    dest: Option<&Path>,
    json: bool,
) -> i32 {
    // The same rule the daemon enforces, from the same place, so the two
    // never disagree about which names exist (cyclops_proto::label).
    if let Some(why) = cyclops_proto::label::refusal(label) {
        // Dead ends invite the next action: name the pane, then come back.
        eprintln!(
            "--agent needs a name a pane can answer to. {why} \
             cyclops status shows every pane and its label; name one, then \
             rerun cyclops hooks install with that name."
        );
        return EXIT_USAGE;
    }
    let dest_dir = dest
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cyclops_proto::cyclops_home().join("hooks").join(label));
    if inside_vendor_dir(&dest_dir) {
        eprintln!(
            "{} is a vendor config directory; cyclops prepares files and prints \
             instructions, it never writes vendor config itself. Use a neutral \
             --dest (default: $CYCLOPS_HOME/hooks/{label}/) and copy the file \
             yourself.",
            dest_dir.display()
        );
        return EXIT_USAGE;
    }
    let content = render(kind, label, &cyclops_bin());
    let path = dest_dir.join(kind.file_name());
    if json {
        println!(
            "{}",
            json!({
                "cli": kind.name(),
                "agent": label,
                "path": path.display().to_string(),
                "written": !dry_run,
                "content": content,
            })
        );
    }
    if dry_run {
        if !json {
            println!("Would write {}:", path.display());
            println!();
            print!("{content}");
            println!();
            println!("{}", instructions(kind, &path, label));
        }
        return 0;
    }
    if let Err(e) = std::fs::create_dir_all(&dest_dir) {
        eprintln!("can't create {}: {e}", dest_dir.display());
        return 1;
    }
    if let Err(e) = std::fs::write(&path, &content) {
        eprintln!("can't write {}: {e}", path.display());
        return 1;
    }
    if !json {
        println!("Wrote {}", path.display());
        println!();
        println!("{}", instructions(kind, &path, label));
    }
    0
}

pub fn run_verify(c: &mut Client, json: bool, style: &Style, target: &str) -> i32 {
    let result = match c.request("hooks.verify", json!({"target": target})) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}", copy::client_error(&e, Some(target)));
            return 1;
        }
    };
    if json {
        println!("{result}");
        return i32::from(result["hooks_verified"] == false);
    }
    let sep = style.dim("·");
    let tier = result["tier"].as_u64().unwrap_or(2);
    let verified = result["hooks_verified"].as_bool();
    let events = result["events"].as_array().cloned().unwrap_or_default();
    // hooks_verified is absent on an unadopted pane even when its bound
    // manifest declares hooks (the events list below): edges are tracked
    // per label, so tracking starts at adoption. Only a pane with no
    // declared hooks at all reads "no hooks declared".
    let unadopted_with_hooks =
        verified.is_none() && result["manifest"].is_string() && !events.is_empty();
    let badge = match verified {
        Some(true) => "✔ hooks verified",
        Some(false) => "⚠ hooks unverified",
        None if unadopted_with_hooks => "hook tracking starts when the pane has a label",
        None => "no hooks declared",
    };
    println!(
        "{} {sep} tier {tier} {sep} {badge}",
        style.role(target, target)
    );
    if !events.is_empty() {
        println!();
        let name_w = events
            .iter()
            .filter_map(|e| e["event"].as_str())
            .map(display_width)
            .max()
            .unwrap_or(0);
        for e in &events {
            let name = e["event"].as_str().unwrap_or("?");
            let age = match e["last_seen_ms_ago"].as_u64() {
                Some(ms) if ms < 1000 => "just now".to_string(),
                Some(ms) => format!("{} ago", human_duration(ms)),
                None => "never".to_string(),
            };
            println!("  {}  {}", pad(name, name_w), style.dim(&age));
        }
    }
    match verified {
        Some(false) => {
            eprintln!(
                "No hook edge has ever reached the daemon from {target}. \
                 Run cyclops hooks selftest {target} to probe it; \
                 cyclops hooks install prints the wiring."
            );
            1
        }
        None if unadopted_with_hooks => {
            eprintln!(
                "{target} declares hooks but has no label, and hook edges \
                 only count for a labeled pane. Name the pane (cyclops \
                 status shows every pane and its label), then rerun \
                 cyclops hooks verify."
            );
            0
        }
        _ => 0,
    }
}

pub fn run_selftest(c: &mut Client, json: bool, style: &Style, target: &str) -> i32 {
    // The daemon delivers, then waits up to its own cap for resolution.
    c.set_read_timeout(SELFTEST_READ_TIMEOUT);
    let result = match c.request("hooks.selftest", json!({"target": target})) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}", copy::client_error(&e, Some(target)));
            return 1;
        }
    };
    if json {
        println!("{result}");
        return i32::from(result["hook_ack"] != true);
    }
    let sep = style.dim("·");
    let hook_ack = result["hook_ack"] == true;
    let tier = result["tier"].as_u64().unwrap_or(2);
    // The delivery state renders in the same badge voice as a send
    // receipt; raw wire spellings never face a human.
    let state = match serde_json::from_value::<DeliveryState>(result["state"].clone()) {
        Ok(s) => receipt_badge(
            &DeliveryReceipt {
                to: target.to_string(),
                state: s,
                position: None,
                note: None,
                pane: None,
            },
            style,
        ),
        Err(_) => result["state"].as_str().unwrap_or("?").to_string(),
    };
    let msg_id = result["msg_id"].as_str().unwrap_or("?");
    let head = if hook_ack {
        "✔ ack hook fired with the marker"
    } else {
        "⚠ no hook ack"
    };
    println!(
        "{} {sep} {head} {sep} {state} {sep} {}",
        style.role(target, target),
        style.dim(msg_id)
    );
    if !hook_ack {
        if tier == 2 {
            eprintln!(
                "{target} has no payload-matchable ack hook (screen tier); \
                 a hook ack can never confirm it. The delivery state above is \
                 the whole answer."
            );
        } else {
            // Install takes a CLI kind, not the target: the daemon names
            // the bound manifest so this command runs as printed.
            eprintln!(
                "The ack hook never reported the marker. Its config is probably \
                 not loaded; cyclops hooks install {} --agent {} prints the \
                 wiring and the trust caveats.",
                result["manifest"].as_str().unwrap_or("<cli>"),
                result["target"].as_str().unwrap_or(target)
            );
        }
    }
    i32::from(!hook_ack)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN_LABEL: &str = "reviewer";
    const GOLDEN_BIN: &str = "/opt/cyclops/bin/cyclops";

    fn golden(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden")
            .join(name);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    #[test]
    fn rendered_templates_match_the_golden_files() {
        for (kind, file) in [
            (CliKind::Claude, "claude.settings.json"),
            (CliKind::Codex, "codex.hooks.json"),
            (CliKind::Agy, "agy.hooks.json"),
            (CliKind::Cursor, "cursor.hooks.json"),
        ] {
            assert_eq!(
                render(kind, GOLDEN_LABEL, GOLDEN_BIN),
                golden(file),
                "{file} drifted from the template; update hooks/ and the golden together"
            );
        }
    }

    #[test]
    fn rendered_templates_are_valid_vendor_json() {
        for kind in [
            CliKind::Claude,
            CliKind::Codex,
            CliKind::Agy,
            CliKind::Cursor,
        ] {
            let out = render(kind, GOLDEN_LABEL, GOLDEN_BIN);
            let v: serde_json::Value = serde_json::from_str(&out)
                .unwrap_or_else(|e| panic!("{} render is not JSON: {e}", kind.name()));
            // No placeholder survives rendering.
            assert!(!out.contains("{label}") && !out.contains("{cyclops_bin}"));
            // Every registered command is self-tagging and carries the agent.
            let text = v.to_string();
            assert!(
                text.contains(&format!("--agent {GOLDEN_LABEL}")),
                "{}: command lost the agent",
                kind.name()
            );
        }
    }

    #[test]
    fn claude_and_codex_share_the_measured_hook_shape() {
        for kind in [CliKind::Claude, CliKind::Codex] {
            let v: serde_json::Value = serde_json::from_str(&render(kind, "r", "cyclops")).unwrap();
            let hooks = v["hooks"].as_object().expect("hooks object");
            for (event, entries) in hooks {
                let cmd = entries[0]["hooks"][0]["command"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{}: {event} entry shape", kind.name()));
                assert_eq!(cmd, format!("cyclops hook {event} --agent r"));
            }
        }
        // Claude registers the four attention-relevant events.
        let v: serde_json::Value =
            serde_json::from_str(&render(CliKind::Claude, "r", "c")).unwrap();
        let mut events: Vec<&String> = v["hooks"].as_object().unwrap().keys().collect();
        events.sort();
        assert_eq!(
            events,
            [
                "Notification",
                "PermissionRequest",
                "Stop",
                "UserPromptSubmit"
            ]
        );
    }

    #[test]
    fn agy_registers_every_event_with_distinct_commands() {
        // F7: payloads carry no event name, so every event needs its own
        // self-tagging command.
        let v: serde_json::Value = serde_json::from_str(&render(CliKind::Agy, "r", "c")).unwrap();
        let named = v["r"].as_object().expect("named hooks under the label");
        let mut events: Vec<&String> = named.keys().collect();
        events.sort();
        assert_eq!(
            events,
            [
                "PostInvocation",
                "PostToolUse",
                "PreInvocation",
                "PreToolUse",
                "Stop"
            ]
        );
        let mut commands: Vec<String> = Vec::new();
        for (event, entries) in named {
            let cmd = if entries[0]["command"].is_string() {
                entries[0]["command"].as_str().unwrap().to_string()
            } else {
                entries[0]["hooks"][0]["command"]
                    .as_str()
                    .unwrap()
                    .to_string()
            };
            assert!(cmd.contains(&format!("hook {event} ")), "{event}: {cmd}");
            commands.push(cmd);
        }
        commands.sort();
        commands.dedup();
        assert_eq!(commands.len(), 5, "commands must be distinct per event");
    }

    #[test]
    fn vendor_dot_dirs_are_refused() {
        for p in [
            "/Users/x/.claude/hooks",
            "/Users/x/.codex",
            "/home/x/.gemini/sub",
            "/work/project/.agents",
            "/work/project/.cursor",
        ] {
            assert!(inside_vendor_dir(Path::new(p)), "{p}");
        }
        assert!(!inside_vendor_dir(Path::new("/Users/x/.cyclops/hooks/rev")));
        assert!(!inside_vendor_dir(Path::new("/work/agents/hooks")));
    }

    #[test]
    fn cursor_registers_only_the_two_measured_events() {
        // The daemon matches incoming events against the manifest's own
        // ack/turn_start/turn_end names (both beforeSubmitPrompt here), so
        // wiring any other Cursor event would just be ignored; this proves
        // the template does not do so, and that the flat schema (no nested
        // "hooks" array, no "type": "command") round-trips correctly.
        let v: serde_json::Value =
            serde_json::from_str(&render(CliKind::Cursor, "r", "cyclops")).unwrap();
        assert_eq!(v["version"], 1);
        let hooks = v["hooks"].as_object().expect("hooks object");
        let mut events: Vec<&String> = hooks.keys().collect();
        events.sort();
        assert_eq!(events, ["beforeSubmitPrompt", "stop"]);
        for (event, entries) in hooks {
            let cmd = entries[0]["command"]
                .as_str()
                .unwrap_or_else(|| panic!("cursor: {event} entry shape"));
            assert_eq!(cmd, format!("cyclops hook {event} --agent r"));
        }
    }
}
