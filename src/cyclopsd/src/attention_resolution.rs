//! Explicit recovery for a notification left in an agent composer.

use std::sync::Arc;

use cyclops_manifest::{strip_csi, Manifest};
use cyclops_proto::{
    AgentState, AttentionChecks, AttentionResolveResult, AttentionShowResult, NotificationBinding,
    NotificationResolution, NotificationTransport, ProcessInstanceId,
};
use cyclops_tmux::{PaneRow, SessionWatcher};

use crate::mailbox::{AttentionTarget, MailboxService, MailboxServiceError};
use crate::{delivery, fusion, Inner};

#[derive(Debug, thiserror::Error)]
pub(crate) enum AttentionActionError {
    #[error(transparent)]
    Store(#[from] MailboxServiceError),
    #[error("the current terminal does not satisfy every attention evidence check")]
    Evidence(Box<AttentionShowResult>),
    #[error("this manifest has no measured whole-composer clear sequence")]
    DiscardUnsupported,
    #[error("the terminal action outcome is uncertain")]
    Uncertain,
}

struct ActionRoute {
    session_idx: usize,
    watcher: Arc<SessionWatcher>,
    row: PaneRow,
    manifest: Manifest,
}

struct Assessment {
    result: AttentionShowResult,
    route: Option<ActionRoute>,
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
    service.begin_attention_resolution(target)?;
    let attempt_id = target.record.attempt_id;

    let first = assess(inner, service, target, false).await;
    if !first.result.checks.all_pass() {
        service.cancel_attention_resolution(attempt_id)?;
        return Err(AttentionActionError::Evidence(Box::new(first.result)));
    }
    if resolution == NotificationResolution::Discard
        && first
            .route
            .as_ref()
            .is_none_or(|route| route.manifest.injection.clear_keys.is_empty())
    {
        service.cancel_attention_resolution(attempt_id)?;
        return Err(AttentionActionError::DiscardUnsupported);
    }

    // Rebuild every proof immediately before the terminal write. This is
    // the same irreducible capture-to-key window as normal staged submit.
    let second = assess(inner, service, target, false).await;
    if !second.result.checks.all_pass() {
        service.cancel_attention_resolution(attempt_id)?;
        return Err(AttentionActionError::Evidence(Box::new(second.result)));
    }
    let Some(second_route) = second.route else {
        service.cancel_attention_resolution(attempt_id)?;
        return Err(AttentionActionError::Evidence(Box::new(second.result)));
    };
    if action_keys(&second_route.manifest, resolution).is_none() {
        service.cancel_attention_resolution(attempt_id)?;
        return Err(AttentionActionError::DiscardUnsupported);
    }

    if let Err(error) = service.record_attention_resolution_intent(target, resolution) {
        service.cancel_attention_resolution(attempt_id)?;
        return Err(AttentionActionError::Store(error));
    }
    delivery::inject_pause(inner, "attention_after_intent").await;

    // The journal append takes time. Rebuild the full proof after it and
    // immediately before the terminal key rather than trusting the pane
    // observation that authorized the intent.
    let final_assessment = assess(inner, service, target, false).await;
    if !final_assessment.result.checks.all_pass() {
        withdraw_pre_key(service, target, resolution)?;
        return Err(AttentionActionError::Evidence(Box::new(
            final_assessment.result,
        )));
    }
    let Some(route) = final_assessment.route else {
        withdraw_pre_key(service, target, resolution)?;
        return Err(AttentionActionError::Evidence(Box::new(
            final_assessment.result,
        )));
    };
    let Some(keys) = action_keys(&route.manifest, resolution) else {
        withdraw_pre_key(service, target, resolution)?;
        return Err(AttentionActionError::DiscardUnsupported);
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
    delivery::inject_pause(inner, "attention_after_key").await;

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
    Ok(AttentionResolveResult {
        attempt_id,
        resolution,
    })
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
    let mut action_route = None;

    let Some(binding) = target.record.binding.as_ref() else {
        return assessment_result(
            target,
            checks,
            expected,
            observed,
            include_diff,
            action_route,
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
            action_route,
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
            action_route,
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
            action_route,
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
            action_route,
        );
    };
    let Some(now) = watcher.pane(&row.pane_id) else {
        return assessment_result(
            target,
            checks,
            expected,
            observed,
            include_diff,
            action_route,
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
            action_route,
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
    if let delivery::ComposerContentProof::Visible(content) =
        delivery::exact_composer_content_from_joined_capture(&manifest, &capture)
    {
        checks.trailer_anchored = true;
        checks.notification_exact = expected.as_deref() == Some(content.as_str());
        if include_diff {
            observed = Some(content);
        }
    }
    if checks.all_pass() {
        action_route = Some(ActionRoute {
            session_idx,
            watcher,
            row: now,
            manifest,
        });
    }
    assessment_result(
        target,
        checks,
        expected,
        observed,
        include_diff,
        action_route,
    )
}

fn assessment_result(
    target: &AttentionTarget,
    checks: AttentionChecks,
    expected: Option<String>,
    observed: Option<String>,
    include_diff: bool,
    route: Option<ActionRoute>,
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
        route,
    }
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
            expected.leader.is_some_and(|leader| {
                process_matches(binding.leader, leader)
                    && process_matches(binding.agent, expected.agent)
            })
        }),
        current.is_some_and(|binding| binding.manifest == expected.manifest.as_str()),
    )
}

fn expected_notification(service: &MailboxService, target: &AttentionTarget) -> Option<String> {
    match (target.record.transport, target.record.doorbell_format) {
        (NotificationTransport::Doorbell, format) => {
            expected_doorbell(&target.record.message_id, format)
        }
        (NotificationTransport::DirectPayload, None) => service
            .message_line(&target.record.message_id)
            .ok()
            .map(|message| delivery::render_canonical_message_payload(&message)),
        (NotificationTransport::DirectPayload, Some(_)) => None,
    }
}

fn expected_doorbell(message_id: &cyclops_proto::MessageId, format: Option<u32>) -> Option<String> {
    match format {
        None => Some(cyclops_proto::render_legacy_doorbell(message_id)),
        Some(cyclops_proto::DOORBELL_FORMAT_COMPACT_CLAIM) => {
            Some(cyclops_proto::render_doorbell_v1(message_id))
        }
        Some(_) => None,
    }
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
composer_prompt_regex = '^> (?P<content>.*)$'
composer_continuation_regex = '^  (?P<content>.*)$'
"#;

    fn manifest() -> Manifest {
        Manifest::parse(MANIFEST, std::path::Path::new("test.toml")).unwrap()
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
            leader: Some(ProcessInstanceId::new(40, 89).unwrap()),
            agent: ProcessInstanceId::new(41, 90).unwrap(),
            manifest: cyclops_proto::NotificationManifestId::new("test").unwrap(),
        };
        let exact = fusion::Binding {
            leader: crate::identity::ProcId { pid: 40, birth: 89 },
            agent: crate::identity::ProcId { pid: 41, birth: 90 },
            manifest: "test".into(),
        };
        assert_eq!(binding_checks(Some(&exact), &expected), (true, true));

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
    fn doorbell_recovery_uses_only_the_recorded_byte_format() {
        let message_id = cyclops_proto::MessageId::new("m-format").unwrap();
        assert_eq!(
            expected_doorbell(&message_id, None),
            Some(cyclops_proto::render_legacy_doorbell(&message_id))
        );
        assert_eq!(
            expected_doorbell(
                &message_id,
                Some(cyclops_proto::DOORBELL_FORMAT_COMPACT_CLAIM)
            ),
            Some(cyclops_proto::render_doorbell_v1(&message_id))
        );
        assert_eq!(expected_doorbell(&message_id, Some(999)), None);
        let checks = AttentionChecks {
            notification_exact: expected_doorbell(&message_id, Some(999)).is_some(),
            trailer_anchored: true,
            process_matches: true,
            manifest_matches: true,
            terminal_action_safe: true,
        };
        assert!(!checks.all_pass());
    }
}
