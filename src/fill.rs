//! Keeps track of the whole-object downloads a mount currently has in flight,
//! so the mount can be taken down without abandoning one.
//!
//! A sequential fill (see [`crate::sparse_cache`]) holds the device's single MTP
//! session for the whole transfer, which for a multi-gigabyte object is minutes.
//! Nothing a reader does may interrupt that. But the *mount* going away is a
//! different matter: dropping a live `mtp_rs::FileDownload` leaves the responder
//! in the middle of a USB transaction, and on Android that is the failure that
//! needs a physical replug. The device has to be told.
//!
//! So teardown goes through here: [`FillTracker::stop_and_wait`] asks every live
//! fill to cancel its transfer and then waits for them to actually finish, which
//! costs about as long as one cancel round-trip rather than the rest of the
//! download. Whoever owns the mount calls it after unmounting and before the
//! tokio runtime is dropped, because dropping the runtime is exactly the silent
//! abort this exists to prevent.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::sparse_cache::SharedSparseCache;

/// How long teardown waits for in-flight fills to cancel before giving up on
/// them. One cancel is a bounded round-trip to the device; a device that
/// doesn't answer at all is already gone, and blocking an unmount on it would
/// be worse than the untidy exit.
pub const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Default)]
struct TrackerState {
    live: Vec<Arc<SharedSparseCache>>,
    stopping: bool,
}

/// The mount's live whole-object downloads.
#[derive(Debug, Default)]
pub struct FillTracker {
    state: Mutex<TrackerState>,
    idle: Condvar,
}

impl FillTracker {
    /// Record a fill that is about to start.
    ///
    /// Returns `false` once teardown has begun, which means the caller must not
    /// open the transfer at all: a download started now would be one nothing is
    /// waiting for.
    pub fn register(&self, cache: Arc<SharedSparseCache>) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.stopping {
            return false;
        }
        state.live.push(cache);
        true
    }

    /// Record that a fill has ended, however it ended.
    pub fn finished(&self, cache: &Arc<SharedSparseCache>) {
        let mut state = self.state.lock().unwrap();
        state.live.retain(|live| !Arc::ptr_eq(live, cache));
        drop(state);
        self.idle.notify_all();
    }

    /// How many fills are in flight.
    #[allow(dead_code)] // used by tests, not by the binaries
    pub fn live_count(&self) -> usize {
        self.state.lock().unwrap().live.len()
    }

    /// Ask every live fill to cancel, then wait for them to finish.
    ///
    /// Returns `true` if they all finished within `timeout`. After this, no new
    /// fill starts, so it is safe to drop the runtime the fills were spawned on.
    pub fn stop_and_wait(&self, timeout: Duration) -> bool {
        let mut state = self.state.lock().unwrap();
        state.stopping = true;
        for cache in &state.live {
            cache.request_stop();
        }

        let deadline = Instant::now() + timeout;
        while !state.live.is_empty() {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (next, wait) = self.idle.wait_timeout(state, remaining).unwrap();
            state = next;
            if wait.timed_out() && !state.live.is_empty() {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn cache() -> Arc<SharedSparseCache> {
        Arc::new(SharedSparseCache::new(8, &std::env::temp_dir()).unwrap())
    }

    #[test]
    fn an_empty_tracker_stops_immediately() {
        let tracker = FillTracker::default();
        assert!(tracker.stop_and_wait(Duration::from_millis(50)));
    }

    #[test]
    fn stopping_asks_every_live_fill_to_cancel() {
        let tracker = Arc::new(FillTracker::default());
        let one = cache();
        let two = cache();
        assert!(tracker.register(Arc::clone(&one)));
        assert!(tracker.register(Arc::clone(&two)));
        assert_eq!(tracker.live_count(), 2);

        let waiter = Arc::clone(&tracker);
        let (finish_one, finish_two) = (Arc::clone(&one), Arc::clone(&two));
        let filler = thread::spawn(move || {
            while !finish_one.stop_requested() || !finish_two.stop_requested() {
                thread::sleep(Duration::from_millis(1));
            }
            waiter.finished(&finish_one);
            waiter.finished(&finish_two);
        });

        assert!(tracker.stop_and_wait(Duration::from_secs(5)));
        filler.join().unwrap();
        assert!(one.stop_requested());
        assert!(two.stop_requested());
        assert_eq!(tracker.live_count(), 0);
    }

    #[test]
    fn a_fill_that_never_answers_times_out_instead_of_blocking_the_unmount() {
        let tracker = FillTracker::default();
        let stuck = cache();
        assert!(tracker.register(Arc::clone(&stuck)));

        assert!(!tracker.stop_and_wait(Duration::from_millis(30)));
        assert!(stuck.stop_requested(), "it was still asked to stop");
    }

    #[test]
    fn no_new_fill_starts_once_teardown_has_begun() {
        let tracker = FillTracker::default();
        assert!(tracker.stop_and_wait(Duration::from_millis(10)));
        assert!(
            !tracker.register(cache()),
            "a fill registered after teardown would be one nobody waits for"
        );
    }
}
