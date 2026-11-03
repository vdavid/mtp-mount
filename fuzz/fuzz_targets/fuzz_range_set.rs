//! `RangeSet`, the bookkeeping behind the sparse read cache, checked against a bitmap.
//!
//! The input is a program: each step is three bytes, `op`, `a`, `b`. Offsets stay below
//! `LIMIT` so the bitmap stays small; the logic doesn't care about scale.
#![no_main]
use libfuzzer_sys::fuzz_target;
use mtp_mount::ranges::RangeSet;

const LIMIT: u64 = 512;

fuzz_target!(|data: &[u8]| {
    let mut set = RangeSet::new();
    let mut model = [false; LIMIT as usize];

    for step in data.chunks_exact(3) {
        let start = u64::from(step[1]) * 2;
        let len = u64::from(step[2]);
        if step[0] % 2 == 0 {
            // Insert, as `SparseCache::write_at` does after a fetch (it never inserts past
            // the object's size).
            let end = (start + len).min(LIMIT);
            let start = start.min(end);
            set.insert(start..end);
            for slot in &mut model[start as usize..end as usize] {
                *slot = true;
            }
        } else {
            // Ask what's missing, as a FUSE read does before it fetches. The request may run
            // past the limit, which clips it.
            let missing = set.missing(start, len, LIMIT);
            let expected = gaps(&model, start, start.saturating_add(len).min(LIMIT));
            // A read fetches exactly the bytes it lacks: fetching a held byte again costs a
            // USB round trip, and skipping a missing one serves zeros as file content.
            assert_eq!(
                missing, expected,
                "missing({start}, {len}) disagrees with the model"
            );
        }

        // The ranges are sorted, non-empty, and neither overlap nor touch, so `missing`
        // can walk them once, and they hold exactly the bytes the model holds.
        let ranges = set.as_slice();
        for pair in ranges.windows(2) {
            assert!(
                pair[0].end < pair[1].start,
                "ranges overlap or touch: {ranges:?}"
            );
        }
        assert!(
            ranges.iter().all(|r| r.start < r.end),
            "an empty range: {ranges:?}"
        );
        assert_eq!(
            gaps(&model, 0, LIMIT),
            complement(ranges),
            "the set lost or gained bytes"
        );
    }
});

/// The runs of `false` in `model[start..end]`.
fn gaps(model: &[bool], start: u64, end: u64) -> Vec<std::ops::Range<u64>> {
    let mut out = Vec::new();
    let mut run: Option<u64> = None;
    for i in start..end.max(start) {
        match (model[i as usize], run) {
            (false, None) => run = Some(i),
            (true, Some(s)) => {
                out.push(s..i);
                run = None;
            }
            _ => {}
        }
    }
    if let Some(s) = run {
        out.push(s..end);
    }
    out
}

/// What `ranges` leaves out of `[0, LIMIT)`.
fn complement(ranges: &[std::ops::Range<u64>]) -> Vec<std::ops::Range<u64>> {
    let mut out = Vec::new();
    let mut cursor = 0;
    for r in ranges {
        if r.start > cursor {
            out.push(cursor..r.start);
        }
        cursor = r.end;
    }
    if cursor < LIMIT {
        out.push(cursor..LIMIT);
    }
    out
}
