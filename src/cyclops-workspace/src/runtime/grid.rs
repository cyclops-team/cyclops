//! Shared cell-grid types for pane VT engines and renderers.

/// A position in the visible grid (0-based column and row).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellPos {
    pub col: u16,
    pub row: u16,
}

/// Semantic color for a cell. Engines map their native palettes here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Which underline a cell carries, if any.
///
/// The engine distinguishes five underline styles. A Ratatui cell can only
/// say "underlined", so every non-`None` variant paints the same way today;
/// the distinction is preserved here so a future renderer can use it and so
/// the corpus can prove the bridge does not flatten it on the way in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Underline {
    #[default]
    None,
    Single,
    Double,
    Curl,
    Dotted,
    Dashed,
}

impl Underline {
    /// Whether the cell should paint an underline at all.
    pub fn is_underlined(self) -> bool {
        !matches!(self, Underline::None)
    }
}

/// Visual attributes on one terminal cell.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CellAttrs {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: Underline,
    pub reverse: bool,
    pub dim: bool,
    pub hidden: bool,
    pub strikeout: bool,
}

/// One cell in the visible grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridCell {
    /// Primary grapheme. Empty means a blank cell.
    pub ch: char,
    /// Combining marks and variation selectors (U+0301 COMBINING ACUTE
    /// ACCENT, U+FE0F VARIATION SELECTOR-16) the engine attaches to `ch`
    /// instead of giving them their own column. Almost every cell has none,
    /// and an empty `Vec` never allocates, so the common case pays only the
    /// 24 bytes of an empty `Vec` header; a renderer that paints `ch` alone
    /// drops these silently.
    pub zerowidth: Vec<char>,
    /// When `ch` is blank but the cell is wide, the spacer column holds
    /// the wide character's trailing half.
    pub wide_spacer: bool,
    pub attrs: CellAttrs,
}

impl Default for GridCell {
    fn default() -> Self {
        Self {
            ch: ' ',
            zerowidth: Vec::new(),
            wide_spacer: false,
            attrs: CellAttrs::default(),
        }
    }
}

/// Cursor shape and visibility in the visible grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorState {
    pub col: u16,
    pub row: u16,
    pub visible: bool,
    pub shape: CursorShape,
    /// Whether the pane asked for a blinking cursor (DECSCUSR's odd
    /// numbers). Forwarded with `shape` so the host terminal's real
    /// cursor — the only cursor the workspace shows — renders what the
    /// pane requested rather than whatever the previous pane left.
    pub blink: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Bar,
}

/// Snapshot used to hydrate a pane runtime from tmux captures.
///
/// A capture is a visual snapshot, not parser-exact state (see design).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HydrationSnapshot {
    pub cols: u16,
    pub rows: u16,
    /// Escaped bytes for the screen the user is currently looking at
    /// (`capture-pane -e`). When `alternate_on`, this is the alternate
    /// screen — the running TUI, not the shell behind it.
    pub visible: Vec<u8>,
    /// Escaped bytes of tmux's saved grid (`capture-pane -e -a`): the
    /// *primary* screen as it was when the TUI took over. It is what the
    /// user sees again after the TUI exits, never the TUI itself (F38).
    pub saved_primary: Option<Vec<u8>>,
    /// Plain scrollback above the visible screen, oldest first, joined
    /// with LF. Used only when the runtime has no local history of its
    /// own (`PaneRuntime::hydrate`): a runtime meeting its pane for the
    /// first time starts with tmux's transcript instead of a scrollback
    /// wall at the attach moment.
    pub history: Vec<u8>,
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub alternate_on: bool,
    /// Whether the pane had any mouse-tracking DECSET on at capture time
    /// (1000, 1002, or 1003) — tmux's `#{mouse_any_flag}`. A capture
    /// restores pixels, not modes; `PaneRuntime::hydrate` re-feeds this one
    /// so a hydrated pane keeps forwarding wheel motion instead of losing
    /// it the moment its runtime gets rebuilt from a snapshot.
    pub mouse_on: bool,
    /// Whether the pane had SGR mouse encoding (DECSET 1006) on at capture
    /// time — tmux's `#{mouse_sgr_flag}`.
    pub mouse_sgr: bool,
}

/// An owned copy of a pane's visible cell grid — the test-facing
/// representation. Production rendering visits engine cells directly
/// through `PaneRuntime::for_each_visible_cell` and never builds one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellGrid {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<GridCell>,
}

impl CellGrid {
    pub fn cell(&self, col: u16, row: u16) -> Option<&GridCell> {
        if col >= self.cols || row >= self.rows {
            return None;
        }
        let idx = row as usize * self.cols as usize + col as usize;
        self.cells.get(idx)
    }

    /// Row-major text, trailing spaces trimmed per row. Zero-width marks
    /// follow their base character, so golden assertions read the same
    /// grapheme a user sees on screen — unlike `PaneRuntime::row_text`,
    /// this is not one `char` per column and must not be used for column
    /// arithmetic.
    pub fn row_texts(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.rows as usize);
        for row in 0..self.rows {
            let mut line = String::new();
            for col in 0..self.cols {
                if let Some(cell) = self.cell(col, row) {
                    if !cell.wide_spacer {
                        line.push(if cell.ch == '\0' || cell.ch == ' ' {
                            ' '
                        } else {
                            cell.ch
                        });
                        line.extend(cell.zerowidth.iter());
                    }
                }
            }
            out.push(line.trim_end().to_string());
        }
        out
    }
}
