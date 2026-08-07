//! The shipped theme files against the crate contract: they load clean,
//! cover the whole vocabulary, keep role fallbacks distinct, and their
//! explicit 256-color fallbacks in the three base themes match the
//! documented derivation wherever the files do not declare hand-tuning
//! (the role slots, and the palette, whose fallbacks are the ANSI
//! indices themselves). Branded themes keep their curated xterm mappings.
//! The high-contrast theme is additionally held to WCAG AA, which is the
//! only promise it makes that a reader cannot check by looking. Two tests
//! here pin docs/guides/themes.md instead of the files: the token table, because
//! a table that outlives its tokens is the bug this milestone went looking
//! for, and the contrast table, because a bar nobody publishes is a
//! threshold and not a check.

use std::path::PathBuf;

use cyclops_theme::{derive_c256, tokens, Theme};

const SHIPPED: [&str; 7] = [
    "catppuccin",
    "dark",
    "gruvbox",
    "high-contrast",
    "light",
    "nord",
    "tokyo-night",
];
const DERIVED_FALLBACKS: [&str; 3] = ["dark", "light", "high-contrast"];

fn theme_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../resources/themes")
        .join(format!("{name}.toml"))
}

fn shipped(name: &str) -> Theme {
    let (theme, warnings) = Theme::load(&theme_path(name)).expect("shipped theme loads");
    assert!(warnings.is_empty(), "{name}: {warnings:?}");
    theme
}

fn themes_doc() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/guides/themes.md");
    std::fs::read_to_string(&path).expect("docs/guides/themes.md")
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
/// distinctness. Palette slots are exempt too: their fallback is the
/// literal ANSI index, which the derivation refuses to produce. A
/// deliberate hand-tune elsewhere updates this test.
#[test]
fn shipped_non_role_fallbacks_match_the_derivation() {
    for name in DERIVED_FALLBACKS {
        let theme = shipped(name);
        for token in tokens::ALL {
            if tokens::ROLE.contains(&token) || tokens::PALETTE.contains(&token) {
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
/// role slots included, is its own 256-color entry. Palette slots are
/// the one exemption: their fallback is the literal ANSI index.
#[test]
fn high_contrast_is_grid_exact() {
    let theme = shipped("high-contrast");
    for token in tokens::ALL {
        if tokens::PALETTE.contains(&token) {
            continue;
        }
        let c = theme.resolve(token);
        assert_eq!(c.c256, derive_c256(c.rgb), "{token} is off the grid");
    }
}

/// The sixteen palette entries are one mapping, not sixteen figures, and
/// a theme that gives two ANSI slots the same color merges every pair of
/// things a pane's program distinguishes with them.
#[test]
fn shipped_palette_entries_are_pairwise_distinct() {
    for name in SHIPPED {
        let theme = shipped(name);
        let entries: Vec<(u8, u8, u8)> = tokens::PALETTE
            .iter()
            .map(|t| theme.resolve(t).rgb)
            .collect();
        for (i, a) in entries.iter().enumerate() {
            assert!(
                !entries[i + 1..].contains(a),
                "{name}: duplicate palette color {a:?} in {entries:?}"
            );
        }
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

/// WCAG contrast ratio between two colors, the number each theme header
/// states: (lighter + 0.05) / (darker + 0.05) on relative luminance.
fn contrast(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// What one theme's header claims about contrast, as numbers.
struct Claim {
    theme: &'static str,
    /// The background the file was drawn on and says so: the site's
    /// `--term-bg` for dark, its `--paper` for light, pure black for
    /// high-contrast. Each theme now ships it as `surface.bg`, so the
    /// measurement below runs against the theme's own resolved ground
    /// and asserts it still equals this stated one.
    ground: (u8, u8, u8),
    /// The floor every token clears.
    floor: f64,
    /// `state.dead`'s own floor. Lower in two of the three: a pane whose
    /// process is gone should be the hardest row on the screen to read,
    /// and every header says so in those words.
    dead: f64,
}

const CONTRAST: [Claim; 3] = [
    Claim {
        theme: "dark",
        ground: (0x0d, 0x0d, 0x0d),
        floor: 4.3,
        dead: 2.8,
    },
    Claim {
        theme: "light",
        ground: (0xfe, 0xfe, 0xfe),
        floor: 4.5,
        dead: 2.8,
    },
    Claim {
        theme: "high-contrast",
        ground: (0x00, 0x00, 0x00),
        floor: 7.0,
        dead: 7.0,
    },
];

/// Contrast is the one promise in a theme header that a reader cannot
/// check by looking, so it is the one that has to be measured. Role hues
/// were picked once, by hand, against these bars; a group added or
/// retuned later has no such moment unless something checks.
#[test]
fn shipped_themes_meet_their_stated_contrast() {
    for claim in CONTRAST {
        let name = claim.theme;
        let theme = shipped(name);
        // The measured ground is the theme's own surface.bg, and it has
        // to be the one the header (and the docs table) state.
        let ground = theme.resolve(tokens::SURFACE_BG).rgb;
        assert_eq!(
            ground, claim.ground,
            "{name}: surface.bg is not the ground its header states"
        );
        for token in tokens::ALL {
            // Chrome is measured as text on its painted grounds below;
            // measuring either ground as a figure would force it light.
            // surface.bg is the ground itself, not a figure on it.
            if token.starts_with("chrome.") || token == tokens::SURFACE_BG {
                continue;
            }
            // Palette entries are content colors the running program
            // picks; the theme only supplies the mapping, and palette.0
            // on a dark ground could never clear a floor.
            if tokens::PALETTE.contains(&token) {
                continue;
            }
            let bar = if token == tokens::STATE_DEAD {
                claim.dead
            } else {
                claim.floor
            };
            let ratio = contrast(theme.resolve(token).rgb, ground);
            assert!(
                ratio >= bar,
                "{name}: {token} {:?} measures {ratio:.2}:1 on {:?}, under the \
                 {bar}:1 its header states",
                theme.resolve(token).rgb,
                ground
            );
        }
        // The grounds' own contract: body text painted on the raised
        // ground (the active tab, a hovered menu row) clears the same
        // floor, and the two grounds are not the same color — raised
        // that equals panel highlights nothing.
        let fg = theme.resolve(tokens::CHROME_TEXT).rgb;
        let panel = theme.resolve(tokens::CHROME_PANEL).rgb;
        let raised = theme.resolve(tokens::CHROME_RAISED).rgb;
        for (ground_name, ground) in [("panel", panel), ("raised", raised)] {
            let ratio = contrast(fg, ground);
            assert!(
                ratio >= claim.floor,
                "{name}: chrome.text on chrome.{ground_name} measures {ratio:.2}:1, \
                 under the {}:1 floor",
                claim.floor
            );
        }
        assert_ne!(
            theme.resolve(tokens::CHROME_PANEL),
            theme.resolve(tokens::CHROME_RAISED),
            "{name}: chrome.panel and chrome.raised are the same color"
        );
        assert_ne!(
            theme.resolve(tokens::CHROME_PANEL).c256,
            theme.resolve(tokens::CHROME_RAISED).c256,
            "{name}: chrome grounds collapse on a 256-color terminal"
        );
        // A gone pane and a quiet one are different rows, so their cells
        // are different colors. Every header describes dead as one step
        // past quiet; collapsing them would make that sentence false and
        // cost a reader the difference at a glance.
        assert_ne!(
            theme.resolve(tokens::STATE_DEAD),
            theme.resolve(tokens::STATE_QUIET),
            "{name}: dead and quiet are the same color"
        );
    }
}

/// The chrome's secondary ink is `surface.dim` on `chrome.panel`: the
/// sidebar rows, the menu button, inactive tab labels. The claims above
/// measure only the three base themes, and their chrome check measures
/// `chrome.text`, so this pair shipped unmeasured and two branded themes
/// shipped it unreadable (nord 1.36:1, tokyo-night 2.35:1). The bar is
/// the workspace's own readability floor, `MIN_CONTRAST` = 3.0 in
/// `src/cyclops-workspace/src/render/mod.rs`; the render clamp applies
/// it to pane cells only and trusts chrome to the theme's own figures,
/// so the figures are what get measured.
#[test]
fn shipped_dim_stays_readable_on_the_chrome_panel() {
    for name in SHIPPED {
        let theme = shipped(name);
        let dim = theme.resolve(tokens::SURFACE_DIM).rgb;
        let panel = theme.resolve(tokens::CHROME_PANEL).rgb;
        let ratio = contrast(dim, panel);
        assert!(
            ratio >= 3.0,
            "{name}: surface.dim {dim:?} on chrome.panel {panel:?} measures \
             {ratio:.2}:1, under the workspace's 3:1 readability floor"
        );
    }
}

/// A contrast bar as the theme headers and docs/guides/themes.md write it: whole
/// numbers bare ("7:1"), everything else to one decimal ("2.8:1").
fn bar_words(bar: f64) -> String {
    if bar.fract() == 0.0 {
        format!("{bar:.0}:1")
    } else {
        format!("{bar:.1}:1")
    }
}

fn hex_words(rgb: (u8, u8, u8)) -> String {
    format!("`#{:02x}{:02x}{:02x}`", rgb.0, rgb.1, rgb.2)
}

/// The bars measured above are the bars the shipped files and
/// docs/guides/themes.md publish, and nothing else is allowed to be.
///
/// A threshold is only a check while it is somebody else's number. Tuned
/// down until the colors pass, it still passes its own assertion and stops
/// meaning anything: `light`'s `state.dead` measured 2.79:1 under a 2.7
/// threshold while its header and the page both said 2.8:1, so the one
/// test named as catching exactly that could not.
#[test]
fn the_published_bars_are_the_bars_that_get_measured() {
    let doc = themes_doc();
    for claim in CONTRAST {
        let name = claim.theme;

        // The page's contrast table: ground, floor, and the exception.
        let head = format!("| `{name}` |");
        let row = doc
            .lines()
            .find(|l| l.starts_with(&head))
            .unwrap_or_else(|| panic!("docs/guides/themes.md has no contrast row for {name}"));
        assert!(
            row.contains(&hex_words(claim.ground)),
            "{name}: the page names a different ground than the one measured: {row}"
        );
        assert!(
            row.contains(&bar_words(claim.floor)),
            "{name}: the page does not publish the {} floor measured here: {row}",
            bar_words(claim.floor)
        );
        if claim.dead == claim.floor {
            assert!(
                row.trim_end().ends_with("| nothing |"),
                "{name}: the page claims an exception this test does not measure: {row}"
            );
        } else {
            assert!(
                row.contains(&format!("`state.dead`, {}", bar_words(claim.dead))),
                "{name}: the page does not publish the {} state.dead bar measured here: {row}",
                bar_words(claim.dead)
            );
        }

        // And the file header, which is where a theme's own numbers live.
        let header = std::fs::read_to_string(theme_path(name)).expect("shipped theme file");
        assert!(
            header.contains(&bar_words(claim.floor)),
            "{name}.toml does not state the {} floor measured here",
            bar_words(claim.floor)
        );
        assert!(
            header.contains(&bar_words(claim.dead)),
            "{name}.toml does not state the {} state.dead bar measured here",
            bar_words(claim.dead)
        );
    }
}

/// docs/guides/themes.md is the user-facing contract for the vocabulary. Two
/// things are pinned: the count the page claims, and the table itself,
/// which may only name groups that exist. A token added or dropped
/// without touching the page fails here.
#[test]
fn the_docs_token_table_matches_the_vocabulary() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/guides/themes.md");
    let doc = std::fs::read_to_string(&path).expect("docs/guides/themes.md");

    let visible = tokens::ALL.len();
    let claim = format!("{visible} tokens change what you see");
    assert!(
        doc.contains(&claim),
        "docs/guides/themes.md must claim \"{claim}\""
    );
    assert!(
        doc.contains("`surface.fg`"),
        "docs/guides/themes.md must still explain surface.fg"
    );

    // The table runs from its header row to the next blank line.
    let table: Vec<&str> = doc
        .lines()
        .skip_while(|l| !l.starts_with("| Group "))
        .take_while(|l| !l.trim().is_empty())
        .collect();
    assert!(
        !table.is_empty(),
        "docs/guides/themes.md has no token table"
    );
    let table = table.join("\n");
    for group in [
        "role", "surface", "eye", "state", "badge", "chrome", "palette",
    ] {
        assert!(
            table.contains(group),
            "docs/guides/themes.md drops `{group}`"
        );
    }
    assert!(
        !table.contains("stream"),
        "docs/guides/themes.md still offers `stream`:\n{table}"
    );
    // Every group token by name, so a group renamed or added in the
    // vocabulary cannot leave the page describing the old set.
    for token in tokens::ALL {
        if token.starts_with("state.") || token.starts_with("badge.") {
            let (_, key) = token.split_once('.').expect("group.key");
            assert!(
                table.contains(key),
                "docs/guides/themes.md does not name `{token}`:\n{table}"
            );
        }
    }
}
