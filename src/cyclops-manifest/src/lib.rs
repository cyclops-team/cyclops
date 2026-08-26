//! Per-CLI detection manifests.
//!
//! Everything Cyclops knows about a vendor TUI is data in a TOML file:
//! which sensors carry signal, how to read busy/idle from title or screen,
//! the modal vocabulary with explicit decline actions, and how to inject.
//! Seeded from the 2026-08-01 validation campaign's measured drafts.
//!
//! This crate parses, validates, compiles and EVALUATES a manifest: given
//! a title or a screen region, which rule matches, and at what priority.
//! Unknown TOML keys are tolerated so manifests can carry evidence notes
//! the code does not model.
//!
//! What it does not own, and each of these has bitten somebody:
//!
//! - Which sensor gets consulted, and what a tier disagreement means.
//!   That is fusion (`cyclopsd/src/fusion.rs`). This crate answers "does
//!   this text match", never "should you have looked".
//! - Reading the screen. Nothing here captures a pane or touches tmux.
//! - Loading the files. The daemon reads the directory once at boot into
//!   an immutable map, so editing a manifest takes a daemon restart. There
//!   is no hot reload; a previous version of this header said there was.
//! - Acting on a rule. A `decline_keys` list is data; sending those keys
//!   is the delivery gate's, and it is the gate that decides whether a
//!   modal may be dismissed at all.
//!
//! Three schema fields exist for vendor quirks that plain text cannot
//! express: `agent.argv_basenames` (bind by pane argv when the kernel comm
//! name is useless, e.g. native Claude installs reporting "2.1.220"), rule
//! `line_regex_esc` (match against a capture-pane -e capture, e.g. codex
//! ghost suggestions are only distinguishable from typed text by SGR dim),
//! and the `injection.composer_trailer_regex` pair (the measured sequence
//! of rows below the composer, in plain and escaped form, so the delivery
//! pipeline can decide whether the terminal sentinel is the last payload
//! row; the escaped half is the quirk plain text cannot express, since
//! chrome is painted and pasted human text is not).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub use regex::Regex;

use cyclops_proto::{AgentState, ComposerSemantic};
use serde::Deserialize;

fn enabled_by_default() -> bool {
    true
}

pub mod mailbox_capability;

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
    #[error("manifest {id}: hooks.turn_key_fields: {why}")]
    BadTurnKey { id: String, why: String },
    #[error("manifest {id}: hooks: {why}")]
    BadHooks { id: String, why: String },
    #[error("manifest {id}: injection: {why}")]
    BadInjection { id: String, why: String },
    #[error("manifest {id}: messaging: {why}")]
    BadMessaging { id: String, why: String },
}

/// Event names as the runtime compares them: ASCII alphanumerics only,
/// lowercased.
///
/// Vendors spell the same event differently across their own documents
/// and payloads, so the runtime matches on this reduced form. Anything
/// validating event names has to reduce them the same way or it validates
/// a spelling nobody compares.
pub fn normalize_event(event: &str) -> String {
    event
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Do two turn roles reduce to the same event?
///
/// An exact lifecycle asks one question of every hook payload: is this
/// the start of a turn, or the end of one. If both roles reduce to the
/// same event, one report answers both, and a start can record its own
/// end: the turn is over before it began, and the composer barrier
/// releases against a turn nothing ran. An acknowledgment that reduces to
/// the end name is the same defect on the receipt path, where an end
/// would verify a delivery and start the turn it just ended.
///
/// Start and acknowledgment MAY be the same event, and are in the shipped
/// vendors: taking the prompt is both the receipt and the beginning of
/// the turn. That one is a fact about the vendor, not a collision.
fn colliding_turn_roles(hooks: &Hooks) -> Option<String> {
    fn declared(e: &Option<String>) -> Option<&str> {
        e.as_deref().filter(|n| !n.trim().is_empty())
    }
    // A declared role has to reduce to a name the runtime can compare.
    // Punctuation and non-ASCII spellings reduce to nothing, so such a
    // role never matches the event it meant, and any two of them are
    // indistinguishable from each other and from an incoming event that
    // also reduces to nothing.
    for (field, raw) in [
        ("turn_start", &hooks.turn_start),
        ("turn_end", &hooks.turn_end),
        ("ack", &hooks.ack),
    ] {
        if let Some(name) = declared(raw) {
            if normalize_event(name).is_empty() {
                return Some(format!("{field} {name:?} has no comparable name"));
            }
        }
    }
    for event in &hooks.turn_end_confirmed {
        if normalize_event(event).is_empty() {
            return Some(format!(
                "confirmed lifecycle end {event:?} has no comparable name"
            ));
        }
    }
    let named = |e: &Option<String>| declared(e).map(normalize_event);
    // Each pair is checked on its own. A manifest that declares only an
    // acknowledgment and an end still runs both roles at runtime, so
    // requiring a start before looking would let that one through.
    let end = named(&hooks.turn_end);
    if let (Some(start), Some(end)) = (named(&hooks.turn_start), end.as_deref()) {
        if start == end {
            return Some(format!(
                "hooks.turn_start and hooks.turn_end are the same event {end:?}"
            ));
        }
    }
    if let (Some(ack), Some(end)) = (named(&hooks.ack), end.as_deref()) {
        if ack == end {
            return Some(format!(
                "hooks.ack and hooks.turn_end are the same event {end:?}"
            ));
        }
    }
    let mut roles: Vec<(String, LifecycleRole)> = Vec::new();
    if let Some(start) = named(&hooks.turn_start) {
        roles.push((start, LifecycleRole::Start));
    }
    if let Some(end) = named(&hooks.turn_end) {
        roles.push((end, LifecycleRole::End));
    }
    roles.extend(
        hooks
            .turn_end_confirmed
            .iter()
            .map(|event| (normalize_event(event), LifecycleRole::End)),
    );
    for (index, (event, role)) in roles.iter().enumerate() {
        if roles[..index]
            .iter()
            .any(|(prior_event, prior_role)| prior_event == event && prior_role != role)
        {
            return Some(format!("lifecycle event {event:?} has both turn roles"));
        }
    }
    if let Some(ack) = named(&hooks.ack) {
        if roles
            .iter()
            .any(|(event, role)| *role == LifecycleRole::End && *event == ack)
        {
            return Some(format!(
                "hooks.ack and a lifecycle end are the same event {ack:?}"
            ));
        }
    }
    None
}

/// A parsed, validated manifest with compiled regexes.
#[derive(Debug, Clone)]
pub struct Manifest {
    pub agent: AgentMeta,
    pub hooks: Hooks,
    pub messaging: Messaging,
    pub rules: Vec<CompiledRule>,
    pub injection: Injection,
    /// Compiled `injection.composer_trailer_regex`, validated at parse time
    /// like rule patterns so a bad regex is a load error, not a surprise
    /// during a delivery.
    pub composer_trailers: Vec<regex::Regex>,
    /// Compiled `injection.composer_trailer_regex_esc`.
    pub composer_trailers_esc: Vec<regex::Regex>,
    /// Compiled collapsed-chip row patterns, plain and escaped.
    pub composer_chips: Vec<regex::Regex>,
    pub composer_chips_esc: Vec<regex::Regex>,
    /// Plain joined-capture row that starts the active composer.
    pub composer_prompt: Option<regex::Regex>,
    /// Plain joined-capture row for every later logical composer line.
    pub composer_continuation: Option<regex::Regex>,
    /// Source path, for reload and error messages.
    pub path: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Messaging {
    #[serde(default)]
    pub mailbox_capability_file: Option<PathBuf>,
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
    /// The command that starts this CLI, for `cyclops start --agents <id>`.
    /// Nothing in detection reads it: binding a pane is the two lists above.
    /// Absent means cyclops cannot start this CLI, and `--agents` refuses
    /// the id rather than guessing at a binary name.
    #[serde(default)]
    pub launch: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleRole {
    Start,
    End,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleCertainty {
    #[default]
    Candidate,
    Confirmed,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AckEvidence {
    #[default]
    Receipt,
    Dispatch,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Hooks {
    #[serde(default)]
    pub config_mechanism: String,
    #[serde(default)]
    pub turn_start: Option<String>,
    /// What the start event proves when it arrives.
    #[serde(default)]
    pub turn_start_evidence: LifecycleCertainty,
    #[serde(default)]
    pub turn_end: Option<String>,
    /// What the end event proves when it arrives.
    #[serde(default)]
    pub turn_end_evidence: LifecycleCertainty,
    /// Additional end events that are conclusive on arrival.
    #[serde(default)]
    pub turn_end_confirmed: Vec<String>,
    /// Quiet period required before a candidate end may use terminal screen
    /// evidence. This covers vendors that run sibling stop hooks concurrently.
    #[serde(default)]
    pub turn_end_settle_ms: u64,
    /// Hook whose payload acknowledges a delivery. None means this CLI has
    /// no payload-matchable ACK and runs on the screen-verified tier (agy).
    #[serde(default)]
    pub ack: Option<String>,
    /// Whether the ACK event proves receipt or only dispatch to vendor hooks.
    #[serde(default)]
    pub ack_evidence: AckEvidence,
    #[serde(default)]
    pub available: Vec<String>,
    /// Payload field carrying the injected text when `ack` is set.
    #[serde(default)]
    pub ack_payload_field: Option<String>,
    /// Payload fields that together name ONE turn, in declared order.
    ///
    /// Declaring these opts a vendor into exact turn correlation: a start
    /// and an end whose values match on every field are the same turn,
    /// and nothing else is. Empty means the vendor cannot prove that, and
    /// its turns run on screen evidence instead.
    ///
    /// Every field has to appear on BOTH the start and the end event, or
    /// the pair can never match. Ordering is part of the declaration
    /// because the values are compared positionally.
    #[serde(default)]
    pub turn_key_fields: Vec<String>,
    /// Flag that points this CLI at a hook config file at launch, when it
    /// has one. Some means the pane can be started already wired, so
    /// nothing has to be written into the vendor's own config tree:
    /// claude reads hooks ONLY from the settings file it was launched
    /// with, and `--settings <path>` is the whole wiring step.
    ///
    /// None is the common case and means the opposite: the CLI discovers
    /// hooks from a fixed location it owns, so wiring it is a file
    /// placement rather than a launch argument (codex reads
    /// $CODEX_HOME/hooks.json, agy reads <workspace>/.agents/hooks.json).
    #[serde(default)]
    pub settings_flag: Option<String>,
}

impl Hooks {
    /// Classify a lifecycle event and the evidence it carries.
    pub fn lifecycle_event(&self, event: &str) -> Option<(LifecycleRole, LifecycleCertainty)> {
        let event = normalize_event(event);
        self.turn_end_confirmed
            .iter()
            .find(|name| normalize_event(name) == event)
            .map(|_| (LifecycleRole::End, LifecycleCertainty::Confirmed))
            .or_else(|| {
                self.turn_start
                    .as_deref()
                    .filter(|name| normalize_event(name) == event)
                    .map(|_| (LifecycleRole::Start, self.turn_start_evidence))
            })
            .or_else(|| {
                self.turn_end
                    .as_deref()
                    .filter(|name| normalize_event(name) == event)
                    .map(|_| (LifecycleRole::End, self.turn_end_evidence))
            })
    }

    pub fn has_lifecycle_role(&self, role: LifecycleRole) -> bool {
        match role {
            LifecycleRole::Start => self
                .turn_start
                .as_deref()
                .is_some_and(|name| !name.trim().is_empty()),
            LifecycleRole::End => {
                self.turn_end
                    .as_deref()
                    .is_some_and(|name| !name.trim().is_empty())
                    || !self.turn_end_confirmed.is_empty()
            }
        }
    }

    pub fn lifecycle_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.turn_end_confirmed.iter().map(String::as_str).collect();
        names.extend(self.turn_start.as_deref());
        names.extend(self.turn_end.as_deref());
        names
    }
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
    /// Match only when the escaped capture contains no escape bytes at all.
    /// This is for vendor layouts measured under NO_COLOR. Without this
    /// guard a plain fallback would also bypass stronger style evidence in a
    /// colored capture.
    #[serde(default)]
    pub unstyled_only: bool,
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
    /// Paired by index with `regex`: clause `i` runs against the region's
    /// SGR-escaped rows joined with newlines and must match starting on the
    /// same line and ending on the same line as `regex[i]` does on the plain
    /// rows. That ties the styled rows to the exact rows the plain pattern
    /// proved, not merely to the same region. MEASURED (Claude Code 2.1.246,
    /// probe a91f): the completed-turn suffix is a uniform 38;5;246 row
    /// followed by the composer box; an older genuine styled completion
    /// above a later unstyled completion-shaped row must not count. The
    /// counts must be equal when `regex_esc` is present; the clause fails
    /// closed when no escaped capture was provided.
    #[serde(default)]
    pub regex_esc: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawRule {
    pub id: String,
    pub state: String,
    /// Vendor-measured meaning of the composer shape this rule matches.
    #[serde(default)]
    pub composer_semantic: Option<ComposerSemantic>,
    pub priority: i64,
    pub region: String,
    /// Whether this rule may confirm or cancel a candidate hook lifecycle.
    /// Advisory rules still report runtime state and block unsafe writes.
    #[serde(default = "enabled_by_default")]
    pub lifecycle_evidence: bool,
    /// Whether this exact idle lifecycle rule may retire an authenticated
    /// active-start hook when it wins the current, binding-stable screen
    /// capture. This is deliberately separate from `lifecycle_evidence`:
    /// ordinary lifecycle-idle rules can confirm candidate dispatches but
    /// must not erase an authenticated start before its terminal hook.
    #[serde(default)]
    pub active_start_terminal: bool,
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
    pub unstyled_only: bool,
    pub contains: Vec<String>,
    pub regex: Vec<regex::Regex>,
    pub line_regex: Vec<regex::Regex>,
    pub line_regex_esc: Vec<regex::Regex>,
    pub regex_esc: Vec<regex::Regex>,
}

impl CompiledMatcher {
    fn is_empty(&self) -> bool {
        self.contains.is_empty()
            && self.regex.is_empty()
            && self.line_regex.is_empty()
            && self.line_regex_esc.is_empty()
            && self.regex_esc.is_empty()
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
        if self.unstyled_only
            && esc_lines.is_none_or(|rows| rows.iter().any(|row| row.contains('\u{1b}')))
        {
            return false;
        }
        let esc_ok = if self.line_regex_esc.is_empty() && self.regex_esc.is_empty() {
            true
        } else {
            match esc_lines {
                Some(el) => {
                    self.line_regex_esc
                        .iter()
                        .all(|r| el.iter().any(|l| r.is_match(l)))
                        && paired_spans_match(&self.regex, &self.regex_esc, lines, el)
                }
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

/// Do the paired `regex`/`regex_esc` clauses each match the SAME line span?
///
/// The plain and escaped rows are aligned one to one (both come from the
/// same bottom-up non-empty filter), so a match is located by the line it
/// starts on and the line it ends on. For every pair there must be one line
/// at which both patterns match starting there and both end on the same
/// line. A pattern that only matches from an older row while its twin
/// matches from a later row proves nothing about the same rows.
fn paired_spans_match(
    plain: &[regex::Regex],
    esc: &[regex::Regex],
    lines: &[&str],
    esc_lines: &[&str],
) -> bool {
    if esc.is_empty() {
        return true;
    }
    if plain.len() != esc.len() || lines.len() != esc_lines.len() {
        return false;
    }
    let joined_plain = lines.join("\n");
    let joined_esc = esc_lines.join("\n");
    let starts = |rows: &[&str]| -> Vec<usize> {
        let mut offsets = Vec::with_capacity(rows.len());
        let mut at = 0usize;
        for row in rows {
            offsets.push(at);
            at += row.len() + 1;
        }
        offsets
    };
    let plain_starts = starts(lines);
    let esc_starts = starts(esc_lines);
    let end_line = |text: &str, end: usize| text[..end].matches('\n').count();
    plain.iter().zip(esc.iter()).all(|(p, e)| {
        (0..lines.len()).any(|line| {
            let plain_end = p
                .find_at(&joined_plain, plain_starts[line])
                .filter(|m| m.start() == plain_starts[line])
                .map(|m| end_line(&joined_plain, m.end()));
            let esc_end = e
                .find_at(&joined_esc, esc_starts[line])
                .filter(|m| m.start() == esc_starts[line])
                .map(|m| end_line(&joined_esc, m.end()));
            matches!((plain_end, esc_end), (Some(a), Some(b)) if a == b)
        })
    })
}

impl CompiledRule {
    /// Does this rule hold for ONE row, given in both forms?
    ///
    /// For callers that have already isolated a row and need to know
    /// whether the manifest recognizes it. Going through the rule keeps
    /// the manifest's own semantics: clauses within a matcher AND
    /// together, `any` alternatives are alternatives, and an esc clause
    /// with no escaped capture fails closed. A caller that reimplements
    /// this as "plain matched OR escaped matched" quietly weakens every
    /// rule that relies on both halves, and the vendor's plain pattern
    /// stops being load-bearing without anyone noticing.
    pub fn matches_row(&self, plain: &str, esc: &str) -> bool {
        let lines = [plain];
        let esc_lines = [esc];
        std::iter::once(&self.matcher)
            .chain(self.any.iter())
            .any(|m| m.matches_esc(plain, &lines, Some(&esc_lines)))
    }
}

#[derive(Debug, Clone)]
pub struct CompiledRule {
    pub id: String,
    pub state: AgentState,
    pub composer_semantic: Option<ComposerSemantic>,
    pub priority: i64,
    pub region: Region,
    pub lifecycle_evidence: bool,
    pub active_start_terminal: bool,
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
    /// Exact tmux key names that clear the whole staged composer.
    /// Empty means this vendor has no measured discard capability.
    #[serde(default)]
    pub clear_keys: Vec<String>,
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
    /// Legacy measurement metadata. Delivery never uses it as write authority.
    /// Omit it from new manifests and encode safety through measured states.
    #[serde(default)]
    pub busy_behavior: Option<String>,
    /// Lines the vendor may render BELOW the composer, which are never
    /// payload: shortcut hints, context meters, status rows. Used only to
    /// decide terminal-sentinel position; anything after the sentinel that
    /// matches none of these fails verification closed.
    #[serde(default)]
    pub composer_trailer_regex: Vec<String>,
    /// The same rows matched against the SGR-escaped capture. Required
    /// alongside the plain patterns: chrome is painted by the vendor and
    /// therefore styled, while text a human pasted into the composer is
    /// not, so the escaped form is what separates a status row from prose
    /// that merely reads like one. A row counts as chrome only when both
    /// forms match, and a manifest carrying these fails closed when no
    /// escaped capture is available.
    #[serde(default)]
    pub composer_trailer_regex_esc: Vec<String>,
    /// Explicit proof available when a vendor renders the composer without
    /// SGR, for example under `NO_COLOR`.
    #[serde(default)]
    pub unstyled_composer_proof: Option<UnstyledComposerProof>,
    /// How many of those rows are REQUIRED, counted from the top of the
    /// layout. The rows below a composer are not an unordered set: the
    /// vendor paints its box rule and status row every time, its hint or
    /// mode rows only sometimes. Without this the anchors can simply be
    /// absent while an arbitrary plausible tail still passes. Zero,
    /// missing, or larger than the layout means the layout is not
    /// measured, and the sentinel path refuses.
    #[serde(default)]
    pub composer_trailer_required_prefix: usize,
    /// The vendor's collapsed-paste chip, as a WHOLE composer row, in
    /// plain and escaped form. Both are required together.
    ///
    /// This replaced a substring test. A generic `verify_pattern` such as
    /// "Pasted" was matched anywhere on a composer row, so a message whose
    /// own subject contained that word verified a paste whose sentinel had
    /// never arrived: the truncated payload submitted itself. A chip is a
    /// specific rendering, so proving it means matching the row the vendor
    /// actually draws, styling included. A manifest with no measured chip
    /// syntax has no chip lane at all.
    #[serde(default)]
    pub composer_chip_regex: Vec<String>,
    #[serde(default)]
    pub composer_chip_regex_esc: Vec<String>,
    /// Joined-capture row patterns with one named `content` capture.
    /// Both are required for exact visible composer extraction.
    #[serde(default)]
    pub composer_prompt_regex: Option<String>,
    #[serde(default)]
    pub composer_continuation_regex: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UnstyledComposerProof {
    /// The first required trailer row is a measured boundary outside the
    /// prompt and continuation row shapes.
    StructuralTrailer,
}

#[derive(Debug, Clone, Deserialize)]
struct RawManifest {
    agent: AgentMeta,
    #[serde(default)]
    hooks: Hooks,
    #[serde(default)]
    messaging: Messaging,
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
    if !raw.regex_esc.is_empty() && raw.regex_esc.len() != raw.regex.len() {
        return Err(ManifestError::BadInjection {
            id: id.into(),
            why: format!(
                "{rule}: regex_esc must pair one-to-one with regex ({} vs {})",
                raw.regex_esc.len(),
                raw.regex.len()
            ),
        });
    }
    Ok(CompiledMatcher {
        unstyled_only: raw.unstyled_only,
        contains: raw.contains.clone(),
        regex: mk(&raw.regex)?,
        line_regex: mk(&raw.line_regex)?,
        line_regex_esc: mk(&raw.line_regex_esc)?,
        regex_esc: mk(&raw.regex_esc)?,
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

/// A named tmux key or modified chord, excluding text and edit aliases.
fn clear_key_name(key: &str) -> bool {
    const NAMED_NON_TEXT: &[&str] = &[
        "escape", "up", "down", "left", "right", "home", "end", "npage", "ppage",
    ];
    const NAMED_TEXT_OR_EDIT: &[&str] = &[
        "space",
        "bspace",
        "backspace",
        "tab",
        "btab",
        "dc",
        "delete",
        "ic",
        "insert",
        "enter",
        "return",
        "kpenter",
        "linefeed",
    ];

    let normalized = key.to_ascii_lowercase();
    let parts: Vec<&str> = normalized.split('-').collect();
    let Some((base, modifiers)) = parts.split_last() else {
        return false;
    };
    if NAMED_TEXT_OR_EDIT.contains(base) {
        return false;
    }
    if modifiers
        .iter()
        .any(|modifier| !matches!(*modifier, "c" | "m" | "s"))
    {
        return false;
    }
    if modifiers.contains(&"c") && matches!(*base, "m" | "j") {
        return false;
    }
    let function_key = base
        .strip_prefix('f')
        .and_then(|number| number.parse::<u8>().ok())
        .is_some_and(|number| (1..=24).contains(&number));
    let named = NAMED_NON_TEXT.contains(base) || function_key;
    if modifiers.is_empty() {
        named
    } else {
        named || (base.len() == 1 && base.chars().all(|character| character.is_ascii_graphic()))
    }
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
                composer_semantic: r.composer_semantic,
                priority: r.priority,
                region,
                lifecycle_evidence: r.lifecycle_evidence,
                active_start_terminal: r.active_start_terminal,
                matcher,
                any,
                decline_keys: r.decline_keys.clone(),
                auto_dismiss: r.auto_dismiss,
            });
        }
        // Highest priority first; evaluation takes the first match.
        rules.sort_by_key(|r| std::cmp::Reverse(r.priority));
        let compile_trailers =
            |pats: &[String], field: &str| -> Result<Vec<regex::Regex>, ManifestError> {
                pats.iter()
                    .map(|p| {
                        let translated = p.replace("\\x{", "\\u{");
                        regex::Regex::new(&translated).map_err(|e| ManifestError::BadRegex {
                            id: raw.agent.id.clone(),
                            rule: field.into(),
                            pattern: p.clone(),
                            source: e,
                        })
                    })
                    .collect()
            };
        let composer_trailers = compile_trailers(
            &raw.injection.composer_trailer_regex,
            "injection.composer_trailer_regex",
        )?;
        let composer_trailers_esc = compile_trailers(
            &raw.injection.composer_trailer_regex_esc,
            "injection.composer_trailer_regex_esc",
        )?;
        let composer_chips = compile_trailers(
            &raw.injection.composer_chip_regex,
            "injection.composer_chip_regex",
        )?;
        let composer_chips_esc = compile_trailers(
            &raw.injection.composer_chip_regex_esc,
            "injection.composer_chip_regex_esc",
        )?;
        let compile_content_row = |pattern: &Option<String>,
                                   field: &str|
         -> Result<Option<regex::Regex>, ManifestError> {
            let Some(pattern) = pattern else {
                return Ok(None);
            };
            let translated = pattern.replace("\\x{", "\\u{");
            let compiled =
                regex::Regex::new(&translated).map_err(|source| ManifestError::BadRegex {
                    id: raw.agent.id.clone(),
                    rule: field.into(),
                    pattern: pattern.clone(),
                    source,
                })?;
            if !pattern.starts_with('^')
                || !pattern.ends_with('$')
                || !compiled
                    .capture_names()
                    .flatten()
                    .any(|name| name == "content")
            {
                return Err(ManifestError::BadInjection {
                    id: raw.agent.id.clone(),
                    why: format!(
                        "{field} must anchor the whole row and define a named 'content' capture"
                    ),
                });
            }
            Ok(Some(compiled))
        };
        let composer_prompt = compile_content_row(
            &raw.injection.composer_prompt_regex,
            "injection.composer_prompt_regex",
        )?;
        let composer_continuation = compile_content_row(
            &raw.injection.composer_continuation_regex,
            "injection.composer_continuation_regex",
        )?;
        if let Some(capability_file) = &raw.messaging.mailbox_capability_file {
            let shown = capability_file.to_string_lossy();
            let path_is_supported =
                capability_file.is_absolute() || shown == "~" || shown.starts_with("~/");
            if !path_is_supported {
                return Err(ManifestError::BadMessaging {
                    id: raw.agent.id.clone(),
                    why: "mailbox_capability_file must be absolute or start with ~/".into(),
                });
            }
        }
        if composer_prompt.is_some() != composer_continuation.is_some() {
            return Err(ManifestError::BadInjection {
                id: raw.agent.id.clone(),
                why: "composer_prompt_regex and composer_continuation_regex must be declared together"
                    .into(),
            });
        }
        if raw.injection.unstyled_composer_proof.is_some()
            && (composer_prompt.is_none()
                || composer_trailers.is_empty()
                || raw.injection.composer_trailer_required_prefix == 0)
        {
            return Err(ManifestError::BadInjection {
                id: raw.agent.id.clone(),
                why: "unstyled_composer_proof requires composer extraction patterns and a measured required trailer"
                    .into(),
            });
        }
        if !raw.injection.clear_keys.is_empty() {
            if composer_prompt.is_none() {
                return Err(ManifestError::BadInjection {
                    id: raw.agent.id.clone(),
                    why: "clear_keys requires measured composer extraction patterns".into(),
                });
            }
            for key in &raw.injection.clear_keys {
                if key.len() <= 1
                    || !key.chars().all(|character| character.is_ascii_graphic())
                    || !clear_key_name(key)
                    || key.eq_ignore_ascii_case(&raw.injection.submit)
                {
                    return Err(ManifestError::BadInjection {
                        id: raw.agent.id.clone(),
                        why: format!("clear_keys contains unsafe key {key:?}"),
                    });
                }
            }
        }
        if composer_chips.len() != composer_chips_esc.len() {
            return Err(ManifestError::BadRegion {
                id: raw.agent.id.clone(),
                rule: "injection.composer_chip_regex_esc".into(),
                region: format!(
                    "{} escaped chip rows against {} plain",
                    composer_chips_esc.len(),
                    composer_chips.len()
                ),
            });
        }
        // A required prefix is a claim about a measured layout, so it is
        // only meaningful when the layout is actually there. Declaring one
        // without the rows, or a count the rows cannot satisfy, describes
        // nothing.
        let required = raw.injection.composer_trailer_required_prefix;
        let declares_layout = !composer_trailers.is_empty() || !composer_trailers_esc.is_empty();
        if (declares_layout || required != 0)
            && (required == 0 || required > composer_trailers.len())
        {
            return Err(ManifestError::BadRegion {
                id: raw.agent.id.clone(),
                rule: "injection.composer_trailer_required_prefix".into(),
                region: format!("{required} required of {} rows", composer_trailers.len()),
            });
        }
        // A turn key is a claim that two EVENTS can be matched, so it is
        // meaningless without both of them, and a field that is empty or
        // repeated cannot carry a position in an ordered comparison.
        // Rejecting is the point: a partial declaration that degraded
        // silently would put a vendor on the screen lifecycle while its
        // manifest says otherwise.
        // Roles first, and for EVERY declared lifecycle. The runtime maps a
        // report to start, end or acknowledgment the same way whether or
        // not a turn key exists, so a collision is wrong wherever it
        // appears; checking it only for keyed vendors would leave the
        // screen lane holding the same contradiction.
        if let Some(why) = colliding_turn_roles(&raw.hooks) {
            return Err(ManifestError::BadHooks {
                id: raw.agent.id.clone(),
                why,
            });
        }
        let key = &raw.hooks.turn_key_fields;
        if !key.is_empty() {
            let why = if !raw.hooks.has_lifecycle_role(LifecycleRole::Start)
                || !raw.hooks.has_lifecycle_role(LifecycleRole::End)
            {
                // Blank counts as absent: an event name that never matches
                // anything loads a manifest claiming exact correlation
                // whose lane can never complete a turn.
                Some("declared without both lifecycle start and end events".to_string())
            } else if key.iter().any(|f| f.trim().is_empty()) {
                Some("empty field name".to_string())
            } else {
                key.iter()
                    .enumerate()
                    .find(|(i, f)| key[..*i].contains(f))
                    .map(|(_, f)| format!("duplicate field {f:?}"))
            };
            if let Some(why) = why {
                return Err(ManifestError::BadTurnKey {
                    id: raw.agent.id.clone(),
                    why,
                });
            }
        }
        let candidate_start = raw
            .hooks
            .turn_start
            .as_ref()
            .is_some_and(|_| raw.hooks.turn_start_evidence == LifecycleCertainty::Candidate);
        let candidate_end = raw
            .hooks
            .turn_end
            .as_ref()
            .is_some_and(|_| raw.hooks.turn_end_evidence == LifecycleCertainty::Candidate);
        let has_ack = raw
            .hooks
            .ack
            .as_deref()
            .is_some_and(|name| !name.trim().is_empty());
        let has_payload = raw
            .hooks
            .ack_payload_field
            .as_deref()
            .is_some_and(|field| !field.trim().is_empty());
        let same_start_ack = match (raw.hooks.turn_start.as_deref(), raw.hooks.ack.as_deref()) {
            (Some(start), Some(ack)) if !start.trim().is_empty() && !ack.trim().is_empty() => {
                normalize_event(start) == normalize_event(ack)
            }
            _ => false,
        };
        // Claude exposes one event-local fact: UserPromptSubmit both starts a
        // candidate and dispatches the exact prompt to the hook pipeline. It
        // exposes no field that can match that start to Stop. This narrow
        // shape may therefore omit a turn key and let visual evidence end the
        // candidate. Any declared end still claims cross-event correlation.
        let unkeyed_start_dispatch = key.is_empty()
            && candidate_start
            && raw.hooks.turn_end.is_none()
            && raw.hooks.turn_end_confirmed.is_empty()
            && raw.hooks.turn_end_settle_ms == 0
            && raw.hooks.ack_evidence == AckEvidence::Dispatch
            && has_ack
            && has_payload
            && same_start_ack;
        if (candidate_start || candidate_end) && key.is_empty() && !unkeyed_start_dispatch {
            return Err(ManifestError::BadTurnKey {
                id: raw.agent.id.clone(),
                why: "candidate lifecycle evidence requires hooks.turn_key_fields unless it is an unkeyed start-only dispatch on the same event"
                    .to_string(),
            });
        }
        if candidate_end && raw.hooks.turn_end_settle_ms == 0 {
            return Err(ManifestError::BadHooks {
                id: raw.agent.id.clone(),
                why: "candidate lifecycle end requires hooks.turn_end_settle_ms".to_string(),
            });
        }
        if raw.hooks.ack_evidence == AckEvidence::Dispatch
            && (!candidate_start
                || !has_ack
                || !has_payload
                || (key.is_empty() && !unkeyed_start_dispatch))
        {
            return Err(ManifestError::BadHooks {
                id: raw.agent.id.clone(),
                why: "dispatch acknowledgment requires a candidate turn start, hooks.ack, hooks.ack_payload_field, and either hooks.turn_key_fields or the same unkeyed start-only event"
                    .to_string(),
            });
        }
        // The two lists are one layout described twice: entry i is row i of
        // the measured sequence below the composer, in plain and escaped
        // form. Different lengths mean the layout is not actually measured,
        // and a half-described layout must not verify anything.
        if (!composer_trailers.is_empty() || !composer_trailers_esc.is_empty())
            && composer_trailers_esc.len() != composer_trailers.len()
        {
            return Err(ManifestError::BadRegion {
                id: raw.agent.id.clone(),
                rule: "injection.composer_trailer_regex_esc".into(),
                region: format!(
                    "{} escaped rows against {} plain rows",
                    composer_trailers_esc.len(),
                    composer_trailers.len()
                ),
            });
        }
        Ok(Manifest {
            agent: raw.agent,
            hooks: raw.hooks,
            messaging: raw.messaging,
            rules,
            injection: raw.injection,
            composer_trailers,
            composer_trailers_esc,
            composer_chips,
            composer_chips_esc,
            composer_prompt,
            composer_continuation,
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
        // A chip-only manifest declares its escaped proof here and
        // nowhere else; without this it would be handed a plain capture
        // and could never satisfy the very pattern it declared.
        !self.composer_chips_esc.is_empty()
            || !self.composer_trailers_esc.is_empty()
            || self.rules.iter().any(|r| {
                r.matcher.unstyled_only
                    || !r.matcher.line_regex_esc.is_empty()
                    || r.any
                        .iter()
                        .any(|m| m.unstyled_only || !m.line_regex_esc.is_empty())
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
        assert_eq!(r.composer_semantic, None);
    }

    #[test]
    fn composer_semantic_is_optional_and_closed() {
        let annotated = MINI.replace(
            "id = \"composer_empty\"\nstate = \"idle\"\npriority = 900",
            "id = \"composer_empty\"\nstate = \"idle\"\ncomposer_semantic = \"clean\"\npriority = 900",
        );
        let manifest = Manifest::parse(&annotated, Path::new("semantic.toml")).unwrap();
        let rule = manifest
            .rules
            .iter()
            .find(|rule| rule.id == "composer_empty")
            .unwrap();
        assert_eq!(rule.composer_semantic, Some(ComposerSemantic::Clean));

        let invalid = annotated.replace(
            "composer_semantic = \"clean\"",
            "composer_semantic = \"unsupported\"",
        );
        assert!(matches!(
            Manifest::parse(&invalid, Path::new("bad-semantic.toml")),
            Err(ManifestError::Parse { .. })
        ));
    }

    #[test]
    fn no_match_is_none() {
        let m = manifest();
        assert!(m.evaluate("plain", "nothing to see").is_none());
    }

    #[test]
    fn mailbox_capability_is_generic_manifest_data() {
        let body = format!(
            "{MINI}\n[messaging]\nmailbox_capability_file = \"/agent/skills/cyclops/SKILL.md\"\n"
        );
        let manifest = Manifest::parse(&body, Path::new("capable.toml")).unwrap();
        let capability_file = manifest
            .messaging
            .mailbox_capability_file
            .expect("manifest declares mailbox capability evidence");
        assert_eq!(
            capability_file,
            PathBuf::from("/agent/skills/cyclops/SKILL.md")
        );

        let malformed = body.replace(
            "/agent/skills/cyclops/SKILL.md",
            "relative/skills/cyclops/SKILL.md",
        );
        assert!(matches!(
            Manifest::parse(&malformed, Path::new("bad-capability.toml")),
            Err(ManifestError::BadMessaging { .. })
        ));
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
    fn an_unstyled_matcher_never_bypasses_colored_evidence() {
        let source = r#"
[agent]
id = "plain"
display_name = "Plain"

[[rule]]
id = "unstyled_working"
state = "working"
priority = 100
region = "bottom_non_empty_lines(3)"
lifecycle_evidence = false
unstyled_only = true
contains = ["Working"]

[[rule]]
id = "fallback"
state = "idle"
priority = 1
region = "bottom_non_empty_lines(3)"
contains = ["Working"]
"#;
        let manifest = Manifest::parse(source, Path::new("plain.toml")).unwrap();
        assert!(manifest.has_escaped_rules());
        assert!(!manifest.rules[0].lifecycle_evidence);
        assert!(manifest.rules[1].lifecycle_evidence);
        let plain = manifest
            .evaluate_esc("title", "Working", Some("Working"))
            .unwrap();
        assert_eq!(plain.id, "unstyled_working");

        let colored = manifest
            .evaluate_esc("title", "Working", Some("\u{1b}[31mWorking\u{1b}[0m"))
            .unwrap();
        assert_eq!(colored.id, "fallback");
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

    #[test]
    fn launch_parses_and_defaults_to_none() {
        let m = Manifest::parse(MINI, Path::new("mini.toml")).unwrap();
        assert_eq!(m.agent.launch, None, "a manifest need not say how to start");
        let named = MINI.replace(
            "display_name = \"Claude Code\"",
            "display_name = \"Claude Code\"\nlaunch = \"claude\"",
        );
        let m = Manifest::parse(&named, Path::new("mini.toml")).unwrap();
        assert_eq!(m.agent.launch.as_deref(), Some("claude"));
    }

    /// The shipped manifests must always parse. They are the product's seed
    /// data (validation campaign drafts plus decline actions).
    #[test]
    fn shipped_manifests_load() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/manifests");
        let all = load_dir(&dir).unwrap();
        for id in ["claude", "codex", "agy", "cursor"] {
            let m = all
                .get(id)
                .unwrap_or_else(|| panic!("missing manifest {id}"));
            assert!(!m.rules.is_empty(), "{id}: no rules");
            assert!(m.injection.verify_before_submit, "{id}: verify gate off");
            // Every shipped CLI can be named to `cyclops start --agents`.
            // A shipped file without this is a CLI cyclops detects but
            // cannot start, which reads as a missing manifest to the
            // operator who typed its id.
            assert!(m.agent.launch.is_some(), "{id}: no launch command");
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

#[cfg(test)]
mod trailer_layout_tests {
    use super::*;

    fn parse(body: &str) -> Result<Manifest, ManifestError> {
        let src = format!("[agent]\nid = \"t\"\ndisplay_name = \"t\"\n\n[injection]\n{body}");
        Manifest::parse(&src, Path::new("t.toml"))
    }

    /// A turn key is a claim that two events can be matched, so an
    /// incomplete or ambiguous declaration is a load error rather than a
    /// vendor that quietly falls back to screen evidence while its
    /// manifest says it correlates turns.
    #[test]
    fn a_partial_turn_key_is_a_load_error() {
        let hooks = |body: &str| {
            let src = format!(
                "[agent]\nid = \"t\"\ndisplay_name = \"t\"\n\n[hooks]\n\
                 turn_start_evidence = \"confirmed\"\n\
                 turn_end_evidence = \"confirmed\"\n{body}"
            );
            Manifest::parse(&src, Path::new("t.toml"))
        };
        let both = "turn_start = \"Start\"\nturn_end = \"Stop\"\n";

        assert!(
            hooks(&format!(
                "{both}turn_key_fields = [\"session_id\", \"turn_id\"]\n"
            ))
            .is_ok(),
            "a complete declaration loads"
        );
        assert!(hooks(both).is_ok(), "declaring no key at all is fine");
        // Start and acknowledgment MAY be the same event, and are in
        // every shipped vendor: taking the prompt is both the
        // acknowledgment and the beginning of the turn.
        assert!(
            hooks(&format!(
                "{both}ack = \"Start\"\nturn_key_fields = [\"turn_id\"]\n"
            ))
            .is_ok(),
            "a start that is also the acknowledgment is a fact about the vendor"
        );

        for (case, body) in [
            (
                "no start",
                "turn_end = \"Stop\"\nturn_key_fields = [\"turn_id\"]\n".to_string(),
            ),
            (
                "no end",
                "turn_start = \"Start\"\nturn_key_fields = [\"turn_id\"]\n".to_string(),
            ),
            (
                "blank start",
                "turn_start = \"  \"\nturn_end = \"Stop\"\nturn_key_fields = [\"turn_id\"]\n"
                    .to_string(),
            ),
            (
                "blank end",
                "turn_start = \"Start\"\nturn_end = \"\"\nturn_key_fields = [\"turn_id\"]\n"
                    .to_string(),
            ),
            (
                "empty field",
                format!("{both}turn_key_fields = [\"turn_id\", \"\"]\n"),
            ),
            (
                "duplicate field",
                format!("{both}turn_key_fields = [\"turn_id\", \"turn_id\"]\n"),
            ),
        ] {
            assert!(
                matches!(hooks(&body), Err(ManifestError::BadTurnKey { .. })),
                "{case} loaded"
            );
        }
    }

    /// One event cannot hold two turn roles, keyed lane or not.
    ///
    /// The bug this pins: the parser required both role names to be
    /// present but never that they DIFFER. The runtime maps a report to
    /// start, end or acknowledgment by reduced name, so a manifest whose
    /// start and end reduce to the same event makes one report answer
    /// both questions: a start records its own end, the turn is over
    /// before it began, and the composer barrier releases against a turn
    /// nothing ran. An acknowledgment that reduces to the end name is the
    /// same defect on the receipt path, where an end would verify a
    /// delivery and start the turn it had just ended.
    #[test]
    fn one_event_cannot_hold_two_turn_roles() {
        let head = "[agent]\nid = \"t\"\ndisplay_name = \"t\"\n\n[hooks]\n";
        let load = |body: &str| {
            Manifest::parse(
                &format!(
                    "{head}turn_start_evidence = \"confirmed\"\n\
                     turn_end_evidence = \"confirmed\"\n{body}"
                ),
                Path::new("t.toml"),
            )
        };

        for (case, body) in [
            (
                "start and end are one event",
                "turn_start = \"Stop\"\nturn_end = \"Stop\"\n",
            ),
            (
                "differing only by punctuation and case",
                "turn_start = \"turn-end\"\nturn_end = \"Turn_End\"\n",
            ),
            (
                "acknowledgment is the end event",
                "turn_start = \"Start\"\nturn_end = \"Stop\"\nack = \"stop\"\n",
            ),
            // Each pair stands alone: a manifest with no start still runs
            // both of the roles it does declare.
            (
                "acknowledgment is the end event, with no start declared",
                "turn_end = \"Stop\"\nack = \"Stop\"\n",
            ),
            // A role that reduces to nothing names no event, and two of
            // them cannot be told apart from each other or from an
            // incoming event that also reduces to nothing.
            (
                "punctuation-only role names",
                "turn_start = \"!!!\"\nturn_end = \"???\"\n",
            ),
            (
                "non-ASCII role name",
                "turn_start = \"Start\"\nturn_end = \"\u{7d42}\u{4e86}\"\n",
            ),
            (
                "punctuation-only acknowledgment",
                "turn_start = \"Start\"\nturn_end = \"Stop\"\nack = \"--\"\n",
            ),
        ] {
            assert!(
                matches!(load(body), Err(ManifestError::BadHooks { .. })),
                "{case} loaded"
            );
        }

        // Checked for every declared lifecycle, not only for vendors that
        // can name their turns: the screen lane maps the same roles.
        for keyed in [
            "turn_start = \"Stop\"\nturn_end = \"Stop\"\nturn_key_fields = [\"t\"]\n",
            "turn_start = \"!!!\"\nturn_end = \"Stop\"\nturn_key_fields = [\"t\"]\n",
        ] {
            assert!(
                matches!(load(keyed), Err(ManifestError::BadHooks { .. })),
                "a keyed manifest is not a special case: {keyed:?}"
            );
        }

        // A start that is ALSO the acknowledgment is a fact about the
        // vendor, not a collision: taking the prompt is both the
        // acknowledgment and the beginning of the turn, which is the
        // shipped shape.
        assert!(load("turn_start = \"Start\"\nturn_end = \"Stop\"\nack = \"Start\"\n").is_ok());
        // And a lifecycle nobody declared has no roles to collide.
        assert!(load("config_mechanism = \"none\"\n").is_ok());
    }

    /// Order is part of the declaration, because the values are compared
    /// positionally: two manifests naming the same fields in different
    /// orders describe different keys.
    #[test]
    fn turn_key_fields_keep_their_declared_order() {
        let src = "[agent]\nid = \"t\"\ndisplay_name = \"t\"\n\n[hooks]\n\
                   turn_start = \"Start\"\nturn_start_evidence = \"confirmed\"\n\
                   turn_end = \"Stop\"\nturn_end_evidence = \"confirmed\"\n\
                   turn_key_fields = [\"b\", \"a\"]\n";
        let m = Manifest::parse(src, Path::new("t.toml")).expect("loads");
        assert_eq!(m.hooks.turn_key_fields, vec!["b", "a"]);
    }

    #[test]
    fn unkeyed_candidate_start_requires_an_event_local_dispatch_contract() {
        let head = "[agent]\nid = \"t\"\ndisplay_name = \"t\"\n\n[hooks]\n";
        let load = |body: &str| Manifest::parse(&format!("{head}{body}"), Path::new("t.toml"));
        let complete = "turn_start = \"UserPromptSubmit\"\n\
                        turn_start_evidence = \"candidate\"\n\
                        ack = \"user_prompt_submit\"\n\
                        ack_evidence = \"dispatch\"\n\
                        ack_payload_field = \"prompt\"\n";
        let manifest = load(complete).expect("unkeyed event-local dispatch start loads");
        assert_eq!(
            manifest.hooks.lifecycle_event("UserPromptSubmit"),
            Some((LifecycleRole::Start, LifecycleCertainty::Candidate))
        );
        assert_eq!(manifest.hooks.ack_evidence, AckEvidence::Dispatch);
        assert!(!manifest.hooks.has_lifecycle_role(LifecycleRole::End));
        assert!(manifest.hooks.turn_key_fields.is_empty());

        for (case, body) in [
            (
                "legacy lifecycle defaults",
                "turn_start = \"Start\"\nturn_end = \"Stop\"\n",
            ),
            (
                "legacy acknowledgment defaults",
                "turn_start = \"Start\"\nack = \"Start\"\nack_payload_field = \"prompt\"\n",
            ),
            (
                "missing payload field",
                "turn_start = \"Start\"\nturn_start_evidence = \"candidate\"\n\
                 ack = \"Start\"\nack_evidence = \"dispatch\"\n",
            ),
            (
                "different acknowledgment event",
                "turn_start = \"Start\"\nturn_start_evidence = \"candidate\"\n\
                 ack = \"Accepted\"\nack_evidence = \"dispatch\"\n\
                 ack_payload_field = \"prompt\"\n",
            ),
            (
                "declared lifecycle end",
                "turn_start = \"Start\"\nturn_start_evidence = \"candidate\"\n\
                 turn_end = \"Stop\"\nturn_end_evidence = \"confirmed\"\n\
                 ack = \"Start\"\nack_evidence = \"dispatch\"\n\
                 ack_payload_field = \"prompt\"\n",
            ),
            (
                "declared confirmed lifecycle end",
                "turn_start = \"Start\"\nturn_start_evidence = \"candidate\"\n\
                 turn_end_confirmed = [\"StopFailure\"]\n\
                 ack = \"Start\"\nack_evidence = \"dispatch\"\n\
                 ack_payload_field = \"prompt\"\n",
            ),
            (
                "end settle policy without an end",
                "turn_start = \"Start\"\nturn_start_evidence = \"candidate\"\n\
                 turn_end_settle_ms = 3000\n\
                 ack = \"Start\"\nack_evidence = \"dispatch\"\n\
                 ack_payload_field = \"prompt\"\n",
            ),
        ] {
            assert!(load(body).is_err(), "{case} loaded");
        }
    }

    #[test]
    fn candidate_end_still_requires_a_turn_key_and_settle_window() {
        let head = "[agent]\nid = \"t\"\ndisplay_name = \"t\"\n\n[hooks]\n";
        let load = |body: &str| Manifest::parse(&format!("{head}{body}"), Path::new("t.toml"));
        let complete = "turn_start = \"Start\"\nturn_start_evidence = \"candidate\"\n\
                        turn_end = \"Stop\"\nturn_end_evidence = \"candidate\"\n\
                        turn_end_settle_ms = 3000\n\
                        ack = \"Start\"\nack_evidence = \"dispatch\"\n\
                        ack_payload_field = \"prompt\"\n\
                        turn_key_fields = [\"session_id\", \"prompt_id\"]\n";
        let manifest = load(complete).expect("keyed candidate lifecycle loads");
        assert_eq!(
            manifest.hooks.lifecycle_event("Stop"),
            Some((LifecycleRole::End, LifecycleCertainty::Candidate))
        );

        let candidate_start_only = "turn_start = \"Start\"\nturn_start_evidence = \"candidate\"\n\
                                    turn_end = \"Stop\"\nturn_end_evidence = \"confirmed\"\n\
                                    ack = \"Start\"\nack_evidence = \"dispatch\"\n\
                                    ack_payload_field = \"prompt\"\n\
                                    turn_key_fields = [\"turn_id\"]\n";
        load(candidate_start_only).expect("a confirmed end needs no candidate settle window");

        let missing_key = "turn_start = \"Start\"\nturn_start_evidence = \"confirmed\"\n\
                           turn_end = \"Stop\"\nturn_end_evidence = \"candidate\"\n\
                           turn_end_settle_ms = 3000\n";
        assert!(matches!(
            load(missing_key),
            Err(ManifestError::BadTurnKey { .. })
        ));

        let missing_settle = "turn_start = \"Start\"\nturn_start_evidence = \"confirmed\"\n\
                              turn_end = \"Stop\"\nturn_end_evidence = \"candidate\"\n\
                              turn_key_fields = [\"turn_id\"]\n";
        assert!(matches!(
            load(missing_settle),
            Err(ManifestError::BadHooks { .. })
        ));

        let confirmed_dispatch = "turn_start = \"Start\"\n\
                                  turn_start_evidence = \"confirmed\"\n\
                                  turn_end = \"Stop\"\n\
                                  turn_end_evidence = \"confirmed\"\n\
                                  ack = \"Start\"\nack_evidence = \"dispatch\"\n\
                                  ack_payload_field = \"prompt\"\n\
                                  turn_key_fields = [\"turn_id\"]\n";
        assert!(matches!(
            load(confirmed_dispatch),
            Err(ManifestError::BadHooks { .. })
        ));
    }

    /// A layout is two descriptions of the same rows plus how many of them
    /// are mandatory. Every way of describing it incompletely is a load
    /// error rather than a lane that silently never verifies.
    #[test]
    fn a_half_described_layout_is_a_load_error() {
        let plain = "composer_trailer_regex = ['^a$', '^b$']\n";
        let esc = "composer_trailer_regex_esc = ['^a$', '^b$']\n";
        let req = "composer_trailer_required_prefix = 2\n";

        assert!(
            parse(&format!("{plain}{esc}{req}")).is_ok(),
            "complete layout"
        );

        for (case, body) in [
            ("missing required prefix", format!("{plain}{esc}")),
            (
                "zero required",
                format!("{plain}{esc}composer_trailer_required_prefix = 0\n"),
            ),
            (
                "required out of range",
                format!("{plain}{esc}composer_trailer_required_prefix = 3\n"),
            ),
            ("plain only", format!("{plain}{req}")),
            ("escaped only", format!("{esc}{req}")),
            (
                "length mismatch",
                format!("{plain}composer_trailer_regex_esc = ['^a$']\n{req}"),
            ),
        ] {
            assert!(parse(&body).is_err(), "{case} must not load");
        }
    }

    #[test]
    fn an_unstyled_trailer_requires_a_structural_composer_boundary() {
        let layout = "composer_trailer_regex = ['^rule$', '^status$']\n\
                      composer_trailer_regex_esc = ['^esc-rule$', '^esc-status$']\n\
                      composer_trailer_required_prefix = 2\n\
                      unstyled_composer_proof = 'structural_trailer'\n";
        assert!(
            parse(layout).is_err(),
            "unstyled layout had no composer boundary"
        );

        let extraction = "composer_prompt_regex = '^> (?P<content>.*)$'\n\
                          composer_continuation_regex = '^  (?P<content>.*)$'\n";
        assert!(parse(&format!("{layout}{extraction}")).is_ok());
    }

    /// A manifest that declares no layout at all still loads: it simply
    /// cannot use the sentinel path.
    #[test]
    fn no_layout_at_all_is_not_an_error() {
        let m = parse("submit = \"Enter\"\n").expect("loads");
        assert!(m.composer_trailers.is_empty());
    }

    #[test]
    fn composer_actions_require_a_complete_safe_declaration() {
        let extraction = "composer_prompt_regex = '^> (?P<content>.*)$'\n\
                          composer_continuation_regex = '^  (?P<content>.*)$'\n";
        let manifest = parse(&format!(
            "submit = \"Enter\"\nclear_keys = [\"C-c\"]\n{extraction}"
        ))
        .expect("measured clear action loads");
        assert_eq!(manifest.injection.clear_keys, ["C-c"]);
        assert!(manifest.composer_prompt.is_some());
        assert!(manifest.composer_continuation.is_some());
        let chord_sequence = parse(&format!(
            "submit = \"Enter\"\nclear_keys = [\"C-a\", \"C-k\"]\n{extraction}"
        ))
        .expect("measured control-key sequence loads");
        assert_eq!(chord_sequence.injection.clear_keys, ["C-a", "C-k"]);

        for (case, body) in [
            (
                "clear without extraction",
                "submit = \"Enter\"\nclear_keys = [\"C-c\"]\n".to_string(),
            ),
            (
                "one extraction half",
                "composer_prompt_regex = '^> (?P<content>.*)$'\n".to_string(),
            ),
            (
                "unanchored extraction",
                "composer_prompt_regex = '> (?P<content>.*)'\n\
                 composer_continuation_regex = '^  (?P<content>.*)$'\n"
                    .to_string(),
            ),
            (
                "missing content capture",
                "composer_prompt_regex = '^> (.*)$'\n\
                 composer_continuation_regex = '^  (?P<content>.*)$'\n"
                    .to_string(),
            ),
            (
                "submit key clears",
                format!("submit = \"Enter\"\nclear_keys = [\"Enter\"]\n{extraction}"),
            ),
            (
                "newline key clears",
                format!("submit = \"Enter\"\nclear_keys = [\"C-m\"]\n{extraction}"),
            ),
            (
                "literal key clears",
                format!("submit = \"Enter\"\nclear_keys = [\"x\"]\n{extraction}"),
            ),
            (
                "named literal key clears",
                format!("submit = \"Enter\"\nclear_keys = [\"Space\"]\n{extraction}"),
            ),
            (
                "named editing key clears",
                format!("submit = \"Enter\"\nclear_keys = [\"BSpace\"]\n{extraction}"),
            ),
            (
                "unknown key expands to text",
                format!("submit = \"Enter\"\nclear_keys = [\"clear\"]\n{extraction}"),
            ),
            (
                "modified submit key clears",
                format!("submit = \"Enter\"\nclear_keys = [\"M-Enter\"]\n{extraction}"),
            ),
        ] {
            assert!(
                matches!(parse(&body), Err(ManifestError::BadInjection { .. })),
                "{case} loaded"
            );
        }
        for key in [
            "Space",
            "BSpace",
            "Backspace",
            "Tab",
            "BTab",
            "DC",
            "Delete",
            "IC",
            "Insert",
            "Enter",
            "Return",
            "KPEnter",
            "Linefeed",
            "C-m",
            "C-j",
            "M-Enter",
        ] {
            let body = format!("submit = \"Enter\"\nclear_keys = [\"{key}\"]\n{extraction}");
            assert!(
                matches!(parse(&body), Err(ManifestError::BadInjection { .. })),
                "unsafe key {key:?} loaded"
            );
        }
    }

    /// `regex_esc` proves an ORDER across styled rows and fails closed
    /// without an escaped capture, like every escaped clause.
    #[test]
    fn regex_esc_orders_styled_rows_and_fails_closed_without_an_escaped_capture() {
        let manifest = Manifest::parse(
            r#"
[agent]
id = "esc"
display_name = "Esc fixture"
process_names = ["esc"]

[[rule]]
id = "done_then_prompt"
state = "idle"
priority = 100
region = "bottom_non_empty_lines(4)"
regex = ['(?m)^DONE\n>\s*\z']
regex_esc = ['(?m)^\x1b\[38;5;246mDONE\x1b\[39m\n\x1b\[39m>\s*\z']
"#,
            Path::new("esc.toml"),
        )
        .unwrap();
        let plain = "DONE\n>";
        let esc = "\u{1b}[38;5;246mDONE\u{1b}[39m\n\u{1b}[39m>";
        assert!(manifest.evaluate_esc("", plain, Some(esc)).is_some());
        assert!(
            manifest.evaluate_esc("", plain, None).is_none(),
            "fails closed"
        );
        let reordered = "\u{1b}[39m>\n\u{1b}[38;5;246mDONE\u{1b}[39m";
        assert!(manifest
            .evaluate_esc("", ">\nDONE", Some(reordered))
            .is_none());
        let unstyled = "DONE\n>";
        assert!(
            manifest.evaluate_esc("", plain, Some(unstyled)).is_none(),
            "plain-looking esc rows"
        );
        // The pair must match the same line span: a styled DONE two rows up
        // with an unstyled DONE directly above the prompt is not the same row.
        let split_plain = "DONE\nx\nDONE\n>";
        let split_esc = "\u{1b}[38;5;246mDONE\u{1b}[39m\nx\nDONE\n\u{1b}[39m>";
        assert!(
            manifest
                .evaluate_esc("", split_plain, Some(split_esc))
                .is_none(),
            "split spans"
        );
        // Pair counts are validated at parse.
        let unpaired = Manifest::parse(
            r#"
[agent]
id = "esc"
display_name = "Esc fixture"
process_names = ["esc"]

[[rule]]
id = "unpaired"
state = "idle"
priority = 100
region = "bottom_non_empty_lines(4)"
regex_esc = ['DONE']
"#,
            Path::new("unpaired.toml"),
        );
        assert!(
            unpaired.is_err(),
            "regex_esc without a paired regex must not parse"
        );
    }
}
