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
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub alternate_on: bool,
}

const META_FORMAT: &str =
    "#{cursor_x}\t#{cursor_y}\t#{alternate_on}\t#{pane_width}\t#{pane_height}";

impl ControlClient {
    /// Fetch escaped visible and alternate captures plus cursor/mode metadata
    /// for one pane. One adapter call, multiple tmux commands underneath.
    pub async fn hydrate_pane(&self, pane_id: &str) -> Result<HydrationBundle, TmuxError> {
        let visible = self.capture_pane_escaped(pane_id).await?;
        let alternate = self.capture_pane_alternate_escaped(pane_id).await.ok();
        let meta = self.display(pane_id, META_FORMAT).await?;
        let (cursor_x, cursor_y, alternate_on, cols, rows) = parse_meta(&meta)?;
        Ok(HydrationBundle {
            cols,
            rows,
            visible_escaped: visible.into_bytes(),
            alternate_escaped: alternate.map(|s| s.into_bytes()),
            cursor_x,
            cursor_y,
            alternate_on,
        })
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

    /// Declare this control client's size to tmux (`refresh-client -C`).
    pub async fn set_client_size(&self, cols: u16, rows: u16) -> Result<(), TmuxError> {
        self.command(&format!("refresh-client -C {cols}x{rows}"))
            .await?;
        Ok(())
    }

    /// Size the attached window by the most recently active client (R5).
    pub async fn set_window_size_latest(&self) -> Result<(), TmuxError> {
        self.command("set-option -w window-size latest").await?;
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

fn parse_meta(line: &str) -> Result<(u16, u16, bool, u16, u16), TmuxError> {
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
    Ok((cursor_x, cursor_y, alternate_on, cols, rows))
}

#[cfg(test)]
mod tests {
    use super::{join_all_ordered, parse_meta};

    #[test]
    fn meta_format_parses() {
        let (x, y, alt, w, h) = parse_meta("3\t5\t0\t120\t30").unwrap();
        assert_eq!((x, y, alt, w, h), (3, 5, false, 120, 30));
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
