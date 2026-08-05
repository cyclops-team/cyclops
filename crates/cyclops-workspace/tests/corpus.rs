//! VT fidelity fixture corpus. Pure tests — no tmux, no testrig.
//!
//! Asserts the production engine (`alacritty_terminal`) against every
//! recorded fixture. F35 settled the engine choice — alacritty 12/12 against
//! `vt100` 5/12 — so that score lives in `findings.md`, not in every run.

use cyclops_workspace::{CellGrid, Color, PaneRuntime};

/// Feed one recorded byte stream into a fresh runtime and take an owned
/// snapshot of the visible grid.
fn feed(bytes: &[u8], cols: u16, rows: u16) -> CellGrid {
    let mut rt = PaneRuntime::new(cols, rows);
    rt.feed(bytes);
    rt.snapshot()
}

/// One recorded byte stream and its expected visible grid.
struct Fixture {
    name: &'static str,
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
        let grid = feed(fx.bytes, fx.cols, fx.rows);
        assert_grid_matches(&fx, &grid, "alacritty");
    }
}

#[test]
fn hydrate_feeds_visible_bytes() {
    use cyclops_workspace::HydrationSnapshot;
    let mut rt = PaneRuntime::new(10, 2);
    let snap = HydrationSnapshot {
        cols: 10,
        rows: 2,
        visible: b"hydrated\r\n".to_vec(),
        saved_primary: None,
        cursor_x: 0,
        cursor_y: 0,
        alternate_on: false,
    };
    rt.hydrate(&snap);
    assert_eq!(rt.snapshot().row_texts()[0], "hydrated");
}
