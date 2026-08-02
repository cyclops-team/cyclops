//! PROBE (MEASURED, tmux 3.6a): refresh-client -B subscriptions DO work in
//! control mode. Subscribing to a per-pane format produces
//! %subscription-changed when the pane title changes, both via
//! select-pane -T from outside and via an OSC escape printed inside the
//! pane. This is why the watcher uses subscriptions as its primary per-pane
//! change signal. If this test starts failing on a new tmux, the watcher
//! must fall back to notification hints plus output-activity reconciles.

mod common;

use std::time::Duration;

use common::TestServer;
use cyclops_tmux::{ControlClient, Notification};
use tokio::sync::mpsc::UnboundedReceiver;

/// Await a subscription-changed for `sub` whose value contains `needle`.
async fn await_sub_value(
    rx: &mut UnboundedReceiver<Notification>,
    sub: &str,
    needle: &str,
) -> Notification {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Some(Notification::SubscriptionChanged {
                    name,
                    value,
                    pane,
                    session,
                    window,
                }) if name == sub && value.contains(needle) => {
                    return Notification::SubscriptionChanged {
                        name,
                        value,
                        pane,
                        session,
                        window,
                    };
                }
                Some(_) => continue,
                None => panic!("control stream closed waiting for {needle}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no %subscription-changed carrying {needle:?}"))
}

#[tokio::test]
async fn subscriptions_fire_on_title_changes_in_control_mode() {
    let Some(srv) = TestServer::new("subprobe") else {
        return;
    };
    srv.new_session("sp");
    let (client, mut notif) = ControlClient::spawn(srv.config("sp")).await.expect("spawn");

    client
        .command(
            "refresh-client -B 'probe:%0:#{pane_title}\t#{pane_dead}\t#{pane_in_mode}\t#{pane_current_command}'",
        )
        .await
        .expect("subscribe");

    // External title change.
    srv.tmux_ok(&["select-pane", "-t", "%0", "-T", "SUB_T_OUTSIDE"]);
    let n = await_sub_value(&mut notif, "probe", "SUB_T_OUTSIDE").await;
    if let Notification::SubscriptionChanged { pane, .. } = &n {
        assert_eq!(pane.as_deref(), Some("%0"), "pane id travels in the header");
    }

    // OSC title change from inside the pane.
    srv.tmux_ok(&[
        "send-keys",
        "-t",
        "%0",
        "printf '\\033]2;SUB_T_OSC\\007'",
        "Enter",
    ]);
    await_sub_value(&mut notif, "probe", "SUB_T_OSC").await;

    // pane_in_mode flips also arrive through the subscription.
    srv.tmux_ok(&["copy-mode", "-t", "%0"]);
    let n = await_sub_value(&mut notif, "probe", "\t1\t").await;
    if let Notification::SubscriptionChanged { value, .. } = &n {
        let fields: Vec<&str> = value.rsplitn(4, '\t').collect();
        assert_eq!(fields[1], "1", "pane_in_mode field is set: {value:?}");
    }

    client.shutdown().await;
}
