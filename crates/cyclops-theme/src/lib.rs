//! Semantic color tokens for every Cyclops renderer.
//!
//! GOALS.md: themes are semantic token files, never raw colors in code, and
//! exactly two encodings carry meaning, role color and the state glyph.
//! Redundant is the whole point. A state's color repeats what its glyph and
//! its word already said, so `NO_COLOR` and `--plain` lose nothing; role hue
//! stays on the agent name and state color on the state cell, so the two
//! encodings never share a cell and no single color has to mean two things.
//!
//! This crate owns the token vocabulary, the four groups that state and
//! delivery color speak in ([`state_token`], [`delivery_token`]), the theme
//! file format, the 256-color fallback derivation, selection (config key
//! plus `CYCLOPS_THEME`), and event-driven hot reload. The CLI's style
//! module and cyclops-ui both resolve through it: neither carries a palette
//! of its own, and neither decides its own groups.
//!
//! A theme file is data-only TOML (v1 keeper: config is values, never code).
//! Unknown tokens warn, missing tokens fall back to the compiled default
//! table, so an old binary renders a newer theme and a bare `CYCLOPS_HOME`
//! renders with no theme file at all.
//!
//! The vocabulary is exactly what the renderers paint. A token no renderer
//! resolves would load in silence and change nothing, which is the same
//! lie as a stale doc, so it does not get to live here (see [`tokens`]).

mod select;

pub use select::{active, active_with, path_for, themes_dir, Selection, ThemeWatch, THEME_ENV};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cyclops_proto::{AgentState, DeliveryState};

/// The token vocabulary. Renderers name colors through these constants;
/// a string literal token in rendering code is a bug.
///
/// Every token here reaches a screen, either because a renderer names it
/// or because one of the group mappings below returns it, so editing it
/// changes what a user sees. The one exception is [`SURFACE_FG`], which the
/// engine resolves when a renderer asks for a token outside the vocabulary;
/// no renderer paints it directly. A token nothing paints does not belong
/// in this list: it would load in silence and do nothing.
///
/// Deliberately absent: `stream.*`, because the stream's gutter resolves
/// `surface.dim` like every other detail column and its subjects and
/// bodies print in the terminal's own foreground, so a second set of
/// tokens would only let the two surfaces drift; `surface.bg`, because
/// nothing paints a ground (one-shot commands print onto the terminal's
/// own background and the alternate screen inherits it).
pub mod tokens {
    /// Stable role palette slots. [`crate::role_slot`] hashes an agent
    /// label into this array, so its length and order are part of visual
    /// stability: reordering re-colors every agent.
    pub const ROLE: [&str; 8] = [
        "role.1", "role.2", "role.3", "role.4", "role.5", "role.6", "role.7", "role.8",
    ];

    /// The engine's fallback for an out-of-vocabulary token. Not painted
    /// by any renderer; see the module doc.
    pub const SURFACE_FG: &str = "surface.fg";
    /// Detail columns, gutters, separators.
    pub const SURFACE_DIM: &str = "surface.dim";
    /// The accent: the marker on the stream's selected entry, and nothing
    /// else. Not the eye, which has its own two tokens below.
    pub const SURFACE_ACCENT: &str = "surface.accent";

    /// The eye, closed and calm.
    pub const EYE_CALM: &str = "eye.calm";
    /// The eye, open on something that needs the human.
    ///
    /// Every surface that wears the eye paints it with these two, so a
    /// theme that moves them moves the mark everywhere it appears. All
    /// three shipped themes give this token SURFACE_ACCENT's exact value,
    /// which is how a surface painting the mark through the accent looked
    /// correct while wearing the alarm color on a calm rig.
    pub const EYE_ALERT: &str = "eye.alert";

    /// The agent-state groups. Four, not one hue per state: the reader is
    /// told which rows are theirs and which will never clear themselves,
    /// which is what a glyph makes them work for. `crate::state_token`
    /// owns the mapping; `dead` sits below quiet because a gone pane is
    /// quieter than an idle one.
    pub const STATE_HEALTHY: &str = "state.healthy";
    pub const STATE_NEEDS_YOU: &str = "state.needs_you";
    pub const STATE_TERMINAL: &str = "state.terminal";
    pub const STATE_QUIET: &str = "state.quiet";
    pub const STATE_DEAD: &str = "state.dead";

    /// The delivery-badge groups, the same four read on a delivery instead
    /// of a pane (`crate::delivery_token`). Separate tokens because a
    /// theme may want the badge column to sit back from the state column;
    /// the shipped themes give each pair the same value.
    pub const BADGE_HEALTHY: &str = "badge.healthy";
    pub const BADGE_NEEDS_YOU: &str = "badge.needs_you";
    pub const BADGE_TERMINAL: &str = "badge.terminal";
    pub const BADGE_QUIET: &str = "badge.quiet";

    /// Every token, the whole vocabulary. Loaders validate against this.
    pub const ALL: [&str; 22] = [
        "role.1",
        "role.2",
        "role.3",
        "role.4",
        "role.5",
        "role.6",
        "role.7",
        "role.8",
        SURFACE_FG,
        SURFACE_DIM,
        SURFACE_ACCENT,
        EYE_CALM,
        EYE_ALERT,
        STATE_HEALTHY,
        STATE_NEEDS_YOU,
        STATE_TERMINAL,
        STATE_QUIET,
        STATE_DEAD,
        BADGE_HEALTHY,
        BADGE_NEEDS_YOU,
        BADGE_TERMINAL,
        BADGE_QUIET,
    ];
}

/// Which group a pane's state is colored in.
///
/// The color is redundant with the glyph and the word by construction: it
/// adds no state a `--plain` reader cannot read. What it saves a sighted
/// reader is decoding the glyph vocabulary to answer two questions. Is
/// this row mine to clear, and will it clear itself?
///
/// One mapping, here, because two surfaces paint states (the CLI grid and
/// the stream) and a second copy is how the eye drifted into two eyes.
pub fn state_token(state: AgentState) -> &'static str {
    match state {
        AgentState::Working => tokens::STATE_HEALTHY,
        // A prompt or a vendor modal: nothing downstream clears either.
        AgentState::BlockedModal | AgentState::BlockedPermission => tokens::STATE_NEEDS_YOU,
        // Quota never auto-retries (GOALS, finding F11), so it is not just
        // waiting on a human, it is waiting on a reset that no amount of
        // attention brings forward.
        AgentState::BlockedQuota => tokens::STATE_TERMINAL,
        AgentState::Idle | AgentState::IdleWithInput | AgentState::Unknown => tokens::STATE_QUIET,
        AgentState::Dead => tokens::STATE_DEAD,
    }
}

/// Which group a delivery's badge is colored in: the same four groups read
/// on a delivery. Every state the pipeline can leave on its own is quiet,
/// whatever step it is on, because a reader has nothing to do about any of
/// them.
pub fn delivery_token(state: DeliveryState) -> &'static str {
    match state {
        DeliveryState::DeliveredVerified | DeliveryState::DeliveredUnverified => {
            tokens::BADGE_HEALTHY
        }
        DeliveryState::AttentionRequired => tokens::BADGE_NEEDS_YOU,
        DeliveryState::ParkedBlockedQuota => tokens::BADGE_TERMINAL,
        DeliveryState::Queued
        | DeliveryState::Gating
        | DeliveryState::Pasting
        | DeliveryState::Staged
        | DeliveryState::Submitted
        | DeliveryState::RetryQueued => tokens::BADGE_QUIET,
    }
}

/// One resolved color: the truecolor value plus its 256-color fallback.
/// Which one a renderer emits is the terminal's business, not the theme's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub rgb: (u8, u8, u8),
    pub c256: u8,
}

impl Color {
    /// Color from "#rrggbb" with the fallback derived. None on any other
    /// shape; themes carry explicit fallbacks when the derivation's pick
    /// is not the wanted one.
    pub fn from_hex(hex: &str) -> Option<Color> {
        let rgb = hex_rgb(hex)?;
        Some(Color {
            rgb,
            c256: derive_c256(rgb),
        })
    }
}

/// "#rrggbb" (case-insensitive) to an RGB triple.
fn hex_rgb(s: &str) -> Option<(u8, u8, u8)> {
    let s = s.strip_prefix('#')?;
    if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).ok();
    Some((byte(0)?, byte(2)?, byte(4)?))
}

/// Nearest xterm-256 color by squared RGB distance. Candidates are the
/// 6x6x6 cube (16..=231, channel levels 0/95/135/175/215/255) and the
/// 24-step grayscale ramp (232..=255, values 8+10i). Per-channel ties take
/// the lower level, gray ties the darker gray, and the cube wins an exact
/// cube-versus-gray tie. The base 16 colors are never derived: terminals
/// remap them freely, so a derived fallback there would not be stable.
pub fn derive_c256(rgb: (u8, u8, u8)) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let near = |v: u8| -> usize {
        let mut best = 0usize;
        let mut best_d = u32::MAX;
        for (i, l) in LEVELS.iter().enumerate() {
            let d = (v as i32 - *l as i32).unsigned_abs();
            if d < best_d {
                best = i;
                best_d = d;
            }
        }
        best
    };
    let dist = |a: (u8, u8, u8), b: (u8, u8, u8)| -> u32 {
        let d = |x: u8, y: u8| (x as i32 - y as i32).pow(2) as u32;
        d(a.0, b.0) + d(a.1, b.1) + d(a.2, b.2)
    };
    let (ri, gi, bi) = (near(rgb.0), near(rgb.1), near(rgb.2));
    let cube_d = dist(rgb, (LEVELS[ri], LEVELS[gi], LEVELS[bi]));
    let mut gray_i = 0usize;
    let mut gray_d = u32::MAX;
    for i in 0..24u8 {
        let v = 8 + 10 * i;
        let d = dist(rgb, (v, v, v));
        if d < gray_d {
            gray_i = i as usize;
            gray_d = d;
        }
    }
    if gray_d < cube_d {
        232 + gray_i as u8
    } else {
        (16 + 36 * ri + 6 * gi + bi) as u8
    }
}

/// Stable palette slot for a label: FNV-1a over the label bytes, modulo the
/// role slot count. Not DefaultHasher: the same label must keep its color
/// across runs, platforms, and rust versions.
pub fn role_slot(label: &str) -> usize {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in label.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    (h % tokens::ROLE.len() as u64) as usize
}

/// The compiled default table: every token has a value even with no theme
/// file anywhere. Seeded from the pre-theme CLI palette (style.rs before
/// M3) and extended to cover the full vocabulary. The shipped themes
/// override all of it; this is the safety net, not the identity.
const DEFAULT: [(&str, (u8, u8, u8), u8); 22] = [
    ("role.1", (97, 175, 239), 75),   // blue
    ("role.2", (152, 195, 121), 114), // green
    ("role.3", (198, 120, 221), 176), // violet
    ("role.4", (86, 182, 194), 73),   // teal
    ("role.5", (229, 192, 123), 180), // sand
    ("role.6", (224, 108, 117), 168), // rose
    ("role.7", (73, 199, 156), 79),   // mint
    ("role.8", (231, 138, 90), 173),  // orange
    ("surface.fg", (237, 236, 236), 255),
    ("surface.dim", (128, 128, 128), 245),
    ("surface.accent", (209, 154, 102), 173),
    ("eye.calm", (128, 128, 128), 245),
    ("eye.alert", (209, 154, 102), 173),
    // The four groups in the same palette: green, sand, rose, and the dim
    // this table already uses for detail columns. Quiet IS surface.dim,
    // because a quiet state is a detail; dead sits one step below it.
    ("state.healthy", (152, 195, 121), 114),
    ("state.needs_you", (229, 192, 123), 180),
    ("state.terminal", (224, 108, 117), 168),
    ("state.quiet", (128, 128, 128), 245),
    ("state.dead", (90, 90, 90), 240),
    ("badge.healthy", (152, 195, 121), 114),
    ("badge.needs_you", (229, 192, 123), 180),
    ("badge.terminal", (224, 108, 117), 168),
    ("badge.quiet", (128, 128, 128), 245),
];

/// A loaded theme: named token overrides on top of the compiled defaults.
/// Resolution is total; rendering never has a missing color.
#[derive(Debug, Clone)]
pub struct Theme {
    name: String,
    overrides: BTreeMap<String, Color>,
}

impl Default for Theme {
    /// The compiled default table alone, no file.
    fn default() -> Self {
        Theme {
            name: "built-in".into(),
            overrides: BTreeMap::new(),
        }
    }
}

/// Theme file failures a caller must decide about. Everything softer
/// (unknown tokens, bad values) degrades to a warning instead.
///
/// Both variants are one line and neither says the word "theme": every
/// surface that prints one already prefixes `theme: `, and the path names
/// the file. Saying it again printed `theme: theme /path/...`.
#[derive(Debug, thiserror::Error)]
pub enum ThemeError {
    #[error("can't read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    /// `detail` is [`parse_detail`]'s one-line summary, not toml's own
    /// Display: the caller quotes this inside a sentence.
    #[error("{} isn't valid TOML ({detail})", path.display())]
    Parse { path: PathBuf, detail: String },
}

/// A TOML parse failure on one line: where it is, and what the parser
/// wanted there.
///
/// `toml::de::Error`'s Display is a five-line diagnostic block with a
/// caret diagram under the offending line. Every caller here quotes the
/// error inside a sentence, so that block split the sentence around
/// itself and left the tail alone on a line of its own. The position and
/// the message are the whole of what a reader can act on.
fn parse_detail(text: &str, e: &toml::de::Error) -> String {
    // toml states one failure per line ("invalid table header", then
    // "expected `.`, `]`"); joined, they stay one line and lose nothing.
    let message = e.message().lines().collect::<Vec<_>>().join("; ");
    match e.span() {
        Some(span) => {
            let (line, column) = line_col(text, span.start);
            format!("line {line}, column {column}: {message}")
        }
        // No span: a whole-document failure with nowhere to point.
        None => message,
    }
}

/// 1-based line and column of a byte offset in `text`. Columns count
/// characters, not bytes, so a file with multi-byte content does not
/// report a column no editor agrees with. An offset landing inside a
/// character walks back to its start rather than panicking on the slice.
fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let mut end = offset.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let upto = &text[..end];
    let line = upto.bytes().filter(|b| *b == b'\n').count() + 1;
    let column = upto.rsplit('\n').next().unwrap_or_default().chars().count() + 1;
    (line, column)
}

impl Theme {
    /// Load a theme file. Only IO failures and TOML syntax errors are hard;
    /// everything else parses tolerantly and comes back as warnings.
    pub fn load(path: &Path) -> Result<(Theme, Vec<String>), ThemeError> {
        let text = std::fs::read_to_string(path).map_err(|source| ThemeError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "theme".into());
        Theme::parse(&text, &stem).map_err(|e| ThemeError::Parse {
            path: path.to_path_buf(),
            detail: parse_detail(&text, &e),
        })
    }

    /// Parse theme TOML. Token tables (`[role]`, `[surface]`, `[eye]`) hold
    /// values that are either "#rrggbb" (fallback derived) or
    /// `{ hex = "#rrggbb", c256 = 0..255 }` (fallback explicit). Unknown
    /// tokens and unusable values warn and are skipped, so resolution
    /// falls back to the compiled defaults for them.
    pub fn parse(text: &str, name: &str) -> Result<(Theme, Vec<String>), toml::de::Error> {
        let table: toml::Table = toml::from_str(text)?;
        let mut theme = Theme {
            name: name.to_string(),
            overrides: BTreeMap::new(),
        };
        let mut warnings = Vec::new();
        for (group, value) in table {
            match (group.as_str(), value) {
                ("name", toml::Value::String(s)) => theme.name = s,
                ("name", other) => warnings.push(format!(
                    "`name` must be a string, not a {}; keeping \"{}\"",
                    other.type_str(),
                    theme.name
                )),
                (_, toml::Value::Table(entries)) => {
                    for (key, v) in entries {
                        let token = format!("{group}.{key}");
                        if !tokens::ALL.contains(&token.as_str()) {
                            warnings.push(format!("unknown token `{token}` ignored"));
                            continue;
                        }
                        match color_value(&v) {
                            Ok((color, soft)) => {
                                if let Some(w) = soft {
                                    warnings.push(format!("token `{token}`: {w}"));
                                }
                                theme.overrides.insert(token, color);
                            }
                            Err(w) => warnings.push(format!("token `{token}`: {w}")),
                        }
                    }
                }
                (g, other) => warnings.push(format!(
                    "`{g}` must be a table of tokens, not a {}; ignored",
                    other.type_str()
                )),
            }
        }
        Ok((theme, warnings))
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// True when the theme file itself sets this token (no fallback needed).
    pub fn defines(&self, token: &str) -> bool {
        self.overrides.contains_key(token)
    }

    /// True when the file sets at least one token some renderer paints.
    ///
    /// False is the degenerate theme: it parsed, so the tolerant loader
    /// took it, and every color it resolves still comes off the compiled
    /// default table. Switching to one repaints nothing anywhere, so
    /// `cyclops theme` keeps it out of the listing and refuses it by name;
    /// a row there is an offer to change the colors.
    ///
    /// [`tokens::SURFACE_FG`] does not count. It is the engine's fallback
    /// for a token name outside the vocabulary and no renderer asks for
    /// it (pinned by `surface_fg_stays_the_engines_own_fallback`), so a
    /// file setting only that one is the same nothing with one line in it.
    pub fn paints_anything(&self) -> bool {
        self.overrides
            .keys()
            .any(|t| t.as_str() != tokens::SURFACE_FG)
    }

    /// Resolve a token to its color. Total: file override first, then the
    /// compiled default table, then `surface.fg` for a token name outside
    /// the vocabulary (a programmer error, kept readable rather than fatal).
    pub fn resolve(&self, token: &str) -> Color {
        if let Some(c) = self.overrides.get(token) {
            return *c;
        }
        for (t, rgb, c256) in DEFAULT {
            if t == token {
                return Color { rgb, c256 };
            }
        }
        self.resolve(tokens::SURFACE_FG)
    }

    /// The stable role color for an agent label.
    pub fn role(&self, label: &str) -> Color {
        self.resolve(tokens::ROLE[role_slot(label)])
    }
}

/// One token value from TOML: the color plus an optional soft warning
/// (explicit fallback unusable, stray keys). Err when no color is usable.
fn color_value(v: &toml::Value) -> Result<(Color, Option<String>), String> {
    const SHAPE: &str = "use \"#rrggbb\" or { hex = \"#rrggbb\", c256 = 0..255 }";
    match v {
        toml::Value::String(s) => match Color::from_hex(s) {
            Some(c) => Ok((c, None)),
            None => Err(format!("can't read \"{s}\" as a color; {SHAPE}")),
        },
        toml::Value::Table(t) => {
            let Some(toml::Value::String(hex)) = t.get("hex") else {
                return Err(format!("`hex` is missing or not a string; {SHAPE}"));
            };
            let Some(mut color) = Color::from_hex(hex) else {
                return Err(format!("can't read \"{hex}\" as a color; {SHAPE}"));
            };
            let mut soft = None;
            match t.get("c256") {
                None => {}
                Some(toml::Value::Integer(n)) if (0..=255).contains(n) => {
                    color.c256 = *n as u8;
                }
                Some(other) => {
                    soft = Some(format!(
                        "`c256` must be an integer 0..255, not {other}; derived {} used",
                        color.c256
                    ));
                }
            }
            if let Some(stray) = t.keys().find(|k| *k != "hex" && *k != "c256") {
                soft = Some(format!("unknown key `{stray}` ignored"));
            }
            Ok((color, soft))
        }
        other => Err(format!("a {} is not a color; {SHAPE}", other.type_str())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vocabulary_and_default_table_agree() {
        assert_eq!(tokens::ALL.len(), DEFAULT.len());
        for token in tokens::ALL {
            assert!(
                DEFAULT.iter().any(|(t, _, _)| *t == token),
                "{token} has no compiled default"
            );
        }
        // No duplicate names on either side.
        for (i, a) in tokens::ALL.iter().enumerate() {
            assert!(!tokens::ALL[i + 1..].contains(a), "duplicate token {a}");
        }
        // The role constants are the first eight vocabulary entries.
        assert_eq!(&tokens::ALL[..8], &tokens::ROLE);
        // Every token resolves to its own default, never through the
        // out-of-vocabulary fallback: no documented token is unresolvable.
        let bare = Theme::default();
        for (token, rgb, c256) in DEFAULT {
            assert_eq!(bare.resolve(token), Color { rgb, c256 }, "{token}");
        }
    }

    /// Every agent state and every delivery state, so a variant added
    /// later cannot quietly skip these tests. Both mappings match without
    /// a wildcard, so a new variant is a compile error there first.
    const EVERY_AGENT_STATE: [AgentState; 8] = [
        AgentState::Unknown,
        AgentState::Idle,
        AgentState::IdleWithInput,
        AgentState::Working,
        AgentState::BlockedModal,
        AgentState::BlockedPermission,
        AgentState::BlockedQuota,
        AgentState::Dead,
    ];

    const EVERY_DELIVERY_STATE: [DeliveryState; 10] = [
        DeliveryState::Queued,
        DeliveryState::Gating,
        DeliveryState::Pasting,
        DeliveryState::Staged,
        DeliveryState::Submitted,
        DeliveryState::DeliveredVerified,
        DeliveryState::DeliveredUnverified,
        DeliveryState::RetryQueued,
        DeliveryState::AttentionRequired,
        DeliveryState::ParkedBlockedQuota,
    ];

    /// The reverse of the M3 audit's test, which asserted `state.*` and
    /// `badge.*` were absent because states are never colored. They are
    /// colored, redundantly: every group a renderer can ask for has to be
    /// IN the vocabulary and resolve to a value of its own. One that is
    /// not resolves through the out-of-vocabulary fallback instead, which
    /// paints a blocked pane in body-text color and warns nobody.
    #[test]
    fn every_group_a_renderer_can_ask_for_resolves_to_a_value_of_its_own() {
        let bare = Theme::default();
        let asked: Vec<&str> = EVERY_AGENT_STATE
            .iter()
            .map(|s| state_token(*s))
            .chain(EVERY_DELIVERY_STATE.iter().map(|s| delivery_token(*s)))
            .collect();
        for token in &asked {
            assert!(
                tokens::ALL.contains(token),
                "{token} is asked for but is not in the vocabulary"
            );
            assert!(
                DEFAULT.iter().any(|(t, _, _)| t == token),
                "{token} has no compiled default, so it paints as surface.fg"
            );
            assert_ne!(
                bare.resolve(token),
                bare.resolve(tokens::SURFACE_FG),
                "{token} resolves to the out-of-vocabulary fallback"
            );
        }
        // And nothing rides along: a group token no state maps to is the
        // same dead weight the audit was right to remove.
        for token in tokens::ALL
            .iter()
            .filter(|t| t.starts_with("state.") || t.starts_with("badge."))
        {
            assert!(
                asked.contains(token),
                "{token} is in the vocabulary but no state maps to it"
            );
        }
    }

    /// Color and the attention register have to agree about whose row it
    /// is. The groups are finer than the rule (needs-you and terminal are
    /// both a human's; only terminal never retries itself), but they may
    /// not contradict it: an amber row the eye does not count, or a green
    /// one it does, is the same fact told two ways.
    #[test]
    fn the_groups_never_contradict_the_attention_rule() {
        for s in EVERY_AGENT_STATE {
            let token = state_token(s);
            let human = token == tokens::STATE_NEEDS_YOU || token == tokens::STATE_TERMINAL;
            assert_eq!(human, s.is_blocked(), "{s} is colored {token}");
        }
        for s in EVERY_DELIVERY_STATE {
            let token = delivery_token(s);
            let human = token == tokens::BADGE_NEEDS_YOU || token == tokens::BADGE_TERMINAL;
            assert_eq!(
                human,
                cyclops_proto::delivery_needs_human(s),
                "{s:?} is colored {token}"
            );
        }
        // Only quota is terminal on either half: it is the one state
        // nothing downstream retries (GOALS, F11).
        assert_eq!(
            state_token(AgentState::BlockedQuota),
            tokens::STATE_TERMINAL
        );
        assert_eq!(
            delivery_token(DeliveryState::ParkedBlockedQuota),
            tokens::BADGE_TERMINAL
        );
    }

    /// The groups that stayed out. The vocabulary is still exactly what
    /// reaches a screen, so a theme naming a second set of stream colors
    /// or a background nothing paints must be told, not ignored.
    #[test]
    fn tokens_nothing_paints_still_warn() {
        let dead = [
            "stream.gutter",
            "stream.subject",
            "stream.body",
            "surface.bg",
            // The per-state names the pre-M3 vocabulary carried. Groups
            // replaced them; a theme file that still names one is stale
            // and gets told so rather than silently doing nothing.
            "state.idle",
            "state.working",
            "state.blocked",
            "state.quota",
            "state.unknown",
            "badge.verified",
            "badge.unverified",
            "badge.queued",
            "badge.parked",
            "badge.attention",
        ];
        for token in dead {
            assert!(
                !tokens::ALL.contains(&token),
                "{token} is in the vocabulary but nothing paints it"
            );
            let (group, key) = token.split_once('.').expect("group.key");
            let text = format!("[{group}]\n{key} = \"#112233\"\n");
            let (_, warnings) = Theme::parse(&text, "t").expect("parse");
            assert!(
                warnings.iter().any(|w| w.contains(token)),
                "{token} loaded without a warning: {warnings:?}"
            );
        }
    }

    #[test]
    fn role_slot_is_stable_and_in_range() {
        assert_eq!(role_slot("reviewer"), role_slot("reviewer"));
        assert!(role_slot("reviewer") < tokens::ROLE.len());
        assert!(role_slot("implementer") < tokens::ROLE.len());
    }

    #[test]
    fn derivation_matches_the_documented_algorithm() {
        // Cube corners and exact cube points map to themselves.
        assert_eq!(derive_c256((0, 0, 0)), 16);
        assert_eq!(derive_c256((255, 255, 255)), 231);
        assert_eq!(derive_c256((95, 175, 255)), 75);
        assert_eq!(derive_c256((215, 135, 95)), 173);
        // Exact ramp grays land on the ramp, not the cube.
        assert_eq!(derive_c256((8, 8, 8)), 232);
        assert_eq!(derive_c256((128, 128, 128)), 244);
        assert_eq!(derive_c256((238, 238, 238)), 255);
        // A gray the cube holds exactly stays in the cube (cube wins ties
        // and exact hits): 135,135,135 is cube index 102, ramp-off by 3.
        assert_eq!(derive_c256((135, 135, 135)), 102);
        // Near-black favors the ramp: the cube's next stop is 95.
        assert_eq!(derive_c256((13, 13, 13)), 232);
        // A chromatic mid tone picks the cube even when a gray is close.
        assert_eq!(derive_c256((183, 195, 150)), 144);
    }

    #[test]
    fn from_hex_parses_and_derives() {
        assert_eq!(
            Color::from_hex("#61AFEF"),
            Some(Color {
                rgb: (97, 175, 239),
                c256: 75
            })
        );
        for bad in ["61afef", "#61afe", "#61afefa", "#61afzg", "", "#"] {
            assert_eq!(Color::from_hex(bad), None, "{bad:?} should not parse");
        }
    }

    #[test]
    fn resolve_prefers_override_then_default_then_fg() {
        let (theme, warnings) = Theme::parse("[surface]\ndim = \"#123456\"\n", "t").expect("parse");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(theme.resolve(tokens::SURFACE_DIM).rgb, (0x12, 0x34, 0x56));
        // Missing token: compiled default.
        assert_eq!(theme.resolve(tokens::SURFACE_ACCENT).rgb, (209, 154, 102));
        assert_eq!(theme.resolve(tokens::SURFACE_ACCENT).c256, 173);
        // Out-of-vocabulary token: readable fg, not a panic.
        assert_eq!(
            theme.resolve("no.such.token"),
            theme.resolve(tokens::SURFACE_FG)
        );
    }

    #[test]
    fn explicit_fallback_wins_derived_fills_in() {
        let text = "[role]\n1 = { hex = \"#b7c396\", c256 = 17 }\n2 = \"#b7c396\"\n";
        let (theme, warnings) = Theme::parse(text, "t").expect("parse");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(theme.resolve("role.1").c256, 17);
        assert_eq!(theme.resolve("role.2").c256, derive_c256((183, 195, 150)));
        assert_eq!(theme.resolve("role.1").rgb, theme.resolve("role.2").rgb);
    }

    #[test]
    fn unknown_tokens_and_bad_values_warn_and_fall_back() {
        let text = concat!(
            // A leftover `stream` key from the pre-audit vocabulary, as a
            // scalar: not a table of tokens, so it warns and is ignored.
            "stream = 4\n",
            "[role]\n9 = \"#112233\"\n",
            "[surface]\ndim = \"oops\"\naccent = { c256 = 4 }\n",
            "[eye]\ncalm = { hex = \"#101010\", c256 = 999 }\n",
            "[nonsense]\nx = \"#112233\"\n",
        );
        let (theme, warnings) = Theme::parse(text, "t").expect("parse");
        // Six: stray top-level key, unknown token, bad hex, missing hex,
        // out-of-range explicit fallback (soft), unknown table.
        assert_eq!(warnings.len(), 6, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("`role.9`")));
        assert!(warnings.iter().any(|w| w.contains("`surface.dim`")));
        assert!(warnings.iter().any(|w| w.contains("`surface.accent`")));
        assert!(warnings.iter().any(|w| w.contains("`nonsense.x`")));
        assert!(warnings.iter().any(|w| w.contains("`stream`")));
        // Every failed value resolves through the compiled defaults.
        assert_eq!(theme.resolve(tokens::SURFACE_DIM).rgb, (128, 128, 128));
        assert_eq!(theme.resolve(tokens::SURFACE_ACCENT).rgb, (209, 154, 102));
        // The out-of-range explicit fallback fell back to the derived one.
        assert_eq!(theme.resolve(tokens::EYE_CALM).rgb, (16, 16, 16));
        assert_eq!(
            theme.resolve(tokens::EYE_CALM).c256,
            derive_c256((16, 16, 16))
        );
        assert!(theme.defines(tokens::EYE_CALM));
        assert!(!theme.defines(tokens::SURFACE_DIM));
    }

    #[test]
    fn name_comes_from_the_file_else_the_stem() {
        let (theme, _) = Theme::parse("name = \"midnight\"\n", "file-stem").expect("parse");
        assert_eq!(theme.name(), "midnight");
        let (theme, _) = Theme::parse("", "file-stem").expect("parse");
        assert_eq!(theme.name(), "file-stem");
        assert_eq!(Theme::default().name(), "built-in");
    }

    #[test]
    fn syntax_error_is_hard() {
        assert!(Theme::parse("[role\n", "t").is_err());
    }

    #[test]
    fn role_uses_the_hashed_slot() {
        let (theme, _) = Theme::parse("[role]\n1 = \"#010203\"\n", "t").expect("parse");
        let slot = role_slot("reviewer");
        assert_eq!(theme.role("reviewer"), theme.resolve(tokens::ROLE[slot]));
    }
}
