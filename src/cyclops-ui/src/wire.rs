//! Size-bounded NDJSON reads shared by every daemon socket in this crate.

use std::io;
use std::io::Write;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

/// Largest daemon frame the UI accepts, excluding its newline.
///
/// A frame is one complete protocol object. Keeping one shared limit means
/// snapshots, events, status reads, and actions cannot disagree about which
/// daemon output is safe to hold in memory.
pub(crate) const MAX_FRAME_BYTES: usize = 1 << 20;

/// Serialize one outbound JSON frame without ever growing past the same cap.
pub(crate) fn encode_json(value: &impl serde::Serialize) -> Result<Vec<u8>, serde_json::Error> {
    let mut writer = BoundedWriter {
        bytes: Vec::with_capacity(8 * 1024),
        limit: MAX_FRAME_BYTES,
    };
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.bytes)
}

struct BoundedWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > self.limit {
            return Err(frame_too_large(self.limit));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// One framed reader with a hard bound before allocation.
pub(crate) struct FrameReader<R> {
    inner: BufReader<R>,
    frame: Vec<u8>,
    limit: usize,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub(crate) fn new(inner: R) -> Self {
        Self::with_limit(inner, MAX_FRAME_BYTES)
    }

    fn with_limit(inner: R, limit: usize) -> Self {
        Self {
            inner: BufReader::new(inner),
            frame: Vec::with_capacity(limit.min(8 * 1024)),
            limit,
        }
    }

    /// Read one newline-delimited frame.
    ///
    /// Oversized and unterminated frames are errors. Callers close that
    /// connection and expose the resulting stale or gap state instead of
    /// trying to resynchronize after bytes have already been discarded.
    pub(crate) async fn next_frame(&mut self) -> io::Result<Option<Vec<u8>>> {
        self.frame.clear();
        loop {
            let available = self.inner.fill_buf().await?;
            if available.is_empty() {
                return if self.frame.is_empty() {
                    Ok(None)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "daemon frame ended without a newline",
                    ))
                };
            }

            if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
                if self.frame.len().saturating_add(newline) > self.limit {
                    return Err(frame_too_large(self.limit));
                }
                self.frame.extend_from_slice(&available[..newline]);
                self.inner.consume(newline + 1);
                if self.frame.last() == Some(&b'\r') {
                    self.frame.pop();
                }
                return Ok(Some(std::mem::take(&mut self.frame)));
            }

            if self.frame.len().saturating_add(available.len()) > self.limit {
                return Err(frame_too_large(self.limit));
            }
            let consumed = available.len();
            self.frame.extend_from_slice(available);
            self.inner.consume(consumed);
        }
    }
}

fn frame_too_large(limit: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("daemon frame exceeds the {limit}-byte limit"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn reads_complete_frames_without_the_delimiter() {
        let mut reader = FrameReader::with_limit(Cursor::new(b"one\r\ntwo\n"), 8);
        assert_eq!(reader.next_frame().await.unwrap(), Some(b"one".to_vec()));
        assert_eq!(reader.next_frame().await.unwrap(), Some(b"two".to_vec()));
        assert_eq!(reader.next_frame().await.unwrap(), None);
    }

    #[tokio::test]
    async fn rejects_a_complete_oversized_frame() {
        let mut reader = FrameReader::with_limit(Cursor::new(b"12345\n"), 4);
        let error = reader.next_frame().await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn rejects_an_unterminated_frame() {
        let mut reader = FrameReader::with_limit(Cursor::new(b"1234"), 4);
        let error = reader.next_frame().await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn outbound_json_stops_at_the_frame_limit() {
        let value = serde_json::json!({"body": "x".repeat(MAX_FRAME_BYTES)});
        assert!(encode_json(&value).is_err());
    }
}
