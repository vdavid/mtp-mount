//! `FileBuffer`, where a file being written waits for its upload, checked against a `Vec`.
//!
//! It runs on a `Cursor<Vec<u8>>`, so no file is opened. The input is a program: each step
//! is an `op` byte, a two-byte offset, and a length byte; a write's data follows it.
#![no_main]
use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use mtp_mount::buffer::FileBuffer;

fuzz_target!(|data: &[u8]| {
    let mut buffer = FileBuffer::with_storage(2, 0, Cursor::new(Vec::new()));
    let mut model: Vec<u8> = Vec::new();
    let mut input = data;

    while let Some((head, rest)) = input.split_first_chunk::<4>() {
        input = rest;
        let offset = i64::from(u16::from_le_bytes([head[1], head[2]]));
        let len = usize::from(head[3]);
        match head[0] % 3 {
            0 => {
                // Write `len` bytes of the input (fewer if it runs out) at `offset`.
                let take = len.min(input.len());
                let (bytes, rest) = input.split_at(take);
                input = rest;
                let written = buffer.write_at(offset, bytes).expect("a write failed");
                assert_eq!(written as usize, bytes.len(), "a short write");
                // An empty write changes nothing, as with `pwrite(2)`.
                if !bytes.is_empty() {
                    let (start, end) = (offset as usize, offset as usize + bytes.len());
                    if model.len() < end {
                        model.resize(end, 0);
                    }
                    model[start..end].copy_from_slice(bytes);
                }
            }
            1 => {
                // Read back, as a FUSE read on a file still open for writing does. It must
                // see exactly what was written, with zeros in any gap.
                let got = buffer.read_at(offset, len as u32).expect("a read failed");
                let start = (offset as usize).min(model.len());
                let end = (start + len).min(model.len());
                assert_eq!(got, &model[start..end], "read({offset}, {len}) disagrees");
            }
            _ => {
                // A negative offset is refused and changes nothing.
                assert!(
                    buffer.write_at(-1 - offset, b"x").is_err(),
                    "a negative write passed"
                );
            }
        }
        // `len` is what getattr reports while the file is open, and what the upload sends.
        assert_eq!(buffer.len(), model.len() as u64, "the length drifted");
        // Anything written makes the buffer dirty, which is what makes the close upload it.
        if !model.is_empty() {
            assert!(
                buffer.is_dirty(),
                "bytes were written but the buffer isn't dirty"
            );
        }
    }

    // What the upload reads is what was written.
    assert_eq!(
        buffer.into_file().into_inner(),
        model,
        "the spooled bytes differ"
    );
});
