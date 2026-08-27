//! One-way "take this mount down" signal.
//!
//! A FUSE callback can't unmount itself: it holds the inode lock and still owes
//! the kernel a reply, and `fuser` hands the unmount handle to whoever mounted
//! the filesystem, not to the filesystem. So when the device is gone for good,
//! the callback records why here and returns an error; the thread that owns the
//! mount picks the reason up and unmounts.

use log::error;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// Shared shutdown request, one reason, first one wins.
#[derive(Debug, Default)]
pub struct Shutdown {
    reason: Mutex<Option<String>>,
    changed: Condvar,
}

impl Shutdown {
    /// Asks for the mount to go away. Later calls are ignored: the first reason
    /// is the one that explains what happened.
    pub fn request(&self, reason: impl Into<String>) {
        let mut slot = self.reason.lock().unwrap();
        if slot.is_none() {
            *slot = Some(reason.into());
            self.changed.notify_all();
        }
    }

    /// Whether a shutdown was requested.
    #[allow(dead_code)] // used by integration tests via lib.rs, not by the bin
    pub fn is_requested(&self) -> bool {
        self.reason.lock().unwrap().is_some()
    }

    /// The reason, if one was recorded.
    #[allow(dead_code)] // used by integration tests via lib.rs, not by the bin
    pub fn reason(&self) -> Option<String> {
        self.reason.lock().unwrap().clone()
    }

    /// Waits up to `timeout` for a shutdown request and returns its reason.
    pub fn wait_timeout(&self, timeout: Duration) -> Option<String> {
        let slot = self.reason.lock().unwrap();
        let (slot, _) = self
            .changed
            .wait_timeout_while(slot, timeout, |reason| reason.is_none())
            .unwrap();
        slot.clone()
    }
}

/// Turn a stop signal into whatever the caller wants to stop.
///
/// `systemd` stops services with `SIGTERM`, and a person stops one in a
/// terminal with `SIGINT`. Both have to unmount everything on the way out:
/// mounts left behind after a `systemctl --user stop` are exactly the wedged
/// directories the daemon exists to avoid. The single-device binary catches
/// the same signals for a different reason: a whole-object read holds the MTP
/// session for the entire transfer, and it has to be cancelled rather than cut.
pub fn spawn_signal_handler<F>(rt: &tokio::runtime::Handle, on_signal: F)
where
    F: FnOnce(&str) + Send + 'static,
{
    rt.spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};

        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(stream) => stream,
            Err(e) => {
                error!("Can't listen for SIGTERM: {e}");
                return;
            }
        };
        let mut interrupt = match signal(SignalKind::interrupt()) {
            Ok(stream) => stream,
            Err(e) => {
                error!("Can't listen for SIGINT: {e}");
                return;
            }
        };

        let signal_name = tokio::select! {
            _ = terminate.recv() => "SIGTERM",
            _ = interrupt.recv() => "SIGINT",
        };
        on_signal(signal_name);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn starts_quiet() {
        let shutdown = Shutdown::default();
        assert!(!shutdown.is_requested());
        assert_eq!(shutdown.reason(), None);
        assert_eq!(shutdown.wait_timeout(Duration::from_millis(1)), None);
    }

    #[test]
    fn keeps_the_first_reason() {
        let shutdown = Shutdown::default();
        shutdown.request("device gone");
        shutdown.request("something else");
        assert!(shutdown.is_requested());
        assert_eq!(shutdown.reason().as_deref(), Some("device gone"));
    }

    #[test]
    fn wakes_a_waiter() {
        let shutdown = Arc::new(Shutdown::default());
        let signaller = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            signaller.request("device gone");
        });
        assert_eq!(
            shutdown.wait_timeout(Duration::from_secs(5)).as_deref(),
            Some("device gone")
        );
    }
}
