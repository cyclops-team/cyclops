//! The shipped theme files against the crate contract: they load clean,
//! cover the whole vocabulary, keep role fallbacks distinct, and their
//! explicit 256-color fallbacks match the documented derivation wherever
//! the file headers do not declare hand-tuning (the role slots). The
//! high-contrast theme is additionally held to WCAG AA, which is the only
//! promise it makes that a reader cannot check by looking. The last test
//! here pins docs/themes.md to the vocabulary, because a token table that
//! outlives its tokens is the bug this milestone went looking for.

use std::path::PathBuf;

use cyclops_theme::{derive_c256, tokens, Theme};

const SHIPPED: [&str; 3] = ["dark", "light", "high-contrast"];

fn shipped(name: &str) -> Theme {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../themes")
        .join(format!("{name}.toml"));
    let (theme, warnings) = Theme::load(&path).expect("shipped theme loads");
    assert!(warnings.is_empty(), "{name}: {warnings:?}");
    theme
}

#[test]
fn shipped_themes_load_clean_and_cover_every_token() {
    for name in SHIPPED {
        let theme = shipped(name);
        assert_eq!(theme.name(), name);
        for token in tokens::ALL {
            assert!(theme.defines(token), "{name} is missing {token}");
        }
    }
}

/// Role color is a meaning-carrying encoding; two roles sharing a
/// 256-color fallback would be indistinguishable on 256-color terminals.
#[test]
fn shipped_role_fallbacks_are_pairwise_distinct() {
    for name in SHIPPED {
        let theme = shipped(name);
        let slots: Vec<u8> = tokens::ROLE.iter().map(|t| theme.resolve(t).c256).collect();
        for (i, a) in slots.iter().enumerate() {
            assert!(
                !slots[i + 1..].contains(a),
                "{name}: duplicate role fallback {a} in {slots:?}"
            );
        }
    }
}

/// Every non-role fallback is exactly what the derivation would pick, so
/// the explicit values in the files cannot drift from the algorithm
/// unnoticed. Role slots are exempt: their files declare hand-tuning for
/// distinctness. A deliberate hand-tune elsewhere updates this test.
#[test]
fn shipped_non_role_fallbacks_match_the_derivation() {
    for name in SHIPPED {
        let theme = shipped(name);
        for token in tokens::ALL {
            if tokens::ROLE.contains(&token) {
                continue;
            }
            let c = theme.resolve(token);
            assert_eq!(
                c.c256,
                derive_c256(c.rgb),
                "{name}: {token} fallback {} is not the derived {}",
                c.c256,
                derive_c256(c.rgb)
            );
        }
    }
}

/// The high-contrast theme promises grid-exact colors: every value,
/// role slots included, is its own 256-color entry.
#[test]
fn high_contrast_is_grid_exact() {
    let theme = shipped("high-contrast");
    for token in tokens::ALL {
        let c = theme.resolve(token);
        assert_eq!(c.c256, derive_c256(c.rgb), "{token} is off the grid");
    }
}

/// WCAG 2.1 relative luminance (sRGB), the input to a contrast ratio.
fn luminance(rgb: (u8, u8, u8)) -> f64 {
    let lin = |c: u8| {
        let c = f64::from(c) / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(rgb.0) + 0.7152 * lin(rgb.1) + 0.0722 * lin(rgb.2)
}

/// The high-contrast theme's whole promise is legibility, and its header
/// states that promise as a number. Every token is measured against black
/// because that is the ground the file assumes: it sets no `surface.bg`,
/// so a high-contrast terminal's own black is what these colors land on.
///
/// The state and badge groups are the reason this test exists. Role hues
/// were picked once, by hand, against this bar; a group added or retuned
/// later has no such moment unless something checks.
#[test]
fn shipped_high_contrast_clears_wcag_aa() {
    const AA: f64 = 4.5;
    let theme = shipped("high-contrast");
    let black = luminance((0, 0, 0));
    for token in tokens::ALL {
        let c = theme.resolve(token);
        let ratio = (luminance(c.rgb) + 0.05) / (black + 0.05);
        assert!(
            ratio >= AA,
            "{token} {:?} measures {ratio:.2}:1 on black, under AA's {AA}:1",
            c.rgb
        );
    }
}

/// docs/themes.md is the user-facing contract for the vocabulary. Two
/// things are pinned: the count the page claims, and the table itself,
/// which may only name groups that exist. A token added or dropped
/// without touching the page fails here.
#[test]
fn the_docs_token_table_matches_the_vocabulary() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/themes.md");
    let doc = std::fs::read_to_string(&path).expect("docs/themes.md");

    // surface.fg is in the vocabulary but no renderer paints it, so the
    // page counts the tokens whose edits are actually visible.
    let visible = tokens::ALL.len() - 1;
    let claim = format!("{visible} tokens change what you see");
    assert!(
        doc.contains(&claim),
        "docs/themes.md must claim \"{claim}\""
    );
    assert!(
        doc.contains("`surface.fg`"),
        "docs/themes.md must still explain surface.fg"
    );

    // The table runs from its header row to the next blank line.
    let table: Vec<&str> = doc
        .lines()
        .skip_while(|l| !l.starts_with("| Group "))
        .take_while(|l| !l.trim().is_empty())
        .collect();
    assert!(!table.is_empty(), "docs/themes.md has no token table");
    let table = table.join("\n");
    for group in ["role", "surface", "eye", "state", "badge"] {
        assert!(table.contains(group), "docs/themes.md drops `{group}`");
    }
    for gone in ["stream", "surface.bg"] {
        assert!(
            !table.contains(gone),
            "docs/themes.md still offers `{gone}`:\n{table}"
        );
    }
    // Every group token by name, so a group renamed or added in the
    // vocabulary cannot leave the page describing the old set.
    for token in tokens::ALL {
        if token.starts_with("state.") || token.starts_with("badge.") {
            let (_, key) = token.split_once('.').expect("group.key");
            assert!(
                table.contains(key),
                "docs/themes.md does not name `{token}`:\n{table}"
            );
        }
    }
}
