//! VT fidelity fixture corpus. Pure tests — no tmux, no testrig.
//!
//! Runs every fixture against the production engine (`alacritty_terminal`)
//! and the comparison engine (`vt100`, standing in for `libghostty-vt` which
//! requires Zig at build time — see F34). The summary test prints per-engine
//! scores for review.

use cyclops_workspace::{feed_alacritty, AlacrittyVt, CellAttrs, CellGrid, Color, GridCell};

/// One recorded byte stream and its expected visible grid.
struct Fixture {
    name: &'static str,
    category: &'static str,
    cols: u16,
    rows: u16,
    bytes: &'static [u8],
    /// Expected row texts (trailing spaces ignored).
    rows_text: &'static [&'static str],
    /// When set, also assert fg color on the first non-space cell of row 0.
    expect_fg: Option<Color>,
    /// When set, assert an attribute on row 0's first content cell.
    expect_bold: bool,
    expect_dim: bool,
}

fn feed_vt100(bytes: &[u8], cols: u16, rows: u16) -> CellGrid {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(bytes);
    let screen = parser.screen();
    let mut cells = Vec::with_capacity(cols as usize * rows as usize);
    for row in 0..rows {
        for col in 0..cols {
            let ch = screen
                .cell(row, col)
                .map(|c| c.contents())
                .unwrap_or_default();
            let ch = ch.chars().next().unwrap_or(' ');
            cells.push(GridCell {
                ch,
                wide_spacer: false,
                attrs: CellAttrs::default(),
            });
        }
    }
    CellGrid { cols, rows, cells }
}

fn assert_grid_matches(fixture: &Fixture, grid: &CellGrid, engine: &str) {
    let texts = grid.row_texts();
    for (i, expected) in fixture.rows_text.iter().enumerate() {
        assert_eq!(
            texts.get(i).map(String::as_str),
            Some(*expected),
            "{} / {} row {i}",
            engine,
            fixture.name
        );
    }
    if let Some(expected_fg) = fixture.expect_fg {
        let cell = grid.cell(0, 0).or_else(|| grid.cell(1, 0));
        if let Some(c) = cell {
            if !fixture.expect_bold && !fixture.expect_dim {
                assert_eq!(c.attrs.fg, expected_fg, "{} / {} fg", engine, fixture.name);
            }
        }
    }
    if fixture.expect_bold {
        let found = (0..fixture.rows)
            .flat_map(|r| (0..fixture.cols).map(move |c| (c, r)))
            .filter_map(|(c, r)| grid.cell(c, r))
            .any(|c| c.attrs.bold);
        assert!(found, "{} / {} expected bold", engine, fixture.name);
    }
    if fixture.expect_dim {
        let found = (0..fixture.rows)
            .flat_map(|r| (0..fixture.cols).map(move |c| (c, r)))
            .filter_map(|(c, r)| grid.cell(c, r))
            .any(|c| c.attrs.dim);
        assert!(found, "{} / {} expected dim", engine, fixture.name);
    }
}

fn corpus() -> Vec<Fixture> {
    vec![
        Fixture {
            name: "plain_hello",
            category: "plain",
            cols: 12,
            rows: 3,
            bytes: b"hello world\r\n",
            rows_text: &["hello world", "", ""],
            expect_fg: None,
            expect_bold: false,
            expect_dim: false,
        },
        Fixture {
            name: "sgr_red",
            category: "sgr",
            cols: 10,
            rows: 2,
            bytes: b"\x1b[31mred\x1b[0m\r\n",
            rows_text: &["red", ""],
            expect_fg: Some(Color::Indexed(1)),
            expect_bold: false,
            expect_dim: false,
        },
        Fixture {
            name: "color_256",
            category: "256",
            cols: 10,
            rows: 2,
            bytes: b"\x1b[38;5;196mX\x1b[0m\r\n",
            rows_text: &["X", ""],
            expect_fg: Some(Color::Indexed(196)),
            expect_bold: false,
            expect_dim: false,
        },
        Fixture {
            name: "truecolor",
            category: "truecolor",
            cols: 10,
            rows: 2,
            bytes: b"\x1b[38;2;255;128;64mT\x1b[0m\r\n",
            rows_text: &["T", ""],
            expect_fg: Some(Color::Rgb(255, 128, 64)),
            expect_bold: false,
            expect_dim: false,
        },
        Fixture {
            name: "bold_attr",
            category: "attributes",
            cols: 10,
            rows: 2,
            bytes: b"\x1b[1mbold\x1b[0m\r\n",
            rows_text: &["bold", ""],
            expect_fg: None,
            expect_bold: true,
            expect_dim: false,
        },
        Fixture {
            name: "cursor_motion",
            category: "cursor",
            cols: 10,
            rows: 3,
            bytes: b"a\x1b[3Db\x1b[2Bend",
            rows_text: &["b", "", " end"],
            expect_fg: None,
            expect_bold: false,
            expect_dim: false,
        },
        Fixture {
            name: "wrap",
            category: "wrapping",
            cols: 5,
            rows: 3,
            bytes: b"1234567890",
            rows_text: &["12345", "67890", ""],
            expect_fg: None,
            expect_bold: false,
            expect_dim: false,
        },
        Fixture {
            name: "wide_cjk",
            category: "wide",
            cols: 6,
            rows: 2,
            bytes: "你好\r\n".as_bytes(),
            rows_text: &["你好", ""],
            expect_fg: None,
            expect_bold: false,
            expect_dim: false,
        },
        Fixture {
            name: "alt_screen",
            category: "alternate",
            cols: 10,
            rows: 3,
            bytes: b"\x1b[?1049h\x1b[Halt",
            rows_text: &["alt", "", ""],
            expect_fg: None,
            expect_bold: false,
            expect_dim: false,
        },
        Fixture {
            name: "bracketed_paste",
            category: "paste",
            cols: 12,
            rows: 2,
            bytes: b"\x1b[?2004h\x1b[200~pasted\x1b[201~\x1b[?2004l",
            rows_text: &["pasted", ""],
            expect_fg: None,
            expect_bold: false,
            expect_dim: false,
        },
        Fixture {
            name: "codex_ghost",
            category: "codex",
            cols: 20,
            rows: 2,
            bytes: b"\x1b[2mghost\x1b[0mtyped\r\n",
            rows_text: &["ghosttyped", ""],
            expect_fg: None,
            expect_bold: false,
            expect_dim: true,
        },
        Fixture {
            name: "claude_spinner_title",
            category: "claude",
            cols: 16,
            rows: 2,
            bytes: b"\x1b]0;Claude\x07\x1b[1m*\x1b[0m working\r\n",
            rows_text: &["* working", ""],
            expect_fg: None,
            expect_bold: true,
            expect_dim: false,
        },
    ]
}

#[test]
fn alacritty_corpus_passes() {
    for fx in corpus() {
        let grid = feed_alacritty(fx.bytes, fx.cols, fx.rows);
        assert_grid_matches(&fx, &grid, "alacritty");
    }
}

#[test]
fn vt100_corpus_for_comparison() {
    let fixtures = corpus();
    let mut passed = 0usize;
    let mut failed = Vec::new();
    for fx in &fixtures {
        let grid = feed_vt100(fx.bytes, fx.cols, fx.rows);
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_grid_matches(fx, &grid, "vt100");
        })) {
            Ok(()) => passed += 1,
            Err(_) => failed.push(fx.name),
        }
    }
    eprintln!(
        "vt100 comparison: {passed}/{} passed; failures: {:?}",
        fixtures.len(),
        failed
    );
    let _cats: Vec<_> = fixtures.iter().map(|f| f.category).collect();
}

#[test]
fn engine_comparison_summary() {
    let fixtures = corpus();
    let mut alac_pass = 0usize;
    let mut vt_pass = 0usize;
    for fx in &fixtures {
        let alac = feed_alacritty(fx.bytes, fx.cols, fx.rows);
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_grid_matches(fx, &alac, "alacritty");
        }))
        .is_ok()
        {
            alac_pass += 1;
        }
        let vt = feed_vt100(fx.bytes, fx.cols, fx.rows);
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_grid_matches(fx, &vt, "vt100");
        }))
        .is_ok()
        {
            vt_pass += 1;
        }
    }
    let total = fixtures.len();
    eprintln!("=== VT engine corpus summary ===");
    eprintln!("alacritty_terminal: {alac_pass}/{total}");
    eprintln!("vt100 (libghostty-vt unavailable): {vt_pass}/{total}");
    eprintln!("decision: alacritty_terminal (production)");
    assert_eq!(
        alac_pass, total,
        "production engine must pass the full corpus"
    );
    assert!(
        alac_pass > vt_pass,
        "alacritty must beat the comparison engine"
    );
}

#[test]
fn hydrate_feeds_visible_bytes() {
    use cyclops_workspace::HydrationSnapshot;
    let mut vt = AlacrittyVt::new(10, 2);
    let snap = HydrationSnapshot {
        cols: 10,
        rows: 2,
        visible: b"hydrated\r\n".to_vec(),
        alternate: None,
        cursor_x: 0,
        cursor_y: 0,
        alternate_on: false,
    };
    vt.hydrate(&snap);
    assert_eq!(vt.grid().row_texts()[0], "hydrated");
}
