//! One live pane's VT runtime: Alacritty state, hydration, scrollback,
//! selection, cursor, and visible-cell iteration.
//!
//! F35 settled the engine choice (alacritty_terminal 12/12 against the
//! corpus), so the runtime owns the engine directly — one concrete
//! implementation, no wrapper layer and no speculative engine trait.
//! Renderers visit cells through [`PaneRuntime::for_each_visible_cell`];
//! tests that need owned values take a [`PaneRuntime::snapshot`].

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::term::cell::{Cell as AlacCell, Flags};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{self, Processor};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor};

use super::grid::{
    CellAttrs, CellGrid, CellPos, Color, CursorShape, CursorState, GridCell, HydrationSnapshot,
    Underline,
};

struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

struct NullListener;

impl EventListener for NullListener {
    fn send_event(&self, _event: Event) {}
}

/// A pane's terminal state, backed by Alacritty's emulation core.
pub struct PaneRuntime {
    term: Term<NullListener>,
    parser: Processor,
    cols: u16,
    rows: u16,
}

impl PaneRuntime {
    pub fn new(cols: u16, rows: u16) -> Self {
        let size = Size {
            cols: cols as usize,
            rows: rows as usize,
        };
        Self {
            term: Term::new(Config::default(), &size, NullListener),
            parser: Processor::new(),
            cols,
            rows,
        }
    }

    /// Feed raw PTY bytes from tmux `%output`.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Resize the visible grid.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        let size = Size {
            cols: cols as usize,
            rows: rows as usize,
        };
        self.term.resize(size);
    }

    /// Replace every parser and terminal-state bit with a clean grid.
    ///
    /// Hydration is an authoritative visual checkpoint after continuity was
    /// lost. Replaying it over the old terminal would retain private modes,
    /// saved cursors, and stale primary/alternate buffers that the capture
    /// cannot describe.
    fn reset(&mut self, cols: u16, rows: u16) {
        let size = Size {
            cols: cols as usize,
            rows: rows as usize,
        };
        self.term = Term::new(Config::default(), &size, NullListener);
        self.parser = Processor::new();
        self.cols = cols;
        self.rows = rows;
    }

    /// Hydrate from a tmux bundle. Visual snapshot only — not parser-exact.
    pub fn hydrate(&mut self, snapshot: &HydrationSnapshot) {
        self.reset(snapshot.cols, snapshot.rows);

        // A capture restores pixels, not VT modes. Full-screen TUIs such as
        // Claude and Codex are already in the alternate buffer; record that
        // fact before replaying their pixels. Otherwise their next exit or
        // redraw sequence switches buffers relative to the wrong baseline
        // and can make the restored UI disappear.
        //
        // Order matters: tmux's saved grid is the *primary* screen from
        // before the TUI started, so it has to be laid down first and then
        // covered by entering the alternate screen. Replaying it after the
        // switch paints the stale shell over the live TUI (F38).
        if snapshot.alternate_on {
            if let Some(saved) = snapshot.saved_primary.as_deref() {
                self.replay_rows(saved);
            }
            self.feed(b"\x1b[?1049h");
        }
        self.replay_rows(&snapshot.visible);

        // The replay leaves the cursor after the last capture cell; move it
        // to where the pane really has it.
        let seq = format!(
            "\x1b[{};{}H",
            snapshot.cursor_y.saturating_add(1),
            snapshot.cursor_x.saturating_add(1)
        );
        self.feed(seq.as_bytes());
    }

    /// Rehydrate after flow-control pause: never trust resumed byte continuity.
    pub fn rehydrate(&mut self, snapshot: &HydrationSnapshot) {
        self.hydrate(snapshot);
    }

    /// Replay captured rows into the active buffer.
    ///
    /// Capture rows arrive joined with bare LF; a VT treats LF as index-down
    /// without carriage return, so each row needs an explicit CRLF or the
    /// columns staircase.
    fn replay_rows(&mut self, bytes: &[u8]) {
        self.feed(b"\x1b[H\x1b[2J");
        for (i, row) in bytes.split(|&b| b == b'\n').enumerate() {
            if i > 0 {
                self.feed(b"\r\n");
            }
            self.feed(row);
        }
    }

    /// Grid dimensions as `(cols, rows)`.
    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Whether the viewport is at the live tail (not scrolled into history).
    ///
    /// Derived from the engine's display offset rather than a second stored
    /// copy: new output arriving while the user reads history moves that
    /// offset to keep the view pinned, and a cached value would go stale.
    pub fn at_tail(&self) -> bool {
        self.term.grid().display_offset() == 0
    }

    /// Visit every visible cell once, in row-major order.
    ///
    /// This is the production render path: one translation from engine
    /// cells to the caller's buffer, with no intermediate full-grid copy.
    /// The engine's display iterator already accounts for the scrollback
    /// display offset, so a pinned viewport visits history, not the tail.
    pub fn for_each_visible_cell(&self, mut f: impl FnMut(u16, u16, GridCell)) {
        let cols = (self.cols as usize).max(1);
        let cell_count = cols * self.rows as usize;
        for (i, cell) in self.term.grid().display_iter().enumerate() {
            if i >= cell_count {
                break;
            }
            f((i % cols) as u16, (i / cols) as u16, cell_from_alac(&cell));
        }
    }

    /// An owned copy of the visible grid.
    ///
    /// Golden tests need values that outlive the borrow of the runtime;
    /// production rendering visits cells through
    /// [`Self::for_each_visible_cell`] instead of paying for this copy.
    pub fn snapshot(&self) -> CellGrid {
        let cell_count = self.cols as usize * self.rows as usize;
        let mut cells = Vec::with_capacity(cell_count);
        // The visit is row-major, so push order is index order; padding
        // covers an engine that yields fewer cells than the full viewport.
        self.for_each_visible_cell(|_, _, cell| cells.push(cell));
        cells.resize(cell_count, GridCell::default());
        CellGrid {
            cols: self.cols,
            rows: self.rows,
            cells,
        }
    }

    /// One visible row, exactly one `char` per column: a wide character
    /// sits at its own column and its spacer column reads as a space, so a
    /// caller's char index is always a column index. This is what word
    /// selection reads.
    ///
    /// Zero-width marks (`GridCell::zerowidth`) are deliberately excluded:
    /// they carry no column of their own, and splicing them in would break
    /// the one-char-per-column contract word selection's column math
    /// depends on. `CellGrid::row_texts` is the grapheme-accurate view for
    /// tests that need to see what a user sees.
    pub fn row_text(&self, row: u16) -> String {
        let mut out = vec![' '; self.cols as usize];
        self.for_each_visible_cell(|col, r, cell| {
            if r == row && !cell.wide_spacer && cell.ch != '\0' {
                out[col as usize] = cell.ch;
            }
        });
        out.into_iter().collect()
    }

    /// Cursor in visible coordinates.
    pub fn cursor(&self) -> CursorState {
        let point = self.term.grid().cursor.point;
        let style = self.term.cursor_style();
        let shape = match style.shape {
            ansi::CursorShape::Underline => CursorShape::Underline,
            ansi::CursorShape::Beam => CursorShape::Bar,
            ansi::CursorShape::Hidden => CursorShape::Block,
            _ => CursorShape::Block,
        };
        CursorState {
            col: point.column.0 as u16,
            row: point.line.0 as u16,
            // DECTCEM (`\x1b[?25l`) hides the cursor through a terminal mode,
            // not through the cursor style. A TUI that hides its cursor while
            // repainting must not get a block painted over its output.
            visible: self.term.mode().contains(TermMode::SHOW_CURSOR)
                && style.shape != ansi::CursorShape::Hidden,
            shape,
            blink: style.blinking,
        }
    }

    /// Scroll the viewport through history.
    pub fn scroll(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(-delta));
    }

    /// Extract selected text between two cell positions.
    pub fn select(&mut self, from: CellPos, to: CellPos) -> Option<String> {
        use alacritty_terminal::selection::{Selection, SelectionType};
        let start = Point::new(Line(from.row as i32), Column(from.col as usize));
        let end = Point::new(Line(to.row as i32), Column(to.col as usize));
        self.term.selection = Some(Selection::new(SelectionType::Simple, start, Side::Left));
        self.term.selection.as_mut()?.update(end, Side::Right);
        self.term.selection_to_string()
    }
}

fn cell_from_alac(cell: &AlacCell) -> GridCell {
    let flags = cell.flags;
    GridCell {
        ch: cell.c,
        // Combining marks and variation selectors: the engine stores these
        // in the cell's extra storage rather than a column of their own
        // (`Cell::zerowidth`), so copying only `c` drops them.
        zerowidth: cell.zerowidth().map(<[char]>::to_vec).unwrap_or_default(),
        wide_spacer: flags.contains(Flags::WIDE_CHAR_SPACER),
        attrs: CellAttrs {
            fg: map_color(cell.fg),
            bg: map_color(cell.bg),
            bold: flags.contains(Flags::BOLD),
            italic: flags.contains(Flags::ITALIC),
            underline: map_underline(flags),
            reverse: flags.contains(Flags::INVERSE),
            dim: flags.contains(Flags::DIM),
            hidden: flags.contains(Flags::HIDDEN),
            strikeout: flags.contains(Flags::STRIKEOUT),
        },
    }
}

/// The engine sets one flag per underline style; the widest wins when a
/// stream sets more than one without resetting in between.
fn map_underline(flags: Flags) -> Underline {
    if flags.contains(Flags::DOUBLE_UNDERLINE) {
        Underline::Double
    } else if flags.contains(Flags::UNDERCURL) {
        Underline::Curl
    } else if flags.contains(Flags::DOTTED_UNDERLINE) {
        Underline::Dotted
    } else if flags.contains(Flags::DASHED_UNDERLINE) {
        Underline::Dashed
    } else if flags.contains(Flags::UNDERLINE) {
        Underline::Single
    } else {
        Underline::None
    }
}

fn map_color(c: AnsiColor) -> Color {
    match c {
        AnsiColor::Named(n) => match n {
            NamedColor::Black => Color::Indexed(0),
            NamedColor::Red => Color::Indexed(1),
            NamedColor::Green => Color::Indexed(2),
            NamedColor::Yellow => Color::Indexed(3),
            NamedColor::Blue => Color::Indexed(4),
            NamedColor::Magenta => Color::Indexed(5),
            NamedColor::Cyan => Color::Indexed(6),
            NamedColor::White => Color::Indexed(7),
            NamedColor::BrightBlack => Color::Indexed(8),
            NamedColor::BrightRed => Color::Indexed(9),
            NamedColor::BrightGreen => Color::Indexed(10),
            NamedColor::BrightYellow => Color::Indexed(11),
            NamedColor::BrightBlue => Color::Indexed(12),
            NamedColor::BrightMagenta => Color::Indexed(13),
            NamedColor::BrightCyan => Color::Indexed(14),
            NamedColor::BrightWhite => Color::Indexed(15),
            NamedColor::Foreground | NamedColor::Background | NamedColor::Cursor => Color::Default,
            _ => Color::Indexed(8),
        },
        AnsiColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        AnsiColor::Indexed(i) => Color::Indexed(i),
    }
}

/// Map a tmux adapter bundle into workspace hydration state.
///
/// `capture-pane -a` reads tmux's *saved* grid, which is the primary screen
/// from before a TUI entered the alternate buffer — not the alternate buffer
/// itself. The adapter still spells that field `alternate_escaped`; the
/// workspace names it for what it holds (F38).
pub fn snapshot_from_bundle(bundle: &cyclops_tmux::HydrationBundle) -> HydrationSnapshot {
    HydrationSnapshot {
        cols: bundle.cols,
        rows: bundle.rows,
        visible: bundle.visible_escaped.clone(),
        saved_primary: bundle.alternate_escaped.clone(),
        cursor_x: bundle.cursor_x,
        cursor_y: bundle.cursor_y,
        alternate_on: bundle.alternate_on,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(bytes: &[u8], cols: u16, rows: u16) -> CellGrid {
        let mut rt = PaneRuntime::new(cols, rows);
        rt.feed(bytes);
        rt.snapshot()
    }

    #[test]
    fn plain_hello() {
        let grid = render(b"hello\r\n", 10, 3);
        assert_eq!(grid.row_texts()[0], "hello");
    }

    #[test]
    fn alternate_hydration_restores_the_buffer_mode_as_well_as_pixels() {
        let mut rt = PaneRuntime::new(10, 2);
        rt.hydrate(&HydrationSnapshot {
            cols: 10,
            rows: 2,
            // What the user is looking at: the agent TUI.
            visible: b"CLAUDE".to_vec(),
            // What tmux saved when that TUI took the alternate screen.
            saved_primary: Some(b"shell".to_vec()),
            cursor_x: 6,
            cursor_y: 0,
            alternate_on: true,
        });

        assert!(
            rt.term.mode().contains(TermMode::ALT_SCREEN),
            "a visual alternate-screen capture must not be replayed into the primary buffer"
        );
        assert_eq!(rt.snapshot().row_texts()[0], "CLAUDE");
    }

    #[test]
    fn leaving_a_hydrated_alternate_screen_reveals_the_saved_shell() {
        let mut rt = PaneRuntime::new(10, 2);
        rt.hydrate(&HydrationSnapshot {
            cols: 10,
            rows: 2,
            visible: b"CLAUDE".to_vec(),
            saved_primary: Some(b"shell".to_vec()),
            cursor_x: 6,
            cursor_y: 0,
            alternate_on: true,
        });

        rt.feed(b"\x1b[?1049l");
        assert_eq!(
            rt.snapshot().row_texts()[0],
            "shell",
            "the saved primary must be underneath, so the TUI's own exit works"
        );
    }

    #[test]
    fn hydration_discards_stale_terminal_state() {
        let mut rt = PaneRuntime::new(10, 2);
        rt.feed(b"stale");
        rt.hydrate(&HydrationSnapshot {
            cols: 10,
            rows: 2,
            visible: b"new".to_vec(),
            saved_primary: None,
            cursor_x: 3,
            cursor_y: 0,
            alternate_on: false,
        });

        assert_eq!(rt.snapshot().row_texts()[0], "new");
        assert!(!rt.term.mode().contains(TermMode::ALT_SCREEN));
    }

    #[test]
    fn row_text_indexes_columns_even_across_wide_characters() {
        let mut rt = PaneRuntime::new(8, 1);
        rt.feed("你ab".as_bytes());
        let row = rt.row_text(0);
        let chars: Vec<char> = row.chars().collect();
        assert_eq!(chars[0], '你');
        assert_eq!(chars[1], ' ', "the spacer column must hold a plain space");
        assert_eq!(chars[2], 'a');
        assert_eq!(chars[3], 'b');
        assert_eq!(chars.len(), 8, "one char per column, always");
    }
}
