//! Data-driven avatars for agents, roles, and humans in the Messages experience.
//!
//! Pure data types and registry. The renderer never performs string pattern-matching
//! or vendor guessing on labels; it consumes the resolved [`Avatar`] struct directly.

use std::collections::HashMap;

/// An avatar badge resolved for a sender or recipient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Avatar {
    /// Two-letter textual initials fallback (e.g. "CC", "CX", "AG", "OP").
    pub initials: String,
    /// Optional official icon glyph (e.g. "✳", "•", "✦"), if explicitly sourced in data.
    pub icon: Option<String>,
    /// Human display name for the agent or identity.
    pub display_name: String,
}

impl Avatar {
    pub fn new(
        initials: impl Into<String>,
        icon: Option<String>,
        display_name: impl Into<String>,
    ) -> Self {
        Self {
            initials: initials.into(),
            icon,
            display_name: display_name.into(),
        }
    }

    /// Formats the avatar badge for compact rendering. If an icon is present,
    /// it may be used alongside or instead of the initials, but textual initials
    /// are always preserved as the deterministic fallback.
    pub fn badge(&self) -> &str {
        if let Some(ref icon) = self.icon {
            icon.as_str()
        } else {
            self.initials.as_str()
        }
    }

    /// Derive a fallback avatar from an arbitrary label without vendor guessing.
    pub fn from_label(label: &str) -> Self {
        let trimmed = label.trim();
        let initials = if trimmed.is_empty() {
            "?".to_string()
        } else {
            let mut chars = trimmed.chars().filter(|c| c.is_alphanumeric());
            let first = chars.next().unwrap_or('?').to_ascii_uppercase();
            let second = chars.next().map(|c| c.to_ascii_uppercase());
            match second {
                Some(s) => format!("{first}{s}"),
                None => format!("{first}"),
            }
        };
        Self {
            initials,
            icon: None,
            display_name: label.to_string(),
        }
    }
}

/// Registry mapping proven manifest IDs to avatars.
#[derive(Debug, Clone)]
pub struct AvatarRegistry {
    entries: HashMap<String, Avatar>,
}

impl Default for AvatarRegistry {
    fn default() -> Self {
        let mut registry = Self {
            entries: HashMap::new(),
        };
        // Data-driven official mappings for shipped agent manifests:
        // Claude Code -> CC, optional icon "✳"
        // Codex CLI   -> CX, optional icon "•"
        // Antigravity / Gemini -> AG, optional icon "✦"
        registry.register("claude", Avatar::new("CC", Some("✳".into()), "Claude Code"));
        registry.register("codex", Avatar::new("CX", Some("•".into()), "Codex CLI"));
        registry.register(
            "agy",
            Avatar::new("AG", Some("✦".into()), "Antigravity CLI"),
        );
        registry.register(
            "gemini",
            Avatar::new("AG", Some("✦".into()), "Antigravity CLI"),
        );
        registry
    }
}

impl AvatarRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, id: impl Into<String>, avatar: Avatar) {
        self.entries.insert(id.into().to_ascii_lowercase(), avatar);
    }

    /// Resolve a manifest ID after its endpoint and route were proven.
    fn resolve_manifest(&self, manifest_id: &str) -> Option<Avatar> {
        self.entries
            .get(&manifest_id.trim().to_ascii_lowercase())
            .cloned()
    }

    /// Resolve an avatar for a durable endpoint by joining it to live mailbox routes
    /// and pane manifests, ensuring session-instance exactness, or falling back
    /// deterministically to initials from the label without vendor guessing.
    pub fn resolve_route_endpoint(
        &self,
        endpoint: &cyclops_proto::RecipientKey,
        display_label: &str,
        live_routes: Option<&[cyclops_proto::StatusMailboxRoute]>,
        pane_manifests: Option<&HashMap<String, String>>,
    ) -> Avatar {
        let is_live_route = live_routes
            .is_some_and(|routes| routes.iter().any(|route| &route.recipient == endpoint));
        let manifest = if is_live_route {
            pane_manifests.and_then(|manifests| manifests.get(endpoint.pane_id()))
        } else {
            None
        };
        if let Some(avatar) = manifest.and_then(|id| self.resolve_manifest(id)) {
            return avatar;
        }
        Avatar::from_label(display_label)
    }

    /// Resolve without live-route proof.
    ///
    /// A manifest keyed only by pane is insufficient because the pane may now
    /// belong to another process generation. This path always uses initials.
    pub fn resolve_endpoint(
        &self,
        _endpoint: &cyclops_proto::RecipientKey,
        display_label: &str,
        _pane_manifests: Option<&HashMap<String, String>>,
    ) -> Avatar {
        Avatar::from_label(display_label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_registry_resolves_known_agents() {
        let reg = AvatarRegistry::default();

        let claude = reg.resolve_manifest("claude").unwrap();
        assert_eq!(claude.initials, "CC");
        assert_eq!(claude.icon.as_deref(), Some("✳"));
        assert_eq!(claude.display_name, "Claude Code");

        let codex = reg.resolve_manifest("codex").unwrap();
        assert_eq!(codex.initials, "CX");
        assert_eq!(codex.icon.as_deref(), Some("•"));

        let gemini = reg.resolve_manifest("gemini").unwrap();
        assert_eq!(gemini.initials, "AG");
        assert_eq!(gemini.icon.as_deref(), Some("✦"));

        let agy = reg.resolve_manifest("agy").unwrap();
        assert_eq!(agy.initials, "AG");
        assert_eq!(agy.icon.as_deref(), Some("✦"));
    }

    #[test]
    fn fallback_derives_initials_without_guessing() {
        let reviewer = Avatar::from_label("reviewer");
        assert_eq!(reviewer.initials, "RE");
        assert_eq!(reviewer.icon, None);

        let custom = Avatar::from_label("custom_worker");
        assert_eq!(custom.initials, "CW");
        assert_eq!(custom.icon, None);

        let single = Avatar::from_label("x");
        assert_eq!(single.initials, "X");

        let empty = Avatar::from_label("   ");
        assert_eq!(empty.initials, "?");
    }

    #[test]
    fn custom_registration_overrides_fallback() {
        let mut reg = AvatarRegistry::default();
        reg.register(
            "custom",
            Avatar::new("CU", Some("★".into()), "Custom Agent"),
        );

        let custom = reg.resolve_manifest("custom").unwrap();
        assert_eq!(custom.initials, "CU");
        assert_eq!(custom.icon.as_deref(), Some("★"));
        assert_eq!(custom.display_name, "Custom Agent");
    }

    #[test]
    fn vendor_icon_requires_exact_live_route_and_manifest() {
        let reg = AvatarRegistry::default();
        let endpoint = cyclops_proto::RecipientKey::parse(
            "00000000-0000-0000-0000-000000000001:00000000-0000-0000-0000-000000000002:%1",
        )
        .unwrap();
        let replacement = cyclops_proto::RecipientKey::parse(
            "00000000-0000-0000-0000-000000000001:99999999-9999-9999-9999-999999999999:%1",
        )
        .unwrap();
        let manifests = HashMap::from([("%1".to_string(), "claude".to_string())]);
        let stale_routes = [cyclops_proto::StatusMailboxRoute {
            recipient: replacement,
            label: "claude".into(),
        }];

        let stale =
            reg.resolve_route_endpoint(&endpoint, "claude", Some(&stale_routes), Some(&manifests));
        assert_eq!(stale.initials, "CL");
        assert_eq!(stale.icon, None);

        let live_routes = [cyclops_proto::StatusMailboxRoute {
            recipient: endpoint,
            label: "renamed-agent".into(),
        }];
        let proven = reg.resolve_route_endpoint(
            &endpoint,
            "renamed-agent",
            Some(&live_routes),
            Some(&manifests),
        );
        assert_eq!(proven.initials, "CC");
        assert_eq!(proven.icon.as_deref(), Some("✳"));
    }
}
