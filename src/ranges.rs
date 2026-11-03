//! Which byte ranges of a file are already cached.
//!
//! [`crate::sparse_cache::SparseCache`] keeps the bytes; this keeps the
//! bookkeeping, apart from any file so it's testable (and fuzzable) on its own.

use std::ops::Range;

/// A set of byte offsets, held as sorted, non-overlapping, non-adjacent ranges.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RangeSet {
    ranges: Vec<Range<u64>>,
}

impl RangeSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// The ranges, sorted by start, with no two overlapping or touching.
    #[allow(dead_code)] // The tests and `fuzz/` read it; `main.rs` compiles this module too.
    pub fn as_slice(&self) -> &[Range<u64>] {
        &self.ranges
    }

    /// Adds `new`, merging it with every range it overlaps or touches. An
    /// empty range changes nothing.
    pub fn insert(&mut self, new: Range<u64>) {
        if new.is_empty() {
            return;
        }
        let mut start = new.start;
        let mut end = new.end;

        // Remove and merge any existing range that overlaps or touches [start, end).
        self.ranges.retain(|r| {
            if r.end < start || r.start > end {
                true
            } else {
                start = start.min(r.start);
                end = end.max(r.end);
                false
            }
        });

        // Find the insertion point that keeps the ranges sorted by start.
        let pos = self
            .ranges
            .binary_search_by(|r| r.start.cmp(&start))
            .unwrap_or_else(|p| p);
        self.ranges.insert(pos, start..end);
    }

    /// The parts of `[offset, offset + size)`, clipped to `[0, limit)`, that
    /// aren't in the set, sorted. Empty when all of it is.
    pub fn missing(&self, offset: u64, size: u64, limit: u64) -> Vec<Range<u64>> {
        let end = offset.saturating_add(size).min(limit);
        if offset >= end {
            return Vec::new();
        }

        let mut missing = Vec::new();
        let mut cursor = offset;

        for populated in &self.ranges {
            if populated.end <= cursor {
                continue;
            }
            if populated.start >= end {
                break;
            }
            if populated.start > cursor {
                missing.push(cursor..populated.start.min(end));
            }
            cursor = populated.end;
            if cursor >= end {
                break;
            }
        }

        if cursor < end {
            missing.push(cursor..end);
        }

        missing
    }
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)] // intentional: asserting against one-range results
mod tests {
    use super::*;

    #[test]
    fn adjacent_and_overlapping_ranges_merge() {
        let mut set = RangeSet::new();
        set.insert(10..20);
        set.insert(30..40);
        set.insert(20..25);
        assert_eq!(set.as_slice(), &[10..25, 30..40]);
        set.insert(24..31);
        assert_eq!(set.as_slice(), &[10..40]);
    }

    #[test]
    fn an_empty_range_adds_nothing() {
        let mut set = RangeSet::new();
        set.insert(5..5);
        assert!(set.as_slice().is_empty());
        assert_eq!(set.missing(0, 10, 10), vec![0..10]);
    }

    #[test]
    fn missing_is_clipped_to_the_limit() {
        let mut set = RangeSet::new();
        set.insert(2..4);
        assert_eq!(set.missing(0, 100, 6), vec![0..2, 4..6]);
        assert!(set.missing(6, 10, 6).is_empty());
    }
}
