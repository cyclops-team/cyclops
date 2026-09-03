//! Gating state machine, composer eligibility, holds, and receipt verification.

use super::*;

/// One transition request for [`advance`].
pub(crate) struct Step<'a> {
    pub(crate) next: DeliveryState,
    pub(crate) cause: Option<&'a str>,
    pub(crate) verified_by: Option<VerifiedBy>,
    pub(crate) note: Option<String>,
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
            note: None,
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
    pub(crate) fn note(mut self, n: String) -> Step<'a> {
        self.note = Some(n);
        self
    }
}

/// Every transition the pipeline performs, checked as legal against
/// `DeliveryState::can_transition_to` by a unit test and a debug assertion
/// in [`advance`].
#[cfg(test)]
pub(crate) const PIPELINE_TRANSITIONS: &[(DeliveryState, DeliveryState)] = {
    use DeliveryState::*;
    &[
        (Queued, Gating),
        (Queued, AttentionRequired),
        (Queued, ParkedBlockedQuota),
        (Gating, Pasting),
        (Gating, RetryQueued),
        (Gating, AttentionRequired),
        (Gating, ParkedBlockedQuota),
        (Pasting, Staged),
        (Pasting, RetryQueued),
        (Pasting, AttentionRequired),
        (Staged, Submitted),
        (Staged, RetryQueued),
        (Staged, AttentionRequired),
        (Submitted, DeliveredVerified),
        (Submitted, DeliveredUnverified),
        (Submitted, RetryQueued),
        (Submitted, AttentionRequired),
        (DeliveredUnverified, DeliveredVerified),
        (RetryQueued, Gating),
        (RetryQueued, AttentionRequired),
    ]
};

/// Write one direct-delivery transition to the record: the `Kind::State`
/// line in every named session ledger, then the matching `delivery-state`
/// event. Mailbox notifications never call this function because their
/// workspace notification record is the sole durable authority.
/// One writer for both the running pipeline ([`advance`]) and the restart
/// closure ([`close_limbo`]): each used to build these by hand, and the
/// restart event had already lost three fields (`verified_by`, `attempts`,
/// `note`) the live one carried.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_delivery_state(
    inner: &Arc<Inner>,
    sessions: &[usize],
    msg_id: &str,
    to: &str,
    recipient: Option<RecipientKey>,
    from: DeliveryState,
    next: DeliveryState,
    cause: Option<&str>,
    note: Option<&str>,
    record: &Delivery,
) -> Option<u64> {
    let line = LedgerLine {
        to: vec![to.to_string()],
        deliveries: vec![record.clone()],
        ..daemon_line(
            Kind::State,
            msg_id.to_string(),
            json!({
                "to": to,
                "recipient": recipient,
                "from": from,
                "to_state": next,
                "cause": cause,
            }),
        )
    };
    // Every session file carrying this delivery's msg line gets the state
    // line too; a per-session ledger is a complete stream on its own.
    let mut seq = None;
    for idx in sessions {
        let s = inner.append_line(*idx, line.clone());
        if seq.is_none() {
            seq = s;
        }
    }
    inner.emit(
        "delivery-state",
        json!({
            "id": msg_id,
            "to": to,
            "recipient": recipient,
            "from": from,
            "to_state": next,
            "cause": cause,
            "verified_by": record.verified_by,
            "attempts": record.attempts,
            "note": note,
        }),
        seq,
    );
    seq
}

/// Apply one transition if the delivery is still in an expected state.
/// Returns false when a concurrent actor (ACK matcher vs worker timeout)
/// already moved it; the caller treats that as "someone else resolved it".
/// Legal-transition checking is a debug assertion: an illegal move is a
/// programming bug, and the ledger records what actually happened.
pub(crate) fn advance(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    allowed_from: &[DeliveryState],
    step: Step<'_>,
) -> bool {
    let (from, record) = {
        let mut st = handle.state.lock().expect("handle state lock");
        if !allowed_from.contains(&st.state) {
            return false;
        }
        let from = st.state;
        debug_assert!(
            from.can_transition_to(step.next),
            "illegal delivery transition {from:?} -> {:?}",
            step.next
        );
        if !from.can_transition_to(step.next) {
            error!(
                id = %handle.msg_id,
                ?from,
                to_state = ?step.next,
                "refusing illegal delivery transition"
            );
            return false;
        }
        st.state = step.next;
        if let Some(v) = step.verified_by {
            st.verified_by = Some(v);
        }
        st.cause = step.cause.map(str::to_string);
        if let Some(n) = &step.note {
            st.note = Some(n.clone());
        }
        if step.next != DeliveryState::Gating {
            st.held_by = None;
        }
        (
            from,
            Delivery {
                to: handle.to.clone(),
                state: st.state,
                verified_by: st.verified_by,
                attempts: st.attempts,
                ts: unix_ms(),
                cause: st.cause.clone(),
            },
        )
    };
    if handle.owns_session_delivery_state() {
        emit_delivery_state(
            inner,
            &handle.ledger_sessions,
            &handle.msg_id,
            &handle.to,
            inner.recipient_key(handle.session_idx, &handle.pane_id),
            from,
            step.next,
            step.cause,
            step.note.as_deref(),
            &record,
        );
    }
    // send_replace, not send: watch::Sender::send drops the value when no
    // receiver exists, and receipt blocking subscribes late. A worker that
    // resolves before the subscribe must still leave the state readable, or
    // the receipt waits out its whole cap on an already-final delivery.
    handle.state_tx.send_replace(step.next);
    // A receipt is the first thing that PROVES the composer was consumed:
    // either the vendor acknowledged this message, or the marker left the
    // composer and a turn started. Send-keys returning Ok proves neither.
    // tmux accepting the key says nothing about what the vendor did with
    // it, and a swallowed Enter leaves the payload staged, which is the
    // staged-never-sent class this whole unit exists for.
    //
    // Only the FIRST resolution promotes. The unverified-to-verified
    // upgrade is the same consumption arriving twice, and re-marking it
    // would push the mark past a turn-end edge that has already arrived.
    let first_receipt = from == DeliveryState::Submitted
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegateAction {
    ImmediateReproof,
    Hold,
    BlockPreWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegateCause {
    BarrierHeld,
    BindingChanged,
    CapabilityChanged,
}

impl RegateCause {
    pub(crate) fn reproof_slot(self) -> Option<usize> {
        match self {
            Self::BarrierHeld => None,
            Self::BindingChanged => Some(0),
            Self::CapabilityChanged => Some(1),
        }
    }
}

/// A held composer waits for its owner to release it. Binding and capability
/// races receive one immediate re-proof per exact evidence generation.
pub(crate) fn regate_action(handle: &DeliveryHandle, cause: RegateCause) -> RegateAction {
    let mut state = handle.state.lock().expect("handle state lock");
    state.regates = state.regates.saturating_add(1);
    match cause {
        RegateCause::BarrierHeld => {
            if handle.notification.is_some() {
                RegateAction::Hold
            } else {
                RegateAction::BlockPreWrite
            }
        }
        RegateCause::BindingChanged | RegateCause::CapabilityChanged => {
            let slot = cause.reproof_slot().expect("reproof slot");
            if state.regate_reproof_used[slot] {
                RegateAction::BlockPreWrite
            } else {
                state.regate_reproof_used[slot] = true;
                RegateAction::ImmediateReproof
            }
        }
    }
}

pub(crate) fn reset_immediate_regates(handle: &DeliveryHandle) {
    handle
        .state
        .lock()
        .expect("handle state lock")
        .regate_reproof_used = [false; 2];
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
    pub(crate) verify_outcome: Option<NotificationVerifyOutcome>,
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
            verify_outcome: None,
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

    /// The pane's manifest requires hook liveness and no admitting edge has
    /// been published for its current binding. Carries the observation so
    /// the durable block names the exact binding and the block itself.
    pub(crate) fn hook_admission_unproven(
        observation: Option<NotificationPreWriteObservation>,
    ) -> Self {
        Self {
            cause: HOOK_ADMISSION_UNPROVEN.into(),
            boundary: WriteBoundary::BeforeWrite,
            pre_write_block: Some(Box::new(PreWriteBlock {
                cause: NotificationPreWriteCause::WriteReadinessChanged,
                observation,
            })),
            verify_outcome: None,
        }
    }

    pub(crate) fn binding_unprovable(observation: Option<NotificationPreWriteObservation>) -> Self {
        Self {
            cause: "binding_unprovable".into(),
            boundary: WriteBoundary::BeforeWrite,
            pre_write_block: Some(Box::new(PreWriteBlock {
                cause: NotificationPreWriteCause::BindingUnprovable,
                observation,
            })),
            verify_outcome: None,
        }
    }

    pub(crate) fn pane_too_narrow(mut observation: NotificationPreWriteObservation) -> Self {
        observation.required_pane_width = Some(cyclops_proto::DOORBELL_V3_MIN_PANE_WIDTH);
        Self {
            cause: "pane_too_narrow".into(),
            boundary: WriteBoundary::BeforeWrite,
            pre_write_block: Some(Box::new(PreWriteBlock {
                cause: NotificationPreWriteCause::WriteReadinessChanged,
                observation: Some(observation),
            })),
            verify_outcome: None,
        }
    }

    pub(crate) fn composer_ownership_unproven() -> Self {
        Self::blocked_before_write(
            "composer_ownership_unproven",
            NotificationPreWriteCause::ComposerOwnershipUnproven,
        )
    }

    /// Does this failure belong back at the gate rather than in the
    /// retry budget? True only where the cause is readiness moving under
    /// a delivery that had not yet written anything.
    pub(crate) fn regate_cause(&self) -> Option<RegateCause> {
        match self.cause.as_str() {
            "barrier_held" => Some(RegateCause::BarrierHeld),
            "binding_changed" => Some(RegateCause::BindingChanged),
            "capability_changed" => Some(RegateCause::CapabilityChanged),
            _ => None,
        }
    }

    /// The composer barrier was not this attempt's to take: somebody
    /// else's payload or a person's typing is in there. Nothing was
    /// written, so this returns to the gate.
    pub(crate) fn barrier_held() -> Self {
        Self {
            cause: "barrier_held".into(),
            boundary: WriteBoundary::BeforeWrite,
            pre_write_block: None,
            verify_outcome: None,
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
        Self {
            cause: "paste_failed".into(),
            boundary: WriteBoundary::AfterWrite,
            pre_write_block: None,
            verify_outcome: None,
        }
    }

    pub(crate) fn verify_failed() -> Self {
        Self::verify_failed_with(NotificationVerifyOutcome::ambiguous())
    }

    pub(crate) fn verify_failed_with(verify_outcome: NotificationVerifyOutcome) -> Self {
        Self {
            cause: "verify_failed".into(),
            boundary: WriteBoundary::AfterWrite,
            pre_write_block: None,
            verify_outcome: Some(verify_outcome),
        }
    }

    pub(crate) fn verify_timeout() -> Self {
        Self::verify_failed_with(NotificationVerifyOutcome {
            kind: NotificationVerifyFailureKind::Timeout,
            observed_composer: ComposerState::ComposerAmbiguous,
        })
    }

    pub(crate) fn verify_mismatch(observed_composer: ComposerState) -> Self {
        Self::verify_failed_with(NotificationVerifyOutcome {
            kind: NotificationVerifyFailureKind::Mismatch,
            observed_composer,
        })
    }

    pub(crate) fn verify_owner_missing(observed_composer: ComposerState) -> Self {
        Self::verify_failed_with(NotificationVerifyOutcome {
            kind: NotificationVerifyFailureKind::OwnerMissing,
            observed_composer,
        })
    }

    pub(crate) fn pane_rebound_after_paste() -> Self {
        Self {
            cause: "pane_rebound_after_paste".into(),
            boundary: WriteBoundary::AfterWrite,
            pre_write_block: None,
            verify_outcome: None,
        }
    }

    pub(crate) fn submit_failed() -> Self {
        Self {
            cause: "submit_failed".into(),
            boundary: WriteBoundary::AfterWrite,
            pre_write_block: None,
            verify_outcome: None,
        }
    }

    /// The pane changed hands after Enter. Terminal, and after the write
    /// boundary: the original occupant may well have received the message,
    /// so this says the outcome is unknown rather than claiming a failure.
    pub(crate) fn receipt_occupant_changed() -> Self {
        Self {
            cause: "receipt_occupant_changed".into(),
            boundary: WriteBoundary::AfterWrite,
            pre_write_block: None,
            verify_outcome: None,
        }
    }

    pub(crate) fn ack_timeout() -> Self {
        Self {
            cause: "ack_timeout".into(),
            boundary: WriteBoundary::AfterWrite,
            pre_write_block: None,
            verify_outcome: None,
        }
    }

    /// The durable boundary could not be advanced after the attempt crossed it.
    /// Retrying could duplicate a notification whose append outcome is unknown.
    pub(crate) fn notification_record_failed() -> Self {
        Self {
            cause: NOTIFICATION_RECORD_FAILED.into(),
            boundary: WriteBoundary::AfterWrite,
            pre_write_block: None,
            verify_outcome: None,
        }
    }

    pub(crate) fn claimed_staged_settlement_failed() -> Self {
        Self {
            cause: CLAIMED_STAGED_SETTLEMENT_FAILED.into(),
            boundary: WriteBoundary::AfterWrite,
            pre_write_block: None,
            verify_outcome: None,
        }
    }

    pub(crate) fn faults_notification_worker(&self) -> bool {
        self.cause == CLAIMED_STAGED_SETTLEMENT_FAILED
    }

    /// Map the injector's closed set of pre-submit causes to the semantic
    /// constructors above. Unknown injector errors remain conservatively
    /// after-write; they must never gain retryability by default.
    pub(crate) fn from_inject(cause: String) -> Self {
        match cause.as_str() {
            "spool_failed" => Self::spool_failed(),
            // The barrier refused before anything was written, so this is
            // readiness changing between the proof and the write, not a
            // transport failure. It goes back to the gate rather than to
            // a human.
            "barrier_held" => Self::barrier_held(),
            "prewrite_session_detached" => Self::session_detached(),
            "prewrite_binding_unprovable" => Self::binding_unprovable(None),
            "composer_ownership_unproven" => Self::composer_ownership_unproven(),
            // The pane's binding moved between the proof and the write.
            // Nothing was written, and re-proving it is the gate's job.
            "binding_changed" | "capability_changed" => Self {
                cause,
                boundary: WriteBoundary::BeforeWrite,
                pre_write_block: None,
                verify_outcome: None,
            },
            "paste_failed" => Self::paste_failed(),
            "verify_failed" => Self::verify_failed(),
            NOTIFICATION_RECORD_FAILED => Self::notification_record_failed(),
            _ => Self {
                cause,
                boundary: WriteBoundary::AfterWrite,
                pre_write_block: None,
                verify_outcome: None,
            },
        }
    }
}

/// One injection attempt: paste, verify, submit, wait for an ACK tier.
///
/// The gate's admitting snapshot is re-checked against the live pane table.
/// The irreversible write and submit bookends require the same pane-root,
/// terminal leader, and admitted agent generations plus the same manifest. A
/// replacement occupant must never receive the payload or Enter.
pub(crate) async fn attempt_delivery(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    manifest_id: &str,
    admitted_pid: i32,
) -> AttemptOutcome {
    // This capability belongs only to the capture immediately before this
    // attempt's paste. A later retry must earn it again from fresh evidence.
    handle.set_working_clean_submit_admitted(false);
    let watcher = match exact_prewrite_watcher(inner, handle, manifest_id) {
        Ok(watcher) => watcher,
        Err(failure) => return AttemptOutcome::Failed(failure),
    };
    let Some(manifest) = inner.manifests.get(manifest_id) else {
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
    let observed_row = watcher.pane(&handle.pane_id);
    let pane_width = observed_row.as_ref().map(|row| row.width);
    let observed =
        observed_row.and_then(|row| fusion::admitted_binding(inner, handle.session_idx, &row));
    let selected = match select_attempt_payload(handle, manifest, observed.as_ref(), pane_width) {
        Ok(selected) => selected,
        Err(error) => {
            error!(id = %handle.msg_id, error = %error, "notification payload reconstruction failed");
            return AttemptOutcome::Failed(AttemptFailure::payload_unavailable());
        }
    };
    handle.set_attempt_payload(selected.bytes.clone(), selected.transport);
    // Spooled FIRST, and deliberately before the pause and the proof
    // below. Loading the buffer costs a control round trip, and a round
    // trip is time a person can type into the composer that no capture
    // afterwards would see, because the proof would already be behind it.
    // Spooling touches no pane, so moving it earlier costs nothing and
    // leaves the admitting capture as the last thing before the write.
    // What remains between the proof and the paste is the command
    // envelope itself, which is irreducible.
    if let Err(cause) = injector.spool(&selected.bytes).await {
        return AttemptOutcome::Failed(AttemptFailure::from_inject(cause));
    }
    inject_pause(inner, "pre_paste").await;
    let watcher = match exact_prewrite_watcher(inner, handle, manifest_id) {
        Ok(watcher) => watcher,
        Err(failure) => {
            injector.discard().await;
            if failure.cause == "pane_rebound" {
                gate_line(
                    inner,
                    handle,
                    "rebound",
                    None,
                    Some("route_binding_changed"),
                );
            }
            return AttemptOutcome::Failed(failure);
        }
    };
    if let Err(detail) = occupant_unchanged(inner, &watcher, handle, manifest_id, admitted_pid) {
        injector.discard().await;
        gate_line(inner, handle, "rebound", None, Some(&detail));
        let failure = if detail == "pane_gone" {
            AttemptFailure::session_detached()
        } else {
            AttemptFailure::pane_rebound_before_paste()
        };
        return AttemptOutcome::Failed(failure);
    }
    // The gate's clean-composer evidence was current when it admitted, and
    // admission is a decision about a moment. A person can start typing in
    // the gap that follows, and the occupant re-check above would not
    // notice: same pane, same pid, same manifest, new draft. So the
    // readiness rule is asked again here, against a capture taken now,
    // immediately before the write that cannot be taken back.
    match crate::observe_pane(
        inner,
        handle.session_idx,
        &watcher,
        &handle.pane_id,
        true,
        "pre_paste",
    )
    .await
    {
        Some(det) => {
            // A positive human draft remains a hard boundary. For an
            // authenticated idle or working agent, however, an unreadable
            // composer is not a reason to strand a durable doorbell after
            // the gate already admitted the same live occupant.
            let unproven_composer_is_still_eligible = handle.notification.is_some()
                && watcher.pane(&handle.pane_id).is_some_and(|row| {
                    notification_pane_for_unproven_composer(inner, handle, &row, manifest_id, &det)
                        .is_some()
                });
            if !det.write_ready && !unproven_composer_is_still_eligible {
                let reason = det.write_block.as_deref().unwrap_or("unstamped");
                gate_line(
                    inner,
                    handle,
                    "hold",
                    None,
                    Some(&format!("not_write_ready:{reason}")),
                );
                injector.discard().await;
                if reason == HOOK_ADMISSION_UNPROVEN {
                    // Not a readiness flicker: nothing on the pane will
                    // clear it. Park the wake as a named durable block
                    // that carries the exact binding it was refused for.
                    let observation = watcher.pane(&handle.pane_id).and_then(|row| {
                        let mut observation =
                            composer_semantic_observation(inner, handle, &row, manifest_id)?;
                        observation.write_block = Some(HOOK_ADMISSION_UNPROVEN.to_string());
                        Some(observation)
                    });
                    return AttemptOutcome::Failed(AttemptFailure::hook_admission_unproven(
                        observation,
                    ));
                }
                return AttemptOutcome::Failed(AttemptFailure::pane_rebound_before_paste());
            }
            // A Working runtime is safe only in this narrow, positive shape.
            // Keep that admission with the in-flight notification: after the
            // paste, the exact doorbell itself naturally renders as input and
            // cannot repeat the clean-composer proof that made the write safe.
            handle.set_working_clean_submit_admitted(
                handle.notification.is_some()
                    && det.state == AgentState::Working
                    && (det.write_ready
                        || det.screen_proves_write_safe_composer()
                        || unproven_composer_is_still_eligible),
            );
        }
        None => {
            injector.discard().await;
            return AttemptOutcome::Failed(AttemptFailure::session_detached());
        }
    }
    // That recompute took a capture, so who owns the pane is checked again
    // after it: otherwise the newest fact about the composer would rest on
    // an older fact about whose composer it is.
    let watcher = match exact_prewrite_watcher(inner, handle, manifest_id) {
        Ok(watcher) => watcher,
        Err(failure) => {
            injector.discard().await;
            return AttemptOutcome::Failed(failure);
        }
    };
    if let Err(detail) = occupant_unchanged(inner, &watcher, handle, manifest_id, admitted_pid) {
        gate_line(inner, handle, "rebound", None, Some(&detail));
        injector.discard().await;
        let failure = if detail == "pane_gone" {
            AttemptFailure::session_detached()
        } else {
            AttemptFailure::pane_rebound_before_paste()
        };
        return AttemptOutcome::Failed(failure);
    }
    // The binding this write depends on, proven ONCE here, immediately
    // after the last capture that admitted it. Three lookups taken
    // separately can disagree with each other; this is one observation of
    // the leader, the agent and the rules that agent is running under.
    let Some(final_row) = watcher.pane(&handle.pane_id) else {
        injector.discard().await;
        return AttemptOutcome::Failed(AttemptFailure::session_detached());
    };
    let observed_binding = if inner
        .fail_next_final_binding_observation
        .swap(false, Ordering::SeqCst)
    {
        None
    } else {
        fusion::admitted_binding(inner, handle.session_idx, &final_row)
    };
    // Retain the last complete binding that this attempt genuinely observed.
    // If the terminal lookup itself is unavailable, this prior proof is the
    // durable baseline that prevents the unchanged occupant from looking like
    // a new route edge and reopening the same blocked attempt.
    let evidence_binding = observed_binding.as_ref().or(observed.as_ref());
    let observation =
        handle
            .notification
            .as_ref()
            .map(|notification| NotificationPreWriteObservation {
                pane_root: evidence_binding
                    .and_then(|binding| process_instance_id(binding.pane_root)),
                selected_manifest: Some(
                    NotificationManifestId::new(manifest_id)
                        .expect("loaded manifest ids are validated before delivery"),
                ),
                binding: evidence_binding
                    .and_then(|binding| notification_binding(notification.recipient(), binding)),
                route_evidence: Some(inner.route_evidence_id(handle.session_idx, &handle.pane_id)),
                pane_width: Some(final_row.width),
                required_pane_width: selected.required_pane_width(),
                write_block: None,
            });
    let proven = match observed_binding {
        // The gate admitted under a manifest, and the live read has to
        // still agree with it: a process that exec'd in place keeps its
        // identity while becoming another program.
        Some(binding) if binding.manifest == manifest_id => binding,
        _ => {
            // Widths are a paired observation used only by the pane-too-narrow
            // bookend below. Carrying either half through a binding failure
            // makes the durable observation invalid and strands the attempt.
            let observation = observation.map(|mut observation| {
                observation.pane_width = None;
                observation.required_pane_width = None;
                observation
            });
            gate_line(inner, handle, "rebound", None, Some("binding_unprovable"));
            injector.discard().await;
            return AttemptOutcome::Failed(AttemptFailure::binding_unprovable(observation));
        }
    };
    if let Some(cause) = notification_prewrite_bookend(
        &selected,
        handle
            .notification
            .as_ref()
            .map(NotificationContext::recipient),
        &proven,
        final_row.width,
    ) {
        injector.discard().await;
        if cause.starts_with("pane_too_narrow:") {
            return AttemptOutcome::Failed(AttemptFailure::pane_too_narrow(
                observation.expect("format 3 belongs to a notification"),
            ));
        }
        return AttemptOutcome::Failed(AttemptFailure::from_inject(cause));
    }
    inject_pause(inner, "post_final_prewrite").await;
    // The composer hold is installed AT the write boundary, by the injector,
    // not before the attempt and not after it resolves. Installing it before
    // the attempt would catch `spool_failed` and block its bounded transport
    // retry with no staged payload and no turn that could clear the hold.
    // Exhausted spool failures use the separate durable pre-write block.
    // Installing the composer hold after the attempt resolves would leave a
    // window where `verify_failed` (the paste may have
    // landed, nobody could prove what it did) is visible to another
    // delivery for the same pane before anything holds it.
    let target = match selected.transport {
        Some(NotificationTransport::Doorbell) => StagingTarget::ExactRow(&selected.bytes),
        Some(NotificationTransport::DirectPayload) | None => {
            StagingTarget::Sentinel(&handle.msg_id)
        }
    };
    let (staged_window, id_staged, payload_at_proof) = match inject(
        &injector,
        handle,
        manifest,
        target,
        &selected.bytes,
        &|| {
            if let Some(notification) = &handle.notification {
                notification
                    .ensure_current_gating()
                    .map_err(notification_write_cause)?;
            }
            // The last thing before the pane is asked to take the
            // payload: the same binding, read again, and equal. Nothing
            // has been written yet, so a change here is the world moving
            // rather than a transport failure.
            let (now, pane_width) = match handle_route(inner, handle) {
                HandleRoute::Exact(watcher) => {
                    let row = watcher
                        .pane(&handle.pane_id)
                        .ok_or_else(|| "prewrite_session_detached".to_string())?;
                    let binding = fusion::admitted_binding(inner, handle.session_idx, &row)
                        .ok_or_else(|| "prewrite_binding_unprovable".to_string())?;
                    (binding, row.width)
                }
                HandleRoute::BindingChanged => return Err("binding_changed".to_string()),
                HandleRoute::BindingUnprovable { .. } => {
                    return Err("prewrite_binding_unprovable".to_string())
                }
                HandleRoute::Unavailable => return Err("prewrite_session_detached".to_string()),
            };
            if now != proven {
                return Err("binding_changed".to_string());
            }
            if let Some(cause) = notification_prewrite_bookend(
                &selected,
                handle
                    .notification
                    .as_ref()
                    .map(NotificationContext::recipient),
                &now,
                pane_width,
            ) {
                return Err(cause);
            }
            let notification_binding = if handle.notification.is_some() {
                Some((
                    ProcessInstanceId::new(proven.pane_root.pid, proven.pane_root.birth)
                        .map_err(|_| NOTIFICATION_RECORD_FAILED.to_string())?,
                    ProcessInstanceId::new(proven.leader.pid, proven.leader.birth)
                        .map_err(|_| NOTIFICATION_RECORD_FAILED.to_string())?,
                    ProcessInstanceId::new(proven.agent.pid, proven.agent.birth)
                        .map_err(|_| NOTIFICATION_RECORD_FAILED.to_string())?,
                ))
            } else {
                None
            };
            latch_hold(inner, handle, &proven)?;
            let mut unwritten_hold = UnwrittenHold::new(inner, handle, &proven);
            let should_panic_attempt = {
                let current_attempt = handle.notification.as_ref().map(|n| n.attempt_id());
                let mut guard = inner.fail_pre_record_writing.lock().unwrap();
                if let Some(target) = *guard {
                    if current_attempt == Some(target) {
                        *guard = None;
                        Some(target)
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if let Some(target_attempt) = should_panic_attempt {
                panic!(
                    "worker exit at synchronous on_write boundary before first durable transition for attempt {target_attempt}"
                );
            }
            if let (Some(notification), Some((pane_root, leader, agent))) =
                (&handle.notification, notification_binding)
            {
                let transport = selected
                    .transport
                    .expect("notification attempts select a transport");
                if let Err(error) = notification.record_writing(
                    pane_root,
                    leader,
                    agent,
                    &proven.manifest,
                    transport,
                    selected.doorbell_format,
                ) {
                    return Err(notification_write_cause(error));
                }
            }
            handle.write_boundary_crossed.store(true, Ordering::SeqCst);
            unwritten_hold.commit();
            Ok(())
        },
    )
    .await
    {
        Ok(v) => v,
        Err(failure) => {
            return finish_attempt_delivery_inject_failure(
                inner,
                handle,
                &proven,
                observation,
                failure,
            );
        }
    };
    let mut staging_verified = !payload_at_proof.is_empty();
    if let Some(notification) = &handle.notification {
        if let Err(error) = notification.record_staged() {
            error!(id = %handle.msg_id, error = %error, "notification staged fact failed");
            return AttemptOutcome::Failed(AttemptFailure::notification_record_failed());
        }
    }
    if !advance(
        inner,
        handle,
        &[DeliveryState::Pasting],
        Step::to(DeliveryState::Staged),
    ) {
        return AttemptOutcome::Done;
    }

    let submit_key = if manifest.injection.submit.is_empty() {
        "Enter"
    } else {
        manifest.injection.submit.as_str()
    };
    inject_pause(inner, "pre_submit").await;
    if let Err(detail) = proven_binding_unchanged(inner, handle, &proven) {
        // The staged payload belongs to the occupant that verified it; the
        // submit key must never reach whoever replaced it.
        unregister_ack(inner, handle);
        gate_line(inner, handle, "rebound", None, Some(&detail));
        return AttemptOutcome::Failed(AttemptFailure::pane_rebound_after_paste());
    }
    // Verification proved a representation at a moment, and Enter is sent
    // at a later one. A person can append to the staged text, or replace
    // it, in between; pressing Enter then submits something nobody
    // verified and nobody wrote. Repaint is also not atomic: after a valid
    // paste proof, a capture can land between the terminal clear and the
    // renderer's next complete frame. Reuse the bounded post-paste evidence
    // schedule so that transient incomplete frames do not turn a clean,
    // owned doorbell into a false verify failure.
    let recheck = if staging_verified {
        match recheck_exact_staging_snapshot(
            &injector,
            &handle.pane_id,
            manifest,
            target,
            &selected.bytes,
            id_staged,
            &payload_at_proof,
        )
        .await
        {
            Ok(now) => now,
            Err(ExactStagingRecheck::Mismatch) => {
                unregister_ack(inner, handle);
                gate_line(inner, handle, "rebound", None, Some("staging_changed"));
                return AttemptOutcome::Failed(AttemptFailure::verify_mismatch(
                    ComposerState::ComposerAmbiguous,
                ));
            }
            Err(ExactStagingRecheck::Unobservable) if handle.notification.is_some() => {
                staging_verified = false;
                injector
                    .capture_joined_escaped(&handle.pane_id)
                    .await
                    .unwrap_or_default()
            }
            Err(ExactStagingRecheck::Unobservable) => {
                // Nobody looked, so nobody may press Enter.
                unregister_ack(inner, handle);
                gate_line(inner, handle, "rebound", None, Some("recheck_unobservable"));
                return AttemptOutcome::Failed(AttemptFailure::verify_timeout());
            }
        }
    } else {
        injector
            .capture_joined_escaped(&handle.pane_id)
            .await
            .unwrap_or_default()
    };
    // The capture above took time, so the occupant is checked once more
    // after it. Otherwise the last thing proven about who owns the pane is
    // older than the last thing proven about what is in it.
    if let Err(detail) = proven_binding_unchanged(inner, handle, &proven) {
        unregister_ack(inner, handle);
        gate_line(inner, handle, "rebound", None, Some(&detail));
        return AttemptOutcome::Failed(AttemptFailure::pane_rebound_after_paste());
    }
    if staging_verified {
        if let Err(detail) =
            notification_staged_action_safe(inner, handle, manifest, &recheck, &proven, true)
        {
            unregister_ack(inner, handle);
            gate_line(inner, handle, "rebound", None, Some(&detail));
            return AttemptOutcome::Failed(AttemptFailure::verify_failed());
        }
    }
    let notification_submit_reserved = if let Some(notification) = &handle.notification {
        match notification.reserve_submit() {
            Ok(SubmitReservation::Reserved) => true,
            Ok(SubmitReservation::ClaimedBeforeSubmit) => {
                return reconcile_claimed_notification_barrier(
                    inner,
                    handle,
                    manifest,
                    StagingExpectation {
                        target,
                        payload: &selected.bytes,
                    },
                    &proven,
                    &injector,
                    ClaimedStagedReconciliation::CurrentStaged,
                )
                .await;
            }
            Err(error) => {
                error!(id = %handle.msg_id, %error, "notification submit reservation failed");
                return AttemptOutcome::Failed(AttemptFailure::notification_record_failed());
            }
        }
    } else {
        false
    };
    // Reserving submit appends a journal fact and therefore opens another
    // content and process replacement window. Re-capture the composer and
    // re-prove the complete binding after that append. `Submitting` reserves
    // one key attempt; it never authorizes changed or unobservable bytes.
    if notification_submit_reserved && staging_verified {
        inject_pause(inner, "post_submit_reservation").await;
        match recheck_exact_staging_snapshot(
            &injector,
            &handle.pane_id,
            manifest,
            target,
            &selected.bytes,
            id_staged,
            &payload_at_proof,
        )
        .await
        {
            Ok(now) => {
                if let Err(detail) =
                    notification_staged_action_safe(inner, handle, manifest, &now, &proven, true)
                {
                    gate_line(
                        inner,
                        handle,
                        "rebound",
                        None,
                        Some(&format!("{detail}_after_submit_reservation")),
                    );
                    return AttemptOutcome::Failed(AttemptFailure::verify_failed());
                }
            }
            Err(ExactStagingRecheck::Mismatch) => {
                gate_line(
                    inner,
                    handle,
                    "rebound",
                    None,
                    Some("staging_changed_after_submit_reservation"),
                );
                return AttemptOutcome::Failed(AttemptFailure::verify_mismatch(
                    ComposerState::ComposerAmbiguous,
                ));
            }
            Err(ExactStagingRecheck::Unobservable) if handle.notification.is_some() => {
                staging_verified = false;
            }
            Err(ExactStagingRecheck::Unobservable) => {
                gate_line(
                    inner,
                    handle,
                    "rebound",
                    None,
                    Some("reserved_staging_unobservable"),
                );
                return AttemptOutcome::Failed(AttemptFailure::verify_timeout());
            }
        }
    }
    if !notification_submit_reserved {
        if let Err(detail) = proven_binding_unchanged(inner, handle, &proven) {
            unregister_ack(inner, handle);
            gate_line(inner, handle, "rebound", None, Some(&detail));
            return AttemptOutcome::Failed(AttemptFailure::pane_rebound_after_paste());
        }
    }
    // The occupant re-check just passed: admitted_pid IS the process the
    // submit key goes to. Send-and-wait pins its wait on this pid.
    // Subscribe before Enter. A fast vendor can paint its entire working
    // phase before send-keys returns, so subscribing inside the receipt
    // waiter loses the only turn evidence that actually followed this key.
    let receipt_events = inner.events.subscribe();
    // This receiver has one job: retain a matching working edge until a
    // screen checkpoint has accounted for it. The main receipt receiver
    // still owns session lifecycle and lag handling below. Keeping those
    // responsibilities separate means a screen receipt cannot settle first
    // and strand the `turn_ended` wait behind an already-observed turn.
    let receipt_turn_events = inner.events.subscribe();
    // Keep an independent receiver alive through receipt settlement and into
    // `wait_pinned`. It can establish only an exact post-submit Working fact;
    // the composed wait opens its own fresh stream for current and future
    // state, so an older Idle cannot replay as a current answer.
    handle.replace_post_submit_turn_events(inner.events.subscribe());
    let receipt_submit_at = Instant::now();
    let receipt_submit_at_ms = unix_ms();
    handle.submitted_pid.store(admitted_pid, Ordering::SeqCst);
    // And the AGENT behind it, which is what a hook report is filed
    // under. The foreground leader can be a tool the agent handed the
    // terminal to, so the two are recorded separately and never
    // substituted for one another: the leader is terminal admission
    // evidence, the agent identity is who this delivery belongs to.
    *handle.submitted_agent.lock().expect("submitted agent lock") = Some(proven.agent);
    handle
        .submitted_at_ms
        .store(receipt_submit_at_ms, Ordering::SeqCst);
    *handle
        .submitted_manifest
        .lock()
        .expect("submitted manifest lock") = Some(proven.manifest.clone());
    // Registered here, after every proof and immediately before the key:
    // the measured hook edge lands 21-28ms after Enter, so this is early
    // enough, and it closes the window where a stale ACK from the same
    // occupant could set the early flag before any submit was attempted.
    register_ack(inner, handle);
    if let Err(cause) = injector.submit(&handle.pane_id, submit_key).await {
        unregister_ack(inner, handle);
        debug_assert_eq!(cause, "submit_failed");
        return AttemptOutcome::Failed(AttemptFailure::submit_failed());
    }
    if let Some(notification) = &handle.notification {
        let record_res = if staging_verified {
            notification.record_submitted()
        } else {
            notification.record_submitted_unverified()
        };
        match record_res {
            Ok(_) => {}
            Err(error) => {
                error!(id = %handle.msg_id, error = %error, "notification submitted fact failed");
                return AttemptOutcome::Failed(AttemptFailure::notification_record_failed());
            }
        }
    }
    // The key is sent and the binding is recorded, so an acknowledgement
    // can arrive from here on, while the delivery is still `Staged`.
    // Always None in production.
    inject_pause(inner, "post_key").await;
    if !advance(
        inner,
        handle,
        &[DeliveryState::Staged],
        Step::to(DeliveryState::Submitted),
    ) {
        return AttemptOutcome::Done;
    }
    if !staging_verified {
        // One-time doorbell submitted unverified.
        // Enter was sent once, state is SubmittedUnverified, never duplicate Enter. Done.
        return AttemptOutcome::Done;
    }
    // Take any accepted early receipt before claim settlement can return.
    // A hook can carry the exact TurnKey while a concurrent socket claim has
    // already made the durable notification Notified. The claim must not
    // discard that stronger receipt.
    let early = take_accepted_early_ack(handle);
    let notified_during_submit_gap = if let Some(notification) = &handle.notification {
        match notification.settle_submitted_claim() {
            Ok(notified) => notified,
            Err(error) => {
                error!(id = %handle.msg_id, error = %error, "notification claim recheck failed");
                return AttemptOutcome::Failed(AttemptFailure::notification_record_failed());
            }
        }
    } else {
        false
    };
    if notified_during_submit_gap {
        if let Some(early) = early {
            advance_with_early_ack(inner, handle, early);
        } else {
            settle_notification_claim(
                inner,
                handle
                    .notification
                    .as_ref()
                    .expect("claim settlement belongs to a notification")
                    .attempt_id(),
            );
        }
        return AttemptOutcome::Done;
    }
    // The window this pause exists for: the delivery is Submitted after the
    // worker took any earlier record. A hook arriving now resolves the exact
    // submitted handle directly instead of installing another early record.
    // Always None in production.
    inject_pause(inner, "post_submit").await;
    // An acknowledgement that arrived between paste verification and the
    // Submitted line was taken under the same state lock the installer used.
    if let Some(early) = early {
        match record_notification_notified(inner, handle) {
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
    // Test-only boundary after receipt observation has finished but before
    // this worker publishes its delivery verdict. It proves the composed
    // wait owns a receiver with no observation gap after an early receipt.
    inject_pause(inner, "post_receipt").await;
    match ack_outcome {
        AckOutcome::Resolved => AttemptOutcome::Done,
        AckOutcome::Screen => {
            // Stays registered: a late matching hook ACK upgrades it to
            // delivered_verified (the legal upgrade transition).
            match record_notification_notified(inner, handle) {
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
        AckOutcome::Timeout => {
            unregister_ack(inner, handle);
            AttemptOutcome::Failed(AttemptFailure::ack_timeout())
        }
        AckOutcome::Rebound => {
            unregister_ack(inner, handle);
            AttemptOutcome::Failed(AttemptFailure::receipt_occupant_changed())
        }
    }
}

/// Resolve the exact injector failure arm of [`attempt_delivery`].
///
/// Keeping the durable correction, runtime boundary, and composer hold in one
/// arm makes their order directly testable without a live tmux process.
pub(crate) fn finish_attempt_delivery_inject_failure(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    proven: &fusion::Binding,
    observation: Option<NotificationPreWriteObservation>,
    failure: InjectFailure,
) -> AttemptOutcome {
    match failure {
        InjectFailure::PasteCommandUnwritten => {
            if let Err(error) = correct_proven_unwritten_paste(handle) {
                error!(id = %handle.msg_id, error = %error, "notification unwritten correction failed");
                return AttemptOutcome::Failed(AttemptFailure::notification_record_failed());
            }
            rollback_unwritten_hold(inner, handle, proven);
            AttemptOutcome::Failed(AttemptFailure::paste_command_unwritten())
        }
        InjectFailure::Other(cause) => {
            if cause == NO_LONGER_CURRENT_BEFORE_WRITE {
                return AttemptOutcome::NoLongerCurrentBeforeWrite;
            }
            if let Some(width) = cause
                .strip_prefix("pane_too_narrow:")
                .and_then(|width| width.parse::<u32>().ok())
            {
                let mut observation = observation.expect("format 3 belongs to a notification");
                observation.pane_width = Some(width);
                return AttemptOutcome::Failed(AttemptFailure::pane_too_narrow(observation));
            }
            AttemptOutcome::Failed(AttemptFailure::from_inject(cause))
        }
    }
}

/// Re-prove that an automatic notification submit still owns this exact
/// staged composer. The caller separately compares the normalized bytes.
/// This check binds that content to the current process generations and
/// manifest, requires a terminal-safe visual state, and refuses any known
/// blocked-state or final-submit conflict. An ordinary in-flight notification
/// can use the exact proof when a vendor's short screen projection loses the
/// prompt row to chrome; recovery and terminal clear paths stay on the
/// quiet-frame rule.
pub(crate) fn notification_staged_action_safe(
    inner: &Arc<Inner>,
    handle: &DeliveryHandle,
    manifest: &Manifest,
    capture: &str,
    proven: &fusion::Binding,
    allow_inflight_working_admission: bool,
) -> Result<(), String> {
    let Some(notification) = &handle.notification else {
        return Ok(());
    };
    let Some(watcher) = watcher_for_handle(inner, handle) else {
        return Err("session_detached".to_string());
    };
    let Some(row) = watcher.pane(&handle.pane_id) else {
        return Err("pane_gone".to_string());
    };
    if row.dead {
        return Err("pane_dead".to_string());
    }
    if row.in_mode {
        return Err("pane_in_mode".to_string());
    }
    let current = fusion::admitted_binding(inner, handle.session_idx, &row);
    if !binding_is_exact(current.as_ref(), proven) {
        return Err("binding_changed".to_string());
    }
    let state = manifest
        .evaluate_esc(&row.title, &strip_csi(capture), Some(capture))
        .map(|rule| rule.state);
    if matches!(
        state,
        Some(
            AgentState::BlockedModal
                | AgentState::BlockedPermission
                | AgentState::BlockedQuota
                | AgentState::Dead
        )
    ) {
        return Err("staged_manifest_state_unsafe".to_string());
    }
    let Some(agent) = process_instance_id(proven.agent) else {
        return Err("binding_unprovable".to_string());
    };
    // Exact bytes and an unchanged binding are stronger than a fixed tail
    // window that happened to omit a long wrapped prompt. This is deliberately
    // limited to a non-Working normal post-paste submit: a freshly observed
    // Working edge still needs the separately recorded clean-composer
    // admission. Claim recovery and terminal clear retain the stricter
    // quiet-frame rule below.
    if allow_inflight_working_admission
        && fusion::staged_exact_submit_ready(
            inner,
            handle.session_idx,
            &handle.pane_id,
            &notification.attempt_id().to_string(),
            agent,
            &proven.manifest,
        )
    {
        return Ok(());
    }
    let working_clean_submit = allow_inflight_working_admission
        && state == Some(AgentState::Working)
        && handle.working_clean_submit_admitted();
    if !matches!(state, Some(AgentState::Idle | AgentState::IdleWithInput)) && !working_clean_submit
    {
        return Err("staged_manifest_state_unsafe".to_string());
    }
    let quiet_staged_action = fusion::staged_action_ready(
        inner,
        handle.session_idx,
        &handle.pane_id,
        &notification.attempt_id().to_string(),
        agent,
        &proven.manifest,
    );
    let working_staged_action = working_clean_submit
        && fusion::staged_working_clean_action_ready(
            inner,
            handle.session_idx,
            &handle.pane_id,
            &notification.attempt_id().to_string(),
            agent,
            &proven.manifest,
        );
    if !quiet_staged_action && !working_staged_action {
        return Err("staged_action_unsafe".to_string());
    }
    Ok(())
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

pub(crate) async fn reconcile_recovered_claimed_notification_barrier(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    barrier: ClaimedNotificationBarrier,
) -> AttemptOutcome {
    let notification = handle
        .notification
        .as_ref()
        .expect("staged recovery belongs to a notification");
    let record = match notification.current_record() {
        Ok(record) => record,
        Err(_) => {
            return AttemptOutcome::Failed(AttemptFailure::notification_record_failed());
        }
    };
    let message = match notification.message_line() {
        Ok(message) => message,
        Err(_) => {
            return AttemptOutcome::Failed(AttemptFailure::from_inject(
                "claim_recovery_message_missing".to_string(),
            ));
        }
    };
    let Some(expected) = expected_notification_payload(&record, &message) else {
        return AttemptOutcome::Failed(AttemptFailure::from_inject(
            "claim_recovery_format_unknown".to_string(),
        ));
    };
    let Some(binding) = record.binding.as_ref() else {
        return AttemptOutcome::Failed(AttemptFailure::from_inject(
            "claim_recovery_binding_missing".to_string(),
        ));
    };
    let Some(pane_root) = binding.pane_root else {
        return AttemptOutcome::Failed(AttemptFailure::from_inject(
            "claim_recovery_pane_root_missing".to_string(),
        ));
    };
    let Some(leader) = binding.leader else {
        return AttemptOutcome::Failed(AttemptFailure::from_inject(
            "claim_recovery_leader_missing".to_string(),
        ));
    };
    let proven = fusion::Binding {
        pane_root: crate::identity::ProcId {
            pid: pane_root.pid(),
            birth: pane_root.birth(),
        },
        leader: crate::identity::ProcId {
            pid: leader.pid(),
            birth: leader.birth(),
        },
        agent: crate::identity::ProcId {
            pid: binding.agent.pid(),
            birth: binding.agent.birth(),
        },
        manifest: binding.manifest.as_str().to_string(),
    };
    let Some(manifest) = inner.manifests.get(&proven.manifest) else {
        return AttemptOutcome::Failed(AttemptFailure::from_inject(
            "claim_recovery_manifest_missing".to_string(),
        ));
    };
    let Some(watcher) = watcher_for_handle(inner, handle) else {
        return AttemptOutcome::Failed(AttemptFailure::from_inject(
            "claim_recovery_route_unavailable".to_string(),
        ));
    };
    let injector = TmuxInjector {
        client: watcher.client(),
        buffer: format!(
            "cyc-{}-{}",
            std::process::id(),
            inner.engine.buffer_seq.fetch_add(1, Ordering::Relaxed)
        ),
    };
    reconcile_claimed_notification_barrier(
        inner,
        handle,
        manifest,
        StagingExpectation {
            target: StagingTarget::ExactRow(&expected),
            payload: &expected,
        },
        &proven,
        &injector,
        ClaimedStagedReconciliation::Recovered(barrier),
    )
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaimedStagedReconciliation {
    CurrentStaged,
    Recovered(ClaimedNotificationBarrier),
}

impl ClaimedStagedReconciliation {
    pub(crate) fn barrier(self) -> ClaimedNotificationBarrier {
        match self {
            Self::CurrentStaged => ClaimedNotificationBarrier::Staged,
            Self::Recovered(barrier) => barrier,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct StagingExpectation<'a> {
    pub(crate) target: StagingTarget<'a>,
    pub(crate) payload: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaimedStagedComposer {
    ExactDoorbell,
    Clean,
    Ambiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaimedStagedAction {
    ClearThenSettle,
    SettleOnly,
    Refuse,
}

pub(crate) fn claimed_staged_action(
    composer: ClaimedStagedComposer,
    reconciliation: ClaimedStagedReconciliation,
) -> ClaimedStagedAction {
    match (composer, reconciliation) {
        (ClaimedStagedComposer::ExactDoorbell, _) => ClaimedStagedAction::ClearThenSettle,
        (ClaimedStagedComposer::Clean, ClaimedStagedReconciliation::Recovered(_)) => {
            ClaimedStagedAction::SettleOnly
        }
        (ClaimedStagedComposer::Clean, ClaimedStagedReconciliation::CurrentStaged)
        | (ClaimedStagedComposer::Ambiguous, _) => ClaimedStagedAction::Refuse,
    }
}

pub(crate) fn classify_claimed_staged_composer(
    manifest: &Manifest,
    capture: &str,
    target: StagingTarget<'_>,
    expected_payload: &str,
) -> ClaimedStagedComposer {
    if exact_staging_proof(manifest, capture, target, expected_payload).is_some() {
        return ClaimedStagedComposer::ExactDoorbell;
    }
    if clean_composer_proof(manifest, capture) {
        return ClaimedStagedComposer::Clean;
    }
    ClaimedStagedComposer::Ambiguous
}

/// Reconcile an exact claimed notification barrier.
///
/// The claim proves payload retrieval, not Enter. Cyclops clears only an
/// exact, still-bound doorbell that it can reconstruct byte for byte. Any
/// missing proof becomes one post-write attention state.
pub(crate) async fn reconcile_claimed_notification_barrier<I: Injector>(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    manifest: &Manifest,
    staging: StagingExpectation<'_>,
    proven: &fusion::Binding,
    injector: &I,
    reconciliation: ClaimedStagedReconciliation,
) -> AttemptOutcome {
    if proven_binding_unchanged(inner, handle, proven).is_err() {
        return AttemptOutcome::Failed(AttemptFailure::pane_rebound_after_paste());
    }
    let Some(watcher) = watcher_for_handle(inner, handle) else {
        return AttemptOutcome::Failed(AttemptFailure::pane_rebound_after_paste());
    };
    let staged = match injector.capture_joined_escaped(&handle.pane_id).await {
        Ok(screen) => screen,
        Err(_) => return AttemptOutcome::Failed(AttemptFailure::verify_timeout()),
    };
    if proven_binding_unchanged(inner, handle, proven).is_err() {
        return AttemptOutcome::Failed(AttemptFailure::pane_rebound_after_paste());
    }
    let Some(row) = watcher.pane(&handle.pane_id) else {
        return AttemptOutcome::Failed(AttemptFailure::pane_rebound_after_paste());
    };
    if row.in_mode {
        return AttemptOutcome::Failed(AttemptFailure::verify_failed());
    }

    let composer =
        classify_claimed_staged_composer(manifest, &staged, staging.target, staging.payload);
    match claimed_staged_action(composer, reconciliation) {
        ClaimedStagedAction::ClearThenSettle => {
            if manifest.injection.clear_keys.is_empty() {
                return AttemptOutcome::Failed(AttemptFailure::from_inject(
                    "claim_clear_unsupported".to_string(),
                ));
            }
            if let Err(cause) =
                notification_staged_action_safe(inner, handle, manifest, &staged, proven, false)
            {
                return AttemptOutcome::Failed(AttemptFailure::from_inject(cause));
            }
            if let Err(cause) = injector
                .clear(&handle.pane_id, &manifest.injection.clear_keys)
                .await
            {
                return AttemptOutcome::Failed(AttemptFailure::from_inject(cause));
            }
            if !observe_exact_composer_clear(inner, handle, manifest, proven, injector).await {
                return AttemptOutcome::Failed(AttemptFailure::from_inject(
                    "claim_clear_unconfirmed".to_string(),
                ));
            }
        }
        ClaimedStagedAction::SettleOnly => {
            // A crash can land after exact clear but before the settlement
            // fact. The fresh clean observation and exact process binding
            // authorize only the missing durable settlement. No terminal
            // input is sent again.
        }
        ClaimedStagedAction::Refuse => {
            let failure = match composer {
                ClaimedStagedComposer::Clean => {
                    AttemptFailure::verify_owner_missing(ComposerState::ComposerClean)
                }
                ClaimedStagedComposer::Ambiguous => AttemptFailure::verify_failed(),
                ClaimedStagedComposer::ExactDoorbell => {
                    unreachable!("an exact claimed doorbell cannot select the refusal action")
                }
            };
            return AttemptOutcome::Failed(failure);
        }
    }

    if proven_binding_unchanged(inner, handle, proven).is_err() {
        return AttemptOutcome::Failed(AttemptFailure::pane_rebound_after_paste());
    }
    inject_pause(inner, "pre_claimed_notification_settlement").await;
    let notification = handle
        .notification
        .as_ref()
        .expect("claim reconciliation belongs to a notification");
    let record = match settle_claimed_notification_after_clear(
        notification,
        reconciliation.barrier(),
    ) {
        Ok(record) => record,
        Err(error) => {
            error!(id = %handle.msg_id, %error, "claimed notification settlement failed twice; notification worker remains faulted");
            return AttemptOutcome::Failed(AttemptFailure::claimed_staged_settlement_failed());
        }
    };
    if let Some(binding) = record.binding.as_ref() {
        fusion::resolve_staged_hold(
            inner,
            handle.session_idx,
            &handle.pane_id,
            &record.attempt_id.to_string(),
            binding.agent,
            binding.manifest.as_str(),
        )
        .await;
    }
    if let Some(messaging) = inner.workspace_messaging() {
        messaging.composer_barrier_retired(record.attempt_id);
        if let Err(error) = messaging.notification_head_changed(notification.recipient()) {
            error!(id = %handle.msg_id, %error, "cannot advance notification FIFO after staged claim");
        }
    } else {
        error!(
            id = %handle.msg_id,
            "cannot advance notification FIFO after staged claim without workspace messaging"
        );
    }
    AttemptOutcome::Done
}

/// Retry only the content-free durable settlement after a proven clear.
///
/// The first error may be an interrupted append whose outcome the caller did
/// not observe. The store operation is idempotent, so one immediate repeat can
/// discover an already-landed fact or append the missing one. It never clears
/// the composer or sends a terminal key.
pub(crate) fn settle_claimed_notification_after_clear(
    notification: &NotificationContext,
    barrier: ClaimedNotificationBarrier,
) -> Result<cyclops_proto::NotificationRecord, NotificationAdapterError> {
    let settle = || match barrier {
        ClaimedNotificationBarrier::Staged => notification.settle_claimed_staged_clear(),
        ClaimedNotificationBarrier::AckTimeout => {
            notification.settle_claimed_ack_timeout_reconciliation()
        }
    };
    match settle() {
        Ok(record) => Ok(record),
        Err(first) => {
            warn!(
                message_id = %notification.message_id(),
                attempt_id = %notification.attempt_id(),
                error = %first,
                "retrying claimed notification settlement once"
            );
            settle()
        }
    }
}

#[cfg(test)]
pub(crate) fn settle_claimed_staged_after_clear(
    notification: &NotificationContext,
) -> Result<cyclops_proto::NotificationRecord, NotificationAdapterError> {
    settle_claimed_notification_after_clear(notification, ClaimedNotificationBarrier::Staged)
}

pub(crate) async fn observe_exact_composer_clear<I: Injector>(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    manifest: &Manifest,
    proven: &fusion::Binding,
    injector: &I,
) -> bool {
    let mut last_delay = 0;
    for delay in VERIFY_DELAYS_MS {
        if delay > last_delay {
            tokio::time::sleep(Duration::from_millis(delay - last_delay)).await;
        }
        last_delay = delay;
        let Some(watcher) = watcher_for_handle(inner, handle) else {
            return false;
        };
        if proven_binding_unchanged(inner, handle, proven).is_err() {
            return false;
        }
        let Ok(capture) = injector.capture_joined_escaped(&handle.pane_id).await else {
            continue;
        };
        let Some(row) = watcher.pane(&handle.pane_id) else {
            return false;
        };
        if proven_binding_unchanged(inner, handle, proven).is_err() {
            return false;
        }
        if !row.in_mode
            && clean_composer_proof(manifest, &capture)
            && notification_staged_action_safe(inner, handle, manifest, &capture, proven, false)
                .is_ok()
        {
            return true;
        }
    }
    false
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

/// Pane-rebind re-check between the gate's admitting recompute and the
/// irreversible injection steps. The pane must still exist, be alive, keep
/// the pid it was admitted with, and bind to the manifest the gate
/// admitted. Err carries the mismatch detail for the gate ledger line; the
/// delivery then retries through the gate, which re-evaluates from scratch.
pub(crate) fn occupant_unchanged(
    inner: &Arc<Inner>,
    watcher: &Arc<SessionWatcher>,
    handle: &Arc<DeliveryHandle>,
    manifest_id: &str,
    admitted_pid: i32,
) -> Result<(), String> {
    let Some(row) = watcher.pane(&handle.pane_id) else {
        return Err("pane_gone".to_string());
    };
    if row.dead {
        return Err("pane_dead".to_string());
    }
    // Copy-mode after admission: the human is scrolling, and a paste now
    // lands somewhere neither of us can see. The gate checks this before
    // admitting, but admission is a decision about a moment and the human
    // can enter copy-mode inside the window that follows.
    if row.in_mode {
        return Err("pane_in_mode".to_string());
    }
    if fusion::foreground_pid(row.pane_pid) != admitted_pid {
        return Err("pane_pid_changed".to_string());
    }
    match fusion::bind_manifest_for(inner, handle.session_idx, &row) {
        Some(m) if m.agent.id == manifest_id => Ok(()),
        Some(_) => Err("manifest_changed".to_string()),
        None => Err("manifest_unbound".to_string()),
    }
}

/// Re-prove the complete binding captured at the write boundary.
///
/// PID numbers alone are reusable. The submit path must retain the same
/// terminal leader generation, admitted agent generation, and manifest that
/// authorized the paste.
pub(crate) fn proven_binding_unchanged(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    proven: &fusion::Binding,
) -> Result<(), String> {
    let Some(watcher) = watcher_for_handle(inner, handle) else {
        return Err("session_detached".to_string());
    };
    let Some(row) = watcher.pane(&handle.pane_id) else {
        return Err("pane_gone".to_string());
    };
    if row.dead {
        return Err("pane_dead".to_string());
    }
    if row.in_mode {
        return Err("pane_in_mode".to_string());
    }
    let Some(current) = fusion::admitted_binding(inner, handle.session_idx, &row) else {
        return Err("binding_unprovable".to_string());
    };
    if !binding_is_exact(Some(&current), proven) {
        return Err("binding_changed".to_string());
    }
    Ok(())
}

pub(crate) fn binding_is_exact(
    current: Option<&fusion::Binding>,
    proven: &fusion::Binding,
) -> bool {
    current == Some(proven)
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

/// A notification may continue without a clean-composer proof only for an
/// A notification may continue without a clean-composer proof for an
/// authenticated agent unless positive human input or a modal is present.
/// Cyclops must never type over a person's active text.
pub(crate) fn unproven_composer_is_eligible(detection: &Detection) -> bool {
    if detection.composer_semantic == Some(ComposerSemantic::HumanInput)
        || matches!(
            detection.state,
            AgentState::BlockedModal
                | AgentState::BlockedPermission
                | AgentState::BlockedQuota
                | AgentState::Dead
        )
        || detection.write_block.as_deref() == Some("composer_hold")
        || detection.write_block.as_deref() == Some("pane_in_mode")
    {
        return false;
    }
    true
}

/// Return the current foreground agent process for the explicit liveness
/// policy. Unreadable composers do not block a notification, but a stale or
/// mismatched process binding still does: that would risk typing into a shell
/// or a different agent.
pub(crate) fn notification_pane_for_unproven_composer(
    inner: &Inner,
    handle: &DeliveryHandle,
    row: &PaneRow,
    manifest_id: &str,
    detection: &Detection,
) -> Option<i32> {
    if !unproven_composer_is_eligible(detection) {
        return None;
    }
    if crate::deadlock::pane_runs_watch(row.pane_pid) {
        return None;
    }
    let binding = fusion::admitted_binding(inner, handle.session_idx, row)?;
    if binding.manifest != manifest_id {
        return None;
    }
    fusion::foreground_pid_checked(row.pane_pid)
}

#[allow(dead_code)]
pub(crate) fn composer_semantic_missing(manifest: &Manifest, detection: &Detection) -> bool {
    detection
        .readings
        .iter()
        .find(|reading| {
            reading.sensor == cyclops_proto::Sensor::Screen && reading.state == AgentState::Idle
        })
        .and_then(|reading| manifest.rules.iter().find(|rule| rule.id == reading.rule))
        .is_some_and(|rule| rule.composer_semantic.is_none())
}

pub(crate) fn composer_semantic_observation(
    inner: &Inner,
    handle: &DeliveryHandle,
    row: &PaneRow,
    manifest_id: &str,
) -> Option<NotificationPreWriteObservation> {
    let notification = handle.notification.as_ref()?;
    let binding = fusion::admitted_binding(inner, handle.session_idx, row)?;
    if binding.manifest != manifest_id {
        return None;
    }

    Some(NotificationPreWriteObservation {
        pane_root: Some(process_instance_id(binding.pane_root)?),
        selected_manifest: Some(NotificationManifestId::new(&binding.manifest).ok()?),
        binding: Some(notification_binding(notification.recipient(), &binding)?),
        route_evidence: Some(inner.route_evidence_id(handle.session_idx, &handle.pane_id)),
        pane_width: None,
        required_pane_width: None,
        write_block: None,
    })
}

pub(crate) fn notification_binding(
    recipient: RecipientKey,
    binding: &fusion::Binding,
) -> Option<NotificationBinding> {
    Some(NotificationBinding {
        recipient,
        pane_root: Some(process_instance_id(binding.pane_root)?),
        leader: Some(process_instance_id(binding.leader)?),
        agent: process_instance_id(binding.agent)?,
        manifest: NotificationManifestId::new(&binding.manifest).ok()?,
    })
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

/// The gate hold cause that no pane event will ever clear: the daemon
/// could not read who is in the pane.
pub(crate) const OBSERVATION_HOLD: &str = "occupant_unprovable";
/// The gate hold for an idle pane whose composer keeps reading `ambiguous`.
/// Held on events like any composer cause, but also on a timed wake at the
/// settle boundary: ambiguity that never changes emits no pane event, and
/// without the timer the wake would wait in memory forever instead of
/// settling as the durable `composer_semantic_ambiguous` block.
pub(crate) const AMBIGUOUS_COMPOSER_HOLD: &str = "not_write_ready:composer_semantic_ambiguous";
pub(crate) const WRITE_READINESS_OBSERVATION_HOLD: &str = "not_write_ready:occupant_unprovable";
/// The write block a hook-liveness manifest stamps when no admitting hook
/// edge has been published for the pane's current binding. Durable, never
/// retried: the wake parks as a named pre-write block until the recipient
/// claims, its next admitting edge reopens the oldest attempt once, or an
/// administrator withdraws the exact attempt.
pub(crate) const HOOK_ADMISSION_UNPROVEN: &str = "hook_admission_unproven";

/// How long that one cause waits before looking again. Short enough that
/// a transient `ps` failure costs a person nothing, long enough that a
/// permanently unreadable process table is not a spin.
pub(crate) const OBSERVATION_RETRY: Duration = Duration::from_millis(250);

/// Mailbox attempts remain in workspace Gating while an event can change the
/// answer. Named exhausted failures settle as durable BlockedPreWrite records.
/// Direct deliveries retain the legacy attention and quota outcomes.
pub(crate) fn workspace_prewrite_hold(handle: &DeliveryHandle, cause: &str) -> Option<String> {
    (!handle.owns_session_delivery_state()).then(|| cause.to_string())
}

pub(crate) fn gate_hold_action(handle: &DeliveryHandle, cause: &str) -> &'static str {
    if !handle.owns_session_delivery_state() && cause == "blocked_quota" {
        "wait"
    } else {
        "hold"
    }
}

pub(crate) fn workspace_prewrite_failure_is_deferred(
    handle: &DeliveryHandle,
    failure: &AttemptFailure,
) -> bool {
    !handle.owns_session_delivery_state() && matches!(failure.boundary, WriteBoundary::BeforeWrite)
}

/// Retry accounting. Only failures proven to precede the pane write may
/// consume the configured retry budget. True means the caller should retry
/// immediately. False means a direct delivery ended in attention_required or
/// a workspace notification remains durably held or blocked for recovery.
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
    if should_retry_attempt(handle, failure, spent, inner.cfg.delivery_retry_max) {
        advance(
            inner,
            handle,
            &from,
            Step::to(DeliveryState::RetryQueued).cause(&failure.cause),
        )
    } else {
        if let (true, Some(block)) = (
            handle.notification.is_some(),
            failure.pre_write_block.as_deref(),
        ) {
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
        if workspace_prewrite_failure_is_deferred(handle, failure) {
            // The workspace attempt remains Gating. No pane write happened,
            // so a terminal notification would be false and a legacy state
            // would create a second authority. A later route event or daemon
            // restart can attach a fresh worker to the same durable attempt.
            notify_notification_deferred(inner, handle, &failure.cause);
            return false;
        }
        if matches!(failure.boundary, WriteBoundary::AfterWrite) {
            if let Some(notification) = &handle.notification {
                let result = match failure.verify_outcome {
                    Some(outcome) => notification.record_verify_attention(outcome),
                    None => {
                        notification.record_attention(notification_attention_cause(&failure.cause))
                    }
                };
                match result {
                    Ok(record) => {
                        if let Some(messaging) = inner.workspace_messaging() {
                            messaging.notification_attention_recorded(record);
                        }
                    }
                    Err(NotificationAdapterError::TerminalConflict(_)) => return false,
                    Err(error) => {
                        // The workspace journal remains at its last
                        // post-write state. Explicit restart recovery can
                        // close it without risking another pane write.
                        //
                        // That is the right durable choice and it used to
                        // be the whole response, which left the attempt
                        // invisible: still in flight, so `open_alarms`
                        // skips it (it filters on AttentionRequired), no
                        // wake block, so the scheduler reports nothing to
                        // do, and the recipient's head never advances. The
                        // pre-write sibling already faults the worker on a
                        // storage failure (`record_notification_prewrite_
                        // block`), so this is the same failure reported the
                        // same way rather than a new mechanism: the fault
                        // reaches `notification_worker_diagnostics` and so
                        // `cyclops status`, which is where an operator
                        // learns that this daemon needs a restart.
                        error!(id = %handle.msg_id, error = %error, "notification attention fact failed");
                        worker.set_fault(format!("notification attention storage failed: {error}"));
                    }
                }
            }
        }
        let moved = advance(
            inner,
            handle,
            &from,
            Step::to(DeliveryState::AttentionRequired).cause(&failure.cause),
        );
        if moved {
            notify_attention(inner, handle, &failure.cause);
        }
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
        "verify_failed" => NotificationAttentionCause::VerifyFailed,
        "pane_rebound_after_paste" => NotificationAttentionCause::PaneReboundAfterPaste,
        "submit_failed" => NotificationAttentionCause::SubmitFailed,
        "receipt_occupant_changed" => NotificationAttentionCause::ReceiptOccupantChanged,
        "ack_timeout" => NotificationAttentionCause::AckTimeout,
        _ => NotificationAttentionCause::TransportOutcomeUnknown,
    }
}

pub(crate) fn should_retry(failure: &AttemptFailure, spent: u32, retry_max: u32) -> bool {
    // Unproven hook admission is a durable block, never a retry budget
    // question: only an admitting edge, a claim, or a withdrawal moves it.
    if failure.cause == HOOK_ADMISSION_UNPROVEN {
        return false;
    }
    matches!(failure.boundary, WriteBoundary::BeforeWrite)
        && !matches!(
            failure.cause.as_str(),
            "pane_too_narrow" | "composer_ownership_unproven" | "binding_unprovable"
        )
        && spent <= retry_max
}

pub(crate) fn should_retry_attempt(
    handle: &DeliveryHandle,
    failure: &AttemptFailure,
    spent: u32,
    retry_max: u32,
) -> bool {
    // A workspace notification already has a durable Writing fact. Its exact
    // zero-byte correction must remain withdrawable instead of being replayed
    // automatically. Legacy direct delivery has no such durable attempt and
    // may use the existing bounded pre-write retry.
    !(handle.notification.is_some() && failure.cause == "paste_command_unwritten")
        && should_retry(failure, spent, retry_max)
}

pub(crate) fn notify_attention(inner: &Arc<Inner>, handle: &Arc<DeliveryHandle>, cause: &str) {
    if !handle.owns_session_delivery_state() {
        // Workspace NotificationState and messages.changed own mailbox
        // attention. A delivery-scoped ping would point at the suppressed
        // session projection and could never observe guarded resolution.
        return;
    }
    admin_notify(
        inner,
        NotifyLevel::ActionRequired,
        &format!("delivery to {} needs attention", handle.to),
        &format!("message {}: {cause}", handle.msg_id),
        Some(&handle.msg_id),
        Some(handle.session_idx),
        About::delivery(&handle.to),
    );
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

/// Quota parking: the in-flight delivery and everything queued behind it
/// park, the worker is flagged, and the admin is alerted once with the
/// reset hint. Nothing here ever requeues automatically.
pub(crate) async fn park_recipient(
    inner: &Arc<Inner>,
    worker: &Arc<Worker>,
    handle: &Arc<DeliveryHandle>,
    hint: Option<String>,
) {
    if let Some(notification) = &handle.notification {
        match notification.record_quota_held() {
            Ok(_) => {}
            Err(NotificationAdapterError::NoLongerCurrentBeforeWrite) => return,
            Err(error) => {
                error!(id = %handle.msg_id, error = %error, "notification quota-held fact failed");
                notify_notification_deferred(inner, handle, NOTIFICATION_RECORD_FAILED);
                return;
            }
        }
        advance(
            inner,
            handle,
            &[DeliveryState::Gating],
            Step::to(DeliveryState::ParkedBlockedQuota).cause("blocked_quota"),
        );
        let hint = hint.unwrap_or_else(|| "quota exhausted".to_string());
        admin_notify(
            inner,
            NotifyLevel::Urgent,
            &format!("{} held: quota exhausted", handle.to),
            &format!(
                "message {} to {} is held ({hint}); it will not resume automatically",
                handle.msg_id, handle.to
            ),
            Some(&handle.msg_id),
            Some(handle.session_idx),
            About::delivery(&handle.to),
        );
        // The positive reset edge can race this durable hold append. If it
        // already won, the edge's scan found no held attempt. Recheck the
        // exact route once after the hold exists so the attempt cannot be
        // stranded until another unrelated redraw.
        if let Some(observation) =
            fusion::quota_reset_observation_now(inner, handle.session_idx, &handle.pane_id)
        {
            crate::apply_messaging_observation(inner, observation);
        }
        return;
    }
    let hint = hint.unwrap_or_else(|| "quota exhausted".to_string());
    *worker.parked.lock().expect("parked lock") = Some(hint.clone());
    advance(
        inner,
        handle,
        &[DeliveryState::Gating],
        Step::to(DeliveryState::ParkedBlockedQuota)
            .cause("blocked_quota")
            .note(hint.clone()),
    );
    let drained = worker.drain_pending();
    let (direct, notifications) = split_legacy_parked_queue(drained);
    for h in direct {
        advance(
            inner,
            &h,
            &[DeliveryState::Queued],
            Step::to(DeliveryState::ParkedBlockedQuota)
                .cause("blocked_quota")
                .note(hint.clone()),
        );
    }
    if !notifications.is_empty() {
        // These handles were ahead of anything enqueued after the drain.
        worker.prepend(notifications);
        worker.notify.notify_one();
    }
    admin_notify(
        inner,
        NotifyLevel::Urgent,
        &format!("{} parked: quota exhausted", handle.to),
        &format!(
            "deliveries to {} are parked ({hint}); re-queue is an operator action",
            handle.to
        ),
        Some(&handle.msg_id),
        Some(handle.session_idx),
        About::delivery(&handle.to),
    );
}

pub(crate) fn legacy_park_hint(handle: &DeliveryHandle, hint: Option<String>) -> Option<String> {
    hint.filter(|_| handle.owns_session_delivery_state())
}

pub(crate) fn split_legacy_parked_queue(
    handles: Vec<Arc<DeliveryHandle>>,
) -> (Vec<Arc<DeliveryHandle>>, Vec<Arc<DeliveryHandle>>) {
    handles
        .into_iter()
        .partition(|handle| handle.owns_session_delivery_state())
}

// ---------------------------------------------------------------------------
// Gate
// ---------------------------------------------------------------------------

pub(crate) enum GateOutcome {
    Proceed {
        manifest_id: String,
        /// pane_pid of the admitted occupant, re-checked before paste and
        /// submit: a pane whose occupant changed after admit must never be
        /// injected into.
        pane_pid: i32,
        /// The regate hold observed an exact state, readiness, or pane edge.
        /// Only this grants a fresh immediate re-proof allowance.
        regate_evidence_changed: bool,
    },
    Park {
        hint: Option<String>,
    },
    Attention {
        cause: String,
    },
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

/// The delivery gate, in spec order: pane resolution and liveness, mode,
/// fused state (quota park, modal decline-or-hold, working composer proof,
/// idle_with_input hold, idle proceed). Event-driven: holds wake on fused
/// state changes, pane field changes, and session reattach. The recompute
/// that admits a delivery runs immediately before pasting, so the gate
/// snapshot is fresher than any human keystroke round-trip.
pub(crate) async fn gate(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    initial_hold: Option<String>,
) -> GateOutcome {
    let mut declines: HashMap<String, u32> = HashMap::new();
    let mut notified_rules: HashSet<String> = HashSet::new();
    let mut last_hold: Option<String> = None;
    let mut forced_hold = initial_hold;
    let mut regate_evidence_changed = false;
    // One-shot visibility for wedged holds: a delivery held in gating past
    // the configured threshold pings the admin exactly once.
    let mut hold_since: Option<Instant> = None;
    let mut hold_notified = false;
    // Subscribe once before the first evaluation and retain this receiver for
    // the gate's whole lifetime. Replacing it between re-evaluations leaves a
    // gap where a settled readiness edge can be published after an early pane
    // wake but before the next receiver exists, stranding a now-clean pane.
    let mut ev_rx = inner.events.subscribe();
    // When the idle-ambiguous composer hold began. Cleared whenever any
    // other verdict interrupts, so only unbroken ambiguity can outlive the
    // settle window and become the durable block.
    let mut ambiguous_since: Option<Instant> = None;
    let ambiguous_settle = Duration::from_millis(inner.cfg.ambiguous_composer_settle_ms);
    'gate: loop {
        // The event receiver predates every evaluation, so events published
        // mid-evaluation or between iterations remain buffered. Evaluation
        // itself is still authoritative.
        let watcher = watcher_for_handle(inner, handle);
        let mut pane_rx = watcher.as_ref().map(|w| w.subscribe());

        if let Some(notification) = &handle.notification {
            match notification.ensure_current_gating() {
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
        }

        // `initial_hold` is only a receipt seed. For a notification that
        // starts while a human draft is visible, take the fresh gate path so
        // it can record the exact durable `composer_hold` refusal below;
        // carrying the cached `idle_with_input` value straight into the wait
        // loop would make that refusal invisible until another pane event.
        let initial_hold = forced_hold.take().filter(|cause| {
            // A cached `Working` verdict is only a receipt hint. Workspace
            // notifications must take the fresh capture path so a clean
            // composer can admit a doorbell during the turn. Likewise a
            // cached draft must be re-read so the durable composer hold is
            // recorded immediately. Direct deliveries retain their legacy
            // cached-hold behaviour.
            !(handle.notification.is_some()
                && matches!(cause.as_str(), "idle_with_input" | "working"))
        });
        let hold = if let Some(cause) = initial_hold {
            Some(cause)
        } else {
            match &watcher {
                None => Some("session_detached".to_string()),
                Some(w) => 'pane: {
                    let Some(row) = w.pane(&handle.pane_id) else {
                        if let Some(hold) = workspace_prewrite_hold(handle, "no_such_pane") {
                            break 'pane Some(hold);
                        }
                        return GateOutcome::Attention {
                            cause: "no_such_pane".to_string(),
                        };
                    };
                    if row.dead {
                        if let Some(hold) = workspace_prewrite_hold(handle, "pane_dead") {
                            break 'pane Some(hold);
                        }
                        return GateOutcome::Attention {
                            cause: "pane_dead".to_string(),
                        };
                    }
                    if row.in_mode {
                        // Human scrolling in copy-mode; %pane-mode-changed
                        // re-triggers via the pane event stream.
                        Some("pane_in_mode".to_string())
                    } else {
                        let Some(manifest) =
                            fusion::bind_manifest_for(inner, handle.session_idx, &row)
                        else {
                            if let Some(hold) = workspace_prewrite_hold(handle, "no_manifest") {
                                break 'pane Some(hold);
                            }
                            return GateOutcome::Attention {
                                cause: "no_manifest".to_string(),
                            };
                        };
                        let manifest_id = manifest.agent.id.clone();
                        let Some(det) = crate::observe_pane(
                            inner,
                            handle.session_idx,
                            w,
                            &handle.pane_id,
                            true,
                            "gate",
                        )
                        .await
                        else {
                            if let Some(hold) = workspace_prewrite_hold(handle, "no_such_pane") {
                                break 'pane Some(hold);
                            }
                            return GateOutcome::Attention {
                                cause: "no_such_pane".to_string(),
                            };
                        };

                        if handle.notification.is_some() {
                            if let Some(pane_pid) = notification_pane_for_unproven_composer(
                                inner,
                                handle,
                                &row,
                                &manifest_id,
                                &det,
                            ) {
                                gate_line(inner, handle, "proceed", Some(&det.decided_by), None);
                                return GateOutcome::Proceed {
                                    manifest_id,
                                    pane_pid,
                                    regate_evidence_changed,
                                };
                            }
                        }
                        if handle.notification.is_some()
                            && det.write_block.as_deref() == Some(HOOK_ADMISSION_UNPROVEN)
                        {
                            let Some(mut observation) =
                                composer_semantic_observation(inner, handle, &row, &manifest_id)
                            else {
                                return GateOutcome::BlockedPreWrite {
                                    cause: NotificationPreWriteCause::BindingUnprovable,
                                    observation: Box::new(binding_unprovable_observation(
                                        inner,
                                        handle,
                                        row.pane_pid,
                                        &manifest_id,
                                    )),
                                };
                            };
                            observation.write_block = Some(HOOK_ADMISSION_UNPROVEN.to_string());
                            return GateOutcome::BlockedPreWrite {
                                cause: NotificationPreWriteCause::WriteReadinessChanged,
                                observation: Box::new(observation),
                            };
                        }
                        match det.state {
                            AgentState::Idle => {
                                // Runtime idle is not permission to write. A
                                // turn-end hook can put the pane in idle while
                                // the composer holds a staged payload the screen
                                // sensor could not read. Proceeding pastes over it.
                                match (det.write_ready, det.write_block.as_deref()) {
                                    (true, _) => {
                                        // The admitted pid is what every
                                        // receipt is later held against, so an
                                        // unreadable process table is a
                                        // refusal, not a shrug. Falling back
                                        // to the pane root here would pin the
                                        // delivery to the SHELL and then
                                        // resolve receipts against whoever
                                        // sits at that prompt next.
                                        //
                                        // A HOLD rather than an ending: not
                                        // being able to name the occupant is
                                        // doubt, and nothing has been written
                                        // yet. A respawned pane updates its
                                        // pid in the table without emitting a
                                        // pane change, so the row can briefly
                                        // name a process that has already
                                        // exited, and ending the delivery
                                        // there would summon a human for a
                                        // table that was about to catch up.
                                        let admitted = fusion::admitted_binding(
                                            inner,
                                            handle.session_idx,
                                            &row,
                                        )
                                        .filter(|b| b.manifest == manifest_id);
                                        match admitted {
                                            None if handle.notification.is_some()
                                                && last_hold.as_deref()
                                                    == Some(OBSERVATION_HOLD) =>
                                            {
                                                return GateOutcome::BlockedPreWrite {
                                                    cause:
                                                        NotificationPreWriteCause::BindingUnprovable,
                                                    observation: Box::new(
                                                        binding_unprovable_observation(
                                                            inner,
                                                            handle,
                                                            row.pane_pid,
                                                            &manifest_id,
                                                        ),
                                                    ),
                                                };
                                            }
                                            None => Some(OBSERVATION_HOLD.to_string()),
                                            Some(_) => {
                                                match fusion::foreground_pid_checked(row.pane_pid) {
                                                    None => Some(OBSERVATION_HOLD.to_string()),
                                                    Some(pane_pid) => {
                                                        gate_line(
                                                            inner,
                                                            handle,
                                                            "proceed",
                                                            Some(&det.decided_by),
                                                            None,
                                                        );
                                                        return GateOutcome::Proceed {
                                                            manifest_id,
                                                            pane_pid,
                                                            regate_evidence_changed,
                                                        };
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    // Hold on an event, never a clock: the next
                                    // pane change re-evaluates, and a screen
                                    // sensor that can see the composer resolves
                                    // it without anyone pasting blind.
                                    // Fusion may have recorded the same failed
                                    // lookup in write readiness. Settle that path
                                    // after the same bounded second observation.
                                    (false, Some(OBSERVATION_HOLD))
                                        if handle.notification.is_some()
                                            && last_hold.as_deref()
                                                == Some(WRITE_READINESS_OBSERVATION_HOLD) =>
                                    {
                                        return GateOutcome::BlockedPreWrite {
                                            cause: NotificationPreWriteCause::BindingUnprovable,
                                            observation: Box::new(binding_unprovable_observation(
                                                inner,
                                                handle,
                                                row.pane_pid,
                                                &manifest_id,
                                            )),
                                        };
                                    }
                                    // A composer that reads `ambiguous` on an
                                    // idle pane may be one frame from proof (a
                                    // redraw caught mid-paint) or may never be
                                    // provable at all (a manifest whose rules
                                    // cannot classify this vendor's clean
                                    // composer). No single frame separates the
                                    // two, so the first reading holds — but
                                    // only for the settle window. Ambiguity
                                    // that outlives it is a manifest gap
                                    // wearing a transient's clothes, and no
                                    // pane event announces "still ambiguous",
                                    // so the wake settles as a durable,
                                    // operator-visible block instead of
                                    // waiting in memory forever. Working
                                    // frames never reach this arm (the
                                    // Working arm above owns them), so
                                    // mid-turn ambiguity — deliberate where a
                                    // vendor's mid-turn injection is
                                    // unmeasured — cannot escalate.
                                    (false, _)
                                        if handle.notification.is_some()
                                            && unproven_composer_is_eligible(&det) =>
                                    {
                                        if fusion::composer_has_unsubmitted_draft(
                                            inner,
                                            handle.session_idx,
                                            &handle.pane_id,
                                        ) {
                                            Some("composer_hold".to_string())
                                        } else {
                                            match fusion::foreground_pid_checked(row.pane_pid) {
                                                None => Some(OBSERVATION_HOLD.to_string()),
                                                Some(pane_pid) => {
                                                    gate_line(
                                                        inner,
                                                        handle,
                                                        "proceed",
                                                        Some(&det.decided_by),
                                                        None,
                                                    );
                                                    return GateOutcome::Proceed {
                                                        manifest_id,
                                                        pane_pid,
                                                        regate_evidence_changed,
                                                    };
                                                }
                                            }
                                        }
                                    }
                                    // A staged human draft is an exact boundary,
                                    // not a terminal delivery outcome. Keep the
                                    // notification in Gating until a pane edge
                                    // proves the draft was submitted or erased.
                                    (false, Some("composer_hold"))
                                        if handle.notification.is_some() =>
                                    {
                                        Some("composer_hold".to_string())
                                    }
                                    (false, reason) => Some(format!(
                                        "not_write_ready:{}",
                                        reason.unwrap_or("unstamped")
                                    )),
                                }
                            }
                            AgentState::Dead => {
                                if let Some(hold) = workspace_prewrite_hold(handle, "pane_dead") {
                                    break 'pane Some(hold);
                                }
                                return GateOutcome::Attention {
                                    cause: "pane_dead".to_string(),
                                };
                            }
                            AgentState::BlockedQuota => {
                                let hint = quota_hint(w, &handle.pane_id).await;
                                gate_line(
                                    inner,
                                    handle,
                                    "park",
                                    Some(&det.decided_by),
                                    Some("blocked_quota"),
                                );
                                return GateOutcome::Park { hint };
                            }
                            AgentState::BlockedModal | AgentState::BlockedPermission => {
                                let rule = inner.manifests.get(&manifest_id).and_then(|m| {
                                    m.rules
                                        .iter()
                                        .find(|r| r.id == det.decided_by && r.state.is_blocked())
                                });
                                match rule {
                                    Some(r)
                                        if r.auto_dismiss
                                            && !r.decline_keys.is_empty()
                                            && *declines.get(&r.id).unwrap_or(&0)
                                                < MAX_DECLINES =>
                                    {
                                        *declines.entry(r.id.clone()).or_insert(0) += 1;
                                        gate_line(inner, handle, "decline", Some(&r.id), None);
                                        let keys = r.decline_keys.clone();
                                        let rule_id = r.id.clone();
                                        if !send_decline_keys(
                                            w,
                                            &handle.pane_id,
                                            manifest,
                                            &rule_id,
                                            &keys,
                                        )
                                        .await
                                        {
                                            // The screen changed under the
                                            // decline (TOCTOU): the confirming
                                            // key was withheld. Back to the
                                            // gate loop to re-read reality.
                                            gate_line(
                                                inner,
                                                handle,
                                                "decline_aborted",
                                                Some(&rule_id),
                                                Some("modal_changed"),
                                            );
                                        }
                                        // One-shot settle so the dismissal
                                        // renders before the re-check; the
                                        // decline count bounds this loop.
                                        tokio::time::sleep(DECLINE_SPACING).await;
                                        continue 'gate;
                                    }
                                    _ => {
                                        // Trust/permission prompts belong to the
                                        // human: hold and alert, never dismiss.
                                        let rule_id = rule
                                            .map(|r| r.id.clone())
                                            .unwrap_or_else(|| det.decided_by.clone());
                                        if notified_rules.insert(rule_id.clone()) {
                                            admin_notify(
                                            inner,
                                            NotifyLevel::ActionRequired,
                                            &format!("{} blocked: {rule_id}", handle.to),
                                            &format!(
                                                "delivery {} is held; rule {rule_id} needs a decision",
                                                handle.msg_id
                                            ),
                                            Some(&handle.msg_id),
                                            Some(handle.session_idx),
                                            // The pane, not the delivery:
                                            // the delivery is only gating,
                                            // and the thing a human clears
                                            // is the prompt on the pane.
                                            About::pane(&handle.pane_id),
                                        );
                                        }
                                        Some(format!("blocked:{rule_id}"))
                                    }
                                }
                            }
                            AgentState::Working => {
                                // Runtime state is not permission to write,
                                // but it is not an automatic refusal either.
                                // Under the direct pane interruption contract:
                                // For notification doorbells, working state is an observation,
                                // not a delivery blocker. Only a proven non-Cyclops draft holds it.
                                if handle.notification.is_some() {
                                    if fusion::composer_has_unsubmitted_draft(
                                        inner,
                                        handle.session_idx,
                                        &handle.pane_id,
                                    ) {
                                        Some("composer_hold".to_string())
                                    } else {
                                        match fusion::foreground_pid_checked(row.pane_pid) {
                                            None if last_hold.as_deref()
                                                == Some(OBSERVATION_HOLD) =>
                                            {
                                                return GateOutcome::BlockedPreWrite {
                                                    cause:
                                                        NotificationPreWriteCause::BindingUnprovable,
                                                    observation: Box::new(
                                                        binding_unprovable_observation(
                                                            inner,
                                                            handle,
                                                            row.pane_pid,
                                                            &manifest_id,
                                                        ),
                                                    ),
                                                };
                                            }
                                            None => Some(OBSERVATION_HOLD.to_string()),
                                            Some(pane_pid) => {
                                                gate_line(
                                                    inner,
                                                    handle,
                                                    "proceed",
                                                    Some(&det.decided_by),
                                                    None,
                                                );
                                                return GateOutcome::Proceed {
                                                    manifest_id,
                                                    pane_pid,
                                                    regate_evidence_changed,
                                                };
                                            }
                                        }
                                    }
                                } else if !det.write_ready {
                                    Some(
                                        det.write_block
                                            .clone()
                                            .unwrap_or_else(|| "working".to_string()),
                                    )
                                } else {
                                    match fusion::foreground_pid_checked(row.pane_pid) {
                                        None => Some(OBSERVATION_HOLD.to_string()),
                                        Some(pane_pid) => {
                                            gate_line(
                                                inner,
                                                handle,
                                                "proceed",
                                                Some(&det.decided_by),
                                                None,
                                            );
                                            return GateOutcome::Proceed {
                                                manifest_id,
                                                pane_pid,
                                                regate_evidence_changed,
                                            };
                                        }
                                    }
                                }
                            }
                            // Human typing always wins. A notification has
                            // reached a conclusive pre-write refusal: publish
                            // it durably now, rather than waiting in memory
                            // for a turn that may never occur (for example a
                            // local slash command).
                            AgentState::IdleWithInput if handle.notification.is_some() => {
                                let Some(mut observation) = composer_semantic_observation(
                                    inner,
                                    handle,
                                    &row,
                                    &manifest_id,
                                ) else {
                                    return GateOutcome::BlockedPreWrite {
                                        cause: NotificationPreWriteCause::BindingUnprovable,
                                        observation: Box::new(binding_unprovable_observation(
                                            inner,
                                            handle,
                                            row.pane_pid,
                                            &manifest_id,
                                        )),
                                    };
                                };
                                observation.write_block = Some("composer_hold".to_string());
                                return GateOutcome::BlockedPreWrite {
                                    cause: NotificationPreWriteCause::WriteReadinessChanged,
                                    observation: Box::new(observation),
                                };
                            }
                            AgentState::IdleWithInput => Some("idle_with_input".to_string()),
                            AgentState::Unknown => Some("unknown".to_string()),
                        }
                    }
                }
            }
        };
        // Only unbroken ambiguity may settle: any other verdict in between
        // restarts the window from zero.
        if hold.as_deref() != Some(AMBIGUOUS_COMPOSER_HOLD) {
            ambiguous_since = None;
        }
        if let Some(cause) = hold {
            handle.set_hold(Some(normalize_hold_cause(&cause)));
            if last_hold.as_deref() != Some(cause.as_str()) {
                gate_line(
                    inner,
                    handle,
                    gate_hold_action(handle, &cause),
                    None,
                    Some(&cause),
                );
                last_hold = Some(cause.clone());
            }
            let since = *hold_since.get_or_insert_with(Instant::now);
            let notify_at = since + Duration::from_millis(inner.cfg.gate_hold_notify_ms);
            // A hold caused by a failed OBSERVATION has no edge coming to
            // release it. Every other cause is a fact about the pane, and
            // the pane announces when that changes; "we could not read the
            // process table" announces nothing, and a transient failure
            // would otherwise wedge the delivery for good. So that one
            // cause, and only that one, also wakes on a bounded retry.
            // The re-evaluation is the full gate: a fresh binding and
            // fresh clean-composer proof, never a shortcut back to
            // proceed.
            // The same doubt reaches the gate two ways: this gate's own
            // foreground check, and a stamped verdict that already
            // refused for it. Both are an observation that did not
            // answer, and neither produces a pane event to wake on.
            let unprovable =
                cause == OBSERVATION_HOLD || cause == format!("not_write_ready:{OBSERVATION_HOLD}");
            // The ambiguous-composer hold gets the same treatment for the
            // same reason: unchanged ambiguity emits no pane event, so the
            // settle boundary needs its own wake to become the durable
            // block rather than an indefinite in-memory wait.
            let retry_at = if unprovable {
                Some(Instant::now() + OBSERVATION_RETRY)
            } else if cause == AMBIGUOUS_COMPOSER_HOLD {
                ambiguous_since.map(|since| since + ambiguous_settle)
            } else if cause == "barrier_held" {
                Some(Instant::now() + Duration::from_millis(50))
            } else {
                None
            };
            let exact_evidence = tokio::select! {
                changed = wait_pane_change(
                    &mut ev_rx,
                    pane_rx.as_mut(),
                    handle.session_idx,
                    &handle.pane_id,
                    &handle.cancel,
                ) => changed,
                _ = async {
                    match retry_at {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => false,
                _ = tokio::time::sleep_until(notify_at), if !hold_notified => {
                    // A wedged hold must at least be visible. One ping per
                    // delivery; the hold itself keeps waiting on events.
                    hold_notified = true;
                    let (kind, about) = if handle.notification.is_some() {
                        ("notification", About::pane(&handle.pane_id))
                    } else {
                        ("delivery", About::delivery(&handle.to))
                    };
                    admin_notify(
                        inner,
                        NotifyLevel::ActionRequired,
                        &format!("{kind} to {} held in gating", handle.to),
                        &format!(
                            "message {} has been held for over {}ms ({cause})",
                            handle.msg_id, inner.cfg.gate_hold_notify_ms
                        ),
                        Some(&handle.msg_id),
                        Some(handle.session_idx),
                        about,
                    );
                    false
                }
            };
            regate_evidence_changed |= exact_evidence;
        }
    }
}

/// Keep receipt vocabulary stable and independent of vendor manifest rule
/// ids. Ledger gate lines retain the exact cause for diagnostics; receipts
/// expose only these normalized tokens.
pub(crate) fn normalize_hold_cause(cause: &str) -> &'static str {
    match cause {
        "session_detached" => "session_detached",
        "pane_in_mode" => "pane_in_mode",
        "working" => "working",
        "idle_with_input" => "idle_with_input",
        "held_for_existing_draft" => "held_for_existing_draft",
        "blocked_quota" => "blocked_quota",
        "unknown" => "unknown",
        c if c.split(':').next() == Some("blocked") => "blocked",
        // Runtime state is idle, but nothing proved the composer
        // was clean. Receipts say so plainly; the exact reason stays on
        // the gate ledger line.
        c if c.split(':').next() == Some("not_write_ready") => "not_write_ready",
        _ => "unknown",
    }
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

/// Parse the quota reset hint from the screen. Only the parsed phrase ever
/// leaves this function; raw captures stay out of the ledger.
pub(crate) async fn quota_hint(watcher: &Arc<SessionWatcher>, pane_id: &str) -> Option<String> {
    let screen = watcher.client().capture_pane(pane_id).await.ok()?;
    parse_reset_hint(&screen)
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
    if let Some(notification) = &handle.notification {
        notification.record_paste_command_unwritten()?;
    }
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

/// Record a receipt before the legacy delivery state claims it.
///
/// False means the notification already resolved the other way in a race.
pub(crate) fn record_notification_notified(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
) -> Result<bool, NotificationAdapterError> {
    let Some(notification) = &handle.notification else {
        return Ok(true);
    };
    match notification.record_notified() {
        Ok(record) => {
            if handle.notification_transport() == Some(NotificationTransport::DirectPayload) {
                notification.record_delivered_direct()?;
                let recipient = notification.recipient();
                if let Some(messaging) = inner.workspace_messaging() {
                    if let Err(error) = messaging.direct_delivery_settled(recipient) {
                        error!(
                            id = %handle.msg_id,
                            %recipient,
                            %error,
                            "direct delivery settled but the next mailbox item could not be scheduled"
                        );
                    }
                } else {
                    error!(
                        id = %handle.msg_id,
                        %recipient,
                        "direct delivery settled without workspace messaging"
                    );
                }
            } else {
                if let Some(messaging) = inner.workspace_messaging() {
                    messaging.notification_became_notified(record);
                }
            }
            Ok(true)
        }
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
/// `agent.wait` follows the fused contract. A composed wait additionally
/// retains the exact delivery identity it inherited across receipt settlement.
pub(crate) fn wait_working_event_is_eligible(
    event: &Event,
    submitted_turn: Option<&Arc<DeliveryHandle>>,
) -> bool {
    match submitted_turn {
        Some(handle) => submitted_working_state_event(event, handle),
        None => confirmed_working_state_event(event),
    }
}

/// Compatibility wrapper for receipt-path callers and focused tests.
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
    match target {
        StagingTarget::Sentinel(msg_id) => {
            sentinel_verified(manifest, screen, msg_id) || marker_in_composer(manifest, screen)
        }
        StagingTarget::ExactRow(expected_row) => {
            staged_representation(manifest, screen, StagingTarget::ExactRow(expected_row)).is_some()
        }
    }
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
        DeliveryState::Submitted => match record_notification_notified(inner, handle) {
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
        },
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
            let recorded = match record_notification_notified(inner, &handle) {
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
                let recorded = match record_notification_notified(inner, &handle) {
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

/// How a wait ended. Serialized into send-and-wait entries and agent.wait
/// error data; NotDelivered only occurs in send-and-wait composition.
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
    /// Legacy direct send-and-wait only: the delivery resolved somewhere
    /// other than delivered, so there is no post-delivery pane transition
    /// to observe.
    NotDelivered,
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
