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

use std::future::Future;
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::ops::Range;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};

use bytes::Bytes;

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
    /// Allocates an unlinked temp file in `spool_dir` (see [`crate::spool`]; the
    /// cache holds every byte read so far, so it must not land in a tmpfs) and
    /// sets its length so that sparse reads past the end don't accidentally
    /// return 0 bytes before the caller has fetched them.
    pub fn new(total_size: u64, spool_dir: &Path) -> io::Result<Self> {
        let file = tempfile::tempfile_in(spool_dir)?;
        // Costs no disk until something is written into it, because the spool
        // sits on a sparse-file filesystem (APFS, ext4, xfs, btrfs).
        file.set_len(total_size)?;
        Ok(Self {
            file,
            ranges: Vec::new(),
            total_size,
        })
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
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "cache range overflow"))?;
        if end > self.total_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "cache write [{offset}, {end}) exceeds the object size {}",
                    self.total_size
                ),
            ));
        }
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(data)?;

        let new_range = offset..end;
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

/// Why a sequential full-object fill stopped before every byte arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillFailure {
    message: String,
    link_lost: bool,
}

impl FillFailure {
    pub fn new(message: impl Into<String>, link_lost: bool) -> Self {
        Self {
            message: message.into(),
            link_lost,
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn is_link_lost(&self) -> bool {
        self.link_lost
    }
}

#[derive(Debug)]
enum FillState {
    Idle,
    Running,
    Complete,
    Failed(FillFailure),
}

#[derive(Debug)]
struct SharedState {
    cache: SparseCache,
    fill: FillState,
    /// Set when the mount is going away and a running fill should stop early.
    /// The filler reacts by cancelling its MTP transfer, which is the only safe
    /// way to stop one (see [`ObjectStream::cancel`]).
    stop_requested: bool,
}

/// One shared sparse cache plus the state of its optional sequential filler.
///
/// [`SparseCache`] remains the authority for whether bytes are real. The fill
/// state only tells waiters whether more bytes can still arrive. Readers always
/// check the requested range first, so a late stream failure cannot hide bytes
/// that were already written successfully.
#[derive(Debug)]
pub struct SharedSparseCache {
    state: Mutex<SharedState>,
    changed: Condvar,
}

impl SharedSparseCache {
    pub fn new(total_size: u64, spool_dir: &Path) -> io::Result<Self> {
        Ok(Self {
            state: Mutex::new(SharedState {
                cache: SparseCache::new(total_size, spool_dir)?,
                fill: FillState::Idle,
                stop_requested: false,
            }),
            changed: Condvar::new(),
        })
    }

    pub fn total_size(&self) -> u64 {
        self.state.lock().unwrap().cache.total_size
    }

    /// Claim the single sequential fill slot for this cache generation.
    pub fn start_fill(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if !matches!(state.fill, FillState::Idle) {
            return false;
        }
        state.fill = FillState::Running;
        true
    }

    /// Whether a sequential fill is in flight right now.
    ///
    /// A cache whose filler is still running has to outlive the last `close()`
    /// on the file: the stream cannot be abandoned, and the bytes it is still
    /// writing are the ones a reopen would otherwise re-download from zero.
    pub fn is_filling(&self) -> bool {
        matches!(self.state.lock().unwrap().fill, FillState::Running)
    }

    /// Ask a running fill to stop at its next chunk boundary.
    ///
    /// This is for the mount going away, not for readers: a seek or a `close()`
    /// must never reach it, or a healthy transfer would restart from byte zero.
    pub fn request_stop(&self) {
        let mut state = self.state.lock().unwrap();
        state.stop_requested = true;
        drop(state);
        self.changed.notify_all();
    }

    /// Whether [`request_stop`](Self::request_stop) has been called.
    pub fn stop_requested(&self) -> bool {
        self.state.lock().unwrap().stop_requested
    }

    /// Make a failed link-loss fill eligible for one new-session retry.
    ///
    /// The caller performs session recovery first. Seeks and reader lifecycle
    /// never call this, so they cannot restart a healthy in-flight stream.
    pub fn reset_after_link_loss(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if matches!(&state.fill, FillState::Failed(failure) if failure.is_link_lost()) {
            state.fill = FillState::Idle;
            true
        } else {
            false
        }
    }

    /// Write the next sequential chunk and wake every waiter.
    pub fn write_sequential(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if !matches!(state.fill, FillState::Running) {
            return Err(io::Error::other("sequential fill is not running"));
        }
        state.cache.write_at(offset, data)?;
        drop(state);
        self.changed.notify_all();
        Ok(())
    }

    /// Finish the fill, rejecting a stream that ended before the advertised size.
    pub fn finish(&self, received: u64) {
        let mut state = self.state.lock().unwrap();
        let expected = state.cache.total_size;
        state.fill = if received == expected {
            FillState::Complete
        } else {
            FillState::Failed(FillFailure::new(
                format!("full-object stream ended after {received} bytes; expected {expected}"),
                false,
            ))
        };
        drop(state);
        self.changed.notify_all();
    }

    pub fn fail(&self, failure: FillFailure) {
        let mut state = self.state.lock().unwrap();
        state.fill = FillState::Failed(failure);
        drop(state);
        self.changed.notify_all();
    }

    /// Block until the exact requested range is populated or can no longer arrive.
    pub fn wait_and_read(&self, offset: u64, size: u64) -> Result<Vec<u8>, FillFailure> {
        let mut state = self.state.lock().unwrap();
        loop {
            if state.cache.missing_ranges(offset, size).is_empty() {
                return state
                    .cache
                    .read_at(offset, size)
                    .map_err(|error| FillFailure::new(error.to_string(), false));
            }
            match &state.fill {
                FillState::Idle => {
                    return Err(FillFailure::new("sequential fill was not started", false));
                }
                FillState::Running => state = self.changed.wait(state).unwrap(),
                FillState::Complete => {
                    return Err(FillFailure::new(
                        "full-object stream completed without the requested range",
                        false,
                    ));
                }
                FillState::Failed(failure) => return Err(failure.clone()),
            }
        }
    }

    pub fn missing_ranges(&self, offset: u64, size: u64) -> Vec<Range<u64>> {
        self.state
            .lock()
            .unwrap()
            .cache
            .missing_ranges(offset, size)
    }

    pub fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.state.lock().unwrap().cache.write_at(offset, data)
    }

    pub fn read_at(&self, offset: u64, size: u64) -> io::Result<Vec<u8>> {
        self.state.lock().unwrap().cache.read_at(offset, size)
    }
}

/// A whole-object download, reduced to what a sequential fill needs.
///
/// The `cancel` half is why this is a trait rather than a plain
/// [`futures::Stream`]: `mtp-rs` marks its `FileDownload` `#[must_use]` because
/// dropping one mid-transfer leaves the responder in the middle of a USB
/// transaction, and on Android that is the failure that needs a physical
/// replug. A fill that has to stop early therefore has to *say so* to the
/// device rather than let the stream fall out of scope.
pub trait ObjectStream {
    /// How this stream reports a transport or protocol failure.
    type Error: std::fmt::Display;

    /// The next chunk, or `None` at end of transfer.
    fn next_chunk(&mut self) -> impl Future<Output = Option<Result<Bytes, Self::Error>>> + Send;

    /// Stop the transfer cleanly, draining whatever the device still owes.
    fn cancel(&mut self) -> impl Future<Output = ()> + Send;
}

/// Why [`fill_from_stream`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillOutcome {
    /// The stream reached end of transfer (whether or not every byte landed).
    Finished,
    /// [`SharedSparseCache::request_stop`] was raised and the transfer was cancelled.
    Stopped,
}

/// Consume a whole-object stream, publishing each chunk only after it has been
/// written successfully into the sparse cache.
///
/// This owns the stream from here to the end of the transfer. Reader seeks, an
/// interrupted `read()`, and `close()` cannot reach it; only
/// [`SharedSparseCache::request_stop`] can, and that cancels rather than drops.
pub async fn fill_from_stream<S, F>(
    cache: Arc<SharedSparseCache>,
    mut stream: S,
    is_link_lost: F,
) -> FillOutcome
where
    S: ObjectStream,
    F: Fn(&S::Error) -> bool,
{
    let mut offset = 0u64;
    let mut write_failure = None;
    let expected = cache.total_size();
    loop {
        if cache.stop_requested() {
            stream.cancel().await;
            cache.fail(FillFailure::new(
                "the whole-object download was stopped because the mount is going away",
                false,
            ));
            return FillOutcome::Stopped;
        }
        let Some(chunk) = stream.next_chunk().await else {
            break;
        };
        let bytes = match chunk {
            Ok(bytes) => bytes,
            Err(error) => {
                cache.fail(FillFailure::new(error.to_string(), is_link_lost(&error)));
                return FillOutcome::Finished;
            }
        };

        if write_failure.is_none() {
            let remaining = usize::try_from(expected.saturating_sub(offset)).unwrap_or(usize::MAX);
            let writable = bytes.len().min(remaining);
            if let Err(error) = cache.write_sequential(offset, &bytes[..writable]) {
                write_failure = Some(FillFailure::new(error.to_string(), false));
            } else if writable != bytes.len() {
                write_failure = Some(FillFailure::new(
                    format!("full-object stream exceeded the advertised size of {expected} bytes"),
                    false,
                ));
            }
        }
        offset = match offset.checked_add(bytes.len() as u64) {
            Some(offset) => offset,
            None => {
                write_failure.get_or_insert_with(|| {
                    FillFailure::new("full-object stream byte count overflowed u64", false)
                });
                u64::MAX
            }
        };
    }
    if let Some(failure) = write_failure {
        cache.fail(failure);
    } else {
        cache.finish(offset);
    }
    FillOutcome::Finished
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)] // intentional: asserting populated_ranges matches a one-range slice
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    /// A scripted [`ObjectStream`] that records what was pulled and whether it
    /// was cancelled.
    struct FakeStream {
        chunks: VecDeque<Result<Bytes, io::Error>>,
        chunks_pulled: Arc<AtomicUsize>,
        cancelled: Arc<AtomicUsize>,
    }

    impl FakeStream {
        fn new(chunks: Vec<Result<Bytes, io::Error>>) -> Self {
            Self {
                chunks: chunks.into(),
                chunks_pulled: Arc::new(AtomicUsize::new(0)),
                cancelled: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl ObjectStream for FakeStream {
        type Error = io::Error;

        async fn next_chunk(&mut self) -> Option<Result<Bytes, io::Error>> {
            let chunk = self.chunks.pop_front();
            if chunk.is_some() {
                self.chunks_pulled.fetch_add(1, Ordering::Relaxed);
            }
            chunk
        }

        async fn cancel(&mut self) {
            self.cancelled.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Unlinked temp files, so the system temp dir is fine for tests; production
    /// resolves a disk-backed spool dir instead.
    fn spool() -> PathBuf {
        std::env::temp_dir()
    }

    #[test]
    fn missing_ranges_empty_cache() {
        let cache = SparseCache::new(1000, &spool()).unwrap();
        assert_eq!(cache.missing_ranges(0, 100), vec![0..100]);
        assert_eq!(cache.missing_ranges(500, 100), vec![500..600]);
    }

    #[test]
    fn missing_ranges_full_hit() {
        let mut cache = SparseCache::new(1000, &spool()).unwrap();
        cache.write_at(0, &[0u8; 500]).unwrap();
        assert_eq!(cache.missing_ranges(100, 200), Vec::<Range<u64>>::new());
        assert_eq!(cache.missing_ranges(0, 500), Vec::<Range<u64>>::new());
    }

    #[test]
    fn missing_ranges_partial_hit_at_start() {
        let mut cache = SparseCache::new(1000, &spool()).unwrap();
        // Populate [0, 100).
        cache.write_at(0, &[0u8; 100]).unwrap();
        // Request [0, 200) — first 100 cached, 100..200 missing.
        assert_eq!(cache.missing_ranges(0, 200), vec![100..200]);
    }

    #[test]
    fn missing_ranges_partial_hit_at_end() {
        let mut cache = SparseCache::new(1000, &spool()).unwrap();
        // Populate [100, 200).
        cache.write_at(100, &[0u8; 100]).unwrap();
        // Request [0, 200) — first 100 missing, last 100 cached.
        assert_eq!(cache.missing_ranges(0, 200), vec![0..100]);
    }

    #[test]
    fn missing_ranges_gap_in_middle() {
        let mut cache = SparseCache::new(1000, &spool()).unwrap();
        cache.write_at(0, &[0u8; 100]).unwrap();
        cache.write_at(200, &[0u8; 100]).unwrap();
        // Request [0, 300) — gap at [100, 200).
        assert_eq!(cache.missing_ranges(0, 300), vec![100..200]);
    }

    #[test]
    fn missing_ranges_multiple_gaps() {
        let mut cache = SparseCache::new(1000, &spool()).unwrap();
        cache.write_at(100, &[0u8; 50]).unwrap();
        cache.write_at(300, &[0u8; 50]).unwrap();
        // Request [0, 400) — gaps at [0,100), [150,300), [350,400).
        assert_eq!(
            cache.missing_ranges(0, 400),
            vec![0..100, 150..300, 350..400]
        );
    }

    #[test]
    fn missing_ranges_clips_to_total_size() {
        let mut cache = SparseCache::new(500, &spool()).unwrap();
        // Request extends beyond total_size; should clip.
        assert_eq!(cache.missing_ranges(400, 1000), vec![400..500]);
        cache.write_at(400, &[0u8; 100]).unwrap();
        assert_eq!(cache.missing_ranges(400, 1000), Vec::<Range<u64>>::new());
    }

    #[test]
    fn missing_ranges_offset_past_end() {
        let cache = SparseCache::new(100, &spool()).unwrap();
        assert_eq!(cache.missing_ranges(200, 100), Vec::<Range<u64>>::new());
    }

    #[test]
    fn adjacent_ranges_merge() {
        let mut cache = SparseCache::new(1000, &spool()).unwrap();
        cache.write_at(0, &[0u8; 100]).unwrap();
        cache.write_at(100, &[0u8; 100]).unwrap();
        assert_eq!(cache.populated_ranges(), &[0..200]);
    }

    #[test]
    fn overlapping_ranges_merge() {
        let mut cache = SparseCache::new(1000, &spool()).unwrap();
        cache.write_at(0, &[0u8; 100]).unwrap();
        cache.write_at(50, &[0u8; 100]).unwrap();
        assert_eq!(cache.populated_ranges(), &[0..150]);
    }

    #[test]
    fn disjoint_ranges_preserved() {
        let mut cache = SparseCache::new(1000, &spool()).unwrap();
        cache.write_at(0, &[0u8; 100]).unwrap();
        cache.write_at(500, &[0u8; 100]).unwrap();
        assert_eq!(cache.populated_ranges(), &[0..100, 500..600]);
    }

    #[test]
    fn insertion_sorted() {
        let mut cache = SparseCache::new(1000, &spool()).unwrap();
        cache.write_at(500, &[0u8; 100]).unwrap();
        cache.write_at(0, &[0u8; 100]).unwrap();
        cache.write_at(300, &[0u8; 50]).unwrap();
        assert_eq!(cache.populated_ranges(), &[0..100, 300..350, 500..600]);
    }

    #[test]
    fn write_read_roundtrip() {
        let mut cache = SparseCache::new(1000, &spool()).unwrap();
        let data: Vec<u8> = (0..200).map(|i| (i % 256) as u8).collect();
        cache.write_at(100, &data).unwrap();
        let read = cache.read_at(100, 200).unwrap();
        assert_eq!(read, data);
    }

    #[test]
    fn read_at_clips_to_total_size() {
        let mut cache = SparseCache::new(150, &spool()).unwrap();
        cache.write_at(100, &[0u8; 50]).unwrap();
        // Request extends past total_size; should return only the available bytes.
        let read = cache.read_at(100, 200).unwrap();
        assert_eq!(read.len(), 50);
    }

    #[test]
    fn read_at_past_end_returns_empty() {
        let mut cache = SparseCache::new(100, &spool()).unwrap();
        let read = cache.read_at(200, 50).unwrap();
        assert_eq!(read, Vec::<u8>::new());
    }

    #[test]
    fn three_way_merge() {
        // Writing a range that bridges two existing ranges should merge all three.
        let mut cache = SparseCache::new(1000, &spool()).unwrap();
        cache.write_at(0, &[0u8; 100]).unwrap();
        cache.write_at(200, &[0u8; 100]).unwrap();
        cache.write_at(100, &[0u8; 100]).unwrap();
        assert_eq!(cache.populated_ranges(), &[0..300]);
    }

    #[test]
    fn write_past_advertised_size_is_rejected_without_marking_bytes() {
        let mut cache = SparseCache::new(4, &spool()).unwrap();
        let error = cache.write_at(2, b"abc").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(cache.missing_ranges(0, 4), vec![0..4]);
    }

    #[tokio::test]
    async fn normal_stream_populates_and_completes_the_cache() {
        let cache = Arc::new(SharedSparseCache::new(6, &spool()).unwrap());
        assert!(cache.start_fill());
        fill_from_stream(
            Arc::clone(&cache),
            FakeStream::new(vec![
                Ok(Bytes::from_static(b"abc")),
                Ok(Bytes::from_static(b"def")),
            ]),
            |_| false,
        )
        .await;

        assert_eq!(cache.wait_and_read(0, 6).unwrap(), b"abcdef");
        assert!(!cache.start_fill(), "a completed cache gets no second fill");
    }

    #[tokio::test]
    async fn truncated_stream_keeps_its_prefix_but_rejects_the_missing_tail() {
        let cache = Arc::new(SharedSparseCache::new(6, &spool()).unwrap());
        assert!(cache.start_fill());
        fill_from_stream(
            Arc::clone(&cache),
            FakeStream::new(vec![Ok(Bytes::from_static(b"abc"))]),
            |_| false,
        )
        .await;

        assert_eq!(cache.wait_and_read(0, 3).unwrap(), b"abc");
        let failure = cache.wait_and_read(3, 3).unwrap_err();
        assert!(failure.message().contains("ended after 3 bytes"));
    }

    #[tokio::test]
    async fn stream_failure_after_a_range_lands_does_not_hide_that_range() {
        let cache = Arc::new(SharedSparseCache::new(6, &spool()).unwrap());
        assert!(cache.start_fill());
        fill_from_stream(
            Arc::clone(&cache),
            FakeStream::new(vec![
                Ok(Bytes::from_static(b"abc")),
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "device left")),
            ]),
            |error| error.kind() == io::ErrorKind::BrokenPipe,
        )
        .await;

        assert_eq!(cache.wait_and_read(0, 3).unwrap(), b"abc");
        let failure = cache.wait_and_read(3, 3).unwrap_err();
        assert!(failure.is_link_lost());
        assert_eq!(failure.message(), "device left");
    }

    #[tokio::test]
    async fn stream_failure_before_a_range_arrives_returns_no_hole_bytes() {
        let cache = Arc::new(SharedSparseCache::new(6, &spool()).unwrap());
        assert!(cache.start_fill());
        fill_from_stream(
            Arc::clone(&cache),
            FakeStream::new(vec![Err(io::Error::other("stream failed"))]),
            |_| false,
        )
        .await;

        assert_eq!(cache.missing_ranges(0, 3), vec![0..3]);
        assert_eq!(
            cache.wait_and_read(0, 3).unwrap_err().message(),
            "stream failed"
        );
    }

    #[tokio::test]
    async fn local_write_failure_still_drains_the_stream_to_eof() {
        let cache = Arc::new(SharedSparseCache::new(3, &spool()).unwrap());
        assert!(cache.start_fill());
        let stream = FakeStream::new(vec![
            Ok(Bytes::from_static(b"abc")),
            Ok(Bytes::from_static(b"de")),
            Ok(Bytes::from_static(b"fg")),
        ]);
        let chunks_seen = Arc::clone(&stream.chunks_pulled);
        let cancelled = Arc::clone(&stream.cancelled);

        fill_from_stream(Arc::clone(&cache), stream, |_| false).await;

        assert_eq!(chunks_seen.load(Ordering::Relaxed), 3);
        assert_eq!(
            cancelled.load(Ordering::Relaxed),
            0,
            "a local write failure drains the transfer rather than cancelling it"
        );
        assert_eq!(cache.wait_and_read(0, 3).unwrap(), b"abc");
        assert!(!cache.start_fill(), "a failed fill cannot silently restart");
    }

    #[test]
    fn a_reader_waits_until_its_entire_range_is_valid() {
        let cache = Arc::new(SharedSparseCache::new(4, &spool()).unwrap());
        assert!(cache.start_fill());
        let (tx, rx) = mpsc::channel();
        let reader = Arc::clone(&cache);
        thread::spawn(move || tx.send(reader.wait_and_read(0, 4)).unwrap());

        cache.write_sequential(0, b"ab").unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        cache.write_sequential(2, b"cd").unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap().unwrap(),
            b"abcd"
        );
        cache.finish(4);
    }

    #[test]
    fn overlapping_and_far_forward_readers_wake_as_their_ranges_land() {
        let cache = Arc::new(SharedSparseCache::new(16, &spool()).unwrap());
        assert!(cache.start_fill());
        let (near_tx, near_rx) = mpsc::channel();
        let (far_tx, far_rx) = mpsc::channel();
        let near = Arc::clone(&cache);
        let far = Arc::clone(&cache);
        thread::spawn(move || near_tx.send(near.wait_and_read(4, 4)).unwrap());
        thread::spawn(move || far_tx.send(far.wait_and_read(10, 4)).unwrap());

        cache.write_sequential(0, b"abcdefgh").unwrap();
        assert_eq!(
            near_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap(),
            b"efgh"
        );
        assert!(far_rx.recv_timeout(Duration::from_millis(50)).is_err());
        cache.write_sequential(8, b"ijklmnop").unwrap();
        assert_eq!(
            far_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap(),
            b"klmn"
        );
        cache.finish(16);
    }

    #[test]
    fn repeated_reads_reuse_bytes_and_eof_never_starts_a_fill() {
        let cache = SharedSparseCache::new(4, &spool()).unwrap();
        assert!(cache.start_fill());
        cache.write_sequential(0, b"data").unwrap();
        cache.finish(4);
        assert_eq!(cache.wait_and_read(1, 2).unwrap(), b"at");
        assert_eq!(cache.wait_and_read(1, 2).unwrap(), b"at");

        let eof = SharedSparseCache::new(4, &spool()).unwrap();
        assert_eq!(eof.wait_and_read(4, 100).unwrap(), Vec::<u8>::new());
    }

    #[tokio::test]
    async fn a_stop_request_cancels_the_transfer_instead_of_dropping_it() {
        let cache = Arc::new(SharedSparseCache::new(6, &spool()).unwrap());
        assert!(cache.start_fill());
        let stream = FakeStream::new(vec![
            Ok(Bytes::from_static(b"abc")),
            Ok(Bytes::from_static(b"def")),
        ]);
        let cancelled = Arc::clone(&stream.cancelled);
        let chunks_pulled = Arc::clone(&stream.chunks_pulled);
        cache.request_stop();

        let outcome = fill_from_stream(Arc::clone(&cache), stream, |_| false).await;

        assert_eq!(outcome, FillOutcome::Stopped);
        assert_eq!(cancelled.load(Ordering::Relaxed), 1);
        assert_eq!(
            chunks_pulled.load(Ordering::Relaxed),
            0,
            "the stop lands before the next chunk, not after the rest of the file"
        );
        // A reader blocked on the stopped fill is released with a reason, never
        // with bytes the device did not send.
        assert!(cache
            .wait_and_read(0, 6)
            .unwrap_err()
            .message()
            .contains("going away"));
    }

    #[tokio::test]
    async fn a_stop_mid_transfer_keeps_the_bytes_that_already_landed() {
        let cache = Arc::new(SharedSparseCache::new(6, &spool()).unwrap());
        assert!(cache.start_fill());
        cache.write_sequential(0, b"abc").unwrap();
        cache.request_stop();

        let stream = FakeStream::new(vec![Ok(Bytes::from_static(b"def"))]);
        let cancelled = Arc::clone(&stream.cancelled);
        fill_from_stream(Arc::clone(&cache), stream, |_| false).await;

        assert_eq!(cancelled.load(Ordering::Relaxed), 1);
        assert_eq!(cache.wait_and_read(0, 3).unwrap(), b"abc");
        assert!(cache.wait_and_read(3, 3).is_err());
    }

    #[test]
    fn a_running_fill_reports_itself_so_its_cache_outlives_the_last_close() {
        let cache = SharedSparseCache::new(4, &spool()).unwrap();
        assert!(!cache.is_filling());
        assert!(cache.start_fill());
        assert!(cache.is_filling());
        cache.finish(4);
        assert!(!cache.is_filling());
    }

    #[test]
    fn only_link_loss_can_reset_a_failed_fill() {
        let cache = SharedSparseCache::new(4, &spool()).unwrap();
        assert!(cache.start_fill());
        cache.fail(FillFailure::new("protocol error", false));
        assert!(!cache.reset_after_link_loss());

        let cache = SharedSparseCache::new(4, &spool()).unwrap();
        assert!(cache.start_fill());
        cache.fail(FillFailure::new("device left", true));
        assert!(cache.reset_after_link_loss());
        assert!(cache.start_fill());
        assert!(!cache.start_fill(), "the retry still owns exactly one slot");
    }
}
