use std::collections::HashSet;

use cyclops_proto::{
    LiveSessionKey, OsBootId, ProcessInstanceId, RecipientKey, SessionIdentityBinding,
    SessionInstanceId, TmuxPaneId, TmuxSessionId, WorkspaceId,
};

const WORKSPACE_A: &str = "550e8400-e29b-41d4-a716-446655440000";
const WORKSPACE_B: &str = "67e55044-10b1-426f-9247-bb680e5fe0c8";
const SESSION_A: &str = "123e4567-e89b-42d3-a456-426614174000";
const SESSION_B: &str = "a8098c1a-f86e-41da-bd1a-00c4e56789ab";

fn workspace(value: &str) -> WorkspaceId {
    value.parse().expect("valid workspace id")
}

fn session(value: &str) -> SessionInstanceId {
    value.parse().expect("valid session instance id")
}

fn os_boot(value: &str) -> OsBootId {
    value.parse().expect("valid boot id")
}

fn process(pid: i32, birth: u64) -> ProcessInstanceId {
    ProcessInstanceId::new(pid, birth).expect("valid process identity")
}

fn tmux_session(value: &str) -> TmuxSessionId {
    value.parse().expect("valid tmux session id")
}

fn pane(value: &str) -> TmuxPaneId {
    value.parse().expect("valid tmux pane id")
}

fn live_key(os_boot_id: &str, server: ProcessInstanceId, session_id: &str) -> LiveSessionKey {
    LiveSessionKey::new(
        workspace(WORKSPACE_A),
        os_boot(os_boot_id),
        server,
        tmux_session(session_id),
    )
}

#[test]
fn durable_ids_have_stable_validated_string_serde() {
    let workspace = workspace(WORKSPACE_A);
    let session = session(SESSION_A);

    assert_eq!(
        serde_json::to_string(&workspace).unwrap(),
        format!(r#""{WORKSPACE_A}""#)
    );
    assert_eq!(
        serde_json::to_string(&session).unwrap(),
        format!(r#""{SESSION_A}""#)
    );
    assert_eq!(
        serde_json::from_str::<WorkspaceId>(&format!(r#""{WORKSPACE_A}""#)).unwrap(),
        workspace
    );
    assert_eq!(
        serde_json::from_str::<SessionInstanceId>(&format!(r#""{SESSION_A}""#)).unwrap(),
        session
    );

    for value in [
        "",
        "not-a-uuid",
        "00000000-0000-0000-0000-000000000000",
        "550E8400-E29B-41D4-A716-446655440000",
        "550e8400e29b41d4a716446655440000",
    ] {
        let wire = serde_json::to_string(value).unwrap();
        assert!(
            serde_json::from_str::<WorkspaceId>(&wire).is_err(),
            "{value:?}"
        );
        assert!(
            serde_json::from_str::<SessionInstanceId>(&wire).is_err(),
            "{value:?}"
        );
    }
}

#[test]
fn observed_components_validate_at_construction_and_deserialization() {
    for value in ["", " boot", "boot ", "boot id", "boot\n"] {
        let wire = serde_json::to_string(value).unwrap();
        assert!(value.parse::<OsBootId>().is_err(), "{value:?}");
        assert!(
            serde_json::from_str::<OsBootId>(&wire).is_err(),
            "{value:?}"
        );
    }

    for value in ["", "$", "0", "$00", "$01", "$-1", "$+1", "$1x"] {
        let wire = serde_json::to_string(value).unwrap();
        assert!(value.parse::<TmuxSessionId>().is_err(), "{value:?}");
        assert!(
            serde_json::from_str::<TmuxSessionId>(&wire).is_err(),
            "{value:?}"
        );
    }

    for value in ["", "%", "0", "%00", "%01", "%-1", "%+1", "%1x"] {
        let wire = serde_json::to_string(value).unwrap();
        assert!(value.parse::<TmuxPaneId>().is_err(), "{value:?}");
        assert!(
            serde_json::from_str::<TmuxPaneId>(&wire).is_err(),
            "{value:?}"
        );
    }

    for (pid, birth) in [(0, 1), (-1, 1), (1, 0)] {
        assert!(ProcessInstanceId::new(pid, birth).is_err());
        let wire = format!(r#"{{"pid":{pid},"birth":{birth}}}"#);
        assert!(serde_json::from_str::<ProcessInstanceId>(&wire).is_err());
    }
}

#[test]
fn observed_component_wire_shapes_are_canonical() {
    let server = process(4242, 818_221);

    assert_eq!(
        serde_json::to_string(&os_boot("boot-42")).unwrap(),
        r#""boot-42""#
    );
    assert_eq!(
        serde_json::to_string(&server).unwrap(),
        r#"{"pid":4242,"birth":818221}"#
    );
    assert_eq!(
        serde_json::to_string(&tmux_session("$0")).unwrap(),
        r#""$0""#
    );
    assert_eq!(serde_json::to_string(&pane("%7")).unwrap(), r#""%7""#);
    assert_eq!(
        serde_json::from_str::<ProcessInstanceId>(r#"{"pid":4242,"birth":818221}"#).unwrap(),
        server
    );
}

#[test]
fn observed_key_is_independent_of_assigned_identity() {
    let key = live_key("boot-a", process(4242, 818_221), "$0");
    let first = SessionIdentityBinding::new(key.clone(), session(SESSION_A));
    let conflicting = SessionIdentityBinding::new(key.clone(), session(SESSION_B));

    assert_eq!(first.live_session_key(), conflicting.live_session_key());
    assert_ne!(first, conflicting);
}

#[test]
fn repeated_tmux_session_id_differs_across_server_incarnations() {
    let original = live_key("boot-a", process(4242, 818_221), "$0");
    let rebooted = live_key("boot-b", process(4242, 818_221), "$0");
    let replacement_pid = live_key("boot-a", process(4243, 818_221), "$0");
    let reused_pid = live_key("boot-a", process(4242, 918_221), "$0");

    assert_eq!(
        HashSet::from([original, rebooted, replacement_pid, reused_pid]).len(),
        4
    );
}

#[test]
fn one_assigned_identity_can_be_detected_on_two_live_keys() {
    let assigned = session(SESSION_A);
    let first =
        SessionIdentityBinding::new(live_key("boot-a", process(4242, 818_221), "$0"), assigned);
    let second =
        SessionIdentityBinding::new(live_key("boot-a", process(4242, 818_221), "$1"), assigned);

    assert_eq!(first.session_instance_id(), second.session_instance_id());
    assert_ne!(first.live_session_key(), second.live_session_key());
    assert_ne!(first, second);
}

#[test]
fn live_session_and_binding_wire_shapes_are_validated() {
    let key = live_key("boot-a", process(4242, 818_221), "$0");
    let binding = SessionIdentityBinding::new(key.clone(), session(SESSION_A));
    let key_wire = format!(
        r#"{{"workspace_id":"{WORKSPACE_A}","os_boot_id":"boot-a","tmux_server":{{"pid":4242,"birth":818221}},"tmux_session_id":"$0"}}"#
    );
    let binding_wire =
        format!(r#"{{"live_session_key":{key_wire},"session_instance_id":"{SESSION_A}"}}"#);

    assert_eq!(serde_json::to_string(&key).unwrap(), key_wire);
    assert_eq!(
        serde_json::from_str::<LiveSessionKey>(&key_wire).unwrap(),
        key
    );
    assert_eq!(serde_json::to_string(&binding).unwrap(), binding_wire);
    assert_eq!(
        serde_json::from_str::<SessionIdentityBinding>(&binding_wire).unwrap(),
        binding
    );

    let invalid = key_wire.replace("\"$0\"", "\"$00\"");
    assert!(serde_json::from_str::<LiveSessionKey>(&invalid).is_err());
}

#[test]
fn recipient_identity_uses_durable_session_and_pane() {
    let before_rename = RecipientKey::agent(workspace(WORKSPACE_A), session(SESSION_A), pane("%7"));
    let after_rename = RecipientKey::agent(workspace(WORKSPACE_A), session(SESSION_A), pane("%7"));

    assert_eq!(before_rename, after_rename);
    let wire = serde_json::to_string(&before_rename).unwrap();
    assert_eq!(
        wire,
        format!(
            r#"{{"kind":"agent","workspace_id":"{WORKSPACE_A}","session_instance_id":"{SESSION_A}","pane_id":"%7"}}"#
        )
    );
    assert!(!wire.contains("label"));
    assert!(!wire.contains("reviewer"));
}

#[test]
fn recipient_keys_are_separated_by_kind_and_scope() {
    let admin = RecipientKey::admin(workspace(WORKSPACE_A));
    let agent = RecipientKey::agent(workspace(WORKSPACE_A), session(SESSION_A), pane("%7"));
    let other_pane = RecipientKey::agent(workspace(WORKSPACE_A), session(SESSION_A), pane("%8"));
    let other_session = RecipientKey::agent(workspace(WORKSPACE_A), session(SESSION_B), pane("%7"));
    let other_workspace =
        RecipientKey::agent(workspace(WORKSPACE_B), session(SESSION_A), pane("%7"));

    assert_eq!(
        HashSet::from([admin, agent, other_pane, other_session, other_workspace]).len(),
        5
    );
    assert_ne!(
        RecipientKey::admin(workspace(WORKSPACE_A)),
        RecipientKey::admin(workspace(WORKSPACE_B))
    );
}

#[test]
fn recipient_serde_rejects_invalid_nested_ids() {
    let invalid_pane = format!(
        r#"{{"kind":"agent","workspace_id":"{WORKSPACE_A}","session_instance_id":"{SESSION_A}","pane_id":"%01"}}"#
    );
    let invalid_workspace = format!(
        r#"{{"kind":"agent","workspace_id":"00000000-0000-0000-0000-000000000000","session_instance_id":"{SESSION_A}","pane_id":"%1"}}"#
    );

    assert!(serde_json::from_str::<RecipientKey>(&invalid_pane).is_err());
    assert!(serde_json::from_str::<RecipientKey>(&invalid_workspace).is_err());
}
