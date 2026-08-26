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
    /// The engine's own floor, which it documents but does not enforce:
    /// `Term::resize` takes whatever it is given, and a grid built at zero
    /// width panics on the first byte that writes a cell (`row[Column(0)]`
    /// on an empty row). Dimensions arrive from tmux layout parsing and can
    /// pass through zero while windows churn — a nested tmux client
    /// redrawing over ssh is exactly the workload that churns them — so
    /// the clamp lives here, on every path that sizes the engine, and not
    /// in any single caller.
    fn clamped(cols: u16, rows: u16) -> (u16, u16) {
        (cols.max(2), rows.max(1))
    }

    pub fn new(cols: u16, rows: u16) -> Self {
        let (cols, rows) = Self::clamped(cols, rows);
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
        let (cols, rows) = Self::clamped(cols, rows);
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
        let (cols, rows) = Self::clamped(cols, rows);
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
    ///
    /// The capture describes the screen; scrollback this runtime already
    /// accumulated is carried across the reset as plain text. Rehydration
    /// runs on every pane resize, and wiping history there would kill the
    /// scroll wheel right after the user resizes a pane.
    pub fn hydrate(&mut self, snapshot: &HydrationSnapshot) {
        let history = self.history_lines();
        self.reset(snapshot.cols, snapshot.rows);
        if history.is_empty() && !snapshot.history.is_empty() {
            // First sight of this pane: tmux's transcript is the history.
            // Without it the wheel hits a wall at the attach moment, and
            // the only thing that "fixed" it was a resize resetting the
            // viewport to the tail. Local history wins on every later
            // hydrate: it already contains everything tmux would say,
            // plus whatever arrived since.
            let lines: Vec<String> = String::from_utf8_lossy(&snapshot.history)
                .split('\n')
                .map(str::to_string)
                .collect();
            self.refill_history(&lines);
        } else {
            self.refill_history(&history);
        }

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

        // A capture restores pixels, not modes either: DECSET 1000/1002/1003
        // (mouse tracking) and 1006 (SGR encoding) are gone from the fresh
        // parser `reset()` built above, same as every other mode. This
        // runtime's mode is only ever read as a gate for wheel forwarding
        // (`wants_sgr_mouse_wheel`), and tmux reports the wheel identically
        // under all three tracking variants, so re-asserting plain click
        // tracking (1000h) regardless of which variant the pane actually
        // set is coarse but cannot mislead that gate.
        if snapshot.mouse_on {
            self.feed(b"\x1b[?1000h");
        }
        if snapshot.mouse_sgr {
            self.feed(b"\x1b[?1006h");
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
    ///
    /// The clear is ED 0 from home, not ED 2: the engine treats ED 2 on the
    /// primary screen as "scroll the viewport into history", which pushes a
    /// phantom blank line even on an empty screen and would bury the
    /// scrollback [`Self::refill_history`] just laid down.
    fn replay_rows(&mut self, bytes: &[u8]) {
        self.feed(b"\x1b[H\x1b[J");
        for (i, row) in bytes.split(|&b| b == b'\n').enumerate() {
            if i > 0 {
                self.feed(b"\r\n");
            }
            self.feed(row);
        }
    }

    /// The scrollback lines the engine holds, oldest first, as plain text.
    /// Soft-wrapped rows come back joined as one logical line, so refeeding
    /// reflows them at the current width. Attributes are not kept: history
    /// survives a rehydrate as text, which is what selection copies anyway.
    fn history_lines(&self) -> Vec<String> {
        let history = self.term.grid().history_size();
        if history == 0 {
            return Vec::new();
        }
        let last = Column((self.cols as usize).saturating_sub(1));
        let text = self.term.bounds_to_string(
            Point::new(Line(-(history as i32)), Column(0)),
            Point::new(Line(-1), last),
        );
        text.split('\n').map(str::to_string).collect()
    }

    /// Refeed saved scrollback into the freshly reset primary screen, then
    /// scroll it fully off so the capture replay finds a blank screen and
    /// history holds exactly the saved lines. The push-off is rows-1 line
    /// feeds: after the trailing CRLF the cursor row is blank, so that count
    /// scrolls every content row into history and not one filler line.
    fn refill_history(&mut self, lines: &[String]) {
        if lines.is_empty() {
            return;
        }
        for line in lines {
            self.feed(line.as_bytes());
            self.feed(b"\r\n");
        }
        for _ in 1..self.rows {
            self.feed(b"\n");
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
        self.scrolled_back() == 0
    }

    /// How many lines back into history the viewport sits (0 at the tail).
    pub fn scrolled_back(&self) -> usize {
        self.term.grid().display_offset()
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

    /// Return the viewport to the live tail. True when it moved.
    pub fn scroll_to_tail(&mut self) -> bool {
        if self.at_tail() {
            return false;
        }
        self.term.scroll_display(Scroll::Bottom);
        true
    }

    /// Whether this pane wants wheel motion delivered as an SGR mouse
    /// report instead of moving this runtime's own scroll offset: mouse
    /// reporting is on and SGR encoding is enabled, the same pair xterm and
    /// tmux both require before treating a wheel notch as an escape
    /// sequence rather than a scrollback nudge.
    pub fn wants_sgr_mouse_wheel(&self) -> bool {
        let mode = self.term.mode();
        mode.intersects(TermMode::MOUSE_MODE) && mode.contains(TermMode::SGR_MOUSE)
    }

    /// Whether the alternate screen is active — this pane's transcript
    /// lives inside the running program, not in this runtime's own
    /// scrollback, so there is nothing local to scroll into.
    pub fn alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// A viewport cell as a grid point, through the current display
    /// offset. This is the only place viewport rows become grid lines:
    /// everything selection-related converts on the way IN and never
    /// stores a viewport coordinate, which is what keeps a selection on
    /// its text when the viewport moves afterwards.
    fn grid_point(&self, cell: CellPos) -> Point {
        let offset = self.scrolled_back() as i32;
        let col = (cell.col as usize).min((self.cols as usize).saturating_sub(1));
        Point::new(Line(cell.row as i32 - offset), Column(col))
    }

    /// Start a selection at a viewport cell. The engine owns it from here:
    /// grid rotation moves it with the text when new output scrolls the
    /// screen, and the display offset projects it back for painting, so
    /// neither scrolling nor fresh output can slide the highlight off what
    /// the user picked.
    pub fn begin_selection(&mut self, cell: CellPos) {
        use alacritty_terminal::selection::{Selection, SelectionType};
        let point = self.grid_point(cell);
        let mut selection = Selection::new(SelectionType::Simple, point, Side::Left);
        selection.include_all();
        self.term.selection = Some(selection);
    }

    /// Move the live end of the selection to a viewport cell.
    ///
    /// `include_all` after every move, because a cell has no half-cell
    /// pointer position to derive a `Side` from. The engine trims a cell
    /// off an endpoint whose side faces away from the selection, which is
    /// right for pixel mice and wrong here: with fixed sides, a leftward
    /// or upward drag lost its first and last cells, and a one-cell
    /// leftward drag selected nothing at all. `include_all` recomputes
    /// both sides from the endpoint order so every drag direction keeps
    /// the cells the operator touched.
    pub fn extend_selection(&mut self, cell: CellPos) {
        let point = self.grid_point(cell);
        if let Some(sel) = self.term.selection.as_mut() {
            sel.update(point, Side::Right);
            sel.include_all();
        }
    }

    /// Select a fixed viewport range in one step (word and line picks).
    pub fn anchor_selection(&mut self, from: CellPos, to: CellPos) {
        self.begin_selection(from);
        self.extend_selection(to);
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    /// The selected text, exactly as the engine holds it: wide characters
    /// once, soft-wrapped rows joined. Never logged.
    pub fn selection_text(&self) -> Option<String> {
        self.term.selection_to_string()
    }

    /// The selection projected onto the current viewport, clamped to its
    /// edges, or None when there is no selection or none of it is on
    /// screen. Painting reads this every frame, which is the other half of
    /// content anchoring: the highlight is recomputed from the text's
    /// position, not remembered from where it was last drawn.
    pub fn selection_screen_range(&self) -> Option<(CellPos, CellPos)> {
        let range = self.term.selection.as_ref()?.to_range(&self.term)?;
        let offset = self.scrolled_back() as i32;
        let start_row = range.start.line.0 + offset;
        let end_row = range.end.line.0 + offset;
        let rows = self.rows as i32;
        if end_row < 0 || start_row >= rows {
            return None;
        }
        let start = if start_row < 0 {
            CellPos { col: 0, row: 0 }
        } else {
            CellPos {
                col: range.start.column.0 as u16,
                row: start_row as u16,
            }
        };
        let end = if end_row >= rows {
            CellPos {
                col: self.cols.saturating_sub(1),
                row: (rows - 1) as u16,
            }
        } else {
            CellPos {
                col: range.end.column.0 as u16,
                row: end_row as u16,
            }
        };
        Some((start, end))
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
        history: bundle.history.clone(),
        cols: bundle.cols,
        rows: bundle.rows,
        visible: bundle.visible_escaped.clone(),
        saved_primary: bundle.alternate_escaped.clone(),
        cursor_x: bundle.cursor_x,
        cursor_y: bundle.cursor_y,
        alternate_on: bundle.alternate_on,
        mouse_on: bundle.mouse_on,
        mouse_sgr: bundle.mouse_sgr,
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
            history: Vec::new(),
            cols: 10,
            rows: 2,
            // What the user is looking at: the agent TUI.
            visible: b"CLAUDE".to_vec(),
            // What tmux saved when that TUI took the alternate screen.
            saved_primary: Some(b"shell".to_vec()),
            cursor_x: 6,
            cursor_y: 0,
            alternate_on: true,
            mouse_on: false,
            mouse_sgr: false,
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
            history: Vec::new(),
            cols: 10,
            rows: 2,
            visible: b"CLAUDE".to_vec(),
            saved_primary: Some(b"shell".to_vec()),
            cursor_x: 6,
            cursor_y: 0,
            alternate_on: true,
            mouse_on: false,
            mouse_sgr: false,
        });

        rt.feed(b"\x1b[?1049l");
        assert_eq!(
            rt.snapshot().row_texts()[0],
            "shell",
            "the saved primary must be underneath, so the TUI's own exit works"
        );
    }

    #[test]
    fn hydration_preserves_scrollback_history() {
        let mut rt = PaneRuntime::new(8, 2);
        rt.feed(b"one\r\ntwo\r\nthree\r\nfour");
        // The resize path rehydrates from a screen-only capture; the two
        // rows already in history must come along.
        rt.hydrate(&HydrationSnapshot {
            history: Vec::new(),
            cols: 10,
            rows: 3,
            visible: b"prompt".to_vec(),
            saved_primary: None,
            cursor_x: 6,
            cursor_y: 0,
            alternate_on: false,
            mouse_on: false,
            mouse_sgr: false,
        });

        assert!(rt.at_tail(), "a rehydrated pane lands at the live tail");
        assert_eq!(rt.snapshot().row_texts()[0], "prompt");
        rt.scroll(-2);
        assert_eq!(
            rt.row_text(0).trim_end(),
            "one",
            "scrollback must survive rehydration, or the wheel goes dead after a resize"
        );
    }

    #[test]
    fn rehydrating_twice_keeps_history_exact() {
        let snapshot = HydrationSnapshot {
            history: Vec::new(),
            cols: 10,
            rows: 3,
            visible: b"prompt".to_vec(),
            saved_primary: None,
            cursor_x: 6,
            cursor_y: 0,
            alternate_on: false,
            mouse_on: false,
            mouse_sgr: false,
        };
        let mut rt = PaneRuntime::new(8, 2);
        rt.feed(b"one\r\ntwo\r\nthree\r\nfour");
        rt.hydrate(&snapshot);
        rt.hydrate(&snapshot);

        // Scroll past the end of history: the view clamps at the oldest
        // line. Nothing duplicated, no blank filler rows.
        rt.scroll(-1000);
        assert_eq!(rt.snapshot().row_texts(), vec!["one", "two", "prompt"]);
    }

    #[test]
    fn hydration_discards_stale_terminal_state() {
        let mut rt = PaneRuntime::new(10, 2);
        rt.feed(b"stale");
        rt.hydrate(&HydrationSnapshot {
            history: Vec::new(),
            cols: 10,
            rows: 2,
            visible: b"new".to_vec(),
            saved_primary: None,
            cursor_x: 3,
            cursor_y: 0,
            alternate_on: false,
            mouse_on: false,
            mouse_sgr: false,
        });

        assert_eq!(rt.snapshot().row_texts()[0], "new");
        assert!(!rt.term.mode().contains(TermMode::ALT_SCREEN));
    }

    #[test]
    fn hydrating_a_snapshot_with_mouse_flags_on_restores_sgr_wheel_wanting() {
        let mut rt = PaneRuntime::new(10, 2);
        rt.hydrate(&HydrationSnapshot {
            history: Vec::new(),
            cols: 10,
            rows: 2,
            visible: b"CLAUDE".to_vec(),
            saved_primary: None,
            cursor_x: 0,
            cursor_y: 0,
            alternate_on: true,
            mouse_on: true,
            mouse_sgr: true,
        });

        assert!(
            rt.wants_sgr_mouse_wheel(),
            "a rebuilt runtime must not lose the mouse-reporting mode a live pane already had"
        );
    }

    #[test]
    fn hydrating_a_snapshot_with_mouse_flags_off_leaves_sgr_wheel_unwanted() {
        let mut rt = PaneRuntime::new(10, 2);
        rt.hydrate(&HydrationSnapshot {
            history: Vec::new(),
            cols: 10,
            rows: 2,
            visible: b"CLAUDE".to_vec(),
            saved_primary: None,
            cursor_x: 0,
            cursor_y: 0,
            alternate_on: true,
            mouse_on: false,
            mouse_sgr: false,
        });

        assert!(
            !rt.wants_sgr_mouse_wheel(),
            "a pane that never asked for mouse reporting must not gain it from hydration"
        );
    }

    #[test]
    fn fresh_runtime_wants_neither_sgr_wheel_nor_alt_screen() {
        let rt = PaneRuntime::new(10, 2);
        assert!(!rt.alt_screen());
        assert!(!rt.wants_sgr_mouse_wheel());
    }

    /// A viewport left in history returns to the tail on demand, once; at
    /// the tail there is nothing to move and the call says so.
    #[test]
    fn a_scrolled_back_viewport_returns_to_the_tail_once() {
        let mut rt = PaneRuntime::new(20, 4);
        for i in 0..40 {
            rt.feed(format!("line{i}\r\n").as_bytes());
        }
        rt.scroll(-6);
        assert!(!rt.at_tail(), "the wheel moved into history");
        assert!(rt.scroll_to_tail(), "the viewport moved back");
        assert!(rt.at_tail());
        assert!(!rt.scroll_to_tail(), "already at the tail: nothing moved");
    }

    #[test]
    fn alt_screen_with_sgr_mouse_reporting_wants_sgr_wheel() {
        let mut rt = PaneRuntime::new(10, 2);
        rt.feed(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h");
        assert!(rt.alt_screen());
        assert!(rt.wants_sgr_mouse_wheel());
    }

    #[test]
    fn alt_screen_alone_does_not_want_sgr_wheel() {
        let mut rt = PaneRuntime::new(10, 2);
        rt.feed(b"\x1b[?1049h");
        assert!(rt.alt_screen());
        assert!(!rt.wants_sgr_mouse_wheel());
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
