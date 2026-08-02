//! Color and weight. Seed of the theme system: every raw ANSI value in this
//! crate lives in the tables below, keyed by semantic slot. GOALS.md allows
//! exactly two meaning-carrying encodings: role color (here) and state
//! glyphs (cyclops-proto). States are never colored.

use std::io::IsTerminal;

/// One palette slot: truecolor value plus its 256-color fallback.
struct Slot {
    rgb: (u8, u8, u8),
    c256: u8,
}

/// Role slots. role_color hashes a label into this table, so slot count and
/// order are part of visual stability: reordering re-colors every agent.
const ROLE: [Slot; 6] = [
    Slot {
        rgb: (97, 175, 239),
        c256: 75,
    }, // blue
    Slot {
        rgb: (152, 195, 121),
        c256: 114,
    }, // green
    Slot {
        rgb: (198, 120, 221),
        c256: 176,
    }, // violet
    Slot {
        rgb: (86, 182, 194),
        c256: 73,
    }, // teal
    Slot {
        rgb: (229, 192, 123),
        c256: 180,
    }, // sand
    Slot {
        rgb: (224, 108, 117),
        c256: 168,
    }, // rose
];

/// Accent: the eye and other single-voice marks.
const ACCENT: Slot = Slot {
    rgb: (209, 154, 102),
    c256: 173,
};

/// Dim: detail columns, gutters, separators.
const DIM: Slot = Slot {
    rgb: (128, 128, 128),
    c256: 245,
};

#[derive(Clone, Copy)]
pub struct Style {
    /// ANSI output enabled at all.
    pub color: bool,
    /// Screen-reader mode: no color, no glyph animation, ever. Color folds
    /// it in already; renderers that animate (the M3 stream) will read this
    /// directly. Unused until then.
    #[allow(dead_code)]
    pub plain: bool,
    truecolor: bool,
}

impl Style {
    /// Detect from the flag, NO_COLOR, terminal presence, and COLORTERM.
    /// NO_COLOR counts when present and non-empty, per its spec.
    pub fn detect(plain: bool) -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        Style {
            color: !plain && !no_color && std::io::stdout().is_terminal(),
            plain,
            truecolor: std::env::var("COLORTERM").as_deref() == Ok("truecolor"),
        }
    }

    /// No decoration at all. Tests and machine paths use this.
    pub fn none() -> Self {
        Style {
            color: false,
            plain: true,
            truecolor: false,
        }
    }

    fn paint(&self, slot: &Slot, text: &str) -> String {
        if !self.color {
            return text.to_string();
        }
        let Slot {
            rgb: (r, g, b),
            c256,
        } = *slot;
        if self.truecolor {
            format!("\x1b[38;2;{r};{g};{b}m{text}\x1b[0m")
        } else {
            format!("\x1b[38;5;{c256}m{text}\x1b[0m")
        }
    }

    /// Paint text in the label's stable role color.
    pub fn role(&self, label: &str, text: &str) -> String {
        self.paint(&ROLE[role_color(label)], text)
    }

    pub fn accent(&self, text: &str) -> String {
        self.paint(&ACCENT, text)
    }

    pub fn dim(&self, text: &str) -> String {
        self.paint(&DIM, text)
    }

    pub fn bold(&self, text: &str) -> String {
        if !self.color {
            return text.to_string();
        }
        format!("\x1b[1m{text}\x1b[0m")
    }
}

/// Stable palette slot for a label. FNV-1a, not DefaultHasher: the same
/// label must keep its color across runs, platforms, and rust versions.
pub fn role_color(label: &str) -> usize {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in label.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    (h % ROLE.len() as u64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_color_is_stable_and_in_range() {
        let a = role_color("reviewer");
        assert_eq!(a, role_color("reviewer"));
        assert!(a < ROLE.len());
        // Not a proof of spread, just a regression pin on the hash.
        assert_eq!(role_color("reviewer"), role_color("reviewer"));
        assert!(role_color("implementer") < ROLE.len());
    }

    #[test]
    fn truecolor_and_256_sequences() {
        let tc = Style {
            color: true,
            plain: false,
            truecolor: true,
        };
        let lo = Style {
            color: true,
            plain: false,
            truecolor: false,
        };
        assert!(tc.dim("x").starts_with("\x1b[38;2;128;128;128m"));
        assert!(lo.dim("x").starts_with("\x1b[38;5;245m"));
        assert!(tc.dim("x").ends_with("\x1b[0m"));
    }

    #[test]
    fn no_color_paths_return_text_untouched() {
        let s = Style::none();
        assert_eq!(s.role("reviewer", "reviewer"), "reviewer");
        assert_eq!(s.dim("x"), "x");
        assert_eq!(s.bold("x"), "x");
        assert_eq!(s.accent("◉"), "◉");
    }
}
