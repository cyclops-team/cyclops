use cyclops_client::{RuntimeIdentity, CLIENT_VERSION};
use cyclops_proto::Hello;
use cyclops_ui::{build, App, BuildHealth, Filter, Theme, View};

fn hello(build: Option<&str>) -> Hello {
    Hello {
        cyclops: CLIENT_VERSION.into(),
        build: build.map(str::to_string),
        daemon_process: None,
        daemon_executable: None,
        proto: cyclops_proto::PROTOCOL_VERSION,
        boot_id: "boot-test".into(),
    }
}

fn app(health: BuildHealth) -> App {
    let mut app = App::new(Theme::none(), View::Admin, Filter::default());
    app.build_health = Some(health);
    app
}

#[test]
fn hello_keeps_both_build_identities_or_names_a_legacy_daemon() {
    let mismatch = BuildHealth::from_hello(&hello(Some("daemon-old")));
    assert!(matches!(
        mismatch,
        BuildHealth::Mismatch {
            ref daemon,
            ref client
        } if daemon.build.as_deref() == Some("daemon-old")
            && client.build.as_deref() == Some(cyclops_proto::BUILD_REF)
            && daemon.version == CLIENT_VERSION
    ));

    let legacy = BuildHealth::from_hello(&hello(None));
    assert!(matches!(
        legacy,
        BuildHealth::UnverifiedDaemon { ref client, ref daemon }
            if client.build.as_deref() == Some(cyclops_proto::BUILD_REF)
                && daemon.build.is_none()
    ));
}

#[test]
fn build_mismatch_stays_visible_after_transient_notices_clear() {
    let mut app = app(BuildHealth::Mismatch {
        client: RuntimeIdentity::new(CLIENT_VERSION, Some("client-new")),
        daemon: RuntimeIdentity::new("0.0.9", Some("daemon-old")),
    });

    let first = build(&mut app, 120, 24).join("\n");
    assert!(first.contains("build mismatch"), "{first}");
    assert!(first.contains("client-new"), "{first}");
    assert!(first.contains("daemon-old"), "{first}");
    assert!(first.contains(CLIENT_VERSION), "{first}");
    assert!(first.contains("0.0.9"), "{first}");

    app.notice = Some("temporary action result".into());
    let with_transient = build(&mut app, 120, 24).join("\n");
    assert!(
        with_transient.contains("build mismatch"),
        "{with_transient}"
    );
    assert!(
        with_transient.contains("temporary action result"),
        "{with_transient}"
    );

    app.notice = None;
    let cleared = build(&mut app, 120, 24).join("\n");
    assert!(cleared.contains("build mismatch"), "{cleared}");
}

#[test]
fn a_legacy_daemon_without_build_identity_stays_visible() {
    let mut app = app(BuildHealth::UnverifiedDaemon {
        client: RuntimeIdentity::new(CLIENT_VERSION, Some("client-new")),
        daemon: RuntimeIdentity::new("0.0.8", None),
    });

    let frame = build(&mut app, 120, 24).join("\n");
    assert!(frame.contains("daemon identity unverified"), "{frame}");
    assert!(frame.contains("client-new"), "{frame}");
    assert!(frame.contains("cyclops daemon restart"), "{frame}");
}
