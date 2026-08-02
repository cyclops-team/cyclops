//! tmux version probe and feature gates.
//!
//! Rule (engineering rule 5): every version-specific behavior is gated here
//! with an explicit probe, not assumed at call sites.

/// Parsed `tmux -V` output plus the feature gates Cyclops cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxVersion {
    /// Verbatim string after "tmux ", e.g. "3.6a", "next-3.8".
    pub raw: String,
    /// (major, minor) when parseable; "next-3.8" parses as (3, 8).
    pub numeric: Option<(u32, u32)>,
}

impl TmuxVersion {
    pub fn parse(v_output: &str) -> TmuxVersion {
        let raw = v_output
            .trim()
            .strip_prefix("tmux ")
            .unwrap_or(v_output.trim())
            .to_string();
        let core = raw.strip_prefix("next-").unwrap_or(&raw);
        let mut parts = core.split('.');
        let major = parts.next().and_then(|s| s.parse::<u32>().ok());
        let minor = parts
            .next()
            .map(|s| s.trim_end_matches(|c: char| c.is_ascii_alphabetic()))
            .and_then(|s| s.parse::<u32>().ok());
        let numeric = match (major, minor) {
            (Some(a), Some(b)) => Some((a, b)),
            _ => None,
        };
        TmuxVersion { raw, numeric }
    }

    /// #{bracket_paste_flag} exists only in next-3.8 and later. On 3.6a and
    /// earlier there is NO way to gate on bracketed-paste degradation, so
    /// post-paste composer verification is the gate (validation amendment b).
    pub fn has_bracket_paste_flag(&self) -> bool {
        matches!(self.numeric, Some((maj, min)) if (maj, min) >= (3, 8))
    }

    /// Control-mode flow control (pause-after / %pause / %continue) landed
    /// in 3.2; all supported versions have it. Probe kept explicit anyway.
    pub fn has_pause_after(&self) -> bool {
        matches!(self.numeric, Some((maj, min)) if (maj, min) >= (3, 2))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_release_and_next() {
        let v = TmuxVersion::parse("tmux 3.6a\n");
        assert_eq!(v.raw, "3.6a");
        assert_eq!(v.numeric, Some((3, 6)));
        assert!(!v.has_bracket_paste_flag());
        assert!(v.has_pause_after());

        let n = TmuxVersion::parse("tmux next-3.8");
        assert_eq!(n.numeric, Some((3, 8)));
        assert!(n.has_bracket_paste_flag());
    }

    #[test]
    fn garbage_is_tolerated() {
        let v = TmuxVersion::parse("openbsd-tmux");
        assert_eq!(v.numeric, None);
        assert!(!v.has_bracket_paste_flag());
    }
}
