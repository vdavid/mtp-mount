//! `--dry-run`: watch real devices, mount nothing, and say what would happen.
//!
//! # What it's for
//!
//! The hotplug path is the one part of the daemon no test can reach: USB can't
//! be simulated, so [`crate::daemon::usb::ident_of`] never sees a real
//! [`MtpDeviceInfo`](mtp_rs::mtp::MtpDeviceInfo) until someone plugs a phone in.
//! The risk that hides there is the mount key: it's derived from what the watch
//! reports, and if an arrival and its matching departure derive it *differently*,
//! no departure ever matches a mounted device and the daemon leaks mount points
//! for as long as it runs. Every other bug in the daemon is visible; that one is
//! silent.
//!
//! So `--dry-run` answers exactly that question against real hardware, on a
//! machine that may have no FUSE at all: plug the phone in, pull it out, and read
//! whether the two keys agreed.
//!
//! # The seam
//!
//! Same shape as the supervisor's (see [`crate::daemon::supervisor`]): the
//! reporter's whole input is a channel, [`crate::daemon::usb::spawn_dry_run_watch`]
//! is the only thing that knows about USB, and a test injects
//! [`DryRunCommand`]s to exercise the matching logic and the output without a
//! cable.
//!
//! The commands carry [`DeviceFacts`] rather than the supervisor's
//! [`DeviceIdent`], because a dry run has to show its work: the raw fields the
//! key came from are the evidence a person needs to see, while a mount only ever
//! needs the key. `DeviceFacts` is also plain owned data, so tests can build one,
//! which they can't do with `MtpDeviceInfo` (`#[non_exhaustive]`).
//!
//! # What it must not do
//!
//! Nothing on the filesystem: no mount, no mount root, no mount point, no stale
//! sweep, and the device is never opened. It prints the paths it *would* use.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use crate::daemon::paths::device_dir_name;
use crate::daemon::supervisor::DeviceIdent;

/// Everything the USB watch reported about one device, as reported.
///
/// The fields are what [`key`](Self::key) is derived from plus the strings that
/// let a person recognize the device in a scrolling log. `Default` is there so
/// tests can state only the fields they care about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceFacts {
    /// Serial number, exactly as reported (untrimmed).
    pub serial: Option<String>,
    /// USB vendor ID.
    pub vendor_id: u16,
    /// USB product ID.
    pub product_id: u16,
    /// USB topology position: which port the device is in.
    pub location_id: u64,
    /// Manufacturer string from the USB descriptor.
    pub manufacturer: Option<String>,
    /// Product string from the USB descriptor.
    pub product: Option<String>,
    /// Negotiated USB link speed, when the OS reports one.
    pub speed: Option<String>,
    /// Why `mtp-rs` classified this device as MTP. Arrivals only, in practice.
    pub match_reason: Option<String>,
    /// What the daemon calls the device in its log (`MtpDeviceInfo::display()`).
    pub label: String,
}

impl DeviceFacts {
    /// The serial the key derivation actually uses: trimmed, and `None` when blank.
    fn usable_serial(&self) -> Option<&str> {
        self.serial
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// The mount key: directory name and device identity in one.
    ///
    /// This is the value the whole dry run is about. See
    /// [`device_dir_name`] for the serial-then-USB-address rule.
    #[must_use]
    pub fn key(&self) -> String {
        device_dir_name(
            self.usable_serial(),
            self.vendor_id,
            self.product_id,
            self.location_id,
        )
    }

    /// What the supervisor would be handed for this device.
    ///
    /// The daemon's real path goes through here too
    /// ([`crate::daemon::usb::ident_of`] is this function over an
    /// `MtpDeviceInfo`), so a dry run and a live mount can't derive keys
    /// differently.
    #[must_use]
    pub fn ident(&self) -> DeviceIdent {
        DeviceIdent {
            key: self.key(),
            label: self.headline(),
            serial: self.usable_serial().map(str::to_string),
        }
    }

    /// The one-line name for the log, falling back to the key for a device that
    /// reports no strings at all.
    fn headline(&self) -> String {
        if self.label.is_empty() {
            format!("MTP device {}", self.key())
        } else {
            self.label.clone()
        }
    }
}

/// What drives the reporter. The USB watch sends these; so do tests.
#[derive(Debug, Clone)]
pub enum DryRunCommand {
    /// A device was plugged in, or was already there when watching started.
    Arrived(DeviceFacts),
    /// A device went away.
    Left(DeviceFacts),
    /// Print the summary and return from [`DryRun::run`].
    Stop(String),
}

/// What a departure turned out to be: the answer the dry run exists for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepartureVerdict {
    /// The departure's key is one that arrived, so the right mount comes down.
    Matched(PathBuf),
    /// The key matched nothing that arrived, and mounts are open: those stay
    /// mounted forever. This is the failure `--dry-run` is looking for.
    WouldLeak(Vec<PathBuf>),
    /// The key matched nothing, but nothing was mounted either, so there's
    /// nothing to leak: a departure before any arrival, or a repeat departure.
    NothingToUnmount,
}

/// Running totals, so a person can plug in and out a few times and glance.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    /// Arrivals seen.
    pub arrivals: usize,
    /// Departures seen.
    pub departures: usize,
    /// Departures that matched an arrival.
    pub matched: usize,
    /// Departures that didn't.
    pub unmatched: usize,
}

/// Reports what the daemon would do, and mounts nothing.
pub struct DryRun {
    mount_root: PathBuf,
    /// Keys that arrived and haven't left: what would be mounted right now.
    open: HashMap<String, PathBuf>,
    /// Every key seen arriving this session, for the no-match message.
    arrived_keys: Vec<String>,
    tally: Tally,
    events: usize,
    out: Box<dyn Write + Send>,
}

/// Width of the label column in the fact list.
const FIELD: usize = 16;

impl DryRun {
    /// A reporter that prints to stdout.
    #[must_use]
    pub fn new(mount_root: PathBuf) -> Self {
        Self::with_output(mount_root, Box::new(std::io::stdout()))
    }

    /// A reporter that prints wherever you tell it. Tests capture the output.
    #[must_use]
    pub fn with_output(mount_root: PathBuf, out: Box<dyn Write + Send>) -> Self {
        Self {
            mount_root,
            open: HashMap::new(),
            arrived_keys: Vec::new(),
            tally: Tally::default(),
            events: 0,
            out,
        }
    }

    /// Totals so far.
    #[must_use]
    pub fn tally(&self) -> Tally {
        self.tally
    }

    /// Keys that arrived and haven't left, sorted. Used by tests.
    #[must_use]
    pub fn open_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.open.keys().cloned().collect();
        keys.sort();
        keys
    }

    /// Report events until [`DryRunCommand::Stop`] arrives or every sender is
    /// dropped, then print the summary.
    ///
    /// Blocks the calling thread, like [`crate::daemon::supervisor::Supervisor::run`],
    /// and for the same reason: the daemon keeps its runtime for the watch.
    pub fn run(mut self, commands: Receiver<DryRunCommand>) {
        self.banner();
        let reason = loop {
            match commands.recv() {
                Ok(DryRunCommand::Arrived(facts)) => self.arrived(&facts),
                Ok(DryRunCommand::Left(facts)) => {
                    self.left(&facts);
                }
                Ok(DryRunCommand::Stop(reason)) => break reason,
                Err(_) => break "the device watch stopped".to_string(),
            }
        };
        self.summary(&reason);
    }

    /// Header: what this mode does and what to do with it.
    pub fn banner(&mut self) {
        let root = self.mount_root.display().to_string();
        self.line("");
        self.line("=== mtp-mountd --dry-run: nothing will be mounted ===");
        self.line(&format!("Mount root it would use: {root}"));
        self.line("Plug a device in, wait for it to appear, then unplug it.");
        self.line("Every departure should say MATCHES. Stop with Ctrl-C.");
        self.line("");
    }

    /// Report an arrival.
    pub fn arrived(&mut self, facts: &DeviceFacts) {
        let key = facts.key();
        let path = self.mount_root.join(&key);

        self.tally.arrivals += 1;
        self.event_header("PLUGGED IN", facts);
        self.facts(facts, true);
        self.field("mount key", &key);
        self.field("would mount at", &path.display().to_string());

        if let Some(existing) = self.open.get(&key) {
            let existing = existing.display().to_string();
            self.line(&format!(
                "    Note: {key} is already down as arrived (at {existing}). The daemon \
                 would keep the first mount and ignore this one."
            ));
        } else {
            self.open.insert(key.clone(), path);
        }
        if !self.arrived_keys.contains(&key) {
            self.arrived_keys.push(key);
        }
        self.running_total();
    }

    /// Report a departure, and say whether it matched. Returns the verdict so
    /// tests can assert on it without parsing the text.
    pub fn left(&mut self, facts: &DeviceFacts) -> DepartureVerdict {
        let key = facts.key();

        self.tally.departures += 1;
        self.event_header("UNPLUGGED", facts);
        self.facts(facts, false);
        self.field("mount key", &key);

        let verdict = match self.open.remove(&key) {
            Some(path) => DepartureVerdict::Matched(path),
            None if self.open.is_empty() => DepartureVerdict::NothingToUnmount,
            None => {
                let mut leaked: Vec<PathBuf> = self.open.values().cloned().collect();
                leaked.sort();
                DepartureVerdict::WouldLeak(leaked)
            }
        };

        match &verdict {
            DepartureVerdict::Matched(path) => {
                self.tally.matched += 1;
                self.line(&format!(
                    ">>> MATCHES the arrival: the daemon would unmount {} <<<",
                    path.display()
                ));
            }
            DepartureVerdict::WouldLeak(leaked) => {
                self.tally.unmatched += 1;
                self.line("!!!");
                self.line(&format!(
                    "!!! NO MATCH - nothing arrived with the key {key}, so this departure",
                ));
                self.line("!!! would take nothing down. These mounts would leak:");
                for path in leaked {
                    self.line(&format!("!!!     {}", path.display()));
                }
                self.line(&format!(
                    "!!! Keys seen arriving: {}",
                    self.arrived_keys.join(", ")
                ));
                self.line("!!! This is the bug --dry-run looks for. Please report it.");
                self.line("!!!");
            }
            DepartureVerdict::NothingToUnmount => {
                self.tally.unmatched += 1;
                self.line(&format!(
                    "--- NO MATCH for {key}, but nothing is mounted, so nothing leaks."
                ));
                self.line(
                    "--- A departure with no arrival behind it: the device left twice, or it \
                     went away before the watch ever reported it.",
                );
            }
        }

        self.running_total();
        verdict
    }

    /// The closing report: the thing to read after a few plug cycles.
    pub fn summary(&mut self, reason: &str) {
        let Tally {
            arrivals,
            departures,
            matched,
            unmatched,
        } = self.tally;
        let still_open = self.open_keys();

        self.line("");
        self.line(&format!("=== Dry run over: {reason} ==="));
        self.line(&format!("  Arrivals:   {arrivals}"));
        self.line(&format!("  Departures: {departures}"));
        self.line(&format!("  Matched:    {matched}"));
        self.line(&format!("  Unmatched:  {unmatched}"));
        if still_open.is_empty() {
            self.line("  Would be mounted now: nothing");
        } else {
            self.line(&format!(
                "  Would be mounted now: {}",
                still_open.join(", ")
            ));
        }
        self.line("");
        if unmatched > 0 {
            self.line(
                "PROBLEM: a departure didn't match its arrival, so the daemon would leak a \
                 mount for this device. The fields above show which one disagreed.",
            );
        } else if departures > 0 {
            self.line(
                "All good: every departure matched an arrival, so the daemon takes the right \
                 mount down for this device.",
            );
        } else if arrivals > 0 {
            self.line(
                "No departures seen. Unplug the device while this is running to check the \
                 half that matters.",
            );
        } else {
            self.line(
                "No devices seen at all. Check the phone is unlocked and set to \"File \
                 Transfer\", and try RUST_LOG=debug.",
            );
        }
        self.line("");
        let _ = self.out.flush();
    }

    fn event_header(&mut self, what: &str, facts: &DeviceFacts) {
        self.events += 1;
        let n = self.events;
        let headline = facts.headline();
        self.line("------------------------------------------------------------");
        // Padded so the two kinds of block line up when a log is scrolling.
        self.line(&format!("#{n}  {what:<10}  {headline}"));
    }

    /// The raw fields the key came from. `full` adds the arrival-only ones.
    fn facts(&mut self, facts: &DeviceFacts, full: bool) {
        let unreported = "(not reported)".to_string();
        self.field(
            "serial",
            &facts.serial.clone().unwrap_or_else(|| unreported.clone()),
        );
        self.field(
            "vendor:product",
            &format!("{:04x}:{:04x}", facts.vendor_id, facts.product_id),
        );
        self.field("location id", &facts.location_id.to_string());
        self.field(
            "manufacturer",
            &facts
                .manufacturer
                .clone()
                .unwrap_or_else(|| unreported.clone()),
        );
        self.field(
            "product",
            &facts.product.clone().unwrap_or_else(|| unreported.clone()),
        );
        if full {
            self.field(
                "usb speed",
                &facts.speed.clone().unwrap_or_else(|| unreported.clone()),
            );
            self.field(
                "matched by",
                &facts.match_reason.clone().unwrap_or(unreported),
            );
        }
    }

    fn running_total(&mut self) {
        let Tally {
            arrivals,
            departures,
            matched,
            unmatched,
        } = self.tally;
        let open = self.open.len();
        self.line(&format!(
            "    So far: {arrivals} arrived, {departures} left, {matched} matched, \
             {unmatched} unmatched, {open} would be mounted."
        ));
    }

    fn field(&mut self, name: &str, value: &str) {
        self.line(&format!("    {name:<FIELD$}{value}"));
    }

    /// One line out. A closed stdout isn't worth stopping a dry run over.
    fn line(&mut self, text: &str) {
        let _ = writeln!(self.out, "{text}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn pixel() -> DeviceFacts {
        DeviceFacts {
            serial: Some("2A31FDH200ABC".into()),
            vendor_id: 0x18d1,
            product_id: 0x4ee1,
            location_id: 3,
            manufacturer: Some("Google".into()),
            product: Some("Pixel 9 Pro XL".into()),
            speed: Some("SuperPlus".into()),
            match_reason: Some("standard_class".into()),
            label: "Google Pixel 9 Pro XL (serial: 2A31FDH200ABC, location: 00000003)".into(),
        }
    }

    #[test]
    fn a_device_with_no_serial_falls_back_to_the_usb_address() {
        let facts = DeviceFacts {
            serial: None,
            vendor_id: 0x04e8,
            product_id: 0x6860,
            location_id: 42,
            ..Default::default()
        };
        assert_eq!(facts.key(), "usb-04e8-6860-42");
        assert_eq!(facts.ident().serial, None);
    }

    #[test]
    fn a_blank_serial_is_no_serial() {
        let facts = DeviceFacts {
            serial: Some("   ".into()),
            vendor_id: 1,
            product_id: 2,
            location_id: 3,
            ..Default::default()
        };
        assert_eq!(facts.key(), "usb-0001-0002-3");
    }

    #[test]
    fn the_ident_carries_the_key_the_supervisor_would_mount_under() {
        let ident = pixel().ident();
        assert_eq!(ident.key, "2A31FDH200ABC");
        assert_eq!(ident.serial.as_deref(), Some("2A31FDH200ABC"));
        assert!(ident.label.contains("Pixel 9 Pro XL"));
    }

    #[test]
    fn a_second_arrival_of_the_same_key_is_called_out() {
        let mut dry = DryRun::with_output(PathBuf::from("/run/mtp"), Box::new(Vec::new()));
        dry.arrived(&pixel());
        dry.arrived(&pixel());
        assert_eq!(dry.open_keys(), vec!["2A31FDH200ABC".to_string()]);
        assert_eq!(dry.tally().arrivals, 2);
    }

    #[test]
    fn the_loop_stops_when_every_sender_is_gone() {
        let (tx, rx) = channel();
        tx.send(DryRunCommand::Arrived(pixel())).unwrap();
        drop(tx);
        let dry = DryRun::with_output(PathBuf::from("/run/mtp"), Box::new(Vec::new()));
        dry.run(rx);
    }
}
