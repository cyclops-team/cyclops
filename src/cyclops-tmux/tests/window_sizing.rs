//! Window sizing against a real tmux server.
//!
//! Every test runs on its own isolated server from `cyclops-testrig`, which
//! kills it and unlinks the socket on drop, so nothing here can touch the
//! developer's tmux. These are the regressions for the contract in
//! `sizing.rs`: one owner sizes a session, everybody else keeps their hands
//! off, and whatever the owner changed is put back the way it was found.

mod common;

use common::TestServer;
use cyclops_tmux::sizing::{Captured, ClientIdentity, PriorWindowSize, ReleaseOutcome, Restored};
use cyclops_tmux::{ControlClient, TmuxError};

/// Size of a window as tmux reports it now.
fn window_size(srv: &TestServer, target: &str) -> String {
    let out = srv.tmux(&[
        "display-message",
        "-p",
        "-t",
        target,
        "#{window_width}x#{window_height}",
    ]);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A window's own `window-size`, empty when it inherits one.
fn window_size_option(srv: &TestServer, window: &str) -> String {
    let out = srv.tmux(&["show-options", "-w", "-t", window, "-qv", "window-size"]);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn first_window(srv: &TestServer, session: &str) -> String {
    let out = srv.tmux(&["list-windows", "-t", session, "-F", "#{window_id}"]);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .expect("a session has a window")
        .to_string()
}

fn window_ids(srv: &TestServer, session: &str) -> Vec<String> {
    let out = srv.tmux(&["list-windows", "-t", session, "-F", "#{window_id}"]);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

/// Claim, capture, pin, and size one window, the way an owning workspace
/// does at boot. Returns the capture that ended up in force.
async fn own_and_size(
    client: &ControlClient,
    session: &str,
    window: &str,
    cols: u16,
    rows: u16,
) -> Result<Captured, TmuxError> {
    let me = client.client_identity().await?;
    assert!(
        client.claim_window_driver(session, &me.marker()).await?,
        "the first claimant must win"
    );
    // Capture strictly before the pin. The reverse order loses the original.
    let prior = client.capture_prior_window_size(window).await?;
    client.pin_window_size_manual(window).await?;
    client.resize_window(window, cols, rows).await?;
    Ok(prior)
}

/// A second workspace on a session somebody already owns never writes a
/// size, so the owner's panes keep the geometry the operator set up.
///
/// This is Admin's measured 62x21 collapse, as a test. Under a vote-based
/// policy the small viewer wins by being small; here it loses by arriving
/// second, and losing means writing nothing at all.
#[tokio::test]
async fn a_small_follower_never_resizes_the_owner() {
    let Some(srv) = TestServer::new("sizing-follower") else {
        return;
    };
    srv.new_session("work");
    let window = first_window(&srv, "work");

    let (owner, _n1) = ControlClient::spawn(srv.config("work"))
        .await
        .expect("owner");
    own_and_size(&owner, "work", &window, 176, 47)
        .await
        .expect("the owner sizes the session");
    assert_eq!(window_size(&srv, "work"), "176x47");

    let (follower, _n2) = ControlClient::spawn(srv.config("work"))
        .await
        .expect("follower");
    let them = follower.client_identity().await.expect("identity");
    assert!(
        !follower
            .claim_window_driver("work", &them.marker())
            .await
            .expect("claim"),
        "a session that already has an owner refuses a second one"
    );
    assert_eq!(
        window_size(&srv, "work"),
        "176x47",
        "the follower's arrival moved the owner's window"
    );

    // And the follower staying attached does not drift it either.
    assert_eq!(
        window_size_option(&srv, &window),
        "manual",
        "the window left manual sizing while a follower was attached"
    );
    assert_eq!(window_size(&srv, "work"), "176x47");
}

/// Two workspaces booting into the same session at the same time produce
/// exactly one owner, because the claim is create-only and the readback,
/// not the write, decides.
#[tokio::test]
async fn concurrent_first_claims_have_one_winner() {
    let Some(srv) = TestServer::new("sizing-first-claim") else {
        return;
    };
    srv.new_session("work");

    let (a, _n1) = ControlClient::spawn(srv.config("work")).await.expect("a");
    let (b, _n2) = ControlClient::spawn(srv.config("work")).await.expect("b");
    let ma = a.client_identity().await.expect("a identity").marker();
    let mb = b.client_identity().await.expect("b identity").marker();
    assert_ne!(ma, mb, "two connections must not share an identity");

    let (won_a, won_b) = tokio::join!(
        a.claim_window_driver("work", &ma),
        b.claim_window_driver("work", &mb)
    );
    let won_a = won_a.expect("a claim");
    let won_b = won_b.expect("b claim");
    assert!(
        won_a ^ won_b,
        "exactly one claimant must win, got a={won_a} b={won_b}"
    );

    let owner = a
        .window_driver("work")
        .await
        .expect("readback")
        .expect("set");
    assert_eq!(owner, if won_a { ma } else { mb });
}

/// Two followers racing to replace an owner that died produce one winner,
/// because the compare and the set happen inside the tmux server.
#[tokio::test]
async fn a_stale_owner_is_taken_over_by_exactly_one_follower() {
    let Some(srv) = TestServer::new("sizing-takeover") else {
        return;
    };
    srv.new_session("work");

    // An owner that is not attached to anything: the marker of a client
    // that has gone away.
    let dead = ClientIdentity {
        name: "client-999999".into(),
        created: "1700000000".into(),
    }
    .marker();
    srv.tmux_ok(&[
        "set-option",
        "-t",
        "work",
        "@cyclops_window_driver",
        dead.as_str(),
    ]);

    let (a, _n1) = ControlClient::spawn(srv.config("work")).await.expect("a");
    let (b, _n2) = ControlClient::spawn(srv.config("work")).await.expect("b");
    let ma = a.client_identity().await.expect("a identity").marker();
    let mb = b.client_identity().await.expect("b identity").marker();

    // Both see the same stale mark, and both act on it.
    let live = a.session_client_markers("work").await.expect("clients");
    assert!(
        !live.contains(&dead),
        "the fixture's dead owner must not be attached"
    );

    let (took_a, took_b) = tokio::join!(
        a.take_over_window_driver("work", &dead, &ma),
        b.take_over_window_driver("work", &dead, &mb)
    );
    let took_a = took_a.expect("a takeover");
    let took_b = took_b.expect("b takeover");
    assert!(
        took_a ^ took_b,
        "exactly one follower must take over, got a={took_a} b={took_b}"
    );
    let owner = a
        .window_driver("work")
        .await
        .expect("readback")
        .expect("set");
    assert_eq!(owner, if took_a { ma } else { mb });
    assert_ne!(owner, dead, "the dead owner still holds the mark");
}

/// A session's windows do not share a `window-size`, and they do not share
/// a history either: one inherited its policy and one had its own. Both go
/// back exactly as they were, which for the inherited one means having no
/// value of its own again rather than being set to the value it was
/// inheriting.
#[tokio::test]
async fn inherited_and_explicit_policies_round_trip_per_window() {
    let Some(srv) = TestServer::new("sizing-round-trip") else {
        return;
    };
    srv.new_session("work");
    srv.tmux_ok(&["new-window", "-t", "work:", "/bin/sh"]);
    let windows = window_ids(&srv, "work");
    assert_eq!(windows.len(), 2, "the fixture wants two windows");
    let (inherited, explicit) = (&windows[0], &windows[1]);

    // One window carries its own policy; the other has never been touched.
    srv.tmux_ok(&["set-option", "-w", "-t", explicit, "window-size", "latest"]);
    assert_eq!(window_size_option(&srv, inherited), "");
    assert_eq!(window_size_option(&srv, explicit), "latest");

    let (owner, _n) = ControlClient::spawn(srv.config("work"))
        .await
        .expect("owner");
    let me = owner.client_identity().await.expect("identity");
    assert!(owner
        .claim_window_driver("work", &me.marker())
        .await
        .expect("claim"));

    for window in &windows {
        owner
            .capture_prior_window_size(window)
            .await
            .expect("capture");
        owner.pin_window_size_manual(window).await.expect("pin");
        owner.resize_window(window, 100, 30).await.expect("resize");
    }
    assert_eq!(window_size_option(&srv, inherited), "manual");
    assert_eq!(window_size_option(&srv, explicit), "manual");

    for window in &windows {
        assert_eq!(
            owner.restore_window_size(window).await.expect("restore"),
            Restored::Exactly,
            "each pinned window had a capture to restore exactly"
        );
    }
    assert_eq!(
        window_size_option(&srv, inherited),
        "",
        "an inherited window came back owning an explicit policy"
    );
    assert_eq!(
        window_size_option(&srv, explicit),
        "latest",
        "an explicit window came back with the wrong value"
    );
}

/// The capture lives in the tmux server, not in the workspace, so a
/// workspace that dies without restoring does not take the original with
/// it. The next owner inherits the true original and restores that, rather
/// than preserving its dead predecessor's `manual` forever.
#[tokio::test]
async fn a_crash_then_a_new_owner_restores_the_original_not_manual() {
    let Some(srv) = TestServer::new("sizing-crash") else {
        return;
    };
    srv.new_session("work");
    let window = first_window(&srv, "work");
    assert_eq!(window_size_option(&srv, &window), "");

    {
        let (doomed, _n) = ControlClient::spawn(srv.config("work"))
            .await
            .expect("first");
        let prior = own_and_size(&doomed, "work", &window, 176, 47)
            .await
            .expect("first owner sizes");
        assert_eq!(prior, Captured::Record(PriorWindowSize::Inherited));
        // Dropped without restoring and without releasing: a crash.
    }
    assert_eq!(
        window_size_option(&srv, &window),
        "manual",
        "the fixture wants the window left pinned by the dead owner"
    );

    let (next, _n) = ControlClient::spawn(srv.config("work"))
        .await
        .expect("next");
    let me = next.client_identity().await.expect("identity");
    let stale = next
        .window_driver("work")
        .await
        .expect("readback")
        .expect("the dead owner left its mark");
    assert!(
        next.take_over_window_driver("work", &stale, &me.marker())
            .await
            .expect("takeover"),
        "the only live workspace must be able to take over"
    );

    // The new owner sees `manual` on the window, and must not record that
    // as the original.
    let prior = next
        .capture_prior_window_size(&window)
        .await
        .expect("capture");
    assert_eq!(
        prior,
        Captured::Record(PriorWindowSize::Inherited),
        "the new owner recorded its predecessor's pin as the original"
    );

    next.restore_window_size(&window).await.expect("restore");
    next.release_window_driver("work").await.expect("release");
    assert_eq!(
        window_size_option(&srv, &window),
        "",
        "a clean exit left the window on manual after a crash"
    );
    assert_eq!(next.window_driver("work").await.expect("readback"), None);
}

/// A window opened after the workspace booted gets the same treatment, and
/// in the same order: what it was is recorded before anything pins it.
#[tokio::test]
async fn a_new_window_is_captured_before_it_is_pinned() {
    let Some(srv) = TestServer::new("sizing-new-window") else {
        return;
    };
    srv.new_session("work");
    let first = first_window(&srv, "work");

    let (owner, _n) = ControlClient::spawn(srv.config("work"))
        .await
        .expect("owner");
    own_and_size(&owner, "work", &first, 176, 47)
        .await
        .expect("boot window");

    // A tab opened later, carrying its own policy so the capture has
    // something specific to be right about.
    srv.tmux_ok(&["new-window", "-t", "work:", "/bin/sh"]);
    let added = window_ids(&srv, "work")
        .into_iter()
        .find(|id| *id != first)
        .expect("a second window");
    srv.tmux_ok(&["set-option", "-w", "-t", &added, "window-size", "smallest"]);

    let prior = owner
        .capture_prior_window_size(&added)
        .await
        .expect("capture");
    assert_eq!(
        prior,
        Captured::Record(PriorWindowSize::Explicit("smallest".into())),
        "the new window's own policy was not the one recorded"
    );
    owner.pin_window_size_manual(&added).await.expect("pin");
    owner.resize_window(&added, 176, 47).await.expect("resize");
    assert_eq!(window_size_option(&srv, &added), "manual");

    owner.restore_window_size(&added).await.expect("restore");
    assert_eq!(
        window_size_option(&srv, &added),
        "smallest",
        "the window opened later did not get its own policy back"
    );
}

/// Recovery can be reached twice: by a clean exit and by a later owner
/// tidying up after a crash. Doing it twice is not an error and does not
/// undo the first one.
#[tokio::test]
async fn recovery_is_idempotent() {
    let Some(srv) = TestServer::new("sizing-idempotent") else {
        return;
    };
    srv.new_session("work");
    let window = first_window(&srv, "work");

    let (owner, _n) = ControlClient::spawn(srv.config("work"))
        .await
        .expect("owner");
    own_and_size(&owner, "work", &window, 176, 47)
        .await
        .expect("owner sizes");

    assert_eq!(
        owner.restore_window_size(&window).await.expect("first"),
        Restored::Exactly,
        "the first restore had a capture to consume"
    );
    assert_eq!(
        owner.restore_window_size(&window).await.expect("second"),
        Restored::Nothing,
        "the second restore invented a capture"
    );
    assert_eq!(
        window_size_option(&srv, &window),
        "",
        "restoring twice moved the window off its original policy"
    );

    // Releasing twice is likewise not an error.
    owner.release_window_driver("work").await.expect("release");
    owner
        .release_window_driver("work")
        .await
        .expect("release again");
    assert_eq!(owner.window_driver("work").await.expect("readback"), None);
}

/// Navigating away from a session and back is not a change of ownership.
///
/// One workspace holds one connection and one identity while
/// `switch-client` moves which session it displays, so leaving session A
/// takes this client out of A's client list without it having died. Nothing
/// about that may re-run the election, put A's windows back, or re-pin
/// them, and A must keep the size its owner gave it throughout.
#[tokio::test]
async fn navigating_away_and_back_does_not_re_elect_or_re_pin() {
    let Some(srv) = TestServer::new("sizing-navigation") else {
        return;
    };
    srv.new_session("alpha");
    srv.new_session("beta");
    let alpha_window = first_window(&srv, "alpha");
    let beta_window = first_window(&srv, "beta");

    let (owner, _n) = ControlClient::spawn(srv.config("alpha"))
        .await
        .expect("owner");
    let me = owner.client_identity().await.expect("identity");
    own_and_size(&owner, "alpha", &alpha_window, 176, 47)
        .await
        .expect("owner sizes alpha");
    let capture_after_boot = owner
        .prior_window_size(&alpha_window)
        .await
        .expect("capture");
    assert_eq!(capture_after_boot, Some(PriorWindowSize::Inherited));

    // Navigate to beta and take ownership there too.
    owner
        .command("switch-client -t 'beta'")
        .await
        .expect("switch to beta");
    let me_after_switch = owner
        .client_identity()
        .await
        .expect("identity after switch");
    assert_eq!(
        me_after_switch, me,
        "one connection must keep one identity across navigation"
    );

    // Alpha no longer lists this client, and that must not read as death.
    let alpha_clients = owner
        .session_client_markers("alpha")
        .await
        .expect("alpha clients");
    assert!(
        !alpha_clients.contains(&me.marker()),
        "the fixture wants the owner displaying beta, not alpha"
    );
    assert_eq!(
        owner.window_driver("alpha").await.expect("readback"),
        Some(me.marker()),
        "navigating away cleared alpha's owner"
    );

    // Sizing beta from here proves a background session stays sizable.
    own_and_size(&owner, "beta", &beta_window, 100, 30)
        .await
        .expect("owner sizes beta");
    assert_eq!(window_size(&srv, "beta"), "100x30");
    assert_eq!(
        window_size(&srv, "alpha"),
        "176x47",
        "alpha moved while the workspace was looking at beta"
    );

    // Back to alpha. A workspace that already owns a session re-reads its
    // own mark and finds itself, so there is no second claim to make.
    owner
        .command("switch-client -t 'alpha'")
        .await
        .expect("switch back");
    assert_eq!(
        owner.window_driver("alpha").await.expect("readback"),
        Some(me.marker()),
        "returning to alpha changed its owner"
    );
    assert_eq!(
        owner
            .prior_window_size(&alpha_window)
            .await
            .expect("capture"),
        capture_after_boot,
        "returning to alpha rewrote the capture taken at boot"
    );
    assert_eq!(
        window_size_option(&srv, &alpha_window),
        "manual",
        "returning to alpha unpinned it"
    );
    assert_eq!(
        window_size(&srv, "alpha"),
        "176x47",
        "returning to alpha resized it"
    );
    assert_eq!(
        window_size(&srv, "beta"),
        "100x30",
        "returning to alpha resized the session it left"
    );
}

/// A record nobody can read means the original is unknowable, so nothing
/// is changed and nothing is thrown away.
///
/// The tempting move is to put the window back on inheritance and get on
/// with it. That invents a policy the operator never set and destroys the
/// only evidence of what the window really was. A window left pinned and
/// still owned is visibly wrong and fully recoverable, which is the better
/// of the two failures. So: the pin stays, the record stays, the ownership
/// mark stays, and the caller is told.
#[tokio::test]
async fn a_malformed_record_changes_nothing_and_keeps_the_evidence() {
    let Some(srv) = TestServer::new("sizing-malformed") else {
        return;
    };
    srv.new_session("work");
    let window = first_window(&srv, "work");

    let (owner, _n) = ControlClient::spawn(srv.config("work"))
        .await
        .expect("owner");
    own_and_size(&owner, "work", &window, 176, 47)
        .await
        .expect("owner sizes");
    let marker = owner.client_identity().await.expect("identity").marker();

    // A record this version cannot read: a future format, a truncated
    // write, an operator experimenting with the option.
    let garbage = "from-a-version-that-does-not-exist-yet";
    srv.tmux_ok(&[
        "set-option",
        "-w",
        "-t",
        &window,
        "@cyclops_prior_window_size",
        garbage,
    ]);

    assert_eq!(
        owner.restore_window_size(&window).await.expect("restore"),
        Restored::Malformed,
        "an unreadable record was treated as a restore"
    );
    assert_eq!(
        window_size_option(&srv, &window),
        "manual",
        "the window was moved off manual on a guess"
    );
    let kept = srv.tmux(&[
        "show-options",
        "-w",
        "-t",
        &window,
        "-qv",
        "@cyclops_prior_window_size",
    ]);
    assert_eq!(
        String::from_utf8_lossy(&kept.stdout).trim(),
        garbage,
        "the only evidence of the original was destroyed"
    );
    assert_eq!(
        owner.window_driver("work").await.expect("readback"),
        Some(marker.clone()),
        "ownership was released over a window that is still pinned"
    );

    // Asking again says the same thing. Nothing drifts, nothing is
    // consumed, and no repeat call quietly succeeds.
    assert_eq!(
        owner.restore_window_size(&window).await.expect("again"),
        Restored::Malformed
    );

    // The operator command refuses too, on the same evidence, and leaves
    // the session owned rather than clearing a mark it cannot honour. The
    // owner goes first: while it is running the command refuses for a
    // different and earlier reason, which is its own test.
    owner.shutdown().await;
    let ReleaseOutcome::Released(released) =
        cyclops_tmux::release_session_sizing("work", Some(srv.sock())).expect("operator release")
    else {
        panic!("the owning client has gone, so this must not refuse for a live owner");
    };
    assert_eq!(
        released
            .iter()
            .filter(|w| w.outcome == Restored::Malformed)
            .count(),
        1,
        "the operator path did not report the unreadable window"
    );
    assert_eq!(window_size_option(&srv, &window), "manual");
    let still = srv.tmux(&[
        "show-options",
        "-w",
        "-t",
        &window,
        "-qv",
        "@cyclops_prior_window_size",
    ]);
    assert_eq!(
        String::from_utf8_lossy(&still.stdout).trim(),
        garbage,
        "the operator path destroyed the evidence"
    );
    let mark = srv.tmux(&[
        "show-options",
        "-t",
        "work",
        "-qv",
        "@cyclops_window_driver",
    ]);
    assert_eq!(
        String::from_utf8_lossy(&mark.stdout).trim(),
        marker,
        "the operator path released a session it could not restore"
    );
}

/// The operator's way out works with no workspace running at all.
///
/// This is the promise the design makes about a workspace killed hard: any
/// later workspace repairs it, and if none is coming, one command does.
/// A promise with no command behind it is the same as no promise.
#[tokio::test]
async fn an_operator_can_release_a_session_with_no_workspace_running() {
    let Some(srv) = TestServer::new("sizing-operator") else {
        return;
    };
    srv.new_session("work");
    srv.tmux_ok(&["new-window", "-t", "work:", "/bin/sh"]);
    let windows = window_ids(&srv, "work");
    let (inherited, explicit) = (windows[0].clone(), windows[1].clone());
    srv.tmux_ok(&["set-option", "-w", "-t", &explicit, "window-size", "latest"]);

    {
        let (doomed, _n) = ControlClient::spawn(srv.config("work"))
            .await
            .expect("owner");
        let me = doomed.client_identity().await.expect("identity");
        assert!(doomed
            .claim_window_driver("work", &me.marker())
            .await
            .expect("claim"));
        for window in &windows {
            doomed
                .capture_prior_window_size(window)
                .await
                .expect("capture");
            doomed.pin_window_size_manual(window).await.expect("pin");
            doomed.resize_window(window, 176, 47).await.expect("size");
        }
        // Killed hard: no restore, no release, and no workspace coming.
    }
    assert_eq!(window_size_option(&srv, &inherited), "manual");
    assert_eq!(window_size_option(&srv, &explicit), "manual");

    let ReleaseOutcome::Released(released) =
        cyclops_tmux::release_session_sizing("work", Some(srv.sock())).expect("operator release")
    else {
        panic!("no workspace is running, so the release must not refuse");
    };
    assert_eq!(released.len(), 2);
    assert!(released.iter().all(|w| w.outcome == Restored::Exactly));
    assert_eq!(
        window_size_option(&srv, &inherited),
        "",
        "the inherited window was left carrying an explicit policy"
    );
    assert_eq!(
        window_size_option(&srv, &explicit),
        "latest",
        "the explicit window did not get its own value back"
    );
    let mark = srv.tmux(&[
        "show-options",
        "-t",
        "work",
        "-qv",
        "@cyclops_window_driver",
    ]);
    assert_eq!(
        String::from_utf8_lossy(&mark.stdout).trim(),
        "",
        "the dead owner still holds the mark"
    );

    // Twice is not an error, and it does not touch windows cyclops never
    // sized, which by now is all of them.
    let ReleaseOutcome::Released(again) =
        cyclops_tmux::release_session_sizing("work", Some(srv.sock())).expect("second release")
    else {
        panic!("the mark was cleared by the first release, so this must not refuse");
    };
    assert!(again.iter().all(|w| w.outcome == Restored::Nothing));
    assert_eq!(window_size_option(&srv, &explicit), "latest");
}

/// Recovery refuses while the session's owner is still running.
///
/// A live owner holds the session in memory and keeps issuing
/// `resize-window` for it. Restoring underneath it would fight a workspace
/// that has not finished, and would leave the two disagreeing about what it
/// owns. Recovery is for an owner that is gone.
#[tokio::test]
async fn an_operator_release_refuses_while_the_owner_is_live() {
    let Some(srv) = TestServer::new("sizing-live-owner") else {
        return;
    };
    srv.new_session("work");
    let window = first_window(&srv, "work");

    let (owner, _n) = ControlClient::spawn(srv.config("work"))
        .await
        .expect("owner");
    own_and_size(&owner, "work", &window, 176, 47)
        .await
        .expect("owner sizes");
    let marker = owner.client_identity().await.expect("identity").marker();

    match cyclops_tmux::release_session_sizing("work", Some(srv.sock())).expect("release") {
        ReleaseOutcome::RefusedLiveOwner { marker: named } => assert_eq!(named, marker),
        ReleaseOutcome::Released(_) => panic!("recovery ran underneath a live owner"),
    }

    // A refusal changes nothing at all.
    assert_eq!(window_size_option(&srv, &window), "manual");
    assert_eq!(window_size(&srv, "work"), "176x47");
    let mark = srv.tmux(&[
        "show-options",
        "-t",
        "work",
        "-qv",
        "@cyclops_window_driver",
    ]);
    assert_eq!(String::from_utf8_lossy(&mark.stdout).trim(), marker);
    let record = srv.tmux(&[
        "show-options",
        "-w",
        "-t",
        &window,
        "-qv",
        "@cyclops_prior_window_size",
    ]);
    assert_eq!(String::from_utf8_lossy(&record.stdout).trim(), "inherited");

    // Once that owner is gone, the same command works.
    owner.shutdown().await;
    let ReleaseOutcome::Released(released) =
        cyclops_tmux::release_session_sizing("work", Some(srv.sock())).expect("release")
    else {
        panic!("the owner has gone, so this must not refuse");
    };
    assert!(released.iter().all(|w| w.outcome == Restored::Exactly));
    assert_eq!(window_size_option(&srv, &window), "");
}

/// A window whose record is already unreadable when a workspace arrives is
/// never pinned, and never silently dropped either.
///
/// This is the hole the first cut left. Capturing parsed the record, a
/// malformed one read as absent, the create-only write was refused, the
/// call errored, adoption logged and moved on, and the window was simply
/// not owned. Quitting then found a session with nothing to restore and
/// released the mark, leaving `manual` plus an unreadable record plus no
/// owner: the exact state this is all meant to forbid.
#[tokio::test]
async fn a_pre_existing_malformed_record_is_reported_not_captured() {
    let Some(srv) = TestServer::new("sizing-pre-malformed") else {
        return;
    };
    srv.new_session("work");
    let window = first_window(&srv, "work");

    // The window is already pinned and already carries a record nobody can
    // read: a dead workspace's leftovers.
    let garbage = "written-by-something-else";
    srv.tmux_ok(&["set-option", "-w", "-t", &window, "window-size", "manual"]);
    srv.tmux_ok(&[
        "set-option",
        "-w",
        "-t",
        &window,
        "@cyclops_prior_window_size",
        garbage,
    ]);

    let (arriving, _n) = ControlClient::spawn(srv.config("work"))
        .await
        .expect("client");
    assert_eq!(
        arriving
            .capture_prior_window_size(&window)
            .await
            .expect("capture"),
        Captured::Malformed,
        "an unreadable record must be reported, not turned into an error"
    );

    // Nothing was written by the attempt.
    let record = srv.tmux(&[
        "show-options",
        "-w",
        "-t",
        &window,
        "-qv",
        "@cyclops_prior_window_size",
    ]);
    assert_eq!(
        String::from_utf8_lossy(&record.stdout).trim(),
        garbage,
        "capturing overwrote a record it could not read"
    );
    assert_eq!(window_size_option(&srv, &window), "manual");
}
