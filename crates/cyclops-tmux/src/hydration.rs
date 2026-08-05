//! Pane hydration bundles for workspace VT runtimes.
//!
//! A bundle is a visual snapshot plus cursor/mode metadata. It never claims
//! parser-exact terminal state — captures initialize the grid; subsequent
//! `%output` bytes continue from there, and rehydration is the recovery path
//! on pause or reconnect.

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
    use super::parse_meta;

    #[test]
    fn meta_format_parses() {
        let (x, y, alt, w, h) = parse_meta("3\t5\t0\t120\t30").unwrap();
        assert_eq!((x, y, alt, w, h), (3, 5, false, 120, 30));
    }
}
