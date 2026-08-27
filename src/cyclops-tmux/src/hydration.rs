//! Pane hydration bundles for workspace VT runtimes.
//!
//! A bundle is a visual snapshot plus cursor/mode metadata. It never claims
//! parser-exact terminal state — captures initialize the grid; subsequent
//! `%output` bytes continue from there, and rehydration is the recovery path
//! on pause or reconnect.

use std::future::Future;
use std::pin::Pin;
use std::task::Poll;

use crate::control::ControlClient;
use crate::error::TmuxError;
use crate::quote::quote_arg;

/// Everything a pane runtime needs to hydrate from tmux in one round trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HydrationBundle {
    pub cols: u16,
    pub rows: u16,
    /// Escaped visible capture (`capture-pane -e`).
    pub visible_escaped: Vec<u8>,
    /// Escaped alternate-screen capture (`capture-pane -e -a`), when present.
    pub alternate_escaped: Option<Vec<u8>>,
    /// Plain scrollback above the visible screen, wrap-joined, oldest
    /// first. Empty unless the caller asked
    /// ([`ControlClient::hydrate_pane_with_history`]): a runtime being
    /// re-hydrated carries its own scrollback across, and only a runtime
    /// seeing the pane for the first time needs tmux's, so the extra
    /// capture is not paid on the resize path.
    pub history: Vec<u8>,
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub alternate_on: bool,
    /// Whether the pane has any mouse-tracking DECSET on (1000, 1002, or
    /// 1003) — tmux's `#{mouse_any_flag}`.
    pub mouse_on: bool,
    /// Whether the pane has SGR mouse encoding (DECSET 1006) on — tmux's
    /// `#{mouse_sgr_flag}`.
    pub mouse_sgr: bool,
    /// Lines of scrollback the pane holds (`#{history_size}`). What gates
    /// the history capture in [`ControlClient::hydrate_pane_with_history`].
    pub history_size: u32,
}

const META_FORMAT: &str = "#{cursor_x}\t#{cursor_y}\t#{alternate_on}\t#{pane_width}\t#{pane_height}\t#{mouse_any_flag}\t#{mouse_sgr_flag}\t#{history_size}";

impl ControlClient {
    /// Fetch escaped visible and alternate captures plus cursor/mode metadata
    /// for one pane. One adapter call, multiple tmux commands underneath.
    pub async fn hydrate_pane(&self, pane_id: &str) -> Result<HydrationBundle, TmuxError> {
        let visible = self.capture_pane_escaped(pane_id).await?;
        let alternate = self.capture_pane_alternate_escaped(pane_id).await.ok();
        let meta = self.display(pane_id, META_FORMAT).await?;
        let (cursor_x, cursor_y, alternate_on, cols, rows, mouse_on, mouse_sgr, history_size) =
            parse_meta(&meta)?;
        Ok(HydrationBundle {
            cols,
            rows,
            visible_escaped: visible.into_bytes(),
            alternate_escaped: alternate.map(|s| s.into_bytes()),
            history: Vec::new(),
            cursor_x,
            cursor_y,
            alternate_on,
            mouse_on,
            mouse_sgr,
            history_size,
        })
    }

    /// [`Self::hydrate_pane`] plus up to `max_lines` of the pane's
    /// scrollback. For a runtime meeting its pane for the first time: tmux
    /// has the transcript from before this client attached, and without it
    /// the wheel hits a wall at the attach moment — the pane scrolls back
    /// a few lines and stops dead, which reads as broken scrolling rather
    /// than missing history.
    ///
    /// The capture is skipped, not merely empty, in the two cases where it
    /// would lie. A pane with no scrollback at all: tmux clamps both
    /// bounds of `-S -N -E -1` against the history size, and at zero they
    /// collapse onto the FIRST VISIBLE ROW, which would seed the screen's
    /// own top line into scrollback and paint it twice. And a pane on the
    /// alternate screen: history belongs to the primary grid, but a plain
    /// capture reads the alternate one, so the "history" would be a row of
    /// the running TUI left stranded above the shell after it exits.
    ///
    /// A failed capture degrades to no history rather than failing the
    /// bundle: hydration must still work when the pane cannot answer.
    pub async fn hydrate_pane_with_history(
        &self,
        pane_id: &str,
        max_lines: u16,
    ) -> Result<HydrationBundle, TmuxError> {
        let mut bundle = self.hydrate_pane(pane_id).await?;
        if bundle.history_size > 0 && !bundle.alternate_on {
            let lines = max_lines.min(u16::try_from(bundle.history_size).unwrap_or(u16::MAX));
            bundle.history = self
                .capture_pane_scrollback(pane_id, lines)
                .await
                .map(String::into_bytes)
                .unwrap_or_default();
        }
        Ok(bundle)
    }

    /// Plain scrollback above the visible screen (`capture-pane -p -J -S
    /// -N -E -1`), wrap-joined so refeeding reflows at the reader's width.
    /// Unlike [`Self::capture_pane_history`] this excludes the visible
    /// grid (`-E -1`): hydration replays the screen separately, and
    /// including it here would put every visible row into scrollback a
    /// second time.
    pub async fn capture_pane_scrollback(
        &self,
        pane_id: &str,
        max_lines: u16,
    ) -> Result<String, TmuxError> {
        let out = self
            .command(&format!(
                "capture-pane -p -J -S -{max_lines} -E -1 -t {}",
                quote_arg(pane_id)
            ))
            .await?;
        Ok(out.join("\n"))
    }

    /// Hydrate every pane in `pane_ids` concurrently, one result per input
    /// id in the same order.
    ///
    /// Each pane's own capture -> capture -> metadata sequence still runs
    /// exactly as [`ControlClient::hydrate_pane`] runs it alone; only
    /// *independent* panes now overlap, pipelined through the same
    /// correlated connection (`control.rs`'s FIFO reply matching keeps every
    /// command's reply correctly paired no matter how commands from
    /// different panes interleave on the wire). One pane's `Err` — a dead or
    /// nonexistent id — never fails the batch; it only fails that pane's
    /// slot.
    ///
    /// This is the concurrency the recommendation asks for
    /// ("Hydrate panes concurrently without weakening ordering"): callers
    /// that today loop `hydrate_pane` per visible pane pay the sum of every
    /// pane's round trips; this pays roughly the slowest one.
    pub async fn hydrate_panes(
        &self,
        pane_ids: &[&str],
    ) -> Vec<Result<HydrationBundle, TmuxError>> {
        let futures = pane_ids.iter().map(|id| self.hydrate_pane(id)).collect();
        join_all_ordered(futures).await
    }

    /// [`Self::hydrate_panes`], with the panes marked in `seed_history`
    /// taking the deeper first-sight bundle
    /// ([`Self::hydrate_pane_with_history`]). One batch, same pipelining,
    /// so a tab mixing new and known panes still pays roughly the slowest
    /// pane rather than the sum. `seed_history` runs parallel to
    /// `pane_ids`; a missing entry means no seeding for that pane.
    pub async fn hydrate_panes_seeding(
        &self,
        pane_ids: &[&str],
        seed_history: &[bool],
        max_lines: u16,
    ) -> Vec<Result<HydrationBundle, TmuxError>> {
        let futures = pane_ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let seed = seed_history.get(i).copied().unwrap_or(false);
                async move {
                    if seed {
                        self.hydrate_pane_with_history(id, max_lines).await
                    } else {
                        self.hydrate_pane(id).await
                    }
                }
            })
            .collect();
        join_all_ordered(futures).await
    }

    /// Declare this control client's size to tmux (`refresh-client -C`).
    ///
    /// This is a vote, not a command: tmux resolves it against every other
    /// size-declared client under the window's `window-size` policy, and a
    /// tty client cannot abstain from voting. The workspace does NOT size
    /// windows this way, because whichever policy is in force some other
    /// client can outvote it and reshape every pane in the session (F76).
    /// Window sizing lives in [`crate::sizing`]; this stays because
    /// declaring a size is a real tmux capability and fixtures need it.
    pub async fn set_client_size(&self, cols: u16, rows: u16) -> Result<(), TmuxError> {
        self.command(&format!("refresh-client -C {cols}x{rows}"))
            .await?;
        Ok(())
    }

    /// Put `window_id` on the `smallest` sizing policy.
    ///
    /// The workspace no longer does this: a policy is a rule for resolving
    /// client votes, and `smallest` resolves them by letting the smallest
    /// viewer win, so one 62x21 terminal collapsed a 176x47 session and
    /// every agent pane in it (F76). Windows a workspace owns go on
    /// `manual` through [`crate::sizing`] instead. This stays as a tmux
    /// capability for fixtures that want a window to track its clients.
    ///
    /// The reasoning that led here, kept because it is still true about
    /// `latest` and explains why the policy was changed twice:
    ///
    /// This replaces the earlier `latest` policy (R5), which held only while
    /// the declaring control client was the lone size authority. A control
    /// client never produces tty input, so under `latest` any regular client
    /// that attaches to the session becomes the authority the moment it is
    /// used, and tmux lays panes out wider than the declared canvas: typed
    /// text runs past the visible pane edge. `smallest` has no authority to
    /// steal. tmux takes the minimum over size-declared clients, so the
    /// window never exceeds this client's declared canvas and still equals
    /// it exactly whenever this client is the only viewer. Control clients
    /// that never declared a size (the daemon's watcher) are ignored under
    /// both policies (F48).
    pub async fn set_window_size_smallest(&self, window_id: &str) -> Result<(), TmuxError> {
        self.command(&format!(
            "set-option -w -t {} window-size smallest",
            quote_arg(window_id)
        ))
        .await?;
        Ok(())
    }

    /// Escaped capture of the alternate screen (`capture-pane -e -a`).
    pub async fn capture_pane_alternate_escaped(&self, pane_id: &str) -> Result<String, TmuxError> {
        let out = self
            .command(&format!("capture-pane -e -p -a -t {}", quote_arg(pane_id)))
            .await?;
        Ok(out.join("\n"))
    }
}

/// Drive a batch of same-typed futures to completion concurrently,
/// preserving input order in the output.
///
/// This crate's only async dependency is tokio, which has no `join_all` for
/// a *dynamic* number of futures: `tokio::join!` is fixed-arity, and
/// `tokio::task::JoinSet` needs `'static` tasks — adopting it would force
/// [`ControlClient::hydrate_panes`] to take `Arc<Self>` instead of `&self`,
/// unlike every other method on this type. `futures::future::join_all` is
/// the obvious tool, but nothing else in this crate needs that dependency.
/// `poll_fn` polls every not-yet-ready future on each wake, which is exactly
/// what `join_all` does; hand-rolling the loop below costs fewer lines than
/// the dependency would for this one call site.
async fn join_all_ordered<F: Future>(futures: Vec<F>) -> Vec<F::Output> {
    let mut slots: Vec<Option<F::Output>> = futures.iter().map(|_| None).collect();
    let mut pending: Vec<Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
    std::future::poll_fn(move |cx| {
        let mut all_ready = true;
        for (slot, fut) in slots.iter_mut().zip(pending.iter_mut()) {
            if slot.is_none() {
                match fut.as_mut().poll(cx) {
                    Poll::Ready(v) => *slot = Some(v),
                    Poll::Pending => all_ready = false,
                }
            }
        }
        if all_ready {
            let done = std::mem::take(&mut slots)
                .into_iter()
                .map(|s| s.expect("every slot filled when all_ready"))
                .collect();
            Poll::Ready(done)
        } else {
            Poll::Pending
        }
    })
    .await
}

/// `(cursor_x, cursor_y, alternate_on, cols, rows, mouse_on, mouse_sgr,
/// history_size)`, named so [`parse_meta`]'s signature reads rather than
/// counting tuple slots.
type ParsedMeta = (u16, u16, bool, u16, u16, bool, bool, u32);

fn parse_meta(line: &str) -> Result<ParsedMeta, TmuxError> {
    let mut fields = line.split('\t');
    let cursor_x = fields
        .next()
        .ok_or_else(|| TmuxError::Protocol("hydration meta: missing cursor_x".into()))?
        .parse::<u16>()
        .map_err(|e| TmuxError::Protocol(format!("hydration meta cursor_x: {e}")))?;
    let cursor_y = fields
        .next()
        .ok_or_else(|| TmuxError::Protocol("hydration meta: missing cursor_y".into()))?
        .parse::<u16>()
        .map_err(|e| TmuxError::Protocol(format!("hydration meta cursor_y: {e}")))?;
    let alternate_on = fields
        .next()
        .ok_or_else(|| TmuxError::Protocol("hydration meta: missing alternate_on".into()))?
        .parse::<u8>()
        .map_err(|e| TmuxError::Protocol(format!("hydration meta alternate_on: {e}")))?
        != 0;
    let cols = fields
        .next()
        .ok_or_else(|| TmuxError::Protocol("hydration meta: missing pane_width".into()))?
        .parse::<u16>()
        .map_err(|e| TmuxError::Protocol(format!("hydration meta pane_width: {e}")))?;
    let rows = fields
        .next()
        .ok_or_else(|| TmuxError::Protocol("hydration meta: missing pane_height".into()))?
        .parse::<u16>()
        .map_err(|e| TmuxError::Protocol(format!("hydration meta pane_height: {e}")))?;
    let mouse_on = fields
        .next()
        .ok_or_else(|| TmuxError::Protocol("hydration meta: missing mouse_any_flag".into()))?
        .parse::<u8>()
        .map_err(|e| TmuxError::Protocol(format!("hydration meta mouse_any_flag: {e}")))?
        != 0;
    let mouse_sgr = fields
        .next()
        .ok_or_else(|| TmuxError::Protocol("hydration meta: missing mouse_sgr_flag".into()))?
        .parse::<u8>()
        .map_err(|e| TmuxError::Protocol(format!("hydration meta mouse_sgr_flag: {e}")))?
        != 0;
    let history_size = fields
        .next()
        .ok_or_else(|| TmuxError::Protocol("hydration meta: missing history_size".into()))?
        .parse::<u32>()
        .map_err(|e| TmuxError::Protocol(format!("hydration meta history_size: {e}")))?;
    Ok((
        cursor_x,
        cursor_y,
        alternate_on,
        cols,
        rows,
        mouse_on,
        mouse_sgr,
        history_size,
    ))
}

#[cfg(test)]
mod tests {
    use super::{join_all_ordered, parse_meta};

    #[test]
    fn meta_format_parses() {
        let (x, y, alt, w, h, mouse_on, mouse_sgr, history) =
            parse_meta("3\t5\t0\t120\t30\t1\t0\t482").unwrap();
        assert_eq!(
            (x, y, alt, w, h, mouse_on, mouse_sgr, history),
            (3, 5, false, 120, 30, true, false, 482)
        );
    }

    #[tokio::test]
    async fn join_all_ordered_preserves_input_order_regardless_of_completion_order() {
        // Each future yields after a different number of scheduler
        // round-trips, so the LAST one to be issued (index 4) is the FIRST
        // to become ready. The output must still land at index 4.
        let futures: Vec<_> = (0..5)
            .map(|i| async move {
                for _ in 0..(4 - i) {
                    tokio::task::yield_now().await;
                }
                i
            })
            .collect();
        let out = join_all_ordered(futures).await;
        assert_eq!(out, vec![0, 1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn join_all_ordered_keeps_each_result_independent() {
        // One "failing" and one "succeeding" future from the same call
        // site, the same shape `hydrate_panes` builds its batch in — every
        // element of the `Vec` passed to `join_all_ordered` is necessarily
        // the same concrete (if anonymous) future type.
        let futures: Vec<_> = (0..2)
            .map(|i| async move {
                if i == 1 {
                    Err("boom")
                } else {
                    Ok::<u32, &str>(1)
                }
            })
            .collect();
        let out = join_all_ordered(futures).await;
        assert_eq!(out, vec![Ok(1), Err("boom")]);
    }
}
