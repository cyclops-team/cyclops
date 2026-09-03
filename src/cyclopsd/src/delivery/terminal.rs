//! Low-level terminal effect verification, payload rendering, and screen parsing.

use cyclops_manifest::{strip_csi, Manifest};
use cyclops_proto::{AgentState, ComposerSemantic, Kind, LedgerLine};

use super::{exact_composer_content_for_state, render_payload, ComposerContentProof};

/// Render the canonical payload for a direct message ledger line.
pub(crate) fn render_canonical_message_payload(message: &LedgerLine) -> String {
    render_payload(
        &message.id,
        &message.from,
        message.subject.as_deref().unwrap_or_default(),
        message.body.as_deref().unwrap_or_default(),
        message.kind == Kind::Fyi,
    )
}

/// The terminal sentinel: the last line of every direct delivery payload.
pub(crate) fn sentinel_for(msg_id: &str) -> String {
    format!("[cyclops:end {msg_id}]")
}

/// Is this hook prompt the payload this delivery rendered?
pub(crate) fn prompt_matches(text: &str, payload: &str) -> bool {
    text == payload || text.strip_suffix('\n') == Some(payload)
}

/// Prove that the composer has a clean idle state under the active manifest.
pub(crate) fn clean_composer_proof(manifest: &Manifest, capture: &str) -> bool {
    let plain = strip_csi(capture);
    crate::fusion::screen_winner_esc(manifest, &plain, Some(capture)).is_some_and(|rule| {
        rule.state == AgentState::Idle && rule.composer_semantic == Some(ComposerSemantic::Clean)
    }) && visible_clean_composer_proof(manifest, capture)
}

/// Prove that the manifest-owned composer region is visibly empty.
pub(crate) fn visible_clean_composer_proof(manifest: &Manifest, capture: &str) -> bool {
    matches!(
        exact_composer_content_for_state(
            manifest,
            capture,
            AgentState::Idle,
            Some(ComposerSemantic::Clean),
        ),
        ComposerContentProof::Visible(content) if content.is_empty()
    )
}

/// Parse quota reset hints from screen captures.
pub(crate) fn parse_reset_hint(screen: &str) -> Option<String> {
    let idx = screen.find("esets in ")?;
    let tail = &screen[idx + "esets in ".len()..];
    let token: String = tail
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    if token.is_empty() {
        None
    } else {
        Some(format!("resets in {token}"))
    }
}
