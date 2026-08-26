//! Reconcile boot-recovered composer barriers with current pane evidence.

use std::collections::HashSet;

use cyclops_proto::{
    AgentState, ComposerHold, Detection, NotificationAttemptId, NotificationBarrierRetirementCause,
    NotificationBinding, NotificationManifestId, NotificationRecord, ProcessInstanceId,
    RecipientKey, TmuxPaneId,
};
use cyclops_tmux::{PaneRow, SessionWatcher};

use crate::{fusion, mailbox, Inner};

/// Exact durable route described by this watcher observation.
pub(crate) fn exact_recipient(
    inner: &Inner,
    session_idx: usize,
    watcher: &SessionWatcher,
    row: &PaneRow,
) -> Option<RecipientKey> {
    let slot = inner.session(session_idx)?;
    let session_instance_id = {
        let link = slot.link.lock().expect("session link lock");
        let current = link.watcher.as_ref()?;
        if !link.attached
            || current.session_id() != watcher.session_id()
            || current.session() != watcher.session()
        {
            return None;
        }
        link.identity.as_ref()?.session_instance_id()
    };
    let pane = row.pane_id.parse::<TmuxPaneId>().ok()?;
    let root = crate::identity::ProcId::of(row.pane_pid)?;
    let pane_root = ProcessInstanceId::new(root.pid, root.birth).ok()?;
    let recipient = RecipientKey::agent(inner.workspace_id, session_instance_id, pane);
    inner
        .adoption_for_observed_route(recipient, &row.pane_id, pane_root)
        .map(|_| recipient)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecoveryAction {
    Hold(&'static str),
    Restore(NotificationAttemptId),
    Retire {
        record: Box<NotificationRecord>,
        cause: NotificationBarrierRetirementCause,
        replacement: Option<NotificationBinding>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleRetirement {
    NotReady,
    Durable(NotificationAttemptId),
    Blocked(&'static str),
}

fn same_physical_pane(left: RecipientKey, right: RecipientKey) -> bool {
    left.workspace_id() == right.workspace_id()
        && left.pane_id().is_some()
        && left.pane_id() == right.pane_id()
}

fn same_composer_occupant(left: &NotificationBinding, right: &NotificationBinding) -> bool {
    left.pane_root.is_some()
        && left.pane_root == right.pane_root
        && left.agent == right.agent
        && left.manifest == right.manifest
}

#[derive(Debug, Default)]
pub(crate) struct RecoveryCoordinator {
    tracked_attempts: HashSet<NotificationAttemptId>,
    retiring: HashSet<NotificationAttemptId>,
    writer_unknown: bool,
}

impl RecoveryCoordinator {
    pub(crate) fn new(attempts: impl IntoIterator<Item = NotificationAttemptId>) -> Self {
        Self {
            tracked_attempts: attempts.into_iter().collect(),
            retiring: HashSet::new(),
            writer_unknown: false,
        }
    }

    /// Track a same-run barrier whose delivery worker already retired.
    ///
    /// An exact claim can outlive its in-memory delivery handle. The durable
    /// barrier then follows the same bound clean-screen retirement path as a
    /// barrier restored at daemon boot.
    pub(crate) fn track(&mut self, attempt_id: NotificationAttemptId) {
        self.tracked_attempts.insert(attempt_id);
    }

    fn writer_unknown(&self) -> bool {
        self.writer_unknown
    }

    pub(crate) fn active_for_recipient(
        &mut self,
        canonical: &[NotificationRecord],
        recipient: RecipientKey,
    ) -> Vec<NotificationRecord> {
        let active: HashSet<_> = canonical.iter().map(|record| record.attempt_id).collect();
        self.tracked_attempts
            .retain(|attempt_id| active.contains(attempt_id));
        self.retiring
            .retain(|attempt_id| active.contains(attempt_id));
        canonical
            .iter()
            .filter(|record| {
                self.tracked_attempts.contains(&record.attempt_id)
                    && same_physical_pane(record.recipient, recipient)
            })
            .cloned()
            .collect()
    }

    fn contains(&self, attempt_id: NotificationAttemptId) -> bool {
        self.tracked_attempts.contains(&attempt_id)
    }

    fn reserve_record(
        &mut self,
        canonical: &[NotificationRecord],
        attempt_id: NotificationAttemptId,
    ) -> Result<Option<NotificationRecord>, &'static str> {
        let active: HashSet<_> = canonical.iter().map(|record| record.attempt_id).collect();
        self.tracked_attempts
            .retain(|candidate| active.contains(candidate));
        self.retiring.retain(|candidate| active.contains(candidate));
        if !self.tracked_attempts.contains(&attempt_id) {
            return Ok(None);
        }
        if self.writer_unknown {
            return Err("composer_recovery_reopen_required");
        }
        if !self.retiring.insert(attempt_id) {
            return Err("composer_recovery_retirement_pending");
        }
        Ok(canonical
            .iter()
            .find(|record| record.attempt_id == attempt_id)
            .cloned())
    }

    pub(crate) fn reconcile(
        &mut self,
        records: &[NotificationRecord],
        live: Option<&NotificationBinding>,
        clean_composer: bool,
        legacy_claimed_clean: bool,
    ) -> Option<RecoveryAction> {
        let [record] = records else {
            return if records.is_empty() {
                None
            } else {
                Some(RecoveryAction::Hold("composer_recovery_ambiguous"))
            };
        };
        if self.retiring.contains(&record.attempt_id) {
            return Some(RecoveryAction::Hold("composer_recovery_retirement_pending"));
        }
        let Some(expected) = record
            .binding
            .as_ref()
            .filter(|binding| binding.recipient == record.recipient)
        else {
            return Some(RecoveryAction::Hold("composer_recovery_unproven"));
        };
        let Some(live) = live.filter(|binding| {
            same_physical_pane(binding.recipient, record.recipient)
                && binding.pane_root.is_some()
                && binding.leader.is_some()
        }) else {
            return Some(RecoveryAction::Hold("composer_recovery_unproven"));
        };

        if crate::mailbox::uses_incomplete_legacy_doorbell_contract(record) {
            if legacy_claimed_clean
                && live.recipient == record.recipient
                && expected.manifest == live.manifest
            {
                self.retiring.insert(record.attempt_id);
                return Some(RecoveryAction::Retire {
                    record: Box::new(record.clone()),
                    cause: NotificationBarrierRetirementCause::RecipientClaimedComposerClear,
                    replacement: None,
                });
            }
            return Some(RecoveryAction::Hold("legacy_durable_binding_incomplete"));
        }

        if expected.pane_root.is_none() {
            return Some(RecoveryAction::Hold("composer_recovery_unproven"));
        }

        if same_composer_occupant(expected, live) {
            if matches!(
                record.state,
                cyclops_proto::NotificationState::Notified
                    | cyclops_proto::NotificationState::WithdrawnAfterStaging
            ) && clean_composer
            {
                self.retiring.insert(record.attempt_id);
                return Some(RecoveryAction::Retire {
                    record: Box::new(record.clone()),
                    cause: NotificationBarrierRetirementCause::ComposerObservedClear,
                    replacement: None,
                });
            }
            return Some(RecoveryAction::Restore(record.attempt_id));
        }

        self.retiring.insert(record.attempt_id);
        let mut replacement = live.clone();
        // The retirement fact belongs to the record's durable route. A pane
        // may have moved to another session, but the process evidence still
        // retires the old route's occupant.
        replacement.recipient = record.recipient;
        Some(RecoveryAction::Retire {
            record: Box::new(record.clone()),
            cause: NotificationBarrierRetirementCause::OccupantReplaced,
            replacement: Some(replacement),
        })
    }

    pub(crate) fn retired(&mut self, attempt_id: NotificationAttemptId) {
        self.tracked_attempts.remove(&attempt_id);
        self.retiring.remove(&attempt_id);
    }

    pub(crate) fn retirement_failed(&mut self, attempt_id: NotificationAttemptId) {
        self.retiring.remove(&attempt_id);
    }

    fn require_reopen(&mut self) {
        self.writer_unknown = true;
        self.retiring.clear();
    }

    /// Revalidate a concurrent retirement while the caller owns its runtime lock.
    pub(crate) fn retirement_pending_reason(
        &self,
        attempt_id: NotificationAttemptId,
    ) -> Option<&'static str> {
        if self.writer_unknown {
            Some("composer_recovery_reopen_required")
        } else if self.retiring.contains(&attempt_id) {
            Some("composer_recovery_retirement_pending")
        } else if self.tracked_attempts.contains(&attempt_id) {
            Some("composer_recovery_retirement_failed")
        } else {
            None
        }
    }
}

/// Merge one canonical recovery decision into the runtime composer barrier.
///
/// A newly restored attempt starts without a lifecycle key because no
/// pre-restart key is trustworthy. Once an exact post-restart start binds the
/// recovered owner, repeated reconciliations preserve that key until its exact
/// end is durably reconciled.
pub(crate) fn merge_barrier(
    action: Option<&RecoveryAction>,
    retired_attempt: Option<NotificationAttemptId>,
    mut hold: ComposerHold,
    mut owner: Option<String>,
    turn_already_running: bool,
) -> (ComposerHold, Option<String>, bool, Option<&'static str>) {
    let mut clear_turn = false;
    if let Some(attempt_id) = retired_attempt {
        let retired_owner = attempt_id.to_string();
        if owner.as_deref() == Some(retired_owner.as_str()) {
            hold = ComposerHold::Clear;
            owner = None;
            clear_turn = true;
        }
    }

    let refusal = match action {
        None => None,
        Some(RecoveryAction::Hold(reason)) => Some(*reason),
        Some(RecoveryAction::Restore(attempt_id)) => {
            let recovered_owner = attempt_id.to_string();
            match owner.as_deref() {
                Some(current) if current == recovered_owner => None,
                None if hold == ComposerHold::Clear => {
                    hold = if turn_already_running {
                        ComposerHold::StagedDuringTurn
                    } else {
                        ComposerHold::Staged
                    };
                    owner = Some(recovered_owner);
                    clear_turn = true;
                    None
                }
                _ => Some("composer_recovery_runtime_conflict"),
            }
        }
        Some(RecoveryAction::Retire { .. }) => Some("composer_recovery_retirement_pending"),
    };
    (hold, owner, clear_turn, refusal)
}

/// Convert one authenticated process observation into the durable shape.
pub(crate) fn observed_binding(
    recipient: RecipientKey,
    binding: &fusion::Binding,
) -> Option<NotificationBinding> {
    Some(NotificationBinding {
        recipient,
        pane_root: Some(
            ProcessInstanceId::new(binding.pane_root.pid, binding.pane_root.birth).ok()?,
        ),
        leader: Some(ProcessInstanceId::new(binding.leader.pid, binding.leader.birth).ok()?),
        agent: ProcessInstanceId::new(binding.agent.pid, binding.agent.birth).ok()?,
        manifest: NotificationManifestId::new(binding.manifest.clone()).ok()?,
    })
}

/// Current positive evidence that a terminal write would not overwrite text.
pub(crate) fn clean_composer(det: &Detection, in_mode: bool) -> bool {
    !in_mode
        && !det.stale
        && !det.disagreement
        && det.state == AgentState::Idle
        && det.screen_proves_write_safe_composer()
        && det
            .readings
            .iter()
            .all(|reading| reading.state == AgentState::Idle)
}

/// Bind clean-screen evidence to the manifest that authenticated the occupant.
pub(crate) fn clean_composer_for_binding(
    det: &Detection,
    in_mode: bool,
    detection_manifest: Option<&str>,
    binding: &NotificationBinding,
) -> bool {
    detection_manifest == Some(binding.manifest.as_str()) && clean_composer(det, in_mode)
}

/// Bind a turn start observed after a recovered barrier was restored.
///
/// `StagedDuringTurn` is excluded because the turn already running when the
/// barrier was restored cannot prove it consumed the staged payload. Once it
/// ends, the hold returns to `Staged` and the next exact start may bind.
pub(crate) fn bind_post_recovery_turn(
    inner: &std::sync::Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    turn: crate::turnkey::TurnKey,
    since_ms: u64,
) -> bool {
    let attempt_id = {
        let detections = inner.detections.lock().expect("detections lock");
        let Some(entry) = detections.get(&crate::PaneKey::new(session_idx, pane_id)) else {
            return false;
        };
        if !matches!(
            entry.hold,
            ComposerHold::Staged | ComposerHold::TurnStarted { .. }
        ) {
            return false;
        }
        let Some(owner) = entry.hold_owner.as_deref() else {
            return false;
        };
        let Ok(attempt_id) = NotificationAttemptId::parse(owner) else {
            return false;
        };
        attempt_id
    };
    if !inner
        .composer_recovery
        .lock()
        .expect("composer recovery lock")
        .contains(attempt_id)
    {
        return false;
    }
    fusion::bind_turn(
        inner,
        session_idx,
        pane_id,
        &attempt_id.to_string(),
        turn,
        since_ms,
    )
    .is_some()
}

/// Persist exact post-restart lifecycle evidence before fusion consumes it.
///
/// The runtime candidate is copied under the established detection then
/// turn-end lock order. Journal IO starts only after both locks are released.
pub(crate) fn retire_exact_lifecycle(
    inner: &Inner,
    session_idx: usize,
    pane_id: &str,
    live: Option<&NotificationBinding>,
    clean_composer: bool,
) -> LifecycleRetirement {
    if !clean_composer {
        return LifecycleRetirement::NotReady;
    }
    let Some(live) = live else {
        return LifecycleRetirement::NotReady;
    };
    let candidate = {
        let pane = crate::PaneKey::new(session_idx, pane_id);
        let detections = inner.detections.lock().expect("detections lock");
        let Some(entry) = detections.get(&pane) else {
            return LifecycleRetirement::NotReady;
        };
        let live_agent = crate::identity::ProcId {
            pid: live.agent.pid(),
            birth: live.agent.birth(),
        };
        if entry.agent != Some(live_agent)
            || entry.manifest.as_deref() != Some(live.manifest.as_str())
            || !matches!(entry.hold, ComposerHold::TurnStarted { .. })
        {
            return LifecycleRetirement::NotReady;
        }
        let (Some(owner), Some(turn)) = (entry.hold_owner.as_deref(), entry.turn.as_ref()) else {
            return LifecycleRetirement::NotReady;
        };
        let Ok(attempt_id) = NotificationAttemptId::parse(owner) else {
            return LifecycleRetirement::NotReady;
        };
        if !crate::turnkey::PaneEnds::holds(
            &inner.turn_ends.lock().expect("turn ends lock"),
            &pane,
            live_agent,
            live.manifest.as_str(),
            turn,
        ) {
            return LifecycleRetirement::NotReady;
        }
        attempt_id
    };

    let Some(service) = inner.mailbox.as_ref() else {
        return LifecycleRetirement::Blocked("composer_recovery_store_unavailable");
    };
    let canonical = match service.active_notification_barriers() {
        Ok(records) => records,
        Err(_) => {
            return LifecycleRetirement::Blocked("composer_recovery_store_unavailable");
        }
    };
    if canonical.iter().any(|record| {
        record.attempt_id == candidate && record.needs_claimed_ack_timeout_reconciliation()
    }) {
        // Exact attempt ACK-timeout recovery owns its own clear-or-clean settlement
        // fact. A turn end cannot remove that barrier or hide its alarm.
        return LifecycleRetirement::Blocked("claimed_notification_reconciliation_pending");
    }
    let record = match inner
        .composer_recovery
        .lock()
        .expect("composer recovery lock")
        .reserve_record(&canonical, candidate)
    {
        Ok(Some(record)) => record,
        // Another durable cause already removed this barrier. Runtime
        // settlement is now safe without appending a duplicate retirement.
        Ok(None) => return LifecycleRetirement::Durable(candidate),
        Err(reason) => return LifecycleRetirement::Blocked(reason),
    };
    let action = RecoveryAction::Retire {
        record: Box::new(record),
        cause: NotificationBarrierRetirementCause::LifecycleReconciled,
        replacement: None,
    };
    match persist(inner, &action) {
        Ok(attempt_id) => LifecycleRetirement::Durable(attempt_id),
        Err(reason) => LifecycleRetirement::Blocked(reason),
    }
}

/// Read active boot barriers for this physical pane from the canonical projection.
///
/// The exact durable recipient still scopes compaction. Recovery additionally
/// follows the globally unique tmux pane id across a session move so the same
/// physical composer cannot shed its barrier by changing routes.
pub(crate) fn active_for_recipient(
    inner: &Inner,
    recipient: RecipientKey,
) -> Result<Vec<NotificationRecord>, &'static str> {
    let service = inner
        .mailbox
        .as_ref()
        .ok_or("composer_recovery_store_unavailable")?;
    let canonical = service
        .active_notification_barriers()
        .map_err(|_| "composer_recovery_store_unavailable")?;
    let mut recovery = inner
        .composer_recovery
        .lock()
        .expect("composer recovery lock");
    let records = recovery.active_for_recipient(&canonical, recipient);
    if recovery.writer_unknown() && !records.is_empty() {
        Err("composer_recovery_reopen_required")
    } else {
        Ok(records)
    }
}

/// Persist a decision after dropping the recovery coordinator lock.
pub(crate) fn persist(
    inner: &Inner,
    action: &RecoveryAction,
) -> Result<NotificationAttemptId, &'static str> {
    let RecoveryAction::Retire {
        record,
        cause,
        replacement,
    } = action
    else {
        return Err("composer_recovery_not_a_retirement");
    };
    let attempt_id = record.attempt_id;
    let Some(service) = inner.mailbox.as_ref() else {
        inner
            .composer_recovery
            .lock()
            .expect("composer recovery lock")
            .retirement_failed(attempt_id);
        return Err("composer_recovery_store_unavailable");
    };
    let result = service.retire_notification_barrier(record, *cause, replacement.clone());
    let writer_unknown = result.as_ref().is_err_and(writer_requires_reopen);
    let mut recovery = inner
        .composer_recovery
        .lock()
        .expect("composer recovery lock");
    if result.is_ok() {
        recovery.retired(attempt_id);
    } else {
        recovery.retirement_failed(attempt_id);
        if writer_unknown {
            recovery.require_reopen();
        }
    }
    result
        .map(|()| attempt_id)
        .map_err(|_| "composer_recovery_retirement_failed")
}

/// Retire every active barrier for one route before the route is forgotten.
pub(crate) fn retire_gone_recipient(
    inner: &Inner,
    recipient: RecipientKey,
) -> Result<(), &'static str> {
    let service = inner
        .mailbox
        .as_ref()
        .ok_or("composer_recovery_store_unavailable")?;
    let records: Vec<_> = service
        .active_notification_barriers()
        .map_err(|_| "composer_recovery_store_unavailable")?
        .into_iter()
        .filter(|record| record.recipient == recipient)
        .collect();
    if records.is_empty() {
        return Ok(());
    }
    if inner
        .composer_recovery
        .lock()
        .expect("composer recovery lock")
        .writer_unknown()
    {
        return Err("composer_recovery_reopen_required");
    }
    for record in records {
        if let Err(error) = service.retire_notification_barrier(
            &record,
            NotificationBarrierRetirementCause::PaneGone,
            None,
        ) {
            if writer_requires_reopen(&error) {
                inner
                    .composer_recovery
                    .lock()
                    .expect("composer recovery lock")
                    .require_reopen();
            }
            return Err("composer_recovery_retirement_failed");
        }
        inner
            .composer_recovery
            .lock()
            .expect("composer recovery lock")
            .retired(record.attempt_id);
    }
    Ok(())
}

/// Retire every barrier owned by a process generation that was replaced.
///
/// The caller first validates the registry's exact old root while holding the
/// route-publication transaction. A stale replacement edge therefore cannot
/// reach this operation or retire barriers owned by a newer occupant. The
/// retirement is persisted before the registry rebind because the physical
/// replacement remains true even if that later registry write fails. The
/// replacement binding is the positive process and manifest observation that
/// makes the durable cause truthful.
pub(crate) fn retire_replaced_recipient(
    inner: &Inner,
    recipient: RecipientKey,
    replacement: Option<NotificationBinding>,
) -> Result<(), &'static str> {
    let service = inner
        .mailbox
        .as_ref()
        .ok_or("composer_recovery_store_unavailable")?;
    let records: Vec<_> = service
        .active_notification_barriers()
        .map_err(|_| "composer_recovery_store_unavailable")?
        .into_iter()
        .filter(|record| record.recipient == recipient)
        .collect();
    if records.is_empty() {
        return Ok(());
    }
    let replacement = replacement.ok_or("composer_recovery_replacement_unproven")?;
    if inner
        .composer_recovery
        .lock()
        .expect("composer recovery lock")
        .writer_unknown()
    {
        return Err("composer_recovery_reopen_required");
    }
    for record in records {
        if let Err(error) = service.retire_notification_barrier(
            &record,
            NotificationBarrierRetirementCause::OccupantReplaced,
            Some(replacement.clone()),
        ) {
            if writer_requires_reopen(&error) {
                inner
                    .composer_recovery
                    .lock()
                    .expect("composer recovery lock")
                    .require_reopen();
            }
            return Err("composer_recovery_retirement_failed");
        }
        inner
            .composer_recovery
            .lock()
            .expect("composer recovery lock")
            .retired(record.attempt_id);
    }
    Ok(())
}

/// Prove physical pane loss against the whole tmux server.
///
/// Absence from one session is only a route change. The pane is gone only
/// when the server has no such pane id or that id now names a different root
/// process generation.
pub(crate) async fn physical_pane_gone(
    watcher: &SessionWatcher,
    adoption: &crate::registry::Adoption,
) -> Result<bool, &'static str> {
    let expected = adoption
        .pane_root
        .ok_or("composer_recovery_pane_root_unproven")?;
    physical_pane_gone_with_expected(watcher, &adoption.pane_id, Some(expected)).await
}

/// Prove physical loss for a route that may no longer have an adoption.
///
/// When the prior root is unknown, continued presence is enough to prevent a
/// false pane-gone retirement. Absence remains authoritative server-wide.
pub(crate) async fn physical_pane_gone_with_expected(
    watcher: &SessionWatcher,
    pane_id: &str,
    expected: Option<ProcessInstanceId>,
) -> Result<bool, &'static str> {
    let observed_pid = watcher
        .client()
        .server_pane_pid(pane_id)
        .await
        .map_err(|_| "composer_recovery_physical_pane_unproven")?;
    let Some(observed_pid) = observed_pid else {
        return Ok(true);
    };
    let Some(expected) = expected else {
        return Ok(false);
    };
    match crate::identity::ProcId::of(observed_pid) {
        Some(observed) => Ok(observed.pid != expected.pid() || observed.birth != expected.birth()),
        None if !fusion::pid_alive(observed_pid) => Ok(true),
        None => Err("composer_recovery_physical_pane_unproven"),
    }
}

/// Conservative physical-loss proof when no tmux watcher can be opened.
pub(crate) fn pane_root_gone(adoption: &crate::registry::Adoption) -> Result<bool, &'static str> {
    let expected = adoption
        .pane_root
        .ok_or("composer_recovery_pane_root_unproven")?;
    match crate::identity::ProcId::of(expected.pid()) {
        Some(observed) => Ok(observed.pid != expected.pid() || observed.birth != expected.birth()),
        None if !fusion::pid_alive(expected.pid()) => Ok(true),
        None => Err("composer_recovery_physical_pane_unproven"),
    }
}

fn writer_requires_reopen(error: &mailbox::MailboxServiceError) -> bool {
    matches!(
        error,
        mailbox::MailboxServiceError::Store(mailbox::MessageStoreError::Ledger(
            cyclops_ledger::LedgerError::Io { .. }
                | cyclops_ledger::LedgerError::WriteStateUnknown(_)
        ))
    )
}

/// Boot-time variant used before the mailbox service owns the store.
pub(crate) fn retire_gone_in_store(
    store: &mut crate::mailbox::MessageStore,
    recipients: impl IntoIterator<Item = RecipientKey>,
) -> Result<(), crate::mailbox::MessageStoreError> {
    let recipients: HashSet<_> = recipients.into_iter().collect();
    let records: Vec<_> = store
        .projection()
        .active_notification_barriers()
        .into_iter()
        .filter(|record| recipients.contains(&record.recipient))
        .collect();
    for record in records {
        store.retire_notification_barrier(
            record.message_id,
            record.recipient,
            record.attempt_id,
            NotificationBarrierRetirementCause::PaneGone,
            None,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyclops_proto::{
        MessageId, NotificationManifestId, NotificationState, NotificationTransport,
        ProcessInstanceId, Sensor, SensorReading, SessionInstanceId, TmuxPaneId, WorkspaceId,
    };
    use std::path::PathBuf;

    fn recipient_at(session: &str, pane: &str) -> RecipientKey {
        RecipientKey::agent(
            "00000000-0000-4000-8000-000000000001"
                .parse::<WorkspaceId>()
                .unwrap(),
            session.parse::<SessionInstanceId>().unwrap(),
            pane.parse::<TmuxPaneId>().unwrap(),
        )
    }

    fn recipient(session: &str) -> RecipientKey {
        recipient_at(session, "%1")
    }

    fn binding(recipient: RecipientKey, pid: i32) -> NotificationBinding {
        NotificationBinding {
            recipient,
            pane_root: Some(ProcessInstanceId::new(pid - 1, pid as u64 + 99).unwrap()),
            leader: Some(ProcessInstanceId::new(pid, pid as u64 + 100).unwrap()),
            agent: ProcessInstanceId::new(pid + 1, pid as u64 + 101).unwrap(),
            manifest: NotificationManifestId::new("codex").unwrap(),
        }
    }

    fn record(recipient: RecipientKey, binding: NotificationBinding) -> NotificationRecord {
        NotificationRecord {
            attempt_id: NotificationAttemptId::generate(),
            message_id: MessageId::new("m-recovered").unwrap(),
            recipient,
            state: NotificationState::AttentionRequired,
            binding: Some(binding),
            transport: NotificationTransport::Doorbell,
            doorbell_format: None,
            cause: None,
            pre_write_cause: None,
            pre_write_observation: None,
            pre_write_reopen_count: 0,
            started_seq: 4,
            updated_seq: 5,
            updated_at: 6,
        }
    }

    fn detection(readings: Vec<SensorReading>) -> Detection {
        Detection {
            state: AgentState::Idle,
            readings,
            disagreement: false,
            decided_by: "screen:idle".into(),
            stale: false,
            write_ready: false,
            write_block: None,
            composer_semantic: Some(cyclops_proto::ComposerSemantic::Clean),
        }
    }

    fn reading(sensor: Sensor, state: AgentState) -> SensorReading {
        SensorReading {
            sensor,
            state,
            rule: "fixture".into(),
            ts: 1,
        }
    }

    #[test]
    fn only_a_settled_attempt_may_retire_from_current_clean_evidence() {
        let route = recipient("00000000-0000-4000-8000-000000000002");
        let expected = binding(route, 40);
        for state in [
            NotificationState::Writing,
            NotificationState::Staged,
            NotificationState::Submitting,
            NotificationState::Submitted,
            NotificationState::AttentionRequired,
        ] {
            let mut record = record(route, expected.clone());
            record.state = state;
            let mut recovery = RecoveryCoordinator::new([record.attempt_id]);
            assert_eq!(
                recovery.reconcile(std::slice::from_ref(&record), Some(&expected), true, false,),
                Some(RecoveryAction::Restore(record.attempt_id)),
                "{state:?} has no receipt proof"
            );
        }

        let mut record = record(route, expected.clone());
        record.state = NotificationState::Notified;
        let mut recovery = RecoveryCoordinator::new([record.attempt_id]);
        assert_eq!(
            recovery.reconcile(std::slice::from_ref(&record), Some(&expected), false, false,),
            Some(RecoveryAction::Restore(record.attempt_id))
        );
        assert_eq!(
            recovery.reconcile(std::slice::from_ref(&record), Some(&expected), true, false,),
            Some(RecoveryAction::Retire {
                record: Box::new(record.clone()),
                cause: NotificationBarrierRetirementCause::ComposerObservedClear,
                replacement: None,
            })
        );

        record.state = NotificationState::WithdrawnAfterStaging;
        let mut recovery = RecoveryCoordinator::new([record.attempt_id]);
        assert!(matches!(
            recovery.reconcile(std::slice::from_ref(&record), Some(&expected), true, false,),
            Some(RecoveryAction::Retire {
                cause: NotificationBarrierRetirementCause::ComposerObservedClear,
                ..
            })
        ));
    }

    #[test]
    fn a_same_run_late_claim_uses_the_recovery_barrier_path() {
        let route = recipient("00000000-0000-4000-8000-000000000002");
        let expected = binding(route, 40);
        let mut record = record(route, expected.clone());
        record.state = NotificationState::Notified;
        let mut recovery = RecoveryCoordinator::default();

        let records = recovery.active_for_recipient(std::slice::from_ref(&record), route);
        assert_eq!(
            recovery.reconcile(&records, Some(&expected), true, false),
            None,
            "an untracked same-run barrier is not boot recovery work"
        );
        recovery.track(record.attempt_id);
        let records = recovery.active_for_recipient(std::slice::from_ref(&record), route);
        assert_eq!(
            recovery.reconcile(&records, Some(&expected), false, false),
            Some(RecoveryAction::Restore(record.attempt_id))
        );
        assert!(matches!(
            recovery.reconcile(&records, Some(&expected), true, false),
            Some(RecoveryAction::Retire {
                cause: NotificationBarrierRetirementCause::ComposerObservedClear,
                ..
            })
        ));
    }

    #[test]
    fn an_incomplete_legacy_barrier_retires_only_after_claim_and_clean_screen() {
        let route = recipient("00000000-0000-4000-8000-000000000002");
        let live = binding(route, 40);
        let mut legacy = live.clone();
        legacy.pane_root = None;
        let record = record(route, legacy);

        let mut recovery = RecoveryCoordinator::new([record.attempt_id]);
        assert_eq!(
            recovery.reconcile(std::slice::from_ref(&record), Some(&live), true, false,),
            Some(RecoveryAction::Hold("legacy_durable_binding_incomplete"))
        );

        let mut recovery = RecoveryCoordinator::new([record.attempt_id]);
        assert_eq!(
            recovery.reconcile(std::slice::from_ref(&record), Some(&live), true, true,),
            Some(RecoveryAction::Retire {
                record: Box::new(record.clone()),
                cause: NotificationBarrierRetirementCause::RecipientClaimedComposerClear,
                replacement: None,
            })
        );

        let mut wrong_manifest = live.clone();
        wrong_manifest.manifest = NotificationManifestId::new("claude").unwrap();
        let mut recovery = RecoveryCoordinator::new([record.attempt_id]);
        assert_eq!(
            recovery.reconcile(
                std::slice::from_ref(&record),
                Some(&wrong_manifest),
                true,
                true,
            ),
            Some(RecoveryAction::Hold("legacy_durable_binding_incomplete"))
        );

        let moved_route = recipient("00000000-0000-4000-8000-000000000003");
        let mut moved = live.clone();
        moved.recipient = moved_route;
        let mut recovery = RecoveryCoordinator::new([record.attempt_id]);
        assert_eq!(
            recovery.reconcile(std::slice::from_ref(&record), Some(&moved), true, true,),
            Some(RecoveryAction::Hold("legacy_durable_binding_incomplete"))
        );

        let mut direct = record.clone();
        direct.transport = NotificationTransport::DirectPayload;
        let mut recovery = RecoveryCoordinator::new([direct.attempt_id]);
        assert_eq!(
            recovery.reconcile(std::slice::from_ref(&direct), Some(&live), true, true,),
            Some(RecoveryAction::Hold("composer_recovery_unproven"))
        );
    }

    #[test]
    fn a_recovered_barrier_follows_its_physical_pane_across_a_session_route() {
        let old = recipient("00000000-0000-4000-8000-000000000002");
        let new = recipient("00000000-0000-4000-8000-000000000003");
        let other = recipient_at("00000000-0000-4000-8000-000000000003", "%2");
        let old_record = record(old, binding(old, 40));
        let mut recovery = RecoveryCoordinator::new([old_record.attempt_id]);

        assert_eq!(
            recovery.active_for_recipient(std::slice::from_ref(&old_record), new),
            vec![old_record.clone()]
        );
        assert!(recovery
            .active_for_recipient(std::slice::from_ref(&old_record), other)
            .is_empty());
    }

    #[test]
    fn one_authenticated_replacement_observation_retires_the_old_occupant() {
        let route = recipient("00000000-0000-4000-8000-000000000002");
        let original = binding(route, 40);
        let replacement = binding(route, 80);
        let record = record(route, original);
        let mut recovery = RecoveryCoordinator::new([record.attempt_id]);

        assert_eq!(
            recovery.reconcile(
                std::slice::from_ref(&record),
                Some(&replacement),
                false,
                false,
            ),
            Some(RecoveryAction::Retire {
                record: Box::new(record),
                cause: NotificationBarrierRetirementCause::OccupantReplaced,
                replacement: Some(replacement),
            })
        );
    }

    #[test]
    fn foreground_leader_changes_do_not_replace_the_composer_occupant() {
        let route = recipient("00000000-0000-4000-8000-000000000002");
        let expected = binding(route, 40);
        let mut changed_leader = expected.clone();
        changed_leader.leader = Some(ProcessInstanceId::new(900, 1_000).unwrap());
        let record = record(route, expected);
        let mut recovery = RecoveryCoordinator::new([record.attempt_id]);

        assert_eq!(
            recovery.reconcile(
                std::slice::from_ref(&record),
                Some(&changed_leader),
                true,
                false,
            ),
            Some(RecoveryAction::Restore(record.attempt_id))
        );
    }

    #[test]
    fn a_manifest_change_replaces_the_occupant_even_when_the_agent_is_unchanged() {
        let old = recipient("00000000-0000-4000-8000-000000000002");
        let new = recipient("00000000-0000-4000-8000-000000000003");
        let expected = binding(old, 40);
        let mut replacement = expected.clone();
        replacement.recipient = new;
        replacement.manifest = NotificationManifestId::new("claude").unwrap();
        let record = record(old, expected);
        let mut recovery = RecoveryCoordinator::new([record.attempt_id]);

        let action = recovery.reconcile(
            std::slice::from_ref(&record),
            Some(&replacement),
            false,
            false,
        );
        let Some(RecoveryAction::Retire {
            replacement: Some(replacement),
            cause: NotificationBarrierRetirementCause::OccupantReplaced,
            ..
        }) = action
        else {
            panic!("manifest replacement must retire deterministically");
        };
        assert_eq!(replacement.recipient, old);
    }

    #[test]
    fn a_leaderless_write_boundary_still_restores_on_agent_manifest_continuity() {
        let route = recipient("00000000-0000-4000-8000-000000000002");
        let mut expected = binding(route, 40);
        expected.leader = None;
        let live = binding(route, 90);
        expected.pane_root = live.pane_root;
        expected.agent = live.agent;
        expected.manifest = live.manifest.clone();
        let record = record(route, expected);
        let mut recovery = RecoveryCoordinator::new([record.attempt_id]);

        assert_eq!(
            recovery.reconcile(std::slice::from_ref(&record), Some(&live), true, false,),
            Some(RecoveryAction::Restore(record.attempt_id))
        );
    }

    #[test]
    fn only_current_conjunctive_screen_evidence_proves_a_clean_composer() {
        let clean = detection(vec![
            reading(Sensor::Title, AgentState::Idle),
            reading(Sensor::Screen, AgentState::Idle),
        ]);
        assert!(clean_composer(&clean, false));

        let mut stale = clean.clone();
        stale.stale = true;
        assert!(!clean_composer(&stale, false));

        let mut disagreement = clean.clone();
        disagreement.disagreement = true;
        assert!(!clean_composer(&disagreement, false));

        let conflicting = detection(vec![
            reading(Sensor::Title, AgentState::Working),
            reading(Sensor::Screen, AgentState::Idle),
        ]);
        assert!(!clean_composer(&conflicting, false));

        let no_screen = detection(vec![reading(Sensor::Title, AgentState::Idle)]);
        assert!(!clean_composer(&no_screen, false));
        assert!(!clean_composer(&clean, true));

        let route = recipient("00000000-0000-4000-8000-000000000002");
        let bound = binding(route, 40);
        assert!(clean_composer_for_binding(
            &clean,
            false,
            Some("codex"),
            &bound
        ));
        assert!(!clean_composer_for_binding(
            &clean,
            false,
            Some("claude"),
            &bound
        ));
    }

    #[test]
    fn a_recovered_attempt_restores_an_owned_barrier_without_a_turn_key() {
        let attempt_id = NotificationAttemptId::generate();
        let (hold, owner, clear_turn, refusal) = merge_barrier(
            Some(&RecoveryAction::Restore(attempt_id)),
            None,
            ComposerHold::Clear,
            None,
            false,
        );

        assert_eq!(hold, ComposerHold::Staged);
        let expected_owner = attempt_id.to_string();
        assert_eq!(owner.as_deref(), Some(expected_owner.as_str()));
        assert!(
            clear_turn,
            "restart recovery must not carry a lifecycle key"
        );
        assert_eq!(refusal, None);
    }

    #[test]
    fn repeated_recovery_preserves_a_post_restart_exact_turn() {
        let attempt_id = NotificationAttemptId::generate();
        let owner = attempt_id.to_string();
        let (hold, retained_owner, clear_turn, refusal) = merge_barrier(
            Some(&RecoveryAction::Restore(attempt_id)),
            None,
            ComposerHold::TurnStarted { since_ms: 8 },
            Some(owner.clone()),
            false,
        );

        assert_eq!(hold, ComposerHold::TurnStarted { since_ms: 8 });
        assert_eq!(retained_owner.as_deref(), Some(owner.as_str()));
        assert!(!clear_turn);
        assert_eq!(refusal, None);
    }

    #[test]
    fn retiring_one_recovered_owner_never_clears_another_runtime_barrier() {
        let recovered = NotificationAttemptId::generate();
        let runtime = NotificationAttemptId::generate().to_string();

        let (hold, owner, clear_turn, refusal) = merge_barrier(
            None,
            Some(recovered),
            ComposerHold::Staged,
            Some(runtime.clone()),
            false,
        );

        assert_eq!(hold, ComposerHold::Staged);
        assert_eq!(owner.as_deref(), Some(runtime.as_str()));
        assert!(!clear_turn);
        assert_eq!(refusal, None);
    }

    #[test]
    fn an_unknown_writer_requires_reopen_instead_of_retrying() {
        let error = mailbox::MailboxServiceError::Store(mailbox::MessageStoreError::Ledger(
            cyclops_ledger::LedgerError::WriteStateUnknown(PathBuf::from("messages.ndjson")),
        ));
        assert!(writer_requires_reopen(&error));

        let attempt_id = NotificationAttemptId::generate();
        let mut recovery = RecoveryCoordinator::new([attempt_id]);
        recovery.require_reopen();
        assert!(recovery.writer_unknown());
        assert!(recovery.retiring.is_empty());
        assert!(recovery.tracked_attempts.contains(&attempt_id));
    }

    #[test]
    fn a_concurrent_retirement_is_revalidated_before_runtime_state_changes() {
        let attempt_id = NotificationAttemptId::generate();
        let mut recovery = RecoveryCoordinator::new([attempt_id]);
        assert_eq!(
            recovery.retirement_pending_reason(attempt_id),
            Some("composer_recovery_retirement_failed")
        );

        recovery.retiring.insert(attempt_id);
        assert_eq!(
            recovery.retirement_pending_reason(attempt_id),
            Some("composer_recovery_retirement_pending")
        );

        recovery.retired(attempt_id);
        assert_eq!(recovery.retirement_pending_reason(attempt_id), None);
    }
}
