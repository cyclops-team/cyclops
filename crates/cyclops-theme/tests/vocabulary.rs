//! The vocabulary against the code that paints it.
//!
//! The crate's rule is that a token no renderer paints does not belong in
//! the list: it would load in silence and change nothing on screen, which
//! is the same lie as a stale doc. The audit that once removed tokens
//! worked from a hand-written list of the names it happened to know. This
//! is the version that needs no list.
//!
//! A renderer reaches a token one of two ways: it names the constant, or
//! it calls one of the engine's group mappings and paints whatever comes
//! back. Both count as painted, and both are checked here.

use std::path::{Path, PathBuf};

use cyclops_proto::{AgentState, DeliveryState};
use cyclops_theme::{delivery_token, state_token, tokens};

/// Repo root: this crate sits at `<root>/crates/cyclops-theme`.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<crate> has two parents")
        .to_path_buf()
}

fn read_rust(dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            read_rust(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            if let Ok(text) = std::fs::read_to_string(&path) {
                out.push(text);
            }
        }
    }
}

/// Every Rust source in the workspace except this crate's own. The engine
/// naming its own tokens would prove nothing about what reaches a screen.
fn renderers() -> Vec<String> {
    let root = repo_root();
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root.join("crates"))
        .expect("crates dir")
        .flatten()
    {
        let path = entry.path();
        if path.is_dir() && path.file_name().is_some_and(|n| n != "cyclops-theme") {
            read_rust(&path, &mut out);
        }
    }
    out
}

/// The identifier a renderer names a token by: the token string uppercased
/// with the dot as an underscore. The eight role slots are the exception
/// they are declared as, one `ROLE` array a renderer indexes into.
fn constant_of(token: &str) -> String {
    if tokens::ROLE.contains(&token) {
        return "ROLE".to_string();
    }
    token.replace('.', "_").to_uppercase()
}

/// Tokens a renderer reaches through a group mapping rather than by name.
/// A renderer that calls `state_token` paints every token that function
/// can return, so the whole group family counts the moment one call site
/// exists; a family nothing maps to is caught by the crate's own tests.
fn reached_through_mappings(sources: &[String]) -> Vec<&'static str> {
    let mut out = Vec::new();
    if sources.iter().any(|s| s.contains("state_token(")) {
        out.extend(
            [
                AgentState::Unknown,
                AgentState::Idle,
                AgentState::IdleWithInput,
                AgentState::Working,
                AgentState::BlockedModal,
                AgentState::BlockedPermission,
                AgentState::BlockedQuota,
                AgentState::Dead,
            ]
            .map(state_token),
        );
    }
    if sources.iter().any(|s| s.contains("delivery_token(")) {
        out.extend(
            [
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
            ]
            .map(delivery_token),
        );
    }
    out
}

#[test]
fn every_token_in_the_vocabulary_is_painted_by_a_renderer() {
    let sources = renderers();
    assert!(!sources.is_empty(), "found no renderer sources to read");
    let mapped = reached_through_mappings(&sources);
    for token in tokens::ALL {
        // surface.fg is the one documented exception, checked below.
        if token == tokens::SURFACE_FG {
            continue;
        }
        if mapped.contains(&token) {
            continue;
        }
        let needle = format!("tokens::{}", constant_of(token));
        assert!(
            sources.iter().any(|s| s.contains(&needle)),
            "no renderer names `{needle}` and no group mapping returns \
             `{token}`, so editing it in a theme file changes nothing on \
             screen: paint it or drop it from the vocabulary (and from \
             docs/themes.md)"
        );
    }
}

/// The other direction, because the exception is a documented claim too:
/// `surface.fg` is the engine's fallback for a token name outside the
/// vocabulary, and both the crate doc and docs/themes.md tell readers that
/// editing it changes nothing. A renderer reaching for it makes all three
/// pages wrong at once.
#[test]
fn surface_fg_stays_the_engines_own_fallback() {
    let needle = format!("tokens::{}", constant_of(tokens::SURFACE_FG));
    let offenders = renderers().iter().filter(|s| s.contains(&needle)).count();
    assert_eq!(
        offenders, 0,
        "{needle} is documented as painted by nobody; a renderer that paints \
         it has to be documented instead"
    );
}
