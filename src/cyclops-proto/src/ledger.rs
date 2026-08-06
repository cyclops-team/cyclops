//! Append-only NDJSON ledger schema.
//!
//! One file per watched session at `~/.cyclops/ledger/<session>.ndjson`.
//! Every message, delivery attempt, ACK, state transition, and gate decision
//! is a line here. Lines are never rewritten; corrections are new lines.
//!
//! `seq` is strictly increasing over the whole FILE, not just within a
//! `boot_id`: the writer scans the tail at open and continues numbering
//! (`cyclops-ledger`), so ordering by seq is correct across a daemon
//! restart. MEASURED on 2026-08-03: a restart carried on from 7 to 8 under
//! a new boot_id. What `boot_id` tells a reader is which daemon run wrote
//! a line, which is how a restart boundary is visible at all. Numbering
//! restarts only if the file itself is gone.
//!
//! This is the schema and nothing else. Appending is `cyclops-ledger`,
//! deciding what to append is cyclopsd, and folding a delivery chain back
//! into its msg line at read time is `cyclopsd/src/history.rs`.
//!
//! The ledger is plain text a human can `jq`. Secrets never enter it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerLine {
    pub seq: u64,
    pub boot_id: String,
    /// Message/event id, e.g. "m-3f9c2a". Delivery/state lines that concern
    /// an earlier message reuse its id so a thread is one `jq` filter.
    pub id: String,
    /// Unix ms.
    pub ts: u64,
    pub kind: Kind,
    pub from: String,
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// Per-recipient delivery records. Present on msg/fyi lines and on the
    /// state lines that advance a delivery.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deliveries: Vec<Delivery>,
    /// Structured extras for state/gate lines (sensor readings, gate
    /// decisions, transition causes). Absent on plain messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Agent-to-agent message expecting engagement.
    Msg,
    /// Announcement expecting no reply.
    Fyi,
    /// Daemon lifecycle: boot, attach, reconnect, self-test results.
    System,
    /// Agent state transition observed by fusion.
    State,
    /// Delivery gate decision: why an injection proceeded or was held.
    Gate,
}

/// One recipient's delivery record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delivery {
    pub to: String,
    pub state: DeliveryState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified_by: Option<VerifiedBy>,
    pub attempts: u32,
    /// Unix ms of the latest transition.
    pub ts: u64,
    /// Machine-readable cause for failure/parked states.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
}

/// Per-recipient delivery state machine (validation-report section 8):
///
/// queued -> gating -> pasting -> staged -> submitted
///   -> delivered_verified | delivered_unverified
/// failures -> retry_queued (bounded) -> attention_required
/// quota detection -> parked_blocked_quota (terminal, never auto-retried)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    Queued,
    /// Idle-wait plus liveness and modal checks.
    Gating,
    Pasting,
    /// Composer verification passed after paste.
    Staged,
    /// Enter sent.
    Submitted,
    /// ACK tier 1: hook fired with matchable payload.
    DeliveredVerified,
    /// ACK tier 2: screen-verified only (agy lane), or hook missing.
    DeliveredUnverified,
    /// Bounded redelivery pending.
    RetryQueued,
    /// Redelivery exhausted or an unsafe condition needs a human.
    AttentionRequired,
    /// Vendor quota exhausted (F11). Terminal until an operator acts.
    ParkedBlockedQuota,
}

impl DeliveryState {
    /// Legal transitions. Everything else is a bug worth a loud error.
    pub fn can_transition_to(self, next: DeliveryState) -> bool {
        use DeliveryState::*;
        match self {
            Queued => matches!(next, Gating | AttentionRequired | ParkedBlockedQuota),
            Gating => matches!(
                next,
                Pasting | RetryQueued | AttentionRequired | ParkedBlockedQuota
            ),
            Pasting => matches!(next, Staged | RetryQueued | AttentionRequired),
            Staged => matches!(next, Submitted | RetryQueued | AttentionRequired),
            Submitted => {
                matches!(
                    next,
                    DeliveredVerified | DeliveredUnverified | RetryQueued | AttentionRequired
                )
            }
            // Screen-verified deliveries may upgrade once a late hook ACK
            // arrives and matches.
            DeliveredUnverified => matches!(next, DeliveredVerified),
            DeliveredVerified => false,
            RetryQueued => matches!(next, Gating | AttentionRequired | ParkedBlockedQuota),
            // Terminal states an operator must clear.
            AttentionRequired => matches!(next, Queued),
            ParkedBlockedQuota => matches!(next, Queued),
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            DeliveryState::DeliveredVerified
                | DeliveryState::AttentionRequired
                | DeliveryState::ParkedBlockedQuota
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifiedBy {
    Hook,
    Screen,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_line_roundtrip() {
        let line = LedgerLine {
            seq: 42,
            boot_id: "b-1".into(),
            id: "m-abc".into(),
            ts: 1_754_000_000_000,
            kind: Kind::Msg,
            from: "codex".into(),
            to: vec!["reviewer".into()],
            subject: Some("Review the rate limiter".into()),
            body: Some("please".into()),
            reply_to: None,
            deliveries: vec![Delivery {
                to: "reviewer".into(),
                state: DeliveryState::DeliveredVerified,
                verified_by: Some(VerifiedBy::Hook),
                attempts: 1,
                ts: 1_754_000_000_100,
                cause: None,
            }],
            data: None,
        };
        let s = serde_json::to_string(&line).unwrap();
        let back: LedgerLine = serde_json::from_str(&s).unwrap();
        assert_eq!(back.seq, 42);
        assert_eq!(back.deliveries[0].state, DeliveryState::DeliveredVerified);
        // Forward compatibility: unknown fields tolerated.
        let with_extra = s.trim_end_matches('}').to_string() + r#","future":1}"#;
        assert!(serde_json::from_str::<LedgerLine>(&with_extra).is_ok());
    }

    #[test]
    fn state_machine_shape() {
        use DeliveryState::*;
        assert!(Queued.can_transition_to(Gating));
        assert!(Gating.can_transition_to(Pasting));
        assert!(Pasting.can_transition_to(Staged));
        assert!(Staged.can_transition_to(Submitted));
        assert!(Submitted.can_transition_to(DeliveredVerified));
        assert!(Submitted.can_transition_to(DeliveredUnverified));
        assert!(DeliveredUnverified.can_transition_to(DeliveredVerified));
        assert!(RetryQueued.can_transition_to(Gating));
        // Quota parking is reachable from queue-side states and is terminal.
        assert!(Queued.can_transition_to(ParkedBlockedQuota));
        assert!(!ParkedBlockedQuota.can_transition_to(Gating));
        assert!(ParkedBlockedQuota.can_transition_to(Queued));
        // Delivered is final.
        assert!(!DeliveredVerified.can_transition_to(Queued));
    }
}
