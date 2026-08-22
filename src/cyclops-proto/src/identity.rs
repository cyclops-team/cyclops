//! Durable identities shared by records, routing, and clients.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    #[error("{0} must be a canonical, non-nil UUID")]
    InvalidUuid(&'static str),
    #[error("OS boot id must be a nonempty token without whitespace or control characters")]
    InvalidOsBootId,
    #[error("process id must be positive")]
    InvalidProcessId,
    #[error("process birth must be positive")]
    InvalidProcessBirth,
    #[error("{0} must use canonical {1}<decimal> form")]
    InvalidTmuxId(&'static str, char),
    #[error(
        "recipient key must use canonical admin:<workspace-id> or agent:<workspace-id>/<session-instance-id>/%<pane> form"
    )]
    InvalidRecipientKey,
}

fn deserialize_from_str<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: fmt::Display,
{
    String::deserialize(deserializer)?
        .parse()
        .map_err(serde::de::Error::custom)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct DurableId(Uuid);

impl DurableId {
    fn parse(value: &str, kind: &'static str) -> Result<Self, IdentityError> {
        let parsed = Uuid::parse_str(value).map_err(|_| IdentityError::InvalidUuid(kind))?;
        if parsed.is_nil() || parsed.hyphenated().to_string() != value {
            return Err(IdentityError::InvalidUuid(kind));
        }
        Ok(Self(parsed))
    }

    fn from_uuid(value: Uuid, kind: &'static str) -> Result<Self, IdentityError> {
        if value.is_nil() {
            return Err(IdentityError::InvalidUuid(kind));
        }
        Ok(Self(value))
    }
}

impl fmt::Display for DurableId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.hyphenated().fmt(f)
    }
}

macro_rules! durable_id {
    ($name:ident, $kind:literal, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(DurableId);

        impl $name {
            pub fn from_uuid(value: Uuid) -> Result<Self, IdentityError> {
                DurableId::from_uuid(value, $kind).map(Self)
            }
        }

        impl FromStr for $name {
            type Err = IdentityError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                DurableId::parse(value, $kind).map(Self)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.to_string())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                deserialize_from_str(deserializer)
            }
        }
    };
}

durable_id!(
    WorkspaceId,
    "workspace id",
    "The state domain rooted at one Cyclops home."
);
durable_id!(
    SessionInstanceId,
    "session instance id",
    "One concrete tmux-session incarnation inside a workspace."
);

/// An operating-system boot token, normalized by the platform reader.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OsBootId(String);

impl OsBootId {
    pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
        let value = value.into();
        if value.is_empty() || !value.chars().all(|character| character.is_ascii_graphic()) {
            return Err(IdentityError::InvalidOsBootId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for OsBootId {
    type Err = IdentityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl fmt::Display for OsBootId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Serialize for OsBootId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for OsBootId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_from_str(deserializer)
    }
}

/// A process identified by its PID and kernel-reported start value.
///
/// `birth` is microseconds since the epoch on macOS and clock ticks since
/// boot on Linux. It is only comparable within one OS boot, which is why
/// live-session keys also carry an OS boot id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProcessInstanceId {
    pid: i32,
    birth: u64,
}

impl ProcessInstanceId {
    pub fn new(pid: i32, birth: u64) -> Result<Self, IdentityError> {
        if pid <= 0 {
            return Err(IdentityError::InvalidProcessId);
        }
        if birth == 0 {
            return Err(IdentityError::InvalidProcessBirth);
        }
        Ok(Self { pid, birth })
    }

    pub fn pid(self) -> i32 {
        self.pid
    }

    pub fn birth(self) -> u64 {
        self.birth
    }
}

#[derive(Serialize, Deserialize)]
struct ProcessInstanceWire {
    pid: i32,
    birth: u64,
}

impl Serialize for ProcessInstanceId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ProcessInstanceWire {
            pid: self.pid,
            birth: self.birth,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ProcessInstanceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ProcessInstanceWire::deserialize(deserializer)?;
        Self::new(wire.pid, wire.birth).map_err(serde::de::Error::custom)
    }
}

fn parse_tmux_id(value: &str, sigil: char, kind: &'static str) -> Result<u64, IdentityError> {
    let invalid = || IdentityError::InvalidTmuxId(kind, sigil);
    let digits = value.strip_prefix(sigil).ok_or_else(invalid)?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let number = digits.parse::<u64>().map_err(|_| invalid())?;
    if format!("{sigil}{number}") != value {
        return Err(invalid());
    }
    Ok(number)
}

macro_rules! tmux_id {
    ($name:ident, $sigil:literal, $kind:literal, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            pub fn number(self) -> u64 {
                self.0
            }
        }

        impl FromStr for $name {
            type Err = IdentityError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                parse_tmux_id(value, $sigil, $kind).map(Self)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}{}", $sigil, self.0)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.to_string())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                deserialize_from_str(deserializer)
            }
        }
    };
}

tmux_id!(
    TmuxSessionId,
    '$',
    "tmux session id",
    "A canonical tmux `$<decimal>` session ID."
);
tmux_id!(
    TmuxPaneId,
    '%',
    "tmux pane id",
    "A canonical tmux `%<decimal>` pane ID."
);

/// The observed identity of one live tmux session.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct LiveSessionKey {
    workspace_id: WorkspaceId,
    os_boot_id: OsBootId,
    tmux_server: ProcessInstanceId,
    tmux_session_id: TmuxSessionId,
}

impl LiveSessionKey {
    pub fn new(
        workspace_id: WorkspaceId,
        os_boot_id: OsBootId,
        tmux_server: ProcessInstanceId,
        tmux_session_id: TmuxSessionId,
    ) -> Self {
        Self {
            workspace_id,
            os_boot_id,
            tmux_server,
            tmux_session_id,
        }
    }

    pub fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub fn os_boot_id(&self) -> &OsBootId {
        &self.os_boot_id
    }

    pub fn tmux_server(&self) -> ProcessInstanceId {
        self.tmux_server
    }

    pub fn tmux_session_id(&self) -> TmuxSessionId {
        self.tmux_session_id
    }
}

/// One durable assignment fact for an observed live session.
///
/// The daemon registry must enforce one instance per live key and one live key
/// per instance. This protocol type stores the mapping without enforcing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIdentityBinding {
    live_session_key: LiveSessionKey,
    session_instance_id: SessionInstanceId,
}

impl SessionIdentityBinding {
    pub fn new(live_session_key: LiveSessionKey, session_instance_id: SessionInstanceId) -> Self {
        Self {
            live_session_key,
            session_instance_id,
        }
    }

    pub fn live_session_key(&self) -> &LiveSessionKey {
        &self.live_session_key
    }

    pub fn session_instance_id(&self) -> SessionInstanceId {
        self.session_instance_id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Recipient {
    Admin,
    Agent {
        session_instance_id: SessionInstanceId,
        pane_id: TmuxPaneId,
    },
}

/// A logical mailbox recipient independent of mutable display labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecipientKey {
    workspace_id: WorkspaceId,
    recipient: Recipient,
}

impl RecipientKey {
    pub fn admin(workspace_id: WorkspaceId) -> Self {
        Self {
            workspace_id,
            recipient: Recipient::Admin,
        }
    }

    pub fn agent(
        workspace_id: WorkspaceId,
        session_instance_id: SessionInstanceId,
        pane_id: TmuxPaneId,
    ) -> Self {
        Self {
            workspace_id,
            recipient: Recipient::Agent {
                session_instance_id,
                pane_id,
            },
        }
    }

    pub fn workspace_id(self) -> WorkspaceId {
        self.workspace_id
    }

    pub fn session_instance_id(self) -> Option<SessionInstanceId> {
        match self.recipient {
            Recipient::Admin => None,
            Recipient::Agent {
                session_instance_id,
                ..
            } => Some(session_instance_id),
        }
    }

    pub fn pane_id(self) -> Option<TmuxPaneId> {
        match self.recipient {
            Recipient::Admin => None,
            Recipient::Agent { pane_id, .. } => Some(pane_id),
        }
    }

    pub fn is_admin(self) -> bool {
        matches!(self.recipient, Recipient::Admin)
    }
}

impl FromStr for RecipientKey {
    type Err = IdentityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || IdentityError::InvalidRecipientKey;
        if let Some(workspace) = value.strip_prefix("admin:") {
            return workspace
                .parse::<WorkspaceId>()
                .map(Self::admin)
                .map_err(|_| invalid());
        }
        let Some(parts) = value.strip_prefix("agent:") else {
            return Err(invalid());
        };
        let mut parts = parts.split('/');
        let workspace = parts.next().ok_or_else(invalid)?;
        let session = parts.next().ok_or_else(invalid)?;
        let pane = parts.next().ok_or_else(invalid)?;
        if parts.next().is_some() {
            return Err(invalid());
        }
        Ok(Self::agent(
            workspace.parse().map_err(|_| invalid())?,
            session.parse().map_err(|_| invalid())?,
            pane.parse().map_err(|_| invalid())?,
        ))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RecipientWire {
    Admin {
        workspace_id: WorkspaceId,
    },
    Agent {
        workspace_id: WorkspaceId,
        session_instance_id: SessionInstanceId,
        pane_id: TmuxPaneId,
    },
}

impl Serialize for RecipientKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self.recipient {
            Recipient::Admin => RecipientWire::Admin {
                workspace_id: self.workspace_id,
            },
            Recipient::Agent {
                session_instance_id,
                pane_id,
            } => RecipientWire::Agent {
                workspace_id: self.workspace_id,
                session_instance_id,
                pane_id,
            },
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RecipientKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match RecipientWire::deserialize(deserializer)? {
            RecipientWire::Admin { workspace_id } => Self::admin(workspace_id),
            RecipientWire::Agent {
                workspace_id,
                session_instance_id,
                pane_id,
            } => Self::agent(workspace_id, session_instance_id, pane_id),
        })
    }
}

impl fmt::Display for RecipientKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.recipient {
            Recipient::Admin => write!(f, "admin:{}", self.workspace_id),
            Recipient::Agent {
                session_instance_id,
                pane_id,
            } => write!(
                f,
                "agent:{}/{}/{}",
                self.workspace_id, session_instance_id, pane_id
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipient_key_display_is_its_canonical_selector() {
        let workspace = "00000000-0000-4000-8000-000000000001";
        let session = "00000000-0000-4000-8000-000000000002";
        for value in [
            format!("admin:{workspace}"),
            format!("agent:{workspace}/{session}/%7"),
        ] {
            let key: RecipientKey = value.parse().expect("canonical recipient key");
            assert_eq!(key.to_string(), value);
        }
        for value in [
            "reviewer",
            "admin:not-a-uuid",
            "agent:00000000-0000-4000-8000-000000000001/%7",
            "agent:00000000-0000-4000-8000-000000000001/00000000-0000-4000-8000-000000000002/7",
        ] {
            assert_eq!(
                value.parse::<RecipientKey>(),
                Err(IdentityError::InvalidRecipientKey)
            );
        }
    }
}
