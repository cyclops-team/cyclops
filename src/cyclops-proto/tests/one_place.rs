//! A tripwire for the common shapes of a copied attention rule.
//!
//! "What needs a human right now" has one home,
//! `src/cyclops-proto/src/attention.rs`. Its predicates are
//! `delivery_needs_human` and `AgentState::is_blocked`, plus
//! `gate_cause_needs_human` for the gate's word for a state. Four review
//! rounds fixed the rule at the site a reviewer named and left another
//! copy in the next file. Behavior tests could not catch that: every copy
//! was green on its own and each surface was self-consistent, and only two
//! surfaces read side by side disagreed.
//!
//! ## Read this before trusting a green run
//!
//! This file is a text scan over `src/**/*.rs` and `tests/testrig/**/*.rs`.
//! It catches the common shapes of a second implementation. It is NOT a
//! proof that the rule has one home, and it cannot be one: deciding
//! whether two pieces of code compute the same predicate is semantic
//! equivalence, and no scan over source text decides that.
//!
//! A determined duplicate passes. Somebody who wants a second copy of
//! this rule can write one that nothing here sees, and somebody already
//! has: a verifier defeated the previous version of this file by deriving
//! the verdict without naming a single state, with every gate green. The
//! defence against a second home for this rule is REVIEW. This file
//! catches the copies people write without meaning to, and it is worth
//! exactly that much.
//!
//! Read a green run as "no file matched a shape below". Never as "no
//! second implementation".
//!
//! ## The shapes it catches
//!
//! One question, asked four ways: which states does this file put on the
//! true side of a boolean? When the answer is exactly the states the rule
//! names, the file is reported as a copy. Each of the four is one function
//! below, and each says next to itself what it cannot see:
//!
//! 1. [`bare_runs`]: state names written as one enumeration, separated by
//!    nothing but commas, or-bars and their own path prefix. Covers
//!    `matches!`, a match arm's or-pattern, `if let`, an array or slice or
//!    `vec!` used as a lookup table, and a `HashSet::from([..])`.
//! 2. [`mapped_to_bool`]: match arms whose body is the bare word `true`
//!    or `false`. Covers an exhaustive match over the whole enum, which
//!    names every state and so hides from every set comparison that asks
//!    for "exactly these".
//! 3. [`compared`]: state names next to `==` or `!=`, gathered across the
//!    whole file. Covers a boolean built from comparisons and an
//!    early-return ladder, in any order and split across statements.
//! 4. [`inside_bool_fns`]: every state name in the body of a function
//!    that returns `bool`. The widest of the four, for a verdict built
//!    inside a dedicated predicate: a set filled with `insert`, a table
//!    built in a loop, a chain of helpers inlined into one body.
//!
//! Both spellings go through all four: the enum variant and the wire name,
//! because a copy that re-spells the states as strings is still a copy.
//!
//! Comments are blanked before any of this, so prose ABOUT the rule (this
//! file's, the owner's, and every explanatory comment in between) is never
//! mistaken for a copy of it. Double-quoted strings survive, because the
//! wire spelling of a state is a string.
//!
//! ## The other two checks
//!
//! [`no_file_judges_the_rules_words_with_a_string_search`] catches a
//! stringly-typed test on the words a human-owned state's name begins
//! with. That exact shape shipped in the stream's calm view and produced a
//! calm eye directly above a line saying a human was needed.
//!
//! [`every_crate_that_counts_from_status_asks_for_the_delivery_half`]
//! catches a surface that builds the register from a `status` answer
//! without asking for the backlog, which is how `cyclops status` and
//! `cyclops ui` came to contradict each other against one daemon at one
//! instant.
//!
//! ## Keeping the shapes it does catch
//!
//! [`each_shape_the_tripwire_was_built_for_still_trips_it`] runs the scan
//! against second implementations written as source text right here, one
//! per extractor plus several that combine them. Weakening an extractor
//! fails that test rather than going quietly green. It holds this file to
//! its own job. It says nothing about the copies it was never built to
//! see.
//!
//! [`the_tripwire_is_not_a_proof_and_says_where_it_fails`] lists the holes
//! that are KNOWN. That list is not the whole set and no version of it
//! will be.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// The rule, in both spellings
// ---------------------------------------------------------------------------

/// One half of the rule, in one spelling: every state of that half, the
/// ones the rule says a human must clear, and whether the names are
/// written as string literals.
struct Spelling {
    /// Every state of the half. A verdict that names one of these outside
    /// `human` is a transition table or a badge table, not the rule.
    all: &'static [&'static str],
    /// What the rule says a human must clear. A verdict whose set is
    /// exactly this is the rule, rewritten.
    human: &'static [&'static str],
    /// Wire names are string literals, so a quote is punctuation between
    /// two of them rather than a break.
    quoted: bool,
}

impl Spelling {
    fn wanted(&self) -> BTreeSet<&'static str> {
        self.human.iter().copied().collect()
    }
}

const DELIVERY: Spelling = Spelling {
    all: &[
        "Queued",
        "Gating",
        "Pasting",
        "Staged",
        "Submitted",
        "DeliveredVerified",
        "DeliveredUnverified",
        "RetryQueued",
        "AttentionRequired",
        "ParkedBlockedQuota",
    ],
    human: &["AttentionRequired", "ParkedBlockedQuota"],
    quoted: false,
};

const DELIVERY_WIRE: Spelling = Spelling {
    all: &[
        "queued",
        "gating",
        "pasting",
        "staged",
        "submitted",
        "delivered_verified",
        "delivered_unverified",
        "retry_queued",
        "attention_required",
        "parked_blocked_quota",
    ],
    human: &["attention_required", "parked_blocked_quota"],
    quoted: true,
};

const AGENT: Spelling = Spelling {
    all: &[
        "Unknown",
        "Idle",
        "IdleWithInput",
        "Working",
        "BlockedModal",
        "BlockedPermission",
        "BlockedQuota",
        "Dead",
    ],
    human: &["BlockedModal", "BlockedPermission", "BlockedQuota"],
    quoted: false,
};

const AGENT_WIRE: Spelling = Spelling {
    all: &[
        "unknown",
        "idle",
        "idle_with_input",
        "working",
        "blocked_modal",
        "blocked_permission",
        "blocked_quota",
        "dead",
    ],
    human: &["blocked_modal", "blocked_permission", "blocked_quota"],
    quoted: true,
};

// ---------------------------------------------------------------------------
// The tree
// ---------------------------------------------------------------------------

/// Repo root: this crate sits at `<root>/src/cyclops-proto`.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("src/<crate> has two parents")
        .to_path_buf()
}

/// The one home for the rule. Nothing in it is scanned.
fn owner() -> PathBuf {
    repo_root().join("src/cyclops-proto/src/attention.rs")
}

/// `AgentState::is_blocked` is rule 1's predicate and lives with the enum
/// it classifies, which the owner's own documentation names. It is the one
/// other file allowed to enumerate the blocked states.
fn agent_predicate_home() -> PathBuf {
    repo_root().join("src/cyclops-proto/src/state.rs")
}

/// This file. It quotes the rule in both spellings on purpose.
fn here() -> PathBuf {
    repo_root().join("src/cyclops-proto/tests/one_place.rs")
}

/// Every `.rs` file under `src/` (the Cargo packages) and `tests/testrig`
/// (the one workspace member that lives outside `src/`), skipping build
/// output.
fn rust_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(&repo_root().join("src"), &mut out);
    walk(&repo_root().join("tests/testrig"), &mut out);
    out.retain(|p| p.extension().is_some_and(|e| e == "rs"));
    out.sort();
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if matches!(entry.file_name().to_str(), Some("target" | ".git")) {
                continue;
            }
            walk(&path, out);
        } else {
            out.push(path);
        }
    }
}

// ---------------------------------------------------------------------------
// Reading the source
// ---------------------------------------------------------------------------

/// The source with every comment turned into spaces.
///
/// Prose about the rule is not a second home for it, and the owner's
/// module documentation, this file's, and every explanatory comment in
/// between all name the states on purpose. Double-quoted strings survive,
/// because the wire spelling of a state is a string.
///
/// Cannot see: a `//` inside a string literal on a line that also holds a
/// violation blanks the rest of that line. That can only cause a miss,
/// never a false alarm.
fn code_only(text: &str) -> String {
    let src: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < src.len() {
        let c = src[i];
        let next = src.get(i + 1).copied();
        match c {
            '/' if next == Some('/') => {
                while i < src.len() && src[i] != '\n' {
                    out.push(' ');
                    i += 1;
                }
            }
            '/' if next == Some('*') => {
                let mut depth = 1usize;
                i += 2;
                out.push_str("  ");
                while i < src.len() && depth > 0 {
                    if src[i] == '/' && src.get(i + 1) == Some(&'*') {
                        depth += 1;
                        i += 2;
                        out.push_str("  ");
                    } else if src[i] == '*' && src.get(i + 1) == Some(&'/') {
                        depth -= 1;
                        i += 2;
                        out.push_str("  ");
                    } else {
                        out.push(if src[i] == '\n' { '\n' } else { ' ' });
                        i += 1;
                    }
                }
            }
            // A char literal holding a quote would otherwise open a string
            // that never closes and blind the rest of the file.
            '\'' if next == Some('"') && src.get(i + 2) == Some(&'\'') => {
                out.push_str("   ");
                i += 3;
            }
            '"' => {
                out.push('"');
                i += 1;
                while i < src.len() {
                    let ch = src[i];
                    out.push(ch);
                    i += 1;
                    if ch == '\\' {
                        if let Some(&escaped) = src.get(i) {
                            out.push(escaped);
                            i += 1;
                        }
                    } else if ch == '"' {
                        break;
                    }
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// The same text with the INSIDE of every string literal turned to spaces,
/// byte for byte, so an index into one indexes the other.
///
/// Used wherever structure is being read rather than names: a `{` or a
/// `=>` inside a message is punctuation in a sentence, not Rust.
fn blank_literals(code: &str) -> String {
    let mut out = String::with_capacity(code.len());
    let mut chars = code.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        out.push(c);
        if c != '"' {
            continue;
        }
        // Inside a string: blank every char, keeping byte width, until the
        // closing quote (which survives, so the delimiters still pair up).
        while let Some((_, ch)) = chars.next() {
            if ch == '"' {
                out.push('"');
                break;
            }
            if ch == '\\' {
                out.push_str("  ");
                if chars.next().is_some() {
                    continue;
                }
                break;
            }
            out.push_str(&" ".repeat(ch.len_utf8()));
        }
    }
    out
}

/// Where each of `names` appears in `code` as a whole word, in order.
fn occurrences<'a>(code: &str, names: &[&'a str]) -> Vec<(usize, usize, &'a str)> {
    let bytes = code.as_bytes();
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut found = Vec::new();
    for name in names {
        let mut from = 0;
        while let Some(rel) = code[from..].find(name) {
            let start = from + rel;
            let end = start + name.len();
            let before_ok = start == 0 || !word(bytes[start - 1]);
            let after_ok = end == bytes.len() || !word(bytes[end]);
            if before_ok && after_ok {
                found.push((start, end, *name));
            }
            from = end;
        }
    }
    found.sort();
    found
}

/// The block that starts at the first `{` at or after `from`, as a byte
/// range, brace-matched. None when there is no block (a trait method
/// declaration ends at `;` instead) or the braces never balance.
fn block_after(skeleton: &str, from: usize) -> Option<(usize, usize)> {
    let bytes = skeleton.as_bytes();
    let mut i = from;
    while i < bytes.len() && bytes[i] != b'{' {
        if bytes[i] == b';' {
            return None;
        }
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    let open = i;
    let mut depth = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((open, i + 1));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// The four extractors
// ---------------------------------------------------------------------------

/// Path prefixes removed: `, DeliveryState::` becomes `, `.
///
/// Two names written `DeliveryState::A, DeliveryState::B` are one
/// enumeration, and the qualifier between them is not punctuation.
fn strip_paths(text: &str) -> String {
    let mut out = String::new();
    let mut token = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_alphanumeric() || c == '_' {
            token.push(c);
        } else if c == ':' && chars.peek() == Some(&':') {
            chars.next();
            token.clear();
        } else {
            out.push_str(&token);
            token.clear();
            out.push(c);
        }
    }
    out.push_str(&token);
    out
}

/// Are these two names part of one bare enumeration?
///
/// What may sit between them: whitespace, a comma, an or-bar, the second
/// one's path prefix, and (for the wire spelling) the quotes around them.
/// Anything else, a bracket, a call, a field name, an operator, ends it.
fn same_enumeration(between: &str, quoted: bool) -> bool {
    strip_paths(between)
        .chars()
        .all(|c| c.is_whitespace() || c == ',' || c == '|' || (quoted && c == '"'))
}

/// Maximal runs of state names written as one bare enumeration, with the
/// byte span of each run.
///
/// This is the or-pattern (`matches!`, a match arm, `if let`) and the
/// lookup table (`[A, B]`, `vec![A, B]`, `HashSet::from([A, B])`) at once,
/// because after the punctuation rule above they are the same text.
///
/// Cannot see: two names joined through anything richer than a comma, so
/// a table of tuples or a list of constructor calls is not a run. Those
/// reach [`inside_bool_fns`] when they sit in a predicate, and nothing
/// otherwise.
fn bare_runs(code: &str, spelling: &Spelling) -> Vec<(usize, usize, BTreeSet<&'static str>)> {
    let found = occurrences(code, spelling.all);
    let mut out: Vec<(usize, usize, BTreeSet<&'static str>)> = Vec::new();
    let mut end_of_previous = None;
    for (start, end, name) in found {
        match end_of_previous {
            Some(prev) if same_enumeration(&code[prev..start], spelling.quoted) => {
                let run = out.last_mut().expect("a run is open");
                run.1 = end;
                run.2.insert(name);
            }
            _ => out.push((start, end, BTreeSet::from([name]))),
        }
        end_of_previous = Some(end);
    }
    out
}

/// The states a file maps to `true`, and the ones it maps to `false`,
/// through match arms whose body is that bare word.
///
/// An exhaustive match over the whole enum names every state, so no
/// "exactly these" comparison can see it. What it decides is still only
/// the arms that reach `true`.
///
/// Cannot see: an arm whose body is an expression that evaluates to a
/// bool without being the literal (`s.is_x()`, `!other(s)`, a block).
fn mapped_to_bool(
    code: &str,
    spelling: &Spelling,
) -> (BTreeSet<&'static str>, BTreeSet<&'static str>) {
    let skeleton = blank_literals(code);
    let runs = bare_runs(code, spelling);
    let mut yes = BTreeSet::new();
    let mut no = BTreeSet::new();
    let mut from = 0;
    while let Some(rel) = skeleton[from..].find("=>") {
        let arrow = from + rel;
        from = arrow + 2;
        // The arm body: everything to the next comma or closing brace.
        let rest = &skeleton[from..];
        let stop = rest.find([',', '}', ';']).unwrap_or(rest.len());
        let body = rest[..stop].trim().trim_start_matches("return ").trim();
        let side = match body {
            "true" => &mut yes,
            "false" => &mut no,
            _ => continue,
        };
        // The pattern: the last run of names that reaches the arrow with
        // nothing but bare punctuation in between. Quotes count as bare
        // whatever the spelling, because a wire name's own closing quote
        // sits between the run and the arrow.
        if let Some(run) = runs
            .iter()
            .rev()
            .find(|(_, end, _)| *end <= arrow && same_enumeration(&code[*end..arrow], true))
        {
            side.extend(run.2.iter().copied());
        }
    }
    (yes, no)
}

/// Every state name the file puts next to `==` or `!=`.
///
/// Gathered over the whole file rather than one expression, because an
/// early-return ladder writes one comparison per statement and a boolean
/// chain writes them in whatever order the author thought of them.
///
/// Cannot see: a comparison against a name the file gave the state
/// (`const NEEDY: DeliveryState = DeliveryState::AttentionRequired;` then
/// `s == NEEDY`), and `assert_eq!`, which is an assertion and not a
/// verdict.
fn compared(code: &str, spelling: &Spelling) -> BTreeSet<&'static str> {
    let mut out = BTreeSet::new();
    for (start, end, name) in occurrences(code, spelling.all) {
        let before = strip_paths(code[..start].trim_end_matches(['"', ' ', '\n', '\t', '\r']));
        let before = before.trim_end();
        let after = code[end..].trim_start_matches(['"', ' ', '\n', '\t', '\r']);
        if before.ends_with("==")
            || before.ends_with("!=")
            || after.starts_with("==")
            || after.starts_with("!=")
        {
            out.insert(name);
        }
    }
    out
}

/// Every state name inside the body of each function that returns `bool`.
///
/// The widest of the four: a predicate can build its verdict out of a set
/// filled with `insert`, a table built in a loop, a chain of helpers
/// inlined into one body, and this still reads the states it names. It
/// sees nothing about a predicate that names no state at all.
///
/// Cannot see: a closure (`|s| ...`) with no return annotation, and a
/// predicate split across several functions, each naming part of the rule.
///
/// Two sets per body, because one of them can be widened by accident and
/// the other cannot: everything the body names, and everything it names
/// OUTSIDE any nested block. A predicate that mentions an unrelated state
/// in a loop or a guard would otherwise escape by having named it at all.
fn inside_bool_fns(code: &str, spelling: &Spelling) -> Vec<BTreeSet<&'static str>> {
    let skeleton = blank_literals(code);
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = skeleton[from..].find("-> bool") {
        let at = from + rel;
        from = at + "-> bool".len();
        let Some((open, close)) = block_after(&skeleton, from) else {
            continue;
        };
        let body = &code[open..close];
        let depth = brace_depths(&skeleton[open..close]);
        let mut all = BTreeSet::new();
        let mut own = BTreeSet::new();
        for (start, _, name) in occurrences(body, spelling.all) {
            all.insert(name);
            if depth[start] == 1 {
                own.insert(name);
            }
        }
        out.extend([all, own].into_iter().filter(|s| !s.is_empty()));
    }
    out
}

/// Brace nesting depth at every byte of `skeleton`, counting the block's
/// own outer braces as depth 1.
fn brace_depths(skeleton: &str) -> Vec<usize> {
    let mut out = Vec::with_capacity(skeleton.len());
    let mut depth = 0usize;
    for b in skeleton.bytes() {
        if b == b'{' {
            depth += 1;
        }
        out.push(depth);
        if b == b'}' {
            depth = depth.saturating_sub(1);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The verdict
// ---------------------------------------------------------------------------

/// Does this source match one of the four shapes, with exactly the states
/// the rule names?
///
/// One question asked four ways. `true` means the text reads as a second
/// implementation. `false` means none of the four matched, which is a
/// weaker statement than "this file does not decide the rule".
fn matches_a_copy_shape(code: &str, spelling: &Spelling) -> bool {
    let want = spelling.wanted();
    if bare_runs(code, spelling).iter().any(|(_, _, s)| *s == want) {
        return true;
    }
    let (yes, no) = mapped_to_bool(code, spelling);
    // Either polarity: a rule written inside out is the same rule.
    if yes == want || no == want {
        return true;
    }
    if compared(code, spelling) == want {
        return true;
    }
    inside_bool_fns(code, spelling)
        .into_iter()
        .any(|s| s == want)
}

/// Files matching a copy shape in either spelling of one half.
fn files_matching_a_copy_shape(spellings: &[&Spelling], skip: &[PathBuf]) -> Vec<PathBuf> {
    let mut offenders = Vec::new();
    for path in rust_files() {
        if skip.contains(&path) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let code = code_only(&text);
        if spellings.iter().any(|s| matches_a_copy_shape(&code, s)) {
            offenders.push(path);
        }
    }
    offenders
}

#[test]
fn no_file_writes_the_delivery_half_in_a_shape_this_catches() {
    let offenders = files_matching_a_copy_shape(&[&DELIVERY, &DELIVERY_WIRE], &[owner(), here()]);
    assert!(
        offenders.is_empty(),
        "these files decide for themselves which deliveries need a human \
         instead of calling cyclops_proto::delivery_needs_human, which is \
         how two surfaces came to count the same backlog differently. A \
         fixture that needs the two states should ask the predicate for \
         them: {offenders:#?}"
    );
}

#[test]
fn no_file_writes_the_agent_half_in_a_shape_this_catches() {
    // state.rs is the second allowed home and the only one: the predicate
    // is a property of the enum and the owner names it as rule 1.
    let skip = [owner(), agent_predicate_home(), here()];
    let offenders = files_matching_a_copy_shape(&[&AGENT, &AGENT_WIRE], &skip);
    assert!(
        offenders.is_empty(),
        "these files enumerate the blocked states themselves instead of \
         calling AgentState::is_blocked: {offenders:#?}"
    );
}

/// The argument list of a call that starts at `text`, or None when `text`
/// does not start one. Paren-matched, so a nested call comes along and a
/// later line does not.
fn call_args(text: &str) -> Option<&str> {
    if !text.starts_with('(') {
        return None;
    }
    let mut depth = 0usize;
    for (i, b) in text.bytes().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[1..i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// A string search on the rule's words, in the call shapes below only. A
/// file can still reach the same verdict from text this never reads.
#[test]
fn no_file_judges_the_rules_words_with_a_string_search() {
    let skip = [owner(), here()];
    let mut offenders = Vec::new();
    for path in rust_files() {
        if skip.contains(&path) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let code = code_only(&text);
        // The words a human-owned state's name begins with. Not the bare
        // word "attention", which is header copy ("1 needs attention") and
        // gets asserted on as rendered text all over the tests.
        for verb in [
            "starts_with",
            "ends_with",
            "contains",
            "strip_prefix",
            "strip_suffix",
            "find",
            "split_once",
        ] {
            let mut from = 0;
            while let Some(rel) = code[from..].find(verb) {
                let at = from + rel;
                from = at + verb.len();
                // Only a CALL, and only its own arguments. Bare text that
                // happens to spell a verb is TOML in a fixture string, and
                // scanning past the call's closing paren reached whatever
                // that fixture said several lines down.
                let Some(call) = call_args(&code[from..]) else {
                    continue;
                };
                for word in ["blocked", "parked", "attention_required"] {
                    if call.contains(&format!("\"{word}")) {
                        offenders.push(format!("{}: {verb}(.. \"{word}..", path.display()));
                    }
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "these files match the rule's words as text rather than asking \
         cyclops_proto (gate_cause_needs_human maps a cause back onto the \
         state it names). A prefix match answers yes for any future word \
         that happens to start the same way: {offenders:#?}"
    );
}

// ---------------------------------------------------------------------------
// Half the rule rides `status` only for a caller that asks for it
// ---------------------------------------------------------------------------

/// The source with every `#[cfg(test)]` module turned to spaces.
///
/// A test builds its own `StatusResult` by hand and never asks a daemon
/// for one, so a test that mentions the parameter proves nothing about
/// the code that talks to the daemon. Leaving them in was how this check
/// passed for a crate whose product code asked for nothing.
fn product_code(code: &str) -> String {
    let skeleton = blank_literals(code);
    let mut out = code.to_string();
    let mut from = 0;
    while let Some(rel) = skeleton[from..].find("#[cfg(test)]") {
        let at = from + rel;
        from = at + 1;
        let Some((_, close)) = block_after(&skeleton, at) else {
            continue;
        };
        out.replace_range(at..close, &" ".repeat(close - at));
    }
    out
}

/// Does this source BUILD a register from a status answer? The definition
/// of `from_status` is not a caller of it.
fn counts_from_status(code: &str) -> bool {
    let mut from = 0;
    while let Some(rel) = code[from..].find("from_status(") {
        let at = from + rel;
        from = at + 1;
        if !code[..at].trim_end().ends_with("fn") {
            return true;
        }
    }
    false
}

/// Does this source SET the parameter, in any spelling: the typed struct
/// literal, a hand-written JSON object, or the escaped one inside a byte
/// string? Quotes, backslashes and whitespace come out first, so reading
/// the RESULT field never counts as asking for it.
fn asks_for_open_deliveries(code: &str) -> bool {
    code.chars()
        .filter(|c| !c.is_whitespace() && *c != '"' && *c != '\\')
        .collect::<String>()
        .contains("open_deliveries:true")
}

/// A crate offends when its product code counts and its product code does
/// not ask. Test modules come out here, so no caller can forget to.
fn counts_without_asking(sources: &[String]) -> bool {
    let product: Vec<String> = sources.iter().map(|t| product_code(t)).collect();
    product.iter().any(|t| counts_from_status(t))
        && !product.iter().any(|t| asks_for_open_deliveries(t))
}

#[test]
fn every_crate_that_counts_from_status_asks_for_the_delivery_half() {
    let root = repo_root();
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(root.join("src"))
        .expect("src/ exists")
        .flatten()
    {
        let krate = entry.path();
        if !krate.is_dir() {
            continue;
        }
        let mut sources = Vec::new();
        walk(&krate.join("src"), &mut sources);
        sources.retain(|p| p.extension().is_some_and(|e| e == "rs"));
        let product: Vec<String> = sources
            .iter()
            .filter_map(|p| std::fs::read_to_string(p).ok())
            .map(|t| code_only(&t))
            .collect();
        if counts_without_asking(&product) {
            offenders.push(krate);
        }
    }
    assert!(
        offenders.is_empty(),
        "these crates build the attention register from a status answer \
         but never set StatusParams::open_deliveries, so their eye counts \
         blocked panes and no deliveries: {offenders:#?}"
    );
}

// ---------------------------------------------------------------------------
// Holding the scan to the shapes it was built for
// ---------------------------------------------------------------------------

/// Second implementations of the rule, as source text. Each one is a
/// working copy of a half that never calls the owner.
///
/// These are the shapes this file knows, written down so that weakening an
/// extractor fails loudly instead of going quietly green. They are not the
/// shapes a copy can take; a copy can take any shape at all.
const SECOND_IMPLEMENTATIONS: &[(&str, &Spelling, &str)] = &[
    (
        "or-pattern inside a matches!",
        &DELIVERY,
        r#"
        fn stuck(s: DeliveryState) -> bool {
            matches!(s, DeliveryState::ParkedBlockedQuota | DeliveryState::AttentionRequired)
        }
        "#,
    ),
    (
        "a slice used as a lookup table",
        &DELIVERY,
        r#"
        const OPERATOR_OWNED: [DeliveryState; 2] =
            [DeliveryState::AttentionRequired, DeliveryState::ParkedBlockedQuota];
        "#,
    ),
    (
        "a set built with insert, inside a predicate",
        &DELIVERY,
        r#"
        fn operator_owned(s: DeliveryState) -> bool {
            let mut owned = std::collections::HashSet::new();
            owned.insert(DeliveryState::AttentionRequired);
            owned.insert(DeliveryState::ParkedBlockedQuota);
            owned.contains(&s)
        }
        "#,
    ),
    (
        "an early-return ladder",
        &DELIVERY,
        r#"
        fn wants_a_person(s: DeliveryState) -> bool {
            if s == DeliveryState::ParkedBlockedQuota {
                return true;
            }
            if s == DeliveryState::AttentionRequired {
                return true;
            }
            false
        }
        "#,
    ),
    (
        "an exhaustive match over the whole enum",
        &AGENT,
        r#"
        fn human_owned(s: AgentState) -> bool {
            match s {
                AgentState::Unknown => false,
                AgentState::Idle => false,
                AgentState::IdleWithInput => false,
                AgentState::Working => false,
                AgentState::BlockedModal => true,
                AgentState::BlockedPermission => true,
                AgentState::BlockedQuota => true,
                AgentState::Dead => false,
            }
        }
        "#,
    ),
    (
        "the same match written inside out",
        &AGENT,
        r#"
        fn resolves_itself(s: AgentState) -> bool {
            match s {
                AgentState::BlockedQuota => false,
                AgentState::BlockedPermission => false,
                AgentState::BlockedModal => false,
                _ => true,
            }
        }
        "#,
    ),
    (
        "wire names in a slice, any order",
        &DELIVERY_WIRE,
        r#"
        pub fn wire_is_stuck(name: &str) -> bool {
            ["parked_blocked_quota", "attention_required"]
                .iter()
                .any(|s| *s == name)
        }
        "#,
    ),
    (
        "wire names compared one statement at a time",
        &AGENT_WIRE,
        r#"
        fn needs_a_person(name: &str) -> bool {
            let mut yes = name == "blocked_quota";
            yes |= name == "blocked_modal";
            yes = yes || name == "blocked_permission";
            yes
        }
        "#,
    ),
    (
        "an if let over an or-pattern",
        &AGENT,
        r#"
        fn raise_the_eye(s: AgentState) -> Option<()> {
            if let AgentState::BlockedPermission
                | AgentState::BlockedModal
                | AgentState::BlockedQuota = s
            {
                return Some(());
            }
            None
        }
        "#,
    ),
    (
        "a Vec pushed into, past an unrelated state named in a branch",
        &DELIVERY,
        r#"
        fn parked_or_attention(s: DeliveryState) -> bool {
            let mut owned = Vec::new();
            if s.is_terminal() {
                let _ = DeliveryState::Queued;
            }
            owned.push(DeliveryState::ParkedBlockedQuota);
            owned.push(DeliveryState::AttentionRequired);
            owned.iter().any(|c| *c == s)
        }
        "#,
    ),
];

/// Sources that name the rule's states without deciding the rule. If any
/// of these trips, the scan has started calling honest code a copy.
const NOT_COPIES: &[(&str, &Spelling, &str)] = &[
    (
        "a transition table that happens to include both",
        &DELIVERY,
        r#"
        fn can_transition_to(self, next: DeliveryState) -> bool {
            match self {
                Queued => matches!(next, Gating | AttentionRequired | ParkedBlockedQuota),
                AttentionRequired => matches!(next, Queued),
                ParkedBlockedQuota => matches!(next, Queued),
                _ => false,
            }
        }
        "#,
    ),
    (
        "a fixture naming both in separate calls",
        &DELIVERY,
        r#"
        let open = vec![
            open_delivery("m-1", "implementer", DeliveryState::ParkedBlockedQuota),
            open_delivery("m-2", "reviewer", DeliveryState::AttentionRequired),
        ];
        "#,
    ),
    (
        "a badge table over every state",
        &DELIVERY,
        r#"
        match r.state {
            DeliveryState::Queued => "queued",
            DeliveryState::AttentionRequired => "needs attention",
            DeliveryState::ParkedBlockedQuota => "parked",
            _ => "moving",
        }
        "#,
    ),
    (
        "a resolved-for-a-receipt predicate, which is a different set",
        &DELIVERY,
        r#"
        fn receipt_resolved(s: DeliveryState) -> bool {
            matches!(
                s,
                DeliveryState::DeliveredVerified
                    | DeliveryState::DeliveredUnverified
                    | DeliveryState::AttentionRequired
                    | DeliveryState::ParkedBlockedQuota
            )
        }
        "#,
    ),
    (
        "two of the three blocked states handled differently on purpose",
        &AGENT,
        r#"
        match det.state {
            AgentState::BlockedQuota => park(),
            AgentState::BlockedModal | AgentState::BlockedPermission => hold(),
            _ => go(),
        }
        "#,
    ),
];

#[test]
fn each_shape_the_tripwire_was_built_for_still_trips_it() {
    for (what, spelling, source) in SECOND_IMPLEMENTATIONS {
        assert!(
            matches_a_copy_shape(&code_only(source), spelling),
            "a shape this scan is supposed to catch walked past it: {what}\n{source}"
        );
    }
    for (what, spelling, source) in NOT_COPIES {
        assert!(
            !matches_a_copy_shape(&code_only(source), spelling),
            "the scan called honest code a copy of the rule: {what}\n{source}"
        );
    }
    // And a copy is caught wherever it is written, including inside a
    // comment-heavy file, because comments come out before any of it.
    let hidden = format!(
        "//! {}\n/* {} */\n{}",
        "AttentionRequired | ParkedBlockedQuota is the rule, described",
        "and described again, at length",
        SECOND_IMPLEMENTATIONS[0].2
    );
    assert!(matches_a_copy_shape(&code_only(&hidden), &DELIVERY));
    // Prose alone is not a copy.
    let prose = "//! delivery_needs_human answers for AttentionRequired \
                 and ParkedBlockedQuota, and for nothing else.\n";
    assert!(!matches_a_copy_shape(&code_only(prose), &DELIVERY));
}

#[test]
fn a_test_mentioning_the_status_param_does_not_answer_for_the_product_code() {
    // The hole this closes: the crate's product code counted from a
    // status answer and asked for nothing, and a test file that happened
    // to name the parameter satisfied the check.
    let counts = "fn seed(res: &StatusResult) -> Attention { Attention::from_status(res) }";
    let asks_in_a_test = "#[cfg(test)]\nmod tests {\n    fn p() { \
                          StatusParams { open_deliveries: true } }\n}\n";
    assert!(
        counts_without_asking(&[format!("{counts}\n{asks_in_a_test}")]),
        "a test module answered for the code that talks to the daemon"
    );
    // Asking in the product code is what clears it, in any spelling.
    for asks in [
        "StatusParams { open_deliveries: true }",
        "b\"{\\\"method\\\":\\\"status\\\",\\\"params\\\":{\\\"open_deliveries\\\":true}}\"",
    ] {
        assert!(!counts_without_asking(&[format!("{counts}\n{asks}")]));
    }
    // Reading the RESULT field is not asking for it.
    assert!(counts_without_asking(&[format!(
        "{counts}\nlet open = res.open_deliveries.clone();"
    )]));
    // Defining `from_status` is not calling it: the owner declares it and
    // asks nobody for anything.
    assert!(!counts_without_asking(&[
        "pub fn from_status(res: &StatusResult) -> Attention { todo!() }".to_string()
    ]));
    // A crate that never counts is never asked to.
    assert!(!counts_without_asking(&["fn main() {}".to_string()]));
}

/// What the checks above cannot see, and why no version of this file will
/// see all of it. Stated as a test so it is read, and so that adding a
/// check means deleting a line from here.
///
/// **A determined duplicate passes this file.** Every extractor above
/// starts from a state name in Rust source text, and a second
/// implementation does not have to be written that way. Closing the gap in
/// general means deciding whether two pieces of code compute the same
/// predicate, which is semantic equivalence: this is a text scan with no
/// type information and no evaluator, so it will never decide that, and a
/// reviewer can always write a shape it does not anticipate. The previous
/// version of this file claimed the opposite and was defeated by a
/// verifier who derived the verdict without naming one state, with every
/// gate green. **The defence against a second home for this rule is
/// review.** This file only catches the copies people write by accident.
///
/// The list below is the holes that are KNOWN. It is not the whole set.
///
/// - A verdict that names no state at all: read off some other value that
///   tracks the rule, or computed over the field the states are derived
///   from. Every extractor above starts from a state name, so a copy that
///   writes none is invisible to all four. This is the one that beat the
///   previous version of this file.
/// - A verdict aliased through names of the file's own making: `const
///   NEEDY: DeliveryState = DeliveryState::AttentionRequired;` and then
///   `s == NEEDY`. The states are named where they are bound, not where
///   they are judged.
/// - A verdict split across several functions, each naming part of the
///   rule, joined by a caller that names none of it. Only when the halves
///   avoid `==` as well: [`compared`] gathers over the whole file, so a
///   split written as comparisons is caught.
/// - A verdict built from data read at runtime: a config file, a manifest,
///   a table in the ledger.
/// - A closure with no `-> bool` annotation doing what a predicate would.
/// - A predicate that names an unrelated state at its own top level, so
///   that neither [`inside_bool_fns`] set is the rule's. Nesting the noise
///   is caught; leaving it beside the verdict is not.
/// - A match arm whose body is a bool-valued expression rather than the
///   literal `true` or `false`.
/// - Anything outside Rust under `src/` or `tests/testrig`: the website,
///   the demos, the docs, the shipped manifests, the python harness.
/// - Semantics. A file may call `delivery_needs_human` on the wrong value,
///   or negate it, and this sees a correct call.
/// - `//` inside a string literal on the same line as a violation: the
///   comment stripper does not track string state, so it blanks the rest
///   of that line. It can only cause a miss, never a false alarm.
#[test]
fn the_tripwire_is_not_a_proof_and_says_where_it_fails() {
    // The checks are text over Rust under src/ and tests/testrig, and that is the whole
    // claim. This assertion exists so the list above has a reason to stay
    // accurate: it fails if the scan silently stops covering the tree.
    let files = rust_files();
    assert!(
        files.len() > 50,
        "the scan found almost nothing: {files:#?}"
    );
    assert!(
        files.iter().any(|p| p == &owner()),
        "the owner module is not in the scanned set, so nothing above is \
         actually exempting anything"
    );
    assert!(
        files.iter().any(|p| p == &here()),
        "this file is not in the scanned set, so the skip that exempts it \
         is exempting nothing and the quotes above would not be found"
    );

    // Three of the holes above, demonstrated rather than asserted about,
    // so the list is describing something real. Deleting a line up there
    // means deleting its case down here and watching this fail.
    let aliased = "
        const NEEDY: DeliveryState = DeliveryState::AttentionRequired;
        const PARKED: DeliveryState = DeliveryState::ParkedBlockedQuota;
        fn needs_a_person(s: DeliveryState) -> bool { s == NEEDY || s == PARKED }
    ";
    assert!(
        !matches_a_copy_shape(&code_only(aliased), &DELIVERY),
        "the aliasing hole is closed; delete it from the list above"
    );
    let split = "
        fn a(s: DeliveryState) -> bool { matches!(s, DeliveryState::AttentionRequired) }
        fn b(s: DeliveryState) -> bool { matches!(s, DeliveryState::ParkedBlockedQuota) }
        fn needs_a_person(s: DeliveryState) -> bool { a(s) || b(s) }
    ";
    assert!(
        !matches_a_copy_shape(&code_only(split), &DELIVERY),
        "the split-across-helpers hole is closed; delete it from the list"
    );
    // The same split written as comparisons IS caught, which is why the
    // list above says "only when the halves avoid ==".
    let split_by_comparison = "
        fn a(s: DeliveryState) -> bool { s == DeliveryState::AttentionRequired }
        fn b(s: DeliveryState) -> bool { s == DeliveryState::ParkedBlockedQuota }
        fn needs_a_person(s: DeliveryState) -> bool { a(s) || b(s) }
    ";
    assert!(matches_a_copy_shape(
        &code_only(split_by_comparison),
        &DELIVERY
    ));
    // The hole that beat the previous version, demonstrated: a verdict
    // that reaches the rule's answer without writing one state name.
    let names_no_state = "
        fn needs_a_person(d: &Delivery) -> bool { d.is_open() && !d.will_retry() }
    ";
    assert!(
        !matches_a_copy_shape(&code_only(names_no_state), &DELIVERY),
        "a copy that names no state is caught now; delete it from the list"
    );
}
