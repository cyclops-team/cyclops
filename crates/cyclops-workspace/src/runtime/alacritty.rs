//! `alacritty_terminal` pane engine — production choice after corpus.

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::term::cell::{Cell as AlacCell, Flags};
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{self, Processor};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor};

use super::grid::{
    CellAttrs, CellGrid, CellGridView, CellPos, Color, CursorShape, CursorState, GridCell,
    HydrationSnapshot,
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

/// Pane VT backed by Alacritty's emulation core.
pub struct AlacrittyVt {
    term: Term<NullListener>,
    parser: Processor,
    cols: u16,
    rows: u16,
    scroll_offset: usize,
    cached_grid: CellGrid,
}

impl AlacrittyVt {
    pub fn new(cols: u16, rows: u16) -> Self {
        let size = Size {
            cols: cols as usize,
            rows: rows as usize,
        };
        let term = Term::new(Config::default(), &size, NullListener);
        let cached_grid = CellGrid {
            cols,
            rows,
            cells: vec![GridCell::default(); cols as usize * rows as usize],
        };
        Self {
            term,
            parser: Processor::new(),
            cols,
            rows,
            scroll_offset: 0,
            cached_grid,
        }
    }

    fn feed_internal(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
        self.refresh_grid();
    }

    fn refresh_grid(&mut self) {
        let cols = self.cols;
        let rows = self.rows;
        let mut cells = Vec::with_capacity(cols as usize * rows as usize);
        for cell in self.term.grid().display_iter() {
            cells.push(cell_from_alac(&cell));
        }
        while cells.len() < cols as usize * rows as usize {
            cells.push(GridCell::default());
        }
        self.cached_grid = CellGrid { cols, rows, cells };
    }

    fn build_grid(&self) -> CellGrid {
        self.cached_grid.clone()
    }
}

fn cell_from_alac(cell: &AlacCell) -> GridCell {
    let flags = cell.flags;
    GridCell {
        ch: cell.c,
        wide_spacer: flags.contains(Flags::WIDE_CHAR_SPACER),
        attrs: CellAttrs {
            fg: map_color(cell.fg),
            bg: map_color(cell.bg),
            bold: flags.contains(Flags::BOLD),
            italic: flags.contains(Flags::ITALIC),
            underline: flags.contains(Flags::UNDERLINE),
            reverse: flags.contains(Flags::INVERSE),
            dim: flags.contains(Flags::DIM),
        },
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

impl AlacrittyVt {
    /// Feed raw PTY bytes from tmux `%output`.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.feed_internal(bytes);
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
        self.scroll_offset = 0;
        self.refresh_grid();
    }

    /// Initialize from a tmux hydration bundle.
    pub fn hydrate(&mut self, snapshot: &HydrationSnapshot) {
        self.resize(snapshot.cols, snapshot.rows);
        let bytes = if snapshot.alternate_on {
            snapshot.alternate.as_deref().unwrap_or(&snapshot.visible)
        } else {
            &snapshot.visible
        };
        // Capture rows arrive joined with bare LF; a VT treats LF as
        // index-down without carriage return, so feed each row with CRLF or
        // columns staircase.
        for (i, row) in bytes.split(|&b| b == b'\n').enumerate() {
            if i > 0 {
                self.feed_internal(b"\r\n");
            }
            self.feed_internal(row);
        }
        // The replay leaves the cursor after the last capture cell; move it
        // to where the pane really has it.
        let seq = format!(
            "\x1b[{};{}H",
            snapshot.cursor_y.saturating_add(1),
            snapshot.cursor_x.saturating_add(1)
        );
        self.feed_internal(seq.as_bytes());
    }

    /// Visible cells and attributes.
    pub fn grid(&self) -> CellGridView<'_> {
        CellGridView {
            grid: &self.cached_grid,
        }
    }

    /// Grid dimensions as `(cols, rows)`.
    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Whether the viewport is at the live tail (not scrolled into history).
    pub fn at_tail(&self) -> bool {
        self.scroll_offset == 0
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
            visible: style.shape != ansi::CursorShape::Hidden,
            shape,
        }
    }

    /// Scroll the viewport through history.
    pub fn scroll(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(-delta));
        self.scroll_offset = self.term.grid().display_offset();
        self.refresh_grid();
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

/// Feed bytes into a fresh Alacritty term and return the visible grid.
pub fn feed_alacritty(bytes: &[u8], cols: u16, rows: u16) -> CellGrid {
    let mut vt = AlacrittyVt::new(cols, rows);
    vt.feed(bytes);
    vt.build_grid()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_hello() {
        let grid = feed_alacritty(b"hello\r\n", 10, 3);
        assert_eq!(grid.row_texts()[0], "hello");
    }
}
