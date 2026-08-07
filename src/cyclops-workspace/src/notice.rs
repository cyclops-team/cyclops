//! The workspace's transient notice: one short message with its own expiry.
//!
//! Feedback the operator is owed but must not have to dismiss. A clipboard
//! write is the first caller — nothing else on screen changes when a
//! selection lands — and anything later that finishes without moving the
//! frame belongs here too.
//!
//! The message carries its own deadline, so the event loop wakes for it the
//! same one-shot way it wakes for the render debounce and the reconnect
//! deadline: armed by an event, disarmed by running, never rescheduled on
//! its own (`docs/development/INVARIANTS.md`, rule 9). Nothing here sleeps
//! or spins; [`NoticeState::deadline`] joins the loop's deadline set and
//! [`NoticeState::expire`] is what running it means.

use tokio::time::{Duration, Instant};

/// How long a notice stays up: long enough to read a short phrase without
/// hunting for it, short enough to be gone before the next action. The
/// first number was a second and a half, which the operator read as
/// gone-before-you-look on a border you are not already watching.
pub const NOTICE_TTL: Duration = Duration::from_millis(3500);

/// One message slot. Empty is the resting state, and an empty slot has no
/// deadline, so an idle workspace still has no wakeups.
#[derive(Default)]
pub struct NoticeState {
    current: Option<(String, Instant)>,
}

impl NoticeState {
    /// Say `text` until [`NOTICE_TTL`] after `now`, replacing whatever was
    /// up. Unlike the render debounce this deadline DOES move: the newer
    /// fact is the one worth showing, and a message cannot starve anything
    /// by outliving its predecessor.
    pub fn show(&mut self, text: impl Into<String>, now: Instant) {
        self.current = Some((text.into(), now + NOTICE_TTL));
    }

    /// The deadline the event loop must wake for, or `None` when nothing
    /// is showing.
    pub fn deadline(&self) -> Option<Instant> {
        self.current.as_ref().map(|(_, until)| *until)
    }

    /// Drop the message once its deadline has passed. Returns whether the
    /// frame changed, so a caller repaints once per expiry and never for a
    /// deadline that belonged to something else.
    pub fn expire(&mut self, now: Instant) -> bool {
        if self
            .current
            .as_ref()
            .is_some_and(|(_, until)| *until <= now)
        {
            self.current = None;
            return true;
        }
        false
    }

    /// What to paint, if anything.
    pub fn text(&self) -> Option<&str> {
        self.current.as_ref().map(|(text, _)| text.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notice_shows_then_expires_on_its_own_deadline() {
        let now = Instant::now();
        let mut notice = NoticeState::default();
        assert_eq!(notice.deadline(), None, "an idle slot arms no wakeup");

        notice.show("copied 4 lines", now);
        assert_eq!(notice.text(), Some("copied 4 lines"));
        assert_eq!(notice.deadline(), Some(now + NOTICE_TTL));

        // Before the deadline nothing changes, so nothing repaints.
        assert!(!notice.expire(now + NOTICE_TTL - Duration::from_millis(1)));
        assert_eq!(notice.text(), Some("copied 4 lines"));

        // On the deadline it is gone, the frame changed exactly once, and
        // the slot stops asking the loop to wake.
        assert!(notice.expire(now + NOTICE_TTL));
        assert_eq!(notice.text(), None);
        assert_eq!(notice.deadline(), None);
        assert!(
            !notice.expire(now + NOTICE_TTL),
            "an already-empty slot must not keep claiming a repaint"
        );
    }

    /// A second message replaces the first outright, deadline included: two
    /// notices are never queued, and the older one cannot cut the newer
    /// one short.
    #[test]
    fn a_newer_notice_replaces_the_one_showing() {
        let now = Instant::now();
        let mut notice = NoticeState::default();
        notice.show("first", now);
        let later = now + Duration::from_millis(400);
        notice.show("second", later);

        assert_eq!(notice.text(), Some("second"));
        assert_eq!(notice.deadline(), Some(later + NOTICE_TTL));
        assert!(!notice.expire(now + NOTICE_TTL), "the first slot is gone");
        assert_eq!(notice.text(), Some("second"));
    }
}
