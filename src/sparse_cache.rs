//! Sparse byte-range cache for on-demand partial file downloads.
//!
//! Backs each open file handle with a tempfile sized to the MTP object's
//! total length. Tracks which byte ranges have been populated. When a FUSE
//! `read(offset, size)` arrives, [`SparseCache::missing_ranges`] tells the
//! caller which ranges still need to be fetched from MTP; after the caller
//! writes the fetched bytes via [`SparseCache::write_at`], [`SparseCache::read_at`]
//! serves the requested slice from the tempfile.
//!
//! Ranges are kept sorted and merged so that adjacent writes coalesce.

use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::ops::Range;

/// A tempfile-backed cache that tracks populated byte ranges.
#[derive(Debug)]
pub struct SparseCache {
    file: std::fs::File,
    /// Sorted, non-overlapping, non-adjacent byte ranges that have been written.
    ranges: Vec<Range<u64>>,
    total_size: u64,
}

impl SparseCache {
    /// Create a new sparse cache for a file of the given total size.
    ///
    /// Allocates a tempfile and sets its length so that sparse reads past the
    /// end don't accidentally return 0 bytes before the caller has fetched them.
    pub fn new(total_size: u64) -> io::Result<Self> {
        let file = tempfile::tempfile()?;
        file.set_len(total_size)?;
        Ok(Self {
            file,
            ranges: Vec::new(),
            total_size,
        })
    }

    /// Total size of the underlying object, as reported by MTP.
    pub fn total_size(&self) -> u64 {
        self.total_size
    }

    /// Returns the byte ranges within `[offset, offset+size)` that are NOT yet populated.
    ///
    /// The returned ranges are sorted and clipped to `[0, total_size)`.
    /// If the entire requested range is already populated, returns an empty `Vec`.
    pub fn missing_ranges(&self, offset: u64, size: u64) -> Vec<Range<u64>> {
        let end = offset.saturating_add(size).min(self.total_size);
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

    /// Write `data` at `offset` and mark `[offset, offset+data.len())` as populated.
    pub fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(data)?;

        let new_range = offset..offset + data.len() as u64;
        self.insert_range(new_range);
        Ok(())
    }

    /// Read `size` bytes at `offset` from the tempfile.
    ///
    /// Callers must ensure the requested range is fully populated (check with
    /// [`missing_ranges`](Self::missing_ranges) and fill gaps via [`write_at`](Self::write_at)).
    /// Reads past `total_size` return a short slice.
    pub fn read_at(&mut self, offset: u64, size: u64) -> io::Result<Vec<u8>> {
        if offset >= self.total_size {
            return Ok(Vec::new());
        }
        let read_len = size.min(self.total_size - offset) as usize;
        let mut buf = vec![0u8; read_len];
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Insert a new range into `self.ranges`, merging with any overlapping or
    /// adjacent existing ranges. Maintains the sorted/merged invariant.
    fn insert_range(&mut self, new: Range<u64>) {
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

        // Find insertion point to keep ranges sorted by start.
        let pos = self
            .ranges
            .binary_search_by(|r| r.start.cmp(&start))
            .unwrap_or_else(|p| p);
        self.ranges.insert(pos, start..end);
    }

    #[cfg(test)]
    pub fn populated_ranges(&self) -> &[Range<u64>] {
        &self.ranges
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_ranges_empty_cache() {
        let cache = SparseCache::new(1000).unwrap();
        assert_eq!(cache.missing_ranges(0, 100), vec![0..100]);
        assert_eq!(cache.missing_ranges(500, 100), vec![500..600]);
    }

    #[test]
    fn missing_ranges_full_hit() {
        let mut cache = SparseCache::new(1000).unwrap();
        cache.write_at(0, &vec![0u8; 500]).unwrap();
        assert_eq!(cache.missing_ranges(100, 200), Vec::<Range<u64>>::new());
        assert_eq!(cache.missing_ranges(0, 500), Vec::<Range<u64>>::new());
    }

    #[test]
    fn missing_ranges_partial_hit_at_start() {
        let mut cache = SparseCache::new(1000).unwrap();
        // Populate [0, 100).
        cache.write_at(0, &vec![0u8; 100]).unwrap();
        // Request [0, 200) — first 100 cached, 100..200 missing.
        assert_eq!(cache.missing_ranges(0, 200), vec![100..200]);
    }

    #[test]
    fn missing_ranges_partial_hit_at_end() {
        let mut cache = SparseCache::new(1000).unwrap();
        // Populate [100, 200).
        cache.write_at(100, &vec![0u8; 100]).unwrap();
        // Request [0, 200) — first 100 missing, last 100 cached.
        assert_eq!(cache.missing_ranges(0, 200), vec![0..100]);
    }

    #[test]
    fn missing_ranges_gap_in_middle() {
        let mut cache = SparseCache::new(1000).unwrap();
        cache.write_at(0, &vec![0u8; 100]).unwrap();
        cache.write_at(200, &vec![0u8; 100]).unwrap();
        // Request [0, 300) — gap at [100, 200).
        assert_eq!(cache.missing_ranges(0, 300), vec![100..200]);
    }

    #[test]
    fn missing_ranges_multiple_gaps() {
        let mut cache = SparseCache::new(1000).unwrap();
        cache.write_at(100, &vec![0u8; 50]).unwrap();
        cache.write_at(300, &vec![0u8; 50]).unwrap();
        // Request [0, 400) — gaps at [0,100), [150,300), [350,400).
        assert_eq!(
            cache.missing_ranges(0, 400),
            vec![0..100, 150..300, 350..400]
        );
    }

    #[test]
    fn missing_ranges_clips_to_total_size() {
        let mut cache = SparseCache::new(500).unwrap();
        // Request extends beyond total_size; should clip.
        assert_eq!(cache.missing_ranges(400, 1000), vec![400..500]);
        cache.write_at(400, &vec![0u8; 100]).unwrap();
        assert_eq!(cache.missing_ranges(400, 1000), Vec::<Range<u64>>::new());
    }

    #[test]
    fn missing_ranges_offset_past_end() {
        let cache = SparseCache::new(100).unwrap();
        assert_eq!(cache.missing_ranges(200, 100), Vec::<Range<u64>>::new());
    }

    #[test]
    fn adjacent_ranges_merge() {
        let mut cache = SparseCache::new(1000).unwrap();
        cache.write_at(0, &vec![0u8; 100]).unwrap();
        cache.write_at(100, &vec![0u8; 100]).unwrap();
        assert_eq!(cache.populated_ranges(), &[0..200]);
    }

    #[test]
    fn overlapping_ranges_merge() {
        let mut cache = SparseCache::new(1000).unwrap();
        cache.write_at(0, &vec![0u8; 100]).unwrap();
        cache.write_at(50, &vec![0u8; 100]).unwrap();
        assert_eq!(cache.populated_ranges(), &[0..150]);
    }

    #[test]
    fn disjoint_ranges_preserved() {
        let mut cache = SparseCache::new(1000).unwrap();
        cache.write_at(0, &vec![0u8; 100]).unwrap();
        cache.write_at(500, &vec![0u8; 100]).unwrap();
        assert_eq!(cache.populated_ranges(), &[0..100, 500..600]);
    }

    #[test]
    fn insertion_sorted() {
        let mut cache = SparseCache::new(1000).unwrap();
        cache.write_at(500, &vec![0u8; 100]).unwrap();
        cache.write_at(0, &vec![0u8; 100]).unwrap();
        cache.write_at(300, &vec![0u8; 50]).unwrap();
        assert_eq!(cache.populated_ranges(), &[0..100, 300..350, 500..600]);
    }

    #[test]
    fn write_read_roundtrip() {
        let mut cache = SparseCache::new(1000).unwrap();
        let data: Vec<u8> = (0..200).map(|i| (i % 256) as u8).collect();
        cache.write_at(100, &data).unwrap();
        let read = cache.read_at(100, 200).unwrap();
        assert_eq!(read, data);
    }

    #[test]
    fn read_at_clips_to_total_size() {
        let mut cache = SparseCache::new(150).unwrap();
        cache.write_at(100, &vec![0u8; 50]).unwrap();
        // Request extends past total_size; should return only the available bytes.
        let read = cache.read_at(100, 200).unwrap();
        assert_eq!(read.len(), 50);
    }

    #[test]
    fn read_at_past_end_returns_empty() {
        let cache = SparseCache::new(100).unwrap();
        let read = cache.read_at(200, 50).unwrap();
        assert_eq!(read, Vec::<u8>::new());
    }

    #[test]
    fn total_size_reported() {
        let cache = SparseCache::new(1234).unwrap();
        assert_eq!(cache.total_size(), 1234);
    }

    #[test]
    fn three_way_merge() {
        // Writing a range that bridges two existing ranges should merge all three.
        let mut cache = SparseCache::new(1000).unwrap();
        cache.write_at(0, &vec![0u8; 100]).unwrap();
        cache.write_at(200, &vec![0u8; 100]).unwrap();
        cache.write_at(100, &vec![0u8; 100]).unwrap();
        assert_eq!(cache.populated_ranges(), &[0..300]);
    }
}
