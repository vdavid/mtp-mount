//! `mtp-mountd --dry-run`: does an arrival's mount key match its departure's?
//!
//! The dry run exists because nothing else can check the hotplug path: USB can't
//! be simulated, so the key derivation only meets a real device when someone
//! plugs one in. These tests cover the half that *can* be checked without a
//! cable, through the same channel the USB watch feeds in production: the
//! matching logic, the wording of the verdict a person reads while plugging, and
//! the promise that a dry run touches nothing on disk.
//!
//! Everything here runs anywhere: no FUSE, no device, no mount.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};

use mtp_mount::daemon::dryrun::{DepartureVerdict, DeviceFacts, DryRun, DryRunCommand};

/// An output sink the test can read back after the reporter has written to it.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).expect("the report is UTF-8")
    }

    fn sink(&self) -> Box<dyn Write + Send> {
        Box::new(self.clone())
    }
}

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn phone(serial: &str) -> DeviceFacts {
    DeviceFacts {
        serial: Some(serial.to_string()),
        vendor_id: 0x18d1,
        product_id: 0x4ee1,
        location_id: 3,
        manufacturer: Some("Google".into()),
        product: Some("Pixel 9 Pro XL".into()),
        speed: Some("SuperPlus".into()),
        match_reason: Some("standard_class".into()),
        label: format!("Google Pixel 9 Pro XL (serial: {serial}, location: 00000003)"),
    }
}

fn reporter(root: &Path) -> (DryRun, Captured) {
    let captured = Captured::default();
    let dry = DryRun::with_output(root.to_path_buf(), captured.sink());
    (dry, captured)
}

#[test]
fn a_departure_matching_its_arrival_says_so() {
    let (mut dry, out) = reporter(Path::new("/run/user/1000/mtp"));
    let device = phone("2A31FDH200ABC");

    dry.arrived(&device);
    let verdict = dry.left(&device);

    assert_eq!(
        verdict,
        DepartureVerdict::Matched(PathBuf::from("/run/user/1000/mtp/2A31FDH200ABC"))
    );
    let text = out.text();
    assert!(text.contains("MATCHES the arrival"), "{text}");
    assert!(
        text.contains("/run/user/1000/mtp/2A31FDH200ABC"),
        "the full mount path is what a person checks: {text}"
    );
    assert!(!text.contains("NO MATCH"), "{text}");
    assert_eq!(dry.tally().matched, 1);
    assert_eq!(dry.tally().unmatched, 0);
    assert!(dry.open_keys().is_empty());
}

#[test]
fn a_departure_with_a_different_key_warns_about_the_leak() {
    // The whole reason the mode exists: a device whose departure derives a
    // different key leaves its mount up for as long as the daemon runs, and
    // nothing in the log would otherwise say so.
    let (mut dry, out) = reporter(Path::new("/run/user/1000/mtp"));

    dry.arrived(&phone("2A31FDH200ABC"));
    let verdict = dry.left(&phone("SOMETHING-ELSE"));

    assert_eq!(
        verdict,
        DepartureVerdict::WouldLeak(vec![PathBuf::from("/run/user/1000/mtp/2A31FDH200ABC")])
    );
    let text = out.text();
    assert!(text.contains("NO MATCH"), "{text}");
    assert!(text.contains("leak"), "{text}");
    assert!(
        text.contains("/run/user/1000/mtp/2A31FDH200ABC"),
        "the warning has to name the mount that would be left behind: {text}"
    );
    assert!(
        text.contains("SOMETHING-ELSE"),
        "and the key that didn't match: {text}"
    );
    assert_eq!(dry.tally().unmatched, 1);
    assert_eq!(dry.open_keys(), vec!["2A31FDH200ABC".to_string()]);
}

#[test]
fn a_departure_with_no_arrival_behind_it_is_reported_plainly() {
    let (mut dry, out) = reporter(Path::new("/run/user/1000/mtp"));

    let verdict = dry.left(&phone("2A31FDH200ABC"));

    assert_eq!(verdict, DepartureVerdict::NothingToUnmount);
    let text = out.text();
    assert!(text.contains("NO MATCH"), "{text}");
    assert!(
        text.contains("nothing leaks"),
        "nothing was mounted, so this isn't the failure case: {text}"
    );
    assert_eq!(dry.tally().unmatched, 1);
}

#[test]
fn a_device_with_no_serial_falls_back_to_the_usb_address() {
    // Same rule the daemon mounts under, so the dry run reports the real path.
    let facts = DeviceFacts {
        serial: None,
        vendor_id: 0x04e8,
        product_id: 0x6860,
        location_id: 42,
        product: Some("Galaxy S24".into()),
        ..Default::default()
    };
    assert_eq!(facts.key(), "usb-04e8-6860-42");
    assert_eq!(facts.ident().key, "usb-04e8-6860-42");
    assert_eq!(facts.ident().serial, None);

    let (mut dry, out) = reporter(Path::new("/run/user/1000/mtp"));
    dry.arrived(&facts);
    assert!(
        out.text().contains("/run/user/1000/mtp/usb-04e8-6860-42"),
        "{}",
        out.text()
    );
}

#[test]
fn a_dry_run_creates_nothing_and_mounts_nothing() {
    // The mode has to be safe to run on a machine with no FUSE, so it must not
    // reach the filesystem at all: not the mount root, not a mount point.
    let root = tempfile::tempdir().unwrap();
    let mount_root = root.path().join("mtp");
    let (events, inbox) = channel();
    let captured = Captured::default();
    let dry = DryRun::with_output(mount_root.clone(), captured.sink());

    events
        .send(DryRunCommand::Arrived(phone("2A31FDH200ABC")))
        .unwrap();
    events
        .send(DryRunCommand::Left(phone("2A31FDH200ABC")))
        .unwrap();
    events.send(DryRunCommand::Stop("test".into())).unwrap();
    dry.run(inbox);

    assert!(
        !mount_root.exists(),
        "a dry run created the mount root at {}",
        mount_root.display()
    );
    assert_eq!(
        std::fs::read_dir(root.path()).unwrap().count(),
        0,
        "a dry run left something in {}",
        root.path().display()
    );
    let text = captured.text();
    assert!(text.contains("MATCHES the arrival"), "{text}");
    assert!(text.contains("Matched:    1"), "{text}");
    assert!(text.contains("Unmatched:  0"), "{text}");
}

#[test]
fn the_summary_calls_out_a_session_where_a_departure_missed() {
    let (dry, out) = reporter(Path::new("/run/user/1000/mtp"));
    let (events, inbox) = channel();

    events
        .send(DryRunCommand::Arrived(phone("2A31FDH200ABC")))
        .unwrap();
    events
        .send(DryRunCommand::Left(phone("SOMETHING-ELSE")))
        .unwrap();
    events.send(DryRunCommand::Stop("test".into())).unwrap();
    dry.run(inbox);

    let text = out.text();
    assert!(text.contains("PROBLEM"), "{text}");
    assert!(text.contains("Unmatched:  1"), "{text}");
    assert!(
        text.contains("Would be mounted now: 2A31FDH200ABC"),
        "{text}"
    );
}

#[test]
fn a_run_with_no_devices_says_what_to_check() {
    let (dry, out) = reporter(Path::new("/run/user/1000/mtp"));
    let (events, inbox) = channel();
    events.send(DryRunCommand::Stop("test".into())).unwrap();
    dry.run(inbox);

    let text = out.text();
    assert!(text.contains("nothing will be mounted"), "{text}");
    assert!(text.contains("No devices seen at all"), "{text}");
}
