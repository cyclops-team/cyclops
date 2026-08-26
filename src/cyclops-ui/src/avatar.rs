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

/// Registry mapping manifest IDs, process names, or agent identifiers to avatars.
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

    /// Resolve an avatar by identity key or label. If not registered, falls back
    /// deterministically to uppercase initials derived from the label.
    pub fn resolve(&self, id_or_label: &str) -> Avatar {
        let normalized = id_or_label.trim().to_ascii_lowercase();
        if let Some(avatar) = self.entries.get(&normalized) {
            return avatar.clone();
        }
        Avatar::from_label(id_or_label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_registry_resolves_known_agents() {
        let reg = AvatarRegistry::default();

        let claude = reg.resolve("claude");
        assert_eq!(claude.initials, "CC");
        assert_eq!(claude.icon.as_deref(), Some("✳"));
        assert_eq!(claude.display_name, "Claude Code");

        let codex = reg.resolve("codex");
        assert_eq!(codex.initials, "CX");
        assert_eq!(codex.icon.as_deref(), Some("•"));

        let gemini = reg.resolve("gemini");
        assert_eq!(gemini.initials, "AG");
        assert_eq!(gemini.icon.as_deref(), Some("✦"));

        let agy = reg.resolve("agy");
        assert_eq!(agy.initials, "AG");
        assert_eq!(agy.icon.as_deref(), Some("✦"));
    }

    #[test]
    fn fallback_derives_initials_without_guessing() {
        let reg = AvatarRegistry::default();

        let reviewer = reg.resolve("reviewer");
        assert_eq!(reviewer.initials, "RE");
        assert_eq!(reviewer.icon, None);

        let custom = reg.resolve("custom_worker");
        assert_eq!(custom.initials, "CW");
        assert_eq!(custom.icon, None);

        let single = reg.resolve("x");
        assert_eq!(single.initials, "X");

        let empty = reg.resolve("   ");
        assert_eq!(empty.initials, "?");
    }

    #[test]
    fn custom_registration_overrides_fallback() {
        let mut reg = AvatarRegistry::default();
        reg.register(
            "custom",
            Avatar::new("CU", Some("★".into()), "Custom Agent"),
        );

        let custom = reg.resolve("custom");
        assert_eq!(custom.initials, "CU");
        assert_eq!(custom.icon.as_deref(), Some("★"));
        assert_eq!(custom.display_name, "Custom Agent");
    }
}
