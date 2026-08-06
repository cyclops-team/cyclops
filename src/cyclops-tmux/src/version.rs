//! tmux version parsing, and the named predicates for version-specific
//! behavior.
//!
//! Rule (engineering rule 5): a version-specific behavior is a named
//! predicate here, never a version comparison written at a call site.
//!
//! What is actually gated on a version today is ONE thing, and it is a log
//! line: [`TmuxVersion::has_bracket_paste_flag`] tells `cyclopsd::boot`
//! whether to say that deliveries will fall back to post-paste composer
//! verification (amendment b). Nothing branches on it, because through
//! 3.6a the answer is always no and verification is the gate either way.
//! [`TmuxVersion::has_pause_after`] has no caller at all: the control
//! client sends `refresh-client -f pause-after=300` unconditionally and
//! treats a `%error` reply as an older tmux, which is a better test than
//! a version number because it asks the server that answered.
//!
//! This module does not run `tmux -V`. `cyclopsd::probe_tmux` spawns it
//! once at boot and hands the text to [`TmuxVersion::parse`], which is the
//! one tmux invocation outside this crate (see the crate header). The
//! parse is total: an unrecognized string leaves `numeric` None, and both
//! predicates then read false.

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
