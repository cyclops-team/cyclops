//! Low-level terminal effect verification, payload rendering, and screen parsing.

use cyclops_manifest::{strip_csi, Manifest};
use cyclops_proto::{AgentState, ComposerSemantic, Kind, LedgerLine};

use super::{exact_composer_content_for_state, ComposerContentProof};

/// The terminal sentinel: the last line of every direct delivery payload.
pub(crate) fn sentinel_for(msg_id: &str) -> String {
    format!("[cyclops:end {msg_id}]")
}

/// Render the human-readable payload string for a direct message.
pub fn render_payload(msg_id: &str, from: &str, subject: &str, body: &str, fyi: bool) -> String {
    let mut lines = vec![format!(
        "[cyclops {msg_id}] FROM: {from}  SUBJECT: {subject}"
    )];
    if !body.is_empty() {
        lines.push(body.to_string());
    }
    if !fyi && from != cyclops_proto::label::ADMIN {
        lines.push(format!(
            "Reply: cyclops send {from} --subject \"...\" --summary \"First sentence. Second sentence.\""
        ));
    }
    lines.push(sentinel_for(msg_id));
    lines.join("\n")
}

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
