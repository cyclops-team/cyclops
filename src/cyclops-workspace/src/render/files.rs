//! The sidebar's file panel: a header naming the folder, then one row per
//! entry of [`crate::files::FileTree`].
//!
//! It shares the sidebar with the session tree and sits under it. Nothing
//! here reads the filesystem — the tree was already read, and this only
//! turns its rows into cells and hit regions.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Span;

use crate::copy;
use crate::files::{FileRow, FileTree, RowKind};
use crate::input::mouse::{HitMap, HitTarget};
use crate::theme::{self, Paint};

/// Disclosure markers, the same triangles the session tree uses for
/// workspace rows: one shape language for "this opens".
const DIR_OPEN: &str = "▾";
const DIR_SHUT: &str = "▸";

/// The climb-out control. A row rather than a button in the header,
/// because it is the one thing an operator reaches for constantly and a
/// full row is a target they cannot miss.
const UP_ROW: &str = "..";

/// Columns one nesting level indents by. Two, matching the session tree's
/// agent rows, so the two halves of the sidebar read as one panel.
const INDENT: u16 = 2;

/// Paint the file panel into `area` and record a hit region per row.
///
/// `area` includes the header row. The tree's own scroll is clamped here
/// because this is the only place that knows how tall the list ended up.
pub fn paint_file_panel(
    tree: &mut FileTree,
    area: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    hits: &mut HitMap,
    hover: Option<(u16, u16)>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    buf.set_style(area, theme::chrome_panel(paint));
    paint_header(tree, area, buf, paint, hits, hover);
    if area.height <= 1 {
        return;
    }

    let list = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
    if !tree.has_root() {
        super::overlay_text(
            buf,
            list,
            list.x,
            list.y,
            copy::FILES_NO_ROOT,
            theme::menu_hint(paint),
        );
        return;
    }

    // The climb-out row is always the first thing in the list and never
    // scrolls away: it is how you leave, and a way out that scrolls off is
    // not a way out.
    let mut y = list.y;
    if tree.parent().is_some() {
        let rect = Rect::new(list.x, y, list.width, 1);
        let lit = hovered(hover, rect);
        if lit {
            buf.set_style(rect, theme::menu_row_hover(paint));
        }
        super::overlay_text(
            buf,
            list,
            list.x,
            y,
            UP_ROW,
            if lit {
                theme::menu_row_hover(paint)
            } else {
                theme::menu_hint(paint)
            },
        );
        hits.push(rect, HitTarget::FileUp);
        y += 1;
    }

    let rows_height = usize::from((list.y + list.height).saturating_sub(y));
    tree.clamp_scroll(rows_height);
    let start = tree.scroll();
    for (index, row) in tree.rows().iter().enumerate().skip(start).take(rows_height) {
        paint_row(
            row,
            index,
            tree,
            Rect::new(list.x, y, list.width, 1),
            buf,
            paint,
            hits,
            hover,
        );
        y += 1;
    }

    // How much is below the fold, in the vocabulary the session tree
    // already uses for the same fact.
    let unseen = tree.rows().len().saturating_sub(start + rows_height);
    if unseen > 0 {
        let note = copy::more_below(unseen);
        let bottom = list.y + list.height - 1;
        let x = list
            .x
            .saturating_add(list.width.saturating_sub(width_of(&note)));
        super::overlay_text(buf, list, x, bottom, &note, theme::menu_hint(paint));
    }
}

/// The folder this panel is looking at. Its own row, dim, above the list.
fn paint_header(
    tree: &FileTree,
    area: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    hits: &mut HitMap,
    hover: Option<(u16, u16)>,
) {
    let name = if tree.has_root() {
        tree.root()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| tree.root().to_string_lossy().into_owned())
    } else {
        copy::FILES_TITLE.to_string()
    };
    let rect = Rect::new(area.x, area.y, area.width, 1);
    let lit = hovered(hover, rect);
    let style = if lit {
        theme::menu_row_hover(paint)
    } else {
        theme::sidebar_workspace(paint)
    };
    if lit {
        buf.set_style(rect, style);
    }
    super::overlay_text(buf, rect, area.x, area.y, &clip(&name, area.width), style);
    // Clicking the folder name re-roots on the focused pane's directory:
    // the panel's "take me where the work is" control.
    hits.push(rect, HitTarget::FileRoot);
}

#[allow(clippy::too_many_arguments)]
fn paint_row(
    row: &FileRow,
    index: usize,
    tree: &FileTree,
    rect: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    hits: &mut HitMap,
    hover: Option<(u16, u16)>,
) {
    let lit = hovered(hover, rect);
    if lit {
        buf.set_style(rect, theme::menu_row_hover(paint));
    }
    let indent = row.depth.saturating_mul(INDENT);
    let x = rect.x.saturating_add(indent);
    let room = rect.width.saturating_sub(indent);
    if room == 0 {
        return;
    }

    let (text, style) = match row.kind {
        RowKind::Dir { expanded } => (
            format!(
                "{} {}",
                if expanded { DIR_OPEN } else { DIR_SHUT },
                row.name
            ),
            theme::sidebar_workspace(paint),
        ),
        // Files sit one column in from their sibling folders' markers, so
        // a folder's chevron column stays a clean vertical line the eye can
        // follow down the tree.
        RowKind::File => (format!("  {}", row.name), theme::sidebar_row(paint)),
        RowKind::Truncated { hidden } => (copy::more_files(hidden), theme::menu_hint(paint)),
    };
    let style = if lit {
        theme::menu_row_hover(paint)
    } else {
        style
    };
    super::overlay_text(buf, rect, x, rect.y, &clip(&text, room), style);

    // A truncation notice is not a path; clicking it must do nothing, so
    // it gets no hit region at all rather than one that no-ops later.
    if matches!(row.kind, RowKind::Truncated { .. }) {
        return;
    }
    hits.push(
        rect,
        HitTarget::FileRow {
            index,
            path: row.path.to_string_lossy().into_owned(),
            is_dir: matches!(row.kind, RowKind::Dir { .. }),
            reference: tree.reference(&row.path),
        },
    );
}

fn hovered(hover: Option<(u16, u16)>, rect: Rect) -> bool {
    hover.is_some_and(|(col, row)| {
        row == rect.y && col >= rect.x && col < rect.x.saturating_add(rect.width)
    })
}

fn width_of(text: &str) -> u16 {
    u16::try_from(Span::raw(text).width()).unwrap_or(u16::MAX)
}

/// Cut `text` to `width` display columns, keeping the END of a name.
///
/// The tail, not the head: filenames in one folder share prefixes far more
/// often than suffixes, and `…rate_limiter.rs` identifies a file that
/// `src/handlers/inbo…` does not.
fn clip(text: &str, width: u16) -> String {
    let width = usize::from(width);
    if width == 0 {
        return String::new();
    }
    if Span::raw(text).width() <= width {
        return text.to_string();
    }
    let budget = width.saturating_sub(1);
    let mut start = text.len();
    for (index, _) in text.char_indices().rev() {
        if Span::raw(&text[index..]).width() > budget {
            break;
        }
        start = index;
    }
    format!("…{}", &text[start..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::test_support::flatten;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = cyclops_proto::scratch::scratch_dir(name);
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch");
            Scratch(dir)
        }

        fn file(&self, rel: &str, body: &str) {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("mkdir -p");
            }
            std::fs::write(path, body).expect("write");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const PANEL: Rect = Rect {
        x: 0,
        y: 0,
        width: 22,
        height: 10,
    };

    fn draw(tree: &mut FileTree, hover: Option<(u16, u16)>) -> (Buffer, HitMap) {
        let mut term = Terminal::new(TestBackend::new(PANEL.width, PANEL.height)).unwrap();
        let mut hits = HitMap::default();
        term.draw(|f| {
            paint_file_panel(
                tree,
                f.area(),
                f.buffer_mut(),
                &Paint::for_test(),
                &mut hits,
                hover,
            );
        })
        .unwrap();
        (term.backend().buffer().clone(), hits)
    }

    /// The shape of the panel: the folder's name on top, its entries under
    /// it, folders marked as openable, children indented under their
    /// parent.
    #[test]
    fn the_panel_reads_as_a_folder() {
        let s = Scratch::new("files-panel-shape");
        s.file("src/main.rs", "");
        s.file("README.md", "");
        let mut tree = FileTree::new();
        tree.reroot(&s.0);
        tree.toggle(&s.0.join("src"));

        let (buf, _) = draw(&mut tree, None);
        let flat = flatten(&buf);
        assert!(flat.contains("src"), "the folder is listed: {flat}");
        assert!(flat.contains(DIR_OPEN), "an open folder shows it: {flat}");
        assert!(flat.contains("main.rs"), "its child is listed: {flat}");
        assert!(flat.contains("README.md"));
        assert!(flat.contains(UP_ROW), "and there is a way out: {flat}");

        // main.rs sits under src, indented past it.
        let row_of = |needle: &str| {
            (0..PANEL.height)
                .find(|y| {
                    (0..PANEL.width)
                        .map(|x| buf[(x, *y)].symbol().to_string())
                        .collect::<String>()
                        .contains(needle)
                })
                .unwrap_or_else(|| panic!("{needle} is not painted"))
        };
        // Counted in CELLS, not bytes: the disclosure markers are
        // multi-byte, so a byte offset reports a folder's name three
        // columns left of where it is and the indent assertion below
        // silently compares the wrong two numbers.
        let col_of = |needle: &str, y: u16| {
            (0..PANEL.width)
                .find(|x| {
                    (*x..PANEL.width)
                        .map(|c| buf[(c, y)].symbol().to_string())
                        .collect::<String>()
                        .starts_with(needle)
                })
                .expect("on this row")
        };
        let src_y = row_of("src");
        let main_y = row_of("main.rs");
        assert!(main_y > src_y, "a child is painted under its folder");
        assert!(
            col_of("main.rs", main_y) > col_of("src", src_y),
            "and indented past it"
        );
    }

    /// Every row answers the mouse, and says what a click on it means. A
    /// directory toggles; a file carries the reference a message would use.
    #[test]
    fn rows_carry_what_a_click_needs_to_know() {
        let s = Scratch::new("files-panel-hits");
        s.file("src/main.rs", "");
        let mut tree = FileTree::new();
        tree.reroot(&s.0);
        tree.toggle(&s.0.join("src"));
        let (_, hits) = draw(&mut tree, None);

        let rows: Vec<&HitTarget> = (0..PANEL.height)
            .filter_map(|y| hits.hit(1, y))
            .filter(|t| matches!(t, HitTarget::FileRow { .. }))
            .collect();
        assert_eq!(rows.len(), 2, "one hit per painted entry");

        let dir = rows
            .iter()
            .find(|t| matches!(t, HitTarget::FileRow { is_dir: true, .. }))
            .expect("the folder answers");
        assert!(matches!(dir, HitTarget::FileRow { path, .. } if path.ends_with("src")));

        let file = rows
            .iter()
            .find(|t| matches!(t, HitTarget::FileRow { is_dir: false, .. }))
            .expect("the file answers");
        assert!(matches!(
            file,
            HitTarget::FileRow { reference, .. } if reference == "src/main.rs"
        ));
    }

    /// The climb-out row is the first thing in the list and stays there. A
    /// way out that scrolls away is not a way out.
    #[test]
    fn the_way_out_never_scrolls_off() {
        let s = Scratch::new("files-panel-up");
        for n in 0..40 {
            s.file(&format!("f{n:02}.txt"), "");
        }
        let mut tree = FileTree::new();
        tree.reroot(&s.0);
        tree.scroll_by(30, 8);

        let (_, hits) = draw(&mut tree, None);
        let up = (0..PANEL.height).find(|y| matches!(hits.hit(1, *y), Some(HitTarget::FileUp)));
        assert_eq!(up, Some(1), "directly under the header, scrolled or not");
    }

    /// A list longer than the panel says how much is below it, in the same
    /// words the session tree uses for the same fact.
    #[test]
    fn a_long_list_says_what_is_below_the_fold() {
        let s = Scratch::new("files-panel-more");
        for n in 0..40 {
            s.file(&format!("f{n:02}.txt"), "");
        }
        let mut tree = FileTree::new();
        tree.reroot(&s.0);

        let (buf, _) = draw(&mut tree, None);
        let flat = flatten(&buf);
        assert!(
            flat.contains("more"),
            "the panel admits what it clipped: {flat}"
        );
    }

    /// A name too long for the panel keeps its end. Filenames in one folder
    /// share prefixes, so the tail is the part that identifies the file.
    #[test]
    fn a_long_name_keeps_the_end_that_identifies_it() {
        assert_eq!(clip("main.rs", 22), "main.rs");
        assert_eq!(clip("a_very_long_handler_name.rs", 12), "…ler_name.rs");
        assert_eq!(clip("x", 0), "");
    }

    /// Pointing at a row lights it. Without this the panel is a wall of
    /// text with no sign that any of it answers a click.
    #[test]
    fn pointing_at_a_row_lights_it() {
        let s = Scratch::new("files-panel-hover");
        s.file("README.md", "");
        let mut tree = FileTree::new();
        tree.reroot(&s.0);

        let (rest, hits) = draw(&mut tree, None);
        let row = (0..PANEL.height)
            .find(|y| matches!(hits.hit(1, *y), Some(HitTarget::FileRow { .. })))
            .expect("a row to point at");
        let (hot, _) = draw(&mut tree, Some((1, row)));
        assert_ne!(
            hot[(1, row)].style(),
            rest[(1, row)].style(),
            "pointing at a row has to show"
        );
    }

    /// Before any folder is known the panel says so rather than painting an
    /// empty box that reads as a folder with nothing in it.
    #[test]
    fn a_rootless_panel_says_it_has_no_folder_yet() {
        let mut tree = FileTree::new();
        let (buf, hits) = draw(&mut tree, None);
        assert!(flatten(&buf).contains(copy::FILES_NO_ROOT));
        assert!(
            (0..PANEL.height).all(|y| !matches!(hits.hit(1, y), Some(HitTarget::FileRow { .. }))),
            "and offers no rows to click"
        );
    }
}
