//! Text selection state and clipboard export (memory-only; never logged).

use std::time::{Duration, Instant};

use crate::input::mouse::HitTarget;
use crate::runtime::{CellPos, PaneRuntime};

/// Active selection in one pane body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub pane_id: String,
    pub from: CellPos,
    pub to: CellPos,
}

/// Tracks click-drag selection and double/triple-click word/line picks.
#[derive(Default)]
pub struct SelectionState {
    pub active: Option<Selection>,
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
    pub fn clear(&mut self) {
        self.active = None;
        self.dragging = None;
        self.pending = None;
    }

    pub fn cancel_drag(&mut self) {
        self.dragging = None;
        self.active = None;
        self.pending = None;
    }

    /// Button down inside a pane body: remember the cell, clear any old
    /// highlight, select nothing yet.
    pub fn press(&mut self, pane_id: String, pos: CellPos) {
        self.active = None;
        self.dragging = None;
        self.pending = Some((pane_id, pos));
    }

    /// Drag motion inside a pane body. Starts the selection once the drag
    /// leaves the press cell; extends it afterwards.
    pub fn drag_to(&mut self, pane_id: &str, pos: CellPos) {
        if self.dragging.is_some() {
            self.update_drag(pos);
            return;
        }
        if let Some((pending_pane, start)) = self.pending.clone() {
            if pending_pane == pane_id && start != pos {
                self.pending = None;
                self.start_drag(pending_pane, start);
                self.update_drag(pos);
            }
        }
    }

    /// Pane the press or drag is anchored in, if any.
    pub fn anchor_pane(&self) -> Option<&str> {
        self.dragging
            .as_ref()
            .map(|(p, _)| p.as_str())
            .or(self.pending.as_ref().map(|(p, _)| p.as_str()))
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

    pub fn start_drag(&mut self, pane_id: String, pos: CellPos) {
        self.dragging = Some((pane_id.clone(), pos));
        self.active = Some(Selection {
            pane_id,
            from: pos,
            to: pos,
        });
    }

    pub fn update_drag(&mut self, pos: CellPos) {
        if let Some(sel) = &mut self.active {
            sel.to = pos;
        }
    }

    pub fn is_dragging(&self) -> bool {
        self.dragging.is_some()
    }

    /// End a drag and return the finished selection. A press that never
    /// moved returns None.
    pub fn finish_drag(&mut self) -> Option<Selection> {
        self.pending = None;
        self.dragging.take()?;
        self.active.clone()
    }

    /// Select the word around a double-click. `row_text` is the clicked
    /// row with one char per column (`PaneRuntime::row_text`).
    pub fn set_word(&mut self, pane_id: String, pos: CellPos, row_text: &str) {
        let (from, to) = word_range(row_text, pos);
        self.dragging = None;
        self.active = Some(Selection { pane_id, from, to });
    }

    pub fn set_line(&mut self, pane_id: String, row: u16, cols: u16) {
        self.dragging = None;
        self.active = Some(Selection {
            pane_id,
            from: CellPos { col: 0, row },
            to: CellPos {
                col: cols.saturating_sub(1),
                row,
            },
        });
    }

    /// Extract selected text from a runtime. Selection is never logged.
    pub fn extract(runtime: &mut PaneRuntime, sel: &Selection) -> Option<String> {
        let (from, to) = normalize(sel.from, sel.to);
        runtime.select(from, to)
    }
}

fn normalize(from: CellPos, to: CellPos) -> (CellPos, CellPos) {
    if from.row < to.row || (from.row == to.row && from.col <= to.col) {
        (from, to)
    } else {
        (to, from)
    }
}

/// Word bounds around a click, over a row given as one `char` per column
/// (`PaneRuntime::row_text`). Indexing chars, not bytes, keeps the columns
/// honest on rows holding wide or multi-byte characters — the old
/// grid-view version indexed bytes and drifted right of every CJK glyph.
fn word_range(row_text: &str, pos: CellPos) -> (CellPos, CellPos) {
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
        let from = CellPos { col: 1, row: 0 };
        let to = CellPos { col: 4, row: 0 };
        let text = rt.select(from, to).expect("text");
        assert_eq!(text.trim(), "bcde");
    }

    #[test]
    fn extract_while_scrolled_copies_the_rows_on_screen() {
        let mut rt = PaneRuntime::new(8, 2);
        rt.feed(b"one\r\ntwo\r\nthree\r\nfour");
        // Two rows scrolled into history; the viewport now shows them.
        rt.scroll(-2);
        assert_eq!(
            rt.row_text(0).trim_end(),
            "one",
            "rig premise: the viewport must be showing history"
        );

        let sel = Selection {
            pane_id: "%0".into(),
            from: CellPos { col: 0, row: 0 },
            to: CellPos { col: 2, row: 0 },
        };
        let text = SelectionState::extract(&mut rt, &sel).expect("text");
        assert_eq!(
            text.trim_end(),
            "one",
            "the copied text must be the row the user sees highlighted"
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
        state.start_drag("%0".into(), pos);
        assert!(state.is_dragging());
        state.update_drag(CellPos { col: 3, row: 0 });
        let sel = state.finish_drag().expect("selection");
        assert_eq!(sel.from, pos);
        assert_eq!(sel.to.col, 3);
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
