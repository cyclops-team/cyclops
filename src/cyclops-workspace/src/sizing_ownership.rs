//! Ownership and recovery for the tmux window sizes an interactive workspace controls.
//!
//! This is one reversible continuity protocol: elect one live workspace per
//! session, capture each window's prior sizing policy before pinning it, carry
//! ownership across reconnects, and restore before releasing. The application
//! loop asks for those operations without knowing their tmux transition order.

use std::collections::{BTreeMap, BTreeSet};

use cyclops_tmux::sizing::ClientIdentity;
use cyclops_tmux::{ControlClient, TmuxError};
use ratatui::layout::Rect;
use tokio::time::Instant;

use crate::app::log_err;
use crate::model::TabModel;
use crate::notice::NoticeState;

/// The smallest grid worth declaring to tmux. Below this the terminal is
/// nearly all chrome, and declaring the leftover sliver would reshape
/// every pane in the session to fit it. Boot and every later resize must
/// apply the same floor: if they disagree, a terminal declarable at boot
/// stops being declarable on the first resize, or the reverse, and the
/// panes are painted for a size tmux was never told about.
const MIN_DECLARABLE_SIZE: (u16, u16) = (10, 3);

pub(crate) fn declarable(size: (u16, u16)) -> bool {
    size.0 >= MIN_DECLARABLE_SIZE.0 && size.1 >= MIN_DECLARABLE_SIZE.1
}

/// Which sessions this workspace sizes, and what it owes them back.
///
/// A window's size is its panes' size, so sizing is not a viewer's private
/// business: it reshapes every agent running in that session. Exactly one
/// workspace per session therefore writes sizes, and the rest render inside
/// whatever it chose. `sizing.rs` holds the tmux side and the measurements
/// behind it; this holds what one process remembers.
///
/// Ownership is per session and lasts for the life of the process, not for
/// the life of a view. A workspace that navigates from a session keeps one
/// connection and one identity, so it vanishes from that session's client
/// list while remaining alive; re-electing on that would hand a session to
/// whoever glanced at it next and would put its windows back while its
/// owner was still using them.
#[derive(Debug, Default)]
pub struct WindowSizing {
    /// This connection's identity, read once. A reconnect is a new client
    /// and therefore a new identity, so this is dropped with the old link.
    pub identity: Option<ClientIdentity>,
    /// Sessions owned, each with what this workspace holds in it. Ordered
    /// so a restore visits them the same way twice.
    pub owned: BTreeMap<String, OwnedSession>,
    /// Sessions found already owned by a live workspace. Kept so a follower
    /// asks tmux once rather than on every reconcile.
    pub following: BTreeSet<String>,
}

/// The tab windows not yet pinned to the sizing policy, in tab order.
pub(crate) fn unpinned_windows<'a>(
    tabs: &'a [TabModel],
    pinned: &BTreeSet<String>,
) -> Vec<&'a str> {
    tabs.iter()
        .filter(|tab| !pinned.contains(&tab.window_id))
        .map(|tab| tab.window_id.as_str())
        .collect()
}

/// What this workspace holds in one session it owns.
#[derive(Debug, Default)]
pub struct OwnedSession {
    /// Windows this workspace pinned, and therefore must put back.
    pub pinned: BTreeSet<String>,
    /// Windows carrying a record this version cannot read.
    ///
    /// Never pinned by this workspace and never changed by it, and yet the
    /// reason the session stays owned. A window already on `manual` with an
    /// unreadable record is exactly the state that cannot recover on its
    /// own, and releasing the mark over it is what strands it: no policy
    /// applies, no owner exists, and no later workspace can tell what it
    /// was. Holding the mark keeps it visibly somebody's problem.
    pub blocked: BTreeSet<String>,
}

impl OwnedSession {
    /// Whether this session may be handed back. A window whose original is
    /// unknowable is not a window that can be put back.
    fn releasable(&self) -> bool {
        self.blocked.is_empty()
    }
}

impl WindowSizing {
    pub(crate) fn owns(&self, session: &str) -> bool {
        self.owned.contains_key(session)
    }

    /// Adopt the displayed windows for one session under this process's
    /// reversible sizing ownership.
    pub(crate) async fn adopt(
        &mut self,
        client: &ControlClient,
        session: &str,
        tabs: &[TabModel],
        home: &std::path::Path,
    ) -> Adopted {
        adopt_windows(self, client, session, tabs, home).await
    }

    /// Resize every window this process can still prove it owns.
    pub(crate) async fn resize_owned(
        &self,
        client: &ControlClient,
        canvas: Rect,
        tabs: &[TabModel],
        home: &std::path::Path,
    ) -> SizingOutcome {
        size_owned_windows(self, client, canvas, tabs, home).await
    }

    /// Whether the authoritative tmux layout no longer matches the size
    /// this owner would declare for at least one pinned window.
    pub(crate) fn any_window_diverged(&self, canvas: Rect, tabs: &[TabModel]) -> bool {
        self.owned.values().any(|owned| {
            owned.pinned.iter().any(|window_id| {
                if let Some(tab) = tabs.iter().find(|tab| &tab.window_id == window_id) {
                    let target = crate::render::window_target_size_for_layout(
                        canvas,
                        &tab.layout,
                        tab.zoomed,
                    );
                    let rect = tab.layout.rect();
                    rect.width != target.0 || rect.height != target.1
                } else {
                    true
                }
            })
        })
    }

    /// Reconcile deliberate one-row panes after a window resize, but only
    /// while the exact current control identity still owns the session.
    pub(crate) async fn recover_geometry(
        &self,
        client: &ControlClient,
        home: &std::path::Path,
        notice: Option<&mut NoticeState>,
    ) -> Result<bool, TmuxError> {
        recover_post_resize_geometry(self, client, home, notice).await
    }

    /// Carry proven ownership from a dead control connection to its
    /// replacement without stealing a session another workspace won.
    pub(crate) async fn reconnect(&mut self, client: &ControlClient, home: &std::path::Path) {
        rekey_ownership(self, client, home).await;
    }

    /// Restore every owned window before releasing its session marker.
    pub(crate) async fn hand_back(&mut self, client: &ControlClient, home: &std::path::Path) {
        restore_owned_sizing(self, client, home).await;
    }

    pub(crate) fn has_window_authority(&self, session: &str, window_id: &str) -> bool {
        self.owned
            .get(session)
            .is_some_and(|o| o.pinned.contains(window_id))
    }
}

/// This connection's identity, read once and remembered.
async fn sizing_identity(
    sizing: &mut WindowSizing,
    client: &ControlClient,
    home: &std::path::Path,
) -> Option<ClientIdentity> {
    if let Some(identity) = &sizing.identity {
        return Some(identity.clone());
    }
    match client.client_identity().await {
        Ok(identity) => {
            sizing.identity = Some(identity.clone());
            Some(identity)
        }
        Err(error) => {
            log_err(home, &error);
            None
        }
    }
}

/// Whether this workspace sizes `session`, claiming it when nobody live
/// does.
///
/// Fails closed everywhere: an unreadable mark, an unreadable client list,
/// or a lost race all answer false, and a workspace that answers false
/// writes no sizes at all. The cost of a wrong false is that a session
/// keeps the size it already had; the cost of a wrong true is two
/// workspaces fighting over every pane in it.
pub(crate) async fn owns_session(
    sizing: &mut WindowSizing,
    client: &ControlClient,
    session: &str,
    home: &std::path::Path,
) -> bool {
    if sizing.owns(session) {
        return true;
    }
    let Some(identity) = sizing_identity(sizing, client, home).await else {
        return false;
    };
    let marker = identity.marker();
    let held = match client.window_driver(session).await {
        Ok(held) => held,
        Err(error) => {
            log_err(home, &error);
            return false;
        }
    };
    let won = match held {
        // Nobody has it. The claim is create-only, so a race is decided by
        // tmux rather than by who read first.
        None => client.claim_window_driver(session, &marker).await,
        // Already ours: a reconcile after we claimed, not a new election.
        Some(held) if held == marker => Ok(true),
        Some(held) => {
            // Server-wide, never this session's client list. An owner that
            // navigated to another session is absent from this session's
            // list while still alive and still sizing these windows
            // (F76, M12); testing liveness there would steal the session
            // out from under a live workspace.
            let live = match client.server_client_markers().await {
                Ok(live) => live,
                Err(error) => {
                    log_err(home, &error);
                    return false;
                }
            };
            if live.contains(&held) {
                // A live owner. Follow it, and say so once.
                sizing.following.insert(session.to_string());
                return false;
            }
            client
                .take_over_window_driver(session, &held, &marker)
                .await
        }
    };
    match won {
        Ok(true) => {
            sizing.following.remove(session);
            sizing.owned.entry(session.to_string()).or_default();
            true
        }
        Ok(false) => {
            sizing.following.insert(session.to_string());
            false
        }
        Err(error) => {
            log_err(home, &error);
            false
        }
    }
}

/// Record what each displayed window's sizing policy was, then take it off
/// every policy so only this workspace moves it.
///
/// The order is the whole point and it is not an implementation detail: a
/// capture without a pin restores to what is already there, while a pin
/// without a capture loses the window's original policy permanently. A
/// window that fails stays unowned so the next reconcile retries it.
/// What one adoption pass changed, for the caller that has to react to it.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Adopted {
    /// This call was the one that found another workspace owns the session,
    /// so exactly one notice is shown for it.
    pub(crate) newly_following: bool,
    /// At least one window was pinned that was not pinned before, so it is
    /// carrying whatever size it had rather than this workspace's canvas.
    pub(crate) took_a_window: bool,
    /// True when this client transitioned from follower to authoritative sizing owner.
    pub(crate) authority_transferred: bool,
}

/// Take ownership of a session's displayed windows: record what each one's
/// sizing policy was, then take it off every policy so only this workspace
/// moves it.
pub(crate) async fn adopt_windows(
    sizing: &mut WindowSizing,
    client: &ControlClient,
    session: &str,
    tabs: &[TabModel],
    home: &std::path::Path,
) -> Adopted {
    let followed_before = sizing.following.contains(session);
    if !owns_session(sizing, client, session, home).await {
        return Adopted {
            newly_following: !followed_before && sizing.following.contains(session),
            took_a_window: false,
            authority_transferred: false,
        };
    }
    // A window that has been closed is not owned any more, and there is
    // nothing left to restore on it. Dropping it here keeps the exit path
    // from asking tmux about windows that no longer exist. Re-adopting one
    // that only looked absent is safe: the capture is create-only, so its
    // original survives a second pass.
    let displayed: BTreeSet<String> = tabs.iter().map(|tab| tab.window_id.clone()).collect();
    if let Some(owned) = sizing.owned.get_mut(session) {
        owned
            .pinned
            .retain(|window_id| displayed.contains(window_id));
        owned
            .blocked
            .retain(|window_id| displayed.contains(window_id));
    }
    let owned = sizing.owned.entry(session.to_string()).or_default();
    // Blocked windows are deliberately not excluded here. They are cheap to
    // re-read, this workspace never pinned them, and if the record they
    // carry is ever repaired the next pass adopts them properly instead of
    // ignoring them for the life of the process.
    let fresh: Vec<String> = unpinned_windows(tabs, &owned.pinned)
        .into_iter()
        .map(str::to_string)
        .collect();
    let mut took_a_window = false;
    for window_id in fresh {
        match client.capture_prior_window_size(&window_id).await {
            Ok(cyclops_tmux::Captured::Record(_)) => {}
            Ok(cyclops_tmux::Captured::Malformed) => {
                // Not pinned, not written to, and not forgotten. Forgetting
                // it is what used to release the session's mark over a
                // window that was already pinned and unreadable, which is
                // the one state nothing recovers from.
                let owned = sizing.owned.entry(session.to_string()).or_default();
                if owned.blocked.insert(window_id.clone()) {
                    log_err(
                        home,
                        &format!(
                            "{window_id}: sizing record unreadable, so this workspace will not \
                             size it and will not release {session}. Inspect it with: tmux \
                             show-options -w -t {window_id} @cyclops_prior_window_size"
                        ),
                    );
                }
                continue;
            }
            Err(error) => {
                log_err(home, &error);
                continue;
            }
        }
        match client.pin_window_size_manual(&window_id).await {
            Ok(()) => {
                sizing
                    .owned
                    .entry(session.to_string())
                    .or_default()
                    .pinned
                    .insert(window_id);
                took_a_window = true;
            }
            Err(error) => log_err(home, &error),
        }
    }
    Adopted {
        newly_following: false,
        took_a_window,
        authority_transferred: followed_before,
    }
}

/// Result of resizing execution across pinned windows in a session.
#[derive(Debug, Default)]
pub(crate) struct SizingOutcome {
    pub succeeded: BTreeSet<String>,
    pub failed: BTreeMap<String, cyclops_tmux::TmuxError>,
}

impl SizingOutcome {
    pub fn all_succeeded(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Push per-window topology-derived target sizes to every window this workspace owns,
/// in every session it owns. Returns exact per-window successes and failures.
pub(crate) async fn size_owned_windows(
    sizing: &WindowSizing,
    client: &ControlClient,
    canvas: Rect,
    tabs: &[TabModel],
    home: &std::path::Path,
) -> SizingOutcome {
    let mut outcome = SizingOutcome::default();
    for owned in sizing.owned.values() {
        for window_id in &owned.pinned {
            let target_size = if let Some(tab) = tabs.iter().find(|t| &t.window_id == window_id) {
                crate::render::window_target_size_for_layout(canvas, &tab.layout, tab.zoomed)
            } else {
                let inner = crate::render::pane_canvas(canvas);
                (inner.width, inner.height)
            };

            if !declarable(target_size) {
                continue;
            }

            match client
                .resize_window(window_id, target_size.0, target_size.1)
                .await
            {
                Ok(()) => {
                    outcome.succeeded.insert(window_id.clone());
                }
                Err(error) => {
                    log_err(home, &error);
                    outcome.failed.insert(window_id.clone(), error);
                }
            }
        }
    }
    outcome
}

/// Put every window this workspace pinned back on the policy it was found
/// with, then stop owning its sessions.
///
/// Restores before releasing, in that order: a marker cleared first would
/// let another workspace claim the session and adopt windows that still
/// carry this one's pin, which is how a `manual` nobody chose becomes
/// permanent.
pub(crate) async fn restore_owned_sizing(
    sizing: &mut WindowSizing,
    client: &ControlClient,
    home: &std::path::Path,
) {
    let Some(marker) = sizing.identity.as_ref().map(ClientIdentity::marker) else {
        // No identity means nothing can be proved to be ours, and putting
        // windows back on a guess would undo whoever does own them.
        sizing.owned.clear();
        return;
    };
    for (session, owned) in std::mem::take(&mut sizing.owned) {
        // Ownership is re-checked here, not assumed from the map. A
        // workspace can lose a session between claiming it and quitting:
        // its link dropped, a follower found the mark stale and took over,
        // and that follower is now the one those windows belong to.
        // Restoring them here would take a live workspace's session out
        // from under it, so the exact marker has to still be this one's.
        match client.window_driver(&session).await {
            Ok(Some(held)) if held == marker => {}
            Ok(_) => continue,
            Err(error) => {
                log_err(home, &error);
                continue;
            }
        }
        // Whether this session was fully handed back. It starts false when a
        // window here carries a record nobody can read, since such a window
        // was never pinned by this workspace and is exactly why the session
        // may not be released.
        let mut handed_back = owned.releasable();
        for window_id in &owned.pinned {
            match client.restore_window_size(window_id).await {
                Ok(cyclops_tmux::Restored::Malformed) => {
                    // The record of what this window was cannot be read, so
                    // the original policy is unknowable. Nothing was
                    // changed, and nothing here will change it: choosing a
                    // policy would invent state the operator never set, and
                    // clearing the record would destroy the only evidence
                    // of what the window originally was. The window stays
                    // pinned and this workspace stays its owner, which is
                    // visibly wrong and fully recoverable.
                    handed_back = false;
                    log_err(
                        home,
                        &format!(
                            "{window_id}: sizing record unreadable, so the original policy is \
                             unknown. The window is left on manual and still owned. Inspect it \
                             with: tmux show-options -w -t {window_id} @cyclops_prior_window_size"
                        ),
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    // A restore that failed leaves the window exactly as
                    // this workspace pinned it: on `manual`, with its record
                    // still attached. Releasing the mark over that is the
                    // same orphaning as the unreadable case, reached through
                    // a transient tmux failure instead: no policy applies,
                    // no client can resize it, and no owner is named for
                    // anyone to blame. The link may be down or the command
                    // may have timed out, and either way this session was
                    // not handed back.
                    handed_back = false;
                    log_err(home, &error);
                }
            }
        }
        if !handed_back {
            // Keeping the mark is the point: a pinned window with no owner
            // is the one state nothing can recover from on its own.
            continue;
        }
        if let Err(error) = client.release_window_driver(&session).await {
            log_err(home, &error);
        }
    }
}

/// Move every session this workspace owns onto the identity of a new
/// connection.
///
/// A reconnect replaces the tmux client, so `client_name:client_created`
/// changes while the process lives on. The marks left behind name a client
/// that no longer exists, which is exactly what a follower watches for, so
/// this is a race with a real other party rather than bookkeeping: between
/// the old client dying and this running, a follower may have taken a
/// session legitimately.
///
/// Each session is therefore moved with one compare-and-set from the exact
/// old marker to the exact new one, and ownership is kept only where that
/// won. A session lost in the gap is dropped from the map entirely, so
/// nothing here resizes it and the exit path will not put it back: it
/// belongs to the workspace that won it.
pub(crate) async fn rekey_ownership(
    sizing: &mut WindowSizing,
    client: &ControlClient,
    home: &std::path::Path,
) {
    sizing.following.clear();
    let Some(previous) = sizing.identity.take() else {
        // Nothing was ever claimed under a proven identity.
        sizing.owned.clear();
        return;
    };
    let stale = previous.marker();
    let Some(identity) = sizing_identity(sizing, client, home).await else {
        // Without a new identity this workspace cannot prove it owns
        // anything, so it claims nothing rather than writing sizes it
        // cannot defend.
        sizing.owned.clear();
        return;
    };
    let marker = identity.marker();
    for session in sizing.owned.keys().cloned().collect::<Vec<_>>() {
        let kept = client
            .take_over_window_driver(&session, &stale, &marker)
            .await;
        match kept {
            Ok(true) => {}
            Ok(false) => {
                sizing.owned.remove(&session);
            }
            Err(error) => {
                log_err(home, &error);
                sizing.owned.remove(&session);
            }
        }
    }
}

/// Shared post-resize recovery helper:
/// 1. Reconciles every exact successfully resized window in every owned session.
/// 2. Always fetches a fresh post-resize snapshot before any pane decision.
/// 3. For each window in owned[session].pinned, revalidates live driver marker before mutating.
/// 4. Panes with deliberate minimization provenance (`Minimized { original_height }`)
///    must remain collapsed at 1 row after tmux automatic reflow on window resize.
/// 5. Panes with `None` provenance that are 1-row high fail closed: they are NOT modified
///    (manual resize is preserved; no auto-uncrush of unknown intent) and surface an explicit banner.
/// 6. Panes with malformed provenance (`Malformed(bad)`) fail closed: surface visible notice,
///    log error, leave option evidence untouched.
/// 7. Fails visibly on errors and returns them so the application can retain
///    its explicit reconciliation retry state.
async fn recover_post_resize_geometry(
    sizing: &WindowSizing,
    client: &ControlClient,
    home: &std::path::Path,
    mut notice: Option<&mut NoticeState>,
) -> Result<bool, TmuxError> {
    let identity = client.client_identity().await?;
    let my_marker = identity.marker();

    let owned_sessions: Vec<(String, Vec<String>)> = sizing
        .owned
        .iter()
        .map(|(sess, owned)| (sess.clone(), owned.pinned.iter().cloned().collect()))
        .collect();

    if owned_sessions.is_empty() {
        return Ok(false);
    }

    let snapshot = client.workspace_snapshot().await?;
    let mut any_modified = false;

    for (session, pinned_windows) in owned_sessions {
        let current_driver = client.window_driver(&session).await?;
        if current_driver.as_ref() != Some(&my_marker) {
            continue;
        }

        let Some(snap_session) = snapshot.sessions.iter().find(|s| s.name == session) else {
            continue;
        };

        for window_id in pinned_windows {
            let Some(snap_window) = snap_session.windows.iter().find(|w| w.id == window_id) else {
                continue;
            };

            for pane in &snap_window.panes {
                match &pane.minimization {
                    cyclops_tmux::PaneMinimizationProvenance::Minimized { .. } => {
                        if pane.height > crate::render::MINIMIZED_ROWS as u32 {
                            if let Err(e) = client
                                .resize_pane_height(&pane.id, crate::render::MINIMIZED_ROWS)
                                .await
                            {
                                log_err(
                                    home,
                                    &format!(
                                        "failed to re-collapse minimized pane {}: {e}",
                                        pane.id
                                    ),
                                );
                                if let Some(ref mut n) = notice {
                                    n.show(
                                        format!("error: failed to re-collapse pane {}", pane.id),
                                        Instant::now(),
                                    );
                                }
                                return Err(e);
                            }
                            any_modified = true;
                        }
                    }
                    cyclops_tmux::PaneMinimizationProvenance::Malformed(bad) => {
                        if pane.height <= crate::render::MINIMIZED_ROWS as u32 {
                            log_err(
                                home,
                                &format!(
                                    "{}: malformed minimization provenance ({bad}), refusing recovery",
                                    pane.id
                                ),
                            );
                            if let Some(ref mut n) = notice {
                                n.show(
                                    format!(
                                        "warning: pane {} has malformed minimization record ({bad}); manual recovery required",
                                        pane.id
                                    ),
                                    Instant::now(),
                                );
                            }
                        }
                    }
                    cyclops_tmux::PaneMinimizationProvenance::None => {
                        if pane.height <= crate::render::MINIMIZED_ROWS as u32 {
                            // Fail closed on unknown intent: do not uncrush without positive provenance.
                            if let Some(ref mut n) = notice {
                                n.show(
                                    format!(
                                        "pane {} is 1 row high (unknown provenance); manual resize required to uncrush",
                                        pane.id
                                    ),
                                    Instant::now(),
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(any_modified)
}

#[cfg(test)]
mod architecture_tests {
    #[test]
    fn the_workspace_loop_does_not_own_the_sizing_transition_protocol() {
        let app = include_str!("app.rs")
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .expect("production app source");

        for adapter_transition in [
            ".client_identity(",
            ".window_driver(",
            ".server_client_markers(",
            ".claim_window_driver(",
            ".take_over_window_driver(",
            ".capture_prior_window_size(",
            ".pin_window_size_manual(",
            ".restore_window_size(",
            ".release_window_driver(",
            ".resize_pane_height(",
        ] {
            assert!(
                !app.contains(adapter_transition),
                "workspace loop recovered sizing transition knowledge through {adapter_transition}"
            );
        }

        for owned_operation in [
            ".adopt(",
            ".resize_owned(",
            ".any_window_diverged(",
            ".recover_geometry(",
            ".reconnect(",
            ".hand_back(",
        ] {
            assert!(
                app.contains(owned_operation),
                "workspace loop stopped delegating {owned_operation} to sizing ownership"
            );
        }
    }
}
