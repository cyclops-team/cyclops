//! Parse tmux `window_layout` strings into a split tree.
//!
//! Layout leaves carry the pane's numeric id (the `N` of `%N`) and its
//! cell-exact geometry inside the window. The workspace renders panes at
//! those exact coordinates — tmux's own one-cell gaps between panes are
//! where dividers are drawn — so the grid a pane runtime holds maps 1:1
//! onto screen cells.

use ratatui::layout::Rect;
use thiserror::Error;

use crate::model::PaneSlot;

/// Split direction in the tmux layout tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDir {
    /// Panes side by side (`{` `}` in tmux).
    Horizontal,
    /// Panes stacked (`[` `]` in tmux).
    Vertical,
}

/// Parsed tmux layout tree. Leaf numbers are pane ids (`%N` without the
/// sigil); every node carries its cell rectangle in window coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutNode {
    Leaf {
        pane_num: usize,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    },
    Split {
        dir: SplitDir,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        children: Vec<LayoutNode>,
    },
}

impl LayoutNode {
    /// Node extent in window coordinates.
    pub fn rect(&self) -> Rect {
        match self {
            LayoutNode::Leaf {
                x,
                y,
                width,
                height,
                ..
            }
            | LayoutNode::Split {
                x,
                y,
                width,
                height,
                ..
            } => Rect::new(*x, *y, *width, *height),
        }
    }
}

/// Resolved layout with tmux pane ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedLayout {
    Leaf {
        pane_id: String,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    },
    Split {
        dir: SplitDir,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        children: Vec<ResolvedLayout>,
    },
}

impl ResolvedLayout {
    /// Node extent in window coordinates.
    pub fn rect(&self) -> Rect {
        match self {
            ResolvedLayout::Leaf {
                x,
                y,
                width,
                height,
                ..
            }
            | ResolvedLayout::Split {
                x,
                y,
                width,
                height,
                ..
            } => Rect::new(*x, *y, *width, *height),
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum LayoutError {
    #[error("empty layout string")]
    Empty,
    #[error("layout parse error at {pos}: {detail}")]
    Parse { pos: usize, detail: String },
}

/// Parse a tmux `#{window_layout}` string.
pub fn parse_layout(s: &str) -> Result<LayoutNode, LayoutError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(LayoutError::Empty);
    }
    let body = s
        .split_once(',')
        .map(|(_, rest)| rest)
        .ok_or_else(|| LayoutError::Parse {
            pos: 0,
            detail: "missing checksum".into(),
        })?;
    let (node, consumed) = parse_node(body, 0)?;
    if consumed != body.len() {
        return Err(LayoutError::Parse {
            pos: consumed,
            detail: "trailing data".into(),
        });
    }
    Ok(node)
}

/// Turn leaf pane numbers into `%N` ids. `known` guards against a layout
/// naming a pane the window listing did not: MEASURED, leaf numbers are the
/// pane's `%N` id, not `#{pane_index}`. Pass an empty slice to skip the
/// guard (control-mode notifications carry the layout but no pane list).
pub fn resolve_layout(node: &LayoutNode, known: &[String]) -> Option<ResolvedLayout> {
    match node {
        LayoutNode::Leaf {
            pane_num,
            x,
            y,
            width,
            height,
        } => {
            let pane_id = format!("%{pane_num}");
            if !known.is_empty() && !known.iter().any(|k| k == &pane_id) {
                return None;
            }
            Some(ResolvedLayout::Leaf {
                pane_id,
                x: *x,
                y: *y,
                width: *width,
                height: *height,
            })
        }
        LayoutNode::Split {
            dir,
            x,
            y,
            width,
            height,
            children,
        } => {
            let mut resolved = Vec::with_capacity(children.len());
            for child in children {
                resolved.push(resolve_layout(child, known)?);
            }
            Some(ResolvedLayout::Split {
                dir: *dir,
                x: *x,
                y: *y,
                width: *width,
                height: *height,
                children: resolved,
            })
        }
    }
}

/// Collect pane ids in layout order.
pub fn pane_ids_in_layout(node: &ResolvedLayout) -> Vec<String> {
    pane_dims_in_layout(node)
        .into_iter()
        .map(|(id, _, _)| id)
        .collect()
}

/// Collect `(pane_id, cols, rows)` for every leaf in layout order.
pub fn pane_dims_in_layout(node: &ResolvedLayout) -> Vec<(String, u16, u16)> {
    let mut out = Vec::new();
    collect_pane_dims(node, &mut out);
    out
}

fn collect_pane_dims(node: &ResolvedLayout, out: &mut Vec<(String, u16, u16)>) {
    match node {
        ResolvedLayout::Leaf {
            pane_id,
            width,
            height,
            ..
        } => out.push((pane_id.clone(), *width, *height)),
        ResolvedLayout::Split { children, .. } => {
            for child in children {
                collect_pane_dims(child, out);
            }
        }
    }
}

fn parse_node(s: &str, base: usize) -> Result<(LayoutNode, usize), LayoutError> {
    let mut pos = 0;
    let (width, height) = parse_dims(s, &mut pos, base)?;
    let x = parse_u16(s, &mut pos, base)?;
    let y = parse_u16(s, &mut pos, base)?;
    if pos < s.len() && s.as_bytes()[pos] == b',' {
        expect_char(s, &mut pos, ',', base)?;
    }
    if pos >= s.len() {
        return Err(LayoutError::Parse {
            pos: base + pos,
            detail: "missing pane body".into(),
        });
    }
    match s.as_bytes()[pos] {
        b'[' => {
            let (inner, end) = extract_group(s, pos, b'[', b']', base)?;
            let children = parse_children(inner, base + pos + 1)?;
            Ok((
                LayoutNode::Split {
                    dir: SplitDir::Vertical,
                    x,
                    y,
                    width,
                    height,
                    children,
                },
                end,
            ))
        }
        b'{' => {
            let (inner, end) = extract_group(s, pos, b'{', b'}', base)?;
            let children = parse_children(inner, base + pos + 1)?;
            Ok((
                LayoutNode::Split {
                    dir: SplitDir::Horizontal,
                    x,
                    y,
                    width,
                    height,
                    children,
                },
                end,
            ))
        }
        _ => {
            let pane_num = parse_usize(s, &mut pos, base)?;
            Ok((
                LayoutNode::Leaf {
                    pane_num,
                    x,
                    y,
                    width,
                    height,
                },
                pos,
            ))
        }
    }
}

fn parse_children(inner: &str, base: usize) -> Result<Vec<LayoutNode>, LayoutError> {
    let mut children = Vec::new();
    let mut pos = 0;
    while pos < inner.len() {
        if inner.as_bytes()[pos] == b',' {
            pos += 1;
            continue;
        }
        let (child, consumed) = parse_node(&inner[pos..], base + pos)?;
        children.push(child);
        pos += consumed;
    }
    Ok(children)
}

fn parse_dims(s: &str, pos: &mut usize, base: usize) -> Result<(u16, u16), LayoutError> {
    let start = *pos;
    let x_pos = s[*pos..].find('x').ok_or_else(|| LayoutError::Parse {
        pos: base + *pos,
        detail: "missing x in dimensions".into(),
    })?;
    let width: u16 = s[*pos..*pos + x_pos]
        .parse()
        .map_err(|e| LayoutError::Parse {
            pos: base + *pos,
            detail: format!("width: {e}"),
        })?;
    *pos += x_pos + 1;
    let end = s[*pos..].find(',').ok_or_else(|| LayoutError::Parse {
        pos: base + *pos,
        detail: "missing comma after height".into(),
    })?;
    let height: u16 = s[*pos..*pos + end]
        .parse()
        .map_err(|e| LayoutError::Parse {
            pos: base + *pos,
            detail: format!("height: {e}"),
        })?;
    *pos = start + x_pos + 1 + end;
    Ok((width, height))
}

fn parse_u16(s: &str, pos: &mut usize, base: usize) -> Result<u16, LayoutError> {
    if *pos < s.len() && s.as_bytes()[*pos] == b',' {
        *pos += 1;
    }
    let start = *pos;
    while *pos < s.len() && s.as_bytes()[*pos].is_ascii_digit() {
        *pos += 1;
    }
    if start == *pos {
        return Err(LayoutError::Parse {
            pos: base + *pos,
            detail: "expected integer".into(),
        });
    }
    s[start..*pos].parse().map_err(|e| LayoutError::Parse {
        pos: base + start,
        detail: format!("integer: {e}"),
    })
}

fn parse_usize(s: &str, pos: &mut usize, base: usize) -> Result<usize, LayoutError> {
    let start = *pos;
    while *pos < s.len() && s.as_bytes()[*pos].is_ascii_digit() {
        *pos += 1;
    }
    if start == *pos {
        return Err(LayoutError::Parse {
            pos: base + *pos,
            detail: "expected pane id".into(),
        });
    }
    s[start..*pos].parse().map_err(|e| LayoutError::Parse {
        pos: base + start,
        detail: format!("pane id: {e}"),
    })
}

fn expect_char(s: &str, pos: &mut usize, ch: char, base: usize) -> Result<(), LayoutError> {
    if *pos >= s.len() || s.as_bytes()[*pos] != ch as u8 {
        return Err(LayoutError::Parse {
            pos: base + *pos,
            detail: format!("expected {ch}"),
        });
    }
    *pos += 1;
    Ok(())
}

fn extract_group(
    s: &str,
    start: usize,
    open: u8,
    close: u8,
    base: usize,
) -> Result<(&str, usize), LayoutError> {
    if s.as_bytes().get(start) != Some(&open) {
        return Err(LayoutError::Parse {
            pos: base + start,
            detail: format!("expected {}", open as char),
        });
    }
    let mut depth = 0;
    for (i, &b) in s.as_bytes()[start..].iter().enumerate() {
        if b == open {
            depth += 1;
        } else if b == close {
            depth -= 1;
            if depth == 0 {
                let inner_start = start + 1;
                let inner_end = start + i;
                return Ok((&s[inner_start..inner_end], start + i + 1));
            }
        }
    }
    Err(LayoutError::Parse {
        pos: base + start,
        detail: format!("unclosed {} group", open as char),
    })
}

/// Pane render rectangles: window coordinates offset into `canvas` and
/// clipped to it. Cells map 1:1 — no scaling, so a runtime grid lands on
/// exactly the cells tmux gave the pane.
pub fn layout_pane_slots(node: &ResolvedLayout, canvas: Rect, focused_pane: &str) -> Vec<PaneSlot> {
    let mut out = Vec::new();
    collect_slots(node, canvas, focused_pane, &mut out);
    out
}

fn collect_slots(node: &ResolvedLayout, canvas: Rect, focused: &str, out: &mut Vec<PaneSlot>) {
    match node {
        ResolvedLayout::Leaf {
            pane_id,
            x,
            y,
            width,
            height,
        } => {
            if let Some(rect) = offset_clip(*x, *y, *width, *height, canvas) {
                out.push(PaneSlot {
                    pane_id: pane_id.clone(),
                    rect,
                    focused: pane_id == focused,
                });
            }
        }
        ResolvedLayout::Split { children, .. } => {
            for child in children {
                collect_slots(child, canvas, focused, out);
            }
        }
    }
}

/// One divider band between adjacent split children, in window coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DividerSeg {
    /// The gap cells tmux leaves between the two children.
    pub rect: Rect,
    /// Direction of the split that owns the gap: `Horizontal` gaps are
    /// vertical lines, `Vertical` gaps horizontal lines.
    pub dir: SplitDir,
    /// The pane whose trailing edge the divider is — the `resize-pane`
    /// target for drags.
    pub pane_id: String,
}

/// Dividers for every split in the tree, window coordinates.
pub fn layout_dividers(node: &ResolvedLayout) -> Vec<DividerSeg> {
    let mut out = Vec::new();
    collect_dividers(node, &mut out);
    out
}

fn collect_dividers(node: &ResolvedLayout, out: &mut Vec<DividerSeg>) {
    if let ResolvedLayout::Split { dir, children, .. } = node {
        for pair in children.windows(2) {
            let a = pair[0].rect();
            let b = pair[1].rect();
            let rect = match dir {
                SplitDir::Horizontal => {
                    let gap_x = a.x + a.width;
                    Rect::new(gap_x, a.y, b.x.saturating_sub(gap_x), a.height)
                }
                SplitDir::Vertical => {
                    let gap_y = a.y + a.height;
                    Rect::new(a.x, gap_y, a.width, b.y.saturating_sub(gap_y))
                }
            };
            if rect.width > 0 && rect.height > 0 {
                if let Some(pane_id) = trailing_leaf(&pair[0], *dir) {
                    out.push(DividerSeg {
                        rect,
                        dir: *dir,
                        pane_id,
                    });
                }
            }
        }
        for child in children {
            collect_dividers(child, out);
        }
    }
}

/// Leaf whose trailing edge (right for horizontal, bottom for vertical)
/// touches the subtree's own trailing edge.
fn trailing_leaf(node: &ResolvedLayout, dir: SplitDir) -> Option<String> {
    match node {
        ResolvedLayout::Leaf { pane_id, .. } => Some(pane_id.clone()),
        ResolvedLayout::Split { children, .. } => {
            let extent = node.rect();
            children
                .iter()
                .find(|c| {
                    let r = c.rect();
                    match dir {
                        SplitDir::Horizontal => r.x + r.width == extent.x + extent.width,
                        SplitDir::Vertical => r.y + r.height == extent.y + extent.height,
                    }
                })
                .and_then(|c| trailing_leaf(c, dir))
        }
    }
}

/// Offset a window-coordinate rectangle into `canvas`, clipping to it.
pub fn offset_clip(x: u16, y: u16, width: u16, height: u16, canvas: Rect) -> Option<Rect> {
    let ax = canvas.x.saturating_add(x);
    let ay = canvas.y.saturating_add(y);
    if ax >= canvas.x + canvas.width || ay >= canvas.y + canvas.height {
        return None;
    }
    let w = width.min(canvas.x + canvas.width - ax);
    let h = height.min(canvas.y + canvas.height - ay);
    if w == 0 || h == 0 {
        return None;
    }
    Some(Rect::new(ax, ay, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn single_pane_leaf() {
        let node = parse_layout("b25e,80x24,0,0,0").expect("parse");
        assert_eq!(
            node,
            LayoutNode::Leaf {
                pane_num: 0,
                x: 0,
                y: 0,
                width: 80,
                height: 24,
            }
        );
    }

    #[test]
    fn vertical_split_keeps_offsets() {
        let node = parse_layout("4c3e,319x89,0,0[319x44,0,0,0,319x44,0,45,1]").expect("parse");
        assert_eq!(
            node,
            LayoutNode::Split {
                dir: SplitDir::Vertical,
                x: 0,
                y: 0,
                width: 319,
                height: 89,
                children: vec![
                    LayoutNode::Leaf {
                        pane_num: 0,
                        x: 0,
                        y: 0,
                        width: 319,
                        height: 44,
                    },
                    LayoutNode::Leaf {
                        pane_num: 1,
                        x: 0,
                        y: 45,
                        width: 319,
                        height: 44,
                    },
                ],
            }
        );
    }

    #[test]
    fn horizontal_split_two_panes() {
        let node = parse_layout("da0d,200x50,0,0{100x50,0,0,0,99x50,101,0,1}").expect("parse");
        let LayoutNode::Split { dir, children, .. } = node else {
            panic!("expected split");
        };
        assert_eq!(dir, SplitDir::Horizontal);
        assert_eq!(children[1].rect(), Rect::new(101, 0, 99, 50));
    }

    #[test]
    fn nested_split() {
        let layout = "abcd,200x50,0,0[200x25,0,0{100x25,0,0,0,99x25,101,0,3},200x24,0,26,2]";
        let node = parse_layout(layout).expect("parse");
        let LayoutNode::Split { dir, children, .. } = &node else {
            panic!("expected split");
        };
        assert_eq!(*dir, SplitDir::Vertical);
        assert_eq!(children.len(), 2);
        assert!(matches!(
            children[0],
            LayoutNode::Split {
                dir: SplitDir::Horizontal,
                ..
            }
        ));
        assert!(matches!(children[1], LayoutNode::Leaf { pane_num: 2, .. }));
    }

    #[test]
    fn leaf_numbers_resolve_as_pane_ids() {
        // Leaf numbers are pane ids, not pane indexes: this window's panes
        // are %3 and %7 at indexes 0 and 1.
        let node = parse_layout("4c3e,319x89,0,0[319x44,0,0,3,319x44,0,45,7]").unwrap();
        let resolved = resolve_layout(&node, &ids(&["%3", "%7"])).expect("resolve");
        assert_eq!(pane_ids_in_layout(&resolved), ids(&["%3", "%7"]));
    }

    #[test]
    fn unknown_leaf_id_fails_resolution() {
        let node = parse_layout("4c3e,319x89,0,0[319x44,0,0,3,319x44,0,45,7]").unwrap();
        assert_eq!(resolve_layout(&node, &ids(&["%0", "%1"])), None);
    }

    #[test]
    fn slots_map_cells_one_to_one() {
        let node = parse_layout("dd63,180x45,0,0{90x45,0,0,0,89x45,91,0,1}").unwrap();
        let resolved = resolve_layout(&node, &[]).unwrap();
        let canvas = Rect::new(20, 1, 180, 45);
        let slots = layout_pane_slots(&resolved, canvas, "%1");
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].rect, Rect::new(20, 1, 90, 45));
        assert_eq!(slots[1].rect, Rect::new(111, 1, 89, 45));
        assert!(!slots[0].focused);
        assert!(slots[1].focused);
    }

    #[test]
    fn slots_clip_to_canvas() {
        let node = parse_layout("dd63,180x45,0,0{90x45,0,0,0,89x45,91,0,1}").unwrap();
        let resolved = resolve_layout(&node, &[]).unwrap();
        // Canvas narrower than the window: the right pane is clipped.
        let canvas = Rect::new(0, 0, 150, 45);
        let slots = layout_pane_slots(&resolved, canvas, "%0");
        assert_eq!(slots[1].rect, Rect::new(91, 0, 59, 45));
    }

    #[test]
    fn divider_fills_the_gap_between_panes() {
        let node = parse_layout("dd63,180x45,0,0{90x45,0,0,0,89x45,91,0,1}").unwrap();
        let resolved = resolve_layout(&node, &[]).unwrap();
        let dividers = layout_dividers(&resolved);
        assert_eq!(
            dividers,
            vec![DividerSeg {
                rect: Rect::new(90, 0, 1, 45),
                dir: SplitDir::Horizontal,
                pane_id: "%0".into(),
            }]
        );
    }

    #[test]
    fn nested_divider_targets_trailing_leaf() {
        // Top row split into %0 | %3, bottom row %2. The horizontal gap
        // between the rows belongs to the top subtree; its bottom edge is
        // shared by both leaves, so either is a valid resize target — the
        // trailing rule picks the child touching the subtree's bottom.
        let layout = "abcd,200x50,0,0[200x25,0,0{100x25,0,0,0,99x25,101,0,3},200x24,0,26,2]";
        let resolved = resolve_layout(&parse_layout(layout).unwrap(), &[]).unwrap();
        let dividers = layout_dividers(&resolved);
        assert_eq!(dividers.len(), 2);
        let row_gap = dividers
            .iter()
            .find(|d| d.dir == SplitDir::Vertical)
            .expect("row divider");
        assert_eq!(row_gap.rect, Rect::new(0, 25, 200, 1));
        let col_gap = dividers
            .iter()
            .find(|d| d.dir == SplitDir::Horizontal)
            .expect("column divider");
        assert_eq!(col_gap.rect, Rect::new(100, 0, 1, 25));
        assert_eq!(col_gap.pane_id, "%0");
    }

    #[test]
    fn real_tmux_layout_strings_parse() {
        use cyclops_testrig::{tmux_available, TmuxServer};

        if !tmux_available() {
            eprintln!("skipping: no tmux binary on PATH");
            return;
        }
        let server = TmuxServer::new("layout-parse");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "lay",
            "-x",
            "120",
            "-y",
            "30",
            "/bin/sh",
        ]);
        let out = server.run(&["list-windows", "-t", "lay", "-F", "#{window_layout}"]);
        let single = String::from_utf8_lossy(&out.stdout);
        parse_layout(single.trim()).expect("single pane layout should parse");

        server.run_ok(&["split-window", "-h", "-t", "lay"]);
        let out = server.run(&["list-windows", "-t", "lay", "-F", "#{window_layout}"]);
        let hsplit = String::from_utf8_lossy(&out.stdout);
        let node = parse_layout(hsplit.trim()).expect("horizontal split layout should parse");
        assert!(matches!(node, LayoutNode::Split { .. }));

        // Leaf numbers must match the server's pane ids exactly.
        let out = server.run(&["list-panes", "-t", "lay", "-F", "#{pane_id}"]);
        let pane_ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        let resolved = resolve_layout(&node, &pane_ids).expect("layout leaves are pane ids");
        assert_eq!(pane_ids_in_layout(&resolved), pane_ids);

        server.run_ok(&["select-pane", "-t", "lay:0.1"]);
        server.run_ok(&["split-window", "-v", "-t", "lay:0.1"]);
        let out = server.run(&["list-windows", "-t", "lay", "-F", "#{window_layout}"]);
        let nested = String::from_utf8_lossy(&out.stdout);
        parse_layout(nested.trim()).expect("nested split layout should parse");
    }
}
