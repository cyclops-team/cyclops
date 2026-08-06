//! The bridge-fidelity floor: what a pane runtime must preserve on the way
//! from engine state to the cells a renderer paints.
//!
//! `corpus.rs` proves the engine *parses* a sequence. These fixtures prove
//! Cyclops does not *lose* the result while converting it. Everything here is
//! deterministic and pure — no tmux, no testrig, no timing.
//!
//! The goal is not terminal-protocol completeness. It is an explicit floor
//! for the programs Cyclops ships support for.

use cyclops_workspace::{CellGrid, Color, HydrationSnapshot, PaneRuntime, Underline};

/// Feed one byte stream into a fresh runtime and take an owned snapshot.
fn render(bytes: &[u8], cols: u16, rows: u16) -> CellGrid {
    let mut rt = PaneRuntime::new(cols, rows);
    rt.feed(bytes);
    rt.snapshot()
}

/// The attributes of the first cell whose character is `ch`.
fn attrs_of(grid: &CellGrid, ch: char) -> cyclops_workspace::CellAttrs {
    for row in 0..grid.rows {
        for col in 0..grid.cols {
            if let Some(cell) = grid.cell(col, row) {
                if cell.ch == ch {
                    return cell.attrs.clone();
                }
            }
        }
    }
    panic!("no cell holding {ch:?} in {:?}", grid.row_texts());
}

// ---------------------------------------------------------------- wide cells

#[test]
fn a_wide_character_keeps_its_spacer_column() {
    let grid = render("你a".as_bytes(), 6, 1);

    assert_eq!(grid.cell(0, 0).unwrap().ch, '你');
    assert!(
        grid.cell(1, 0).unwrap().wide_spacer,
        "the column under a wide char's right half must be marked, or the \
         renderer prints a duplicate glyph"
    );
    assert_eq!(grid.cell(2, 0).unwrap().ch, 'a');
    assert_eq!(grid.row_texts()[0], "你a");
}

#[test]
fn a_wide_character_that_cannot_fit_the_last_column_wraps_whole() {
    // Five columns, four taken: 你 cannot straddle the edge, so it wraps
    // rather than splitting across two rows.
    let grid = render("abcd你".as_bytes(), 5, 2);

    assert_eq!(grid.row_texts()[0], "abcd");
    assert_eq!(
        grid.row_texts()[1],
        "你",
        "a wide char must move to the next row intact"
    );
}

#[test]
fn a_narrower_resize_reflows_wide_characters_without_splitting_them() {
    let mut rt = PaneRuntime::new(8, 2);
    rt.feed("你好".as_bytes());
    rt.resize(4, 2);

    let grid = rt.snapshot();
    assert_eq!(grid.cols, 4);
    let joined = grid.row_texts().join("");
    assert!(
        !joined.contains('\u{fffd}'),
        "resize must not leave a broken half-character: {joined:?}"
    );
}

#[test]
fn a_combining_mark_stays_with_its_base_character() {
    // e + U+0301 COMBINING ACUTE ACCENT occupies one cell.
    let grid = render("e\u{301}x".as_bytes(), 6, 1);

    let base = grid.cell(0, 0).unwrap();
    assert_eq!(base.ch, 'e', "the base character must still own the column");
    assert_eq!(
        base.zerowidth,
        vec!['\u{301}'],
        "the accent must be kept, not dropped, or an accented letter \
         renders as the bare base letter"
    );
    assert_eq!(
        grid.cell(1, 0).unwrap().ch,
        'x',
        "a combining mark must not consume a column of its own"
    );
    assert_eq!(
        grid.row_texts()[0],
        "e\u{301}x",
        "the row text a golden test reads must carry the accent too"
    );
}

#[test]
fn an_emoji_occupies_two_columns() {
    let grid = render("😀x".as_bytes(), 6, 1);

    assert_eq!(grid.cell(0, 0).unwrap().ch, '😀');
    assert!(
        grid.cell(1, 0).unwrap().wide_spacer,
        "an emoji is double-width; the spacer column must say so"
    );
    assert_eq!(grid.cell(2, 0).unwrap().ch, 'x');
}

/// Measured, not assumed: the engine sizes by the character's own width
/// class and does not widen a narrow glyph when VS16 asks for emoji
/// presentation. Pinned so a future engine bump surfaces the change rather
/// than silently shifting every warning glyph by a column.
#[test]
fn a_variation_selector_does_not_widen_a_narrow_glyph() {
    let grid = render("⚠\u{fe0f}x".as_bytes(), 6, 1);

    let base = grid.cell(0, 0).unwrap();
    assert_eq!(base.ch, '⚠');
    assert_eq!(
        base.zerowidth,
        vec!['\u{fe0f}'],
        "the selector must be kept, not dropped, or the glyph loses its \
         emoji-presentation request"
    );
    assert!(!grid.cell(1, 0).unwrap().wide_spacer);
    assert_eq!(grid.cell(1, 0).unwrap().ch, 'x');
    assert_eq!(grid.row_texts()[0], "⚠\u{fe0f}x");
}

// ------------------------------------------------------------------- styling

#[test]
fn hidden_survives_the_bridge() {
    let grid = render(b"\x1b[8mH\x1b[0m", 4, 1);
    assert!(
        attrs_of(&grid, 'H').hidden,
        "concealed text must stay concealed or a password prompt leaks"
    );
}

#[test]
fn strikeout_survives_the_bridge() {
    let grid = render(b"\x1b[9mS\x1b[0m", 4, 1);
    assert!(attrs_of(&grid, 'S').strikeout);
}

#[test]
fn reverse_video_survives_the_bridge() {
    let grid = render(b"\x1b[7mR\x1b[0m", 4, 1);
    assert!(attrs_of(&grid, 'R').reverse);
}

#[test]
fn every_underline_style_keeps_its_own_identity() {
    // SGR 4 / 21 / 4:3 / 4:4 / 4:5 — single, double, curl, dotted, dashed.
    // 4:2 is double; bare SGR 21 is bold-off on a modern terminal, not an
    // underline at all.
    for (bytes, expected) in [
        (b"\x1b[4mU\x1b[0m".as_slice(), Underline::Single),
        (b"\x1b[4:2mU\x1b[0m".as_slice(), Underline::Double),
        (b"\x1b[4:3mU\x1b[0m".as_slice(), Underline::Curl),
        (b"\x1b[4:4mU\x1b[0m".as_slice(), Underline::Dotted),
        (b"\x1b[4:5mU\x1b[0m".as_slice(), Underline::Dashed),
    ] {
        let grid = render(bytes, 4, 1);
        assert_eq!(
            attrs_of(&grid, 'U').underline,
            expected,
            "underline style flattened for {bytes:?}"
        );
    }
}

#[test]
fn a_reset_clears_every_style_bit() {
    let grid = render(b"\x1b[1;3;4;7;8;9mX\x1b[0mY", 6, 1);
    let plain = attrs_of(&grid, 'Y');

    assert!(!plain.bold);
    assert!(!plain.italic);
    assert!(!plain.reverse);
    assert!(!plain.hidden);
    assert!(!plain.strikeout);
    assert_eq!(plain.underline, Underline::None);
}

#[test]
fn each_color_family_reaches_the_cell_unflattened() {
    // default, indexed, bright, and truecolor foregrounds paired with a
    // background, plus a dim cell that must not be confused for a color.
    let grid = render(b"\x1b[31;42mA\x1b[0m", 4, 1);
    let a = attrs_of(&grid, 'A');
    assert_eq!(a.fg, Color::Indexed(1));
    assert_eq!(a.bg, Color::Indexed(2));

    let grid = render(b"\x1b[91;102mB\x1b[0m", 4, 1);
    let b = attrs_of(&grid, 'B');
    assert_eq!(b.fg, Color::Indexed(9), "bright fg must not fold to normal");
    assert_eq!(b.bg, Color::Indexed(10));

    let grid = render(b"\x1b[38;5;196;48;5;21mC\x1b[0m", 4, 1);
    let c = attrs_of(&grid, 'C');
    assert_eq!(c.fg, Color::Indexed(196));
    assert_eq!(c.bg, Color::Indexed(21));

    let grid = render(b"\x1b[38;2;255;128;64;48;2;1;2;3mD\x1b[0m", 4, 1);
    let d = attrs_of(&grid, 'D');
    assert_eq!(
        d.fg,
        Color::Rgb(255, 128, 64),
        "truecolor must not quantize"
    );
    assert_eq!(d.bg, Color::Rgb(1, 2, 3));

    let grid = render(b"\x1b[2mE\x1b[0m", 4, 1);
    let e = attrs_of(&grid, 'E');
    assert!(e.dim);
    assert_eq!(e.fg, Color::Default, "dim is an attribute, not a color");

    let grid = render(b"F", 4, 1);
    assert_eq!(attrs_of(&grid, 'F').fg, Color::Default);
}

// -------------------------------------------------------------------- cursor

#[test]
fn the_cursor_reports_its_position_visibility_and_shape() {
    use cyclops_workspace::CursorShape;

    let mut rt = PaneRuntime::new(10, 3);
    rt.feed(b"\x1b[2;4H");
    let c = rt.cursor();
    assert_eq!((c.col, c.row), (3, 1), "CUP is 1-based; cells are 0-based");
    assert!(c.visible);

    // DECTCEM off, then on again.
    rt.feed(b"\x1b[?25l");
    assert!(!rt.cursor().visible, "a hidden cursor must not be painted");
    rt.feed(b"\x1b[?25h");
    assert!(rt.cursor().visible);

    // DECSCUSR: even numbers are steady, odd are blinking; both halves
    // must survive to the host cursor or a pane's insert-mode bar renders
    // as whatever the previous pane left behind.
    for (bytes, shape, blink) in [
        (b"\x1b[1 q".as_slice(), CursorShape::Block, true),
        (b"\x1b[2 q".as_slice(), CursorShape::Block, false),
        (b"\x1b[3 q".as_slice(), CursorShape::Underline, true),
        (b"\x1b[4 q".as_slice(), CursorShape::Underline, false),
        (b"\x1b[5 q".as_slice(), CursorShape::Bar, true),
        (b"\x1b[6 q".as_slice(), CursorShape::Bar, false),
    ] {
        rt.feed(bytes);
        let c = rt.cursor();
        assert_eq!((c.shape, c.blink), (shape, blink), "for {bytes:?}");
    }
}

// ----------------------------------------------------------- alternate screen

#[test]
fn the_alternate_screen_redraws_and_exits_back_to_hydrated_content() {
    let mut rt = PaneRuntime::new(10, 2);
    rt.hydrate(&HydrationSnapshot {
        cols: 10,
        rows: 2,
        visible: b"shell".to_vec(),
        saved_primary: None,
        cursor_x: 5,
        cursor_y: 0,
        alternate_on: false,
    });
    assert_eq!(rt.snapshot().row_texts()[0], "shell");

    // A TUI takes the alternate screen, paints, repaints, then leaves.
    rt.feed(b"\x1b[?1049h\x1b[H\x1b[2Jfirst");
    assert_eq!(rt.snapshot().row_texts()[0], "first");

    rt.feed(b"\x1b[H\x1b[2Jsecond");
    assert_eq!(rt.snapshot().row_texts()[0], "second");

    rt.feed(b"\x1b[?1049l");
    assert_eq!(
        rt.snapshot().row_texts()[0],
        "shell",
        "leaving the alternate screen must reveal the primary buffer again"
    );
}

#[test]
fn hydrating_an_alternate_capture_lands_in_the_alternate_buffer() {
    let mut rt = PaneRuntime::new(10, 2);
    rt.hydrate(&HydrationSnapshot {
        cols: 10,
        rows: 2,
        // `capture-pane` reads the screen in front of the user — the TUI.
        visible: b"TUI".to_vec(),
        // `capture-pane -a` reads tmux's saved grid — the shell behind it.
        saved_primary: Some(b"shell".to_vec()),
        cursor_x: 3,
        cursor_y: 0,
        alternate_on: true,
    });
    assert_eq!(rt.snapshot().row_texts()[0], "TUI");

    // Because the mode was restored too, the TUI's own exit still works.
    rt.feed(b"\x1b[?1049l");
    assert_eq!(
        rt.snapshot().row_texts()[0],
        "shell",
        "a restored alternate capture must exit relative to the right baseline"
    );
}

// ----------------------------------------------------------- chunked delivery

#[test]
fn an_escape_sequence_split_across_chunks_still_applies() {
    let mut rt = PaneRuntime::new(10, 1);
    // The control client hands over whatever bytes arrived; a CSI can land
    // in two %output notifications.
    rt.feed(b"\x1b[3");
    rt.feed(b"1mZ\x1b[0m");

    assert_eq!(
        attrs_of(&rt.snapshot(), 'Z').fg,
        Color::Indexed(1),
        "parser state must persist across feeds"
    );
}

#[test]
fn a_utf8_character_split_across_chunks_is_not_corrupted() {
    let mut rt = PaneRuntime::new(6, 1);
    let bytes = "好".as_bytes();
    rt.feed(&bytes[..1]);
    rt.feed(&bytes[1..]);

    assert_eq!(rt.snapshot().row_texts()[0], "好");
}

#[test]
fn synchronized_output_markers_do_not_reach_the_grid() {
    // DECSET 2026 brackets an atomic repaint. Whether or not the engine
    // defers, the markers themselves must never print as text.
    let grid = render(b"\x1b[?2026hab\x1b[?2026l", 6, 1);
    assert_eq!(grid.row_texts()[0], "ab");
}

// ---------------------------------------------------------------- scrollback

#[test]
fn a_pinned_scrollback_viewport_does_not_follow_new_output() {
    let mut rt = PaneRuntime::new(10, 3);
    for i in 0..20 {
        rt.feed(format!("line{i}\r\n").as_bytes());
    }
    assert!(rt.at_tail());

    // Negative scrolls back into history, matching the wheel-up direction
    // the mouse handler sends.
    rt.scroll(-5);
    assert!(!rt.at_tail(), "scrolling back must leave the live tail");
    let pinned = rt.snapshot().row_texts();

    rt.feed(b"newest\r\n");
    assert_eq!(
        rt.snapshot().row_texts(),
        pinned,
        "output arriving while the user reads history must not yank the view"
    );
    assert!(!rt.at_tail());

    // Returning to the tail shows the new line.
    rt.scroll(100);
    assert!(rt.at_tail());
    assert!(rt.snapshot().row_texts().iter().any(|r| r == "newest"));
}

// ------------------------------------------------- shipped agent TUI captures

/// Recorded fragments from the agent CLIs Cyclops ships manifests for. Each
/// is the shape that drives a workspace decision, not a whole screen.
#[test]
fn shipped_agent_tui_output_renders_as_the_user_sees_it() {
    // Claude: OSC title set, then a bold spinner and prompt box.
    let grid = render(
        b"\x1b]0;Claude\x07\x1b[1m\xe2\x9c\xbb\x1b[0m Thinking\xe2\x80\xa6\r\n",
        24,
        2,
    );
    assert_eq!(
        grid.row_texts()[0],
        "✻ Thinking…",
        "an OSC title must not print into the grid"
    );

    // Codex: dim ghost text ahead of typed input on one line. Assert by
    // column — both words contain a 't'.
    let grid = render(b"\x1b[2mghost\x1b[0mtyped", 24, 1);
    assert_eq!(grid.row_texts()[0], "ghosttyped");
    assert!(
        grid.cell(0, 0).unwrap().attrs.dim,
        "ghost text must stay dim"
    );
    assert!(
        !grid.cell(5, 0).unwrap().attrs.dim,
        "typed text must not be dim"
    );

    // Cursor: a box-drawing frame around a prompt.
    let grid = render("╭──╮\r\n│ok│\r\n╰──╯".as_bytes(), 8, 3);
    assert_eq!(grid.row_texts(), vec!["╭──╮", "│ok│", "╰──╯"]);
}
