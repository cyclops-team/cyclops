//! Backend-neutral requests and outcomes for the messaging presentation.
//!
//! Presentation decides which exact request an operator confirmed and how a
//! completed answer changes the view. The watch adapter in `action_io.rs`
//! performs that request over the daemon socket.

use cyclops_proto::{MessageId, NotificationAttemptId};

use crate::detail::Loaded;

/// What a request is doing, which decides how its silence is read.
///
/// A read is not automatically harmless: an open that claims mutates a
/// mailbox. Carrying this with the request is what lets an unanswered
/// claiming open be recorded as unknown rather than as nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Read { claims: bool },
    Act(crate::detail::Action),
}

/// One in-flight request, named so its answer cannot land anywhere else.
///
/// The nonce is minted per request and never reused, so a response that
/// arrives after the reader closed one detail and opened another is
/// dropped rather than applied to a detail that never asked for it. The
/// target is carried too: two checks, because this one is about a body
/// reaching the wrong reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestToken {
    pub nonce: String,
    /// The frozen row AND the exact attempt, together.
    ///
    /// Both, because they answer different questions. The row says which
    /// detail may apply this answer and survives an alarm appearing or
    /// clearing underneath it. The attempt says what the request was
    /// actually about, and it is allowed to change: a requeue replaces
    /// it, so an answer carrying the old one must not be applied to a
    /// detail now looking at the new one.
    pub frozen: crate::queue::FrozenTarget,
    pub kind: RequestKind,
}

impl RequestToken {
    pub fn new(frozen: crate::queue::FrozenTarget, kind: RequestKind) -> RequestToken {
        RequestToken {
            nonce: uuid::Uuid::new_v4().to_string(),
            frozen,
            kind,
        }
    }

    /// The stable row this request belongs to.
    pub fn row(&self) -> &crate::queue::QueueTarget {
        &self.frozen.target
    }

    /// The exact attempt this request names, if any.
    pub fn attempt(&self) -> Option<NotificationAttemptId> {
        self.frozen.attempt
    }

    /// Did this request mutate anything, if it reached the daemon?
    pub fn mutates(&self) -> bool {
        match self.kind {
            RequestKind::Read { claims } => claims,
            RequestKind::Act(_) => true,
        }
    }

    pub fn action(&self) -> Option<crate::detail::Action> {
        match self.kind {
            RequestKind::Act(action) => Some(action),
            RequestKind::Read { .. } => None,
        }
    }
}

/// One thing to ask the daemon for an open detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionRequest {
    /// Open a message. `claim` is true only for an inbound pending row:
    /// a claim is what authorizes the body, and claiming something you
    /// are only observing would take somebody else's mail.
    OpenMessage {
        message_id: MessageId,
        claim: bool,
    },
    /// Open an alarm. Always by attempt id: a message id resolves only
    /// when exactly one recipient is stuck, and a broadcast with two
    /// answers ambiguous_attention.
    OpenAttention {
        attempt_id: NotificationAttemptId,
    },
    Reply {
        message_id: MessageId,
        body: String,
        client_key: String,
    },
    WithdrawNotification {
        attempt_id: NotificationAttemptId,
        recipient: cyclops_proto::RecipientKey,
    },
    ClearAlarm {
        attempt_id: NotificationAttemptId,
    },
    AttentionComplete {
        attempt_id: NotificationAttemptId,
    },
    AttentionDiscard {
        attempt_id: NotificationAttemptId,
    },
}

/// What came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionOutcome {
    /// A read succeeded and this is what the detail shows.
    Opened(Box<Loaded>),
    /// A mutation succeeded.
    Done(String),
    /// The daemon refused. Final: the operator should read it, not retry.
    Refused { code: String, message: String },
    /// The request never left this process. Nothing happened.
    ///
    /// A connect or hello failure is knowledge, not doubt: the daemon
    /// never saw the request, so every action is still available.
    NotSent(String),
    /// The request was written and the outcome is unknown. It may have
    /// landed, so a reply must be retried under the same key and the
    /// terminal verbs must not be repeated as fresh actions. A later
    /// matching terminal-accepted fact may permit no-key reconciliation.
    Uncertain(String),
}
