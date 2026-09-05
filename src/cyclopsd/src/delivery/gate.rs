//! The delivery gate, the one write it admits, and the receipt that follows.
//!
//! Ordinary doorbell delivery writes one line and presses Enter for a bound,
//! live agent process unless a human draft is positively observed or a named
//! block is present (modal, permission, quota, dead, copy-mode, durable
//! composer hold). Ambiguous or absent composer evidence does not hold a
//! doorbell. A raw send bypasses the composer check entirely and is recorded
//! as an unverified write. Uncertainty is recorded, never retried
//! automatically.
//!
//! [`gate`] is the first half of that sentence and [`attempt_delivery`] the
//! second; everything after them decides what the journal says the write
//! proved.

use std::borrow::Cow;

use super::*;

/// One transition request for [`advance`].
pub(crate) struct Step<'a> {
    pub(crate) next: DeliveryState,
    pub(crate) cause: Option<&'a str>,
    pub(crate) verified_by: Option<VerifiedBy>,
    /// When the vendor edge that justifies this step was taken, for the
    /// steps that carry one. Passed through rather than looked up again:
    /// the stored hook slot is mutable, so a re-read can hand back a
    /// concurrent Stop instead of the ACK being acted on.
    pub(crate) turn_edge_ms: Option<u64>,
    /// The turn the vendor named in the payload that justifies this step.
    /// Carried for the same reason as the edge: the stored slot is
    /// mutable, and a re-read can hand back a different turn than the one
    /// being acted on.
    pub(crate) turn: Option<crate::turnkey::TurnKey>,
}

impl<'a> Step<'a> {
    pub(crate) fn to(next: DeliveryState) -> Step<'a> {
        Step {
            next,
            cause: None,
            verified_by: None,
            turn_edge_ms: None,
            turn: None,
        }
    }
    pub(crate) fn turn(mut self, turn: Option<crate::turnkey::TurnKey>) -> Step<'a> {
        self.turn = turn;
        self
    }
    pub(crate) fn turn_edge(mut self, ms: u64) -> Step<'a> {
        self.turn_edge_ms = Some(ms);
        self
    }
    pub(crate) fn cause(mut self, c: &'a str) -> Step<'a> {
        self.cause = Some(c);
        self
    }
    pub(crate) fn verified(mut self, v: VerifiedBy) -> Step<'a> {
        self.verified_by = Some(v);
        self
    }
}

/// Why the gate is holding a notification instead of writing.
///
/// Rendered to the same journal strings older readers already parse (the
/// CLI and `cyclops-ui` map them to words), so a reader of a 1.0 ledger and
/// a 1.1 ledger sees one vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HoldCause {
    /// The session's control connection is down.
    SessionDetached,
    NoSuchPane,
    PaneDead,
    /// The human is reading their own scrollback; a paste now lands where
    /// neither side can see it.
    PaneInMode,
    NoManifest,
    /// Nothing proved that the pane's foreground process is the bound agent.
    BindingUnprovable,
    /// The agent handed the terminal to a tool and the screen does not
    /// read as the agent: keystrokes would land in the tool.
    ForegroundNotAgent,
    /// The occupant changed between the gate's proof and the write.
    BindingChanged,
    /// A human draft was positively observed, or a doorbell an earlier
    /// attempt staged has not been consumed yet.
    ComposerHold,
    BlockedQuota,
    /// A modal or permission prompt the manifest does not dismiss on its
    /// own; the rule id names it.
    Blocked(String),
    /// Another attempt claimed this pane's composer between the gate's
    /// verdict and the write.
    BarrierHeld,
}

impl HoldCause {
    /// The gate ledger line's cause, exact for diagnostics.
    pub(crate) fn journal(&self) -> Cow<'static, str> {
        match self {
            Self::SessionDetached => "session_detached".into(),
            Self::NoSuchPane => "no_such_pane".into(),
            Self::PaneDead => "pane_dead".into(),
            Self::PaneInMode => "pane_in_mode".into(),
            Self::NoManifest => "no_manifest".into(),
            Self::BindingUnprovable => "occupant_unprovable".into(),
            Self::ForegroundNotAgent => "foreground_not_agent".into(),
            Self::BindingChanged => "binding_changed".into(),
            Self::ComposerHold => "composer_hold".into(),
            Self::BlockedQuota => "blocked_quota".into(),
            Self::Blocked(rule) => format!("blocked:{rule}").into(),
            Self::BarrierHeld => "barrier_held".into(),
        }
    }

    /// The normalized `held_by` token a receipt carries. Vendor rule ids
    /// stay on the ledger line; receipts expose only this closed set.
    pub(crate) fn receipt_token(&self) -> &'static str {
        match self {
            Self::SessionDetached => "session_detached",
            Self::PaneInMode => "pane_in_mode",
            Self::ComposerHold => "held_for_existing_draft",
            Self::BlockedQuota => "blocked_quota",
            Self::Blocked(_) => "blocked",
            Self::NoSuchPane
            | Self::PaneDead
            | Self::NoManifest
            | Self::BindingUnprovable
            | Self::ForegroundNotAgent
            | Self::BindingChanged
            | Self::BarrierHeld => "unknown",
        }
    }
}

/// The bytes one attempt writes, fixed at the write boundary.
pub(crate) struct AttemptPayload {
    pub(crate) bytes: String,
    pub(crate) transport: NotificationTransport,
    pub(crate) doorbell_format: Option<u32>,
}

/// What one attempt pastes: the Format 4 doorbell (the sender label, the
/// sender-authored or daemon-derived summary, and the exact attempt claim
/// command), or for a raw send the whole rendered message.
pub(crate) fn select_attempt_payload(
    handle: &DeliveryHandle,
) -> Result<AttemptPayload, NotificationAdapterError> {
    let notification = &handle.notification;
    let message = notification.message_line()?;
    if handle.raw {
        return Ok(AttemptPayload {
            bytes: render_canonical_message_payload(&message),
            transport: NotificationTransport::Raw,
            doorbell_format: None,
        });
    }
    let bytes = render_summary_doorbell(&message, notification.attempt_id())
        .ok_or(NotificationAdapterError::MessageMissing)?;
    Ok(AttemptPayload {
        bytes,
        transport: NotificationTransport::Doorbell,
        doorbell_format: Some(cyclops_proto::DOORBELL_FORMAT_SUMMARY_CLAIM),
    })
}

/// Format 4 for one message line: the sender and the recipients from the
/// immutable presentation, the recorded summary when the sender gave one,
/// else the same derivation the accept path applies, so a message accepted
/// before summaries existed still renders one exact row.
fn render_summary_doorbell(
    message: &LedgerLine,
    attempt_id: NotificationAttemptId,
) -> Option<String> {
    let metadata = message.data.as_ref().and_then(|data| {
        serde_json::from_value::<cyclops_proto::MessageMetadata>(data.clone()).ok()
    })?;
    let summary = metadata.summary.or_else(|| {
        cyclops_proto::derive_message_summary(
            message.body.as_deref().unwrap_or_default(),
            message.subject.as_deref().unwrap_or_default(),
        )
    })?;
    let recipients = cyclops_proto::render_recipient_list(
        &metadata
            .presentation
            .recipient_labels
            .iter()
            .map(|presentation| presentation.label.as_str())
            .collect::<Vec<_>>(),
        metadata.broadcast,
    );
    Some(cyclops_proto::render_doorbell_v4(
        &metadata.presentation.sender_label,
        &recipients,
        &summary,
        attempt_id,
    ))
}

/// Rebuild the exact payload selected at this notification's write boundary.
///
/// The composer projection and the hook prompt matcher share this owner so a
/// transport format cannot be actionable in one path and unprovable in the
/// other. Every format an older daemon wrote stays rebuildable for replay;
/// only Format 4 and raw are written now.
pub(crate) fn expected_notification_payload(
    record: &cyclops_proto::NotificationRecord,
    message: &LedgerLine,
) -> Option<String> {
    if message.id != record.message_id.as_str() {
        return None;
    }
    match (record.transport, record.doorbell_format) {
        (NotificationTransport::Doorbell, format) => match format {
            None => Some(cyclops_proto::render_legacy_doorbell(&record.message_id)),
            Some(cyclops_proto::DOORBELL_FORMAT_COMPACT_CLAIM) => {
                Some(cyclops_proto::render_doorbell_v1(&record.message_id))
            }
            Some(cyclops_proto::DOORBELL_FORMAT_ATTEMPT_CLAIM) => Some(
                cyclops_proto::render_doorbell_v2(&record.message_id, record.attempt_id),
            ),
            Some(cyclops_proto::DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM) => {
                Some(cyclops_proto::render_doorbell_v3(record.attempt_id))
            }
            Some(cyclops_proto::DOORBELL_FORMAT_SUMMARY_CLAIM) => {
                render_summary_doorbell(message, record.attempt_id)
            }
            Some(_) => None,
        },
        (NotificationTransport::DirectPayload | NotificationTransport::Raw, None) => {
            Some(render_canonical_message_payload(message))
        }
        (NotificationTransport::DirectPayload | NotificationTransport::Raw, Some(_)) => None,
    }
}

/// Result of checking the live route against its durable mailbox binding.
pub(crate) enum HandleRoute {
    Exact(Arc<SessionWatcher>),
    BindingChanged,
    BindingUnprovable,
    Unavailable,
}

/// Classify the live watcher without treating an identity mismatch as absence.
///
/// A pane replacement can reach the watcher before the ordered registry event.
/// That route is present but changed, so the pre-write barrier must record a
/// reprovable identity change instead of a permanent session-unavailable block.
pub(crate) fn handle_route(inner: &Inner, handle: &DeliveryHandle) -> HandleRoute {
    let recipient = handle.notification.recipient();
    let Some(session_instance_id) = recipient.session_instance_id() else {
        return HandleRoute::Unavailable;
    };
    let Some(pane_id) = recipient.pane_id() else {
        return HandleRoute::Unavailable;
    };
    if pane_id.to_string() != handle.pane_id {
        return HandleRoute::BindingChanged;
    }
    let Some(slot) = inner.session(handle.session_idx) else {
        return HandleRoute::Unavailable;
    };
    let watcher = {
        let link = slot.link.lock().expect("session link lock");
        if !link.attached
            || link
                .identity
                .as_ref()
                .map(|identity| identity.session_instance_id())
                != Some(session_instance_id)
        {
            return HandleRoute::Unavailable;
        }
        link.watcher.as_ref().map(Arc::clone)
    };
    let Some(watcher) = watcher else {
        return HandleRoute::Unavailable;
    };
    let Some(row) = watcher.pane(&handle.pane_id) else {
        return HandleRoute::Unavailable;
    };
    let Some(root) = crate::identity::ProcId::of(row.pane_pid) else {
        return HandleRoute::BindingUnprovable;
    };
    let Ok(pane_root) = ProcessInstanceId::new(root.pid, root.birth) else {
        return HandleRoute::BindingUnprovable;
    };
    let registry = inner.registry.lock().expect("registry lock");
    if registry.for_route(recipient, pane_root).is_some() {
        HandleRoute::Exact(watcher)
    } else if registry.for_recipient(recipient).is_some() {
        HandleRoute::BindingChanged
    } else {
        HandleRoute::BindingUnprovable
    }
}

/// Resolve only a watcher whose durable route binding is exact.
pub(crate) fn watcher_for_handle(
    inner: &Inner,
    handle: &DeliveryHandle,
) -> Option<Arc<SessionWatcher>> {
    match handle_route(inner, handle) {
        HandleRoute::Exact(watcher) => Some(watcher),
        HandleRoute::BindingChanged | HandleRoute::BindingUnprovable | HandleRoute::Unavailable => {
            None
        }
    }
}

/// Apply one in-memory transition if the attempt is still in an expected
/// state. Returns false when a concurrent actor (ACK matcher vs worker
/// timeout) already moved it; the caller treats that as "someone else
/// resolved it". The durable record is the workspace notification fact
/// appended separately through `NotificationContext`; this state only
/// coordinates the worker, the ACK matcher, quiesce, and receipt waiters.
pub(crate) fn advance(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    allowed_from: &[DeliveryState],
    step: Step<'_>,
) -> bool {
    let from = {
        let mut st = handle.state.lock().expect("handle state lock");
        if !allowed_from.contains(&st.state) {
            return false;
        }
        let from = st.state;
        st.state = step.next;
        if let Some(v) = step.verified_by {
            st.verified_by = Some(v);
        }
        st.cause = step.cause.map(str::to_string);
        if step.next != DeliveryState::Gating {
            st.held_by = None;
        }
        from
    };
    // send_replace, not send: watch::Sender::send drops the value when no
    // receiver exists, and receipt blocking subscribes late. A worker that
    // resolves before the subscribe must still leave the state readable, or
    // the receipt waits out its whole cap on an already-final delivery.
    handle.state_tx.send_replace(step.next);
    // A receipt is the first thing that PROVES the composer was consumed:
    // either the vendor acknowledged this message, or the marker left the
    // composer and a turn started. Send-keys returning Ok proves neither.
    // tmux accepting the key says nothing about what the vendor did with
    // it, and a swallowed Enter leaves the payload staged.
    //
    // Only the FIRST resolution promotes. The unverified-to-verified
    // upgrade is the same consumption arriving twice, and re-marking it
    // would push the mark past a turn-end edge that has already arrived.
    // A resolution with no verifier proved nothing about the composer and
    // settles no hold; its caller releases the barrier outright.
    let first_receipt = from == DeliveryState::Submitted
        && step.verified_by.is_some()
        && matches!(
            step.next,
            DeliveryState::DeliveredVerified | DeliveryState::DeliveredUnverified
        );
    // A late upgrade carries the correlated edge a screen-only receipt
    // did not have. If that receipt left the hold waiting, this is the
    // evidence it was waiting for, so it settles here too.
    let late_correlated =
        from == DeliveryState::DeliveredUnverified && step.next == DeliveryState::DeliveredVerified;
    if first_receipt || late_correlated {
        settle_hold_on_receipt(
            inner,
            handle,
            step.verified_by,
            step.turn_edge_ms,
            step.turn,
        );
    }
    true
}

/// Append a gate decision line and emit the matching event.
pub(crate) fn gate_line(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    action: &str,
    rule: Option<&str>,
    cause: Option<&str>,
) {
    let recipient = inner.recipient_key(handle.session_idx, &handle.pane_id);
    let line = LedgerLine {
        to: vec![handle.to.clone()],
        ..daemon_line(
            Kind::Gate,
            handle.msg_id.clone(),
            json!({
                "to": handle.to,
                "recipient": recipient,
                "action": action,
                "rule": rule,
                "cause": cause,
            }),
        )
    };
    let seq = inner.append_line(handle.session_idx, line);
    inner.emit(
        "gate",
        json!({
            "id": handle.msg_id,
            "to": handle.to,
            "recipient": recipient,
            "action": action,
            "rule": rule,
            "cause": cause,
        }),
        seq,
    );
}

pub(crate) enum AttemptOutcome {
    /// Delivery resolved (verified, unverified, or matcher-resolved).
    Done,
    /// The mailbox atomically withdrew this attempt before the pane write.
    NoLongerCurrentBeforeWrite,
    /// This attempt failed; the boundary feeds retry accounting.
    Failed(AttemptFailure),
}

/// The irreversible boundary for one failed attempt. Once a write may have
/// happened, repeating it is unsafe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteBoundary {
    BeforeWrite,
    AfterWrite,
}

/// A delivery failure and its closed, semantic boundary. Call sites select a
/// named failure constructor, so an after-write cause cannot accidentally be
/// marked retryable by passing a boolean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttemptFailure {
    pub(crate) cause: String,
    pub(crate) boundary: WriteBoundary,
    pub(crate) pre_write_block: Option<Box<PreWriteBlock>>,
    /// Set when the failure is readiness moving under a delivery that had
    /// not written anything: it goes back to the gate, not to the budget.
    pub(crate) regate: Option<HoldCause>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreWriteBlock {
    pub(crate) cause: NotificationPreWriteCause,
    pub(crate) observation: Option<NotificationPreWriteObservation>,
}

impl AttemptFailure {
    pub(crate) fn blocked_before_write(
        cause: impl Into<String>,
        block: NotificationPreWriteCause,
    ) -> Self {
        Self {
            cause: cause.into(),
            boundary: WriteBoundary::BeforeWrite,
            pre_write_block: Some(Box::new(PreWriteBlock {
                cause: block,
                observation: None,
            })),
            regate: None,
        }
    }

    fn after_write(cause: impl Into<String>) -> Self {
        Self {
            cause: cause.into(),
            boundary: WriteBoundary::AfterWrite,
            pre_write_block: None,
            regate: None,
        }
    }

    pub(crate) fn session_detached() -> Self {
        Self::blocked_before_write(
            "session_detached",
            NotificationPreWriteCause::SessionUnavailable,
        )
    }

    pub(crate) fn no_manifest() -> Self {
        Self::blocked_before_write(
            "no_manifest",
            NotificationPreWriteCause::ManifestUnavailable,
        )
    }

    pub(crate) fn payload_unavailable() -> Self {
        Self::blocked_before_write(
            "payload_unavailable",
            NotificationPreWriteCause::PayloadUnavailable,
        )
    }

    pub(crate) fn pane_rebound_before_paste() -> Self {
        Self::blocked_before_write(
            "pane_rebound",
            NotificationPreWriteCause::WriteReadinessChanged,
        )
    }

    /// The composer barrier was not this attempt's to take: somebody
    /// else's payload or a person's typing is in there. Nothing was
    /// written, so this returns to the gate.
    pub(crate) fn barrier_held() -> Self {
        Self {
            cause: HoldCause::BarrierHeld.journal().into_owned(),
            boundary: WriteBoundary::BeforeWrite,
            pre_write_block: None,
            regate: Some(HoldCause::BarrierHeld),
        }
    }

    pub(crate) fn spool_failed() -> Self {
        Self::blocked_before_write("spool_failed", NotificationPreWriteCause::SpoolFailed)
    }

    pub(crate) fn paste_command_unwritten() -> Self {
        Self::blocked_before_write(
            "paste_command_unwritten",
            NotificationPreWriteCause::PasteCommandUnwritten,
        )
    }

    pub(crate) fn paste_failed() -> Self {
        Self::after_write("paste_failed")
    }

    pub(crate) fn pane_rebound_after_paste() -> Self {
        Self::after_write("pane_rebound_after_paste")
    }

    pub(crate) fn submit_failed() -> Self {
        Self::after_write("submit_failed")
    }

    /// The durable boundary could not be advanced after the attempt crossed it.
    /// Retrying could duplicate a notification whose append outcome is unknown.
    pub(crate) fn notification_record_failed() -> Self {
        Self::after_write(NOTIFICATION_RECORD_FAILED)
    }

    /// Map the injector's closed set of pre-submit causes to the semantic
    /// constructors above. Unknown injector errors remain conservatively
    /// after-write; they must never gain retryability by default.
    pub(crate) fn from_inject(cause: String) -> Self {
        match cause.as_str() {
            "spool_failed" => Self::spool_failed(),
            "barrier_held" => Self::barrier_held(),
            "paste_failed" => Self::paste_failed(),
            NOTIFICATION_RECORD_FAILED => Self::notification_record_failed(),
            _ => Self::after_write(cause),
        }
    }
}

/// One doorbell write, in the order the steps must run.
///
/// The gate proved `proven` a moment ago. That moment is over by the time
/// the paste command goes out, so the occupant is re-read once before the
/// paste and once before Enter, by the same function, and nothing else is
/// re-proven in between: a second look at the composer cannot make a paste
/// not have happened, so Enter follows whatever the read-back said and the
/// journal records what was seen.
pub(crate) async fn attempt_delivery(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    proven: &fusion::Binding,
) -> AttemptOutcome {
    // 1. The route and the rules the gate admitted under.
    let Some(watcher) = watcher_for_handle(inner, handle) else {
        return AttemptOutcome::Failed(AttemptFailure::session_detached());
    };
    let Some(manifest) = inner.manifests.get(&proven.manifest) else {
        return AttemptOutcome::Failed(AttemptFailure::no_manifest());
    };
    let injector = TmuxInjector {
        client: watcher.client(),
        buffer: format!(
            "cyc-{}-{}",
            std::process::id(),
            inner.engine.buffer_seq.fetch_add(1, Ordering::Relaxed)
        ),
    };
    // 2. The bytes, spooled BEFORE the occupant check. Loading the buffer
    //    costs a control round trip, and a round trip is time a person can
    //    type that no later capture would see; spooling touches no pane, so
    //    it goes first and the check stays the last thing before the write.
    let selected = match select_attempt_payload(handle) {
        Ok(selected) => selected,
        Err(error) => {
            error!(id = %handle.msg_id, error = %error, "notification payload reconstruction failed");
            return AttemptOutcome::Failed(AttemptFailure::payload_unavailable());
        }
    };
    handle.set_attempt_payload(selected.bytes.clone(), Some(selected.transport));
    if let Err(cause) = injector.spool(&selected.bytes).await {
        return AttemptOutcome::Failed(AttemptFailure::from_inject(cause));
    }
    inject_pause(inner, "pre_paste").await;
    // 3. The one pre-paste occupant check.
    if let Err(detail) = occupant_unchanged(inner, handle, proven) {
        injector.discard().await;
        gate_line(inner, handle, "rebound", None, Some(&detail.journal()));
        return AttemptOutcome::Failed(rebound_before_paste(detail));
    }
    // 4. The paste. `on_write` runs immediately before the command that may
    //    put bytes in the composer: it claims the composer barrier and
    //    records `Writing`, so from here the attempt is post-write whatever
    //    tmux answers. A refused claim means readiness moved in the gap and
    //    the attempt goes back to the gate with nothing written.
    let target = StagingTarget::ExactRow(&selected.bytes);
    let (staged_window, id_staged, payload_at_proof) = match inject(
        &injector,
        handle,
        manifest,
        target,
        &selected.bytes,
        &|| {
            handle
                .notification
                .ensure_current_gating()
                .map_err(notification_write_cause)?;
            let pane_root = ProcessInstanceId::new(proven.pane_root.pid, proven.pane_root.birth)
                .map_err(|_| NOTIFICATION_RECORD_FAILED.to_string())?;
            let leader = ProcessInstanceId::new(proven.leader.pid, proven.leader.birth)
                .map_err(|_| NOTIFICATION_RECORD_FAILED.to_string())?;
            let agent = ProcessInstanceId::new(proven.agent.pid, proven.agent.birth)
                .map_err(|_| NOTIFICATION_RECORD_FAILED.to_string())?;
            latch_hold(inner, handle, proven)?;
            let mut unwritten_hold = UnwrittenHold::new(inner, handle, proven);
            fail_pre_record_writing_if_requested(inner, handle);
            handle
                .notification
                .record_writing(
                    pane_root,
                    leader,
                    agent,
                    &proven.manifest,
                    NotificationTransport::Doorbell,
                    selected.doorbell_format,
                )
                .map_err(notification_write_cause)?;
            handle.write_boundary_crossed.store(true, Ordering::SeqCst);
            unwritten_hold.commit();
            Ok(())
        },
    )
    .await
    {
        Ok(v) => v,
        Err(failure) => {
            return finish_inject_failure(handle, failure, || {
                rollback_unwritten_hold(inner, handle, proven)
            });
        }
    };
    let staging_verified = !payload_at_proof.is_empty();
    if !advance(
        inner,
        handle,
        &[DeliveryState::Pasting],
        Step::to(DeliveryState::Staged),
    ) {
        return AttemptOutcome::Done;
    }
    // 5. The one post-paste occupant check, immediately before Enter. The
    //    staged row belongs to the occupant that took it; the key must
    //    never reach whoever replaced them.
    inject_pause(inner, "pre_submit").await;
    if let Err(detail) = occupant_unchanged(inner, handle, proven) {
        unregister_ack(inner, handle);
        gate_line(inner, handle, "rebound", None, Some(&detail.journal()));
        return AttemptOutcome::Failed(AttemptFailure::pane_rebound_after_paste());
    }
    // 6. Enter. Subscribed before the key: a fast vendor can paint its whole
    //    working phase before send-keys returns, and the ACK registration
    //    lands here too, after every proof and before the key, because the
    //    measured hook edge follows Enter by 21-28ms.
    let submit_key = if manifest.injection.submit.is_empty() {
        "Enter"
    } else {
        manifest.injection.submit.as_str()
    };
    let receipt_events = inner.events.subscribe();
    let receipt_turn_events = inner.events.subscribe();
    let receipt_submit_at = Instant::now();
    let receipt_submit_at_ms = unix_ms();
    // The AGENT behind the binding is what a hook report is filed under;
    // the foreground leader can be a tool the agent handed the terminal to.
    *handle.submitted_agent.lock().expect("submitted agent lock") = Some(proven.agent);
    handle
        .submitted_at_ms
        .store(receipt_submit_at_ms, Ordering::SeqCst);
    *handle
        .submitted_manifest
        .lock()
        .expect("submitted manifest lock") = Some(proven.manifest.clone());
    register_ack(inner, handle);
    if let Err(cause) = injector.submit(&handle.pane_id, submit_key).await {
        unregister_ack(inner, handle);
        debug_assert_eq!(cause, "submit_failed");
        return AttemptOutcome::Failed(AttemptFailure::submit_failed());
    }
    let record_res = if staging_verified {
        handle.notification.record_submitted()
    } else {
        handle.notification.record_submitted_unverified()
    };
    if let Err(error) = record_res {
        error!(id = %handle.msg_id, error = %error, "notification submitted fact failed");
        return AttemptOutcome::Failed(AttemptFailure::notification_record_failed());
    }
    // Test-only pause: an acknowledgement can arrive from here on, while the
    // delivery is still `Staged` in memory.
    inject_pause(inner, "post_key").await;
    if !advance(
        inner,
        handle,
        &[DeliveryState::Staged],
        Step::to(DeliveryState::Submitted),
    ) {
        return AttemptOutcome::Done;
    }
    // 7. The receipt: a hook ACK, then screen evidence, then nothing. Any
    //    accepted early receipt is taken before claim settlement can return:
    //    a hook can carry the exact TurnKey while a concurrent socket claim
    //    has already made the durable notification Notified, and the claim
    //    must not discard that stronger receipt.
    let early = take_accepted_early_ack(handle);
    let notified_during_submit_gap = match handle.notification.settle_submitted_claim() {
        Ok(notified) => notified,
        Err(error) => {
            error!(id = %handle.msg_id, error = %error, "notification claim recheck failed");
            return AttemptOutcome::Failed(AttemptFailure::notification_record_failed());
        }
    };
    if notified_during_submit_gap {
        if let Some(early) = early {
            advance_with_early_ack(inner, handle, early);
        } else {
            settle_notification_claim(inner, handle.notification.attempt_id());
        }
        return AttemptOutcome::Done;
    }
    // Test-only pause: a hook arriving now resolves the submitted handle
    // directly instead of installing another early record.
    inject_pause(inner, "post_submit").await;
    if let Some(early) = early {
        match record_notification_notified(handle, Some(VerifiedBy::Hook)) {
            Ok(true) => {}
            Ok(false) => return AttemptOutcome::Done,
            Err(error) => {
                error!(id = %handle.msg_id, error = %error, "notification receipt fact failed");
                return AttemptOutcome::Failed(AttemptFailure::notification_record_failed());
            }
        }
        if advance_with_early_ack(inner, handle, early) {
            return AttemptOutcome::Done;
        }
    }
    let ack_outcome = await_ack(
        inner,
        handle,
        ReceiptWait {
            manifest,
            staged_window: &staged_window,
            id_staged,
            target,
            submit_at: receipt_submit_at,
            events: receipt_events,
            turn_events: receipt_turn_events,
        },
    )
    .await;
    // Test-only pause after receipt observation, before the verdict.
    inject_pause(inner, "post_receipt").await;
    match ack_outcome {
        AckOutcome::Resolved => AttemptOutcome::Done,
        AckOutcome::Screen => {
            // Stays registered: a late matching hook ACK upgrades it to
            // delivered_verified (the legal upgrade transition).
            match record_notification_notified(handle, Some(VerifiedBy::Screen)) {
                Ok(true) => {}
                Ok(false) => return AttemptOutcome::Done,
                Err(error) => {
                    error!(id = %handle.msg_id, error = %error, "notification receipt fact failed");
                    return AttemptOutcome::Failed(AttemptFailure::notification_record_failed());
                }
            }
            let _ = advance(
                inner,
                handle,
                &[DeliveryState::Submitted],
                Step::to(DeliveryState::DeliveredUnverified)
                    .cause("screen_evidence")
                    .verified(VerifiedBy::Screen),
            );
            AttemptOutcome::Done
        }
        AckOutcome::Timeout => settle_without_receipt(inner, handle, "no_receipt"),
        AckOutcome::Rebound => settle_without_receipt(inner, handle, "receipt_occupant_changed"),
    }
}

/// Neither receipt tier answered, or the pane changed hands while waiting.
///
/// Enter reached the admitted occupant, so the notification is recorded as
/// notified with no verifier, and this attempt's composer barrier is
/// released: from here the screen sensor alone decides whether the next
/// doorbell may write. A doorbell left in the composer reads as human input
/// and holds; an unreadable composer does not.
fn settle_without_receipt(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    cause: &'static str,
) -> AttemptOutcome {
    match record_notification_notified(handle, None) {
        Ok(true) => {}
        Ok(false) => return AttemptOutcome::Done,
        Err(error) => {
            error!(id = %handle.msg_id, error = %error, "notification receipt fact failed");
            return AttemptOutcome::Failed(AttemptFailure::notification_record_failed());
        }
    }
    let _ = advance(
        inner,
        handle,
        &[DeliveryState::Submitted],
        Step::to(DeliveryState::DeliveredUnverified).cause(cause),
    );
    gate_line(inner, handle, "unverified", None, Some(cause));
    fusion::clear_hold_owner(
        inner,
        handle.session_idx,
        &handle.pane_id,
        &handle.barrier_owner(),
    );
    AttemptOutcome::Done
}

/// One raw write: the whole rendered message, then Enter, then `Notified`
/// with no verifier. Nothing about the pane is checked beyond its
/// existence, nothing is read back, and no receipt is awaited; the sender
/// asked for exactly that and the journal says so.
pub(crate) async fn attempt_raw_delivery(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
) -> AttemptOutcome {
    let Some(watcher) = watcher_for_handle(inner, handle) else {
        return AttemptOutcome::Failed(AttemptFailure::session_detached());
    };
    let injector = TmuxInjector {
        client: watcher.client(),
        buffer: format!(
            "cyc-{}-{}",
            std::process::id(),
            inner.engine.buffer_seq.fetch_add(1, Ordering::Relaxed)
        ),
    };
    let selected = match select_attempt_payload(handle) {
        Ok(selected) => selected,
        Err(error) => {
            error!(id = %handle.msg_id, error = %error, "raw payload reconstruction failed");
            return AttemptOutcome::Failed(AttemptFailure::payload_unavailable());
        }
    };
    handle.set_attempt_payload(selected.bytes.clone(), Some(selected.transport));
    if let Err(cause) = injector.spool(&selected.bytes).await {
        return AttemptOutcome::Failed(AttemptFailure::from_inject(cause));
    }
    inject_pause(inner, "pre_paste").await;
    if !watcher.pane(&handle.pane_id).is_some_and(|row| !row.dead) {
        injector.discard().await;
        return AttemptOutcome::Failed(AttemptFailure::session_detached());
    }
    // `Writing` without a binding: nothing about the occupant was proven,
    // and the record must not pretend otherwise.
    if let Err(failure) = injector
        .commit(&handle.pane_id, &|| {
            handle
                .notification
                .ensure_current_gating()
                .map_err(notification_write_cause)?;
            handle
                .notification
                .record_writing_raw()
                .map_err(notification_write_cause)?;
            handle.write_boundary_crossed.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await
    {
        return finish_inject_failure(handle, failure, || {});
    }
    if !advance(
        inner,
        handle,
        &[DeliveryState::Pasting],
        Step::to(DeliveryState::Submitted),
    ) {
        return AttemptOutcome::Done;
    }
    inject_pause(inner, "pre_submit").await;
    if let Err(cause) = injector.submit(&handle.pane_id, "Enter").await {
        debug_assert_eq!(cause, "submit_failed");
        return AttemptOutcome::Failed(AttemptFailure::submit_failed());
    }
    match record_notification_notified(handle, None) {
        Ok(true) => {}
        Ok(false) => return AttemptOutcome::Done,
        Err(error) => {
            error!(id = %handle.msg_id, error = %error, "raw notified fact failed");
            return AttemptOutcome::Failed(AttemptFailure::notification_record_failed());
        }
    }
    let _ = advance(
        inner,
        handle,
        &[DeliveryState::Submitted],
        Step::to(DeliveryState::DeliveredUnverified).cause("raw"),
    );
    gate_line(inner, handle, "unverified", None, Some("raw"));
    AttemptOutcome::Done
}

/// Test seam: exit the worker at the synchronous write boundary, after the
/// composer claim and before the first durable transition, for one named
/// attempt. Always inert in production.
fn fail_pre_record_writing_if_requested(inner: &Inner, handle: &DeliveryHandle) {
    let current_attempt = handle.notification.attempt_id();
    // The guard is released before the panic so the recovery run can read
    // the seam again instead of inheriting a poisoned lock.
    let armed = {
        let mut guard = inner.fail_pre_record_writing.lock().unwrap();
        (*guard == Some(current_attempt))
            .then(|| guard.take())
            .is_some()
    };
    if armed {
        panic!(
            "worker exit at synchronous on_write boundary before first durable transition for attempt {current_attempt}"
        );
    }
}

/// Resolve the injector's failure arm for both write paths.
///
/// A paste command tmux provably accepted no byte of corrects the durable
/// boundary back to pre-write (and `release_hold` gives the composer claim
/// back); every other outcome keeps its cause and stays post-write.
pub(crate) fn finish_inject_failure(
    handle: &Arc<DeliveryHandle>,
    failure: InjectFailure,
    release_hold: impl FnOnce(),
) -> AttemptOutcome {
    match failure {
        InjectFailure::PasteCommandUnwritten => {
            if let Err(error) = correct_proven_unwritten_paste(handle) {
                error!(id = %handle.msg_id, error = %error, "notification unwritten correction failed");
                return AttemptOutcome::Failed(AttemptFailure::notification_record_failed());
            }
            release_hold();
            AttemptOutcome::Failed(AttemptFailure::paste_command_unwritten())
        }
        InjectFailure::Other(cause) if cause == NO_LONGER_CURRENT_BEFORE_WRITE => {
            AttemptOutcome::NoLongerCurrentBeforeWrite
        }
        InjectFailure::Other(cause) => AttemptOutcome::Failed(AttemptFailure::from_inject(cause)),
    }
}

/// A pre-paste occupant change is retryable through the gate unless the
/// route itself is gone, which is a durable pre-write block.
fn rebound_before_paste(detail: HoldCause) -> AttemptFailure {
    match detail {
        HoldCause::SessionDetached | HoldCause::NoSuchPane | HoldCause::PaneDead => {
            AttemptFailure::session_detached()
        }
        _ => AttemptFailure::pane_rebound_before_paste(),
    }
}

/// Is the pane still the one the gate admitted: present, alive, out of
/// copy-mode, and running the same agent under the same rules?
///
/// Called once before the paste and once before Enter, by this one
/// function, so the two checks cannot drift apart. The binding compares
/// whole: pids are reusable, and a process that exec'd in place keeps its
/// identity while becoming another program.
pub(crate) fn occupant_unchanged(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    proven: &fusion::Binding,
) -> Result<(), HoldCause> {
    let Some(watcher) = watcher_for_handle(inner, handle) else {
        return Err(HoldCause::SessionDetached);
    };
    let Some(row) = watcher.pane(&handle.pane_id) else {
        return Err(HoldCause::NoSuchPane);
    };
    if row.dead {
        return Err(HoldCause::PaneDead);
    }
    if row.in_mode {
        return Err(HoldCause::PaneInMode);
    }
    if inner
        .fail_next_final_binding_observation
        .swap(false, Ordering::SeqCst)
    {
        return Err(HoldCause::BindingUnprovable);
    }
    match fusion::admitted_binding(inner, handle.session_idx, &row) {
        Some(current) if current == *proven => Ok(()),
        Some(_) => Err(HoldCause::BindingChanged),
        None => Err(HoldCause::BindingUnprovable),
    }
}

// ---------------------------------------------------------------------------
// Gate
// ---------------------------------------------------------------------------

pub(crate) enum GateOutcome {
    Proceed(Admission),
    /// Repeated identical evidence proved this exact mailbox attempt cannot
    /// reach the write boundary. The durable block makes it visible and
    /// operator-withdrawable without touching the pane.
    BlockedPreWrite {
        cause: NotificationPreWriteCause,
        observation: Box<NotificationPreWriteObservation>,
    },
    /// A mailbox notification remains durably queued or gating. The
    /// in-memory worker stops, and the next route or restart reconciliation
    /// can attach a fresh worker without inventing a terminal session fact.
    Deferred {
        cause: String,
    },
    Withdrawn,
}

/// What the gate admitted and what the write may rely on.
pub(crate) enum Admission {
    /// A bound, live agent process. The write re-checks exactly this
    /// binding before the paste and before Enter.
    Doorbell {
        binding: fusion::Binding,
        /// The rule that decided the admitting frame, for the gate line.
        decided_by: String,
    },
    /// The sender asked for a raw write: only the pane's existence was
    /// checked.
    Raw,
}

enum Refusal {
    Hold(HoldCause),
    /// The process table could not prove the occupant. Held once; the same
    /// reading again settles as the durable block carrying this
    /// observation, because "we could not read it" announces no pane event
    /// and would otherwise wait in memory forever.
    Unprovable(Box<NotificationPreWriteObservation>),
    /// Decline keys went out; the pane needs a moment to redraw before it is
    /// read again.
    Declined,
}

/// The gate: hold until the admission path proves the write may happen.
///
/// Event-driven. A hold wakes on fused state and readiness changes, pane
/// field changes, and session reattach; the two causes that announce no
/// event (an unreadable process table, a refused barrier claim) wake once
/// on a bounded timer. A wedged hold pings the admin exactly once after
/// `gate_hold_notify_ms` and keeps waiting. `regate` is the cause an attempt
/// came back with after the write boundary refused it.
pub(crate) async fn gate(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    regate: Option<HoldCause>,
) -> GateOutcome {
    let mut declines: HashMap<String, u32> = HashMap::new();
    let mut regate = regate;
    let mut last_hold: Option<HoldCause> = None;
    let mut hold_since: Option<Instant> = None;
    let mut hold_notified = false;
    // Subscribed once, before the first evaluation, and kept for the gate's
    // whole life: replacing it between evaluations leaves a gap where a
    // readiness edge published after an early pane wake but before the next
    // receiver exists strands a now-clean pane.
    let mut ev_rx = inner.events.subscribe();
    loop {
        let watcher = watcher_for_handle(inner, handle);
        let mut pane_rx = watcher.as_ref().map(|w| w.subscribe());
        match handle.notification.ensure_current_gating() {
            Ok(()) => {}
            Err(NotificationAdapterError::NoLongerCurrentBeforeWrite) => {
                return GateOutcome::Withdrawn;
            }
            Err(error) => {
                error!(id = %handle.msg_id, error = %error, "notification gate recheck failed");
                return GateOutcome::Deferred {
                    cause: NOTIFICATION_RECORD_FAILED.to_string(),
                };
            }
        }
        let refusal = match (regate.take(), &watcher) {
            (Some(cause), _) => Refusal::Hold(cause),
            (None, None) => Refusal::Hold(HoldCause::SessionDetached),
            (None, Some(watcher)) => match admit(inner, handle, watcher, &mut declines).await {
                Ok(admission) => {
                    let rule = match &admission {
                        Admission::Doorbell { decided_by, .. } => Some(decided_by.as_str()),
                        Admission::Raw => None,
                    };
                    gate_line(inner, handle, "proceed", rule, None);
                    return GateOutcome::Proceed(admission);
                }
                Err(refusal) => refusal,
            },
        };
        let cause = match refusal {
            Refusal::Hold(cause) => cause,
            Refusal::Declined => {
                // One-shot settle so the dismissal renders before the
                // re-check; the decline count bounds this loop.
                tokio::time::sleep(DECLINE_SPACING).await;
                continue;
            }
            Refusal::Unprovable(observation) => {
                if last_hold == Some(HoldCause::BindingUnprovable) {
                    return GateOutcome::BlockedPreWrite {
                        cause: NotificationPreWriteCause::BindingUnprovable,
                        observation,
                    };
                }
                HoldCause::BindingUnprovable
            }
        };
        handle.set_hold(Some(cause.receipt_token()));
        if last_hold.as_ref() != Some(&cause) {
            gate_line(inner, handle, "hold", None, Some(&cause.journal()));
            last_hold = Some(cause.clone());
        }
        let since = *hold_since.get_or_insert_with(Instant::now);
        let notify_at = since + Duration::from_millis(inner.cfg.gate_hold_notify_ms);
        let retry_at = match cause {
            HoldCause::BindingUnprovable => Some(Instant::now() + OBSERVATION_RETRY),
            HoldCause::BarrierHeld => Some(Instant::now() + BARRIER_RETRY),
            _ => None,
        };
        tokio::select! {
            _ = wait_pane_change(
                &mut ev_rx,
                pane_rx.as_mut(),
                handle.session_idx,
                &handle.pane_id,
                &handle.cancel,
            ) => {}
            _ = async {
                match retry_at {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {}
            _ = tokio::time::sleep_until(notify_at), if !hold_notified => {
                // A wedged hold must at least be visible. One ping per
                // delivery; the hold itself keeps waiting on events.
                hold_notified = true;
                admin_notify(
                    inner,
                    NotifyLevel::ActionRequired,
                    &format!("notification to {} held in gating", handle.to),
                    &format!(
                        "message {} has been held for over {}ms ({})",
                        handle.msg_id,
                        inner.cfg.gate_hold_notify_ms,
                        cause.journal()
                    ),
                    Some(&handle.msg_id),
                    Some(handle.session_idx),
                    About::pane(&handle.pane_id),
                );
            }
        }
    }
}

/// The admission path, in the order the contract names it.
///
/// Steps 4 and 5 read one fresh capture. The binding is read last, after
/// that capture, so it is the newest fact the write depends on: the composer
/// verdict must not rest on an older answer about whose composer it is.
async fn admit(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    watcher: &Arc<SessionWatcher>,
    declines: &mut HashMap<String, u32>,
) -> Result<Admission, Refusal> {
    // 1. The pane must be present and alive, and not in copy-mode.
    let Some(row) = watcher.pane(&handle.pane_id) else {
        return Err(Refusal::Hold(HoldCause::NoSuchPane));
    };
    if row.dead {
        return Err(Refusal::Hold(HoldCause::PaneDead));
    }
    // A raw write asked for nothing beyond that.
    if handle.raw {
        return Ok(Admission::Raw);
    }
    if row.in_mode {
        return Err(Refusal::Hold(HoldCause::PaneInMode));
    }
    // 2. A manifest must claim the pane, or nothing knows how to read it.
    let Some(manifest) = fusion::bind_manifest_for(inner, handle.session_idx, &row) else {
        return Err(Refusal::Hold(HoldCause::NoManifest));
    };
    let manifest_id = manifest.agent.id.clone();
    // 3. One fresh capture, fused, so the verdict is newer than any human
    //    keystroke round trip.
    let Some(det) = crate::observe_pane(
        inner,
        handle.session_idx,
        watcher,
        &handle.pane_id,
        true,
        "gate",
    )
    .await
    else {
        return Err(Refusal::Hold(HoldCause::NoSuchPane));
    };
    // 4. Named blocks. A modal the manifest can dismiss gets its decline
    //    keys, bounded by MAX_DECLINES; every other block waits for a human.
    match det.state {
        AgentState::Dead => return Err(Refusal::Hold(HoldCause::PaneDead)),
        AgentState::BlockedQuota => return Err(Refusal::Hold(HoldCause::BlockedQuota)),
        AgentState::BlockedModal | AgentState::BlockedPermission => {
            let rule = manifest
                .rules
                .iter()
                .find(|rule| rule.id == det.decided_by && rule.state.is_blocked());
            return Err(match rule {
                Some(rule)
                    if rule.auto_dismiss
                        && !rule.decline_keys.is_empty()
                        && *declines.get(&rule.id).unwrap_or(&0) < MAX_DECLINES =>
                {
                    *declines.entry(rule.id.clone()).or_insert(0) += 1;
                    gate_line(inner, handle, "decline", Some(&rule.id), None);
                    if !send_decline_keys(
                        watcher,
                        &handle.pane_id,
                        manifest,
                        &rule.id,
                        &rule.decline_keys,
                    )
                    .await
                    {
                        // The screen changed under the decline: the
                        // confirming key was withheld.
                        gate_line(
                            inner,
                            handle,
                            "decline_aborted",
                            Some(&rule.id),
                            Some("modal_changed"),
                        );
                    }
                    Refusal::Declined
                }
                _ => Refusal::Hold(HoldCause::Blocked(
                    rule.map(|rule| rule.id.clone())
                        .unwrap_or_else(|| det.decided_by.clone()),
                )),
            });
        }
        _ => {}
    }
    // 5. The composer. Only a positively observed human draft, or a hold a
    //    delivery owns (a doorbell staged and not consumed, or the turn it
    //    started that has not ended), holds. An unreadable or ambiguous
    //    composer does not; what it costs is a line the journal records as
    //    unverified.
    if fusion::composer_is_held(inner, handle.session_idx, &handle.pane_id) {
        return Err(Refusal::Hold(HoldCause::ComposerHold));
    }
    // 6. The binding: the agent this manifest describes must be the pane's
    //    foreground process or an ancestor of it. The foreground process is
    //    where the keystrokes land, so a tool the agent handed the terminal
    //    to is admitted only while the fused screen positively reads as the
    //    agent (an agent that runs its composer in a child process, as the
    //    parity fixture does); an unrecognized screen in front of a live
    //    agent holds. Falling back to the pane root would pin the delivery
    //    to the SHELL and resolve receipts against whoever sits at that
    //    prompt next, so an unreadable process table is a hold.
    match fusion::admitted_binding(inner, handle.session_idx, &row) {
        Some(binding)
            if binding.manifest == manifest_id
                && (binding.leader == binding.agent || det.state != AgentState::Unknown) =>
        {
            Ok(Admission::Doorbell {
                binding,
                decided_by: det.decided_by,
            })
        }
        Some(binding) if binding.manifest == manifest_id => {
            Err(Refusal::Hold(HoldCause::ForegroundNotAgent))
        }
        _ => Err(Refusal::Unprovable(Box::new(
            binding_unprovable_observation(inner, handle, row.pane_pid, &manifest_id),
        ))),
    }
}

/// Take only receipt evidence that can settle the submitted delivery.
pub(crate) fn take_accepted_early_ack(handle: &DeliveryHandle) -> Option<PendingAck> {
    let mut state = handle.state.lock().expect("handle state lock");
    match state.early_ack.as_ref().map(|ack| ack.evidence) {
        Some(PendingAckEvidence::Receipt | PendingAckEvidence::DispatchAccepted) => {
            state.early_ack.take()
        }
        Some(PendingAckEvidence::DispatchPending) | None => None,
    }
}

/// Preserve exact hook receipt and TurnKey evidence across claim settlement.
pub(crate) fn advance_with_early_ack(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    early: PendingAck,
) -> bool {
    advance(
        inner,
        handle,
        &[DeliveryState::Submitted],
        early_ack_step(early),
    )
}

pub(crate) fn early_ack_step(early: PendingAck) -> Step<'static> {
    let cause = match early.evidence {
        PendingAckEvidence::Receipt => "hook_ack",
        PendingAckEvidence::DispatchAccepted => "hook_dispatch_accepted_start",
        PendingAckEvidence::DispatchPending => unreachable!("pending dispatch was not taken"),
    };
    Step::to(DeliveryState::DeliveredVerified)
        .cause(cause)
        .verified(VerifiedBy::Hook)
        .turn_edge(early.edge_ms)
        .turn(early.turn)
}

/// Is the pane still held by the process and rules that Enter reached?
///
/// Receipt evidence answers "did the message land", and it can only
/// answer that about the occupant it was sent to. A pane id is reusable,
/// so a replacement process can clear the marker, change the window, emit
/// output, and even fire a hook carrying the old message id. None of that
/// is evidence about the delivery, and treating it as evidence is how a
/// record starts to lie.
pub(crate) fn submitted_binding_holds(
    inner: &Arc<Inner>,
    watcher: &Arc<SessionWatcher>,
    handle: &Arc<DeliveryHandle>,
) -> bool {
    let want_agent = *handle.submitted_agent.lock().expect("submitted agent lock");
    let want_manifest = handle
        .submitted_manifest
        .lock()
        .expect("submitted manifest lock")
        .clone();
    let (Some(want_agent), Some(want_manifest), Some(row)) =
        (want_agent, want_manifest, watcher.pane(&handle.pane_id))
    else {
        return false;
    };
    if row.dead {
        return false;
    }
    // The foreground leader is allowed to change after Enter. An agent can
    // hand the terminal to a tool or take it back without changing who owns
    // the delivery. Re-prove the admitted agent instance and its rules.
    fusion::admitted_binding(inner, handle.session_idx, &row)
        .is_some_and(|binding| binding.agent == want_agent && binding.manifest == want_manifest)
}

pub(crate) fn process_instance(pid: i32) -> Option<ProcessInstanceId> {
    let process = crate::identity::ProcId::of(pid)?;
    process_instance_id(process)
}

pub(crate) fn process_instance_id(process: crate::identity::ProcId) -> Option<ProcessInstanceId> {
    ProcessInstanceId::new(process.pid, process.birth).ok()
}

pub(crate) fn binding_unprovable_observation(
    inner: &Inner,
    handle: &DeliveryHandle,
    pane_pid: i32,
    manifest_id: &str,
) -> NotificationPreWriteObservation {
    NotificationPreWriteObservation {
        pane_root: process_instance(pane_pid),
        selected_manifest: NotificationManifestId::new(manifest_id).ok(),
        binding: None,
        route_evidence: Some(inner.route_evidence_id(handle.session_idx, &handle.pane_id)),
        pane_width: None,
        required_pane_width: None,
        write_block: None,
    }
}

/// Await the test-only injection pause, when one is installed. Production
/// never installs one; this is a no-op there.
pub(crate) async fn inject_pause(inner: &Arc<Inner>, phase: &'static str) {
    let hook = inner
        .inject_pause
        .lock()
        .expect("inject pause lock")
        .clone();
    if let Some(h) = hook {
        h(phase).await;
    }
}

/// How long an unreadable process table waits before it is read again.
/// Short enough that a transient `ps` failure costs a person nothing, long
/// enough that a permanently unreadable table is not a spin.
pub(crate) const OBSERVATION_RETRY: Duration = Duration::from_millis(250);
/// A refused barrier claim is a race with another attempt's release, which
/// broadcasts readiness; this bounds the wait if that broadcast was missed.
pub(crate) const BARRIER_RETRY: Duration = Duration::from_millis(50);

/// Retry accounting. Only failures proven to precede the pane write may
/// consume the configured retry budget. True means the caller should retry
/// immediately. False means the attempt remains durably held or blocked for
/// recovery, or ended in attention.
pub(crate) async fn fail_attempt(
    inner: &Arc<Inner>,
    worker: &Worker,
    handle: &Arc<DeliveryHandle>,
    failure: &AttemptFailure,
) -> bool {
    // What the budget has actually spent: attempts that reached the
    // transport, not attempts that stopped at a refused barrier.
    let spent = {
        let st = handle.state.lock().expect("handle state lock");
        st.attempts.saturating_sub(st.regates)
    };
    let from = [
        DeliveryState::Pasting,
        DeliveryState::Staged,
        DeliveryState::Submitted,
        DeliveryState::RetryQueued,
    ];
    if should_retry(failure, spent, inner.cfg.delivery_retry_max) {
        advance(
            inner,
            handle,
            &from,
            Step::to(DeliveryState::RetryQueued).cause(&failure.cause),
        )
    } else {
        if let Some(block) = failure.pre_write_block.as_deref() {
            persist_notification_prewrite_block(
                inner,
                worker,
                handle,
                block.cause,
                block.observation.clone(),
            )
            .await;
            return false;
        }
        if matches!(failure.boundary, WriteBoundary::BeforeWrite) {
            // The workspace attempt remains Gating. No pane write happened,
            // so a terminal notification would be false. A later route event
            // or daemon restart can attach a fresh worker to the same
            // durable attempt.
            notify_notification_deferred(inner, handle, &failure.cause);
            return false;
        }
        let notification = &handle.notification;
        match notification.record_attention(notification_attention_cause(&failure.cause)) {
            Ok(_) => {}
            Err(NotificationAdapterError::TerminalConflict(_)) => return false,
            Err(error) => {
                // The workspace journal remains at its last post-write
                // state. Explicit restart recovery can close it without
                // risking another pane write. Faulting the worker is what
                // makes that visible: the fault reaches
                // `notification_worker_diagnostics` and so `cyclops
                // status`, where an operator learns that this daemon needs
                // a restart. Without it the attempt would stay in flight
                // with no alarm, no wake block, and a recipient head that
                // never advances.
                error!(id = %handle.msg_id, error = %error, "notification attention fact failed");
                worker.set_fault(format!("notification attention storage failed: {error}"));
            }
        }
        advance(
            inner,
            handle,
            &from,
            Step::to(DeliveryState::AttentionRequired).cause(&failure.cause),
        );
        false
    }
}

pub(crate) fn notify_notification_prewrite_blocked(
    inner: &Arc<Inner>,
    handle: &DeliveryHandle,
    block: &MessagingPreWriteBlock,
) {
    let cause = serde_json::to_value(block.cause)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    admin_notify(
        inner,
        NotifyLevel::ActionRequired,
        &format!("notification to {} blocked before write", handle.to),
        &format!(
            "message {} attempt {}: {cause}; the mailbox remains claimable",
            handle.msg_id, block.attempt_id
        ),
        Some(&handle.msg_id),
        Some(handle.session_idx),
        About::pane(&handle.pane_id),
    );
}

pub(crate) fn notification_attention_cause(cause: &str) -> NotificationAttentionCause {
    match cause {
        "paste_failed" => NotificationAttentionCause::PasteFailed,
        "pane_rebound_after_paste" => NotificationAttentionCause::PaneReboundAfterPaste,
        "submit_failed" => NotificationAttentionCause::SubmitFailed,
        _ => NotificationAttentionCause::TransportOutcomeUnknown,
    }
}

pub(crate) fn should_retry(failure: &AttemptFailure, spent: u32, retry_max: u32) -> bool {
    // The attempt already has a durable Writing fact. Its exact zero-byte
    // correction must remain withdrawable instead of being replayed
    // automatically.
    if failure.cause == "paste_command_unwritten" {
        return false;
    }
    matches!(failure.boundary, WriteBoundary::BeforeWrite) && spent <= retry_max
}

/// Report a pre-write notification stall without inventing a terminal fact.
/// The payload and composer capture remain outside both the ping and logs.
pub(crate) fn notify_notification_deferred(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    cause: &str,
) {
    admin_notify(
        inner,
        NotifyLevel::Fyi,
        "notification remains queued before write",
        &format!(
            "message {} to {} remains queued or gating ({cause})",
            handle.msg_id, handle.to
        ),
        None,
        None,
        About::default(),
    );
}

/// Manifest decline keys, in order, with spacing. The keys come from the
/// manifest rule, never a generic Enter/Escape.
///
/// TOCTOU guard: before the FINAL confirming key of a multi-key sequence
/// the screen is re-captured, and the same modal rule must still be the
/// winning match. A dialog that vanished or changed between keys (the
/// human answered it, the app redrew) must not receive the confirm.
/// Returns false when the sequence was aborted.
pub(crate) async fn send_decline_keys(
    watcher: &Arc<SessionWatcher>,
    pane_id: &str,
    manifest: &Manifest,
    rule_id: &str,
    keys: &[String],
) -> bool {
    for (i, key) in keys.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(DECLINE_SPACING).await;
        }
        if i > 0 && i == keys.len() - 1 {
            let title = watcher.pane(pane_id).map(|r| r.title).unwrap_or_default();
            let screen = match watcher.client().capture_pane(pane_id).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(pane = pane_id, error = %e, "decline recheck capture failed");
                    return false;
                }
            };
            if !modal_still_matches(manifest, &title, &screen, rule_id) {
                return false;
            }
        }
        if let Err(e) = watcher.client().send_keys(pane_id, &[key.as_str()]).await {
            warn!(pane = pane_id, error = %e, "decline key failed");
            return true; // sent what we could; not a TOCTOU abort
        }
    }
    true
}

/// True while `rule_id` is still the winning match for this screen.
pub(crate) fn modal_still_matches(
    manifest: &Manifest,
    title: &str,
    screen: &str,
    rule_id: &str,
) -> bool {
    manifest
        .evaluate(title, screen)
        .is_some_and(|r| r.id == rule_id && r.state.is_blocked())
}

/// Mark a pane as holding text, without waiting for a sensor to see it.
///
/// Used after our own paste lands. A hold set here releases through the
/// same turn lifecycle as one a sensor raised: nothing about it is
/// special except that the evidence came from having done the write.
pub(crate) fn latch_hold(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    proven: &fusion::Binding,
) -> Result<(), String> {
    // A claim, not a set. The cached verdict and the readiness wake move
    // with it, so a pane whose composer holds our payload stops reporting
    // itself writable to anyone who asks between now and the next
    // recompute; and the claim names THIS attempt, so evidence arriving
    // late for an earlier delivery cannot settle this barrier.
    let owner = handle.barrier_owner();
    if fusion::claim_hold(
        inner,
        handle.session_idx,
        &handle.pane_id,
        &owner,
        Some(proven.agent),
        Some(proven.manifest.as_str()),
    ) {
        handle.state.lock().expect("handle state lock").barrier = Some(owner);
        Ok(())
    } else {
        Err("barrier_held".to_string())
    }
}

pub(crate) fn rollback_unwritten_hold(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    proven: &fusion::Binding,
) {
    let owner = handle
        .state
        .lock()
        .expect("handle state lock")
        .barrier
        .clone();
    let Some(owner) = owner else {
        return;
    };
    if fusion::release_unwritten_hold(
        inner,
        handle.session_idx,
        &handle.pane_id,
        &owner,
        proven.agent,
        &proven.manifest,
    ) {
        handle.state.lock().expect("handle state lock").barrier = None;
    }
}

/// Correct the provisional boundary after tmux proves that its command pipe
/// accepted no byte of the paste command.
///
/// The durable correction must succeed before the runtime boundary is cleared.
/// Otherwise restart recovery must continue treating the attempt as post-write.
pub(crate) fn correct_proven_unwritten_paste(
    handle: &DeliveryHandle,
) -> Result<(), NotificationAdapterError> {
    handle.notification.record_paste_command_unwritten()?;
    handle.write_boundary_crossed.store(false, Ordering::SeqCst);
    Ok(())
}

/// Releases a claimed composer barrier if the synchronous boundary hook unwinds.
pub(crate) struct UnwrittenHold<'a> {
    pub(crate) inner: &'a Arc<Inner>,
    pub(crate) handle: &'a Arc<DeliveryHandle>,
    pub(crate) binding: &'a fusion::Binding,
    pub(crate) armed: bool,
}

impl<'a> UnwrittenHold<'a> {
    pub(crate) fn new(
        inner: &'a Arc<Inner>,
        handle: &'a Arc<DeliveryHandle>,
        binding: &'a fusion::Binding,
    ) -> Self {
        Self {
            inner,
            handle,
            binding,
            armed: true,
        }
    }

    pub(crate) fn commit(&mut self) {
        self.armed = false;
    }
}

impl Drop for UnwrittenHold<'_> {
    fn drop(&mut self) {
        if self.armed {
            rollback_unwritten_hold(self.inner, self.handle, self.binding);
        }
    }
}

pub(crate) fn notification_write_cause(error: NotificationAdapterError) -> String {
    match error {
        NotificationAdapterError::NoLongerCurrentBeforeWrite => {
            NO_LONGER_CURRENT_BEFORE_WRITE.to_string()
        }
        other => {
            error!(error = %other, "notification write fact failed");
            NOTIFICATION_RECORD_FAILED.to_string()
        }
    }
}

/// Record a receipt before the in-memory delivery state claims it.
///
/// False means the notification already resolved the other way in a race.
pub(crate) fn record_notification_notified(
    handle: &Arc<DeliveryHandle>,
    verified_by: Option<VerifiedBy>,
) -> Result<bool, NotificationAdapterError> {
    match handle.notification.record_notified(verified_by) {
        Ok(_) => Ok(true),
        Err(NotificationAdapterError::TerminalConflict(_)) => Ok(false),
        Err(error) => Err(error),
    }
}

/// What a receipt says about the turn that consumed our payload.
///
/// A hook acknowledgement can name both the exact payload and a TurnKey.
/// That key binds the hold to one turn, which releases only when the same
/// key reports an end and the screen is clean. Arrival timestamps never
/// substitute for that match.
///
/// A hook acknowledgement without a TurnKey, or a screen receipt, proves
/// consumption but selects the screen lifecycle. Its timestamp is retained
/// for diagnosis only.
pub(crate) fn settle_hold_on_receipt(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    verified_by: Option<VerifiedBy>,
    turn_edge_ms: Option<u64>,
    turn: Option<crate::turnkey::TurnKey>,
) {
    // A receipt only ever settles ITS OWN barrier: see `set_hold_owned`.
    // A delivery that never claimed one has none to settle. Its receipt
    // still resolves the delivery; it just says nothing about whatever
    // this pane's composer is holding for somebody else.
    let Some(owner) = handle
        .state
        .lock()
        .expect("handle state lock")
        .barrier
        .clone()
    else {
        return;
    };
    if verified_by == Some(VerifiedBy::Hook) {
        let since_ms = turn_edge_ms.unwrap_or_else(unix_ms);
        match turn {
            // The vendor named the turn that took this payload, so the
            // hold binds to it and joins the exact lifecycle: only that
            // turn's own end can end it.
            Some(turn) => {
                let _ = fusion::bind_turn(
                    inner,
                    handle.session_idx,
                    &handle.pane_id,
                    &owner,
                    turn,
                    since_ms,
                );
            }
            // A vendor that names no turns still acknowledges. The hold
            // stays on the screen lifecycle and carries the observed edge
            // only for diagnosis. Only a hold still waiting takes it.
            None => {
                fusion::set_hold_owned(
                    inner,
                    handle.session_idx,
                    &handle.pane_id,
                    &owner,
                    |hold| {
                        hold.is_waiting()
                            .then_some(cyclops_proto::ComposerHold::TurnStarted { since_ms })
                    },
                );
            }
        }
        return;
    }
    // A screen receipt names no turn, so it promotes this hold to the
    // screen lane. Consumption is proven, and the submit time is retained
    // for diagnosis. Reading the lane from the manifest instead would
    // leave a keyed vendor whose
    // hook was never installed holding a barrier forever, waiting on an
    // exact end that nobody is going to send. A matching ACK arriving
    // late can still upgrade this same owner to the exact lane.
    latch_turn_started(
        inner,
        handle.session_idx,
        &handle.pane_id,
        &owner,
        handle.submitted_at_ms.load(Ordering::SeqCst),
    );
}

/// Reconcile an authenticated claim with a doorbell whose submit succeeded.
///
/// A claim while the delivery is still staged proves only mailbox retrieval.
/// It wakes the worker so that worker can reserve submit or clear the exact
/// staged bytes. It never creates turn evidence.
pub(crate) fn settle_notification_claim(
    inner: &Arc<Inner>,
    attempt_id: NotificationAttemptId,
) -> bool {
    let Some(handle) = inner.engine.notification_handle(attempt_id) else {
        return false;
    };
    if handle.notification_transport() != Some(NotificationTransport::Doorbell) {
        return false;
    }
    let settled = advance(
        inner,
        &handle,
        &[DeliveryState::Submitted],
        Step::to(DeliveryState::DeliveredUnverified).cause("mailbox_claim"),
    );
    if settled
        || matches!(
            handle.state(),
            DeliveryState::DeliveredVerified | DeliveryState::DeliveredUnverified
        )
    {
        fusion::clear_hold_owner(
            inner,
            handle.session_idx,
            &handle.pane_id,
            &handle.barrier_owner(),
        );
        handle.ack.notify_one();
        return true;
    }
    if handle.state() == DeliveryState::Staged {
        handle.cancel.notify_one();
        return true;
    }
    false
}

pub(crate) fn latch_turn_started(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    owner: &str,
    since_ms: u64,
) -> bool {
    // An existing mark is never replaced. It records the first observed
    // turn or the submit boundary for the screen lifecycle; it does not
    // correlate a turn end. Only a manifest-declared TurnKey can do that.
    // `StagedDuringTurn` counts as waiting: a turn already running
    // cannot consume a person's draft, but this payload is one Cyclops
    // wrote and submitted itself, and the receipt names the turn that
    // took it.
    fusion::set_hold_owned(inner, session_idx, pane_id, owner, |hold| {
        hold.is_waiting()
            .then_some(cyclops_proto::ComposerHold::TurnStarted { since_ms })
    })
}

/// Block until an event that could change the gate verdict for this pane:
/// a fused state change, a session attach/detach, or a pane field change
/// (mode, death, title, command). Lag counts as doubt and wakes too.
pub(crate) async fn wait_pane_change(
    ev_rx: &mut broadcast::Receiver<Event>,
    pane_rx: Option<&mut broadcast::Receiver<PaneEvent>>,
    session_idx: usize,
    pane_id: &str,
    cancel: &Notify,
) -> bool {
    match pane_rx {
        Some(prx) => {
            let mut event_open = true;
            let mut pane_open = true;
            loop {
                tokio::select! {
                    ev = ev_rx.recv(), if event_open => match ev {
                        Ok(event) => if let Some(exact) = event_wake(&event, session_idx, pane_id) {
                            return exact;
                        },
                        Err(broadcast::error::RecvError::Lagged(_)) => return false,
                        Err(broadcast::error::RecvError::Closed) => event_open = false,
                    },
                    pe = prx.recv(), if pane_open => match pe {
                        Ok(event) => if let Some(exact) = pane_event_wake(&event, pane_id) {
                            return exact;
                        },
                        Err(broadcast::error::RecvError::Lagged(_)) => return false,
                        Err(broadcast::error::RecvError::Closed) => pane_open = false,
                    },
                    _ = cancel.notified() => return false,
                }
            }
        }
        None => {
            let mut event_open = true;
            loop {
                tokio::select! {
                    ev = ev_rx.recv(), if event_open => match ev {
                        Ok(event) => if let Some(exact) = event_wake(&event, session_idx, pane_id) {
                            return exact;
                        },
                        Err(broadcast::error::RecvError::Lagged(_)) => return false,
                        Err(broadcast::error::RecvError::Closed) => event_open = false,
                    },
                    _ = cancel.notified() => return false,
                }
            }
        }
    }
}

pub(crate) fn event_wake(event: &Event, session_idx: usize, pane_id: &str) -> Option<bool> {
    match event.event.as_str() {
        // A readiness change with no state change is exactly the
        // shape of a hold lifting, and it is the whole reason this
        // arm exists: without it a delivery sleeps through its own
        // release.
        "state" | "readiness" => (event.data["pane_id"] == pane_id
            && event.data["session_idx"] == session_idx)
            .then_some(true),
        "session" => Some(false),
        _ => None,
    }
}

pub(crate) fn pane_event_wake(event: &PaneEvent, pane_id: &str) -> Option<bool> {
    match event {
        PaneEvent::PaneChanged { id, .. } => (id == pane_id).then_some(true),
        PaneEvent::PaneRemoved(id) => (id == pane_id).then_some(true),
        PaneEvent::Disconnected => Some(false),
        _ => None,
    }
}

pub(crate) enum AckOutcome {
    /// The matcher resolved it (delivered_verified is on the handle).
    Resolved,
    /// The pane changed hands after Enter, so no later evidence belongs to
    /// this delivery.
    Rebound,
    /// Screen evidence: marker left the composer and the pane moved.
    Screen,
    /// Neither tier inside the deadline.
    Timeout,
}

/// Outcome of one tier-2 evidence pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Evidence {
    /// The conjunctive rule held: marker gone plus turn evidence.
    Confirmed,
    /// The pane was observed and the evidence is not there (yet).
    Absent,
    /// Nobody looked: the watcher is gone (a detach can clear it before
    /// the lifecycle event is broadcast) or the capture failed. Doubt,
    /// never expiry, mirroring fusion's capture-failure handling.
    Unobservable,
    /// The pane changed hands after the submit key. Whatever is on screen
    /// now belongs to somebody else, so it can neither confirm nor deny
    /// this delivery, and waiting longer cannot fix that.
    Rebound,
}

/// What one checkpoint pass means for the ACK loop. Expiry may stand only
/// on a pass that actually looked and saw nothing; doubt freezes the clock
/// until observability returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckpointStep {
    Deliver,
    Rebound,
    Freeze,
    Expire,
    Wait,
}

pub(crate) fn checkpoint_step(evidence: Evidence, expired: bool) -> CheckpointStep {
    match evidence {
        Evidence::Confirmed => CheckpointStep::Deliver,
        // Its own outcome, deliberately: folding it into expiry would
        // record an ack timeout for a pane that changed hands, which is a
        // different fact and a worse one to leave in the ledger.
        Evidence::Rebound => CheckpointStep::Rebound,
        Evidence::Unobservable => CheckpointStep::Freeze,
        Evidence::Absent if expired => CheckpointStep::Expire,
        Evidence::Absent => CheckpointStep::Wait,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReceiptRefresh {
    Observe,
    Resolved,
    Freeze,
    Rebound,
}

/// Classify the stable fusion refresh that precedes every receipt check.
///
/// A missing watcher is a detach and freezes time. A live watcher that no
/// longer has the submitted pane proves a rebound. Once a pane is observed,
/// only stale screen evidence, pane mode, or an unprovable occupant freezes
/// the clock. Other refusals remain observable facts and must not stop the
/// receipt ladder.
pub(crate) fn receipt_refresh_step(
    watcher_live: bool,
    detection: Option<&Detection>,
    resolved: bool,
) -> ReceiptRefresh {
    if resolved {
        return ReceiptRefresh::Resolved;
    }
    if !watcher_live {
        return ReceiptRefresh::Freeze;
    }
    let Some(detection) = detection else {
        return ReceiptRefresh::Rebound;
    };
    if detection.state == AgentState::Dead {
        return ReceiptRefresh::Rebound;
    }
    if detection.stale
        || matches!(
            detection.write_block.as_deref(),
            Some("pane_in_mode" | "occupant_unprovable")
        )
    {
        ReceiptRefresh::Freeze
    } else {
        ReceiptRefresh::Observe
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReceiptStep {
    Resolved,
    Deliver,
    Rebound,
    Freeze,
    Expire,
    Wait,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReceiptPaneStep {
    Ignore,
    Recheck,
    Rebound,
    Freeze,
}

pub(crate) fn receipt_pane_step(
    event: &Result<PaneEvent, broadcast::error::RecvError>,
    pane_id: &str,
    frozen: bool,
) -> ReceiptPaneStep {
    match event {
        Ok(PaneEvent::PaneRemoved(id)) if id == pane_id => ReceiptPaneStep::Rebound,
        Ok(PaneEvent::PaneChanged { id, row, .. }) if id == pane_id => {
            if row.dead {
                ReceiptPaneStep::Rebound
            } else {
                ReceiptPaneStep::Recheck
            }
        }
        Ok(PaneEvent::OutputActivity { pane_id: id, .. }) if id == pane_id && frozen => {
            ReceiptPaneStep::Recheck
        }
        Ok(PaneEvent::Disconnected) | Err(broadcast::error::RecvError::Closed) => {
            ReceiptPaneStep::Freeze
        }
        _ => ReceiptPaneStep::Ignore,
    }
}

/// The per-delivery ACK timeline: the tier-1 hook window, the tier-2
/// screen-evidence checkpoints, and the give-up deadline.
///
/// While the session's control connection is down, the daemon cannot observe
/// the pane, so the clock freezes. On reattach every remaining instant shifts
/// by the outage duration. Time lost to a detach never counts against an
/// acknowledgment window.
pub(crate) struct AckClock {
    /// End of the tier-1 hook phase; None once the phase ended (or for
    /// screen-tier agents that never had one).
    pub(crate) hook_deadline: Option<Instant>,
    pub(crate) checkpoints: Vec<Instant>,
    pub(crate) next: usize,
    pub(crate) deadline: Instant,
    pub(crate) frozen_at: Option<Instant>,
}

impl AckClock {
    pub(crate) fn new(submit_at: Instant, hook_window: Option<Duration>) -> AckClock {
        AckClock {
            hook_deadline: hook_window.map(|w| submit_at + w),
            checkpoints: ACK_CHECKPOINTS_MS
                .iter()
                .map(|ms| submit_at + Duration::from_millis(*ms))
                .collect(),
            next: 0,
            deadline: submit_at + SCREEN_ACK_DEADLINE,
            frozen_at: None,
        }
    }

    pub(crate) fn frozen(&self) -> bool {
        self.frozen_at.is_some()
    }

    pub(crate) fn freeze(&mut self, now: Instant) {
        if self.frozen_at.is_none() {
            self.frozen_at = Some(now);
        }
    }

    /// Reattach: shift every remaining instant by the detach duration.
    pub(crate) fn unfreeze(&mut self, now: Instant) {
        let Some(at) = self.frozen_at.take() else {
            return;
        };
        let lost = now.saturating_duration_since(at);
        if let Some(h) = &mut self.hook_deadline {
            *h += lost;
        }
        for c in &mut self.checkpoints[self.next..] {
            *c += lost;
        }
        self.deadline += lost;
    }

    /// Next timer to arm: (instant, is_hook_phase_end). None while frozen;
    /// a frozen clock never fires.
    pub(crate) fn next_target(&self) -> Option<(Instant, bool)> {
        if self.frozen() {
            return None;
        }
        if let Some(h) = self.hook_deadline {
            return Some((h, true));
        }
        Some((
            self.checkpoints
                .get(self.next)
                .copied()
                .unwrap_or(self.deadline),
            false,
        ))
    }

    /// The tier-1 phase ended, so tier 2 opens here: one pass now, then the
    /// checkpoints the hook window did not cover.
    ///
    /// The passes inside the window are dropped rather than replayed. None
    /// of them ran (the hook deadline is the only armed timer while the
    /// phase lasts), and three captures of one screen in the same instant
    /// answer the same question three times.
    ///
    /// The pass AT `now` is the one that matters, and leaving it out is
    /// what shipped. MEASURED at the defaults: submit at +20ms, hook window
    /// closes at +1520ms, and the next unexpired checkpoint is submit+3000,
    /// so nothing looked at a pane that had held the evidence since +20ms
    /// for a second and a half. receipt_block_ms (2500) expires inside that
    /// hole, which is why every send to an agent whose hooks are not wired
    /// printed "queued" and no delivery badge ever reached the sender.
    pub(crate) fn end_hook_phase(&mut self, now: Instant) {
        self.hook_deadline = None;
        while self.next < self.checkpoints.len() && self.checkpoints[self.next] <= now {
            self.next += 1;
        }
        self.checkpoints.insert(self.next, now);
    }

    pub(crate) fn advance_checkpoint(&mut self) {
        self.next += 1;
    }

    pub(crate) fn expired(&self, now: Instant) -> bool {
        !self.frozen() && now >= self.deadline
    }
}

/// Receive from an optional pane-event stream; pends forever when the
/// session is detached (no watcher, no stream).
pub(crate) async fn recv_pane(
    rx: &mut Option<broadcast::Receiver<PaneEvent>>,
) -> Result<PaneEvent, broadcast::error::RecvError> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

/// Tier 1: the manifest hook ACK inside ack_timeout_ms. Tier 2: screen
/// evidence until the deadline, checked on pane events and bounded
/// one-shot checkpoints. A hook ACK is accepted at any point.
///
/// The clock freezes across a session detach and a reattach runs an
/// immediate evidence pass BEFORE any deadline can expire, so a delivery
/// that landed during the outage resolves as delivered instead of being
/// resubmitted (the m1 soak's duplicate). Hook ACKs arriving during the
/// outage are accepted by the matcher independently of this loop.
pub(crate) struct ReceiptWait<'a> {
    pub(crate) manifest: &'a Manifest,
    pub(crate) staged_window: &'a str,
    pub(crate) id_staged: bool,
    pub(crate) target: StagingTarget<'a>,
    pub(crate) submit_at: Instant,
    pub(crate) events: broadcast::Receiver<Event>,
    pub(crate) turn_events: broadcast::Receiver<Event>,
}

pub(crate) fn receipt_is_resolved(handle: &DeliveryHandle) -> bool {
    matches!(
        handle.state(),
        DeliveryState::DeliveredVerified | DeliveryState::DeliveredUnverified
    )
}

/// Preserve the tier-1 diagnostic window when screen evidence resolves first.
/// A hook arriving before `deadline` records liveness and suppresses the ping.
pub(crate) fn schedule_missing_hook_diagnostic(
    inner: &Arc<Inner>,
    handle: &DeliveryHandle,
    manifest_id: &str,
    deadline: Option<Instant>,
) {
    let Some(deadline) = deadline else {
        return;
    };
    let Some(agent) = *handle.submitted_agent.lock().expect("submitted agent lock") else {
        return;
    };
    let pane = PaneKey::new(handle.session_idx, &handle.pane_id);
    let Some(binding) = inner.hook_liveness.binding(&pane, agent, manifest_id) else {
        return;
    };
    let task_inner = Arc::clone(inner);
    let msg_id = handle.msg_id.clone();
    let to = handle.to.clone();
    let mut stop = inner.stop.clone();
    inner.engine.spawn_descendant_task(async move {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                crate::selftest::notify_f1_once(&task_inner, &msg_id, &to, binding);
            }
            _ = stop.changed() => {}
        }
    });
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn receipt_checkpoint_pass(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    manifest: &Manifest,
    staged_window: &str,
    id_staged: bool,
    target: StagingTarget<'_>,
    working_seen: bool,
    turn_events: &mut broadcast::Receiver<Event>,
    output_seen: bool,
    clock: &AckClock,
) -> ReceiptStep {
    // `turn_events` subscribed before Enter. A screen checkpoint can become
    // ready at the same instant as the state event, so account for every
    // already-buffered matching edge before that checkpoint is allowed to
    // settle the delivery. This is a fact recorder only; lifecycle handling
    // remains on the main receipt event stream.
    let working_seen = working_seen
        || handle.working_seen.load(Ordering::SeqCst)
        || record_buffered_working_evidence(turn_events, handle);
    let Some(watcher) = inner.watcher_of(handle.session_idx) else {
        return ReceiptStep::Freeze;
    };
    let detection = crate::observe_pane(
        inner,
        handle.session_idx,
        &watcher,
        &handle.pane_id,
        true,
        "receipt_checkpoint",
    )
    .await;
    let same_watcher = inner
        .watcher_of(handle.session_idx)
        .is_some_and(|current| Arc::ptr_eq(&current, &watcher));
    match receipt_refresh_step(
        same_watcher,
        detection.as_ref(),
        receipt_is_resolved(handle),
    ) {
        ReceiptRefresh::Resolved => ReceiptStep::Resolved,
        ReceiptRefresh::Freeze => ReceiptStep::Freeze,
        ReceiptRefresh::Rebound => ReceiptStep::Rebound,
        ReceiptRefresh::Observe => {
            match checkpoint_step(
                screen_evidence(
                    inner,
                    handle,
                    manifest,
                    staged_window,
                    id_staged,
                    target,
                    working_seen,
                    output_seen,
                )
                .await,
                clock.expired(Instant::now()),
            ) {
                CheckpointStep::Deliver => ReceiptStep::Deliver,
                CheckpointStep::Rebound => ReceiptStep::Rebound,
                CheckpointStep::Freeze => ReceiptStep::Freeze,
                CheckpointStep::Expire => ReceiptStep::Expire,
                CheckpointStep::Wait => ReceiptStep::Wait,
            }
        }
    }
}

/// Latch a matching working edge from the backlog present when this is called.
/// The scan is bounded by that fixed backlog, not an arbitrary count. A
/// receipt's main event stream handles later receipt lifecycle, while a
/// composed wait has already opened a fresh stream for later fused state.
///
/// If the receiver has lagged, this records no fact. Missing a turn can only
/// make `turn_ended` time out; inventing one would be a false success.
pub(crate) fn record_buffered_working_evidence(
    events: &mut broadcast::Receiver<Event>,
    handle: &Arc<DeliveryHandle>,
) -> bool {
    let backlog = events.len();
    for _ in 0..backlog {
        match events.try_recv() {
            Ok(event) => {
                if submitted_working_state_event(&event, handle) {
                    handle.working_seen.store(true, Ordering::SeqCst);
                    return true;
                }
            }
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
                return false;
            }
            Err(broadcast::error::TryRecvError::Lagged(_)) => return false,
        }
    }
    false
}

pub(crate) async fn await_ack(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    wait: ReceiptWait<'_>,
) -> AckOutcome {
    let ReceiptWait {
        manifest,
        staged_window,
        id_staged,
        target,
        submit_at,
        events: mut ev_rx,
        turn_events: mut turn_ev_rx,
    } = wait;
    let tier1 = manifest.hooks.ack.is_some() && manifest.hooks.ack_payload_field.is_some();
    let mut pane_rx = inner.watcher_of(handle.session_idx).map(|w| w.subscribe());
    let mut working_seen = false;
    let output_seen = false;
    let mut clock = AckClock::new(
        submit_at,
        tier1.then(|| Duration::from_millis(inner.cfg.ack_timeout_ms)),
    );
    if pane_rx.is_none() {
        clock.freeze(Instant::now());
    }

    loop {
        // Asked before every sleep, not only on a notification. The
        // notification is edge-triggered and can fire before this loop is
        // listening for it, which would leave a delivery that is already
        // resolved waiting out the whole acknowledgement window.
        if matches!(
            handle.state(),
            DeliveryState::DeliveredVerified | DeliveryState::DeliveredUnverified
        ) {
            return AckOutcome::Resolved;
        }
        let checkpoint_target = clock.next_target();
        tokio::select! {
            _ = handle.ack.notified() => {
                if matches!(
                    handle.state(),
                    DeliveryState::DeliveredVerified | DeliveryState::DeliveredUnverified
                ) {
                    return AckOutcome::Resolved;
                }
            }
            _ = tokio::time::sleep_until(checkpoint_target.map(|(t, _)| t).unwrap_or_else(Instant::now)),
                if checkpoint_target.is_some() =>
            {
                let now = Instant::now();
                if checkpoint_target.is_some_and(|(_, hook_end)| hook_end) {
                    // Tier-1 window over: the delivery downgrades to screen
                    // evidence. On a pane that has never produced a hook
                    // edge, this is the missing-hook signature: configuration
                    // does not equal subscription. The admin hears once.
                    if tier1 {
                        if let Some(agent) = *handle
                            .submitted_agent
                            .lock()
                            .expect("submitted agent lock")
                        {
                            let pane = PaneKey::new(handle.session_idx, &handle.pane_id);
                            if let Some(binding) = inner.hook_liveness.binding(
                                &pane,
                                agent,
                                &manifest.agent.id,
                            ) {
                                crate::selftest::notify_f1_once(
                                    inner,
                                    &handle.msg_id,
                                    &handle.to,
                                    binding,
                                );
                            }
                        }
                    }
                    clock.end_hook_phase(now);
                    continue;
                }
                clock.advance_checkpoint();
                match receipt_checkpoint_pass(
                    inner, handle, manifest, staged_window, id_staged, target,
                    working_seen, &mut turn_ev_rx, output_seen, &clock,
                ).await {
                    ReceiptStep::Resolved => return AckOutcome::Resolved,
                    ReceiptStep::Deliver => {
                        schedule_missing_hook_diagnostic(
                            inner,
                            handle,
                            &manifest.agent.id,
                            clock.hook_deadline,
                        );
                        return AckOutcome::Screen;
                    }
                    ReceiptStep::Expire => return AckOutcome::Timeout,
                    ReceiptStep::Rebound => return AckOutcome::Rebound,
                    ReceiptStep::Freeze => {
                        // The stable refresh could not prove the screen or
                        // binding. A timeout here would stand on nothing.
                        clock.freeze(Instant::now());
                    }
                    ReceiptStep::Wait => {}
                }
            }
            // Reattach/detach truth for THIS session comes from
            // `inner.watcher_of(handle.session_idx)`, resolved fresh here,
            // never from matching a "session" event's own `data["name"]`
            // against a name captured at function entry: a followed
            // rename (`PaneEvent::SessionRenamed`, `rename_session_slot`
            // in lib.rs) changes the live name mid-wait, and a stale
            // snapshot then never matches an attach or a detach line
            // again. The clock freezes on the first outage and never
            // unfreezes, which is exactly the "ledger append silently
            // drops" failure mode `emit_state`'s doc comment describes,
            // in delivery-wait clothing. `watcher_of` cannot go stale: it
            // reads the live link for this exact idx, so what changed is
            // compared against what IS, not against a name.
            ev = ev_rx.recv() => {
                if track_state_event(&ev, handle) {
                    working_seen = true;
                    handle.working_seen.store(true, Ordering::SeqCst);
                }
                if is_session_event(&ev) {
                    let live = inner.watcher_of(handle.session_idx);
                    if pane_rx.is_none() {
                        if let Some(w) = live {
                            pane_rx = Some(w.subscribe());
                            clock.unfreeze(Instant::now());
                            // Reattach evidence pass, before any deadline
                            // can fire: did the payload arrive during the
                            // outage?
                            match receipt_checkpoint_pass(
                                inner, handle, manifest, staged_window, id_staged, target,
                                working_seen, &mut turn_ev_rx, output_seen, &clock,
                            ).await {
                                ReceiptStep::Resolved => return AckOutcome::Resolved,
                                ReceiptStep::Deliver => {
                                    schedule_missing_hook_diagnostic(
                                        inner,
                                        handle,
                                        &manifest.agent.id,
                                        clock.hook_deadline,
                                    );
                                    return AckOutcome::Screen;
                                }
                                ReceiptStep::Expire => return AckOutcome::Timeout,
                                ReceiptStep::Rebound => return AckOutcome::Rebound,
                                ReceiptStep::Freeze => clock.freeze(Instant::now()),
                                ReceiptStep::Wait => {}
                            }
                        }
                    } else if live.is_none() {
                        pane_rx = None;
                        clock.freeze(Instant::now());
                    }
                } else if matches!(ev, Err(broadcast::error::RecvError::Lagged(_)))
                    && clock.frozen()
                {
                    // A lagged event stream can swallow the reattach
                    // notice; reconcile against the link instead of
                    // staying frozen forever.
                    if let Some(w) = inner.watcher_of(handle.session_idx) {
                        pane_rx = Some(w.subscribe());
                        clock.unfreeze(Instant::now());
                    }
                }
            }
            pe = recv_pane(&mut pane_rx) => {
                match receipt_pane_step(&pe, &handle.pane_id, clock.frozen()) {
                    ReceiptPaneStep::Recheck => {
                        // Output is only a cue to look. PaneChanged carries a
                        // new watcher revision. Both resume a frozen clock and
                        // run the same stable receipt checkpoint immediately.
                        clock.unfreeze(Instant::now());
                        match receipt_checkpoint_pass(
                            inner, handle, manifest, staged_window, id_staged, target,
                            working_seen, &mut turn_ev_rx, output_seen, &clock,
                        ).await {
                            ReceiptStep::Resolved => return AckOutcome::Resolved,
                            ReceiptStep::Deliver => {
                                schedule_missing_hook_diagnostic(
                                    inner,
                                    handle,
                                    &manifest.agent.id,
                                    clock.hook_deadline,
                                );
                                return AckOutcome::Screen;
                            }
                            ReceiptStep::Expire => return AckOutcome::Timeout,
                            ReceiptStep::Rebound => return AckOutcome::Rebound,
                            ReceiptStep::Freeze => clock.freeze(Instant::now()),
                            ReceiptStep::Wait => {}
                        }
                    }
                    ReceiptPaneStep::Rebound => return AckOutcome::Rebound,
                    ReceiptPaneStep::Freeze => {
                        pane_rx = None;
                        clock.freeze(Instant::now());
                    }
                    ReceiptPaneStep::Ignore => {}
                }
            }
        }
    }
}

/// True when an event is an explicitly confirmed Working observation.
pub(crate) fn confirmed_working_state_event(event: &Event) -> bool {
    event.event == "state"
        && event.data["state"] == "working"
        && event.data["working_confirmed"] == true
}

/// True when an event proves a Working edge for this exact submitted delivery.
///
/// This does not associate a turn with a particular message. It only proves
/// that the submitted process generation entered Working after the submit
/// boundary, which is the conservative evidence `turn_ended` may carry from
/// delivery into its separate pane wait.
pub(crate) fn submitted_working_state_event(event: &Event, handle: &Arc<DeliveryHandle>) -> bool {
    if !confirmed_working_state_event(event) || event.data["pane_id"] != handle.pane_id.as_str() {
        return false;
    }
    if event.data["session_idx"].as_u64() != Some(handle.session_idx as u64) {
        return false;
    }
    let submitted_at_ms = handle.submitted_at_ms.load(Ordering::SeqCst);
    if event.data["observed_at_ms"]
        .as_u64()
        .is_none_or(|observed_at_ms| observed_at_ms < submitted_at_ms)
    {
        return false;
    }
    // The event carries the binding that produced it, so this asks whether
    // the working edge came from the process that received the submit.
    // It does not identify which message or task the turn handled.
    // Comparing against the row as it looks now
    // would accept a replacement's turn, and would keep accepting it for
    // as long as the pane happened to look familiar again.
    // Both halves of the identity travel on the event, because a pid
    // alone is transferable and this is a trust comparison.
    let Some(birth) = event.data["source_birth"].as_u64() else {
        return false;
    };
    let agent = crate::identity::ProcId {
        pid: event.data["source_pid"].as_i64().unwrap_or_default() as i32,
        birth,
    };
    let manifest = event.data["source_manifest"].as_str().unwrap_or_default();
    handle.submitted_binding_is(agent, manifest)
}

/// True when a wait may count an event as its Working phase. A standalone
/// Record a confirmed Working edge for this exact submitted notification.
pub(crate) fn track_state_event(
    ev: &Result<Event, broadcast::error::RecvError>,
    handle: &Arc<DeliveryHandle>,
) -> bool {
    ev.as_ref()
        .is_ok_and(|event| submitted_working_state_event(event, handle))
}

/// True when the event is a session lifecycle line: attach, detach, or
/// this daemon's own rename bookkeeping riding the same "session" name
/// (`session_lifecycle`, lib.rs). Which one, and whether it is about THIS
/// caller's session at all, is deliberately not decided here: see the doc
/// comment on `await_ack`'s event arm for why comparing against
/// `inner.watcher_of(session_idx)`'s live truth, not the event's own
/// `data["name"]`, is what a caller does with this.
pub(crate) fn is_session_event(ev: &Result<Event, broadcast::error::RecvError>) -> bool {
    matches!(ev, Ok(e) if e.event == "session")
}

/// Screen evidence for tier 2 uses the protocol's conjunctive form:
/// the marker left the composer AND turn evidence appeared.
///
/// "Left the composer" is manifest-driven: the marker still sits in the
/// composer only when an idle_with_input rule identifies a composer line
/// that carries it (staged-but-unsubmitted text, e.g. Claude's collapsed
/// paste on the `❯` line). Manifests without an idle_with_input rule
/// cannot pin staged text.
///
/// Turn evidence is a working state, output activity, or a changed
/// composer window. The changed window counts only when verification
/// demonstrably staged the id pattern: a redraw can change the window of a
/// pane that never took the paste, but it cannot have staged OUR id first.
/// (%output events can be swallowed by the watcher's per-pane rate limit
/// for single short bursts, MEASURED: a cat pane's echoed submit stays
/// under the 100ms floor, which is why the changed window matters.)
pub(crate) fn staging_target_still_present(
    manifest: &Manifest,
    screen: &str,
    target: StagingTarget<'_>,
) -> bool {
    // This is a post-submit presence check, not ownership proof. A collapsed
    // chip may show that the staged representation remains, but it never
    // authorizes a terminal key.
    staged_representation(manifest, screen, target).is_some()
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn screen_evidence(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    manifest: &Manifest,
    staged_window: &str,
    id_staged: bool,
    target: StagingTarget<'_>,
    working_seen: bool,
    output_seen: bool,
) -> Evidence {
    let Some(watcher) = inner.watcher_of(handle.session_idx) else {
        return Evidence::Unobservable;
    };
    // Use the staging capture flavor so esc-only composer discriminators
    // still apply and the de-escaped window comparison stays like-for-like.
    // The binding is checked on both sides of the read: a capture is not
    // instantaneous, and evidence about a pane whose occupant changed
    // while it was being read is evidence about nobody.
    if !submitted_binding_holds(inner, &watcher, handle) {
        return Evidence::Rebound;
    }
    let capture = if manifest.has_escaped_rules() {
        watcher.client().capture_pane_escaped(&handle.pane_id).await
    } else {
        watcher.client().capture_pane(&handle.pane_id).await
    };
    let Ok(screen) = capture else {
        return Evidence::Unobservable;
    };
    if !submitted_binding_holds(inner, &watcher, handle) {
        return Evidence::Rebound;
    }
    let changed = bottom_window(&strip_csi(&screen), COMPOSER_WINDOW) != staged_window;
    // "The marker left the COMPOSER", and the emphasis is the whole
    // point: a submitted message stays on screen, it just stops being
    // staged input. Asking whether the id appears anywhere in the bottom
    // region would answer yes forever, because the transcript keeps it.
    // So this asks the same two questions staging asked, which are both
    // composer-pinned: is our sentinel still the staged row, or is the
    // vendor's chip still on the composer line.
    let marker_present = staging_target_still_present(manifest, &screen, target);
    if !marker_present
        && tier2_evidence(
            manifest.hooks.ack_evidence,
            changed,
            id_staged,
            working_seen,
            output_seen,
        )
    {
        Evidence::Confirmed
    } else {
        Evidence::Absent
    }
}

/// The tier-2 turn-evidence rule, factored for the unit test: a changed
/// window alone is only evidence when the id demonstrably staged.
pub(crate) fn tier2_evidence(
    ack_evidence: AckEvidence,
    changed: bool,
    id_staged: bool,
    working_seen: bool,
    output_seen: bool,
) -> bool {
    let _ = output_seen;
    match ack_evidence {
        AckEvidence::Receipt => working_seen || (changed && id_staged),
        // A dispatch hook can precede a sibling hook that rejects the prompt.
        // Only the exact candidate's later visual acceptance resolves it.
        AckEvidence::Dispatch => false,
    }
}

pub(crate) fn register_ack(inner: &Arc<Inner>, handle: &Arc<DeliveryHandle>) {
    let mut acks = inner.engine.acks.lock().expect("acks lock");
    let entry = acks
        .entry(PaneKey::new(handle.session_idx, &handle.pane_id))
        .or_default();
    entry.retain(|h| {
        !Arc::ptr_eq(h, handle)
            && matches!(
                h.state(),
                DeliveryState::Submitted | DeliveryState::DeliveredUnverified
            )
    });
    if entry.len() >= ACK_REGISTRY_CAP {
        entry.remove(0);
    }
    entry.push(Arc::clone(handle));
}

pub(crate) fn unregister_ack(inner: &Arc<Inner>, handle: &Arc<DeliveryHandle>) {
    if let Some(entry) = inner
        .engine
        .acks
        .lock()
        .expect("acks lock")
        .get_mut(&PaneKey::new(handle.session_idx, &handle.pane_id))
    {
        entry.retain(|h| !Arc::ptr_eq(h, handle));
    }
}

/// Deliveries on a pane a hook ACK could match right now.
pub(crate) fn ack_candidates(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
) -> Vec<Arc<DeliveryHandle>> {
    inner
        .engine
        .acks
        .lock()
        .expect("acks lock")
        .get(&PaneKey::new(session_idx, pane_id))
        .map(|v| v.to_vec())
        .unwrap_or_default()
}

/// Resolve a hook ACK onto a delivery: verify a submitted one, or upgrade
/// a screen-verified one (the legal DeliveredUnverified -> Verified move
/// that keeps receipts honest). Racing ahead of the Submitted line sets
/// the early-ack flag the worker consumes.
pub(crate) fn resolve_hook_ack(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    reporter: crate::identity::ProcId,
    reporter_manifest: &str,
    edge_ms: u64,
    turn: Option<crate::turnkey::TurnKey>,
) -> bool {
    // A hook proves a process ran a turn, and a pane id is reusable, so
    // the report has to come from the process Enter actually reached. The
    // caller already authenticated the reporting process and resolved its
    // row and manifest, so that binding is compared directly here. Looking
    // it up again through a live watcher would be worse than redundant: a
    // detached control connection has no watcher, and a legitimate hook
    // arriving during an outage would be thrown away.
    if !handle.submitted_binding_is(reporter, reporter_manifest) {
        return false;
    }
    // Classifying the state and installing an early acknowledgement are
    // ONE decision, under the lock the worker transitions through. Read
    // the state, see `Staged`, and install afterwards, and the worker can
    // move to `Submitted` and take in between: the record is then written
    // after the only read of it and the acknowledgement is lost.
    //
    // The FIRST one installed stands. A second acknowledgement for the
    // same delivery describes the same consumption, and overwriting would
    // move the edge to whichever report happened to arrive last.
    let state = {
        let mut st = handle.state.lock().expect("handle state lock");
        if st.state == DeliveryState::Staged && st.early_ack.is_none() {
            st.early_ack = Some(PendingAck {
                edge_ms,
                turn: turn.clone(),
                evidence: PendingAckEvidence::Receipt,
            });
        }
        st.state
    };
    let moved = match state {
        // Past the point where an early record would be read, so this
        // resolves the delivery here instead. `advance` is its own
        // transaction and refuses if the state moved again underneath,
        // which is the safe handoff back to the worker.
        DeliveryState::Submitted => {
            match record_notification_notified(handle, Some(VerifiedBy::Hook)) {
                Ok(true) => advance(
                    inner,
                    handle,
                    &[DeliveryState::Submitted],
                    Step::to(DeliveryState::DeliveredVerified)
                        .cause("hook_ack")
                        .verified(VerifiedBy::Hook)
                        .turn_edge(edge_ms)
                        .turn(turn),
                ),
                Ok(false) => false,
                Err(error) => {
                    error!(id = %handle.msg_id, error = %error, "notification receipt fact failed");
                    false
                }
            }
        }
        // A screen receipt that already resolved stands. The replacement
        // occupant cannot upgrade it, and it must not be taken away
        // either: the original binding earned it before the pane changed
        // hands, and the record does not retract what was true.
        DeliveryState::DeliveredUnverified => advance(
            inner,
            handle,
            &[DeliveryState::DeliveredUnverified],
            Step::to(DeliveryState::DeliveredVerified)
                .cause("hook_ack_upgrade")
                .verified(VerifiedBy::Hook)
                .turn_edge(edge_ms)
                .turn(turn),
        ),
        // Installed above, under the lock that read this state. The
        // worker takes it immediately after its Submitted line.
        DeliveryState::Staged => true,
        _ => false,
    };
    handle.ack.notify_waiters();
    moved
}

/// Record an exact hook dispatch that still needs visual acceptance.
///
/// This does not resolve the delivery. Vendors can invoke several prompt
/// hooks concurrently, and one sibling can reject the prompt after this hook
/// has already reported it. The matching turn remains pending until fusion
/// confirms a later Working observation for the same process and manifest.
pub(crate) fn record_dispatch_candidate(
    handle: &Arc<DeliveryHandle>,
    reporter: crate::identity::ProcId,
    reporter_manifest: &str,
    edge_ms: u64,
    turn: Option<crate::turnkey::TurnKey>,
) -> bool {
    if !handle.submitted_binding_is(reporter, reporter_manifest) {
        return false;
    }
    let recorded = {
        let mut st = handle.state.lock().expect("handle state lock");
        if !matches!(
            st.state,
            DeliveryState::Staged | DeliveryState::Submitted | DeliveryState::DeliveredUnverified
        ) {
            return false;
        }
        match &st.early_ack {
            Some(existing)
                if existing.edge_ms != edge_ms
                    || existing.turn.as_ref() != turn.as_ref()
                    || existing.evidence != PendingAckEvidence::DispatchPending =>
            {
                false
            }
            Some(_) => true,
            None => {
                st.early_ack = Some(PendingAck {
                    edge_ms,
                    turn,
                    evidence: PendingAckEvidence::DispatchPending,
                });
                true
            }
        }
    };
    if recorded {
        handle.ack.notify_waiters();
    }
    recorded
}

/// Mark every exact payload match as ambiguous without turning any of them
/// into receipt evidence. This is the duplicate-bytes case: a pane hook cannot
/// identify which attempt it belongs to, so all candidates remain recoverable.
pub(crate) fn mark_dispatch_match_ambiguous(
    handle: &Arc<DeliveryHandle>,
    reporter: crate::identity::ProcId,
    reporter_manifest: &str,
    cause: &str,
) -> bool {
    if !handle.submitted_binding_is(reporter, reporter_manifest) {
        return false;
    }
    let marked = {
        let mut state = handle.state.lock().expect("handle state lock");
        if !matches!(
            state.state,
            DeliveryState::Staged | DeliveryState::Submitted | DeliveryState::DeliveredUnverified
        ) {
            return false;
        }
        state.cause = Some(cause.to_string());
        true
    };
    if marked {
        handle.ack.notify_waiters();
    }
    marked
}

pub(crate) enum UnkeyedDispatchSelection {
    None,
    Unique(Arc<DeliveryHandle>, String),
    Ambiguous(Vec<Arc<DeliveryHandle>>),
}

pub(crate) fn select_unkeyed_dispatch_candidate(
    handles: Vec<Arc<DeliveryHandle>>,
    reporter: crate::identity::ProcId,
    reporter_manifest: &str,
    dispatch_edge_ms: u64,
) -> UnkeyedDispatchSelection {
    let mut matching = handles
        .into_iter()
        .filter_map(|handle| {
            if !handle.submitted_binding_is(reporter, reporter_manifest) {
                return None;
            }
            let state = handle.state.lock().expect("handle state lock");
            let owner = state
                .early_ack
                .as_ref()
                .filter(|pending| {
                    pending.turn.is_none()
                        && pending.evidence == PendingAckEvidence::DispatchPending
                        && pending.edge_ms == dispatch_edge_ms
                })
                .and_then(|_| state.barrier.clone())?;
            drop(state);
            Some((handle, owner))
        })
        .collect::<Vec<_>>();
    match matching.len() {
        0 => UnkeyedDispatchSelection::None,
        1 => {
            let (handle, owner) = matching.pop().expect("one dispatch candidate");
            UnkeyedDispatchSelection::Unique(handle, owner)
        }
        _ => UnkeyedDispatchSelection::Ambiguous(
            matching.into_iter().map(|(handle, _)| handle).collect(),
        ),
    }
}

/// Accept an unkeyed prompt dispatch after the exact pane shows a later
/// lifecycle-capable Working frame.
///
/// The prompt hook alone is provisional because another vendor hook may still
/// reject the prompt. The barrier owner makes the later visual observation
/// recipient- and attempt-specific even though the vendor exposes no turn id.
pub(crate) fn confirm_unkeyed_dispatch_ack(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    reporter: crate::identity::ProcId,
    reporter_manifest: &str,
    dispatch_edge_ms: u64,
    accepted_ms: u64,
) -> bool {
    if dispatch_edge_ms >= accepted_ms {
        return false;
    }
    let (handle, owner) = match select_unkeyed_dispatch_candidate(
        ack_candidates(inner, session_idx, pane_id),
        reporter,
        reporter_manifest,
        dispatch_edge_ms,
    ) {
        UnkeyedDispatchSelection::Unique(handle, owner) => (handle, owner),
        UnkeyedDispatchSelection::Ambiguous(handles) => {
            for handle in handles {
                mark_dispatch_match_ambiguous(
                    &handle,
                    reporter,
                    reporter_manifest,
                    "hook_dispatch_ambiguous",
                );
            }
            return false;
        }
        UnkeyedDispatchSelection::None => return false,
    };
    if !fusion::set_hold_owned(inner, session_idx, pane_id, &owner, Some) {
        return false;
    }
    let state = {
        let mut st = handle.state.lock().expect("handle state lock");
        let Some(current) = st.early_ack.as_mut() else {
            return false;
        };
        if current.turn.is_some()
            || current.evidence != PendingAckEvidence::DispatchPending
            || current.edge_ms != dispatch_edge_ms
        {
            return false;
        }
        current.evidence = PendingAckEvidence::DispatchAccepted;
        current.edge_ms = accepted_ms;
        st.state
    };
    handle.ack.notify_waiters();
    match state {
        DeliveryState::Staged => {}
        DeliveryState::Submitted => {
            let recorded = match record_notification_notified(&handle, Some(VerifiedBy::Hook)) {
                Ok(recorded) => recorded,
                Err(error) => {
                    error!(id = %handle.msg_id, error = %error, "notification receipt fact failed");
                    return false;
                }
            };
            if recorded {
                let _ = advance(
                    inner,
                    &handle,
                    &[DeliveryState::Submitted],
                    Step::to(DeliveryState::DeliveredVerified)
                        .cause("hook_dispatch_accepted_start")
                        .verified(VerifiedBy::Hook)
                        .turn_edge(accepted_ms),
                );
            }
        }
        DeliveryState::DeliveredUnverified => {
            let _ = advance(
                inner,
                &handle,
                &[DeliveryState::DeliveredUnverified],
                Step::to(DeliveryState::DeliveredVerified)
                    .cause("hook_dispatch_accepted_start")
                    .verified(VerifiedBy::Hook)
                    .turn_edge(accepted_ms),
            );
        }
        _ => {}
    }
    true
}

/// Retire receipt evidence for one exact unkeyed hook edge. The pane runtime
/// candidate is managed separately in fusion and may still report Working for
/// a later human prompt.
pub(crate) fn reject_unkeyed_dispatch_ack(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    reporter: crate::identity::ProcId,
    reporter_manifest: &str,
    dispatch_edge_ms: u64,
    cause: &str,
) -> usize {
    let mut rejected = 0;
    for handle in ack_candidates(inner, session_idx, pane_id) {
        if !handle.submitted_binding_is(reporter, reporter_manifest) {
            continue;
        }
        let removed = {
            let mut state = handle.state.lock().expect("handle state lock");
            let matches = state.early_ack.as_ref().is_some_and(|pending| {
                pending.turn.is_none()
                    && pending.evidence == PendingAckEvidence::DispatchPending
                    && pending.edge_ms == dispatch_edge_ms
            });
            if matches {
                state.early_ack = None;
                state.cause = Some(cause.to_string());
            }
            matches
        };
        if removed {
            rejected += 1;
            handle.ack.notify_waiters();
        }
    }
    rejected
}

/// Bind an exact dispatch to its composer barrier without publishing receipt.
///
/// Lifecycle reconciliation uses this before it updates public state. The
/// exact end can then settle the barrier in that same observation, while the
/// delivery remains unresolved until the fused state is cached and emitted.
pub(crate) fn prepare_dispatch_ack(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    reporter: crate::identity::ProcId,
    reporter_manifest: &str,
    turn: &crate::turnkey::TurnKey,
) -> DispatchPreparation {
    let mut result = DispatchPreparation::default();
    for handle in ack_candidates(inner, session_idx, pane_id) {
        if !handle.submitted_binding_is(reporter, reporter_manifest) {
            continue;
        }
        let pending = {
            let st = handle.state.lock().expect("handle state lock");
            st.early_ack
                .as_ref()
                .filter(|pending| {
                    pending.turn.as_ref() == Some(turn)
                        && pending.evidence == PendingAckEvidence::DispatchPending
                })
                .and_then(|pending| st.barrier.clone().map(|owner| (owner, pending.edge_ms)))
        };
        let Some((owner, start_ms)) = pending else {
            continue;
        };
        if let Some(bound) =
            fusion::bind_turn(inner, session_idx, pane_id, &owner, turn.clone(), start_ms)
        {
            result.prepared = true;
            result.end_already_present |= bound.end_already_present;
        }
    }
    result
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DispatchPreparation {
    pub(crate) prepared: bool,
    pub(crate) end_already_present: bool,
}

/// Accept pending dispatches after the exact turn has independent evidence.
///
/// The usual evidence is a later visual Working observation, cached before
/// this call. A matching terminal hook also proves that a short turn existed
/// even when the watcher missed its Working frame. The composer barrier keeps
/// either path safe while the terminal outcome is reconciled.
pub(crate) fn confirm_dispatch_ack(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    reporter: crate::identity::ProcId,
    reporter_manifest: &str,
    turn: &crate::turnkey::TurnKey,
    accepted_ms: u64,
) {
    for handle in ack_candidates(inner, session_idx, pane_id) {
        if !handle.submitted_binding_is(reporter, reporter_manifest) {
            continue;
        }
        let state = {
            let mut st = handle.state.lock().expect("handle state lock");
            let Some(pending) = st.early_ack.as_mut() else {
                continue;
            };
            if pending.turn.as_ref() != Some(turn)
                || pending.evidence != PendingAckEvidence::DispatchPending
            {
                continue;
            }
            pending.evidence = PendingAckEvidence::DispatchAccepted;
            pending.edge_ms = accepted_ms;
            st.state
        };
        handle.ack.notify_waiters();
        match state {
            DeliveryState::Staged => {}
            DeliveryState::Submitted => {
                let recorded = match record_notification_notified(&handle, Some(VerifiedBy::Hook)) {
                    Ok(recorded) => recorded,
                    Err(error) => {
                        error!(id = %handle.msg_id, error = %error, "notification receipt fact failed");
                        continue;
                    }
                };
                if recorded {
                    let moved = advance(
                        inner,
                        &handle,
                        &[DeliveryState::Submitted],
                        Step::to(DeliveryState::DeliveredVerified)
                            .cause("hook_dispatch_accepted_start")
                            .verified(VerifiedBy::Hook)
                            .turn_edge(accepted_ms)
                            .turn(Some(turn.clone())),
                    );
                    if !moved && handle.state() == DeliveryState::DeliveredUnverified {
                        let _ = advance(
                            inner,
                            &handle,
                            &[DeliveryState::DeliveredUnverified],
                            Step::to(DeliveryState::DeliveredVerified)
                                .cause("hook_dispatch_accepted_start")
                                .verified(VerifiedBy::Hook)
                                .turn_edge(accepted_ms)
                                .turn(Some(turn.clone())),
                        );
                    }
                }
            }
            DeliveryState::DeliveredUnverified => {
                let _ = advance(
                    inner,
                    &handle,
                    &[DeliveryState::DeliveredUnverified],
                    Step::to(DeliveryState::DeliveredVerified)
                        .cause("hook_dispatch_accepted_start")
                        .verified(VerifiedBy::Hook)
                        .turn_edge(accepted_ms)
                        .turn(Some(turn.clone())),
                );
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// agent.wait
// ---------------------------------------------------------------------------

/// How a wait ended. Serialized into agent.wait answers and error data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WaitOutcome {
    /// The target reached `until`.
    Reached,
    /// The timeout expired first.
    Timeout,
    /// The pinned pane died or its occupant changed mid-wait; resolving
    /// anything else would be a false answer about a different process.
    OccupantChanged,
}

/// A finished wait: how it ended, the fused state it ended on, and how
/// long it actually waited.
pub(crate) struct WaitEnd {
    pub(crate) outcome: WaitOutcome,
    pub(crate) state: AgentState,
    pub(crate) waited_ms: u64,
}

/// The pane row behind a wait target: the live table while attached, the
/// frozen last-known table during a detach (frozen rows cannot false-alarm
/// the pin; the reattach re-check settles it).
pub(crate) fn occupant_of(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
) -> Option<PaneRow> {
    if let Some(w) = inner.watcher_of(session_idx) {
        return w.pane(pane_id);
    }
    inner
        .session(session_idx)?
        .last_panes
        .lock()
        .expect("last panes lock")
        .get(pane_id)
        .map(|pane| pane.row.clone())
}

/// True when the pinned occupant is gone: pane missing, dead, or running
/// under a different root pid than the one pinned at wait start.
pub(crate) fn occupant_gone(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    pinned_pid: i32,
) -> bool {
    match occupant_of(inner, session_idx, pane_id) {
        Some(row) => row.dead || row.pane_pid != pinned_pid,
        None => true,
    }
}
