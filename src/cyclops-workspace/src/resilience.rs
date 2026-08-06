//! Resilience timers — bounded one-shot reconnect chain.
//!
//! `RECONNECT_ATTEMPT_1` .. `RECONNECT_ATTEMPT_4`: delay before each retry.
//! The chain stops at `RECONNECT_CAP`; nothing reschedules itself.

use std::time::Duration;

/// Maximum reconnect attempts before giving up to the server-gone flow.
pub const RECONNECT_CAP: usize = 4;

/// One-shot delays (ms) for attempts 0..RECONNECT_CAP-1.
pub const RECONNECT_ATTEMPT_1: Duration = Duration::from_millis(100);
pub const RECONNECT_ATTEMPT_2: Duration = Duration::from_millis(300);
pub const RECONNECT_ATTEMPT_3: Duration = Duration::from_millis(800);
pub const RECONNECT_ATTEMPT_4: Duration = Duration::from_millis(2000);

const DELAYS: [Duration; RECONNECT_CAP] = [
    RECONNECT_ATTEMPT_1,
    RECONNECT_ATTEMPT_2,
    RECONNECT_ATTEMPT_3,
    RECONNECT_ATTEMPT_4,
];

/// Delay before reconnect attempt `n` (0-based). Clamps at the last entry.
pub fn reconnect_delay(attempt: usize) -> Duration {
    DELAYS[attempt.min(RECONNECT_CAP - 1)]
}

/// Whether another reconnect attempt is allowed.
pub fn may_retry(attempt: usize) -> bool {
    attempt < RECONNECT_CAP
}

/// Control-mode link health.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    Live,
    Reconnecting { attempt: usize },
    ServerGone,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_chain_stops_at_cap() {
        assert!(may_retry(0));
        assert!(may_retry(RECONNECT_CAP - 1));
        assert!(!may_retry(RECONNECT_CAP));
    }

    #[test]
    fn reconnect_delays_are_bounded() {
        for i in 0..RECONNECT_CAP {
            assert!(reconnect_delay(i) <= RECONNECT_ATTEMPT_4);
        }
        assert_eq!(reconnect_delay(99), RECONNECT_ATTEMPT_4);
    }
}
