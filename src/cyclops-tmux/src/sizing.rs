//! Who may size a session's windows, and how what they changed is put back.
//!
//! ## Why this is not `refresh-client -C`
//!
//! A client size declaration is a vote. Under `window-size smallest` tmux
//! takes the minimum over every client that has declared one, and a tty
//! client always declares: it cannot abstain. MEASURED (F76): an elected
//! driver declared 176x47 and the window became 176x47, and then one plain
//! `tmux attach` from a 62x21 terminal collapsed that window, and every pane
//! in it, to 62x19. A window's size is its panes' size, so that is not a
//! viewer's cosmetic problem, it is every agent in the session reflowing
//! its TUI and rewrapping its scrollback because somebody opened a small
//! terminal.
//!
//! `window-size manual` plus `resize-window` has no vote to lose. MEASURED
//! on the same rig: the window held 176x47 with that 62x21 client attached,
//! and still held after it left.
//!
//! ## What that costs, and what pays for it
//!
//! A declared size dies with the client that declared it. A manual size is
//! window state and outlives its owner, so this module has to do by hand
//! what tmux was doing for free:
//!
//! - **Ownership.** One workspace per session sizes it. The rest follow and
//!   never write. [`WINDOW_DRIVER_OPTION`] names the owner, claimed
//!   create-only so the first writer wins, and taken over from a dead owner
//!   with one atomic compare-and-set.
//! - **The original.** Before a window is pinned, what its `window-size`
//!   was is recorded in [`PRIOR_WINDOW_SIZE_OPTION`], also create-only, so
//!   a later owner restores the true original rather than its dead
//!   predecessor's `manual`. The capture lives in the tmux server, not in
//!   the workspace, precisely because the workspace is the thing that can
//!   crash.
//!
//! Both are per window, because `window-size` is per window.

use crate::control::ControlClient;
use crate::error::TmuxError;
use crate::quote::quote_arg;

/// Session option naming the client that sizes this session's windows.
pub const WINDOW_DRIVER_OPTION: &str = "@cyclops_window_driver";

/// Window option holding what `window-size` was before Cyclops pinned it.
pub const PRIOR_WINDOW_SIZE_OPTION: &str = "@cyclops_prior_window_size";

/// One tmux client, named so that the name cannot come back.
///
/// `client_name` alone is not enough. MEASURED (F76): for a control client
/// with no tty, `list-clients` reports `name=[client-29090] pid=[29090]`, so
/// the name IS the pid formatted, and pids are reused. `client_created` is a
/// per-connection timestamp, and a recycled pid cannot bring the old
/// creation time with it, so the pair is stable where either half alone is
/// not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIdentity {
    pub name: String,
    pub created: String,
}

impl ClientIdentity {
    /// The exact string written into [`WINDOW_DRIVER_OPTION`].
    pub fn marker(&self) -> String {
        format!("{}:{}", self.name, self.created)
    }

    /// Read a marker back. Split from the right because a tty client's name
    /// is a device path (`/dev/ttys010`), which holds no colon, while the
    /// creation stamp is always the last field.
    pub fn parse(raw: &str) -> Option<ClientIdentity> {
        let (name, created) = raw.trim().rsplit_once(':')?;
        (!name.is_empty() && !created.is_empty()).then(|| ClientIdentity {
            name: name.to_string(),
            created: created.to_string(),
        })
    }
}

/// What a window's `window-size` was before Cyclops pinned it to `manual`.
///
/// The two cases are genuinely different and collapsing them corrupts the
/// user's configuration. MEASURED (F76): `show-options -w window-size`
/// prints nothing at all when the value is inherited, and the global
/// default is `latest`, not `smallest`. So a window that inherited must be
/// restored by *unsetting*, and restoring it by setting any value, even the
/// right one, leaves it carrying an explicit option it never had, which
/// silently stops it following later changes to the session or global
/// option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PriorWindowSize {
    /// No `window-size` of its own; it took the session or global value.
    Inherited,
    /// Its own value, to be put back exactly.
    Explicit(String),
}

impl PriorWindowSize {
    /// Encoded for the tmux option. Sentinels rather than an empty string,
    /// so the meaning does not depend on quoting surviving a round trip.
    pub fn encode(&self) -> String {
        match self {
            PriorWindowSize::Inherited => "inherited".to_string(),
            PriorWindowSize::Explicit(value) => format!("explicit:{value}"),
        }
    }

    /// Read a capture back, or `None` for anything this module did not
    /// write. An unreadable capture is never guessed at: the caller leaves
    /// the window alone rather than restoring an invented policy.
    pub fn parse(raw: &str) -> Option<PriorWindowSize> {
        let raw = raw.trim();
        if raw == "inherited" {
            return Some(PriorWindowSize::Inherited);
        }
        raw.strip_prefix("explicit:")
            .filter(|value| !value.is_empty())
            .map(|value| PriorWindowSize::Explicit(value.to_string()))
    }
}

/// What one window's restore actually managed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restored {
    /// No capture on the window: it was never pinned by Cyclops, or it has
    /// already been put back. Not an error, and the reason recovery can be
    /// run twice.
    Nothing,
    /// The capture was read and the exact policy it recorded is back.
    Exactly,
    /// A capture exists that this version cannot read, so the original
    /// policy is unknowable.
    ///
    /// **Nothing was changed.** Not the pin, not the capture, and the
    /// caller must not release ownership either. Guessing a policy would
    /// invent user state, and unsetting the capture would destroy the only
    /// evidence of what the window originally was. A window that stays
    /// pinned and owned is visibly wrong and fully recoverable; a window
    /// silently returned to a policy nobody chose is neither.
    Malformed,
}

/// What one capture attempt found, or established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Captured {
    /// The record in force: either the one already there, or the one this
    /// call just wrote.
    Record(PriorWindowSize),
    /// A record exists that this version cannot read, so the window's
    /// original policy is unknowable.
    ///
    /// Nothing was written and nothing was changed. A window in this state
    /// must not be pinned, because pinning it would make this workspace
    /// responsible for putting back something it cannot know; and it must
    /// still keep its session owned, because releasing the mark over it is
    /// how a pinned window ends up with no owner at all.
    Malformed,
}

impl ControlClient {
    /// This client's own identity, for claiming and for recognising its own
    /// mark later.
    pub async fn client_identity(&self) -> Result<ClientIdentity, TmuxError> {
        let lines = self
            .command(&format!(
                "display-message -p {}",
                quote_arg("#{client_name}:#{client_created}")
            ))
            .await?;
        lines
            .first()
            .map(String::as_str)
            .and_then(ClientIdentity::parse)
            .ok_or_else(|| TmuxError::Protocol("tmux named no client for this connection".into()))
    }

    /// The marker of whoever owns sizing for `session`, or `None`.
    ///
    /// The bare session name, not the `=name` exact form the rest of this
    /// crate sends: MEASURED on tmux 3.6a, `set-option -t =name` and
    /// `show-options -t =name` answer "no such session: =name" while
    /// `list-clients -t =name` resolves it.
    pub async fn window_driver(&self, session: &str) -> Result<Option<String>, TmuxError> {
        let lines = self
            .command(&format!(
                "show-options -t {} -qv {WINDOW_DRIVER_OPTION}",
                quote_arg(session)
            ))
            .await?;
        Ok(lines
            .first()
            .map(|line| line.trim().to_string())
            .filter(|value| !value.is_empty()))
    }

    /// Claim sizing for `session`, and answer whether this client got it.
    ///
    /// Create-only, so two workspaces booting together cannot both believe
    /// they won: tmux refuses the second write. The refusal arrives as a
    /// command error, which is why it is dropped here rather than
    /// propagated. The readback is the only authority, and it fails closed:
    /// if the mark cannot be read, this client is not the owner and will
    /// not write a size.
    pub async fn claim_window_driver(
        &self,
        session: &str,
        marker: &str,
    ) -> Result<bool, TmuxError> {
        let _refused_if_already_owned = self
            .command(&format!(
                "set-option -o -t {} {WINDOW_DRIVER_OPTION} {}",
                quote_arg(session),
                quote_arg(marker)
            ))
            .await;
        Ok(self.window_driver(session).await?.as_deref() == Some(marker))
    }

    /// Take sizing for `session` from an owner that is gone, and answer
    /// whether this client got it.
    ///
    /// `if-shell -F` is evaluated and branched by the tmux server itself,
    /// which is single threaded, so the compare and the set cannot be
    /// interleaved by another workspace doing the same thing. Two followers
    /// racing to replace the same dead owner therefore produce one winner:
    /// the second one's condition no longer matches the marker it read.
    pub async fn take_over_window_driver(
        &self,
        session: &str,
        stale_marker: &str,
        marker: &str,
    ) -> Result<bool, TmuxError> {
        let condition = format!("#{{==:#{{{WINDOW_DRIVER_OPTION}}},{stale_marker}}}");
        let set = format!(
            "set-option -t {} {WINDOW_DRIVER_OPTION} {}",
            quote_arg(session),
            quote_arg(marker)
        );
        self.command(&format!(
            "if-shell -t {} -F {} {}",
            quote_arg(session),
            quote_arg(&condition),
            quote_arg(&set)
        ))
        .await?;
        Ok(self.window_driver(session).await?.as_deref() == Some(marker))
    }

    /// Give up sizing for `session`. Part of a clean exit, after the windows
    /// it owned have been restored.
    pub async fn release_window_driver(&self, session: &str) -> Result<(), TmuxError> {
        self.command(&format!(
            "set-option -t {} -u {WINDOW_DRIVER_OPTION}",
            quote_arg(session)
        ))
        .await?;
        Ok(())
    }

    /// Every client on the server, by marker. **This is the liveness test.**
    ///
    /// Whether an owner is still alive is a question about the server, not
    /// about a session, and getting that wrong is not a small bug. MEASURED
    /// (F76, M12): a workspace that claims session A and then navigates to
    /// session B keeps one connection and one identity, and
    /// `list-clients -t =A` then returns NOTHING while
    /// `list-clients` still lists it. A follower testing liveness against
    /// A's client list would read a live owner as dead, steal the session,
    /// and start sizing windows the real owner is still sizing. Two writers
    /// on one window is the collapse this whole module exists to prevent.
    ///
    /// A dead client is absent here too, so the reconnect-gap takeover this
    /// also backs stays correct.
    pub async fn server_client_markers(&self) -> Result<Vec<String>, TmuxError> {
        let lines = self
            .command(&format!(
                "list-clients -F {}",
                quote_arg("#{client_name}:#{client_created}")
            ))
            .await?;
        Ok(lines
            .iter()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect())
    }

    /// Every client currently *showing* `session`, by marker.
    ///
    /// Not a liveness test, and never to be used as one: see
    /// [`Self::server_client_markers`] for why an owner that navigated away
    /// is missing from here while being perfectly alive. This answers
    /// "who is looking at this session", which is a different question.
    pub async fn session_client_markers(&self, session: &str) -> Result<Vec<String>, TmuxError> {
        let lines = self
            .command(&format!(
                "list-clients -t {} -F {}",
                quote_arg(&format!("={session}")),
                quote_arg("#{client_name}:#{client_created}")
            ))
            .await?;
        Ok(lines
            .iter()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect())
    }

    /// Record what `window_id`'s `window-size` is now, before anything pins
    /// it, and answer the capture that is in force.
    ///
    /// Create-only, so the answer may be somebody else's earlier capture,
    /// and that is the point: if a previous owner crashed with the window
    /// left at `manual`, the honest original is the one that owner wrote,
    /// not the `manual` this one would otherwise observe and preserve
    /// forever.
    ///
    /// Call this before [`Self::pin_window_size_manual`], never after. A
    /// capture without a pin is harmless, since restoring it puts back what
    /// is already there. A pin without a capture loses the original for
    /// good.
    pub async fn capture_prior_window_size(&self, window_id: &str) -> Result<Captured, TmuxError> {
        // Read the raw record, not the parsed one. A record that exists and
        // cannot be read is a different answer from no record at all, and
        // collapsing them is how a malformed record used to fall through to
        // a write tmux then refused, leaving the caller with an error, the
        // window unowned, and the session released out from under a pin.
        if let Some(raw) = self.raw_prior_window_size(window_id).await? {
            return Ok(match PriorWindowSize::parse(&raw) {
                Some(prior) => Captured::Record(prior),
                None => Captured::Malformed,
            });
        }
        let current = self
            .command(&format!(
                "show-options -w -t {} -qv window-size",
                quote_arg(window_id)
            ))
            .await?
            .first()
            .map(|line| line.trim().to_string())
            .unwrap_or_default();
        let prior = if current.is_empty() {
            PriorWindowSize::Inherited
        } else {
            PriorWindowSize::Explicit(current)
        };
        let _refused_if_already_captured = self
            .command(&format!(
                "set-option -w -o -t {} {PRIOR_WINDOW_SIZE_OPTION} {}",
                quote_arg(window_id),
                quote_arg(&prior.encode())
            ))
            .await;
        // Read back rather than trust the write: create-only means somebody
        // else may have won, and what is in force is what matters.
        match self.raw_prior_window_size(window_id).await? {
            Some(raw) => Ok(match PriorWindowSize::parse(&raw) {
                Some(prior) => Captured::Record(prior),
                None => Captured::Malformed,
            }),
            None => Err(TmuxError::Protocol(
                "the window capture did not survive its write".into(),
            )),
        }
    }

    /// The capture in force for `window_id`, or `None` if nothing readable
    /// was ever written.
    pub async fn prior_window_size(
        &self,
        window_id: &str,
    ) -> Result<Option<PriorWindowSize>, TmuxError> {
        let lines = self
            .command(&format!(
                "show-options -w -t {} -qv {PRIOR_WINDOW_SIZE_OPTION}",
                quote_arg(window_id)
            ))
            .await?;
        Ok(lines
            .first()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty())
            .and_then(PriorWindowSize::parse))
    }

    /// Take `window_id` off every sizing policy, so only an explicit
    /// `resize-window` moves it and no attaching client can.
    pub async fn pin_window_size_manual(&self, window_id: &str) -> Result<(), TmuxError> {
        self.command(&format!(
            "set-option -w -t {} window-size manual",
            quote_arg(window_id)
        ))
        .await?;
        Ok(())
    }

    /// Set `window_id`'s size.
    ///
    /// MEASURED (F76): this needs no attachment to the session the window is
    /// in. One control client attached only to session B resized session A's
    /// window, which is what lets one workspace keep sizing the sessions it
    /// owns while it is looking at another one.
    pub async fn resize_window(
        &self,
        window_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<(), TmuxError> {
        self.command(&format!(
            "resize-window -t {} -x {cols} -y {rows}",
            quote_arg(window_id)
        ))
        .await?;
        Ok(())
    }

    /// Put `window_id`'s sizing policy back the way it was found and drop
    /// the capture, answering what it managed to do.
    ///
    /// Idempotent by construction: the capture is consumed, so a second call
    /// finds nothing and reports `false` rather than failing. That matters
    /// because both a clean exit and a later owner's recovery can reach the
    /// same window.
    ///
    /// This restores the policy, not the pixels. MEASURED (F76): unsetting
    /// `window-size` leaves the window at the size it currently has; tmux
    /// recomputes it the next time the set of attached clients changes,
    /// which is exactly when a policy has anything to say.
    pub async fn restore_window_size(&self, window_id: &str) -> Result<Restored, TmuxError> {
        let raw = self.raw_prior_window_size(window_id).await?;
        match raw.as_deref().map(PriorWindowSize::parse) {
            None => return Ok(Restored::Nothing),
            // Read but not understood. Every write below is skipped on
            // purpose, including the one that clears the capture: it is the
            // evidence, and a caller that cannot restore a window must also
            // not stop owning it.
            Some(None) => return Ok(Restored::Malformed),
            Some(Some(PriorWindowSize::Inherited)) => {
                self.command(&format!(
                    "set-option -w -t {} -u window-size",
                    quote_arg(window_id)
                ))
                .await?;
            }
            Some(Some(PriorWindowSize::Explicit(value))) => {
                self.command(&format!(
                    "set-option -w -t {} window-size {}",
                    quote_arg(window_id),
                    quote_arg(&value)
                ))
                .await?;
            }
        };
        self.command(&format!(
            "set-option -w -t {} -u {PRIOR_WINDOW_SIZE_OPTION}",
            quote_arg(window_id)
        ))
        .await?;
        Ok(Restored::Exactly)
    }

    /// The capture exactly as tmux holds it, before any parsing. Separate
    /// from [`Self::prior_window_size`] because "there is no capture" and
    /// "there is a capture nobody can read" call for different handling,
    /// and collapsing them is how a window ends up pinned with its owner
    /// released.
    async fn raw_prior_window_size(&self, window_id: &str) -> Result<Option<String>, TmuxError> {
        let lines = self
            .command(&format!(
                "show-options -w -t {} -qv {PRIOR_WINDOW_SIZE_OPTION}",
                quote_arg(window_id)
            ))
            .await?;
        Ok(lines
            .first()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty()))
    }
}

/// The answer to one operator-driven release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseOutcome {
    /// The session's mark names a client the server still has, so nothing
    /// was read and nothing was written. Recovery is for a workspace that
    /// is gone; running it under one that is not would fight it.
    RefusedLiveOwner { marker: String },
    /// What each window's release did.
    Released(Vec<ReleasedWindow>),
}

/// What an operator-driven release did to one window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasedWindow {
    pub window_id: String,
    pub outcome: Restored,
}

/// Undo Cyclops window sizing across a session, with no workspace running.
///
/// This is the operator's way out, and it exists because the failure it
/// answers is real and otherwise permanent: a workspace killed hard leaves
/// its windows on `manual` and its mark naming a client that is gone, and
/// while any later workspace repairs that, an operator who is finished with
/// Cyclops has no workspace coming. Without this they would have to know
/// the option names themselves.
///
/// One-shot tmux commands rather than a control connection, because the
/// caller is a CLI invocation and there is no session to attach to.
///
/// Deliberately conservative: a window with no Cyclops capture is left
/// exactly as it is, `manual` included, because Cyclops did not put it
/// there and an operator may have. Idempotent for the same reason a
/// workspace's own restore is: the capture is consumed.
pub fn release_session_sizing(
    session: &str,
    socket: Option<&str>,
) -> Result<ReleaseOutcome, TmuxError> {
    // Liveness first, and server-wide, before anything is read or written.
    // A marker naming a client the server still has is a workspace that is
    // running, holds this session in memory, and will keep issuing
    // `resize-window` for it. Restoring underneath it would fight a live
    // owner and leave the two disagreeing about what it owns, so this
    // refuses instead. Server-wide because an owner displaying another
    // session is absent from this one's client list while perfectly alive
    // (F76, M12).
    let held = crate::cmd::run(
        socket,
        None,
        &["show-options", "-t", session, "-qv", WINDOW_DRIVER_OPTION],
    )?;
    let held = held.trim().to_string();
    if !held.is_empty() {
        let clients = crate::cmd::run(
            socket,
            None,
            &["list-clients", "-F", "#{client_name}:#{client_created}"],
        )?;
        if clients.lines().map(str::trim).any(|line| line == held) {
            return Ok(ReleaseOutcome::RefusedLiveOwner { marker: held });
        }
    }
    let target = crate::session_target(session);
    let listed = crate::cmd::run(
        socket,
        None,
        &["list-windows", "-t", &target, "-F", "#{window_id}"],
    )?;
    let mut released = Vec::new();
    for window_id in listed.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let raw = crate::cmd::run(
            socket,
            None,
            &[
                "show-options",
                "-w",
                "-t",
                window_id,
                "-qv",
                PRIOR_WINDOW_SIZE_OPTION,
            ],
        )?;
        let raw = raw.trim();
        if raw.is_empty() {
            released.push(ReleasedWindow {
                window_id: window_id.to_string(),
                outcome: Restored::Nothing,
            });
            continue;
        }
        let outcome = match PriorWindowSize::parse(raw) {
            Some(PriorWindowSize::Explicit(value)) => {
                crate::cmd::run(
                    socket,
                    None,
                    &["set-option", "-w", "-t", window_id, "window-size", &value],
                )?;
                Restored::Exactly
            }
            Some(PriorWindowSize::Inherited) => {
                crate::cmd::run(
                    socket,
                    None,
                    &["set-option", "-w", "-t", window_id, "-u", "window-size"],
                )?;
                Restored::Exactly
            }
            None => {
                // Unknowable, so nothing is touched and the evidence stays
                // where it is. The caller reports this and exits nonzero
                // rather than choosing a policy on the operator's behalf.
                released.push(ReleasedWindow {
                    window_id: window_id.to_string(),
                    outcome: Restored::Malformed,
                });
                continue;
            }
        };
        crate::cmd::run(
            socket,
            None,
            &[
                "set-option",
                "-w",
                "-t",
                window_id,
                "-u",
                PRIOR_WINDOW_SIZE_OPTION,
            ],
        )?;
        released.push(ReleasedWindow {
            window_id: window_id.to_string(),
            outcome,
        });
    }
    // Ownership is released only once every window it covered is actually
    // back. A session with an unreadable capture keeps its mark, because
    // dropping it would leave a pinned window with no owner and no record
    // of what it was.
    if released
        .iter()
        .all(|window| window.outcome != Restored::Malformed)
    {
        crate::cmd::run(
            socket,
            None,
            &["set-option", "-t", session, "-u", WINDOW_DRIVER_OPTION],
        )?;
    }
    Ok(ReleaseOutcome::Released(released))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The marker has to survive a round trip through a tmux option, and a
    /// tty client's name carries slashes, so the split cannot be on the
    /// first colon.
    #[test]
    fn a_marker_round_trips_for_both_kinds_of_client() {
        for identity in [
            ClientIdentity {
                name: "client-29090".into(),
                created: "1787793368".into(),
            },
            ClientIdentity {
                name: "/dev/ttys010".into(),
                created: "1787793400".into(),
            },
        ] {
            let marker = identity.marker();
            assert_eq!(ClientIdentity::parse(&marker), Some(identity));
        }
    }

    /// Nothing half-formed becomes an identity, because a bad identity would
    /// be compared against a real one and could win.
    #[test]
    fn a_malformed_marker_is_nobody() {
        for raw in ["", "client-1", ":", "client-1:", ":123"] {
            assert_eq!(ClientIdentity::parse(raw), None, "{raw:?} named a client");
        }
    }

    /// Inherited and explicit are different facts, and `explicit:` must
    /// carry values that themselves contain a colon without losing them.
    #[test]
    fn a_capture_round_trips_and_keeps_inherited_distinct() {
        for prior in [
            PriorWindowSize::Inherited,
            PriorWindowSize::Explicit("latest".into()),
            PriorWindowSize::Explicit("smallest".into()),
            PriorWindowSize::Explicit("manual".into()),
        ] {
            assert_eq!(PriorWindowSize::parse(&prior.encode()), Some(prior));
        }
        assert_ne!(
            PriorWindowSize::Inherited.encode(),
            PriorWindowSize::Explicit("latest".into()).encode()
        );
    }

    /// A capture this module did not write is not guessed at. The window is
    /// left alone rather than restored to an invented policy.
    #[test]
    fn an_unreadable_capture_restores_nothing() {
        for raw in ["", "manual", "explicit:", "inherited-ish", "latest"] {
            assert_eq!(
                PriorWindowSize::parse(raw),
                None,
                "{raw:?} became a capture"
            );
        }
    }
}
