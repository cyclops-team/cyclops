//! Text selection lifecycle and clipboard export (memory-only; never
//! logged). The selection's geometry does not live here: the pane's VT
//! engine owns it ([`PaneRuntime::begin_selection`] and friends), anchored
//! to grid content so it stays on its text through scrolling and new
//! output. What this module tracks is whose selection exists and what the
//! mouse is in the middle of doing about it.

use std::time::{Duration, Instant};

use crate::input::mouse::HitTarget;
use crate::runtime::{CellPos, PaneRuntime};

/// One step of a drag, for the caller to apply to the pane's runtime.
/// Returned rather than applied here because the runtime lives in the
/// registry, keyed by pane, and this state machine deliberately holds no
/// reference into it.
#[derive(Debug, PartialEq, Eq)]
pub enum DragStep {
    /// Not a selection drag (no press, or a different pane).
    None,
    /// The drag left its press cell: begin at the press cell, extend to
    /// the current one.
    Begin { start: CellPos, now: CellPos },
    /// An in-flight drag moved.
    Extend { now: CellPos },
}

/// Tracks click-drag selection and double/triple-click word/line picks.
#[derive(Default)]
pub struct SelectionState {
    /// The pane holding a live selection, while one exists. The geometry
    /// is in that pane's runtime; this is who to ask and who to clear.
    active: Option<String>,
    dragging: Option<(String, CellPos)>,
    /// Button-down cell waiting for movement. A press alone never selects;
    /// the selection starts on the first drag into a different cell.
    pending: Option<(String, CellPos)>,
    click: ClickTracker,
}

#[derive(Default)]
struct ClickTracker {
    last: Option<(HitTarget, u16, u16)>,
    last_at: Option<Instant>,
    count: u8,
}

const CLICK_WINDOW: Duration = Duration::from_millis(400);

impl SelectionState {
    /// Forget the selection here. The caller clears the owning runtime's
    /// geometry with what [`Self::take_active`] returns; state alone
    /// cannot, and a highlight with no owner would never unpaint.
    pub fn clear(&mut self) {
        self.active = None;
        self.dragging = None;
        self.pending = None;
    }

    /// The pane whose selection exists, surrendered for clearing.
    pub fn take_active(&mut self) -> Option<String> {
        self.active.take()
    }

    /// The pane whose selection exists (paint and copy read this).
    pub fn active_pane(&self) -> Option<&str> {
        self.active.as_deref()
    }

    /// A selection now exists in this pane (word/line picks and begun
    /// drags both land here).
    pub fn set_active(&mut self, pane_id: String) {
        self.active = Some(pane_id);
        self.dragging = None;
    }

    /// Button down inside a pane body: remember the cell, clear any old
    /// highlight, select nothing yet.
    pub fn press(&mut self, pane_id: String, pos: CellPos) {
        self.active = None;
        self.dragging = None;
        self.pending = Some((pane_id, pos));
    }

    /// Drag motion inside a pane body. Reports the step for the caller to
    /// apply to the pane's runtime: the selection starts once the drag
    /// leaves the press cell and extends afterwards.
    pub fn drag_to(&mut self, pane_id: &str, pos: CellPos) -> DragStep {
        if let Some((dragging_pane, _)) = &self.dragging {
            if dragging_pane == pane_id {
                return DragStep::Extend { now: pos };
            }
            return DragStep::None;
        }
        if let Some((pending_pane, start)) = self.pending.clone() {
            if pending_pane == pane_id && start != pos {
                self.pending = None;
                self.dragging = Some((pending_pane.clone(), start));
                self.active = Some(pending_pane);
                return DragStep::Begin { start, now: pos };
            }
        }
        DragStep::None
    }

    /// Pane the press or drag is anchored in, if any.
    pub fn anchor_pane(&self) -> Option<&str> {
        self.dragging
            .as_ref()
            .map(|(p, _)| p.as_str())
            .or(self.pending.as_ref().map(|(p, _)| p.as_str()))
    }

    /// Pane with a drag in flight, if any. The wheel reads this: local
    /// scrolling is allowed to continue mid-drag so a selection can grow
    /// past one screen, and the caller re-extends to the pointer after
    /// each scroll.
    pub fn dragging_pane(&self) -> Option<&str> {
        self.dragging.as_ref().map(|(p, _)| p.as_str())
    }

    /// Record a click for double/triple detection. Returns 1, 2, or 3.
    pub fn register_click(&mut self, target: &HitTarget, col: u16, row: u16) -> u8 {
        let now = Instant::now();
        let same = self
            .click
            .last
            .as_ref()
            .is_some_and(|(t, c, r)| t == target && *c == col && *r == row);
        let recent = self
            .click
            .last_at
            .is_some_and(|t| now.duration_since(t) <= CLICK_WINDOW);
        self.click.count = if same && recent {
            self.click.count.saturating_add(1).min(3)
        } else {
            1
        };
        self.click.last = Some((target.clone(), col, row));
        self.click.last_at = Some(now);
        self.click.count
    }

    pub fn is_dragging(&self) -> bool {
        self.dragging.is_some()
    }

    /// End a drag and return the pane whose selection finished. A press
    /// that never moved returns None. The selection stays active (the
    /// highlight outlives the button) until something clears it.
    pub fn finish_drag(&mut self) -> Option<String> {
        self.pending = None;
        let (pane, _) = self.dragging.take()?;
        Some(pane)
    }

    /// Extract a pane's selected text from its runtime. Never logged.
    pub fn extract(runtime: &PaneRuntime) -> Option<String> {
        runtime.selection_text()
    }
}

/// Word bounds around a click, over a row given as one `char` per column
/// (`PaneRuntime::row_text`). Indexing chars, not bytes, keeps the columns
/// honest on rows holding wide or multi-byte characters — the old
/// grid-view version indexed bytes and drifted right of every CJK glyph.
pub fn word_range(row_text: &str, pos: CellPos) -> (CellPos, CellPos) {
    let row = pos.row;
    let chars: Vec<char> = row_text.chars().collect();
    if chars.is_empty() {
        return (pos, pos);
    }
    let col = (pos.col as usize).min(chars.len() - 1);
    let mut start = col;
    let mut end = col;
    while start > 0 && is_word_char(chars[start - 1]) {
        start -= 1;
    }
    while end + 1 < chars.len() && is_word_char(chars[end + 1]) {
        end += 1;
    }
    (
        CellPos {
            col: start as u16,
            row,
        },
        CellPos {
            col: end as u16,
            row,
        },
    )
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Native clipboard writers, first on PATH wins. wl-copy / xclip cover
/// Linux, pbcopy macOS; no extra dependency.
const NATIVE_TOOLS: &[(&str, &[&str])] = &[
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
    ("pbcopy", &[]),
];

/// Copy text to the system clipboard. Both paths can run: the native tool
/// writes the local clipboard, OSC 52 reaches the terminal on the near
/// side of SSH. An OSC 52 stdout write "succeeding" proves nothing, since
/// macOS Terminal.app ignores the sequence and the write still returns
/// Ok, so it must never be the reason the native path is skipped.
pub fn copy_to_clipboard(text: &str) {
    use std::io::IsTerminal;
    if text.is_empty() {
        return;
    }
    if let Some((bin, args)) = native_tool() {
        let _ = copy_native(bin, args, text);
    }
    if std::io::stdout().is_terminal() {
        copy_osc52(text);
    }
}

/// First native clipboard tool on PATH. A PATH lookup, not a spawn.
fn native_tool() -> Option<(&'static str, &'static [&'static str])> {
    NATIVE_TOOLS.iter().copied().find(|(bin, _)| on_path(bin))
}

fn on_path(bin: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(bin).is_file())
}

/// Emit the OSC 52 escape, fire and forget. No return value on purpose:
/// the write result says nothing about the terminal honoring it.
fn copy_osc52(text: &str) {
    use std::io::Write;
    let encoded = base64_encode(text.as_bytes());
    let seq = format!("\x1b]52;c;{encoded}\x07");
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(seq.as_bytes());
    let _ = stdout.flush();
}

/// Pipe text into one native tool. The caller already proved the binary
/// is on PATH.
fn copy_native(bin: &str, args: &[&str], text: &str) -> bool {
    if let Ok(mut child) = std::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .spawn()
    {
        use std::io::Write;
        if let Some(stdin) = child.stdin.as_mut() {
            return stdin.write_all(text.as_bytes()).is_ok() && child.wait().is_ok();
        }
    }
    false
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i] as u32;
        let b1 = bytes.get(i + 1).copied().unwrap_or(0) as u32;
        let b2 = bytes.get(i + 2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        if i + 1 < bytes.len() {
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if i + 2 < bytes.len() {
            out.push(TABLE[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
        i += 3;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_range_finds_token() {
        let (from, to) = word_range("hello world ", CellPos { col: 8, row: 0 });
        assert_eq!(from.col, 6);
        assert_eq!(to.col, 10);
    }

    #[test]
    fn word_range_stays_column_accurate_after_a_wide_character() {
        // Column-indexed row text: 你 at col 0, its spacer at col 1. A
        // byte-indexed range would land two cells right of the word.
        let (from, to) = word_range("你 abc   ", CellPos { col: 3, row: 0 });
        assert_eq!(from.col, 2);
        assert_eq!(to.col, 4);
    }

    #[test]
    fn selection_extracts_across_cells() {
        let mut rt = PaneRuntime::new(10, 2);
        rt.feed(b"abcdef\r\n");
        rt.anchor_selection(CellPos { col: 1, row: 0 }, CellPos { col: 4, row: 0 });
        let text = rt.selection_text().expect("text");
        assert_eq!(text.trim(), "bcde");
    }

    /// The v7 bug, both halves. A selection is anchored to its text, not
    /// to screen rows: scrolling the viewport after selecting must not
    /// slide the selection onto different rows, and the copy must return
    /// what was highlighted when the mouse picked it.
    #[test]
    fn a_selection_stays_on_its_text_while_the_viewport_scrolls() {
        let mut rt = PaneRuntime::new(8, 2);
        rt.feed(b"one\r\ntwo\r\nthree\r\nfour");
        // Scroll back so "one" is on screen and select it there.
        rt.scroll(-2);
        assert_eq!(rt.row_text(0).trim_end(), "one", "rig premise");
        rt.anchor_selection(CellPos { col: 0, row: 0 }, CellPos { col: 2, row: 0 });
        assert_eq!(rt.selection_text().expect("text").trim_end(), "one");

        // Scroll away: the highlight leaves the screen with its text
        // instead of restyling whatever landed under it...
        rt.scroll(2);
        assert_eq!(
            rt.selection_screen_range(),
            None,
            "the highlight must follow the text off screen"
        );
        // ...and the copy is still the text the user picked.
        assert_eq!(rt.selection_text().expect("text").trim_end(), "one");

        // Scrolling back re-projects the highlight where the text is.
        rt.scroll(-2);
        let (from, to) = rt.selection_screen_range().expect("visible again");
        assert_eq!((from.row, to.row), (0, 0));
        assert_eq!((from.col, to.col), (0, 2));
    }

    /// Drags run backwards as often as forwards, and the engine's side
    /// semantics trim endpoint cells on swapped ranges: with fixed sides a
    /// leftward drag lost the cells under both the press and the pointer,
    /// and a one-cell leftward drag selected nothing. Every direction must
    /// keep exactly the cells the operator touched.
    #[test]
    fn a_backwards_drag_keeps_the_cells_the_pointer_touched() {
        let mut rt = PaneRuntime::new(12, 3);
        rt.feed(b"abcdefghij\r\nklmnopqrst\r\nuvwxyz");

        // Rightward, the baseline.
        rt.begin_selection(CellPos { col: 2, row: 0 });
        rt.extend_selection(CellPos { col: 5, row: 0 });
        assert_eq!(rt.selection_text().expect("text").trim_end(), "cdef");

        // The same span dragged leftward selects the same text.
        rt.begin_selection(CellPos { col: 5, row: 0 });
        rt.extend_selection(CellPos { col: 2, row: 0 });
        assert_eq!(rt.selection_text().expect("text").trim_end(), "cdef");

        // One cell leftward: both cells, not none.
        rt.begin_selection(CellPos { col: 1, row: 1 });
        rt.extend_selection(CellPos { col: 0, row: 1 });
        assert_eq!(rt.selection_text().expect("text").trim_end(), "kl");

        // Upward across rows: the press row's cell survives.
        rt.begin_selection(CellPos { col: 0, row: 2 });
        rt.extend_selection(CellPos { col: 8, row: 1 });
        let text = rt.selection_text().expect("text");
        assert!(
            text.starts_with("st") && text.trim_end().ends_with('u'),
            "an upward drag keeps both endpoints: {text:?}"
        );
    }

    /// New output rotates the grid; the engine rotates the selection with
    /// it, so text arriving mid-selection cannot slide the highlight onto
    /// lines the user never touched.
    #[test]
    fn new_output_moves_the_selection_with_its_text() {
        let mut rt = PaneRuntime::new(8, 3);
        rt.feed(b"alpha\r\nbeta");
        // Select "alpha" on the top row, at the tail.
        rt.anchor_selection(CellPos { col: 0, row: 0 }, CellPos { col: 4, row: 0 });
        assert_eq!(rt.selection_text().expect("text").trim_end(), "alpha");

        // Two more lines push "alpha" up and into history.
        rt.feed(b"\r\ngamma\r\ndelta");
        assert_eq!(
            rt.selection_text().expect("text").trim_end(),
            "alpha",
            "the selection must ride the scroll, not stay at row 0"
        );
    }

    #[test]
    fn click_tracker_counts_triple() {
        let mut state = SelectionState::default();
        let target = HitTarget::PaneBody {
            pane_id: "%0".into(),
        };
        assert_eq!(state.register_click(&target, 1, 1), 1);
        assert_eq!(state.register_click(&target, 1, 1), 2);
        assert_eq!(state.register_click(&target, 1, 1), 3);
    }

    #[test]
    fn drag_lifecycle() {
        let mut state = SelectionState::default();
        let pos = CellPos { col: 0, row: 0 };
        state.press("%0".into(), pos);
        assert!(!state.is_dragging(), "a press alone never selects");
        // Motion within the press cell is not a drag yet.
        assert_eq!(state.drag_to("%0", pos), DragStep::None);
        assert_eq!(
            state.drag_to("%0", CellPos { col: 3, row: 0 }),
            DragStep::Begin {
                start: pos,
                now: CellPos { col: 3, row: 0 }
            }
        );
        assert!(state.is_dragging());
        assert_eq!(state.dragging_pane(), Some("%0"));
        assert_eq!(
            state.drag_to("%0", CellPos { col: 5, row: 1 }),
            DragStep::Extend {
                now: CellPos { col: 5, row: 1 }
            }
        );
        assert_eq!(state.finish_drag(), Some("%0".to_string()));
        // The selection outlives the button: still active until cleared.
        assert_eq!(state.active_pane(), Some("%0"));
        assert_eq!(state.take_active(), Some("%0".to_string()));
        assert_eq!(state.active_pane(), None);
    }

    #[test]
    fn on_path_probe_finds_real_binaries_only() {
        assert!(on_path("sh"));
        assert!(!on_path("cyclops-no-such-clipboard-tool"));
    }

    #[test]
    fn base64_roundtrip_shape() {
        let enc = base64_encode(b"hi");
        assert!(!enc.is_empty());
        assert!(enc
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='));
    }
}
