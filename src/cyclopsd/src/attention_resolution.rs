//! Explicit recovery for a notification left in an agent composer.

use std::sync::Arc;
use std::time::Duration;

use cyclops_manifest::{strip_csi, Manifest};
use cyclops_proto::{
    AgentState, AttentionChecks, AttentionResolveResult, AttentionShowResult, Event,
    NotificationBinding, NotificationResolution, NotificationResolutionConsumptionObservation,
    ProcessInstanceId,
};
use cyclops_tmux::{PaneRow, SessionWatcher};

use crate::mailbox::{
    AttentionConsumptionSignal, AttentionResolutionStart, AttentionTarget, MailboxService,
    MailboxServiceError,
};
use crate::{delivery, fusion, unix_ms, Inner};

// Bound terminal-action settlement while allowing slower terminal clients to
// render the clean composer that proves the exact action took effect.
const POST_ACTION_PROOF_DELAYS_MS: [u64; 5] = [0, 120, 240, 480, 1_000];
const POST_ACTION_EVENT_SCANS_PER_CHECKPOINT: usize = 8;

#[derive(Debug, thiserror::Error)]
pub(crate) enum AttentionActionError {
    #[error(transparent)]
    Store(#[from] MailboxServiceError),
    #[error("the current terminal does not satisfy every attention evidence check")]
    Evidence(Box<AttentionShowResult>),
    #[error("this manifest has no measured whole-composer clear sequence")]
    DiscardUnsupported,
    #[error(
        "the terminal action outcome is uncertain; no second key will be sent; reopen this exact attempt after its required durable evidence is recorded"
    )]
    Uncertain,
}

struct ActionRoute {
    session_idx: usize,
    watcher: Arc<SessionWatcher>,
    row: PaneRow,
    manifest: Manifest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolutionPathKind {
    TerminalKey,
    ComposerAlreadyClear,
}

enum ResolutionPath {
    TerminalKey(ActionRoute),
    ComposerAlreadyClear(ActionRoute),
}

struct Assessment {
    result: AttentionShowResult,
    path: Option<ResolutionPath>,
}

pub(crate) async fn show(
    inner: &Arc<Inner>,
    service: &MailboxService,
    target: &AttentionTarget,
    include_diff: bool,
) -> AttentionShowResult {
    assess(inner, service, target, include_diff).await.result
}

pub(crate) async fn resolve(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    target: &AttentionTarget,
    resolution: NotificationResolution,
) -> Result<AttentionResolveResult, AttentionActionError> {
    let start = service.begin_attention_resolution(target, resolution)?;
    let attempt_id = target.record.attempt_id;

    match start {
        AttentionResolutionStart::Fresh => {}
        AttentionResolutionStart::ReconcileOnly => {
            return reconcile_existing_intent(inner, service, target, resolution, false).await;
        }
        AttentionResolutionStart::IntentOnlyUncertain => {
            if resolution == NotificationResolution::Discard {
                return resolve_clear_composer_discard(inner, service, target).await;
            }
            service.cancel_attention_resolution(attempt_id)?;
            return Err(AttentionActionError::Uncertain);
        }
        AttentionResolutionStart::AcceptedUnconsumed => {
            return reconcile_existing_intent(inner, service, target, resolution, true).await;
        }
    }

    let first = assess(inner, service, target, false).await;
    let Some(path_kind) = resolution_path(&first, resolution) else {
        service.cancel_attention_resolution(attempt_id)?;
        return Err(AttentionActionError::Evidence(Box::new(first.result)));
    };
    if path_kind == ResolutionPathKind::ComposerAlreadyClear {
        return resolve_clear_composer_discard(inner, service, target).await;
    }
    if matches!(first.path.as_ref(), Some(ResolutionPath::TerminalKey(route)) if resolution == NotificationResolution::Discard && route.manifest.injection.clear_keys.is_empty())
    {
        service.cancel_attention_resolution(attempt_id)?;
        return Err(AttentionActionError::DiscardUnsupported);
    }

    // Rebuild before recording the operator action.
    let second = assess(inner, service, target, false).await;
    if resolution_path(&second, resolution) != Some(path_kind) {
        service.cancel_attention_resolution(attempt_id)?;
        return Err(AttentionActionError::Evidence(Box::new(second.result)));
    }
    if matches!(second.path.as_ref(), Some(ResolutionPath::TerminalKey(route)) if action_keys(&route.manifest, resolution).is_none())
    {
        service.cancel_attention_resolution(attempt_id)?;
        return Err(AttentionActionError::DiscardUnsupported);
    }

    if let Err(error) = service.record_attention_resolution_intent(target, resolution) {
        service.cancel_attention_resolution(attempt_id)?;
        return Err(AttentionActionError::Store(error));
    }
    delivery::inject_pause(inner, "attention_after_intent").await;

    // The journal append takes time. Rebuild the proof before the terminal
    // key rather than trusting an earlier capture.
    let final_assessment = assess(inner, service, target, false).await;
    if resolution_path(&final_assessment, resolution) != Some(path_kind) {
        withdraw_pre_key(service, target, resolution)?;
        return Err(AttentionActionError::Evidence(Box::new(
            final_assessment.result,
        )));
    }
    let route = match final_assessment.path {
        Some(ResolutionPath::TerminalKey(route)) => {
            let Some(keys) = action_keys(&route.manifest, resolution) else {
                withdraw_pre_key(service, target, resolution)?;
                return Err(AttentionActionError::DiscardUnsupported);
            };
            let mut evidence_events = inner.events.subscribe();
            let dispatch_started_ms = unix_ms();
            let consumption_registration = if resolution == NotificationResolution::Complete {
                let Some(expected_payload) = expected_notification(service, target) else {
                    withdraw_pre_key(service, target, resolution)?;
                    return Err(AttentionActionError::Uncertain);
                };
                let signal = match service.register_attention_consumption_candidate(
                    target,
                    route.session_idx,
                    route.row.pane_id.clone(),
                    expected_payload,
                    dispatch_started_ms,
                ) {
                    Ok(signal) => signal,
                    Err(error) => {
                        withdraw_pre_key(service, target, resolution)?;
                        return Err(AttentionActionError::Store(error));
                    }
                };
                signal.map(|signal| {
                    ConsumptionRegistration::new(Arc::clone(service), attempt_id, signal)
                })
            } else {
                None
            };
            if route
                .watcher
                .client()
                .send_keys(&route.row.pane_id, &keys)
                .await
                .is_err()
            {
                let _ = service.cancel_attention_resolution(attempt_id);
                // The durable intent remains ambiguous. No automatic retry may
                // press a second key sequence.
                return Err(AttentionActionError::Uncertain);
            }
            delivery::inject_pause(inner, "attention_after_key_before_accepted").await;
            if let Err(error) =
                service.record_attention_resolution_action_accepted(target, resolution)
            {
                tracing::error!(
                    attempt_id = %attempt_id,
                    %error,
                    "terminal action was accepted but its durable boundary failed"
                );
                let _ = service.cancel_attention_resolution(attempt_id);
                return Err(AttentionActionError::Uncertain);
            }
            delivery::inject_pause(inner, "attention_after_action_accepted").await;
            let consumption = (resolution == NotificationResolution::Complete).then_some(
                ConsumptionRequirement {
                    binding: target
                        .record
                        .binding
                        .clone()
                        .expect("terminal action requires a durable binding"),
                    signal: consumption_registration
                        .as_ref()
                        .map(ConsumptionRegistration::signal),
                },
            );
            let Some(confirmed) = observe_post_action_clear(
                inner,
                service,
                target,
                &mut evidence_events,
                consumption.as_ref(),
            )
            .await
            else {
                let _ = service.cancel_attention_resolution(attempt_id);
                // The durable intent remains ambiguous. The exact barrier and
                // open attention item stay recoverable, and no second key may
                // be sent for this intent.
                return Err(AttentionActionError::Uncertain);
            };
            confirmed
        }
        Some(ResolutionPath::ComposerAlreadyClear(_)) => {
            unreachable!("no-key discard returned before terminal intent")
        }
        None => unreachable!("resolution path was checked above"),
    };

    settle_resolution(inner, service, target, resolution, route).await
}

/// Settle Discard without a terminal key after two current exact-empty proofs.
///
/// No intent is appended on this path. The resolution fact is the first and
/// only durable action boundary, so replay observes either no action or the
/// completed Discard. A matching legacy intent-only Discard may use the same
/// path, but it still cannot send a key.
async fn resolve_clear_composer_discard(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    target: &AttentionTarget,
) -> Result<AttentionResolveResult, AttentionActionError> {
    let attempt_id = target.record.attempt_id;
    let first = assess(inner, service, target, false).await;
    if resolution_path(&first, NotificationResolution::Discard)
        != Some(ResolutionPathKind::ComposerAlreadyClear)
    {
        service.cancel_attention_resolution(attempt_id)?;
        return Err(AttentionActionError::Evidence(Box::new(first.result)));
    }
    delivery::inject_pause(inner, "attention_before_no_key_resolution").await;
    let second = assess(inner, service, target, false).await;
    let Some(ResolutionPath::ComposerAlreadyClear(route)) = second.path else {
        service.cancel_attention_resolution(attempt_id)?;
        return Err(AttentionActionError::Evidence(Box::new(second.result)));
    };
    if !second.result.checks.terminal_action_safe {
        service.cancel_attention_resolution(attempt_id)?;
        return Err(AttentionActionError::Evidence(Box::new(second.result)));
    }
    if let Err(error) = service.resolve_attention_without_terminal_action(target) {
        tracing::error!(
            attempt_id = %attempt_id,
            %error,
            "no-key Discard was proven but its atomic resolution fact failed"
        );
        let _ = service.cancel_attention_resolution(attempt_id);
        return Err(AttentionActionError::Uncertain);
    }
    delivery::inject_pause(inner, "attention_after_no_key_resolution").await;
    resolve_staged_hold(inner, target, &route);
    Ok(AttentionResolveResult {
        attempt_id,
        resolution: NotificationResolution::Discard,
    })
}

/// Reconcile one matching durable intent without sending another terminal key.
///
/// A prior key and its required consumption proof were durably recorded, but
/// final composer proof was lost. The matching durable chain authorizes no
/// second key. Fresh exact binding and a positively empty composer settle that
/// same operator action; every other observation leaves the intent open.
async fn reconcile_existing_intent(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    target: &AttentionTarget,
    resolution: NotificationResolution,
    consumption_required: bool,
) -> Result<AttentionResolveResult, AttentionActionError> {
    let attempt_id = target.record.attempt_id;
    let mut evidence_events = inner.events.subscribe();
    let consumption = consumption_required.then(|| ConsumptionRequirement {
        binding: target
            .record
            .binding
            .clone()
            .expect("accepted Complete requires a durable binding"),
        signal: None,
    });
    let Some(route) = observe_post_action_clear(
        inner,
        service,
        target,
        &mut evidence_events,
        consumption.as_ref(),
    )
    .await
    else {
        let _ = service.cancel_attention_resolution(attempt_id);
        return Err(AttentionActionError::Uncertain);
    };
    settle_resolution(inner, service, target, resolution, route).await
}

async fn settle_resolution(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    target: &AttentionTarget,
    resolution: NotificationResolution,
    route: ActionRoute,
) -> Result<AttentionResolveResult, AttentionActionError> {
    let attempt_id = target.record.attempt_id;
    if let Err(error) = service.resolve_attention(target, resolution) {
        tracing::error!(
            attempt_id = %attempt_id,
            %error,
            "terminal action landed but its resolution fact failed"
        );
        let _ = service.cancel_attention_resolution(attempt_id);
        return Err(AttentionActionError::Uncertain);
    }
    delivery::inject_pause(inner, "attention_after_resolution").await;
    resolve_staged_hold(inner, target, &route);
    Ok(AttentionResolveResult {
        attempt_id,
        resolution,
    })
}

fn resolve_staged_hold(inner: &Arc<Inner>, target: &AttentionTarget, route: &ActionRoute) {
    if let Some(binding) = target.record.binding.as_ref() {
        fusion::resolve_staged_hold(
            inner,
            route.session_idx,
            &route.row.pane_id,
            &target.record.attempt_id.to_string(),
            binding.agent,
            binding.manifest.as_str(),
        );
    }
}

/// Require positive post-action composer evidence before durable settlement.
///
/// The terminal client accepting a key sequence proves only that it accepted
/// bytes. A bounded sequence of fresh captures must prove the same binding and
/// a manifest-owned visible empty composer. Fresh Complete also requires a
/// exact authenticated prompt receipt or a recipient claim ordered after this
/// action. Runtime Working events only wake re-evaluation and never prove
/// consumption. The durable intent, accepted-action fact, Complete consumption
/// fact, and binding keep crash reconciliation scoped to the same operator
/// action. Failure leaves the intent unresolved and never authorizes another
/// terminal key.
async fn observe_post_action_clear(
    inner: &Arc<Inner>,
    service: &MailboxService,
    target: &AttentionTarget,
    evidence_events: &mut tokio::sync::broadcast::Receiver<Event>,
    consumption: Option<&ConsumptionRequirement>,
) -> Option<ActionRoute> {
    let started = tokio::time::Instant::now();
    let mut checkpoint = 1;
    let mut event_scans = 0;
    let mut assess_now = true;
    let mut consumption_observed = consumption.is_none();
    let pane_id = target.record.recipient.pane_id()?.to_string();
    loop {
        if !consumption_observed {
            let observation = consumption
                .and_then(|required| required.signal_observation())
                .or_else(|| service.attention_claim_consumption(target).ok().flatten());
            if let Some(observation) = observation {
                if consumption.is_some_and(|required| {
                    required.current_binding_matches(inner, service, target)
                }) {
                    if let Err(error) = service
                        .record_attention_resolution_consumption_observed(target, observation)
                    {
                        tracing::error!(
                            attempt_id = %target.record.attempt_id,
                            %error,
                            "exact notification consumption was observed but its durable boundary failed"
                        );
                        return None;
                    }
                    consumption_observed = true;
                    delivery::inject_pause(inner, "attention_after_consumption_observed").await;
                }
            }
        }
        if assess_now {
            let assessment = assess(inner, service, target, false).await;
            if consumption_observed {
                if let Some(ResolutionPath::ComposerAlreadyClear(route)) = assessment.path {
                    return Some(route);
                }
            }
        }
        let delay = POST_ACTION_PROOF_DELAYS_MS.get(checkpoint).copied()?;
        let deadline = started + Duration::from_millis(delay);
        if event_scans >= POST_ACTION_EVENT_SCANS_PER_CHECKPOINT {
            tokio::time::sleep_until(deadline).await;
            checkpoint += 1;
            event_scans = 0;
            assess_now = true;
            continue;
        }
        match wait_for_attention_evidence_event(evidence_events, &pane_id, deadline).await {
            EvidenceWait::Deadline => {
                checkpoint += 1;
                event_scans = 0;
                assess_now = true;
            }
            EvidenceWait::Relevant => {
                event_scans += 1;
                assess_now = true;
            }
            EvidenceWait::Irrelevant => {
                event_scans += 1;
                assess_now = false;
            }
            EvidenceWait::Closed => {
                tokio::time::sleep_until(deadline).await;
                checkpoint += 1;
                event_scans = 0;
                assess_now = true;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EvidenceWait {
    Deadline,
    Relevant,
    Irrelevant,
    Closed,
}

async fn wait_for_attention_evidence_event(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    pane_id: &str,
    deadline: tokio::time::Instant,
) -> EvidenceWait {
    tokio::select! {
        _ = tokio::time::sleep_until(deadline) => EvidenceWait::Deadline,
        event = events.recv() => match event {
            Ok(event)
                if matches!(event.event.as_str(), "session" | "messages.changed")
                    || (matches!(event.event.as_str(), "state" | "readiness")
                        && event.data["pane_id"] == pane_id) =>
            {
                EvidenceWait::Relevant
            }
            Ok(_) => EvidenceWait::Irrelevant,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                EvidenceWait::Relevant
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => EvidenceWait::Closed,
        }
    }
}

/// Exact post-dispatch evidence that a Complete action consumed this message.
struct ConsumptionRequirement {
    binding: NotificationBinding,
    signal: Option<Arc<AttentionConsumptionSignal>>,
}

impl ConsumptionRequirement {
    fn signal_observation(&self) -> Option<NotificationResolutionConsumptionObservation> {
        self.signal.as_ref().and_then(|signal| signal.observation())
    }

    fn current_binding_matches(
        &self,
        inner: &Arc<Inner>,
        service: &MailboxService,
        target: &AttentionTarget,
    ) -> bool {
        let Some(route) =
            crate::messaging::notification_route(inner, service, target.record.recipient)
                .ok()
                .flatten()
        else {
            return false;
        };
        let row = route.row;
        if row.dead || row.in_mode {
            return false;
        }
        let current = fusion::admitted_binding(inner, route.session_idx, &row);
        binding_checks(current.as_ref(), &self.binding) == (true, true)
    }
}

struct ConsumptionRegistration {
    service: Arc<MailboxService>,
    attempt_id: cyclops_proto::NotificationAttemptId,
    signal: Arc<AttentionConsumptionSignal>,
}

impl ConsumptionRegistration {
    fn new(
        service: Arc<MailboxService>,
        attempt_id: cyclops_proto::NotificationAttemptId,
        signal: Arc<AttentionConsumptionSignal>,
    ) -> Self {
        Self {
            service,
            attempt_id,
            signal,
        }
    }

    fn signal(&self) -> Arc<AttentionConsumptionSignal> {
        Arc::clone(&self.signal)
    }
}

impl Drop for ConsumptionRegistration {
    fn drop(&mut self) {
        self.service
            .unregister_attention_consumption_candidate(self.attempt_id);
    }
}

fn resolution_path(
    assessment: &Assessment,
    resolution: NotificationResolution,
) -> Option<ResolutionPathKind> {
    match assessment.path.as_ref()? {
        ResolutionPath::TerminalKey(_) => Some(ResolutionPathKind::TerminalKey),
        ResolutionPath::ComposerAlreadyClear(_)
            if resolution == NotificationResolution::Discard
                && assessment.result.checks.terminal_action_safe =>
        {
            Some(ResolutionPathKind::ComposerAlreadyClear)
        }
        ResolutionPath::ComposerAlreadyClear(_) => None,
    }
}

fn withdraw_pre_key(
    service: &MailboxService,
    target: &AttentionTarget,
    resolution: NotificationResolution,
) -> Result<(), AttentionActionError> {
    if let Err(error) = service.withdraw_attention_resolution_intent(target, resolution) {
        tracing::error!(
            attempt_id = %target.record.attempt_id,
            %error,
            "pre-key refusal could not withdraw its durable action intent"
        );
        let _ = service.cancel_attention_resolution(target.record.attempt_id);
        return Err(AttentionActionError::Uncertain);
    }
    service.cancel_attention_resolution(target.record.attempt_id)?;
    Ok(())
}

async fn assess(
    inner: &Arc<Inner>,
    service: &MailboxService,
    target: &AttentionTarget,
    include_diff: bool,
) -> Assessment {
    let expected = expected_notification(service, target);
    let mut checks = AttentionChecks {
        notification_exact: false,
        trailer_anchored: false,
        process_matches: false,
        manifest_matches: false,
        terminal_action_safe: false,
    };
    let mut observed = None;
    let mut action_path = None;

    let Some(binding) = target.record.binding.as_ref() else {
        return assessment_result(
            target,
            checks,
            expected,
            observed,
            include_diff,
            action_path,
        );
    };
    let Some(route) = crate::messaging::notification_route(inner, service, target.record.recipient)
        .ok()
        .flatten()
    else {
        return assessment_result(
            target,
            checks,
            expected,
            observed,
            include_diff,
            action_path,
        );
    };
    let session_idx = route.session_idx;
    let watcher = route.watcher;
    let row = route.row;
    let Some(manifest) = inner.manifests.get(binding.manifest.as_str()).cloned() else {
        return assessment_result(
            target,
            checks,
            expected,
            observed,
            include_diff,
            action_path,
        );
    };

    let before = fusion::admitted_binding(inner, session_idx, &row);
    (checks.process_matches, checks.manifest_matches) = binding_checks(before.as_ref(), binding);
    if !checks.process_matches || !checks.manifest_matches || row.dead || row.in_mode {
        return assessment_result(
            target,
            checks,
            expected,
            observed,
            include_diff,
            action_path,
        );
    }

    let Ok(capture) = watcher
        .client()
        .capture_pane_joined_escaped(&row.pane_id)
        .await
    else {
        return assessment_result(
            target,
            checks,
            expected,
            observed,
            include_diff,
            action_path,
        );
    };
    let Some(now) = watcher.pane(&row.pane_id) else {
        return assessment_result(
            target,
            checks,
            expected,
            observed,
            include_diff,
            action_path,
        );
    };
    let after = fusion::admitted_binding(inner, session_idx, &now);
    (checks.process_matches, checks.manifest_matches) = binding_checks(after.as_ref(), binding);
    if !checks.process_matches || !checks.manifest_matches {
        return assessment_result(
            target,
            checks,
            expected,
            observed,
            include_diff,
            action_path,
        );
    }

    let plain = strip_csi(&capture);
    let fresh_state = manifest
        .evaluate_esc(&now.title, &plain, Some(&capture))
        .map(|rule| rule.state);
    let safe_staged_composer = staged_composer_state_is_safe(fresh_state);
    checks.terminal_action_safe = !now.dead
        && !now.in_mode
        && safe_staged_composer
        && fusion::staged_action_ready(
            inner,
            session_idx,
            &now.pane_id,
            &target.record.attempt_id.to_string(),
            binding.agent,
            binding.manifest.as_str(),
        );

    // Composer extraction proves the terminal layout independently from
    // equality. An operator asking for a diff needs the actual mismatch,
    // while a terminal action still requires exact normalized content.
    let content_proof = delivery::exact_composer_content_from_joined_capture(&manifest, &capture);
    if let delivery::ComposerContentProof::Visible(content) = &content_proof {
        checks.trailer_anchored = true;
        checks.notification_exact = expected.as_deref() == Some(content.as_str());
        if include_diff {
            observed = Some(content.clone());
        }
    }
    let composer_already_clear = composer_already_clear_is_safe(
        &checks,
        &manifest,
        &capture,
        !now.dead && !now.in_mode,
        fresh_state,
    );
    let route = ActionRoute {
        session_idx,
        watcher,
        row: now,
        manifest,
    };
    if checks.all_pass() {
        action_path = Some(ResolutionPath::TerminalKey(route));
    } else if composer_already_clear {
        action_path = Some(ResolutionPath::ComposerAlreadyClear(route));
    }
    assessment_result(
        target,
        checks,
        expected,
        observed,
        include_diff,
        action_path,
    )
}

fn assessment_result(
    target: &AttentionTarget,
    checks: AttentionChecks,
    expected: Option<String>,
    observed: Option<String>,
    include_diff: bool,
    path: Option<ResolutionPath>,
) -> Assessment {
    Assessment {
        result: AttentionShowResult {
            attempt_id: target.record.attempt_id,
            message_id: target.record.message_id.clone(),
            recipient: target.record.recipient,
            checks,
            expected: include_diff.then_some(expected).flatten(),
            observed: include_diff.then_some(observed).flatten(),
        },
        path,
    }
}

fn composer_already_clear_is_safe(
    checks: &AttentionChecks,
    manifest: &Manifest,
    capture: &str,
    pane_action_safe: bool,
    observed_state: Option<AgentState>,
) -> bool {
    // After a terminal key lands, the lifecycle may release the staged hold
    // before this read. Settlement therefore proves the current binding and
    // visible empty composer directly. The pre-key path separately requires
    // terminal_action_safe before it can treat this as a no-key discard.
    checks.process_matches
        && checks.manifest_matches
        && pane_action_safe
        && matches!(observed_state, Some(AgentState::Idle | AgentState::Working))
        && delivery::visible_clean_composer_proof(manifest, capture)
}

fn process_matches(current: crate::identity::ProcId, expected: ProcessInstanceId) -> bool {
    current.pid == expected.pid() && current.birth == expected.birth()
}

fn staged_composer_state_is_safe(state: Option<AgentState>) -> bool {
    matches!(state, Some(AgentState::Idle | AgentState::IdleWithInput))
}

fn binding_checks(
    current: Option<&fusion::Binding>,
    expected: &NotificationBinding,
) -> (bool, bool) {
    (
        current.is_some_and(|binding| {
            expected.pane_root.is_some_and(|pane_root| {
                process_matches(binding.pane_root, pane_root)
                    && expected.leader.is_some_and(|leader| {
                        process_matches(binding.leader, leader)
                            && process_matches(binding.agent, expected.agent)
                    })
            })
        }),
        current.is_some_and(|binding| binding.manifest == expected.manifest.as_str()),
    )
}

fn expected_notification(service: &MailboxService, target: &AttentionTarget) -> Option<String> {
    let message = service.message_line(&target.record.message_id).ok()?;
    expected_notification_from_message(target, &message)
}

fn expected_notification_from_message(
    target: &AttentionTarget,
    message: &cyclops_proto::LedgerLine,
) -> Option<String> {
    delivery::expected_notification_payload(&target.record, message)
}

fn submit_key(manifest: &Manifest) -> &str {
    if manifest.injection.submit.is_empty() {
        "Enter"
    } else {
        manifest.injection.submit.as_str()
    }
}

fn action_keys(manifest: &Manifest, resolution: NotificationResolution) -> Option<Vec<&str>> {
    match resolution {
        NotificationResolution::Complete => Some(vec![submit_key(manifest)]),
        NotificationResolution::Discard => (!manifest.injection.clear_keys.is_empty()).then(|| {
            manifest
                .injection
                .clear_keys
                .iter()
                .map(String::as_str)
                .collect()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
[agent]
id = "test"
display_name = "test"

[[rule]]
id = "composer_clean"
state = "idle"
composer_semantic = "clean"
priority = 200
region = "bottom_non_empty_lines(8)"
line_regex = ['^>$']
line_regex_esc = ['^>$']

[[rule]]
id = "composer"
state = "idle_with_input"
priority = 100
region = "bottom_non_empty_lines(8)"
line_regex = ['^> .+']

[injection]
submit = "Enter"
clear_keys = ["C-c"]
composer_trailer_regex = ['^TRAILER$']
composer_trailer_regex_esc = ['TRAILER']
composer_trailer_required_prefix = 1
composer_prompt_regex = '^> ?(?P<content>.*)$'
composer_continuation_regex = '^  (?P<content>.*)$'
"#;

    fn manifest() -> Manifest {
        Manifest::parse(MANIFEST, std::path::Path::new("test.toml")).unwrap()
    }

    #[tokio::test]
    async fn evidence_wait_consumes_one_event_and_keeps_the_later_exact_cue() {
        let (events, mut receiver) = tokio::sync::broadcast::channel(16);
        events
            .send(Event {
                event: "other".into(),
                data: serde_json::json!({"pane_id": "%other"}),
                seq: Some(1),
            })
            .unwrap();
        events
            .send(Event {
                event: "readiness".into(),
                data: serde_json::json!({"pane_id": "%1"}),
                seq: None,
            })
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);

        assert_eq!(
            wait_for_attention_evidence_event(&mut receiver, "%1", deadline).await,
            EvidenceWait::Irrelevant
        );
        assert_eq!(
            wait_for_attention_evidence_event(&mut receiver, "%1", deadline).await,
            EvidenceWait::Relevant
        );
    }

    #[tokio::test]
    async fn evidence_wait_keeps_the_checkpoint_deadline_live() {
        let (_events, mut receiver) = tokio::sync::broadcast::channel(1);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(5);
        assert_eq!(
            wait_for_attention_evidence_event(&mut receiver, "%1", deadline).await,
            EvidenceWait::Deadline
        );
    }

    #[tokio::test]
    async fn generic_working_is_only_a_recheck_cue() {
        let (events, mut receiver) = tokio::sync::broadcast::channel(4);
        events
            .send(Event {
                event: "state".into(),
                data: serde_json::json!({
                    "pane_id": "%1",
                    "state": "working",
                    "working_confirmed": true,
                }),
                seq: Some(1),
            })
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        assert_eq!(
            wait_for_attention_evidence_event(&mut receiver, "%1", deadline).await,
            EvidenceWait::Relevant
        );
    }

    #[test]
    fn process_generation_must_match() {
        let current = crate::identity::ProcId { pid: 41, birth: 90 };
        assert!(process_matches(
            current,
            ProcessInstanceId::new(41, 90).unwrap()
        ));
        assert!(!process_matches(
            current,
            ProcessInstanceId::new(41, 91).unwrap()
        ));
        assert!(!process_matches(
            current,
            ProcessInstanceId::new(42, 90).unwrap()
        ));
    }

    #[test]
    fn process_and_manifest_replacement_fail_independently() {
        let expected = NotificationBinding {
            recipient: "00000000-0000-0000-0000-000000000001"
                .parse::<cyclops_proto::WorkspaceId>()
                .map(cyclops_proto::RecipientKey::admin)
                .unwrap(),
            pane_root: Some(ProcessInstanceId::new(39, 88).unwrap()),
            leader: Some(ProcessInstanceId::new(40, 89).unwrap()),
            agent: ProcessInstanceId::new(41, 90).unwrap(),
            manifest: cyclops_proto::NotificationManifestId::new("test").unwrap(),
        };
        let exact = fusion::Binding {
            pane_root: crate::identity::ProcId { pid: 39, birth: 88 },
            leader: crate::identity::ProcId { pid: 40, birth: 89 },
            agent: crate::identity::ProcId { pid: 41, birth: 90 },
            manifest: "test".into(),
        };
        assert_eq!(binding_checks(Some(&exact), &expected), (true, true));

        let replaced_pane_root = fusion::Binding {
            pane_root: crate::identity::ProcId { pid: 39, birth: 89 },
            ..exact.clone()
        };
        assert_eq!(
            binding_checks(Some(&replaced_pane_root), &expected),
            (false, true)
        );

        let replaced_process = fusion::Binding {
            agent: crate::identity::ProcId { pid: 41, birth: 91 },
            ..exact.clone()
        };
        assert_eq!(
            binding_checks(Some(&replaced_process), &expected),
            (false, true)
        );
        let replaced_leader = fusion::Binding {
            leader: crate::identity::ProcId { pid: 40, birth: 90 },
            ..exact.clone()
        };
        assert_eq!(
            binding_checks(Some(&replaced_leader), &expected),
            (false, true)
        );
        let replaced_manifest = fusion::Binding {
            manifest: "other".into(),
            ..exact.clone()
        };
        assert_eq!(
            binding_checks(Some(&replaced_manifest), &expected),
            (true, false)
        );

        let legacy = NotificationBinding {
            pane_root: None,
            leader: None,
            ..expected
        };
        assert_eq!(binding_checks(Some(&exact), &legacy), (false, true));
    }

    #[test]
    fn only_positive_composer_states_can_authorize_a_terminal_key() {
        for state in [AgentState::Idle, AgentState::IdleWithInput] {
            assert!(staged_composer_state_is_safe(Some(state)));
        }
        for state in [
            AgentState::Working,
            AgentState::BlockedModal,
            AgentState::BlockedPermission,
            AgentState::BlockedQuota,
            AgentState::Unknown,
        ] {
            assert!(!staged_composer_state_is_safe(Some(state)));
        }
        assert!(!staged_composer_state_is_safe(None));
    }

    #[test]
    fn clear_composer_discard_requires_positive_visible_empty_proof() {
        let checks = AttentionChecks {
            notification_exact: false,
            trailer_anchored: false,
            process_matches: true,
            manifest_matches: true,
            terminal_action_safe: true,
        };
        let manifest = manifest();
        let clean = ">\nTRAILER";
        assert!(composer_already_clear_is_safe(
            &checks,
            &manifest,
            clean,
            true,
            Some(AgentState::Idle),
        ));
        assert!(composer_already_clear_is_safe(
            &checks,
            &manifest,
            clean,
            true,
            Some(AgentState::Working),
        ));
        assert!(!composer_already_clear_is_safe(
            &checks,
            &manifest,
            clean,
            true,
            Some(AgentState::BlockedModal),
        ));
        assert!(!composer_already_clear_is_safe(
            &checks,
            &manifest,
            clean,
            false,
            Some(AgentState::Idle),
        ));
        let staged = "> cyclops inbox claim m-one\nTRAILER";
        assert_eq!(
            delivery::exact_composer_content_from_joined_capture(&manifest, staged),
            delivery::ComposerContentProof::Visible("cyclops inbox claim m-one".into())
        );
        assert!(!composer_already_clear_is_safe(
            &checks,
            &manifest,
            staged,
            true,
            Some(AgentState::Idle),
        ));
        assert!(!composer_already_clear_is_safe(
            &checks,
            &manifest,
            ">",
            true,
            Some(AgentState::Idle),
        ));
        assert!(!composer_already_clear_is_safe(
            &checks,
            &manifest,
            "> human draft\nTRAILER",
            true,
            Some(AgentState::Idle),
        ));

        let wrong_process = AttentionChecks {
            process_matches: false,
            ..checks
        };
        assert!(!composer_already_clear_is_safe(
            &wrong_process,
            &manifest,
            clean,
            true,
            Some(AgentState::Idle),
        ));

        let unsupported = Manifest::parse(
            MANIFEST
                .replace(
                    "composer_prompt_regex = '^> ?(?P<content>.*)$'\ncomposer_continuation_regex = '^  (?P<content>.*)$'\n",
                    "",
                )
                .replace("clear_keys = [\"C-c\"]\n", "")
                .as_str(),
            std::path::Path::new("unsupported-clean.toml"),
        )
        .unwrap();
        assert!(!composer_already_clear_is_safe(
            &checks,
            &unsupported,
            clean,
            true,
            Some(AgentState::Idle),
        ));

        let hidden = Manifest::parse(
            MANIFEST
                .replace(
                    "composer_continuation_regex = '^  (?P<content>.*)$'",
                    "composer_continuation_regex = '^  (?P<content>.*)$'\ncomposer_chip_regex = ['^> \\[Pasted text #\\d+\\]$']\ncomposer_chip_regex_esc = ['^> \\[Pasted text #\\d+\\]$']",
                )
                .as_str(),
            std::path::Path::new("hidden-clean.toml"),
        )
        .unwrap();
        let chip = "> [Pasted text #1]\nTRAILER";
        assert_eq!(
            delivery::exact_composer_content_from_joined_capture(&hidden, chip),
            delivery::ComposerContentProof::Hidden
        );
        assert!(!composer_already_clear_is_safe(
            &checks,
            &hidden,
            chip,
            true,
            Some(AgentState::Idle),
        ));
    }

    #[test]
    fn discard_requires_a_manifest_owned_clear_sequence() {
        let measured = manifest();
        assert_eq!(
            action_keys(&measured, NotificationResolution::Discard),
            Some(vec!["C-c"])
        );

        let unsupported = Manifest::parse(
            MANIFEST.replace("clear_keys = [\"C-c\"]\n", "").as_str(),
            std::path::Path::new("unsupported.toml"),
        )
        .unwrap();
        assert_eq!(
            action_keys(&unsupported, NotificationResolution::Discard),
            None
        );
        assert_eq!(
            action_keys(&unsupported, NotificationResolution::Complete),
            Some(vec!["Enter"])
        );
    }

    #[test]
    fn exact_extraction_refuses_duplicate_marker_and_trailing_input() {
        let manifest = manifest();
        let exact = "> [cyclops m-one] FROM: admin  SUBJECT: Test\n  body\n  [cyclops:end m-one]\n\u{1b}[2mTRAILER\u{1b}[0m";
        assert!(matches!(
            delivery::composer_content_from_joined_capture(&manifest, exact, "m-one"),
            delivery::ComposerContentProof::Visible(_)
        ));

        let duplicate = "> [cyclops m-one] FROM: admin  SUBJECT: Test\n  [cyclops:end m-one]\n  [cyclops:end m-one]\n\u{1b}[2mTRAILER\u{1b}[0m";
        assert_eq!(
            delivery::composer_content_from_joined_capture(&manifest, duplicate, "m-one"),
            delivery::ComposerContentProof::Unprovable
        );

        let trailing = "> [cyclops m-one] FROM: admin  SUBJECT: Test\n  [cyclops:end m-one]\n  human text\n\u{1b}[2mTRAILER\u{1b}[0m";
        assert_eq!(
            delivery::composer_content_from_joined_capture(&manifest, trailing, "m-one"),
            delivery::ComposerContentProof::Unprovable
        );
    }

    #[test]
    fn attention_assessment_uses_canonical_format_rejection() {
        let message_id = cyclops_proto::MessageId::new("m-format").unwrap();
        let workspace = "00000000-0000-0000-0000-000000000001"
            .parse::<cyclops_proto::WorkspaceId>()
            .unwrap();
        let recipient = cyclops_proto::RecipientKey::admin(workspace);
        let message = cyclops_proto::LedgerLine {
            seq: 1,
            boot_id: "boot".into(),
            id: message_id.to_string(),
            ts: 1,
            kind: cyclops_proto::Kind::Msg,
            from: recipient.to_string(),
            to: vec![recipient.to_string()],
            subject: Some("Format".into()),
            body: Some("Body".into()),
            reply_to: None,
            deliveries: Vec::new(),
            data: None,
        };
        let mut target = AttentionTarget {
            record: cyclops_proto::NotificationRecord {
                attempt_id: cyclops_proto::NotificationAttemptId::parse(
                    "att-00000000-0000-4000-8000-000000000001",
                )
                .unwrap(),
                message_id: message_id.clone(),
                recipient,
                state: cyclops_proto::NotificationState::AttentionRequired,
                binding: None,
                transport: cyclops_proto::NotificationTransport::Doorbell,
                doorbell_format: None,
                cause: None,
                pre_write_cause: None,
                pre_write_observation: None,
                pre_write_reopen_count: 0,
                started_seq: 1,
                updated_seq: 1,
                updated_at: 1,
            },
        };
        assert_eq!(
            expected_notification_from_message(&target, &message),
            Some(cyclops_proto::render_legacy_doorbell(&message_id))
        );
        target.record.doorbell_format = Some(cyclops_proto::DOORBELL_FORMAT_COMPACT_CLAIM);
        assert_eq!(
            expected_notification_from_message(&target, &message),
            Some(cyclops_proto::render_doorbell_v1(&message_id))
        );
        target.record.doorbell_format = Some(cyclops_proto::DOORBELL_FORMAT_ATTEMPT_CLAIM);
        assert_eq!(
            expected_notification_from_message(&target, &message),
            Some(cyclops_proto::render_doorbell_v2(
                &message_id,
                target.record.attempt_id
            ))
        );
        target.record.doorbell_format = Some(999);
        assert_eq!(expected_notification_from_message(&target, &message), None);
        let checks = AttentionChecks {
            notification_exact: expected_notification_from_message(&target, &message).is_some(),
            trailer_anchored: true,
            process_matches: true,
            manifest_matches: true,
            terminal_action_safe: true,
        };
        assert!(!checks.all_pass());
    }
}
