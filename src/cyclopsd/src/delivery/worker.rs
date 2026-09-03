//! Notification and direct-delivery worker loops and supervisor lifecycle.

use super::*;

pub(crate) struct NotificationWorker {
    pub(crate) worker: Arc<Worker>,
    pub(crate) task: JoinHandle<()>,
}

pub(crate) struct LegacyWorker {
    pub(crate) worker: Arc<Worker>,
    pub(crate) task: JoinHandle<()>,
}

/// Per-recipient FIFO worker. Notification workers sleep on `notify`; legacy
/// workers retire their registry entry once the FIFO becomes idle.
pub(crate) struct Worker {
    pub(crate) state: StdMutex<WorkerState>,
    pub(crate) notify: Notify,
    /// Set when quota parking hit this recipient; carries the reset hint.
    /// Cleared only by an operator recovery verb. Never auto-retried.
    pub(crate) parked: StdMutex<Option<String>>,
}

pub(crate) struct WorkerState {
    pub(crate) queue: VecDeque<Arc<DeliveryHandle>>,
    /// Strong ownership of the exact job removed from the FIFO.
    pub(crate) current: Option<Arc<DeliveryHandle>>,
    /// Visible reason the supervisor stopped restarting this worker.
    pub(crate) fault: Option<String>,
    /// Bounds failures that happen outside an exact current job.
    pub(crate) empty_restarts: u8,
}

impl Worker {
    pub(crate) fn new() -> Self {
        Self {
            state: StdMutex::new(WorkerState {
                queue: VecDeque::new(),
                current: None,
                fault: None,
                empty_restarts: 0,
            }),
            notify: Notify::new(),
            parked: StdMutex::new(None),
        }
    }

    pub(crate) fn enqueue_back(&self, handle: Arc<DeliveryHandle>) {
        self.state
            .lock()
            .expect("worker state lock")
            .queue
            .push_back(handle);
    }

    /// Return the exact in-flight job to the FIFO head without releasing
    /// ownership of it. Used when quiesce closes the gate after admission
    /// but before the pane write.
    pub(crate) fn requeue_current_front(&self, handle: &Arc<DeliveryHandle>) -> bool {
        let mut state = self.state.lock().expect("worker state lock");
        let owns = state
            .current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, handle));
        if owns {
            let current = state.current.take().expect("current checked above");
            state.queue.push_front(current);
        }
        owns
    }

    pub(crate) fn drain_pending(&self) -> Vec<Arc<DeliveryHandle>> {
        self.state
            .lock()
            .expect("worker state lock")
            .queue
            .drain(..)
            .collect()
    }

    pub(crate) fn prepend(&self, handles: Vec<Arc<DeliveryHandle>>) {
        let mut state = self.state.lock().expect("worker state lock");
        for handle in handles.into_iter().rev() {
            state.queue.push_front(handle);
        }
    }

    /// Return the already-owned job after a supervisor restart, or take the FIFO head.
    pub(crate) fn current_or_next(&self) -> Option<Arc<DeliveryHandle>> {
        let mut state = self.state.lock().expect("worker state lock");
        if let Some(current) = &state.current {
            return Some(Arc::clone(current));
        }
        let next = state.queue.pop_front()?;
        state.current = Some(Arc::clone(&next));
        Some(next)
    }

    /// Release this exact in-flight job. False means ownership already moved,
    /// such as a quiesce handback or a supervisor replacing a failed run.
    pub(crate) fn finish(&self, handle: &Arc<DeliveryHandle>) -> bool {
        let mut state = self.state.lock().expect("worker state lock");
        let owns = state
            .current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, handle));
        if owns {
            state.current = None;
            state.empty_restarts = 0;
        }
        owns
    }

    pub(crate) fn replace_current(
        &self,
        old: &Arc<DeliveryHandle>,
        new: Arc<DeliveryHandle>,
    ) -> bool {
        let mut state = self.state.lock().expect("worker state lock");
        let owns = state
            .current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, old));
        if owns {
            state.current = Some(new);
        }
        owns
    }

    #[cfg(test)]
    pub(crate) fn current(&self) -> Option<Arc<DeliveryHandle>> {
        self.state
            .lock()
            .expect("worker state lock")
            .current
            .clone()
    }

    pub(crate) fn is_idle(&self) -> bool {
        let state = self.state.lock().expect("worker state lock");
        state.current.is_none() && state.queue.is_empty()
    }

    pub(crate) fn set_fault(&self, cause: impl Into<String>) {
        self.state.lock().expect("worker state lock").fault = Some(cause.into());
    }

    pub(crate) fn is_faulted(&self) -> bool {
        self.state
            .lock()
            .expect("worker state lock")
            .fault
            .is_some()
    }

    /// Deliveries ahead of `handle` from the sender's point of view.
    pub(crate) fn position_of(&self, handle: &Arc<DeliveryHandle>) -> u32 {
        let state = self.state.lock().expect("worker state lock");
        let busy = state.current.is_some() as u32;
        match state.queue.iter().position(|h| Arc::ptr_eq(h, handle)) {
            Some(i) => i as u32 + busy,
            // Not queued: it is the in-flight one (or already resolved).
            None => 0,
        }
    }
}

/// Run one queue publication against the FIFO worker owning one pane.
pub(crate) fn with_worker<T, F>(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    action: F,
) -> Option<T>
where
    F: FnOnce(&Arc<Worker>) -> T,
{
    let pane = PaneKey::new(session_idx, pane_id);
    let task_inner = Arc::clone(inner);
    let task_pane = pane.clone();
    inner.engine.with_legacy_worker(
        pane,
        move |worker| {
            tokio::spawn(worker_supervisor(
                task_inner,
                task_pane,
                Arc::clone(&worker),
            ))
        },
        action,
    )
}

/// Attach an already-queued mailbox notification to the pane's existing FIFO worker.
///
/// Recipient selection and oldest-pending policy belong to the coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotificationEnqueueRefusal {
    DaemonStopping,
    WorkerFaulted,
    WorkerSupervisorExited,
    AttemptUnowned,
    ClassificationUnavailable,
    PayloadUnavailable,
}

/// The loop child owns normal FIFO work. The supervisor owns its task and
/// classifies every unexpected exit against the exact current handle before
/// starting a new child. A clean task return is not proof of success: only
/// daemon stop, a visible worker fault, or exact notification-registry
/// retirement is expected. Claim cancellation drains into that exact
/// retirement. Quiesce parks the child and is not an exit. Dropping the
/// supervisor aborts its child through `DeliveryTask`.
pub(crate) async fn supervise_worker_task<S, R, E>(
    mut spawn: S,
    mut recover: R,
    mut expected_exit: E,
) where
    S: FnMut() -> JoinHandle<()>,
    R: FnMut() -> bool,
    E: FnMut() -> bool,
{
    loop {
        let mut child = DeliveryTask(spawn());
        match child.wait().await {
            Ok(()) if expected_exit() => return,
            Ok(()) => {
                error!("delivery worker loop returned while it still owned its registry slot");
                if !recover() {
                    std::future::pending::<()>().await;
                }
            }
            Err(error) => {
                let exit = if error.is_cancelled() {
                    "cancelled"
                } else if error.is_panic() {
                    "panicked"
                } else {
                    "failed"
                };
                error!(%error, exit, "delivery worker loop failed");
                if expected_exit() {
                    return;
                }
                if !recover() {
                    std::future::pending::<()>().await;
                }
            }
        }
    }
}

pub(crate) fn recover_outer_worker(inner: &Arc<Inner>, worker: &Arc<Worker>) -> bool {
    if let Some(handle) = worker.current_or_next() {
        return recover_failed_job(inner, worker, &handle);
    }
    let mut state = worker.state.lock().expect("worker state lock");
    if state.empty_restarts == 0 {
        state.empty_restarts = 1;
        true
    } else {
        state.fault = Some("worker loop failed repeatedly without an owned job".into());
        false
    }
}

pub(crate) async fn worker_supervisor(inner: Arc<Inner>, pane: PaneKey, worker: Arc<Worker>) {
    supervise_worker_task(
        || {
            inner.engine.spawn_descendant_task(worker_loop(
                Arc::clone(&inner),
                pane.clone(),
                Arc::clone(&worker),
            ))
        },
        || recover_outer_worker(&inner, &worker),
        || {
            inner.engine.is_stopping()
                || worker.is_faulted()
                || !inner.engine.legacy_worker_is_current(&pane, &worker)
        },
    )
    .await;
}

pub(crate) async fn notification_worker_supervisor(
    inner: Arc<Inner>,
    recipient: RecipientKey,
    worker: Arc<Worker>,
) {
    supervise_worker_task(
        || {
            inner.engine.spawn_descendant_task(notification_worker_loop(
                Arc::clone(&inner),
                recipient,
                Arc::clone(&worker),
            ))
        },
        || recover_outer_worker(&inner, &worker),
        || {
            inner.engine.is_stopping()
                || worker.is_faulted()
                || !inner
                    .engine
                    .notification_worker_is_current(recipient, &worker)
        },
    )
    .await;
}

pub(crate) async fn worker_loop(inner: Arc<Inner>, pane: PaneKey, worker: Arc<Worker>) {
    loop {
        // A quiesce holds the pipeline still: finish nothing new until
        // resume_workers notifies. Jobs stay queued (pre-paste, safe
        // across the restart the quiesce is for).
        if inner.engine.paused.load(Ordering::SeqCst) {
            worker.notify.notified().await;
            continue;
        }
        let job = worker.current_or_next();
        match job {
            Some(handle) => {
                let parked_hint =
                    legacy_park_hint(&handle, worker.parked.lock().expect("parked lock").clone());
                if let Some(hint) = parked_hint {
                    // A job that raced in around the parking moment parks
                    // too. Workspace notifications bypass this legacy
                    // flag and let their own gate hold on live quota state.
                    advance(
                        &inner,
                        &handle,
                        &[DeliveryState::Queued],
                        Step::to(DeliveryState::ParkedBlockedQuota)
                            .cause("blocked_quota")
                            .note(hint),
                    );
                    worker.finish(&handle);
                    continue;
                }
                match supervised_process(&inner, &worker, &handle).await {
                    Ok(()) => {
                        worker.finish(&handle);
                    }
                    Err(error) => {
                        error!(id = %handle.msg_id, %error, "legacy delivery worker failed");
                        if !recover_failed_job(&inner, &worker, &handle) {
                            return;
                        }
                    }
                }
            }
            None => {
                if inner.engine.retire_legacy_worker(&pane, &worker) {
                    return;
                }
            }
        }
    }
}

pub(crate) async fn notification_worker_loop(
    inner: Arc<Inner>,
    recipient: RecipientKey,
    worker: Arc<Worker>,
) {
    loop {
        if inner.engine.paused.load(Ordering::SeqCst) {
            if inner.engine.retire_notification_worker(recipient, &worker) {
                return;
            }
            worker.notify.notified().await;
            continue;
        }
        let job = worker.current_or_next();
        let Some(handle) = job else {
            if inner.engine.retire_notification_worker(recipient, &worker) {
                return;
            }
            continue;
        };
        match supervised_process(&inner, &worker, &handle).await {
            Ok(()) => {
                if worker.is_faulted() {
                    return;
                }
                // Quiesce may have atomically returned this same handle to
                // the FIFO. In that case its attempt index remains active.
                if worker.finish(&handle) {
                    let retired = inner.engine.retire_notification_run(&handle);
                    if retired
                        && handle
                            .claimed_notification_rerun_requested
                            .swap(false, Ordering::SeqCst)
                    {
                        if let Some(notification) = &handle.notification {
                            if let Some(messaging) = inner.workspace_messaging() {
                                if let Err(error) =
                                    messaging.notification_head_changed(notification.recipient())
                                {
                                    error!(
                                        id = %handle.msg_id,
                                        %error,
                                        "cannot reschedule claimed notification after readiness edge"
                                    );
                                }
                            } else {
                                error!(
                                    id = %handle.msg_id,
                                    "cannot reschedule claimed notification without workspace messaging"
                                );
                            }
                        }
                    }
                }
            }
            Err(error) => {
                error!(id = %handle.msg_id, %error, "notification delivery worker failed");
                if !recover_failed_job(&inner, &worker, &handle) {
                    return;
                }
            }
        }
    }
}

/// Abort the child if its owning worker is cancelled during shutdown.
pub(crate) struct DeliveryTask(pub(crate) JoinHandle<()>);

impl DeliveryTask {
    pub(crate) async fn wait(&mut self) -> Result<(), tokio::task::JoinError> {
        (&mut self.0).await
    }
}

impl Drop for DeliveryTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) async fn supervised_process(
    inner: &Arc<Inner>,
    worker: &Arc<Worker>,
    handle: &Arc<DeliveryHandle>,
) -> Result<(), tokio::task::JoinError> {
    let task_inner = Arc::clone(inner);
    let worker = Arc::clone(worker);
    let handle = Arc::clone(handle);
    let mut task = DeliveryTask(inner.engine.spawn_descendant_task(async move {
        process(&task_inner, &worker, &handle).await;
    }));
    task.wait().await
}

/// Classify one panicked job from its exact durable boundary.
///
/// True restarts the worker loop. False leaves a visible fault and stops it.
pub(crate) fn recover_failed_job(
    inner: &Arc<Inner>,
    worker: &Arc<Worker>,
    handle: &Arc<DeliveryHandle>,
) -> bool {
    let recovery = handle.worker_recoveries.fetch_add(1, Ordering::SeqCst);
    if let Some(notification) = &handle.notification {
        let current = match notification.current_record() {
            Ok(current) => current,
            Err(error) => {
                worker.set_fault(format!("notification recovery failed: {error}"));
                return false;
            }
        };
        if current.attempt_id != notification.attempt_id()
            || current.execution_epoch() != notification.run_epoch()
        {
            // Another run now owns this attempt. The stale task has no fact left to write.
            worker.finish(handle);
            inner.engine.retire_notification_run(handle);
            return true;
        }
        match current.state {
            NotificationState::Queued | NotificationState::Gating if recovery == 0 => {
                let fresh = DeliveryHandle::for_notification(
                    &handle.to,
                    &handle.pane_id,
                    handle.session_idx,
                    cyclops_proto::render_doorbell_v3(notification.attempt_id()),
                    notification.clone(),
                );
                fresh.worker_recoveries.store(1, Ordering::SeqCst);
                if !worker.replace_current(handle, Arc::clone(&fresh))
                    || !inner.engine.replace_notification_run(handle, &fresh)
                {
                    worker.set_fault("notification recovery lost exact job ownership");
                    worker.finish(&fresh);
                    return false;
                }
                true
            }
            NotificationState::Queued | NotificationState::Gating => {
                let Some(messaging) = inner.workspace_messaging() else {
                    worker.set_fault("notification recovery has no workspace messaging Module");
                    return false;
                };
                let block = match messaging.record_worker_failed_prewrite(notification) {
                    Ok(MessagingPreWriteBlockOutcome::Recorded(block)) => Some(block),
                    Ok(MessagingPreWriteBlockOutcome::Obsolete) => None,
                    Err(error) => {
                        worker.set_fault(format!("notification recovery failed: {error}"));
                        return false;
                    }
                };
                if let Some(block) = block {
                    notify_notification_prewrite_blocked(inner, handle, &block);
                }
                worker.finish(handle);
                inner.engine.retire_notification_run(handle);
                true
            }
            NotificationState::Staged
                if recovery == 0
                    && notification
                        .claimed_notification_barrier()
                        .is_ok_and(|barrier| barrier.is_some()) =>
            {
                true
            }
            NotificationState::Writing
            | NotificationState::Staged
            | NotificationState::Submitting
            | NotificationState::Submitted => {
                if let Err(error) = notification
                    .record_attention(NotificationAttentionCause::TransportOutcomeUnknown)
                {
                    worker.set_fault(format!("notification recovery failed: {error}"));
                    return false;
                }
                unregister_ack(inner, handle);
                let _ = advance(
                    inner,
                    handle,
                    &[
                        DeliveryState::Pasting,
                        DeliveryState::Staged,
                        DeliveryState::Submitted,
                        DeliveryState::RetryQueued,
                    ],
                    Step::to(DeliveryState::AttentionRequired).cause("worker_failed_after_write"),
                );
                worker.finish(handle);
                inner.engine.retire_notification_run(handle);
                true
            }
            NotificationState::Notified => {
                unregister_ack(inner, handle);
                if current.transport == NotificationTransport::DirectPayload {
                    if let Err(error) = notification.record_delivered_direct() {
                        worker.set_fault(format!("direct settlement recovery failed: {error}"));
                        return false;
                    }
                    let recipient = notification.recipient();
                    if let Some(messaging) = inner.workspace_messaging() {
                        if let Err(error) = messaging.direct_delivery_settled(recipient) {
                            worker
                                .set_fault(format!("direct settlement scheduling failed: {error}"));
                            return false;
                        }
                    } else {
                        worker.set_fault(
                            "direct settlement scheduling failed: workspace messaging unavailable",
                        );
                        return false;
                    }
                }
                let _ = advance(
                    inner,
                    handle,
                    &[DeliveryState::Submitted],
                    Step::to(DeliveryState::DeliveredUnverified)
                        .cause("worker_recovered_notified")
                        .verified(VerifiedBy::Screen),
                );
                worker.finish(handle);
                inner.engine.retire_notification_run(handle);
                true
            }
            NotificationState::BlockedPreWrite
            | NotificationState::QuotaHeld
            | NotificationState::QuotaResetObserved
            | NotificationState::SubmittedUnverified
            | NotificationState::AttentionRequired
            | NotificationState::Withdrawn
            | NotificationState::WithdrawnAfterStaging
            | NotificationState::WithdrawnByOperator
            | NotificationState::Superseded => {
                worker.finish(handle);
                inner.engine.retire_notification_run(handle);
                true
            }
        }
    } else if !handle.write_boundary_crossed.load(Ordering::SeqCst) && recovery == 0 {
        let state = handle.state();
        if state != DeliveryState::Queued
            && state != DeliveryState::RetryQueued
            && !advance(
                inner,
                handle,
                &[DeliveryState::Gating, DeliveryState::Pasting],
                Step::to(DeliveryState::RetryQueued).cause("worker_recovered_before_write"),
            )
        {
            worker.set_fault("legacy pre-write recovery could not requeue its exact job");
            return false;
        }
        true
    } else {
        let cause = if handle.write_boundary_crossed.load(Ordering::SeqCst) {
            "worker_failed_after_write"
        } else {
            "worker_failed_before_write"
        };
        let moved = advance(
            inner,
            handle,
            &[
                DeliveryState::Queued,
                DeliveryState::Gating,
                DeliveryState::Pasting,
                DeliveryState::Staged,
                DeliveryState::Submitted,
                DeliveryState::RetryQueued,
            ],
            Step::to(DeliveryState::AttentionRequired).cause(cause),
        );
        if moved {
            notify_attention(inner, handle, cause);
        }
        worker.finish(handle);
        if !moved {
            worker.set_fault(format!("legacy recovery stopped in {:?}", handle.state()));
            return false;
        }
        true
    }
}

/// Persist one known pre-write block without releasing FIFO ownership on an
/// uncertain journal append.
///
/// A successful or stale append lets the worker retire normally. A storage
/// failure faults the worker, so its current handle remains ahead of every
/// later notification for this recipient.
pub(crate) async fn persist_notification_prewrite_block(
    inner: &Arc<Inner>,
    worker: &Worker,
    handle: &DeliveryHandle,
    cause: NotificationPreWriteCause,
    observation: Option<NotificationPreWriteObservation>,
) {
    let Some(notification) = &handle.notification else {
        return;
    };
    let route_evidence = inner.route_evidence_id(handle.session_idx, &handle.pane_id);
    // Test seam: an admitting edge can land between the gate's verdict and
    // this append. The reconcile below must catch it, never strand it.
    inject_pause(inner, "pre_prewrite_block").await;
    let Some(messaging) = inner.workspace_messaging() else {
        worker.set_fault("notification pre-write block has no workspace messaging Module");
        return;
    };
    let block = match messaging.record_notification_prewrite_block(
        notification,
        cause,
        observation,
        route_evidence,
        handle.session_idx,
        &handle.pane_id,
    ) {
        Ok(MessagingPreWriteBlockOutcome::Recorded(block)) => block,
        Ok(MessagingPreWriteBlockOutcome::Obsolete) => return,
        Err(error) => {
            error!(id = %notification.message_id(), %error, "notification pre-write block fact failed");
            worker.set_fault(format!(
                "notification pre-write block storage failed: {error}"
            ));
            return;
        }
    };
    notify_notification_prewrite_blocked(inner, handle, &block);
    // A positive route edge can race just ahead of this append and see only
    // Gating. Re-observe after the durable block exists so that edge is not
    // lost. The Module enforces the one-reopen limit and exact binding checks.
    if let Some(watcher) = inner.watcher_of(handle.session_idx) {
        let route_evidence = inner.route_evidence_id(handle.session_idx, &handle.pane_id);
        crate::observe_pane_for_route_evidence(
            inner,
            handle.session_idx,
            &watcher,
            &handle.pane_id,
            true,
            "prewrite_block_reconcile",
            &route_evidence,
        )
        .await;
        messaging.notification_prewrite_blocked(handle.session_idx, &handle.pane_id);
    }
}

/// Drive one delivery through gate, inject, submit, ACK, bounded retry.
pub(crate) async fn process(
    inner: &Arc<Inner>,
    worker: &Arc<Worker>,
    handle: &Arc<DeliveryHandle>,
) {
    if let Some(notification) = &handle.notification {
        match notification.claimed_notification_barrier() {
            Ok(Some(barrier)) => {
                if let AttemptOutcome::Failed(failure) =
                    reconcile_recovered_claimed_notification_barrier(inner, handle, barrier).await
                {
                    inject_pause(inner, "post_claimed_notification_refusal").await;
                    if !fault_notification_worker(worker, &failure) {
                        let _ = fail_attempt(inner, worker, handle, &failure).await;
                    }
                }
                return;
            }
            Ok(None) => {}
            Err(error) => {
                error!(id = %handle.msg_id, %error, "cannot classify staged claim recovery");
                return;
            }
        }
    }
    // retry_queued alongside queued: a chain requeued across a daemon
    // restart, or parked by a quiesce, re-enters here in that state.
    if !advance(
        inner,
        handle,
        &[DeliveryState::Queued, DeliveryState::RetryQueued],
        Step::to(DeliveryState::Gating),
    ) {
        return;
    }
    if let Some(notification) = &handle.notification {
        match notification.record_gating() {
            Ok(_) => {}
            Err(NotificationAdapterError::NoLongerCurrentBeforeWrite) => {
                // A claim or replacement retired this attempt before it
                // touched the pane.
                return;
            }
            Err(error) => {
                error!(id = %handle.msg_id, error = %error, "notification gating fact failed");
                notify_notification_deferred(inner, handle, NOTIFICATION_RECORD_FAILED);
                return;
            }
        }
    }
    let mut regate_hold = None;
    loop {
        let gate_outcome = gate(inner, handle, regate_hold.take()).await;
        if let Some(notification) = &handle.notification {
            match notification.ensure_current_gating() {
                Ok(()) => {}
                Err(NotificationAdapterError::NoLongerCurrentBeforeWrite) => return,
                Err(error) => {
                    error!(id = %handle.msg_id, error = %error, "notification gate outcome recheck failed");
                    notify_notification_deferred(inner, handle, NOTIFICATION_RECORD_FAILED);
                    return;
                }
            }
        }
        match gate_outcome {
            GateOutcome::Withdrawn => return,
            GateOutcome::BlockedPreWrite { cause, observation } => {
                if handle.notification.is_none() {
                    return;
                }
                persist_notification_prewrite_block(
                    inner,
                    worker,
                    handle,
                    cause,
                    Some(*observation),
                )
                .await;
                return;
            }
            GateOutcome::Deferred { cause } => {
                notify_notification_deferred(inner, handle, &cause);
                return;
            }
            GateOutcome::Park { hint } => {
                park_recipient(inner, worker, handle, hint).await;
                return;
            }
            GateOutcome::Attention { cause } => {
                advance(
                    inner,
                    handle,
                    &[DeliveryState::Gating],
                    Step::to(DeliveryState::AttentionRequired).cause(&cause),
                );
                notify_attention(inner, handle, &cause);
                return;
            }
            GateOutcome::Proceed {
                manifest_id,
                pane_pid,
                regate_evidence_changed,
            } => {
                if regate_evidence_changed {
                    reset_immediate_regates(handle);
                }
                // A quiesce that landed while this delivery was at the
                // gate: nothing may cross the paste boundary now. Park
                // pre-paste and hand the job back; it re-enters when the
                // pipeline resumes or requeues across the restart the
                // quiesce was for.
                if inner.engine.paused.load(Ordering::SeqCst) {
                    if advance(
                        inner,
                        handle,
                        &[DeliveryState::Gating],
                        Step::to(DeliveryState::RetryQueued).cause("quiesce"),
                    ) && !worker.requeue_current_front(handle)
                    {
                        error!(id = %handle.msg_id, "quiesce lost exact worker ownership");
                    }
                    return;
                }
                {
                    let mut st = handle.state.lock().expect("handle state lock");
                    st.attempts += 1;
                }
                if !advance(
                    inner,
                    handle,
                    &[DeliveryState::Gating],
                    Step::to(DeliveryState::Pasting),
                ) {
                    return;
                }
                match attempt_delivery(inner, handle, &manifest_id, pane_pid).await {
                    AttemptOutcome::Done => return,
                    // The mailbox fact records why this attempt is no longer
                    // current. This worker only proves it never wrote.
                    AttemptOutcome::NoLongerCurrentBeforeWrite => return,
                    AttemptOutcome::Failed(failure) => {
                        if fault_notification_worker(worker, &failure) {
                            return;
                        }
                        // Readiness moved between the gate's proof and
                        // the write. Nothing was written and no transport
                        // was spent, so this is not a retry: it goes back
                        // to the gate, which waits on the barrier's own
                        // release rather than on a budget.
                        if let Some(regate_cause) = failure.regate_cause() {
                            let action = regate_action(handle, regate_cause);
                            // The legal path back to the gate runs through
                            // RetryQueued. A mailbox race that cannot be
                            // re-proven settles as a durable pre-write block;
                            // legacy direct delivery waits on pane evidence.
                            if !advance(
                                inner,
                                handle,
                                &[DeliveryState::Pasting],
                                Step::to(DeliveryState::RetryQueued).cause(&failure.cause),
                            ) {
                                return;
                            }
                            if !advance(
                                inner,
                                handle,
                                &[DeliveryState::RetryQueued],
                                Step::to(DeliveryState::Gating).cause("regate"),
                            ) {
                                return;
                            }
                            match action {
                                RegateAction::ImmediateReproof => {}
                                RegateAction::Hold => {
                                    regate_hold = Some(failure.cause.clone());
                                }
                                RegateAction::BlockPreWrite => {
                                    if handle.notification.is_some() {
                                        persist_notification_prewrite_block(
                                            inner,
                                            worker,
                                            handle,
                                            NotificationPreWriteCause::WriteReadinessChanged,
                                            None,
                                        )
                                        .await;
                                        return;
                                    }
                                    // Legacy direct delivery has no durable
                                    // mailbox state or withdrawal verb. Keep
                                    // it held until exact pane evidence moves.
                                    regate_hold = Some(failure.cause.clone());
                                }
                            }
                            continue;
                        }
                        if !fail_attempt(inner, worker, handle, &failure).await {
                            return;
                        }
                        // Bounded retry: back through the gate.
                        if !advance(
                            inner,
                            handle,
                            &[DeliveryState::RetryQueued],
                            Step::to(DeliveryState::Gating).cause("retry"),
                        ) {
                            return;
                        }
                    }
                }
            }
        }
    }
}

pub(crate) fn fault_notification_worker(worker: &Worker, failure: &AttemptFailure) -> bool {
    if !failure.faults_notification_worker() {
        return false;
    }
    worker.set_fault(CLAIMED_STAGED_SETTLEMENT_FAILED);
    true
}
