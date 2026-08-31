//! Frame builder: pure App state in, rows of text out. No terminal here,
//! so every layout behavior pins down as an exact-string test.
//!
//! The grid: header, one breathing line, the attention band when it has
//! something to say, the stream window, footer. The stream renders only
//! the visible window (entries are counted before any string is built),
//! which is what keeps a 10k-entry ring under the 16ms frame budget. Width
//! overflow is clipped by the terminal (autowrap off), never wrapped:
//! arrivals can never reflow lines already on screen.
//!
//! Height is the only dimension the builder needs, which is why it is the
//! only one it takes: rows are emitted at full length and the terminal
//! cuts them at its own edge.
//!
//! Nothing here decides what needs a human. The header asks the register
//! (`cyclops_proto::attention`) for its own words, and the band asks the
//! app which of those the current view cannot reach.

use crate::app::{App, Density, RowTarget, View, Which};
use crate::grid::display_width;
use crate::stream::{Entry, Filter};

/// The narrowest terminal that gets the sidebar. The stream's widest
/// routine line is 59 columns (docs/guides/workspaces.md walks the arithmetic),
/// the sidebar caps at [`SIDEBAR_MAX_W`], and the separator is 3, so at
/// 96 both halves keep the words they exist to carry.
const SIDEBAR_MIN_TERM_W: usize = 96;
/// Sidebar content width bounds. The floor keeps the panel readable when
/// every name is short; the cap keeps a long pane title from eating the
/// stream.
const SIDEBAR_MIN_W: usize = 18;
const SIDEBAR_MAX_W: usize = 30;

/// Rows for one frame, exactly `height` of them.
///
/// Also rewrites `app.row_targets`, one entry per row: the frame is the
/// only thing that knows what ended up where, so it is the only thing
/// that can tell a click what it landed on.
pub fn build(app: &mut App, width: usize, height: usize) -> Vec<String> {
    // Remembered so the key handler can ask whether this frame had room
    // to show what a destructive key targets.
    app.last_size = (width, height);
    // The work queue is its own list with its own model. It shares the
    // frame budget and nothing else, so it renders here and returns
    // before the stream's walk begins.
    if app.view == crate::app::View::Messages {
        let status = messages_status(app);
        let rows = if app.overlay {
            messages_help(width, height, status.as_deref())
        } else {
            // A detail takes the whole frame. One surface at a time: a
            // list behind a confirmation is a list somebody can act on
            // by mistake.
            match &app.detail {
                Some(detail) => {
                    crate::detail::render_with_status(detail, width, height, status.as_deref())
                }
                None => {
                    crate::queue::render_with_status(&app.queue, width, height, status.as_deref())
                }
            }
        };
        // Messages is keyboard-only. Clicks have no target to resolve.
        app.row_targets = vec![(RowTarget::Nothing, RowTarget::Nothing); height];
        app.sidebar_w = 0;
        return rows;
    }
    let mut rows = Vec::with_capacity(height);
    let mut targets = vec![(RowTarget::Nothing, RowTarget::Nothing); height];
    let mut sidebar_w = 0usize;
    if height < 5 {
        // A sliver of a terminal: header plus whatever fits.
        rows.push(header(app));
        while rows.len() < height {
            rows.push(String::new());
        }
        app.row_targets = targets;
        app.sidebar_w = 0;
        return rows;
    }
    // One walk of the view per frame: the header, the band, and the
    // window all read the same list.
    let top;
    {
        let list = app.visible();
        rows.push(header(app));
        rows.push(String::new());
        let band = attention_band(app, &list);
        let region_h = height - 3;
        let stream_h = region_h - usize::from(band.is_some());
        let band_rows = usize::from(band.is_some());
        let mut region: Vec<String> = Vec::with_capacity(region_h);
        region.extend(band);
        let mut uids: Vec<Option<u64>> = vec![None; band_rows];
        if app.overlay {
            region.extend(cheatsheet(stream_h));
            uids.resize(region_h, None);
            top = None;
        } else {
            let (stream, new_top, stream_uids) = stream_rows(app, &list, stream_h);
            region.extend(stream);
            uids.extend(stream_uids);
            top = new_top;
        }
        uids.resize(region_h, None);
        // The sidebar, beside the whole region. Terminal rows are single
        // strings the terminal clips at its own edge, so the sidebar is
        // the padded head of each row and the stream is the tail; only
        // the head needs measuring.
        match sidebar(app, width, region_h) {
            Some((cells, side_w)) => {
                sidebar_w = side_w;
                for (i, row) in region.into_iter().enumerate() {
                    let (cell, side_target) = cells
                        .get(i)
                        .cloned()
                        .unwrap_or((String::new(), RowTarget::Nothing));
                    let stream_target = uids[i].map_or(RowTarget::Nothing, RowTarget::Entry);
                    targets[2 + i] = (side_target, stream_target);
                    rows.push(format!(
                        "{}{} {}",
                        pad_display(&cell, side_w),
                        app.theme.dim("│"),
                        row
                    ));
                }
            }
            None => {
                for (i, row) in region.into_iter().enumerate() {
                    let stream_target = uids[i].map_or(RowTarget::Nothing, RowTarget::Entry);
                    targets[2 + i] = (RowTarget::Nothing, stream_target);
                    rows.push(row);
                }
            }
        }
    }
    app.top = top;
    rows.push(footer(app));
    app.row_targets = targets;
    app.sidebar_w = sidebar_w;
    rows
}

fn messages_status(app: &App) -> Option<String> {
    let notice = app.notice_text();
    match app.refresh.link() {
        crate::messages::Link::Connecting => Some(match &notice {
            Some(notice) => format!("connecting to cyclops; actions unavailable; {notice}"),
            None => "connecting to cyclops; actions unavailable".into(),
        }),
        crate::messages::Link::Lost => Some(match &notice {
            Some(notice) => {
                format!("connection lost; R reconnect; {notice}; shown data may be stale")
            }
            None => "connection lost; R reconnect; shown data may be stale".into(),
        }),
        crate::messages::Link::Connected if !app.refresh.may_mutate() => Some(match &notice {
            Some(notice) => format!(
                "refreshing messages; actions unavailable; {notice}; shown data may be stale"
            ),
            None => "refreshing messages; actions unavailable; shown data may be stale".into(),
        }),
        crate::messages::Link::Connected => notice,
    }
}

pub fn messages_help(width: usize, height: usize, status: Option<&str>) -> Vec<String> {
    let lines = [
        "Messages keys",
        "",
        "  tab    next view",
        "  s      next scope",
        "  enter  open selected message",
        "  up/down or j/k  move selection",
        "  R      reconnect after connection loss",
        "  ?      close this help",
        "  q      quit",
    ];
    let content_height = height.saturating_sub(usize::from(status.is_some()));
    let mut rows: Vec<String> = lines
        .iter()
        .take(content_height)
        .map(|line| crate::queue::fit(line, width))
        .collect();
    while rows.len() < content_height {
        rows.push(crate::queue::fit("", width));
    }
    if let Some(status) = status {
        rows.push(crate::queue::fit(status, width));
    }
    rows
}

/// The agent panel: every watched pane, its state, and how long it has
/// stood there. `None` when it should not be drawn: hidden by `a`, no
/// roster to show, or a terminal too narrow to split.
///
/// Each agent is two lines and a blank: who (with the CLI the daemon
/// detects), then where they stand and since when. Both content lines
/// answer a click with the pane, because the panel's whole job is being
/// the fastest route to the pane that needs you.
#[allow(clippy::type_complexity)]
fn sidebar(app: &App, width: usize, region_h: usize) -> Option<(Vec<(String, RowTarget)>, usize)> {
    if !app.show_roster || app.roster_len() == 0 || width < SIDEBAR_MIN_TERM_W {
        return None;
    }
    let theme = &app.theme;
    let mut cells: Vec<(String, RowTarget)> = Vec::new();
    cells.push((theme.dim("agents"), RowTarget::Nothing));
    cells.push((String::new(), RowTarget::Nothing));
    let total = app.roster_len();
    for (shown, row) in app.roster().enumerate() {
        // Three lines to close this agent plus one for "+N more" if the
        // rest will not fit: stop while the message is still honest.
        let remaining = region_h.saturating_sub(cells.len());
        if remaining < 3 && shown < total {
            let more = total - shown;
            cells.pop_if_empty();
            cells.push((theme.dim(&format!("+{more} more")), RowTarget::Nothing));
            break;
        }
        let target = RowTarget::Agent(row.pane_id.clone());
        let who = match &row.manifest {
            Some(m) => format!(
                "{} {} {}",
                theme.role(&row.name, &row.name),
                theme.dim("·"),
                theme.dim(m)
            ),
            None => theme.role(&row.name, &row.name).to_string(),
        };
        cells.push((who, target.clone()));
        let mut standing = format!("  {}", crate::grid::state_cell(row.state, theme));
        if let Some(ms) = row.elapsed_ms() {
            standing.push_str(&format!(
                " {} {}",
                theme.dim("·"),
                theme.dim(&cyclops_proto::duration_words(ms))
            ));
        }
        cells.push((standing, target));
        cells.push((String::new(), RowTarget::Nothing));
    }
    cells.pop_if_empty();
    cells.truncate(region_h);
    let side_w = cells
        .iter()
        .map(|(s, _)| display_width(s))
        .max()
        .unwrap_or(0)
        .clamp(SIDEBAR_MIN_W, SIDEBAR_MAX_W);
    Some((cells, side_w))
}

/// Pad to `w` display columns. A cell already wider is left whole:
/// cutting it means cutting inside an escape sequence, which corrupts
/// the row, so a rare over-wide cell pushes its own row's separator
/// right instead.
fn pad_display(s: &str, w: usize) -> String {
    let have = display_width(s);
    let mut out = s.to_string();
    for _ in have..w {
        out.push(' ');
    }
    out
}

/// Drop a trailing spacer so a panel never ends on a blank.
trait PopIfEmpty {
    fn pop_if_empty(&mut self);
}
impl PopIfEmpty for Vec<(String, RowTarget)> {
    fn pop_if_empty(&mut self) {
        if self.last().is_some_and(|(s, _)| s.is_empty()) {
            self.pop();
        }
    }
}

/// The eye leads: glyph, count beside it when open, then the view and its
/// filters, then what needs a human.
///
/// The eye cell and the attention words are composed by the register, not
/// here: `cyclops status` wears the same two, and mirroring them is how
/// the two surfaces drifted apart with no test able to see it.
fn header(app: &App) -> String {
    let theme = &app.theme;
    // The drawn glyph, not the register's: the eye ticks through one
    // intermediate frame per change and the count still answers to the
    // register while it does.
    let eye = app.attention().header_drawn(app.eye());
    let mut parts = vec![format!(
        "{} {}",
        theme.eye(eye.calm, &eye.cell),
        theme.bold("cyclops")
    )];
    parts.push(app.view.words().to_string());
    if let Some(words) = app.filter.words() {
        parts.push(words);
    }
    if let Some(tail) = eye.tail {
        parts.push(tail);
    }
    if app.admin_unread() > 0 {
        parts.push(format!("inbox {}", app.admin_unread()));
    }
    if app.conn_lost {
        parts.push("connection lost".into());
    }
    parts.join(&format!(" {} ", theme.dim("·")))
}

/// The line that keeps the header honest: counted items with no line in
/// the current view, named where the reader is looking.
///
/// It rides every frame, in both views, at either density, filtered or
/// not. The previous shape answered only when the window was empty, so a
/// filtered count over a busy stream had nothing behind it at all.
///
/// Two things can strand an item, and the copy says which: a filter is
/// hiding its line, or the line aged out of the ring. Everything else got
/// a line from the startup reconciliation.
fn attention_band(app: &App, list: &[&Entry]) -> Option<String> {
    let stranded = app.unreachable(list);
    if stranded.is_empty() {
        return None;
    }
    // The item wears the same cell its own line would, in the same color:
    // the band stands in for that line, so a reader must not have to
    // decode two encodings for one item.
    let named = match stranded.len() {
        1 => crate::grid::attention_phrase(&stranded[0], &app.theme),
        n => format!(
            "{} and {} more",
            crate::grid::attention_phrase(&stranded[0], &app.theme),
            n - 1
        ),
    };
    Some(if app.filter.is_empty() {
        format!("  {named}. Older than the loaded stream · cyclops history has the record.")
    } else {
        format!("  {named}. {}", clear_the_filters(&app.filter))
    })
}

/// How to get a hidden line back: the filters actually in play, the key
/// that opens each, and the one thing that clears one.
///
/// Both halves were wrong before. The copy named `w` whatever was set, so
/// a reader filtering with `--from` was sent to the key that opens `with`
/// and left theirs exactly where it was; and the input line opens
/// pre-filled with the current value (app.rs `open_input`), so enter alone
/// re-applies it rather than clearing it.
fn clear_the_filters(filter: &Filter) -> String {
    let set: Vec<Which> = Which::ALL
        .into_iter()
        .filter(|w| w.value(filter).is_some())
        .collect();
    let words = join_and(set.iter().map(|w| w.word().to_string()));
    let keys = join_and(set.iter().map(|w| w.key().to_string()));
    if set.len() == 1 {
        format!("Hidden by the {words} filter · press {keys}, empty the line, then enter.")
    } else {
        format!("Hidden by the {words} filters · press {keys}, empty each line, then enter.")
    }
}

/// "with", "from and to". Two is the most the filters can reach: `with`
/// replaces the other two and they replace it (app.rs `handle_input_key`).
fn join_and(parts: impl Iterator<Item = String>) -> String {
    parts.collect::<Vec<_>>().join(" and ")
}

fn footer(app: &App) -> String {
    let theme = &app.theme;
    if let Some(input) = &app.input {
        return format!(
            "{}: {}{}",
            input.which.word(),
            input.buf,
            theme.dim("  · enter applies · esc cancels")
        );
    }
    if let Some(notice) = app.notice_text() {
        return format!("  {notice}");
    }
    theme.dim("? keys · tab view · a agents · c density · w/f/t filter · enter focus · q quit")
}

fn cheatsheet(height: usize) -> Vec<String> {
    let lines = [
        "  keys",
        "",
        "  tab    admin stream / firehose / messages",
        "  a      agents panel · on wide terminals",
        "  w f t  filter with / from / to · enter applies · esc cancels",
        "  enter  focus the entry's pane",
        "  ↑ ↓    scroll · unpins from the tail",
        "  end    back to the tail",
        "  c      density · comfortable / compact",
        "  ?      close this help",
        "  q      quit",
        "",
        "  mouse  click an agent to focus its pane · click an entry to",
        "         select it · wheel scrolls",
    ];
    let mut rows: Vec<String> = lines.iter().take(height).map(|l| l.to_string()).collect();
    while rows.len() < height {
        rows.push(String::new());
    }
    rows
}

/// The line under an empty stream window.
///
/// Only the calm case reaches the copy: an empty window can reach no item
/// at all, so anything counted is stranded and the band above has already
/// named it. Gating this copy on the window alone is what once let the
/// body say "All calm" under a header saying one needs attention.
fn empty_state(app: &App) -> String {
    if app.attention_count() > 0 || app.admin_unread() > 0 {
        return String::new();
    }
    match app.view {
        View::Admin => "  All calm. tab shows the firehose.".to_string(),
        View::Firehose => "  Nothing yet. Events land here as they arrive.".to_string(),
        View::Messages => "  No messages.".to_string(),
    }
}

/// Lines one entry occupies at the current density, spacer included when a
/// newer entry follows. Counted without building strings: the window is
/// chosen arithmetically, then only visible entries are formatted.
fn block_len(comfortable: bool, spacer_below: bool) -> usize {
    1 + usize::from(comfortable && spacer_below)
}

/// The stream window, exactly `sh` rows, plus the top anchor to store.
/// Pinned: the tail fills bottom-up, no anchor. Unpinned: the stored top
/// anchor holds still, so arrivals append below the viewport instead of
/// shifting it; the selection is kept visible.
fn stream_rows(
    app: &App,
    list: &[&Entry],
    sh: usize,
) -> (Vec<String>, Option<u64>, Vec<Option<u64>>) {
    let comfortable = app.density == Density::Comfortable;
    if list.is_empty() {
        let mut rows = vec![empty_state(app)];
        while rows.len() < sh {
            rows.push(String::new());
        }
        let uids = vec![None; rows.len()];
        return (rows, None, uids);
    }

    // Choose the window: (start index, lines to cut from the top block).
    let (start, cut_top) = if app.pinned {
        window_ending_at(list.len() - 1, sh, comfortable)
    } else {
        let sel_pos = app
            .selected
            .and_then(|uid| list.iter().position(|e| e.uid == uid))
            .unwrap_or(list.len() - 1);
        let anchor = app
            .top
            .and_then(|uid| list.iter().position(|e| e.uid == uid));
        match anchor {
            // Anchor gone (evicted or filtered out): selection sits at the
            // window bottom.
            None => window_ending_at(sel_pos, sh, comfortable),
            Some(top) if sel_pos < top => (sel_pos, 0),
            Some(top) => {
                // Selection below the window? Slide down just enough.
                if fits(list, top, sel_pos, sh, comfortable) {
                    (top, 0)
                } else {
                    window_ending_at(sel_pos, sh, comfortable)
                }
            }
        }
    };
    let new_top = if app.pinned {
        None
    } else {
        Some(list[start].uid)
    };

    let theme = &app.theme;
    let mut rows: Vec<String> = Vec::with_capacity(sh + 2);
    // Which entry each visual row belongs to, so a click can name it.
    // Spacer rows belong to nobody; a click on breathing room does
    // nothing, which is what breathing room is for.
    let mut uids: Vec<Option<u64>> = Vec::with_capacity(sh + 2);
    for (i, e) in list.iter().enumerate().skip(start) {
        let marker = if app.selected == Some(e.uid) {
            format!("{} ", theme.accent("▸"))
        } else {
            "  ".to_string()
        };
        for (n, line) in e.lines(theme).into_iter().enumerate() {
            if n == 0 {
                rows.push(format!("{marker}{line}"));
            } else {
                rows.push(format!("  {line}"));
            }
            uids.push(Some(e.uid));
        }
        if comfortable && i + 1 < list.len() {
            rows.push(String::new());
            uids.push(None);
        }
        if rows.len() >= cut_top + sh {
            break;
        }
    }
    let mut rows: Vec<String> = rows.into_iter().skip(cut_top).take(sh).collect();
    let mut uids: Vec<Option<u64>> = uids.into_iter().skip(cut_top).take(sh).collect();
    while rows.len() < sh {
        rows.push(String::new());
        uids.push(None);
    }
    (rows, new_top, uids)
}

/// Walk backward from `end` until `sh` lines are covered. Returns the
/// first entry index and how many of its lines scroll off the top.
fn window_ending_at(end: usize, sh: usize, comfortable: bool) -> (usize, usize) {
    let mut needed = sh;
    let mut idx = end + 1;
    while idx > 0 && needed > 0 {
        idx -= 1;
        let n = block_len(comfortable, idx < end);
        if n >= needed {
            return (idx, n - needed);
        }
        needed -= n;
    }
    (0, 0)
}

/// Do the blocks from `top` through `sel` fit in `sh` lines?
fn fits(list: &[&Entry], top: usize, sel: usize, sh: usize, comfortable: bool) -> bool {
    let mut total = 0;
    for (idx, _) in list.iter().enumerate().take(sel + 1).skip(top) {
        total += block_len(comfortable, idx < sel);
        if total > sh {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, View};
    use crate::key::Key;
    use crate::stream::{EndpointFilter, Entry, EntryKind, Filter};
    use crate::theme::Theme;
    use cyclops_proto::{AgentState, DeliveryState};

    fn endpoint(label: &str) -> EndpointFilter {
        EndpointFilter::new(
            "admin:00000000-0000-0000-0000-000000000001"
                .parse()
                .unwrap(),
            label,
        )
    }

    fn msg(ts: u64, from: &str, to: &[&str], subject: &str) -> Entry {
        Entry {
            uid: 0,
            ts,
            seq: None,
            id: Some("m-1".into()),
            kind: EntryKind::Msg {
                from: from.into(),
                to: to.iter().map(|t| t.to_string()).collect(),
                endpoints: None,
                subject: subject.into(),
                fyi: false,
            },
        }
    }

    fn fixture() -> App {
        let mut a = App::new(Theme::none(), View::Firehose, Filter::default());
        a.live(msg(
            43_471_000,
            "codex",
            &["reviewer"],
            "Review the rate limiter",
        ));
        a.live(Entry {
            uid: 0,
            ts: 43_472_000,
            seq: None,
            id: Some("m-1".into()),
            kind: EntryKind::Delivery {
                to: "reviewer".into(),
                recipient: None,
                state: DeliveryState::DeliveredVerified,
                cause: None,
            },
        });
        a.live(Entry {
            uid: 0,
            ts: 43_480_000,
            seq: None,
            id: Some("e-1".into()),
            kind: EntryKind::State {
                target: "reviewer".into(),
                recipient: None,
                session_idx: 0,
                pane_id: Some("%1".into()),
                state: AgentState::BlockedPermission,
            },
        });
        a
    }

    #[test]
    fn frame_is_exact_comfortable() {
        let mut a = fixture();
        // Settle the eye: one blocked agent, target opening.
        while a.tick_eye() {}
        a.tick_eye();
        let got = build(&mut a, 80, 12);
        let expected = vec![
            "◑ 1 cyclops · firehose · 1 needs attention".to_string(),
            String::new(),
            "  12:04:31  codex → reviewer  Review the rate limiter".to_string(),
            String::new(),
            "  12:04:32  reviewer  ✔ delivered · verified".to_string(),
            String::new(),
            "  12:04:40  reviewer  ⚠ blocked_permission".to_string(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            "? keys · tab view · a agents · c density · w/f/t filter · enter focus · q quit"
                .to_string(),
        ];
        assert_eq!(got, expected);
        for line in &got {
            assert_eq!(line, line.trim_end(), "trailing space in {line:?}");
        }
    }

    /// The calm frame carries the alarm AND the line that ended it.
    ///
    /// It used to carry the alarm alone: `⚠ blocked_permission` under a
    /// closed eye, with the idle line that answered it in the firehose
    /// only. The record does not retract, so the alarm row stays; the
    /// clearance is the second line, written where the register said the
    /// item stopped needing a human (`cyclops_proto::attention`, rule 3).
    #[test]
    fn frame_is_exact_compact_and_calm() {
        let mut a = fixture();
        a.handle_key(Key::Char('c'));
        // No blocked agents once reviewer goes idle: the eye closes.
        a.live(Entry {
            uid: 0,
            ts: 43_481_000,
            seq: None,
            id: Some("e-2".into()),
            kind: EntryKind::State {
                target: "reviewer".into(),
                recipient: None,
                session_idx: 0,
                pane_id: Some("%1".into()),
                state: AgentState::Idle,
            },
        });
        while a.tick_eye() {}
        a.tick_eye();
        let got = build(&mut a, 80, 9);
        let expected = vec![
            "‿ cyclops · firehose".to_string(),
            String::new(),
            "  12:04:31  codex → reviewer  Review the rate limiter".to_string(),
            "  12:04:32  reviewer  ✔ delivered · verified".to_string(),
            "  12:04:40  reviewer  ⚠ blocked_permission".to_string(),
            "  12:04:41  reviewer  ○ idle".to_string(),
            "  12:04:41  reviewer  ✔ cleared · was ⚠ blocked_permission".to_string(),
            String::new(),
            "? keys · tab view · a agents · c density · w/f/t filter · enter focus · q quit"
                .to_string(),
        ];
        assert_eq!(got, expected);
    }

    #[test]
    fn pinned_tail_shows_the_newest_when_overflowing() {
        let mut a = App::new(Theme::none(), View::Firehose, Filter::default());
        a.handle_key(Key::Char('c')); // compact: one line per entry
        for i in 0..50 {
            a.live(msg(1000 * i, "codex", &["reviewer"], &format!("m{i}")));
        }
        let got = build(&mut a, 80, 8);
        // Stream area is 5 rows: the last five messages, tail at bottom.
        assert!(got[2].ends_with("m45"), "{:?}", got[2]);
        assert!(got[6].ends_with("m49"), "{:?}", got[6]);
    }

    #[test]
    fn unpinned_viewport_holds_still_as_entries_arrive() {
        let mut a = App::new(Theme::none(), View::Firehose, Filter::default());
        a.handle_key(Key::Char('c'));
        for i in 0..20 {
            a.live(msg(1000 * i, "codex", &["reviewer"], &format!("m{i}")));
        }
        // Scroll up once: selection m18, anchor stored on first build.
        a.handle_key(Key::Up);
        let before = build(&mut a, 80, 8);
        // New arrivals must not move the viewport.
        for i in 20..30 {
            a.live(msg(1000 * i, "codex", &["reviewer"], &format!("m{i}")));
        }
        let after = build(&mut a, 80, 8);
        assert_eq!(before[2..7], after[2..7], "viewport moved on arrival");
        // The selected line carries the marker.
        assert!(after.iter().any(|l| l.starts_with("▸ ")), "{after:?}");
        // End repins to the tail.
        a.handle_key(Key::End);
        let tail = build(&mut a, 80, 8);
        assert!(tail[6].ends_with("m29"), "{:?}", tail[6]);
    }

    #[test]
    fn overlay_and_input_lines_render() {
        let mut a = fixture();
        a.handle_key(Key::Char('?'));
        let got = build(&mut a, 80, 12);
        assert_eq!(got[2], "  keys");
        assert!(got[4].starts_with("  tab"));
        a.handle_key(Key::Char('?'));
        a.handle_key(Key::Char('w'));
        for c in "rev".chars() {
            a.handle_key(Key::Char(c));
        }
        let got = build(&mut a, 80, 12);
        assert_eq!(
            got.last().unwrap(),
            "with: rev  · enter applies · esc cancels"
        );
    }

    #[test]
    fn empty_states_invite_the_next_action() {
        let mut a = App::new(Theme::none(), View::Admin, Filter::default());
        let got = build(&mut a, 80, 8);
        assert_eq!(got[2], "  All calm. tab shows the firehose.");
        a.view = View::Firehose;
        let got = build(&mut a, 80, 8);
        assert_eq!(got[2], "  Nothing yet. Events land here as they arrive.");
    }

    #[test]
    fn unread_admin_messages_are_visible_without_stream_rows() {
        let mut app = App::new(Theme::none(), View::Admin, Filter::default());
        app.seed_status(crate::stream::StatusSeed {
            admin_unread: 2,
            ..Default::default()
        });
        let rows = build(&mut app, 80, 8);
        assert!(rows[0].contains("inbox 2"), "{:?}", rows[0]);
        assert!(!rows.iter().any(|row| row.contains("All calm")));
    }

    /// The band exists to get a hidden line back, so it has to name the
    /// key that opens the filter actually in play and the thing that
    /// clears it. It named `w` whatever was set, which for a reader
    /// filtering `from` or `to` opens a filter they never set; and the
    /// input line opens pre-filled, so enter alone re-applies the value
    /// instead of clearing it.
    #[test]
    fn the_band_names_the_key_that_clears_the_filter_in_play() {
        let one = |set: fn(&mut Filter)| {
            let mut a = fixture();
            set(&mut a.filter);
            build(&mut a, 80, 12)[2].clone()
        };
        assert_eq!(
            one(|f| f.with = Some(endpoint("nobody"))),
            "  reviewer  ⚠ blocked_permission. Hidden by the with filter · \
             press w, empty the line, then enter."
        );
        assert_eq!(
            one(|f| f.from = Some(endpoint("nobody"))),
            "  reviewer  ⚠ blocked_permission. Hidden by the from filter · \
             press f, empty the line, then enter."
        );
        assert_eq!(
            one(|f| f.to = Some(endpoint("nobody"))),
            "  reviewer  ⚠ blocked_permission. Hidden by the to filter · \
             press t, empty the line, then enter."
        );
        // from and to hold together, so both keys get named.
        assert_eq!(
            one(|f| {
                f.from = Some(endpoint("nobody"));
                f.to = Some(endpoint("nobody"));
            }),
            "  reviewer  ⚠ blocked_permission. Hidden by the from and to filters · \
             press f and t, empty each line, then enter."
        );
        // No filter at all is the other reason a line can be missing.
        let mut a = fixture();
        a.filter.with = Some(endpoint("nobody"));
        assert!(build(&mut a, 80, 12)[2].contains("Hidden by the"));
    }

    /// The band's keys and the keys that open the inputs are one table.
    /// Naming a key the reader can press is the band's whole job.
    #[test]
    fn the_band_names_the_key_that_opens_the_filter_it_names() {
        for which in Which::ALL {
            let mut a = fixture();
            a.handle_key(Key::Char(which.key()));
            assert_eq!(
                a.input.as_ref().map(|i| i.which),
                Some(which),
                "{} is not what {} opens",
                which.word(),
                which.key()
            );
        }
    }

    #[test]
    fn header_names_the_filters_and_connection_state() {
        let mut a = fixture();
        a.filter.with = Some(endpoint("reviewer"));
        a.conn_lost = true;
        while a.tick_eye() {}
        a.tick_eye();
        let got = build(&mut a, 80, 12);
        assert_eq!(
            got[0],
            "◑ 1 cyclops · firehose · with reviewer · 1 needs attention · connection lost"
        );
    }
}
