use cyclops_client::{HelloCompatibility, RuntimeIdentity, CLIENT_VERSION};
use cyclops_proto::{Hello, BUILD_REF, PROTOCOL_VERSION, VERSION_WITH_BUILD};

#[test]
fn a_mismatched_hello_names_both_exact_versions_and_builds() {
    let compatibility = HelloCompatibility::between(
        RuntimeIdentity::new("0.1.0", Some("client-new")),
        RuntimeIdentity::new("0.0.9", Some("daemon-old")),
    );

    assert!(!compatibility.version_matches());
    assert_eq!(compatibility.build_matches(), Some(false));
    assert_eq!(
        compatibility.identities().0.description(),
        "0.1.0 (client-new)"
    );
    assert_eq!(
        compatibility.identities().1.description(),
        "0.0.9 (daemon-old)"
    );
}

#[test]
fn hello_classification_uses_the_cargo_version_and_shared_build_stamp() {
    let current = RuntimeIdentity::current_client();
    assert_eq!(current.version, CLIENT_VERSION);
    assert_eq!(current.build.as_deref(), Some(BUILD_REF));
    assert_eq!(
        current.description(),
        VERSION_WITH_BUILD,
        "--version and Hello checks must use the same compiled identity"
    );

    let hello = Hello {
        cyclops: CLIENT_VERSION.to_string(),
        build: Some(BUILD_REF.to_string()),
        daemon_process: None,
        daemon_executable: None,
        proto: PROTOCOL_VERSION,
        boot_id: "identity-test".into(),
    };
    assert!(matches!(
        HelloCompatibility::from_hello(&hello),
        HelloCompatibility::Current { .. }
    ));
    let compatibility = HelloCompatibility::from_hello(&hello);
    assert!(compatibility.version_matches());
    assert_eq!(compatibility.build_matches(), Some(true));
}

#[test]
fn an_old_daemon_without_a_build_remains_explicitly_unverified() {
    let compatibility = HelloCompatibility::between(
        RuntimeIdentity::new("0.1.0", Some("client-new")),
        RuntimeIdentity::new("0.0.8", None),
    );

    assert!(!compatibility.version_matches());
    assert_eq!(compatibility.build_matches(), None);
}

#[test]
fn a_binary_version_line_round_trips_through_the_runtime_identity() {
    let identity = RuntimeIdentity::parse("0.1.0 (abc1234)").expect("valid identity");
    assert_eq!(identity, RuntimeIdentity::new("0.1.0", Some("abc1234")));
    assert_eq!(identity.description(), "0.1.0 (abc1234)");

    for invalid in ["0.1.0", "0.1.0 ()", " (abc1234)", ""] {
        assert!(RuntimeIdentity::parse(invalid).is_none(), "{invalid:?}");
    }
}
