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

/// Visual attributes on one terminal cell.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CellAttrs {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
    pub dim: bool,
}

/// One cell in the visible grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridCell {
    /// Primary grapheme. Empty means a blank cell.
    pub ch: char,
    /// When `ch` is blank but the cell is wide, the spacer column holds
    /// the wide character's trailing half.
    pub wide_spacer: bool,
    pub attrs: CellAttrs,
}

impl Default for GridCell {
    fn default() -> Self {
        Self {
            ch: ' ',
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
    /// Escaped visible-screen bytes (`capture-pane -e`).
    pub visible: Vec<u8>,
    /// Escaped alternate-screen bytes (`capture-pane -e -a`), when present.
    pub alternate: Option<Vec<u8>>,
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub alternate_on: bool,
}

/// Borrowed view of a pane's visible cell grid.
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

    /// Row-major text, trailing spaces trimmed per row.
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
                    }
                }
            }
            out.push(line.trim_end().to_string());
        }
        out
    }
}

/// Borrowed grid view returned by [`super::vt::PaneVt::grid`].
pub struct CellGridView<'a> {
    pub grid: &'a CellGrid,
}

impl<'a> CellGridView<'a> {
    pub fn cell(&self, col: u16, row: u16) -> Option<&'a GridCell> {
        self.grid.cell(col, row)
    }

    pub fn row_texts(&self) -> Vec<String> {
        self.grid.row_texts()
    }
}
