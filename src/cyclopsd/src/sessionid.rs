//! Stable identities for live tmux session incarnations.
//!
//! Display names are mutable. Identity instead combines the workspace,
//! OS boot, tmux server process instance, and tmux session id. The
//! registry enforces a bijection between that live key and its durable id.

use std::collections::HashMap;

use cyclops_proto::{LiveSessionKey, SessionIdentityBinding, SessionInstanceId};

/// Result of resolving a live session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resolved {
    /// The existing durable identity.
    Existing(SessionInstanceId),
    /// A newly accepted durable identity.
    Minted(SessionInstanceId),
}

impl Resolved {
    #[cfg(test)]
    pub(crate) fn id(self) -> SessionInstanceId {
        match self {
            Resolved::Existing(id) | Resolved::Minted(id) => id,
        }
    }
}

/// A binding conflicts with one side of the existing bijection.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum BindError {
    #[error("live session already bound to {0}")]
    KeyTaken(SessionInstanceId),
    #[error("session instance already bound to another live session")]
    IdTaken(Box<LiveSessionKey>),
}

/// Bidirectional live-session to durable-id registry.
#[derive(Debug, Default, Clone)]
pub(crate) struct SessionIdentityRegistry {
    by_key: HashMap<LiveSessionKey, SessionInstanceId>,
    by_id: HashMap<SessionInstanceId, LiveSessionKey>,
}

impl SessionIdentityRegistry {
    pub(crate) fn new() -> SessionIdentityRegistry {
        SessionIdentityRegistry::default()
    }

    /// Return the existing id or bind one candidate id to a new live key.
    ///
    /// The caller creates the candidate so persistence can record the
    /// exact id that this method validates. Collisions fail without
    /// changing either map.
    pub(crate) fn resolve(
        &mut self,
        key: &LiveSessionKey,
        mint: impl FnOnce() -> SessionInstanceId,
    ) -> Result<Resolved, BindError> {
        if let Some(id) = self.by_key.get(key) {
            return Ok(Resolved::Existing(*id));
        }
        let id = mint();
        if let Some(bound) = self.by_id.get(&id) {
            return Err(BindError::IdTaken(Box::new(bound.clone())));
        }
        self.by_key.insert(key.clone(), id);
        self.by_id.insert(id, key.clone());
        Ok(Resolved::Minted(id))
    }

    /// Adopt a persisted binding after validating both directions.
    /// Replaying the same binding is idempotent.
    pub(crate) fn bind(&mut self, binding: &SessionIdentityBinding) -> Result<(), BindError> {
        let (key, id) = (binding.live_session_key(), binding.session_instance_id());
        match (self.by_key.get(key), self.by_id.get(&id)) {
            (Some(&bound), _) if bound != id => return Err(BindError::KeyTaken(bound)),
            (_, Some(bound)) if bound != key => {
                return Err(BindError::IdTaken(Box::new(bound.clone())))
            }
            _ => {}
        }
        self.by_key.insert(key.clone(), id);
        self.by_id.insert(id, key.clone());
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn instance_of(&self, key: &LiveSessionKey) -> Option<SessionInstanceId> {
        self.by_key.get(key).copied()
    }

    pub(crate) fn live_session_of(&self, id: SessionInstanceId) -> Option<&LiveSessionKey> {
        self.by_id.get(&id)
    }

    /// Every assignment, in a stable order.
    ///
    /// Sorted because the record is written whole: an order that follows
    /// hash iteration rewrites the same set as different bytes every
    /// time, which makes a real change indistinguishable from noise.
    pub(crate) fn bindings(&self) -> Vec<SessionIdentityBinding> {
        let mut out: Vec<_> = self
            .by_id
            .iter()
            .map(|(id, key)| SessionIdentityBinding::new(key.clone(), *id))
            .collect();
        out.sort_by_key(|b| b.session_instance_id());
        out
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        debug_assert_eq!(self.by_key.len(), self.by_id.len(), "bijection lost");
        self.by_key.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyclops_proto::{OsBootId, ProcessInstanceId, WorkspaceId};

    fn workspace() -> WorkspaceId {
        "11111111-1111-4111-8111-111111111111"
            .parse()
            .expect("workspace id")
    }

    fn instance(n: u8) -> SessionInstanceId {
        format!("22222222-2222-4222-8222-2222222222{n:02}")
            .parse()
            .expect("session instance id")
    }

    /// A live session, varied one fact at a time by the tests below.
    fn key(boot: &str, server: (i32, u64), session: &str) -> LiveSessionKey {
        LiveSessionKey::new(
            workspace(),
            OsBootId::new(boot).expect("os boot id"),
            ProcessInstanceId::new(server.0, server.1).expect("tmux server id"),
            session.parse().expect("tmux session id"),
        )
    }

    fn base() -> LiveSessionKey {
        key("boot-a", (900, 1000), "$1")
    }

    /// Minting a name for a session that already has one would strand
    /// everything filed under the first, so the same live session always
    /// answers with the same durable identity.
    #[test]
    fn the_same_live_session_keeps_the_name_it_was_given() {
        let mut reg = SessionIdentityRegistry::new();
        let first = reg.resolve(&base(), || instance(1)).unwrap();
        assert_eq!(first, Resolved::Minted(instance(1)));

        // The minter would hand back something else, and is never asked.
        let again = reg
            .resolve(&base(), || panic!("minted twice for one session"))
            .unwrap();
        assert_eq!(again, Resolved::Existing(instance(1)));
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.instance_of(&base()), Some(instance(1)));
        assert_eq!(reg.live_session_of(instance(1)), Some(&base()));
    }

    /// Every fact in the key names something that cannot be renamed, and
    /// changing any one of them is a different incarnation: a machine
    /// that rebooted, a tmux server that restarted, a server pid handed
    /// on to another process, or a session killed and recreated. None of
    /// them may inherit the previous incarnation's durable name.
    #[test]
    fn a_different_incarnation_is_a_different_identity() {
        for (case, other) in [
            ("the machine rebooted", key("boot-b", (900, 1000), "$1")),
            (
                "the tmux server pid changed",
                key("boot-a", (901, 1000), "$1"),
            ),
            (
                "the pid was reused by another server",
                key("boot-a", (900, 1001), "$1"),
            ),
            (
                "the session was killed and recreated",
                key("boot-a", (900, 1000), "$2"),
            ),
        ] {
            let mut reg = SessionIdentityRegistry::new();
            reg.resolve(&base(), || instance(1)).unwrap();
            assert_eq!(
                reg.resolve(&other, || instance(2)).unwrap(),
                Resolved::Minted(instance(2)),
                "{case}: the previous identity was inherited"
            );
            assert_eq!(reg.len(), 2, "{case}");
        }
    }

    /// The display name is not part of identity, and the type is what
    /// guarantees it: there is nowhere in a live session key to put one.
    /// Two observations of the same session are equal however the
    /// operator has renamed it in between.
    #[test]
    fn renaming_a_session_is_not_an_identity_change() {
        let mut reg = SessionIdentityRegistry::new();
        let minted = reg.resolve(&base(), || instance(1)).unwrap().id();

        // The same four facts, observed again after any number of
        // renames, because none of them records a name.
        let after_rename = key("boot-a", (900, 1000), "$1");
        assert_eq!(after_rename, base());
        assert_eq!(
            reg.resolve(&after_rename, || panic!("a rename minted a new identity"))
                .unwrap(),
            Resolved::Existing(minted)
        );
    }

    /// A live session with two durable names splits its own history, and
    /// a durable name over two live sessions merges two histories into
    /// one. Both are refused, and a refusal leaves the registry exactly
    /// as it was rather than half-applied.
    #[test]
    fn neither_direction_of_the_bijection_may_be_broken() {
        let mut reg = SessionIdentityRegistry::new();
        reg.resolve(&base(), || instance(1)).unwrap();
        let other = key("boot-a", (900, 1000), "$2");

        // Forward: this live session is already bound to another name.
        let forward = SessionIdentityBinding::new(base(), instance(2));
        assert_eq!(reg.bind(&forward), Err(BindError::KeyTaken(instance(1))));

        // Reverse: this name is already bound to another live session.
        let reverse = SessionIdentityBinding::new(other.clone(), instance(1));
        assert_eq!(
            reg.bind(&reverse),
            Err(BindError::IdTaken(Box::new(base())))
        );

        // Neither refusal published anything.
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.instance_of(&base()), Some(instance(1)));
        assert_eq!(reg.instance_of(&other), None);
        assert_eq!(reg.live_session_of(instance(2)), None);
    }

    /// Adopting a binding decided elsewhere, and adopting it again.
    /// Reading the same durable record twice must not be an error, or
    /// every restart that replays its own state would refuse itself.
    #[test]
    fn adopting_a_binding_twice_changes_nothing() {
        let mut reg = SessionIdentityRegistry::new();
        let binding = SessionIdentityBinding::new(base(), instance(1));
        assert_eq!(reg.bind(&binding), Ok(()));
        assert_eq!(reg.bind(&binding), Ok(()));
        assert_eq!(reg.len(), 1);

        // And an adopted binding is the one `resolve` answers with.
        assert_eq!(
            reg.resolve(&base(), || panic!("minted over an adopted binding"))
                .unwrap(),
            Resolved::Existing(instance(1))
        );
    }

    /// A candidate id already in use is refused without mutation.
    #[test]
    fn a_repeated_mint_is_refused() {
        let mut reg = SessionIdentityRegistry::new();
        reg.resolve(&base(), || instance(1)).unwrap();

        let other = key("boot-a", (900, 1000), "$2");
        assert_eq!(
            reg.resolve(&other, || instance(1)),
            Err(BindError::IdTaken(Box::new(base())))
        );
        assert_eq!(reg.live_session_of(instance(1)), Some(&base()));
        assert_eq!(reg.instance_of(&other), None);
    }
}
