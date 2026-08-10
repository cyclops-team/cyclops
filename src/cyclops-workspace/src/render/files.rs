//! The sidebar's file panel: a header naming the folder, a navigation row
//! under it, then one row per entry of [`crate::files::FileTree`].
//!
//! It shares the sidebar with the session tree and sits under it. Nothing
//! here reads the filesystem — the tree was already read, and this only
//! turns its rows into cells and hit regions.
//!
//! The panel is three bands, and the top two never scroll:
//!
//! ```text
//!   clops-workspace          header: the folder this is looking at
//!   ..              ◂ ▸      navigation: climb out, back, forward
//!   ▸ src                    entries
//!     (rs) main.rs
//!     (md) README.md
//!   +12 more                 what did not fit, on a row of its own
//! ```

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

/// Retrace the walk, and undo that. Same triangles as the sidebar's own
/// collapse chevron, pointing the way they move you, because this panel
/// should not invent a third arrow vocabulary for the same idea.
const NAV_BACK: &str = "◂";
const NAV_FORWARD: &str = "▸";

/// Columns one nesting level indents by. Two, matching the session tree's
/// agent rows, so the two halves of the sidebar read as one panel.
const INDENT: u16 = 2;

/// Longest type tag painted. Past four the badge is eating the name it was
/// meant to identify, and no common extension is longer.
const TAG_MAX: usize = 4;

/// Columns a name needs before the badge is worth its own width. Under
/// this the badge would be identifying a file by two letters of its name,
/// which is not identifying it at all.
const MIN_NAME: u16 = 5;

/// Rows the panel spends on chrome that never scrolls: the header and the
/// navigation row.
const CHROME_ROWS: u16 = 2;

/// Columns a file row leads with before its badge, so a folder's chevron
/// column stays a clean vertical line the eye can follow down the tree.
const FILE_PREFIX: u16 = 2;

/// Paint the file panel into `area` and record a hit region per row.
///
/// `area` includes the header and navigation rows. The tree's own scroll is
/// clamped here because this is the only place that knows how tall the list
/// ended up.
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

    if !tree.has_root() {
        super::overlay_text(
            buf,
            area,
            area.x,
            area.y + 1,
            copy::FILES_NO_ROOT,
            theme::menu_hint(paint),
        );
        return;
    }

    paint_nav(
        tree,
        Rect::new(area.x, area.y + 1, area.width, 1),
        buf,
        paint,
        hits,
        hover,
    );
    if area.height <= CHROME_ROWS {
        return;
    }

    let body = area.height - CHROME_ROWS;
    let total = tree.rows().len();
    // The overflow note gets a row to itself rather than being written over
    // the last entry. It used to share that row, and the result was a name
    // with a count welded onto its end: ".zsh_histor+3 more".
    //
    // Whether the row is reserved is a property of the list, not of where
    // the list is scrolled to. Deciding it from the scroll position instead
    // would flip the capacity by one at the bottom of the list, which flips
    // whether it overflows, which flips the capacity back.
    let overflows = total > usize::from(body);
    let shown = if overflows { body - 1 } else { body };
    tree.clamp_scroll(usize::from(shown));
    // Then follow the keyboard cursor, which may sit outside that window.
    // Order matters: clamping after this would undo the reveal.
    tree.reveal_cursor(usize::from(shown));

    let list = Rect::new(area.x, area.y + CHROME_ROWS, area.width, shown);
    let badge = badge_field(tree, list.width);
    let cursor = tree.cursor();
    let start = tree.scroll();
    // `index` is the row's place in the tree, which a click reports; `y` is
    // where it lands on screen. They differ by the scroll, so the screen row
    // is zipped in rather than counted alongside.
    for ((index, row), y) in tree
        .rows()
        .iter()
        .enumerate()
        .skip(start)
        .take(usize::from(shown))
        .zip(list.y..)
    {
        paint_row(
            row,
            index,
            tree,
            Rect::new(list.x, y, list.width, 1),
            badge,
            buf,
            paint,
            hits,
            hover,
            cursor == Some(index),
        );
    }

    // How much is below the fold, in the vocabulary the session tree
    // already uses for the same fact. Zero at the bottom of the list leaves
    // the reserved row blank, which is what the end of a list looks like.
    let unseen = total.saturating_sub(start + usize::from(shown));
    if overflows && unseen > 0 {
        let note = copy::more_below(unseen);
        let bottom = area.y + area.height - 1;
        let x = area
            .x
            .saturating_add(area.width.saturating_sub(width_of(&note)));
        super::overlay_text(buf, area, x, bottom, &note, theme::menu_hint(paint));
    }
}

/// The folder this panel is looking at. Its own row, above everything.
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
    // The whole width for the name. Everything else this panel navigates
    // with lives on the row below, so the one thing that says WHERE you are
    // never competes with a control for columns.
    super::overlay_text(
        buf,
        rect,
        area.x,
        area.y,
        &clip_head(&name, area.width),
        style,
    );
    // Clicking the folder name re-roots on the focused pane's directory:
    // the panel's "take me where the work is" control.
    hits.push(rect, HitTarget::FileRoot);
}

/// The navigation row: climb out at the left, retrace at the right.
///
/// Painted whether or not any of the three has anywhere to go. A row that
/// existed only while a control did would slide the whole list up and down
/// as an operator walks the tree, under a pointer already on a target, and
/// the same reasoning already keeps the session tree's daemon note on a row
/// it does not always fill.
///
/// An unavailable control is painted dim and given no hit region, so it can
/// be seen to be unavailable rather than answering a click with nothing.
fn paint_nav(
    tree: &FileTree,
    row: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    hits: &mut HitMap,
    hover: Option<(u16, u16)>,
) {
    // The arrows own the right end of the row. Measured first, because the
    // climb-out takes everything to their left and needs to know where they
    // start.
    let pair = width_of(NAV_BACK) + 1 + width_of(NAV_FORWARD);
    let arrows_x = row.width.checked_sub(pair).map(|left| row.x + left);

    if tree.parent().is_some() {
        // Everything up to the arrows, not just the two cells the glyphs
        // occupy. This is the most-used control in the panel and it reads
        // as a row, so it has to answer as one: a four-column target left
        // the middle of the row dead to both clicks and the wheel.
        let reach = Rect::new(
            row.x,
            row.y,
            arrows_x.unwrap_or(row.x + row.width).saturating_sub(row.x),
            1,
        );
        let lit = hovered(hover, reach);
        if lit {
            buf.set_style(reach, theme::menu_row_hover(paint));
        }
        super::overlay_text(
            buf,
            row,
            row.x,
            row.y,
            UP_ROW,
            if lit {
                theme::menu_row_hover(paint)
            } else {
                theme::menu_hint(paint)
            },
        );
        hits.push(reach, HitTarget::FileUp);
    }

    // Both arrows or neither, right-aligned as one block, so the pair keeps
    // one shape and one place whichever of them currently has somewhere to
    // go.
    let Some(x) = arrows_x else {
        return;
    };
    let controls = [
        (NAV_BACK, tree.can_go_back(), HitTarget::FileBack, x),
        (
            NAV_FORWARD,
            tree.can_go_forward(),
            HitTarget::FileForward,
            x + width_of(NAV_BACK) + 1,
        ),
    ];
    for (glyph, live, target, at) in controls {
        let cell = Rect::new(at, row.y, width_of(glyph), 1);
        let lit = live && hovered(hover, cell);
        let style = if lit {
            theme::add_button_hover(paint)
        } else if live {
            theme::sidebar_footer_button(paint)
        } else {
            theme::menu_hint(paint)
        };
        super::overlay_text(buf, row, at, row.y, glyph, style);
        if live {
            hits.push(cell, target);
        }
    }
}

/// Columns every file row spends on its type badge, the space after it
/// included, or zero when the panel cannot afford them.
///
/// Measured over the whole tree rather than the rows currently on screen.
/// A field sized to the visible window would slide every name sideways as
/// the list scrolled past a longer tag, and a column that moves while you
/// read it is worse than one that is wider than it needs to be.
///
/// It can still move: opening a folder that holds a longer extension, or a
/// poll that sees one written, rewidens the field. Both are moments the
/// panel is redrawing anyway, which is why this is the cheaper of the two
/// wrong answers.
fn badge_field(tree: &FileTree, room: u16) -> u16 {
    let mut widest = 0u16;
    let mut deepest = 0u16;
    for row in tree.rows() {
        deepest = deepest.max(row.depth);
        if let Some(tag) = row.type_tag() {
            // Display columns, not chars, and the same measure `badge`
            // pads by. A tag of CJK characters counts two columns each, so
            // sizing this by char count leaves that one row's name pushed
            // right of every other row's, which is the one thing the
            // shared field exists to prevent.
            widest = widest.max(tag_columns(&tag));
        }
    }
    if widest == 0 {
        return 0;
    }
    // The tag, its two parens, and one space before the name.
    let field = widest.saturating_add(3);
    // Checked against the DEEPEST row in the tree, not a top-level one.
    // Every row spends the same badge width, so a badge that only fits at
    // the top leaves a row four folders down painting a tag and no name at
    // all. Nesting costs INDENT per level and comes out of the same budget.
    let name_room = room
        .saturating_sub(deepest.saturating_mul(INDENT))
        .saturating_sub(FILE_PREFIX)
        .saturating_sub(field);
    if name_room < MIN_NAME {
        0
    } else {
        field
    }
}

/// `(md) ` for a tag of `md`, padded to `field` so names line up.
///
/// Padded on the LEFT, so the badge sits against the name it describes
/// rather than across a gap from it. A folder holding both `.rs` and
/// `.toml` reads `(rs) main.rs` over `(toml) Cargo.toml`, names in one
/// column and each tag touching its own name, instead of `(rs)   main.rs`
/// with the tag stranded three cells away.
fn badge(tag: &str, field: u16) -> String {
    let text = format!("({})", elide_tag(tag));
    // Padded by display columns, not by chars: `{:>n$}` counts chars, and a
    // two-column glyph padded as though it were one column pushes that
    // row's name right of every other row's.
    let pad = usize::from(field.saturating_sub(1).saturating_sub(width_of(&text)));
    format!("{}{text} ", " ".repeat(pad))
}

/// A tag cut to [`TAG_MAX`] columns, saying so when it had to cut.
///
/// Silent truncation states a type that is not the file's: `notes.markdown`
/// read as `(mark)`, `App.svelte` as `(svel)`, `db.sqlite3` as `(sqli)`. The
/// badge exists so a clipped name still says what a file is, so a badge that
/// lies about it is worse than no badge. The ellipsis costs nothing: it
/// takes the column the fourth character would have, so `(mar…)` is exactly
/// as wide as `(toml)` and the shared field does not move.
fn elide_tag(tag: &str) -> String {
    // The tag's real width, not `tag_columns`: that one caps at TAG_MAX,
    // so asking it whether the tag exceeds TAG_MAX always answers no.
    if usize::from(width_of(tag)) <= TAG_MAX {
        return tag.to_string();
    }
    let budget = TAG_MAX.saturating_sub(1);
    let mut end = 0;
    for (index, ch) in tag.char_indices() {
        let next = index + ch.len_utf8();
        if usize::from(tag_columns(&tag[..next])) > budget {
            break;
        }
        end = next;
    }
    format!("{}…", &tag[..end])
}

/// A tag's width in display columns, capped at what a badge will spend.
fn tag_columns(tag: &str) -> u16 {
    width_of(tag).min(u16::try_from(TAG_MAX).unwrap_or(u16::MAX))
}

#[allow(clippy::too_many_arguments)]
fn paint_row(
    row: &FileRow,
    index: usize,
    tree: &FileTree,
    rect: Rect,
    badge_width: u16,
    buf: &mut Buffer,
    paint: &Paint,
    hits: &mut HitMap,
    hover: Option<(u16, u16)>,
    under_cursor: bool,
) {
    // The keyboard cursor outranks the pointer's hover. Both are "the row
    // you are about to act on", and only one of them is holding the
    // keyboard, so when they disagree the keyboard wins.
    let lit = under_cursor || hovered(hover, rect);
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
                clip_head(&row.name, room.saturating_sub(FILE_PREFIX)),
            ),
            theme::sidebar_workspace(paint),
        ),
        // Files sit one column in from their sibling folders' markers, so
        // a folder's chevron column stays a clean vertical line the eye can
        // follow down the tree. The badge follows that gap, which puts
        // every type tag in one column too.
        RowKind::File => {
            let lead = match row.type_tag() {
                Some(tag) if badge_width > 0 => badge(&tag, badge_width),
                // A name with no tag still lines up with the ones that have
                // one. A ragged left edge on half the rows reads as damage.
                _ => " ".repeat(usize::from(badge_width)),
            };
            let name_room = room.saturating_sub(FILE_PREFIX).saturating_sub(badge_width);
            (
                format!("  {lead}{}", clip_head(&row.name, name_room)),
                theme::sidebar_row(paint),
            )
        }
        RowKind::Truncated { hidden } => (copy::more_files(hidden), theme::menu_hint(paint)),
    };
    let style = if lit {
        theme::menu_row_hover(paint)
    } else {
        style
    };
    super::overlay_text(buf, rect, x, rect.y, &text, style);

    // A truncation notice is not a path; clicking it must do nothing, so
    // it gets no hit region at all rather than one that no-ops later.
    if matches!(row.kind, RowKind::Truncated { .. }) {
        return;
    }
    let is_dir = matches!(row.kind, RowKind::Dir { .. });
    hits.push(
        rect,
        HitTarget::FileRow {
            index,
            path: row.path.to_string_lossy().into_owned(),
            is_dir,
            reference: tree.reference(&row.path),
        },
    );
    // The chevron column keeps the open-in-place meaning the whole row used
    // to carry, now that clicking the name walks into the folder instead.
    // Pushed AFTER the row so it wins its own cells: `HitMap::hit` scans in
    // reverse. Same two-target shape as a workspace row's disclosure.
    if is_dir {
        hits.push(
            Rect::new(x, rect.y, width_of(DIR_SHUT).min(room), 1),
            HitTarget::FileDisclosure {
                path: row.path.to_string_lossy().into_owned(),
            },
        );
    }
}

fn hovered(hover: Option<(u16, u16)>, rect: Rect) -> bool {
    hover.is_some_and(|(col, row)| {
        row == rect.y && col >= rect.x && col < rect.x.saturating_add(rect.width)
    })
}

fn width_of(text: &str) -> u16 {
    u16::try_from(Span::raw(text).width()).unwrap_or(u16::MAX)
}

/// Cut `text` to `width` display columns, keeping the START.
///
/// The panel used to keep the END of every name, on the reasoning that
/// names in one folder share prefixes more often than suffixes, so
/// `…rate_limiter.rs` beat `src/handlers/inbo…`. That reasoning was about
/// PATHS, and this panel paints bare names. The suffix a bare filename
/// actually shares with its neighbours is its extension, and a badged row
/// already shows that. So on a badged row the tail is the redundant end:
/// `(md) HELL…` identifies a file, `(md) …LLO.md` does not.
///
/// One rule, not two. Clipping the other way when the panel is too narrow
/// for a badge was tried and is worse: at that width every row is cut, and
/// a column reading `…n.rs` `…r.rs` `…g.rs` tells you only what the folder
/// already told you. The head at least says which file.
///
/// What this costs, named honestly: names that share a long prefix collapse
/// onto each other, so `test_auth_login.rs` and `test_auth_logout.rs` both
/// cut to `test_auth_…`. Widening the panel is the answer there, and the
/// panel is draggable.
fn clip_head(text: &str, width: u16) -> String {
    let width = usize::from(width);
    if width == 0 {
        return String::new();
    }
    if Span::raw(text).width() <= width {
        return text.to_string();
    }
    let budget = width.saturating_sub(1);
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        let next = index + ch.len_utf8();
        if Span::raw(&text[..next]).width() > budget {
            break;
        }
        end = next;
    }
    format!("{}…", &text[..end])
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
        draw_sized(tree, hover, PANEL.width, PANEL.height)
    }

    fn draw_sized(
        tree: &mut FileTree,
        hover: Option<(u16, u16)>,
        width: u16,
        height: u16,
    ) -> (Buffer, HitMap) {
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
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

    /// Row index of the first row whose painted text contains `needle`.
    fn row_of(buf: &Buffer, needle: &str) -> u16 {
        (0..buf.area.height)
            .find(|y| line(buf, *y).contains(needle))
            .unwrap_or_else(|| panic!("{needle} is not painted"))
    }

    fn line(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect()
    }

    /// Column the painted `needle` starts on, counted in CELLS.
    ///
    /// Not bytes: the disclosure markers are multi-byte, so a byte offset
    /// reports a folder's name three columns left of where it is and an
    /// indent assertion silently compares the wrong two numbers.
    fn col_of(buf: &Buffer, needle: &str, y: u16) -> u16 {
        (0..buf.area.width)
            .find(|x| {
                (*x..buf.area.width)
                    .map(|c| buf[(c, y)].symbol().to_string())
                    .collect::<String>()
                    .starts_with(needle)
            })
            .expect("on this row")
    }

    /// The shape of the panel: the folder's name on top, the navigation row
    /// under it, its entries below that, folders marked as openable,
    /// children indented under their parent.
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

        let src_y = row_of(&buf, "src");
        let main_y = row_of(&buf, "main.rs");
        assert!(main_y > src_y, "a child is painted under its folder");
        assert!(
            col_of(&buf, "main.rs", main_y) > col_of(&buf, "src", src_y),
            "and indented past it"
        );
    }

    /// A file row leads with its type, so a name the panel had to cut still
    /// says what the thing is. This is the whole point of the badge.
    #[test]
    fn a_file_leads_with_its_type_and_keeps_it_when_the_name_is_cut() {
        let s = Scratch::new("files-panel-badge");
        s.file("HELLO.md", "");
        s.file("a_very_long_module_name_indeed.rs", "");
        let mut tree = FileTree::new();
        tree.reroot(&s.0);

        let (buf, _) = draw(&mut tree, None);
        let hello = line(&buf, row_of(&buf, "HELLO"));
        assert!(hello.contains("(md)"), "the type leads the row: {hello}");
        assert!(hello.contains("HELLO.md"), "{hello}");

        // The long one cannot fit, and what survives is the type plus the
        // head of the name.
        let long = line(&buf, row_of(&buf, "a_very"));
        assert!(long.contains("(rs)"), "the type survives the cut: {long}");
        assert!(long.contains('…'), "and the name was in fact cut: {long}");
        assert!(
            long.contains("a_very_l"),
            "the head of the name is what is kept: {long}"
        );
    }

    /// Names line up whether or not they have a tag. A ragged left edge on
    /// half the rows reads as damage rather than as a listing.
    #[test]
    fn a_file_with_no_type_still_lines_up_with_the_ones_that_have_one() {
        let s = Scratch::new("files-panel-badge-align");
        s.file("Makefile", "");
        s.file("main.rs", "");
        let mut tree = FileTree::new();
        tree.reroot(&s.0);

        let (buf, _) = draw(&mut tree, None);
        assert_eq!(
            col_of(&buf, "Makefile", row_of(&buf, "Makefile")),
            col_of(&buf, "main.rs", row_of(&buf, "main.rs")),
            "a tagged and an untagged name start in the same column"
        );
    }

    /// A panel too narrow to afford a badge drops it rather than spending
    /// the name's last columns on it.
    #[test]
    fn a_narrow_panel_spends_its_columns_on_the_name() {
        let s = Scratch::new("files-panel-badge-narrow");
        s.file("Cargo.toml", "");
        let mut tree = FileTree::new();
        tree.reroot(&s.0);

        let (wide, _) = draw_sized(&mut tree, None, 22, 8);
        assert!(flatten(&wide).contains("(toml)"), "{}", flatten(&wide));

        let (narrow, _) = draw_sized(&mut tree, None, 10, 8);
        let flat = flatten(&narrow);
        assert!(
            !flat.contains("(toml)"),
            "a badge that leaves no name is not worth its width: {flat}"
        );
    }

    /// Every row answers the mouse, and says what a click on it means. A
    /// folder's chevron opens it in place; the rest of a folder's row walks
    /// into it; a file carries the reference a message would use.
    #[test]
    fn rows_carry_what_a_click_needs_to_know() {
        let s = Scratch::new("files-panel-hits");
        s.file("src/main.rs", "");
        let mut tree = FileTree::new();
        tree.reroot(&s.0);
        tree.toggle(&s.0.join("src"));
        let (buf, hits) = draw(&mut tree, None);

        // Away from the chevron column, so this reads the row's own target.
        let rows: Vec<&HitTarget> = (0..PANEL.height)
            .filter_map(|y| hits.hit(6, y))
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

        // The chevron cell itself is the open-in-place control, and it must
        // win those cells from the row underneath it.
        let src_y = row_of(&buf, "src");
        assert!(
            matches!(hits.hit(0, src_y), Some(HitTarget::FileDisclosure { path }) if path.ends_with("src")),
            "the chevron column toggles rather than walking in"
        );
        // A file has no chevron, so nothing but the row answers on its row.
        let main_y = row_of(&buf, "main.rs");
        assert!(!matches!(
            hits.hit(0, main_y),
            Some(HitTarget::FileDisclosure { .. })
        ));
    }

    /// Back and forward sit on the navigation row, and only answer once
    /// they have somewhere to go. A control that takes a click and does
    /// nothing teaches the operator it is broken.
    #[test]
    fn retrace_controls_appear_dead_until_there_is_a_walk_to_retrace() {
        let s = Scratch::new("files-panel-history");
        s.file("src/main.rs", "");
        let mut tree = FileTree::new();
        tree.reroot(&s.0);

        let find = |hits: &HitMap, want: fn(&HitTarget) -> bool| {
            (0..PANEL.width)
                .flat_map(|x| (0..PANEL.height).map(move |y| (x, y)))
                .find(|&(x, y)| hits.hit(x, y).is_some_and(want))
        };

        let (fresh, hits) = draw(&mut tree, None);
        assert!(
            find(&hits, |t| matches!(t, HitTarget::FileBack)).is_none(),
            "nothing to go back to yet"
        );
        // Painted anyway, so the row does not change shape as you walk.
        assert!(
            line(&fresh, 1).contains(NAV_BACK),
            "the control is still visible: {}",
            line(&fresh, 1)
        );

        tree.reroot(s.0.join("src"));
        let (_, hits) = draw(&mut tree, None);
        let back = find(&hits, |t| matches!(t, HitTarget::FileBack))
            .expect("walking somewhere opens the way back");
        assert!(
            find(&hits, |t| matches!(t, HitTarget::FileForward)).is_none(),
            "but not the way forward"
        );
        assert_eq!(back.1, 1, "both live on the navigation row");

        tree.go_back();
        let (_, hits) = draw(&mut tree, None);
        assert!(
            find(&hits, |t| matches!(t, HitTarget::FileForward)).is_some(),
            "stepping back opens the way forward"
        );
    }

    /// The climb-out row is the first thing under the header and stays
    /// there. A way out that scrolls away is not a way out.
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
        let up = (0..PANEL.height).find(|y| matches!(hits.hit(0, *y), Some(HitTarget::FileUp)));
        assert_eq!(up, Some(1), "directly under the header, scrolled or not");
    }

    /// A list longer than the panel says how much is below it, on a row of
    /// its own.
    ///
    /// The note used to be written over the bottom entry, which produced
    /// rows reading ".zsh_histor+3 more": a filename with a count welded to
    /// its end, where neither could be read.
    #[test]
    fn the_overflow_note_gets_its_own_row_instead_of_a_filename() {
        let s = Scratch::new("files-panel-overflow");
        for n in 0..40 {
            s.file(&format!("f{n:02}.txt"), "");
        }
        let mut tree = FileTree::new();
        tree.reroot(&s.0);

        // Ten rows: header, navigation, then eight of body. The note claims
        // one of those eight, so seven entries show and 33 do not.
        let (buf, _) = draw(&mut tree, None);
        let note = copy::more_below(33);
        let note_y = row_of(&buf, &note);
        assert_eq!(
            note_y,
            PANEL.height - 1,
            "the note is the last row of the panel"
        );
        let note_row = line(&buf, note_y);
        assert!(
            !note_row.contains(".txt"),
            "and shares that row with no entry: {note_row}"
        );
        assert_eq!(
            (0..PANEL.height)
                .filter(|y| line(&buf, *y).contains(".txt"))
                .count(),
            7,
            "seven entries, and the eighth row went to the note"
        );
    }

    /// A name too long for the panel keeps its head, because its tail is
    /// the extension the badge already shows.
    #[test]
    fn a_long_name_keeps_the_head_that_identifies_it() {
        assert_eq!(clip_head("main.rs", 22), "main.rs");
        assert_eq!(clip_head("a_very_long_handler_name.rs", 12), "a_very_long…");
        assert_eq!(clip_head("x", 0), "");
        // The cut never lands inside a wide glyph's own pair of cells, and
        // never inside a UTF-8 sequence.
        assert_eq!(clip_head("視視視", 4), "視…");
        assert_eq!(clip_head("é́xample.rs", 3), "é́x…");
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

    /// A tag too long for the badge says it was cut instead of naming a
    /// type the file is not.
    #[test]
    fn a_long_type_tag_is_elided_rather_than_silently_renamed() {
        // Untouched when it fits.
        assert_eq!(elide_tag("md"), "md");
        assert_eq!(elide_tag("toml"), "toml");
        // `notes.markdown` is not a `mark` file, and `App.svelte` is not a
        // `svel` file. Both used to read that way.
        assert_eq!(elide_tag("markdown"), "mar…");
        assert_eq!(elide_tag("svelte"), "sve…");
        assert_eq!(elide_tag("sqlite3"), "sql…");
        // And the elided badge is exactly as wide as a full one, so the
        // shared field does not move.
        assert_eq!(width_of("(mar…)"), width_of("(toml)"));
    }

    /// Every name starts in the same column even when one row's tag is
    /// twice as wide per character as another's.
    #[test]
    fn a_wide_glyph_tag_does_not_push_its_own_row_out_of_the_column() {
        let s = Scratch::new("files-panel-wide-tag");
        s.file("main.rs", "");
        s.file("notes.日本", "");
        let mut tree = FileTree::new();
        tree.reroot(&s.0);

        let (buf, _) = draw(&mut tree, None);
        assert_eq!(
            col_of(&buf, "main.rs", row_of(&buf, "main.rs")),
            col_of(&buf, "notes.", row_of(&buf, "notes.")),
            "a two-column tag must be padded by columns, not by characters"
        );
    }

    /// The climb-out answers across the row, not just on the two cells its
    /// glyphs occupy. It reads as a row, so it has to behave as one, and a
    /// four-column target left the middle of the row dead to both clicks
    /// and the wheel.
    #[test]
    fn the_climb_out_answers_across_the_whole_row() {
        let s = Scratch::new("files-panel-up-reach");
        s.file("src/main.rs", "");
        let mut tree = FileTree::new();
        tree.reroot(&s.0);

        let (_, hits) = draw(&mut tree, None);
        let up: Vec<u16> = (0..PANEL.width)
            .filter(|x| matches!(hits.hit(*x, 1), Some(HitTarget::FileUp)))
            .collect();
        assert!(
            up.len() > 10,
            "the climb-out should own most of its row, got {} columns",
            up.len()
        );
        // But not the arrows' own cells: those are different controls.
        let arrows: Vec<u16> = (0..PANEL.width)
            .filter(|x| {
                matches!(
                    hits.hit(*x, 1),
                    Some(HitTarget::FileBack | HitTarget::FileForward)
                )
            })
            .collect();
        assert!(
            arrows.iter().all(|x| !up.contains(x)),
            "the retrace arrows keep their own cells"
        );
    }

    /// Pointing at the panel has to reach the renderer at all.
    ///
    /// The panel paints a hover state for its rows, but bare motion is
    /// filtered out before `app.hover` is ever written unless the target
    /// under the pointer is on one list. Every row here was missing from
    /// it, so the highlight this panel paints could never fire in the
    /// running workspace. The unit test above passes hover in directly and
    /// cannot see that; this one checks the coupling.
    #[test]
    fn motion_over_the_panel_is_not_filtered_out_before_it_lights_anything() {
        use crate::input::mouse::motion_touches_hover_button;

        let s = Scratch::new("files-panel-motion");
        s.file("src/main.rs", "");
        let mut tree = FileTree::new();
        tree.reroot(&s.0);
        tree.toggle(&s.0.join("src"));
        tree.reroot(s.0.join("src"));
        let (buf, hits) = draw(&mut tree, None);

        for (label, y) in [
            ("the header", 0u16),
            ("the climb-out row", 1),
            ("a file row", row_of(&buf, "main.rs")),
        ] {
            assert!(
                motion_touches_hover_button(&hits, None, 1, y),
                "motion over {label} must reach the renderer, or it never lights"
            );
        }
        // The retrace arrows too, on their own cells.
        let back = (0..PANEL.width)
            .find(|x| matches!(hits.hit(*x, 1), Some(HitTarget::FileBack)))
            .expect("a live back control");
        assert!(motion_touches_hover_button(&hits, None, back, 1));
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

    /// A panel with almost no room paints what it can and never reaches
    /// outside its own rect. The sidebar can be dragged to shapes like
    /// these, and a panic here takes the whole workspace down.
    #[test]
    fn a_panel_with_no_room_paints_nothing_and_panics_on_nothing() {
        let s = Scratch::new("files-panel-tiny");
        s.file("Cargo.toml", "");
        s.file("src/main.rs", "");
        let mut tree = FileTree::new();
        tree.reroot(&s.0);
        for height in 0..5u16 {
            for width in 0..5u16 {
                let _ = draw_sized(&mut tree, None, width.max(1), height.max(1));
            }
        }
    }
}
