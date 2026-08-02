//! Per-CLI detection manifests.
//!
//! Everything Cyclops knows about a vendor TUI is data in a TOML file:
//! which sensors carry signal, how to read busy/idle from title or screen,
//! the modal vocabulary with explicit decline actions, and how to inject.
//! Seeded from the 2026-08-01 validation campaign's measured drafts.
//!
//! Hot reload is the daemon's job; this crate parses, validates, and
//! evaluates. Unknown TOML keys are tolerated so manifests can carry
//! evidence notes the code does not model.
//!
//! Two schema fields exist for vendor quirks that plain text cannot express:
//! `agent.argv_basenames` (bind by pane argv when the kernel comm name is
//! useless, e.g. native Claude installs reporting "2.1.220") and rule
//! `line_regex_esc` (match against a capture-pane -e capture, e.g. codex
//! ghost suggestions are only distinguishable from typed text by SGR dim).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use cyclops_proto::AgentState;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("manifest {id}: rule {rule}: bad regex {pattern:?}: {source}")]
    BadRegex {
        id: String,
        rule: String,
        pattern: String,
        source: regex::Error,
    },
    #[error("manifest {id}: rule {rule}: bad region {region:?}")]
    BadRegion {
        id: String,
        rule: String,
        region: String,
    },
    #[error("manifest {id}: rule {rule}: unknown state {state:?}")]
    BadState {
        id: String,
        rule: String,
        state: String,
    },
}

/// A parsed, validated manifest with compiled regexes.
#[derive(Debug, Clone)]
pub struct Manifest {
    pub agent: AgentMeta,
    pub hooks: Hooks,
    pub rules: Vec<CompiledRule>,
    pub injection: Injection,
    /// Source path, for reload and error messages.
    pub path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentMeta {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub version_tested: String,
    #[serde(default)]
    pub process_names: Vec<String>,
    /// Fallback binding when `process_names` misses: argv[0] basenames to
    /// match against the pane's process argv, resolved via `pane_pid`.
    /// Needed because #{pane_current_command} is the kernel comm name of the
    /// resolved executable, not the invoked name. MEASURED (m1 soak): native
    /// Claude installs symlink ~/.local/bin/claude to versions/2.1.220, so
    /// comm reports the bare version string and "claude" never binds.
    #[serde(default)]
    pub argv_basenames: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Hooks {
    #[serde(default)]
    pub config_mechanism: String,
    #[serde(default)]
    pub turn_start: Option<String>,
    #[serde(default)]
    pub turn_end: Option<String>,
    /// Hook whose payload acknowledges a delivery. None means this CLI has
    /// no payload-matchable ACK and runs on the screen-verified tier (agy).
    #[serde(default)]
    pub ack: Option<String>,
    #[serde(default)]
    pub available: Vec<String>,
    /// Payload field carrying the injected text when `ack` is set.
    #[serde(default)]
    pub ack_payload_field: Option<String>,
}

/// Where a rule looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    /// #{pane_title} via the control connection. No screen scraping.
    PaneTitle,
    /// The last N non-empty lines of a visible-grid capture.
    BottomNonEmptyLines(usize),
}

impl Region {
    fn parse(s: &str) -> Option<Region> {
        if s == "pane_title" {
            return Some(Region::PaneTitle);
        }
        let inner = s
            .strip_prefix("bottom_non_empty_lines(")?
            .strip_suffix(')')?;
        inner.trim().parse().ok().map(Region::BottomNonEmptyLines)
    }
}

/// One matcher clause. Clauses within a matcher AND together; a rule fires
/// if its own clauses match or any `any` alternative matches.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct RawMatcher {
    #[serde(default)]
    pub contains: Vec<String>,
    #[serde(default)]
    pub regex: Vec<String>,
    #[serde(default)]
    pub line_regex: Vec<String>,
    /// Like `line_regex`, but run against an SGR-escaped capture
    /// (capture-pane -e), so a rule can discriminate on rendering style the
    /// plain text cannot express. MEASURED (codex-cli 0.146.0): ghost
    /// suggestions render dim (ESC[2m...ESC[0m) after the composer glyph
    /// while typed text is bare, which is the only signal separating
    /// idle from idle_with_input on that CLI. A matcher carrying these
    /// clauses fails closed when no escaped capture was provided.
    #[serde(default)]
    pub line_regex_esc: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawRule {
    pub id: String,
    pub state: String,
    pub priority: i64,
    pub region: String,
    #[serde(default)]
    pub any: Vec<RawMatcher>,
    #[serde(flatten)]
    pub matcher: RawMatcher,
    /// Explicit safe dismissal for blocked_modal rules: tmux send-keys
    /// arguments pressed in order, e.g. ["3", "Enter"] or ["Escape"].
    /// Amendment g: never a generic Enter/Escape chosen by code.
    #[serde(default)]
    pub decline_keys: Vec<String>,
    /// When false the daemon only reports the modal and parks the delivery;
    /// dismissal needs an operator. Trust/permission dialogs set this.
    #[serde(default)]
    pub auto_dismiss: bool,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub evidence: String,
}

#[derive(Debug, Clone)]
pub struct CompiledMatcher {
    pub contains: Vec<String>,
    pub regex: Vec<regex::Regex>,
    pub line_regex: Vec<regex::Regex>,
    pub line_regex_esc: Vec<regex::Regex>,
}

impl CompiledMatcher {
    fn is_empty(&self) -> bool {
        self.contains.is_empty()
            && self.regex.is_empty()
            && self.line_regex.is_empty()
            && self.line_regex_esc.is_empty()
    }

    /// All clauses must hold. `lines` are the region lines; `joined` is the
    /// region text joined with newlines. `esc_lines` are the same region's lines from
    /// an SGR-escaped capture (capture-pane -e); None means no escaped
    /// capture was taken, so `line_regex_esc` clauses cannot hold and the
    /// matcher fails closed rather than guessing.
    fn matches_esc(&self, joined: &str, lines: &[&str], esc_lines: Option<&[&str]>) -> bool {
        if self.is_empty() {
            return false;
        }
        let esc_ok = if self.line_regex_esc.is_empty() {
            true
        } else {
            match esc_lines {
                Some(el) => self
                    .line_regex_esc
                    .iter()
                    .all(|r| el.iter().any(|l| r.is_match(l))),
                None => false,
            }
        };
        esc_ok
            && self.contains.iter().all(|s| joined.contains(s.as_str()))
            && self.regex.iter().all(|r| r.is_match(joined))
            && self
                .line_regex
                .iter()
                .all(|r| lines.iter().any(|l| r.is_match(l)))
    }
}

#[derive(Debug, Clone)]
pub struct CompiledRule {
    pub id: String,
    pub state: AgentState,
    pub priority: i64,
    pub region: Region,
    pub matcher: CompiledMatcher,
    pub any: Vec<CompiledMatcher>,
    pub decline_keys: Vec<String>,
    pub auto_dismiss: bool,
}

impl CompiledRule {
    pub fn matches(&self, joined: &str, lines: &[&str]) -> bool {
        self.matches_esc(joined, lines, None)
    }

    /// `matches` with the region's escaped-capture lines available, for
    /// rules carrying `line_regex_esc` clauses.
    pub fn matches_esc(&self, joined: &str, lines: &[&str], esc_lines: Option<&[&str]>) -> bool {
        if self.matcher.matches_esc(joined, lines, esc_lines) {
            return true;
        }
        self.any
            .iter()
            .any(|m| m.matches_esc(joined, lines, esc_lines))
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Injection {
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub submit: String,
    #[serde(default)]
    pub verify_before_submit: bool,
    /// Substrings proving the paste staged; "<message_id>" is replaced with
    /// the delivery's marker before matching.
    #[serde(default)]
    pub verify_pattern: Vec<String>,
    #[serde(default)]
    pub safe_states: Vec<String>,
    #[serde(default)]
    pub unsafe_states: Vec<String>,
    /// "queues" when text pasted mid-turn stages and runs as its own turn
    /// (measured on Claude). Absent means unestablished: gate on idle.
    #[serde(default)]
    pub busy_behavior: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawManifest {
    agent: AgentMeta,
    #[serde(default)]
    hooks: Hooks,
    #[serde(default, rename = "rule")]
    rules: Vec<RawRule>,
    #[serde(default)]
    injection: Injection,
}

fn parse_state(s: &str) -> Option<AgentState> {
    Some(match s {
        "unknown" => AgentState::Unknown,
        "idle" => AgentState::Idle,
        "idle_with_input" => AgentState::IdleWithInput,
        "working" => AgentState::Working,
        "blocked_modal" => AgentState::BlockedModal,
        "blocked_permission" => AgentState::BlockedPermission,
        "blocked_quota" => AgentState::BlockedQuota,
        "dead" => AgentState::Dead,
        _ => return None,
    })
}

fn compile_matcher(
    id: &str,
    rule: &str,
    raw: &RawMatcher,
) -> Result<CompiledMatcher, ManifestError> {
    let mk = |pats: &[String]| -> Result<Vec<regex::Regex>, ManifestError> {
        pats.iter()
            .map(|p| {
                // Validation drafts use \x{2733}-style escapes (Python/PCRE);
                // the regex crate spells that \u{2733}.
                let translated = p.replace("\\x{", "\\u{");
                regex::Regex::new(&translated).map_err(|e| ManifestError::BadRegex {
                    id: id.into(),
                    rule: rule.into(),
                    pattern: p.clone(),
                    source: e,
                })
            })
            .collect()
    };
    Ok(CompiledMatcher {
        contains: raw.contains.clone(),
        regex: mk(&raw.regex)?,
        line_regex: mk(&raw.line_regex)?,
        line_regex_esc: mk(&raw.line_regex_esc)?,
    })
}

/// Remove CSI escape sequences (ESC [ ... final byte), i.e. the SGR styling
/// a capture-pane -e capture carries. Used to judge line emptiness in the
/// escaped capture the same way the plain capture does, so both region
/// slices select the same screen rows.
pub fn strip_csi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                // Parameter and intermediate bytes end at the final byte @..~.
                for n in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&n) {
                        break;
                    }
                }
            }
            // Bare ESC (or a non-CSI escape introducer) is dropped.
            continue;
        }
        out.push(c);
    }
    out
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Manifest, ManifestError> {
        let text = std::fs::read_to_string(path).map_err(|e| ManifestError::Io {
            path: path.into(),
            source: e,
        })?;
        Self::parse(&text, path)
    }

    pub fn parse(text: &str, path: &Path) -> Result<Manifest, ManifestError> {
        let raw: RawManifest = toml::from_str(text).map_err(|e| ManifestError::Parse {
            path: path.into(),
            source: e,
        })?;
        let id = raw.agent.id.clone();
        let mut rules = Vec::with_capacity(raw.rules.len());
        for r in &raw.rules {
            let state = parse_state(&r.state).ok_or_else(|| ManifestError::BadState {
                id: id.clone(),
                rule: r.id.clone(),
                state: r.state.clone(),
            })?;
            let region = Region::parse(&r.region).ok_or_else(|| ManifestError::BadRegion {
                id: id.clone(),
                rule: r.id.clone(),
                region: r.region.clone(),
            })?;
            let matcher = compile_matcher(&id, &r.id, &r.matcher)?;
            let any = r
                .any
                .iter()
                .map(|m| compile_matcher(&id, &r.id, m))
                .collect::<Result<Vec<_>, _>>()?;
            rules.push(CompiledRule {
                id: r.id.clone(),
                state,
                priority: r.priority,
                region,
                matcher,
                any,
                decline_keys: r.decline_keys.clone(),
                auto_dismiss: r.auto_dismiss,
            });
        }
        // Highest priority first; evaluation takes the first match.
        rules.sort_by_key(|r| std::cmp::Reverse(r.priority));
        Ok(Manifest {
            agent: raw.agent,
            hooks: raw.hooks,
            rules,
            injection: raw.injection,
            path: path.into(),
        })
    }

    /// Evaluate title + screen against the rules. Returns the winning rule.
    /// `screen` is a full visible-grid capture; region slicing happens here.
    /// Rules needing an escaped capture (`line_regex_esc`) cannot fire on
    /// this path; use `evaluate_esc` to supply one.
    pub fn evaluate(&self, title: &str, screen: &str) -> Option<&CompiledRule> {
        self.evaluate_esc(title, screen, None)
    }

    /// `evaluate` with an optional SGR-escaped capture (capture-pane -e) of
    /// the same grid. Escaped-region lines are judged non-empty on their
    /// CSI-stripped text so both captures slice the same screen rows.
    pub fn evaluate_esc(
        &self,
        title: &str,
        screen: &str,
        screen_esc: Option<&str>,
    ) -> Option<&CompiledRule> {
        let non_empty: Vec<&str> = screen
            .lines()
            .rev()
            .filter(|l| !l.trim().is_empty())
            .collect();
        let non_empty_esc: Option<Vec<&str>> = screen_esc.map(|s| {
            s.lines()
                .rev()
                .filter(|l| !strip_csi(l).trim().is_empty())
                .collect()
        });
        for rule in &self.rules {
            let (joined, lines, esc_lines): (String, Vec<&str>, Option<Vec<&str>>) =
                match rule.region {
                    Region::PaneTitle => (title.to_string(), vec![title], None),
                    Region::BottomNonEmptyLines(n) => {
                        // non_empty is bottom-up; restore top-down order.
                        let mut sel: Vec<&str> = non_empty.iter().take(n).copied().collect();
                        sel.reverse();
                        let esc = non_empty_esc.as_ref().map(|ne| {
                            let mut sel: Vec<&str> = ne.iter().take(n).copied().collect();
                            sel.reverse();
                            sel
                        });
                        (sel.join("\n"), sel, esc)
                    }
                };
            if rule.matches_esc(&joined, &lines, esc_lines.as_deref()) {
                return Some(rule);
            }
        }
        None
    }

    /// The modal rule matching a screen, if any. Used by the delivery gate
    /// to pick manifest-declared decline keys.
    pub fn matching_modal(&self, title: &str, screen: &str) -> Option<&CompiledRule> {
        self.evaluate(title, screen)
            .filter(|r| r.state.is_blocked())
    }

    /// True when any rule carries a `line_regex_esc` clause, i.e. the full
    /// rule set needs an SGR-escaped capture (capture-pane -e) to fire.
    /// The daemon uses this to decide whether to take the second capture.
    pub fn has_escaped_rules(&self) -> bool {
        self.rules.iter().any(|r| {
            !r.matcher.line_regex_esc.is_empty()
                || r.any.iter().any(|m| !m.line_regex_esc.is_empty())
        })
    }
}

/// All manifests in a directory, keyed by agent id.
pub fn load_dir(dir: &Path) -> Result<HashMap<String, Manifest>, ManifestError> {
    let mut out = HashMap::new();
    let entries = std::fs::read_dir(dir).map_err(|e| ManifestError::Io {
        path: dir.into(),
        source: e,
    })?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("toml") {
            let m = Manifest::load(&path)?;
            out.insert(m.agent.id.clone(), m);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINI: &str = r#"
[agent]
id = "claude"
display_name = "Claude Code"

[hooks]
ack = "UserPromptSubmit"
ack_payload_field = "prompt"

[[rule]]
id = "title_working_spinner"
state = "working"
priority = 1100
region = "pane_title"
regex = ['^[\x{2800}-\x{28FF}]']

[[rule]]
id = "title_idle_sparkle"
state = "idle"
priority = 1000
region = "pane_title"
regex = ['^\x{2733}']

[[rule]]
id = "composer_empty"
state = "idle"
priority = 900
region = "bottom_non_empty_lines(6)"
line_regex = ['^\s*❯\s*$']

[[rule]]
id = "startup_modal"
state = "blocked_modal"
priority = 1200
region = "bottom_non_empty_lines(16)"
any = [
  { contains = ["Enter to confirm"] },
  { contains = ["Esc to keep"] },
]
decline_keys = ["Escape"]
auto_dismiss = true

[injection]
method = "load-buffer + paste-buffer -p"
submit = "Enter"
verify_before_submit = true
verify_pattern = ["<message_id>"]
safe_states = ["idle"]
unsafe_states = ["blocked_modal"]
"#;

    fn manifest() -> Manifest {
        Manifest::parse(MINI, Path::new("mini.toml")).unwrap()
    }

    #[test]
    fn spinner_title_wins_over_screen() {
        let m = manifest();
        let r = m.evaluate("⠂ Run the tests", "some\nscreen\n❯ ").unwrap();
        assert_eq!(r.id, "title_working_spinner");
        assert_eq!(r.state, AgentState::Working);
    }

    #[test]
    fn idle_sparkle_title() {
        let m = manifest();
        let r = m.evaluate("✳ Run the tests", "").unwrap();
        assert_eq!(r.state, AgentState::Idle);
    }

    #[test]
    fn modal_beats_everything() {
        let m = manifest();
        let screen = "Claude in Chrome detected\n❯ 1. Yes\nEnter to confirm";
        let r = m.evaluate("plain title", screen).unwrap();
        assert_eq!(r.id, "startup_modal");
        assert_eq!(r.decline_keys, vec!["Escape"]);
        assert!(m.matching_modal("plain title", screen).is_some());
    }

    #[test]
    fn composer_line_regex_matches_bottom_region() {
        let m = manifest();
        let screen = "chat text\nmore\n\n❯ \n  hint line";
        let r = m.evaluate("plain", screen).unwrap();
        assert_eq!(r.id, "composer_empty");
        assert_eq!(r.state, AgentState::Idle);
    }

    #[test]
    fn no_match_is_none() {
        let m = manifest();
        assert!(m.evaluate("plain", "nothing to see").is_none());
    }

    #[test]
    fn bad_region_rejected() {
        let bad = MINI.replace("bottom_non_empty_lines(6)", "middle_of_screen");
        assert!(Manifest::parse(&bad, Path::new("bad.toml")).is_err());
    }

    #[test]
    fn strip_csi_removes_sgr_only() {
        assert_eq!(
            strip_csi("\u{1b}[1m›\u{1b}[0m \u{1b}[2mghost\u{1b}[0m"),
            "› ghost"
        );
        assert_eq!(strip_csi("plain"), "plain");
        assert_eq!(
            strip_csi("\u{1b}[38;2;246;226;183mcolor\u{1b}[39m"),
            "color"
        );
    }

    // Mini manifest exercising the escaped-capture discriminator: dim text
    // after the glyph is a ghost suggestion (idle), bare text is typed
    // input (idle_with_input), and the plain rule is the fallback.
    const ESC_MINI: &str = r#"
[agent]
id = "codex"
display_name = "Codex CLI"

[[rule]]
id = "composer_typed_input"
state = "idle_with_input"
priority = 1050
region = "bottom_non_empty_lines(6)"
line_regex_esc = ['^\s*(?:\x1b\[[0-9;]*m)*›(?:\x1b\[[0-9;]*m)*\s+[^\x1b\s]']

[[rule]]
id = "composer_ghost_suggestion"
state = "idle"
priority = 1040
region = "bottom_non_empty_lines(6)"
line_regex_esc = ['^\s*(?:\x1b\[[0-9;]*m)*›(?:\x1b\[[0-9;]*m)*\s+\x1b\[2m']

[[rule]]
id = "composer_empty_or_ghost"
state = "idle"
priority = 1000
region = "bottom_non_empty_lines(6)"
line_regex = ['^\s*›']

[injection]
verify_before_submit = true
verify_pattern = ["<message_id>"]
"#;

    #[test]
    fn esc_rules_fail_closed_without_escaped_capture() {
        let m = Manifest::parse(ESC_MINI, Path::new("esc.toml")).unwrap();
        // No escaped capture: both esc rules cannot fire, plain fallback wins.
        let r = m.evaluate("proj", "› typed text here").unwrap();
        assert_eq!(r.id, "composer_empty_or_ghost");
        assert_eq!(r.state, AgentState::Idle);
    }

    #[test]
    fn esc_rules_discriminate_ghost_from_typed() {
        let m = Manifest::parse(ESC_MINI, Path::new("esc.toml")).unwrap();
        let typed_plain = "› fix the rate limiter";
        let typed_esc = "\u{1b}[1m›\u{1b}[0m fix the rate limiter";
        let r = m
            .evaluate_esc("proj", typed_plain, Some(typed_esc))
            .unwrap();
        assert_eq!(r.id, "composer_typed_input");
        assert_eq!(r.state, AgentState::IdleWithInput);

        let ghost_plain = "› Find and fix a bug in @filename";
        let ghost_esc = "\u{1b}[1m›\u{1b}[0m \u{1b}[2mFind and fix a bug in @filename\u{1b}[0m";
        let r = m
            .evaluate_esc("proj", ghost_plain, Some(ghost_esc))
            .unwrap();
        assert_eq!(r.id, "composer_ghost_suggestion");
        assert_eq!(r.state, AgentState::Idle);
    }

    #[test]
    fn has_escaped_rules_reflects_esc_clauses() {
        let with = Manifest::parse(ESC_MINI, Path::new("esc.toml")).unwrap();
        assert!(with.has_escaped_rules());
        let without = manifest();
        assert!(!without.has_escaped_rules());
    }

    #[test]
    fn argv_basenames_parse_and_default_empty() {
        let with = format!("{MINI}\n");
        let m = Manifest::parse(&with, Path::new("mini.toml")).unwrap();
        assert!(m.agent.argv_basenames.is_empty());
        let extended = MINI.replace(
            "display_name = \"Claude Code\"",
            "display_name = \"Claude Code\"\nargv_basenames = [\"claude\"]",
        );
        let m = Manifest::parse(&extended, Path::new("mini.toml")).unwrap();
        assert_eq!(m.agent.argv_basenames, vec!["claude"]);
    }

    /// The shipped manifests must always parse. They are the product's seed
    /// data (validation campaign drafts plus decline actions).
    #[test]
    fn shipped_manifests_load() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../manifests");
        let all = load_dir(&dir).unwrap();
        for id in ["claude", "codex", "agy"] {
            let m = all
                .get(id)
                .unwrap_or_else(|| panic!("missing manifest {id}"));
            assert!(!m.rules.is_empty(), "{id}: no rules");
            assert!(m.injection.verify_before_submit, "{id}: verify gate off");
        }
        // Claude's title tier: the braille spinner must classify as working.
        let claude = &all["claude"];
        let r = claude.evaluate("⠂ Run sleep command", "").unwrap();
        assert_eq!(r.state, AgentState::Working);
        let r = claude.evaluate("✳ Done", "").unwrap();
        assert_eq!(r.state, AgentState::Idle);
        // Codex update dialog carries an explicit decline, never bare Enter.
        let codex = &all["codex"];
        let modal = codex
            .evaluate(
                "codexproj",
                "✨ Update available! 0.145.0 -> 0.146.0\n› 1. Update now\nPress enter to continue",
            )
            .unwrap();
        assert_eq!(modal.state, AgentState::BlockedModal);
        assert_eq!(modal.decline_keys, vec!["3", "Enter"]);
        assert!(modal.auto_dismiss);
        // Codex trust dialog must never auto-dismiss.
        let trust = codex
            .evaluate(
                "codexproj",
                "Do you trust the contents of this directory\n1. Yes",
            )
            .unwrap();
        assert!(!trust.auto_dismiss);
        // agy quota exhaustion is blocked_quota, not a modal.
        let agy = &all["agy"];
        let quota = agy
            .evaluate("mac", "⚠ Individual quota reached. Please upgrade your subscription to increase your limits.")
            .unwrap();
        assert_eq!(quota.state, AgentState::BlockedQuota);
    }
}
