//! Policy for riding out a device disconnect.
//!
//! A flaky USB cable drops the device for a moment and brings it back. The
//! filesystem stays mounted while [`ReconnectPolicy`] still has budget, retrying
//! the reopen on a capped exponential backoff. Pure logic, no I/O, so the whole
//! schedule is unit-testable.

use std::time::Duration;

/// First pause after a disconnect, before the first reopen attempt.
const FIRST_DELAY: Duration = Duration::from_millis(250);

/// Longest pause between two reopen attempts.
const MAX_DELAY: Duration = Duration::from_secs(2);

/// How long to keep trying to reopen a device that went away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectPolicy {
    timeout: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self::from_secs(Self::DEFAULT_TIMEOUT_SECS)
    }
}

impl ReconnectPolicy {
    /// Default reconnect window, in seconds.
    pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

    /// Builds a policy that gives the device `secs` seconds to come back.
    /// Zero disables reconnection: the mount gives up on the first disconnect.
    pub fn from_secs(secs: u64) -> Self {
        Self {
            timeout: Duration::from_secs(secs),
        }
    }

    /// Whether reconnection is turned off (`--reconnect-timeout 0`).
    pub fn is_disabled(&self) -> bool {
        self.timeout.is_zero()
    }

    /// The reconnect window.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The pauses to take before each reopen attempt, in order. The iterator
    /// ends once the window is used up, which is the signal to give up and
    /// unmount.
    pub fn schedule(&self) -> Backoff {
        Backoff {
            remaining: self.timeout,
            next: FIRST_DELAY,
        }
    }
}

/// Capped exponential backoff bounded by the reconnect window.
///
/// Yields the sleep duration before each attempt. The last item is shortened so
/// the pauses never add up to more than the window.
#[derive(Debug)]
pub struct Backoff {
    remaining: Duration,
    next: Duration,
}

impl Iterator for Backoff {
    type Item = Duration;

    fn next(&mut self) -> Option<Duration> {
        if self.remaining.is_zero() {
            return None;
        }
        let delay = self.next.min(self.remaining);
        self.remaining -= delay;
        self.next = (self.next * 2).min(MAX_DELAY);
        Some(delay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_thirty_seconds() {
        assert_eq!(
            ReconnectPolicy::default().timeout(),
            Duration::from_secs(30)
        );
        assert!(!ReconnectPolicy::default().is_disabled());
    }

    #[test]
    fn zero_disables_reconnect() {
        let policy = ReconnectPolicy::from_secs(0);
        assert!(policy.is_disabled());
        assert_eq!(policy.schedule().count(), 0, "no attempts when disabled");
    }

    #[test]
    fn delays_grow_then_cap() {
        let delays: Vec<Duration> = ReconnectPolicy::from_secs(30).schedule().take(6).collect();
        assert_eq!(
            delays,
            vec![
                Duration::from_millis(250),
                Duration::from_millis(500),
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(2),
                Duration::from_secs(2),
            ]
        );
    }

    #[test]
    fn total_wait_never_exceeds_the_window() {
        for secs in [1, 3, 30, 120] {
            let total: Duration = ReconnectPolicy::from_secs(secs).schedule().sum();
            assert_eq!(
                total,
                Duration::from_secs(secs),
                "the schedule should use exactly the window for {secs}s"
            );
        }
    }

    #[test]
    fn short_window_still_gets_an_attempt() {
        let delays: Vec<Duration> = ReconnectPolicy::from_secs(1).schedule().collect();
        assert_eq!(delays.first(), Some(&Duration::from_millis(250)));
        assert!(delays.len() >= 3, "a 1s window should retry a few times");
    }

    #[test]
    fn last_delay_is_clipped_to_the_window() {
        // 3s window: 0.25 + 0.5 + 1 + 1.25 (clipped from 2).
        let delays: Vec<Duration> = ReconnectPolicy::from_secs(3).schedule().collect();
        assert_eq!(delays.last(), Some(&Duration::from_millis(1250)));
    }
}
