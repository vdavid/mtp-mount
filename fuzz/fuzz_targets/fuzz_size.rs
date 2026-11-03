//! `parse_size` and `format_size`: what `--full-download-limit` reads, and what `--help` prints.
#![no_main]
use libfuzzer_sys::fuzz_target;
use mtp_mount::size::{format_size, parse_size};

fuzz_target!(|data: &[u8]| {
    // Any text a person might type must parse or be refused, never panic.
    if let Ok(text) = std::str::from_utf8(data) {
        if let Ok(bytes) = parse_size(text) {
            // A size that parses formats to text that parses back to the same size, so the
            // help text and an error message can quote a limit the flag accepts.
            assert_eq!(
                parse_size(&format_size(bytes)),
                Ok(bytes),
                "{text:?} parsed to {bytes}, which didn't survive format_size"
            );
        }
    }

    // The same round trip for any byte count, read from the first eight bytes.
    if let Some(raw) = data.first_chunk::<8>() {
        let bytes = u64::from_le_bytes(*raw);
        assert_eq!(
            parse_size(&format_size(bytes)),
            Ok(bytes),
            "{bytes} didn't round-trip"
        );
    }
});
