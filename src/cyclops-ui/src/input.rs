//! Key input: a blocking reader thread feeding decoded keys into the
//! runtime channel. The event loop never touches stdin itself, so a slow
//! terminal can never block a frame (keypress budget: under 50ms).

use crate::key::Key;

/// Decode one raw read into keys. Escape sequences arrive whole in one
/// read in practice; a lone ESC byte is the escape key.
pub fn decode(buf: &[u8]) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        match buf[i] {
            0x03 => keys.push(Key::CtrlC),
            0x04 => keys.push(Key::CtrlD),
            b'\r' | b'\n' => keys.push(Key::Enter),
            b'\t' => keys.push(Key::Tab),
            0x7f | 0x08 => keys.push(Key::Backspace),
            0x1b => {
                // CSI arrow/home/end/mouse forms; anything else after ESC
                // is treated as the escape key plus the remaining bytes.
                if i + 2 < buf.len() && buf[i + 1] == b'[' {
                    let (key, used) = match buf[i + 2] {
                        b'A' => (Some(Key::Up), 3),
                        b'B' => (Some(Key::Down), 3),
                        b'F' => (Some(Key::End), 3),
                        b'H' => (Some(Key::Home), 3),
                        // SGR mouse report: ESC [ < btn ; col ; row M|m.
                        b'<' => {
                            let len = csi_len(&buf[i..]);
                            (decode_mouse(&buf[i..i + len]), len)
                        }
                        // Bracketed paste brackets, and anything else CSI
                        // is skipped through its final byte.
                        _ => {
                            let len = csi_len(&buf[i..]);
                            (paste_marker(&buf[i..i + len]), len)
                        }
                    };
                    if let Some(k) = key {
                        keys.push(k);
                    }
                    i += used;
                    continue;
                }
                keys.push(Key::Esc);
            }
            _ => {
                // One UTF-8 character, control bytes dropped.
                let len = utf8_len(buf[i]);
                if i + len <= buf.len() {
                    if let Ok(s) = std::str::from_utf8(&buf[i..i + len]) {
                        if let Some(c) = s.chars().next() {
                            if !c.is_control() {
                                keys.push(Key::Char(c));
                            }
                        }
                    }
                }
                i += len;
                continue;
            }
        }
        i += 1;
    }
    keys
}

/// One SGR mouse report, `ESC [ < btn ; col ; row M` (press) or `m`
/// (release), the terminal's cells 1-based.
///
/// Three reports matter: a left press is a click, and wheel motion is a
/// wheel. Everything else (releases, drags, other buttons, clicks with
/// modifiers held) decodes to nothing, on purpose: an unhandled gesture
/// must not turn into a surprise.
fn decode_mouse(seq: &[u8]) -> Option<Key> {
    let last = *seq.last()?;
    let body = std::str::from_utf8(&seq[3..seq.len() - 1]).ok()?;
    let mut parts = body.split(';');
    let btn: u16 = parts.next()?.parse().ok()?;
    let col: u16 = parts.next()?.parse().ok()?;
    let row: u16 = parts.next()?.parse().ok()?;
    match (btn, last) {
        (0, b'M') => Some(Key::Click {
            x: col.saturating_sub(1),
            y: row.saturating_sub(1),
        }),
        (64, b'M') => Some(Key::WheelUp),
        (65, b'M') => Some(Key::WheelDown),
        _ => None,
    }
}

/// `ESC [ 200 ~` and `ESC [ 201 ~`: the terminal bracketing a paste.
fn paste_marker(seq: &[u8]) -> Option<Key> {
    match seq {
        b"\x1b[200~" => Some(Key::PasteStart),
        b"\x1b[201~" => Some(Key::PasteEnd),
        _ => None,
    }
}

/// Length of a CSI sequence starting at ESC: through its final byte, or
/// up to the first byte that cannot belong to one.
///
/// After `ESC [` a CSI carries parameter and intermediate bytes
/// (0x20..=0x3f) and ends at a final byte (0x40..=0x7e). Anything else is
/// a broken sequence, and stopping there rather than swallowing the rest
/// of the buffer is what lets a Ctrl-C typed into a stuck sequence get
/// out: 0x03 cannot appear in a CSI, so it ends the broken one and is
/// then decoded as itself.
fn csi_len(buf: &[u8]) -> usize {
    for (n, b) in buf.iter().enumerate().skip(2) {
        if (0x40..=0x7e).contains(b) {
            return n + 1;
        }
        if !(0x20..=0x3f).contains(b) {
            return n;
        }
    }
    buf.len()
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// Spawn the reader thread. It parks in read() and dies with the process;
/// the runtime only ever sees decoded keys on the channel.
pub fn spawn_reader(tx: tokio::sync::mpsc::Sender<Key>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut pending: Vec<u8> = Vec::new();
        let mut in_paste = false;
        let mut paste_scan_from = 0;
        loop {
            for key in drain(&mut pending, &mut in_paste, &mut paste_scan_from) {
                if tx.blocking_send(key).is_err() {
                    return;
                }
            }
            // Nothing more can be decided without more bytes. Wait for
            // them, with a bound only where waiting forever would be
            // wrong.
            if !pending.is_empty() {
                let grace = if in_paste {
                    PASTE_ABANDON_MS
                } else {
                    PARTIAL_GRACE_MS
                };
                if !readable_within(grace) {
                    if in_paste {
                        // A paste the terminal never closed. Every byte
                        // held is payload, so it is DISCARDED rather than
                        // decoded: promoting it to keys is exactly the
                        // thing this buffer exists to prevent.
                        pending.clear();
                        in_paste = false;
                        paste_scan_from = 0;
                        if tx.blocking_send(Key::PasteRejected).is_err() {
                            return;
                        }
                    } else if pending == [0x1b] {
                        for key in decode(&pending) {
                            if tx.blocking_send(key).is_err() {
                                return;
                            }
                        }
                        pending.clear();
                    }
                    continue;
                }
            }
            let n = match read_fd(&mut buf) {
                Some(0) | None => return,
                Some(n) => n,
            };
            pending.extend_from_slice(&buf[..n]);
        }
    });
}

const PASTE_BEGIN: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Largest paste held before it is abandoned. A clipboard this size is
/// not a keystroke, and an unbounded buffer is a way to be killed.
const PASTE_MAX: usize = 1 << 20;

/// How long an unterminated paste waits for its closing marker before its
/// bytes are thrown away.
const PASTE_ABANDON_MS: libc::c_int = 2_000;

/// Take everything decidable out of `pending`.
///
/// A paste is emitted only once its terminator has been seen, whole, as
/// PasteStart, its text, then PasteEnd. Nothing inside a paste is ever
/// decoded as a sequence, so a clipboard containing `ESC q`, or `ESC[200~`
/// again, or any other control run, cannot put the UI back into command
/// mode one byte early. That is the property markers alone could not
/// give: with them, the payload's own bytes decided when quarantine
/// ended.
fn drain(pending: &mut Vec<u8>, in_paste: &mut bool, paste_scan_from: &mut usize) -> Vec<Key> {
    let mut out = Vec::new();
    loop {
        if *in_paste {
            match find(&pending[*paste_scan_from..], PASTE_END)
                .map(|relative| *paste_scan_from + relative)
            {
                Some(at) => {
                    let payload: Vec<u8> = pending.drain(..at).collect();
                    pending.drain(..PASTE_END.len());
                    *in_paste = false;
                    *paste_scan_from = 0;
                    // Checked here too, not only while waiting: a paste
                    // whose terminator arrives in the same read was
                    // delivered whole however large it was.
                    if payload.len() > PASTE_MAX {
                        out.push(Key::PasteRejected);
                        continue;
                    }
                    out.push(Key::PasteStart);
                    out.extend(payload_keys(&payload));
                    out.push(Key::PasteEnd);
                }
                None => {
                    if pending.len() > PASTE_MAX {
                        pending.clear();
                        *in_paste = false;
                        *paste_scan_from = 0;
                        out.push(Key::PasteRejected);
                    } else {
                        // A future terminator can start only in the suffix
                        // shorter than the marker. The rest was already
                        // searched on the previous read.
                        *paste_scan_from = pending
                            .len()
                            .saturating_sub(PASTE_END.len().saturating_sub(1));
                    }
                    return out;
                }
            }
        } else {
            match find(pending, PASTE_BEGIN) {
                Some(at) => {
                    let head: Vec<u8> = pending.drain(..at).collect();
                    pending.drain(..PASTE_BEGIN.len());
                    out.extend(decode(&head));
                    *in_paste = true;
                    *paste_scan_from = 0;
                }
                None => {
                    // Hold back anything that could still become the
                    // opening marker, as well as any half-read unit.
                    let keep = complete_len(pending).min(pending.len() - partial_begin(pending));
                    let head: Vec<u8> = pending.drain(..keep).collect();
                    out.extend(decode(&head));
                    return out;
                }
            }
        }
    }
}

/// How many trailing bytes are a prefix of the paste-begin marker.
fn partial_begin(buf: &[u8]) -> usize {
    (1..PASTE_BEGIN.len().min(buf.len() + 1))
        .rev()
        .find(|&n| buf.ends_with(&PASTE_BEGIN[..n]))
        .unwrap_or(0)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Pasted text as keys. Text only: a paste carries no commands, so
/// control bytes other than a newline are dropped rather than decoded.
fn payload_keys(payload: &[u8]) -> Vec<Key> {
    let text = String::from_utf8_lossy(payload);
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // CRLF is ONE line break. Emitting two put a blank line into
            // every reply pasted from anything Windows touched.
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(Key::Enter);
            }
            '\n' => out.push(Key::Enter),
            // Four spaces, matching grid::safe_text. Dropping tabs lost
            // the indentation of every pasted snippet; keeping the byte
            // would put a character the frame measures as one column and
            // the terminal draws as eight into a width-checked row.
            '\t' => out.extend(std::iter::repeat_n(Key::Char(' '), 4)),
            c if c.is_control() => {}
            c => out.push(Key::Char(c)),
        }
    }
    out
}

/// How long a lone trailing ESC waits before it is taken as the escape
/// key.
///
/// The one irreducible ambiguity in terminal input: a bare ESC is either
/// the escape key or the first byte of a sequence, and only time
/// separates them. Half-read sequences do NOT use this; they wait for
/// their final byte. Neither does a paste, which waits for its
/// terminator and is discarded rather than decoded if none comes.
const PARTIAL_GRACE_MS: libc::c_int = 200;

/// One raw read from the same descriptor `readable_within` polls.
///
/// Deliberately NOT `std::io::Stdin`, which keeps its own buffer in this
/// process. A read that filled that buffer and handed back only part of
/// it left the rest invisible to `poll`, so the grace timer could expire
/// while the remainder of a sequence was already in hand. One buffer, and
/// readiness asked of the descriptor that actually holds the bytes.
fn read_fd(buf: &mut [u8]) -> Option<usize> {
    loop {
        let n = unsafe {
            libc::read(
                libc::STDIN_FILENO,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };
        if n >= 0 {
            return Some(n as usize);
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return None;
        }
    }
}

/// Is there more input within `ms`?
fn readable_within(ms: libc::c_int) -> bool {
    let mut fds = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    // EINTR reports "nothing yet". For a lone ESC that means the escape
    // key, and for a paste it means the buffer is discarded: both are the
    // safe direction.
    unsafe { libc::poll(&mut fds, 1, ms) > 0 }
}

/// How much of `buf` ends on a unit boundary.
///
/// Everything after the returned index is a partial escape sequence or a
/// partial UTF-8 character, which decodes correctly only once the rest
/// arrives.
fn complete_len(buf: &[u8]) -> usize {
    let mut i = 0;
    while i < buf.len() {
        let len = match buf[i] {
            0x1b => {
                if i + 1 >= buf.len() {
                    return i;
                }
                if buf[i + 1] == b'[' {
                    match csi_complete(&buf[i..]) {
                        Some(n) => n,
                        None => return i,
                    }
                } else {
                    1
                }
            }
            b if b < 0x80 => 1,
            b => {
                let n = utf8_len(b);
                if i + n > buf.len() {
                    return i;
                }
                n
            }
        };
        i += len;
    }
    i
}

/// Length through a CSI's final byte, or through the byte that broke it.
/// `None` means it is still coming and must be waited for.
fn csi_complete(buf: &[u8]) -> Option<usize> {
    for (n, b) in buf.iter().enumerate().skip(2) {
        if (0x40..=0x7e).contains(b) {
            return Some(n + 1);
        }
        // Not a byte a CSI can carry, so this one is broken. It is over,
        // whatever it was, and the offending byte is decoded on its own.
        if !(0x20..=0x3f).contains(b) {
            return Some(n);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_chars_and_controls_decode() {
        assert_eq!(decode(b"q"), vec![Key::Char('q')]);
        assert_eq!(decode(b"?"), vec![Key::Char('?')]);
        assert_eq!(decode(b"\t"), vec![Key::Tab]);
        assert_eq!(decode(b"\r"), vec![Key::Enter]);
        assert_eq!(decode(&[0x7f]), vec![Key::Backspace]);
        assert_eq!(decode(&[0x03]), vec![Key::CtrlC]);
        assert_eq!(decode(&[0x1b]), vec![Key::Esc]);
    }

    #[test]
    fn arrows_and_ends_decode() {
        assert_eq!(decode(b"\x1b[A"), vec![Key::Up]);
        assert_eq!(decode(b"\x1b[B"), vec![Key::Down]);
        assert_eq!(decode(b"\x1b[F"), vec![Key::End]);
        assert_eq!(decode(b"\x1b[H"), vec![Key::Home]);
        // Unknown CSI sequences are swallowed whole, not misread as chars.
        assert_eq!(decode(b"\x1b[15~q"), vec![Key::Char('q')]);
    }

    #[test]
    fn mouse_reports_decode_and_the_rest_are_dropped() {
        // 1-based cells arrive, 0-based cells leave.
        assert_eq!(decode(b"\x1b[<0;5;3M"), vec![Key::Click { x: 4, y: 2 }]);
        assert_eq!(decode(b"\x1b[<64;10;2M"), vec![Key::WheelUp]);
        assert_eq!(decode(b"\x1b[<65;10;2M"), vec![Key::WheelDown]);
        // Release, drag, right button: swallowed whole, never misread.
        assert_eq!(decode(b"\x1b[<0;5;3m"), vec![]);
        assert_eq!(decode(b"\x1b[<32;5;3M"), vec![]);
        assert_eq!(decode(b"\x1b[<2;5;3M"), vec![]);
        // And a key after one still decodes.
        assert_eq!(decode(b"\x1b[<0;5;3mq"), vec![Key::Char('q')]);
    }

    #[test]
    fn multibyte_chars_and_batches_decode() {
        assert_eq!(decode("é".as_bytes()), vec![Key::Char('é')]);
        assert_eq!(
            decode(b"wq\x1b[A"),
            vec![Key::Char('w'), Key::Char('q'), Key::Up]
        );
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;

    /// Feed bytes through the real reader state machine in chunks of
    /// `chunk`, ending with the abandon rule so an unterminated paste is
    /// discarded exactly as the reader discards it.
    fn through_reader(bytes: &[u8], chunk: usize) -> Vec<Key> {
        let mut pending: Vec<u8> = Vec::new();
        let mut in_paste = false;
        let mut paste_scan_from = 0;
        let mut out = Vec::new();
        for piece in bytes.chunks(chunk.max(1)) {
            pending.extend_from_slice(piece);
            out.extend(drain(&mut pending, &mut in_paste, &mut paste_scan_from));
        }
        // Input stops here. A half-read sequence that is just an ESC
        // becomes the escape key. An unterminated paste is thrown away
        // and reported through the same rejection event as the live reader.
        if in_paste {
            pending.clear();
            out.push(Key::PasteRejected);
        } else if pending == [0x1b] {
            out.extend(decode(&pending));
            pending.clear();
        }
        out
    }

    /// Marker-delimited quarantine fails when a clipboard carries `ESC q`:
    /// the ESC leaves quarantine and the q becomes a quit command.
    /// Buffering the paste to its terminator means neither byte is ever
    /// anything but text.
    #[test]
    fn a_pasted_escape_then_q_is_text_at_every_chunking() {
        let raw = b"\x1b[200~a\x1bq\x1b[201~";
        for chunk in 1..=raw.len() {
            let keys = through_reader(raw, chunk);
            assert_eq!(
                keys.first(),
                Some(&Key::PasteStart),
                "chunk {chunk}: {keys:?}"
            );
            assert_eq!(keys.last(), Some(&Key::PasteEnd), "chunk {chunk}: {keys:?}");
            assert!(
                !keys.contains(&Key::Esc),
                "chunk {chunk}: a pasted ESC became the escape key: {keys:?}"
            );
            // The ESC is dropped as a control byte; the letters survive.
            assert!(keys.contains(&Key::Char('a')) && keys.contains(&Key::Char('q')));
        }
    }

    /// Nothing from a paste reaches the app before its terminator does,
    /// however the bytes are chopped.
    #[test]
    fn a_paste_is_emitted_whole_or_not_at_all() {
        let raw = b"\x1b[200~q1\ny\x1b[201~";
        for chunk in 1..=raw.len() {
            let keys = through_reader(raw, chunk);
            let mut bracketed = false;
            for key in &keys {
                match key {
                    Key::PasteStart => bracketed = true,
                    Key::PasteEnd => bracketed = false,
                    Key::Char(_) | Key::Enter => assert!(
                        bracketed,
                        "chunk {chunk}: {key:?} escaped the paste: {keys:?}"
                    ),
                    other => panic!("chunk {chunk}: {other:?} came out of a paste"),
                }
            }
            assert_eq!(keys.first(), Some(&Key::PasteStart), "chunk {chunk}");
            assert_eq!(keys.last(), Some(&Key::PasteEnd), "chunk {chunk}");
        }
    }

    /// A paste the terminal never closes is discarded, never decoded.
    /// Promoting held bytes to keys is the failure being prevented.
    #[test]
    fn an_unterminated_paste_is_visible_and_not_decoded() {
        let keys = through_reader(b"\x1b[200~q1y", 2);
        assert_eq!(keys, vec![Key::PasteRejected]);
    }

    /// CRLF is one line break, not two, and a tab is text.
    #[test]
    fn a_payload_keeps_its_shape() {
        let keys = through_reader(b"\x1b[200~a\r\nb\tc\x1b[201~", 4);
        let text: String = keys
            .iter()
            .filter_map(|k| match k {
                Key::Char(c) => Some(*c),
                Key::Enter => Some('\n'),
                _ => None,
            })
            .collect();
        assert_eq!(
            text, "a\nb    c",
            "CRLF became two breaks, or a tab was dropped: {keys:?}"
        );
    }

    /// A multibyte character split across the chunk boundary inside a
    /// paste must survive: the payload is buffered whole, so this is
    /// about the buffer, not the decoder.
    #[test]
    fn a_payload_survives_a_utf8_split() {
        let mut raw = Vec::from(&b"\x1b[200~"[..]);
        raw.extend_from_slice("héllo → 🙂".as_bytes());
        raw.extend_from_slice(PASTE_END);
        for chunk in 1..=raw.len() {
            let text: String = through_reader(&raw, chunk)
                .iter()
                .filter_map(|k| match k {
                    Key::Char(c) => Some(*c),
                    _ => None,
                })
                .collect();
            assert_eq!(text, "héllo → 🙂", "chunk {chunk} mangled the payload");
        }
    }

    /// A payload larger than the cap is dropped rather than buffered
    /// without limit, and the reader keeps working afterwards.
    #[test]
    fn an_oversized_payload_is_dropped_and_the_reader_recovers() {
        let mut raw = Vec::from(&b"\x1b[200~"[..]);
        raw.extend(std::iter::repeat_n(b'x', PASTE_MAX + 16));
        raw.extend_from_slice(PASTE_END);
        raw.push(b'j');
        let keys = through_reader(&raw, 4096);
        assert!(
            !keys.contains(&Key::Char('x')),
            "an oversized paste was buffered and delivered"
        );
        assert!(keys.contains(&Key::PasteRejected));
        assert!(
            keys.contains(&Key::Char('j')),
            "the reader did not recover after dropping a huge paste: {keys:?}"
        );
    }

    /// A paste whose opening marker is malformed is not a paste. It must
    /// not open quarantine, and it must not act either.
    #[test]
    fn a_malformed_opening_marker_does_not_open_a_paste() {
        let keys = through_reader(b"\x1b[200q", 2);
        assert!(
            !keys.contains(&Key::PasteStart),
            "a malformed marker opened a paste: {keys:?}"
        );
    }

    /// Ordinary keys still work, split anywhere.
    #[test]
    fn an_arrow_key_survives_every_split_point() {
        for chunk in 1..=3 {
            assert_eq!(
                through_reader(b"\x1b[A", chunk),
                vec![Key::Up],
                "arrow lost at chunk {chunk}"
            );
        }
    }

    #[test]
    fn a_multibyte_character_survives_every_split_point() {
        for text in ["é", "→", "🙂"] {
            let raw = text.as_bytes();
            for chunk in 1..=raw.len() {
                assert_eq!(
                    through_reader(raw, chunk),
                    vec![Key::Char(text.chars().next().unwrap())],
                    "{text} lost at chunk {chunk}"
                );
            }
        }
    }

    #[test]
    fn a_lone_escape_still_arrives() {
        assert_eq!(through_reader(b"\x1b", 1), vec![Key::Esc]);
    }

    /// Text on either side of a paste is still ordinary input.
    #[test]
    fn keys_around_a_paste_are_unaffected() {
        let keys = through_reader(b"j\x1b[200~hi\x1b[201~k", 3);
        assert_eq!(keys.first(), Some(&Key::Char('j')));
        assert_eq!(keys.last(), Some(&Key::Char('k')));
    }
}
