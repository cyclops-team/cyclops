//!
//! Recipient resolution, directory indexing, and display label validation.

use std::collections::{HashMap, HashSet};

use cyclops_proto::{
    MessageId, MessagePresentation, MessageRecipientRoute, RecipientKey, TmuxPaneId, WorkspaceId,
};

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxIdentity {
    pub key: RecipientKey,
    pub label: String,
}

pub struct MailboxSend {
    pub addresses: Vec<String>,
    pub recipient_keys: Option<Vec<RecipientKey>>,
    pub subject: String,
    pub summary: Option<String>,
    pub body: String,
    pub fyi: bool,
    pub client_key: Option<String>,
    pub supersedes: Option<MessageId>,
    /// Paste the whole message and press Enter with no composer check.
    pub raw: bool,
}

pub struct MailboxDirectory {
    workspace_id: WorkspaceId,
    by_address: HashMap<String, MailboxIdentity>,
    by_pane: HashMap<TmuxPaneId, MailboxIdentity>,
    by_recipient: HashMap<RecipientKey, MailboxIdentity>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MailboxDirectoryError {
    #[error("mailbox identity belongs to another workspace")]
    ForeignWorkspace,
    #[error("admin is not an agent directory entry")]
    AdminEntry,
    #[error("mailbox label must be non-empty and contain no control characters")]
    InvalidLabel,
    #[error("mailbox address '{0}' identifies more than one recipient")]
    DuplicateAddress(String),
    #[error("recipient '{0}' is not in the durable mailbox directory")]
    UnknownRecipient(String),
    #[error("recipient labels and durable recipient keys cannot be combined")]
    MixedRecipientSelectors,
    #[error("a reply derives its recipient from reply_to and cannot name recipients")]
    ReplyRecipientSelectors,
    #[error("'*' must be the only recipient address")]
    MixedBroadcast,
}

impl MailboxDirectory {
    pub fn new(
        workspace_id: WorkspaceId,
        agents: impl IntoIterator<Item = MailboxIdentity>,
    ) -> Result<Self, MailboxDirectoryError> {
        let mut directory = Self {
            workspace_id,
            by_address: HashMap::new(),
            by_pane: HashMap::new(),
            by_recipient: HashMap::new(),
        };
        let mut pane_candidates: HashMap<TmuxPaneId, Vec<MailboxIdentity>> = HashMap::new();
        for identity in agents {
            if identity.key.workspace_id() != workspace_id {
                return Err(MailboxDirectoryError::ForeignWorkspace);
            }
            if identity.key.is_admin() {
                return Err(MailboxDirectoryError::AdminEntry);
            }
            if identity.label.is_empty() || identity.label.chars().any(char::is_control) {
                return Err(MailboxDirectoryError::InvalidLabel);
            }
            // A headless agent is an agent entry with no pane: addressed by
            // its label alone, never by a pane id, and never the admin.
            let pane = identity.key.pane_id();
            if pane.is_none_or(|pane| identity.label != pane.to_string()) {
                let address = identity.label.clone();
                if directory
                    .by_address
                    .insert(address.clone(), identity.clone())
                    .is_some()
                {
                    return Err(MailboxDirectoryError::DuplicateAddress(address));
                }
            }
            if directory
                .by_recipient
                .insert(identity.key, identity.clone())
                .is_some()
            {
                return Err(MailboxDirectoryError::DuplicateAddress(
                    pane.map(|pane| pane.to_string())
                        .unwrap_or_else(|| identity.key.to_string()),
                ));
            }
            if let Some(pane) = pane {
                pane_candidates.entry(pane).or_default().push(identity);
            }
        }
        for (pane, candidates) in pane_candidates {
            let [identity] = candidates.as_slice() else {
                continue;
            };
            let address = pane.to_string();
            if directory
                .by_address
                .insert(address.clone(), identity.clone())
                .is_some()
            {
                return Err(MailboxDirectoryError::DuplicateAddress(address));
            }
            directory.by_pane.insert(pane, identity.clone());
        }
        Ok(directory)
    }

    pub fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub fn admin(&self) -> MailboxIdentity {
        MailboxIdentity {
            key: RecipientKey::admin(self.workspace_id),
            label: "admin".to_string(),
        }
    }

    pub fn agent_for_pane(&self, pane: TmuxPaneId) -> Option<MailboxIdentity> {
        self.by_pane.get(&pane).cloned()
    }

    pub fn identity_for_recipient(&self, recipient: RecipientKey) -> Option<MailboxIdentity> {
        if recipient == self.admin().key {
            return Some(self.admin());
        }
        self.by_recipient.get(&recipient).cloned()
    }

    /// Current human-facing routes keyed by their durable mailbox identity.
    pub fn routes(&self) -> Vec<MailboxIdentity> {
        let mut routes = Vec::with_capacity(self.by_recipient.len() + 1);
        routes.push(self.admin());
        routes.extend(self.by_recipient.values().cloned());
        routes.sort_by_key(|identity| identity.key);
        routes
    }

    pub(crate) fn current_routes(&self) -> HashMap<RecipientKey, MessageRecipientRoute> {
        self.by_recipient
            .iter()
            .map(|(recipient, identity)| {
                (
                    *recipient,
                    MessageRecipientRoute {
                        label: identity.label.clone(),
                        pane_id: recipient.pane_id(),
                    },
                )
            })
            .collect()
    }

    pub fn resolve(
        &self,
        addresses: &[String],
    ) -> Result<Vec<MailboxIdentity>, MailboxDirectoryError> {
        if addresses == ["*".to_string()] {
            let mut identities: Vec<_> = self.by_recipient.values().cloned().collect();
            identities.sort_by_key(|identity| identity.key);
            return Ok(identities);
        }
        if addresses.iter().any(|address| address == "*") {
            return Err(MailboxDirectoryError::MixedBroadcast);
        }
        let mut seen = HashSet::new();
        let mut identities = Vec::with_capacity(addresses.len());
        for address in addresses {
            let identity = if address == "admin" {
                self.admin()
            } else {
                self.by_address
                    .get(address)
                    .cloned()
                    .ok_or_else(|| MailboxDirectoryError::UnknownRecipient(address.clone()))?
            };
            if seen.insert(identity.key) {
                identities.push(identity);
            }
        }
        Ok(identities)
    }

    pub(crate) fn resolve_recipient_keys(
        &self,
        recipient_keys: &[RecipientKey],
    ) -> Result<Vec<MailboxIdentity>, MailboxDirectoryError> {
        let mut seen = HashSet::new();
        let mut identities = Vec::with_capacity(recipient_keys.len());
        for recipient in recipient_keys {
            if recipient.workspace_id() != self.workspace_id {
                return Err(MailboxDirectoryError::ForeignWorkspace);
            }
            let identity = self
                .identity_for_recipient(*recipient)
                .ok_or_else(|| MailboxDirectoryError::UnknownRecipient(recipient.to_string()))?;
            if seen.insert(identity.key) {
                identities.push(identity);
            }
        }
        Ok(identities)
    }
}

pub(crate) fn presentation_labels(
    recipients: &[RecipientKey],
    presentation: &MessagePresentation,
) -> Result<(String, Vec<String>), MailboxError> {
    validate_display_label("sender", &presentation.sender_label)?;
    if presentation.recipient_labels.len() != recipients.len() {
        return Err(MailboxError::InvalidPresentation(format!(
            "expected {} recipient labels, found {}",
            recipients.len(),
            presentation.recipient_labels.len()
        )));
    }
    let mut labels = Vec::with_capacity(recipients.len());
    for (index, (expected, snapshot)) in recipients
        .iter()
        .zip(&presentation.recipient_labels)
        .enumerate()
    {
        if snapshot.recipient != *expected {
            return Err(MailboxError::InvalidPresentation(format!(
                "recipient label {index} is bound to '{}', expected '{}'",
                snapshot.recipient, expected
            )));
        }
        validate_display_label("recipient", &snapshot.label)?;
        labels.push(snapshot.label.clone());
    }
    Ok((presentation.sender_label.clone(), labels))
}

pub(crate) fn reply_subject(parent: Option<&str>) -> Option<String> {
    parent.map(|subject| {
        if subject.starts_with("Re: ") {
            subject.to_string()
        } else {
            format!("Re: {subject}")
        }
    })
}

pub(crate) fn validate_display_label(kind: &str, label: &str) -> Result<(), MailboxError> {
    if label.is_empty() || label.chars().any(char::is_control) {
        return Err(MailboxError::InvalidPresentation(format!(
            "{kind} label must be non-empty and contain no control characters"
        )));
    }
    Ok(())
}
