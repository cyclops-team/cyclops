//! The shipped theme files against the crate contract: they load clean,
//! cover the whole vocabulary, keep role fallbacks distinct, and their
//! explicit 256-color fallbacks in the three base themes match the
//! documented derivation wherever the files do not declare hand-tuning
//! (the role slots, and the palette, whose fallbacks are the ANSI
//! indices themselves). Every other theme keeps its own curated mappings.
//! The high-contrast theme is additionally held to WCAG AA, which is the
//! only promise it makes that a reader cannot check by looking.
//!
//! `SHIPPED` below is held to resources/themes/ rather than maintained by
//! hand, because it is one of two such lists in two crates and the other
//! one is what actually seeds a machine.
//!
//! Three tests here pin docs/guides/themes.md instead of the files: the token
//! table, because a table that outlives its tokens is the bug this
//! milestone went looking for, the contrast table, because a bar nobody
//! publishes is a threshold and not a check, and the theme listing,
//! because the page is the third place the shipped set is written down.

use std::path::PathBuf;

use cyclops_theme::{derive_c256, tokens, Theme};

/// Every gate below runs once per name here, and
/// `the_shipped_list_is_the_themes_directory` holds the list to
/// resources/themes/ so a name cannot be left off.
const SHIPPED: [&str; 17] = [
    "blossom",
    "buttercream",
    "catppuccin",
    "dark",
    "ember",
    "forest",
    "gruvbox",
    "high-contrast",
    "light",
    "meadow",
    "midnight",
    "nord",
    "obsidian",
    "periwinkle",
    "seafoam",
    "sorbet",
    "tokyo-night",
];
const DERIVED_FALLBACKS: [&str; 3] = ["dark", "light", "high-contrast"];

fn themes_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../resources/themes")
}

fn theme_path(name: &str) -> PathBuf {
    themes_dir().join(format!("{name}.toml"))
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

/// The shipped set lived in two lists in two crates with nothing tying
/// them together: this one, and `SHIPPED` in
/// `src/cyclops/src/themeseed.rs`, which is what the binary embeds and
/// seeds. A theme in that list and not this one shipped with no gate
/// ever loading it.
///
/// Neither crate can see the other's list, so both are pinned to the one
/// thing they describe: the directory. Each has a copy of this test, and
/// two lists that both equal the directory cannot disagree.
#[test]
fn the_shipped_list_is_the_themes_directory() {
    let mut on_disk: Vec<String> = std::fs::read_dir(themes_dir())
        .expect("resources/themes")
        .map(|e| {
            e.expect("theme dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter_map(|n| n.strip_suffix(".toml").map(str::to_string))
        .collect();
    on_disk.sort();

    for name in &on_disk {
        assert!(
            SHIPPED.contains(&name.as_str()),
            "resources/themes/{name}.toml ships untested; add \"{name}\" to SHIPPED \
             in src/cyclops-theme/tests/shipped.rs"
        );
    }
    for name in SHIPPED {
        assert!(
            on_disk.contains(&name.to_string()),
            "SHIPPED names {name}, but resources/themes/{name}.toml does not exist"
        );
    }
    assert_eq!(
        SHIPPED.to_vec(),
        on_disk,
        "SHIPPED is not resources/themes/ in alphabetical order"
    );
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

/// A count as docs/guides/themes.md writes it. The page spells theme
/// counts out ("Seven themes ship"), so a pin on that sentence spells too.
fn count_words(n: usize) -> String {
    const ONES: [&str; 20] = [
        "zero",
        "one",
        "two",
        "three",
        "four",
        "five",
        "six",
        "seven",
        "eight",
        "nine",
        "ten",
        "eleven",
        "twelve",
        "thirteen",
        "fourteen",
        "fifteen",
        "sixteen",
        "seventeen",
        "eighteen",
        "nineteen",
    ];
    const TENS: [&str; 10] = [
        "", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
    ];
    assert!(n < 100, "{n} themes: spell it here first");
    if n < 20 {
        return ONES[n].to_string();
    }
    match n % 10 {
        0 => TENS[n / 10].to_string(),
        o => format!("{}-{}", TENS[n / 10], ONES[o]),
    }
}

/// docs/guides/themes.md is the third place the shipped set is written
/// down, after the two `SHIPPED` lists, and it was the only one nothing
/// held to the files. A theme added to both lists and left off the page
/// shipped undocumented, and the count sentence went stale silently.
///
/// Three things get pinned, all of them derived from `SHIPPED`: the
/// count, the sentence that repeats it, and the sample `cyclops theme`
/// listing, which is the page's own claim about what a reader will see.
#[test]
fn the_docs_name_every_shipped_theme() {
    let doc = themes_doc();
    let lower = doc.to_lowercase();

    for claim in [
        format!("{} themes ship", count_words(SHIPPED.len())),
        format!("seeds all {} into", count_words(SHIPPED.len())),
    ] {
        assert!(
            lower.contains(&claim),
            "docs/guides/themes.md must claim \"{claim}\", however it capitalizes it"
        );
    }

    // The sample listing: the fenced block carrying the switch hint.
    // Every row is one theme, `▸` marking the one that is on.
    let block = doc
        .split("```")
        .find(|b| b.contains("cyclops theme <name> to switch"))
        .expect("docs/guides/themes.md has no theme listing block");
    let listed: Vec<&str> = block
        .lines()
        .filter_map(|l| l.strip_prefix("  ").or_else(|| l.strip_prefix("▸ ")))
        .filter_map(|l| l.split_whitespace().next())
        .filter(|w| *w != "cyclops")
        .collect();
    assert_eq!(
        listed, SHIPPED,
        "the sample listing in docs/guides/themes.md is not the shipped set"
    );

    for name in SHIPPED {
        assert!(
            doc.contains(&format!("`{name}`")),
            "docs/guides/themes.md never names `{name}`"
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

/// CIE 1976 L*a*b*, D65: the space role separation is judged in. WCAG
/// contrast answers "can this be read", not "is this a different color
/// from that one", and two ashes a lightness apart clear every contrast
/// bar in this file while naming the same agent twice.
fn lab(rgb: (u8, u8, u8)) -> (f64, f64, f64) {
    let lin = |c: u8| {
        let c = f64::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = (lin(rgb.0), lin(rgb.1), lin(rgb.2));
    let x = (0.412_456_4 * r + 0.357_576_1 * g + 0.180_437_5 * b) / 0.950_47;
    let y = 0.212_672_9 * r + 0.715_152_2 * g + 0.072_175_0 * b;
    let z = (0.019_333_9 * r + 0.119_192_0 * g + 0.950_304_1 * b) / 1.088_83;
    let f = |t: f64| {
        if t > 216.0 / 24389.0 {
            t.cbrt()
        } else {
            (841.0 / 108.0) * t + 4.0 / 29.0
        }
    };
    let (fx, fy, fz) = (f(x), f(y), f(z));
    (116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz))
}

/// dE76: euclidean distance in CIELAB.
fn de76(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    let (a, b) = (lab(a), lab(b));
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt()
}

/// The floor every shipped theme's role slots clear, and the bar a theme
/// whose header promises more is held to. The floor is the worst the
/// shipped set carries (`nord`, 9.60): a guard against a new theme
/// shipping roles nobody can tell apart, not a retune of the set.
/// `obsidian` shipped at 7.03 with six of eight slots inside 13.7 of
/// another, which is what put this test here, and its header now states 22.
const ROLE_SEPARATION: f64 = 9.5;
const ROLE_SEPARATION_STATED: [(&str, f64); 1] = [("obsidian", 22.0)];
/// Role color names an agent; pane text is not an agent. A slot within
/// this of `surface.fg` reads as ordinary body text in the pane it labels.
const ROLE_VS_FG: f64 = 15.0;

#[test]
fn shipped_role_colors_stay_far_enough_apart_to_name_an_agent() {
    for name in SHIPPED {
        let theme = shipped(name);
        let bar = ROLE_SEPARATION_STATED
            .iter()
            .find(|(n, _)| *n == name)
            .map_or(ROLE_SEPARATION, |(_, b)| *b);
        let slots: Vec<(u8, u8, u8)> = tokens::ROLE.iter().map(|t| theme.resolve(t).rgb).collect();
        for (i, a) in slots.iter().enumerate() {
            for (j, b) in slots.iter().enumerate().skip(i + 1) {
                let d = de76(*a, *b);
                assert!(
                    d >= bar,
                    "{name}: role.{} {a:?} and role.{} {b:?} are dE76 {d:.2} apart, \
                     under the {bar} two agents need to be told apart",
                    i + 1,
                    j + 1
                );
            }
            let d = de76(*a, theme.resolve(tokens::SURFACE_FG).rgb);
            assert!(
                d >= bar.max(ROLE_VS_FG),
                "{name}: role.{} {a:?} is dE76 {d:.2} from surface.fg, so an agent \
                 named in that slot reads as ordinary pane text",
                i + 1
            );
        }
    }
}

/// A theme is a palette, and two themes that hand a pane's program the
/// same sixteen colors are one palette wearing two names: the agent CLI
/// output that fills most of a pane looks identical under both. `sorbet`
/// shipped nine of its sixteen byte-identical with `meadow`'s while its
/// own header called the mapping something else.
/// One theme's sixteen ANSI colors, in `tokens::PALETTE` order.
type Palette<'a> = (&'a str, Vec<(u8, u8, u8)>);

#[test]
fn no_two_shipped_palettes_are_the_same_palette() {
    let loaded: Vec<Palette<'_>> = SHIPPED
        .iter()
        .map(|name| {
            let theme = shipped(name);
            (
                *name,
                tokens::PALETTE
                    .iter()
                    .map(|t| theme.resolve(t).rgb)
                    .collect(),
            )
        })
        .collect();
    for (i, (a, pa)) in loaded.iter().enumerate() {
        for (b, pb) in loaded.iter().skip(i + 1) {
            let shared: Vec<usize> = (0..pa.len()).filter(|k| pa[*k] == pb[*k]).collect();
            assert!(
                shared.is_empty(),
                "{a} and {b} ship the same color in palette slots {shared:?}; a slot \
                 two themes agree on byte for byte is one theme's identity in the \
                 other's file"
            );
        }
    }
}
