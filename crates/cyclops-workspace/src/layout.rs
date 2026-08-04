//! Parse tmux `window_layout` strings into a split tree.

use std::collections::HashMap;

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

/// Parsed tmux layout tree. Leaves carry pane indices; map to `%n` ids separately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutNode {
    Leaf {
        pane_index: usize,
        width: u16,
        height: u16,
    },
    Split {
        dir: SplitDir,
        children: Vec<LayoutNode>,
    },
}

/// Resolved layout with tmux pane ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedLayout {
    Leaf {
        pane_id: String,
        width: u16,
        height: u16,
    },
    Split {
        dir: SplitDir,
        children: Vec<ResolvedLayout>,
    },
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

/// Map pane indices from the layout tree to tmux pane ids.
pub fn resolve_layout(
    node: &LayoutNode,
    index_to_id: &HashMap<usize, String>,
) -> Option<ResolvedLayout> {
    match node {
        LayoutNode::Leaf {
            pane_index,
            width,
            height,
        } => index_to_id
            .get(pane_index)
            .map(|pane_id| ResolvedLayout::Leaf {
                pane_id: pane_id.clone(),
                width: *width,
                height: *height,
            }),
        LayoutNode::Split { dir, children } => {
            let mut resolved = Vec::with_capacity(children.len());
            for child in children {
                resolved.push(resolve_layout(child, index_to_id)?);
            }
            Some(ResolvedLayout::Split {
                dir: *dir,
                children: resolved,
            })
        }
    }
}

/// Collect pane ids in layout order.
pub fn pane_ids_in_layout(node: &ResolvedLayout) -> Vec<String> {
    let mut out = Vec::new();
    collect_pane_ids(node, &mut out);
    out
}

fn collect_pane_ids(node: &ResolvedLayout, out: &mut Vec<String>) {
    match node {
        ResolvedLayout::Leaf { pane_id, .. } => out.push(pane_id.clone()),
        ResolvedLayout::Split { children, .. } => {
            for child in children {
                collect_pane_ids(child, out);
            }
        }
    }
}

fn parse_node(s: &str, base: usize) -> Result<(LayoutNode, usize), LayoutError> {
    let mut pos = 0;
    let (width, height) = parse_dims(s, &mut pos, base)?;
    let x = parse_u16(s, &mut pos, base)?;
    let _ = x;
    let y = parse_u16(s, &mut pos, base)?;
    let _ = y;
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
                    children,
                },
                end,
            ))
        }
        _ => {
            let pane_index = parse_usize(s, &mut pos, base)?;
            Ok((
                LayoutNode::Leaf {
                    pane_index,
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
            detail: "expected pane index".into(),
        });
    }
    s[start..*pos].parse().map_err(|e| LayoutError::Parse {
        pos: base + start,
        detail: format!("pane index: {e}"),
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

/// Assign render rectangles to each pane in a resolved layout tree.
pub fn layout_pane_slots(node: &ResolvedLayout, area: Rect, focused_pane: &str) -> Vec<PaneSlot> {
    let mut out = Vec::new();
    layout_slots_recursive(node, area, focused_pane, &mut out);
    out
}

fn layout_slots_recursive(
    node: &ResolvedLayout,
    area: Rect,
    focused: &str,
    out: &mut Vec<PaneSlot>,
) {
    match node {
        ResolvedLayout::Leaf { pane_id, .. } => {
            out.push(PaneSlot {
                pane_id: pane_id.clone(),
                rect: area,
                focused: pane_id == focused,
            });
        }
        ResolvedLayout::Split { dir, children } => {
            let weights: Vec<u32> = children
                .iter()
                .map(|c| primary_size(c, *dir) as u32)
                .collect();
            let total: u32 = weights.iter().sum::<u32>().max(1);
            let mut offset_x = area.x;
            let mut offset_y = area.y;
            for (i, child) in children.iter().enumerate() {
                let share = weights[i];
                let is_last = i + 1 == children.len();
                let child_area = match dir {
                    SplitDir::Horizontal => {
                        let w = if is_last {
                            area.width.saturating_sub(offset_x - area.x)
                        } else {
                            (area.width as u32 * share / total) as u16
                        };
                        let rect = Rect::new(offset_x, area.y, w.max(1), area.height);
                        offset_x = offset_x.saturating_add(w);
                        rect
                    }
                    SplitDir::Vertical => {
                        let h = if is_last {
                            area.height.saturating_sub(offset_y - area.y)
                        } else {
                            (area.height as u32 * share / total) as u16
                        };
                        let rect = Rect::new(area.x, offset_y, area.width, h.max(1));
                        offset_y = offset_y.saturating_add(h);
                        rect
                    }
                };
                layout_slots_recursive(child, child_area, focused, out);
            }
        }
    }
}

fn primary_size(node: &ResolvedLayout, dir: SplitDir) -> u16 {
    match node {
        ResolvedLayout::Leaf { width, height, .. } => match dir {
            SplitDir::Horizontal => *width,
            SplitDir::Vertical => *height,
        },
        ResolvedLayout::Split { children, .. } => {
            children.iter().map(|c| primary_size(c, dir)).sum()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_pane_leaf() {
        let node = parse_layout("b25e,80x24,0,0,0").expect("parse");
        assert_eq!(
            node,
            LayoutNode::Leaf {
                pane_index: 0,
                width: 80,
                height: 24,
            }
        );
    }

    #[test]
    fn vertical_split_two_panes() {
        let node = parse_layout("4c3e,319x89,0,0[319x44,0,0,0,319x44,0,45,1]").expect("parse");
        assert_eq!(
            node,
            LayoutNode::Split {
                dir: SplitDir::Vertical,
                children: vec![
                    LayoutNode::Leaf {
                        pane_index: 0,
                        width: 319,
                        height: 44,
                    },
                    LayoutNode::Leaf {
                        pane_index: 1,
                        width: 319,
                        height: 44,
                    },
                ],
            }
        );
    }

    #[test]
    fn horizontal_split_two_panes() {
        let node = parse_layout("da0d,200x50,0,0{100x50,0,0,0,100x50,100,0,1}").expect("parse");
        assert_eq!(
            node,
            LayoutNode::Split {
                dir: SplitDir::Horizontal,
                children: vec![
                    LayoutNode::Leaf {
                        pane_index: 0,
                        width: 100,
                        height: 50,
                    },
                    LayoutNode::Leaf {
                        pane_index: 1,
                        width: 100,
                        height: 50,
                    },
                ],
            }
        );
    }

    #[test]
    fn nested_split() {
        let layout = "abcd,200x50,0,0[200x25,0,0{100x25,0,0,0,100x25,100,0,0},200x24,0,26,2]";
        let node = parse_layout(layout).expect("parse");
        assert!(matches!(
            node,
            LayoutNode::Split {
                dir: SplitDir::Vertical,
                ..
            }
        ));
        if let LayoutNode::Split { children, .. } = node {
            assert_eq!(children.len(), 2);
            assert!(matches!(
                children[0],
                LayoutNode::Split {
                    dir: SplitDir::Horizontal,
                    ..
                }
            ));
            assert!(matches!(
                children[1],
                LayoutNode::Leaf { pane_index: 2, .. }
            ));
        }
    }

    #[test]
    fn resolve_maps_indices_to_ids() {
        let node = parse_layout("4c3e,319x89,0,0[319x44,0,0,0,319x44,0,45,1]").unwrap();
        let map = HashMap::from([(0, "%0".to_string()), (1, "%1".to_string())]);
        let resolved = resolve_layout(&node, &map).expect("resolve");
        assert_eq!(
            pane_ids_in_layout(&resolved),
            vec!["%0".to_string(), "%1".to_string()]
        );
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

        server.run_ok(&["select-pane", "-t", "lay:0.1"]);
        server.run_ok(&["split-window", "-v", "-t", "lay:0.1"]);
        let out = server.run(&["list-windows", "-t", "lay", "-F", "#{window_layout}"]);
        let nested = String::from_utf8_lossy(&out.stdout);
        parse_layout(nested.trim()).expect("nested split layout should parse");
    }
}
