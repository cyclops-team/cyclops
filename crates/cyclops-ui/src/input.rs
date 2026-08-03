//! Key input: a blocking reader thread feeding decoded keys into the
//! runtime channel. The event loop never touches stdin itself, so a slow
//! terminal can never block a frame (keypress budget: under 50ms).

use std::io::Read;

/// The keys the stream UI answers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Esc,
    Tab,
    Backspace,
    Up,
    Down,
    End,
    Home,
    CtrlC,
}

/// Decode one raw read into keys. Escape sequences arrive whole in one
/// read in practice; a lone ESC byte is the escape key.
pub fn decode(buf: &[u8]) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        match buf[i] {
            0x03 => keys.push(Key::CtrlC),
            b'\r' | b'\n' => keys.push(Key::Enter),
            b'\t' => keys.push(Key::Tab),
            0x7f | 0x08 => keys.push(Key::Backspace),
            0x1b => {
                // CSI arrow/home/end forms; anything else after ESC is
                // treated as the escape key plus the remaining bytes.
                if i + 2 < buf.len() && buf[i + 1] == b'[' {
                    let (key, used) = match buf[i + 2] {
                        b'A' => (Some(Key::Up), 3),
                        b'B' => (Some(Key::Down), 3),
                        b'F' => (Some(Key::End), 3),
                        b'H' => (Some(Key::Home), 3),
                        // Unknown CSI: skip through its final byte.
                        _ => (None, csi_len(&buf[i..])),
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

/// Length of a CSI sequence starting at ESC: through its final byte
/// (0x40..=0x7e), or the whole buffer when unterminated.
fn csi_len(buf: &[u8]) -> usize {
    for (n, b) in buf.iter().enumerate().skip(2) {
        if (0x40..=0x7e).contains(b) {
            return n + 1;
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
pub fn spawn_reader(tx: tokio::sync::mpsc::UnboundedSender<Key>) {
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 64];
        loop {
            let n = match stdin.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            for key in decode(&buf[..n]) {
                if tx.send(key).is_err() {
                    return;
                }
            }
        }
    });
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
    fn multibyte_chars_and_batches_decode() {
        assert_eq!(decode("é".as_bytes()), vec![Key::Char('é')]);
        assert_eq!(
            decode(b"wq\x1b[A"),
            vec![Key::Char('w'), Key::Char('q'), Key::Up]
        );
    }
}
