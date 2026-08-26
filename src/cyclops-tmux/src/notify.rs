//! Control-mode notification parsing.
//!
//! Every `%`-prefixed line outside a reply block is parsed here. Unknown
//! lines become [`Notification::Other`], never an error: tmux control mode
//! is unversioned and grows notifications between releases, and the adapter
//! must keep working against tmux HEAD (ADR-001 T7). Lines enter as raw
//! bytes ([`parse_notification_bytes`]): %output data is not guaranteed to
//! be valid UTF-8 on the wire (F22), and decoding must never turn a
//! notification into an error.

/// One control-mode notification or adapter continuity event.
///
/// Field shapes follow tmux 3.6a, verified live during M0. Ids keep their
/// sigils: sessions `$0`, windows `@0`, panes `%0`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notification {
    /// The adapter dropped one or more notifications after its bounded
    /// queue filled. This is synthetic, never parsed from tmux. Consumers
    /// must replace derived state from an authoritative snapshot before
    /// trusting later notifications as a contiguous stream.
    ContinuityLost,
    /// Pane output. `data` is the raw byte stream after octal unescaping.
    Output {
        /// Pane id, e.g. "%3".
        pane: String,
        /// Unescaped output bytes.
        data: Vec<u8>,
    },
    /// Pane output in flow-control form. tmux switches from %output to this
    /// once the client sets the pause-after flag (MEASURED on 3.6a).
    ExtendedOutput {
        /// Pane id.
        pane: String,
        /// Milliseconds tmux buffered this chunk before sending.
        age_ms: u64,
        /// Unescaped output bytes.
        data: Vec<u8>,
    },
    /// This client's attached session changed.
    SessionChanged { session: String, name: String },
    /// A session was renamed. tmux 3.7 includes its stable id; older
    /// versions may send only the new name.
    SessionRenamed {
        session: Option<String>,
        name: String,
    },
    /// Sessions were created or destroyed somewhere on the server.
    SessionsChanged,
    /// Window linked into the attached session.
    WindowAdd { window: String },
    /// Window closed in the attached session.
    WindowClose { window: String },
    /// Window renamed in the attached session.
    WindowRenamed { window: String, name: String },
    /// Window created outside the attached session.
    UnlinkedWindowAdd { window: String },
    /// Window closed outside the attached session.
    UnlinkedWindowClose { window: String },
    /// Window renamed outside the attached session.
    UnlinkedWindowRenamed { window: String, name: String },
    /// Pane layout of a window changed (split, kill, resize). `rest` is the
    /// raw layout description; the watcher treats this as a hint only.
    LayoutChange { window: String, rest: String },
    /// Active pane of a window changed.
    WindowPaneChanged { window: String, pane: String },
    /// A pane entered or left a mode (copy mode and friends). Which way it
    /// went is not in the notification; reconcile to find out.
    PaneModeChanged { pane: String },
    /// A client detached from the server.
    ClientDetached { client: String },
    /// Another client switched session.
    ClientSessionChanged {
        client: String,
        session: String,
        name: String,
    },
    /// A `refresh-client -B` subscription value changed. This is the primary
    /// per-pane change signal (MEASURED working on 3.6a).
    SubscriptionChanged {
        /// Subscription name as given to -B.
        name: String,
        /// Session id when present.
        session: Option<String>,
        /// Window id when present.
        window: Option<String>,
        /// Pane id when present.
        pane: Option<String>,
        /// Expanded format value.
        value: String,
    },
    /// Flow control paused a pane (pause-after). The client auto-resumes;
    /// the consumer sees this for observability.
    Pause { pane: String },
    /// Flow control resumed a pane.
    Continue { pane: String },
    /// The server is closing this control connection.
    Exit { reason: Option<String> },
    /// Anything we do not recognize, kept verbatim for debug logging.
    /// Forward compatibility with tmux HEAD: never an error.
    Other(String),
}

/// Parse one control-mode line given as raw bytes. Total, like
/// [`parse_notification`].
///
/// Invalid UTF-8 is a real wire condition, not corruption (MEASURED on
/// 3.6a, F22): pane bytes >= 0x80 ride %output/%extended-output verbatim,
/// and a multi-byte character split across two pty reads makes each of the
/// two lines invalid on its own. Those lines are parsed byte-faithfully so
/// the pane's exact bytes reach the consumer. Any other notification kind
/// keeps its structure through a lossy conversion.
pub fn parse_notification_bytes(line: &[u8]) -> Notification {
    match std::str::from_utf8(line) {
        Ok(s) => parse_notification(s),
        Err(_) => match parse_output_bytes(line) {
            Some(n) => n,
            None => parse_notification(&String::from_utf8_lossy(line)),
        },
    }
}

/// Byte-faithful %output / %extended-output parse for lines that are not
/// valid UTF-8. None for any other shape (caller falls back to lossy).
fn parse_output_bytes(line: &[u8]) -> Option<Notification> {
    if let Some(rest) = line.strip_prefix(b"%output ".as_slice()) {
        let sp = rest.iter().position(|&b| b == b' ')?;
        let pane = std::str::from_utf8(&rest[..sp]).ok()?;
        return Some(Notification::Output {
            pane: pane.to_string(),
            data: unescape_octal_bytes(&rest[sp + 1..]),
        });
    }
    if let Some(rest) = line.strip_prefix(b"%extended-output ".as_slice()) {
        let sp = rest.iter().position(|&b| b == b' ')?;
        let pane = std::str::from_utf8(&rest[..sp]).ok()?;
        let tail = &rest[sp + 1..];
        // Same shape as the str parser: age (plus reserved words), then
        // " : ", then data; empty data leaves the line ending in " :".
        let (head, data) = match find_subslice(tail, b" : ") {
            Some(i) => (&tail[..i], &tail[i + 3..]),
            None => (tail.strip_suffix(b" :".as_slice())?, &[][..]),
        };
        let age_ms = head
            .split(|&b| b == b' ')
            .next()
            .and_then(|a| std::str::from_utf8(a).ok())
            .and_then(|a| a.parse().ok())
            .unwrap_or(0);
        return Some(Notification::ExtendedOutput {
            pane: pane.to_string(),
            age_ms,
            data: unescape_octal_bytes(data),
        });
    }
    None
}

/// First occurrence of `needle` in `hay`.
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Parse one control-mode line into a [`Notification`]. Total: unknown or
/// malformed lines land in [`Notification::Other`].
pub fn parse_notification(line: &str) -> Notification {
    let (word, rest) = match line.split_once(' ') {
        Some((w, r)) => (w, r),
        None => (line, ""),
    };
    match word {
        "%output" => match rest.split_once(' ') {
            Some((pane, data)) => Notification::Output {
                pane: pane.to_string(),
                data: unescape_octal(data),
            },
            // A pane write can be empty; the pane id alone is still valid.
            None if !rest.is_empty() => Notification::Output {
                pane: rest.to_string(),
                data: Vec::new(),
            },
            None => Notification::Other(line.to_string()),
        },
        "%extended-output" => parse_extended_output(line, rest),
        "%session-changed" => match rest.split_once(' ') {
            Some((session, name)) => Notification::SessionChanged {
                session: session.to_string(),
                name: name.to_string(),
            },
            None => Notification::Other(line.to_string()),
        },
        "%session-renamed" => match rest.split_once(' ') {
            Some((session, name)) if session.starts_with('$') => Notification::SessionRenamed {
                session: Some(session.to_string()),
                name: name.to_string(),
            },
            _ => Notification::SessionRenamed {
                session: None,
                name: rest.to_string(),
            },
        },
        "%sessions-changed" => Notification::SessionsChanged,
        "%window-add" => Notification::WindowAdd {
            window: rest.to_string(),
        },
        "%window-close" => Notification::WindowClose {
            window: rest.to_string(),
        },
        "%window-renamed" => match rest.split_once(' ') {
            Some((window, name)) => Notification::WindowRenamed {
                window: window.to_string(),
                name: name.to_string(),
            },
            None => Notification::Other(line.to_string()),
        },
        "%unlinked-window-add" => Notification::UnlinkedWindowAdd {
            window: rest.to_string(),
        },
        "%unlinked-window-close" => Notification::UnlinkedWindowClose {
            window: rest.to_string(),
        },
        "%unlinked-window-renamed" => match rest.split_once(' ') {
            Some((window, name)) => Notification::UnlinkedWindowRenamed {
                window: window.to_string(),
                name: name.to_string(),
            },
            // 3.6a sends this without a name; keep the window id.
            None => Notification::UnlinkedWindowRenamed {
                window: rest.to_string(),
                name: String::new(),
            },
        },
        "%layout-change" => match rest.split_once(' ') {
            Some((window, layout)) => Notification::LayoutChange {
                window: window.to_string(),
                rest: layout.to_string(),
            },
            None => Notification::Other(line.to_string()),
        },
        "%window-pane-changed" => match rest.split_once(' ') {
            Some((window, pane)) => Notification::WindowPaneChanged {
                window: window.to_string(),
                pane: pane.to_string(),
            },
            None => Notification::Other(line.to_string()),
        },
        "%pane-mode-changed" => Notification::PaneModeChanged {
            pane: rest.to_string(),
        },
        "%client-detached" => Notification::ClientDetached {
            client: rest.to_string(),
        },
        "%client-session-changed" => {
            let mut it = rest.splitn(3, ' ');
            match (it.next(), it.next(), it.next()) {
                (Some(client), Some(session), Some(name)) => Notification::ClientSessionChanged {
                    client: client.to_string(),
                    session: session.to_string(),
                    name: name.to_string(),
                },
                _ => Notification::Other(line.to_string()),
            }
        }
        "%subscription-changed" => parse_subscription_changed(line, rest),
        "%pause" => Notification::Pause {
            pane: rest.to_string(),
        },
        "%continue" => Notification::Continue {
            pane: rest.to_string(),
        },
        "%exit" => Notification::Exit {
            reason: if rest.is_empty() {
                None
            } else {
                Some(rest.to_string())
            },
        },
        _ => Notification::Other(line.to_string()),
    }
}

/// `%extended-output %pane age [reserved...] : data`. Words between the age
/// and the lone `:` are reserved for future tmux versions and ignored.
fn parse_extended_output(line: &str, rest: &str) -> Notification {
    let Some((pane, tail)) = rest.split_once(' ') else {
        return Notification::Other(line.to_string());
    };
    let Some((head, data)) = tail.split_once(" : ").or_else(|| {
        // Data can be empty, leaving the line ending in " :".
        tail.strip_suffix(" :").map(|h| (h, ""))
    }) else {
        return Notification::Other(line.to_string());
    };
    let age_ms = head
        .split_whitespace()
        .next()
        .and_then(|a| a.parse::<u64>().ok())
        .unwrap_or(0);
    Notification::ExtendedOutput {
        pane: pane.to_string(),
        age_ms,
        data: unescape_octal(data),
    }
}

/// `%subscription-changed name $session @window window-index %pane ... : value`
/// on 3.6a. Header tokens are recognized by sigil so session-level or
/// window-level subscriptions (fewer ids) still parse.
fn parse_subscription_changed(line: &str, rest: &str) -> Notification {
    let Some((header, value)) = rest
        .split_once(" : ")
        .or_else(|| rest.strip_suffix(" :").map(|h| (h, "")))
    else {
        return Notification::Other(line.to_string());
    };
    let mut tokens = header.split_whitespace();
    let Some(name) = tokens.next() else {
        return Notification::Other(line.to_string());
    };
    let mut session = None;
    let mut window = None;
    let mut pane = None;
    for t in tokens {
        if t.starts_with('$') {
            session = Some(t.to_string());
        } else if t.starts_with('@') {
            window = Some(t.to_string());
        } else if t.starts_with('%') {
            pane = Some(t.to_string());
        }
        // Bare numbers (window index) and dashes are ignored.
    }
    Notification::SubscriptionChanged {
        name: name.to_string(),
        session,
        window,
        pane,
        value: value.to_string(),
    }
}

/// Undo tmux's octal escaping: `\ooo` (three octal digits) becomes one byte;
/// everything else passes through. tmux escapes control bytes, tab, newline,
/// and backslash this way and leaves valid UTF-8 alone, so the result is the
/// pane's raw byte stream.
pub fn unescape_octal(s: &str) -> Vec<u8> {
    unescape_octal_bytes(s.as_bytes())
}

/// [`unescape_octal`] over raw bytes: needed because %output data is not
/// guaranteed to be valid UTF-8 on the wire (F22).
pub fn unescape_octal_bytes(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            let d = &b[i + 1..i + 4];
            if d.iter().all(|c| (b'0'..=b'7').contains(c)) {
                let v = u32::from(d[0] - b'0') * 64
                    + u32::from(d[1] - b'0') * 8
                    + u32::from(d[2] - b'0');
                out.push(v as u8);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_with_octal_escapes() {
        // Escapes as tmux 3.6a actually writes them: \015 CR, \033 ESC,
        // \134 backslash.
        let n = parse_notification("%output %0 a\\015\\033[1mb\\134c");
        match n {
            Notification::Output { pane, data } => {
                assert_eq!(pane, "%0");
                assert_eq!(data, b"a\r\x1b[1mb\\c");
            }
            other => panic!("wrong parse: {other:?}"),
        }
    }

    #[test]
    fn output_keeps_utf8_and_lone_backslash() {
        let n = parse_notification("%output %2 ok \u{1F441} tail\\x");
        match n {
            Notification::Output { data, .. } => {
                assert_eq!(data, "ok \u{1F441} tail\\x".as_bytes());
            }
            other => panic!("wrong parse: {other:?}"),
        }
    }

    #[test]
    fn extended_output_with_age_and_colon() {
        // Verbatim shape from the 3.6a probe.
        let n = parse_notification("%extended-output %0 128 : EXTPROBE\\015\\012");
        match n {
            Notification::ExtendedOutput { pane, age_ms, data } => {
                assert_eq!(pane, "%0");
                assert_eq!(age_ms, 128);
                assert_eq!(data, b"EXTPROBE\r\n");
            }
            other => panic!("wrong parse: {other:?}"),
        }
        // Reserved words before the colon are ignored.
        let n = parse_notification("%extended-output %1 5 future words : x");
        match n {
            Notification::ExtendedOutput { pane, age_ms, data } => {
                assert_eq!(pane, "%1");
                assert_eq!(age_ms, 5);
                assert_eq!(data, b"x");
            }
            other => panic!("wrong parse: {other:?}"),
        }
    }

    #[test]
    fn subscription_changed_full_header() {
        // Verbatim shape from the 3.6a probe.
        let n = parse_notification("%subscription-changed ps $0 @0 0 %0 : mac,0,0,zsh");
        match n {
            Notification::SubscriptionChanged {
                name,
                session,
                window,
                pane,
                value,
            } => {
                assert_eq!(name, "ps");
                assert_eq!(session.as_deref(), Some("$0"));
                assert_eq!(window.as_deref(), Some("@0"));
                assert_eq!(pane.as_deref(), Some("%0"));
                assert_eq!(value, "mac,0,0,zsh");
            }
            other => panic!("wrong parse: {other:?}"),
        }
    }

    #[test]
    fn session_and_window_notifications() {
        assert_eq!(
            parse_notification("%session-changed $0 probe"),
            Notification::SessionChanged {
                session: "$0".into(),
                name: "probe".into()
            }
        );
        assert_eq!(
            parse_notification("%session-renamed $1 renamed project"),
            Notification::SessionRenamed {
                session: Some("$1".into()),
                name: "renamed project".into()
            }
        );
        assert_eq!(
            parse_notification("%session-renamed legacy-name"),
            Notification::SessionRenamed {
                session: None,
                name: "legacy-name".into()
            }
        );
        assert_eq!(
            parse_notification("%window-renamed @0 my window name"),
            Notification::WindowRenamed {
                window: "@0".into(),
                name: "my window name".into()
            }
        );
        assert_eq!(
            parse_notification("%window-pane-changed @0 %1"),
            Notification::WindowPaneChanged {
                window: "@0".into(),
                pane: "%1".into()
            }
        );
        assert_eq!(
            parse_notification("%pane-mode-changed %0"),
            Notification::PaneModeChanged { pane: "%0".into() }
        );
        assert_eq!(
            parse_notification("%sessions-changed"),
            Notification::SessionsChanged
        );
    }

    #[test]
    fn pause_continue_exit() {
        assert_eq!(
            parse_notification("%pause %5"),
            Notification::Pause { pane: "%5".into() }
        );
        assert_eq!(
            parse_notification("%continue %5"),
            Notification::Continue { pane: "%5".into() }
        );
        assert_eq!(
            parse_notification("%exit"),
            Notification::Exit { reason: None }
        );
        assert_eq!(
            parse_notification("%exit detached"),
            Notification::Exit {
                reason: Some("detached".into())
            }
        );
    }

    #[test]
    fn bytes_valid_utf8_delegates_to_str_parser() {
        assert_eq!(
            parse_notification_bytes(b"%output %0 hello"),
            parse_notification("%output %0 hello")
        );
        assert_eq!(
            parse_notification_bytes(b"%pause %5"),
            Notification::Pause { pane: "%5".into() }
        );
    }

    #[test]
    fn bytes_raw_invalid_output_is_byte_faithful() {
        // MEASURED wire shape (F22): bytes >= 0x80 arrive verbatim.
        let n = parse_notification_bytes(b"%output %0 X\xffY\\015");
        match n {
            Notification::Output { pane, data } => {
                assert_eq!(pane, "%0");
                assert_eq!(data, b"X\xffY\r");
            }
            other => panic!("wrong parse: {other:?}"),
        }
    }

    #[test]
    fn bytes_split_multibyte_fragment_survives() {
        // MEASURED (F22): a split braille char yields two lines, each an
        // invalid fragment on its own.
        let n = parse_notification_bytes(b"%output %0 \xe2\xa0");
        match n {
            Notification::Output { data, .. } => assert_eq!(data, b"\xe2\xa0"),
            other => panic!("wrong parse: {other:?}"),
        }
        let n = parse_notification_bytes(b"%extended-output %1 42 : \x9b\\012");
        match n {
            Notification::ExtendedOutput { pane, age_ms, data } => {
                assert_eq!(pane, "%1");
                assert_eq!(age_ms, 42);
                assert_eq!(data, b"\x9b\n");
            }
            other => panic!("wrong parse: {other:?}"),
        }
    }

    #[test]
    fn bytes_invalid_non_output_lines_keep_structure_lossily() {
        // Free-text tails of other notifications should not break parsing;
        // structure survives with U+FFFD in the text.
        let n = parse_notification_bytes(b"%session-renamed na\xffme");
        match n {
            Notification::SessionRenamed { session, name } => {
                assert_eq!(session, None);
                assert_eq!(name, "na\u{FFFD}me");
            }
            other => panic!("wrong parse: {other:?}"),
        }
    }

    #[test]
    fn unknown_lines_become_other_never_error() {
        // Notifications tmux HEAD may grow must not break the adapter.
        let lines = [
            "%paste-buffer-changed buf0",
            "%message some text",
            "%totally-new-thing a b c",
            "%config-error /path: bad",
        ];
        for l in lines {
            assert_eq!(parse_notification(l), Notification::Other(l.into()), "{l}");
        }
    }
}
