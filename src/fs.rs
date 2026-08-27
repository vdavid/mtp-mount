use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::io::{Seek, SeekFrom};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use fuser::{
    BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, KernelConfig, LockOwner, MountOption, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
    ReplyWrite, Request, TimeOrNow, WriteFlags,
};
use log::{debug, error, info, warn};
use mtp_rs::mtp::{DeviceEvent, MtpDevice};
use mtp_rs::{ByteRange, NewObjectInfo, ObjectHandle, Storage};

use crate::buffer::WriteBuffer;
use crate::device::{is_link_lost, DeviceOpener, UnplugSwitch};
use crate::fill::FillTracker;
use crate::inode::{ChildInfo, InodeEntry, InodeKind, InodeTable, FUSE_ROOT_INODE};
use crate::reconnect::ReconnectPolicy;
use crate::shutdown::Shutdown;
use crate::sparse_cache::{fill_from_stream, FillFailure, ObjectStream, SharedSparseCache};

const TTL: Duration = Duration::from_secs(1);

/// How much of a spool file an upload holds in memory at a time.
const UPLOAD_CHUNK: usize = 65536;

/// Default ceiling on the whole-object fallback: the largest object a responder
/// with no partial-read operation will be read for without being asked twice.
///
/// The bound is not about what the device can do, it's about what a background
/// process can do to you. A fill holds the device's one session for the entire
/// transfer, and with `fuser`'s single event-loop thread every other process on
/// the mount waits behind it, so a thumbnailer that opens a 30 GB file freezes
/// the mount for a quarter of an hour. No value avoids that (1 GiB over a 20
/// MiB/s link is already ~50 seconds), so this caps the damage rather than
/// preventing it, and `--full-download-limit` is how someone who *means* to copy
/// a big file lifts it. Same shape as [`ReconnectPolicy::DEFAULT_TIMEOUT_SECS`]:
/// conservative by default, opt in when you know what you're waiting for.
pub const DEFAULT_FULL_DOWNLOAD_LIMIT: u64 = 4 * 1024 * 1024 * 1024;

/// How a read gets its bytes, decided per object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadStrategy {
    /// The responder has a partial-read op: fetch just the missing ranges.
    Ranged,
    /// No partial-read op: one whole-object stream fills the cache as it lands.
    SequentialFull,
    /// No partial-read op and the object is over the limit, so reading it would
    /// mean an unbounded hold on the session that nobody asked for.
    TooLargeForSequentialFull,
}

/// Pick the read path. `limit` of `0` means no ceiling.
fn read_strategy(supports_partial_download: bool, object_size: u64, limit: u64) -> ReadStrategy {
    if supports_partial_download {
        ReadStrategy::Ranged
    } else if limit == 0 || object_size <= limit {
        ReadStrategy::SequentialFull
    } else {
        ReadStrategy::TooLargeForSequentialFull
    }
}

/// How many times an operation is retried across reconnects before it gives up.
/// One reconnect is the cable glitch we're here for; a second is a device that
/// keeps dropping mid-operation, and past that the retry is not the answer.
const MAX_ATTEMPTS: u32 = 3;

/// How many times an operation re-resolves its handles and tries again after
/// the device says a handle is stale.
///
/// One, which is what `mtp-rs` prescribes: the first `StaleHandle` means the
/// device re-keyed the object and a fresh listing has the new token, so the
/// retry works. A second one for the same operation means the re-resolved token
/// died too, which isn't a re-key any more; looping on it would hammer the
/// device instead of telling the caller.
///
/// This budget is deliberately separate from [`MAX_ATTEMPTS`]: the two failures
/// have nothing in common. A stale handle costs one listing against a healthy
/// session, a dead link costs a reopen and a backoff wait, so spending one
/// shouldn't shorten the other, and a stale handle must never fall through into
/// the reconnect path.
const MAX_STALE_RETRIES: u32 = 1;

type MtpResult<T> = Result<T, mtp_rs::Error>;

/// How long a cancel waits for the device to stop talking before giving up.
/// `mtp-rs` drains the pipe over this window; it is a round-trip, not a
/// transfer, so it stays short even for a 30 GB object.
const CANCEL_DRAIN_TIMEOUT: Duration = Duration::from_secs(3);

/// `mtp-rs`'s whole-object download, as the sequential fill sees it.
struct FullObjectStream(mtp_rs::FileDownload);

impl ObjectStream for FullObjectStream {
    type Error = mtp_rs::Error;

    async fn next_chunk(&mut self) -> Option<Result<bytes::Bytes, mtp_rs::Error>> {
        self.0.next_chunk().await
    }

    async fn cancel(&mut self) {
        // A cancel that fails has still done the useful part (the request went
        // out); there is nothing better left to try, and the mount is on its way
        // out anyway. Say so and move on rather than block the unmount.
        if let Err(error) = self.0.cancel(CANCEL_DRAIN_TIMEOUT).await {
            warn!("Cancelling the whole-object download did not complete cleanly: {error}");
        }
    }
}

/// Everything the mount needs beyond the device itself.
pub struct MtpFsConfig {
    pub read_only: bool,
    /// Disk-backed directory for write buffers and read caches (see [`crate::spool`]).
    /// Must exist and be writable; resolve it with [`crate::spool::spool_dir_from_env`]
    /// and [`crate::spool::prepare_spool_dir`].
    pub spool_dir: PathBuf,
    /// How long to wait for a device that went away.
    pub reconnect: ReconnectPolicy,
    /// Ceiling on the whole-object read fallback, in bytes. `0` lifts it.
    /// See [`DEFAULT_FULL_DOWNLOAD_LIMIT`].
    pub full_download_limit: u64,
    /// The pretend cable, shared with the [`DeviceOpener`] (tests only).
    pub unplug: UnplugSwitch,
}

fn mtp_datetime_to_system_time(dt: &mtp_rs::DateTime) -> SystemTime {
    fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = (y - era * 400) as u64;
        let m_adj = if m > 2 { m - 3 } else { m + 9 } as u64;
        let doy = (153 * m_adj + 2) / 5 + d as u64 - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146097 + doe as i64 - 719468
    }

    let days = days_from_civil(dt.year as i64, dt.month as i64, dt.day as i64);
    let secs = days * 86400 + dt.hour as i64 * 3600 + dt.minute as i64 * 60 + dt.second as i64;
    if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH
    }
}

fn inode_to_file_attr(entry: &InodeEntry) -> FileAttr {
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    FileAttr {
        ino: INodeNo(entry.inode),
        size: entry.size,
        blocks: entry.size.div_ceil(512),
        atime: entry.atime,
        mtime: entry.mtime,
        ctime: entry.mtime,
        crtime: entry.mtime,
        kind: if entry.is_dir() {
            FileType::Directory
        } else {
            FileType::RegularFile
        },
        perm: if entry.is_dir() { 0o755 } else { 0o644 },
        nlink: if entry.is_dir() { 2 } else { 1 },
        uid,
        gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

/// The name a storage shows up under in the mount.
fn storage_name(storage: &Storage) -> String {
    if storage.info().description.is_empty() {
        format!("Storage_{}", storage.id().0)
    } else {
        storage.info().description.clone()
    }
}

/// The temporary name a safe flush uploads under before renaming into place.
fn temp_upload_name(name: &str) -> String {
    format!(".~tmp~{name}")
}

/// Wraps a local I/O failure as an MTP error so spool problems flow through the
/// same result type as device problems.
fn io_error(e: io::Error) -> mtp_rs::Error {
    mtp_rs::Error::Io {
        message: e.to_string(),
    }
}

/// Helper to create an `Unpin` stream from a `Vec<u8>`.
fn bytes_stream(
    data: Vec<u8>,
) -> futures::stream::Iter<std::vec::IntoIter<Result<Bytes, io::Error>>> {
    let chunks = if data.is_empty() {
        vec![Ok(Bytes::new())]
    } else {
        vec![Ok(Bytes::from(data))]
    };
    futures::stream::iter(chunks)
}

/// Streams a spool file in [`UPLOAD_CHUNK`]-sized pieces, reading each one only
/// when the consumer asks for it. That's what keeps an upload's memory flat: a
/// 4 GB file costs one chunk of RAM, not 4 GB.
///
/// The read is a blocking `std::fs` read, which is fine because the only caller
/// polls this from `Handle::block_on` on a FUSE callback thread that has nothing
/// else to do. Don't spawn this stream onto the runtime: it would park a worker.
fn file_stream(
    file: std::fs::File,
) -> Pin<Box<dyn futures::Stream<Item = Result<Bytes, io::Error>> + Send>> {
    use std::io::Read as _;
    // The `Option` is the terminator. Handing the file back only after a
    // successful read means EOF and errors both end the stream; a version that
    // kept the file after an error would re-emit that same error forever.
    Box::pin(futures::stream::unfold(Some(file), |state| async move {
        let mut file = state?;
        let mut buf = vec![0u8; UPLOAD_CHUNK];
        match file.read(&mut buf) {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(Bytes::from(buf)), Some(file)))
            }
            Err(e) => Some((Err(e), None)),
        }
    }))
}

/// Mutable state protected by `RefCell` so fuser's `&self` callbacks can mutate it.
struct Inner {
    storages: Vec<Storage>,
    /// Disk-backed directory for write buffers and read caches (see [`crate::spool`]).
    spool_dir: PathBuf,
    inodes: InodeTable,
    write_buf: WriteBuffer,
    /// One sparse cache per *open object*, keyed by inode rather than by file
    /// handle: two file descriptors on the same file share the bytes, and on the
    /// whole-object path they share the single download instead of racing two.
    read_cache: HashMap<u64, Arc<SharedSparseCache>>,
    dirs_loaded: HashMap<u64, bool>,
    fh_to_inode: HashMap<u64, u64>,
}

/// FUSE filesystem backed by an MTP device.
pub struct MtpFs {
    rt: tokio::runtime::Handle,
    device: Mutex<MtpDevice>,
    /// How to reopen the same device after it goes away.
    opener: Arc<dyn DeviceOpener>,
    policy: ReconnectPolicy,
    unplug: UnplugSwitch,
    /// Raised when the device is gone for good, so whoever owns the mount takes
    /// it down instead of leaving something that answers every call with EIO.
    shutdown: Arc<Shutdown>,
    /// Bumped on every reconnect so the event loop from the previous session
    /// notices it's been superseded and exits.
    event_epoch: Arc<AtomicU64>,
    inner: Arc<Mutex<Inner>>,
    next_fh: AtomicU64,
    read_only: bool,
    /// Counter incremented on every MTP partial-read fetch. Used by integration
    /// tests to verify that the sparse cache prevents redundant fetches.
    fetch_counter: Arc<AtomicU64>,
    /// Counter incremented when a no-partial responder starts a full-object
    /// filler. Used to pin the capability split in tests.
    full_fill_counter: Arc<AtomicU64>,
    /// Ceiling on the whole-object fallback, in bytes (`0` lifts it).
    full_download_limit: u64,
    /// The whole-object downloads in flight, so teardown can cancel them
    /// instead of dropping them. See [`crate::fill`].
    fills: Arc<FillTracker>,
}

impl MtpFs {
    /// Builds a filesystem over an already-open device.
    ///
    /// `opener` must resolve to that same device; it's what the mount uses to
    /// come back after a disconnect.
    pub fn new(
        device: MtpDevice,
        opener: Arc<dyn DeviceOpener>,
        rt: tokio::runtime::Handle,
        config: MtpFsConfig,
    ) -> Self {
        let MtpFsConfig {
            read_only,
            spool_dir,
            reconnect,
            full_download_limit,
            unplug,
        } = config;
        Self {
            rt,
            device: Mutex::new(device),
            opener,
            policy: reconnect,
            unplug,
            shutdown: Arc::new(Shutdown::default()),
            event_epoch: Arc::new(AtomicU64::new(0)),
            inner: Arc::new(Mutex::new(Inner {
                storages: Vec::new(),
                spool_dir: spool_dir.clone(),
                inodes: InodeTable::new(),
                write_buf: WriteBuffer::new(spool_dir),
                read_cache: HashMap::new(),
                dirs_loaded: HashMap::new(),
                fh_to_inode: HashMap::new(),
            })),
            next_fh: AtomicU64::new(1),
            read_only,
            fetch_counter: Arc::new(AtomicU64::new(0)),
            full_fill_counter: Arc::new(AtomicU64::new(0)),
            full_download_limit,
            fills: Arc::new(FillTracker::default()),
        }
    }

    /// The signal that asks for this mount to be taken down.
    ///
    /// Whoever mounted the filesystem owns the unmount handle, so it has to
    /// watch this and unmount when a reason shows up. See [`crate::shutdown`].
    pub fn shutdown(&self) -> Arc<Shutdown> {
        Arc::clone(&self.shutdown)
    }

    /// Returns a shared handle to the MTP fetch counter.
    ///
    /// The counter increments each time a partial-read operation is issued to
    /// the device. Primarily used by integration tests to verify cache behavior.
    #[allow(dead_code)] // used by integration tests via lib.rs, not by the bin
    pub fn fetch_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.fetch_counter)
    }

    #[allow(dead_code)] // used by integration tests via lib.rs, not by the bin
    pub fn full_fill_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.full_fill_counter)
    }

    /// Drop an object's read cache once nothing holds the file open.
    ///
    /// A cache whose whole-object fill is still running stays: the transfer must
    /// not be abandoned, and a reopen while it runs joins it instead of starting
    /// a second download of the same object. That filler drops the entry itself
    /// when it finishes. A cache with no fill running (every ranged read, and a
    /// finished fill) goes right away, so reopening a file re-reads it from the
    /// device rather than serving bytes of unknown age.
    fn drop_read_cache_if_unused(inner: &mut Inner, inode: u64) {
        if inner.fh_to_inode.values().any(|&open| open == inode) {
            return;
        }
        if inner
            .read_cache
            .get(&inode)
            .is_some_and(|cache| cache.is_filling())
        {
            return;
        }
        inner.read_cache.remove(&inode);
    }

    /// How an object of this size would be read on the device we're attached to.
    fn read_strategy_for(&self, object_size: u64) -> ReadStrategy {
        let supports_partial_download = self
            .device
            .lock()
            .unwrap()
            .capabilities()
            .supports_partial_download;
        read_strategy(
            supports_partial_download,
            object_size,
            self.full_download_limit,
        )
    }

    /// The mount's in-flight whole-object downloads.
    ///
    /// Whoever owns the mount takes this *before* handing the filesystem to
    /// `fuser` and calls [`FillTracker::stop_and_wait`] after unmounting, so a
    /// transfer is cancelled rather than dropped when the process goes away.
    pub fn fills(&self) -> Arc<FillTracker> {
        Arc::clone(&self.fills)
    }

    fn alloc_fh(&self) -> u64 {
        self.next_fh.fetch_add(1, Ordering::Relaxed)
    }

    /// Find the storage index that owns a given inode by walking up the tree.
    fn find_storage_index(inner: &Inner, inode: u64) -> Option<usize> {
        let mut current = inode;
        loop {
            let entry = inner.inodes.get(current)?;
            if let InodeKind::Storage { storage_id } = &entry.kind {
                return inner.storages.iter().position(|s| s.id() == *storage_id);
            }
            if current == entry.parent {
                return None;
            }
            current = entry.parent;
        }
    }

    /// Runs an MTP operation, riding out the two ways its handles can die under
    /// it: the device went away, or the device re-keyed the object.
    ///
    /// `attempt` is re-run from scratch after either recovery, so it must
    /// resolve its own handles (through [`Self::file_handle`] and friends) rather
    /// than close over handles from the previous try.
    ///
    /// The two paths are siblings, not variations. A dead link needs a reopen
    /// and a wait; a stale handle needs neither, because the session is fine and
    /// only the token is dead, so it re-resolves by path and retries straight
    /// away. Reopening a healthy device would be a real regression: on Android
    /// a reopen is expensive and can wedge the device.
    fn with_recovery<T>(
        &self,
        inner: &mut Inner,
        mut attempt: impl FnMut(&Self, &mut Inner) -> MtpResult<T>,
    ) -> MtpResult<T> {
        for _ in 0..MAX_ATTEMPTS {
            // Stale-handle retries happen against the current session, on their
            // own budget, and never fall through to the reconnect below.
            let mut stale_retries = MAX_STALE_RETRIES;
            loop {
                if self.unplug.is_unplugged() {
                    break;
                }
                match attempt(self, inner) {
                    Ok(value) => return Ok(value),
                    Err(e) if e.is_stale_handle() => {
                        if stale_retries == 0 {
                            error!("Operation still hit a stale object handle after re-resolving");
                            return Err(e);
                        }
                        stale_retries -= 1;
                        debug!("Operation hit a stale object handle, re-resolving by path");
                        Self::invalidate_handles(inner);
                    }
                    Err(e) if !is_link_lost(&e) => return Err(e),
                    Err(e) => {
                        debug!("Operation hit a dead session: {e}");
                        break;
                    }
                }
            }
            self.reconnect(inner)?;
        }
        Err(mtp_rs::Error::Disconnected)
    }

    /// Marks every cached object handle stale so the next use re-resolves it by
    /// path, and drops the cached listings that produced them.
    ///
    /// Whole-table rather than just the inode that failed, for two reasons. A
    /// device that re-keys re-keys in batches (Android's MediaProvider does it
    /// across a whole media rescan), so the neighbours are suspect too. And the
    /// generation counter makes marking free: re-resolution is lazy, so the only
    /// inodes that pay for a listing are the ones something actually touches
    /// again. This is the same mechanism a reconnect uses, minus the reopen.
    fn invalidate_handles(inner: &mut Inner) {
        inner.inodes.bump_generation();
        inner.dirs_loaded.clear();
        inner.dirs_loaded.insert(FUSE_ROOT_INODE, true);
    }

    /// Waits for the device to come back and rebuilds the session on top of the
    /// existing inode tree. Gives up (and unmounts) when the window runs out.
    fn reconnect(&self, inner: &mut Inner) -> MtpResult<()> {
        let who = self.opener.describe();

        if self.policy.is_disabled() {
            self.give_up(&format!(
                "{who} disconnected and reconnect is off (--reconnect-timeout 0)"
            ));
            return Err(mtp_rs::Error::Disconnected);
        }

        let secs = self.policy.timeout().as_secs();
        info!("{who} disconnected, waiting up to {secs}s for it to come back...");
        eprintln!("mtp-mount: {who} disconnected, waiting up to {secs}s for it to come back...");

        for delay in self.policy.schedule() {
            std::thread::sleep(delay);
            if self.unplug.is_unplugged() {
                continue;
            }
            let device = match self.opener.open(&self.rt) {
                Ok(device) => device,
                Err(e) => {
                    debug!("Reconnect attempt failed: {e}");
                    continue;
                }
            };
            match self.adopt(inner, device) {
                Ok(()) => {
                    eprintln!("mtp-mount: {who} is back, carrying on.");
                    return Ok(());
                }
                Err(e) => {
                    warn!("Reopened {who} but couldn't resume the mount: {e}");
                    continue;
                }
            }
        }

        self.give_up(&format!("{who} didn't come back within {secs}s"));
        Err(mtp_rs::Error::Disconnected)
    }

    /// Takes over a freshly opened device: re-maps storage IDs, marks every
    /// cached object handle stale, and starts a new event loop.
    ///
    /// Inode numbers, names, the tree shape, open file handles, read caches, and
    /// write spools all survive untouched. Only the session-scoped tokens change.
    fn adopt(&self, inner: &mut Inner, device: MtpDevice) -> MtpResult<()> {
        let storages = self.rt.block_on(device.storages())?;

        // Storage IDs are session-scoped too. Match the new storages to the
        // storage inodes by name, falling back to position when a device
        // reports no description (the old name embeds the old ID).
        let storage_inodes = inner.inodes.children(FUSE_ROOT_INODE);
        for (position, storage_ino) in storage_inodes.iter().enumerate() {
            let name = match inner.inodes.get(*storage_ino) {
                Some(entry) => entry.name.clone(),
                None => continue,
            };
            let matched = storages
                .iter()
                .find(|s| storage_name(s) == name)
                .or_else(|| storages.get(position));
            match matched {
                Some(storage) => inner.inodes.set_storage_id(*storage_ino, storage.id()),
                None => warn!("Storage '{name}' is missing after the reconnect"),
            }
        }

        inner.storages = storages;
        *self.device.lock().unwrap() = device.clone();
        inner.inodes.bump_generation();

        // Names and sizes are re-read on the next access; the handles behind
        // them are re-resolved lazily by path.
        inner.dirs_loaded.clear();
        inner.dirs_loaded.insert(FUSE_ROOT_INODE, true);

        let epoch = self.event_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        self.spawn_event_loop(device, epoch);
        Ok(())
    }

    /// Says why the mount is going away and asks for it to be taken down. The
    /// operation that triggered this still returns an error to its caller.
    fn give_up(&self, reason: &str) {
        error!("{reason}; unmounting");
        eprintln!("mtp-mount: {reason}. Unmounting.");
        self.shutdown.request(reason);
    }

    /// Re-resolves an inode's MTP handle by path if it came from an older
    /// session, walking down from the storage root and refreshing each ancestor
    /// on the way. Inode numbers never change here.
    fn ensure_fresh(&self, inner: &mut Inner, inode: u64) -> MtpResult<()> {
        if inner.inodes.is_fresh(inode) {
            return Ok(());
        }

        // Collect the chain from the storage root down to this inode.
        let mut chain = Vec::new();
        let mut current = inode;
        loop {
            let entry = inner.inodes.get(current).ok_or(mtp_rs::Error::NotFound)?;
            match entry.kind {
                InodeKind::Root | InodeKind::Storage { .. } => break,
                _ => {
                    chain.push(current);
                    if current == entry.parent {
                        return Err(mtp_rs::Error::NotFound);
                    }
                    current = entry.parent;
                }
            }
        }
        chain.reverse();

        let storage_idx = Self::find_storage_index(inner, inode).ok_or(mtp_rs::Error::NotFound)?;

        let mut parent_handle: Option<ObjectHandle> = None;
        for node in chain {
            let entry = inner.inodes.get(node).ok_or(mtp_rs::Error::NotFound)?;
            let name = entry.name.clone();
            if inner.inodes.is_fresh(node) {
                parent_handle = match inner.inodes.get(node).map(|e| &e.kind) {
                    Some(InodeKind::Directory { handle } | InodeKind::File { handle }) => {
                        Some(*handle)
                    }
                    _ => return Err(mtp_rs::Error::NotFound),
                };
                continue;
            }

            let objects = self
                .rt
                .block_on(inner.storages[storage_idx].list_objects(parent_handle))?;
            let found = objects
                .into_iter()
                .find(|obj| obj.filename == name)
                .ok_or(mtp_rs::Error::NotFound)?;
            inner.inodes.set_handle(node, found.handle);
            parent_handle = Some(found.handle);
        }

        Ok(())
    }

    /// The current handle of a file inode.
    fn file_handle(&self, inner: &mut Inner, inode: u64) -> MtpResult<ObjectHandle> {
        self.ensure_fresh(inner, inode)?;
        match inner.inodes.get(inode).map(|e| &e.kind) {
            Some(InodeKind::File { handle }) => Ok(*handle),
            _ => Err(mtp_rs::Error::NotFound),
        }
    }

    /// The current handle of a file or directory inode.
    fn object_handle(&self, inner: &mut Inner, inode: u64) -> MtpResult<ObjectHandle> {
        self.ensure_fresh(inner, inode)?;
        match inner.inodes.get(inode).map(|e| &e.kind) {
            Some(InodeKind::File { handle } | InodeKind::Directory { handle }) => Ok(*handle),
            _ => Err(mtp_rs::Error::NotFound),
        }
    }

    /// The MTP parent handle to use for operations inside a directory inode
    /// (`None` means the storage root).
    fn parent_handle(&self, inner: &mut Inner, inode: u64) -> MtpResult<Option<ObjectHandle>> {
        self.ensure_fresh(inner, inode)?;
        match inner.inodes.get(inode).map(|e| &e.kind) {
            Some(InodeKind::Storage { .. }) => Ok(None),
            Some(InodeKind::Directory { handle }) => Ok(Some(*handle)),
            _ => Err(mtp_rs::Error::NotFound),
        }
    }

    /// Load children of a directory from MTP into the inode table.
    fn load_dir(&self, inner: &mut Inner, parent_inode: u64) {
        if inner.dirs_loaded.get(&parent_inode) == Some(&true) {
            return;
        }

        if parent_inode == FUSE_ROOT_INODE {
            inner.dirs_loaded.insert(parent_inode, true);
            return;
        }

        match self.with_recovery(inner, |fs, inner| fs.list_into_table(inner, parent_inode)) {
            Ok(()) => {
                inner.dirs_loaded.insert(parent_inode, true);
            }
            Err(e) => error!("Failed to list MTP objects: {e}"),
        }
    }

    /// One listing pass: ask the device, then reconcile the inode table.
    fn list_into_table(&self, inner: &mut Inner, parent_inode: u64) -> MtpResult<()> {
        let mtp_parent = self.parent_handle(inner, parent_inode)?;
        let storage_idx =
            Self::find_storage_index(inner, parent_inode).ok_or(mtp_rs::Error::NotFound)?;

        let objects = self
            .rt
            .block_on(inner.storages[storage_idx].list_objects(mtp_parent))?;

        let children: Vec<ChildInfo> = objects
            .into_iter()
            .map(|obj| ChildInfo {
                handle: obj.handle,
                is_dir: obj.is_folder(),
                size: obj.size,
                mtime: obj
                    .modified
                    .as_ref()
                    .map(mtp_datetime_to_system_time)
                    .unwrap_or(UNIX_EPOCH),
                name: obj.filename,
            })
            .collect();

        inner.inodes.sync_children(parent_inode, &children);
        Ok(())
    }

    /// Flush a dirty write buffer to MTP.
    ///
    /// When the device supports rename, uses a safe upload-then-delete-then-rename
    /// sequence to avoid data loss if the upload fails. Falls back to
    /// delete-then-upload on devices without rename support.
    ///
    /// The spooled bytes live in an unlinked temp file that a disconnect can't
    /// touch, so an upload interrupted by a cable glitch is retried from the
    /// start once the device is back. Only the upload is retried: once it lands,
    /// the data is on the device and a second attempt would just duplicate it.
    fn flush_to_mtp(&self, inner: &mut Inner, fh: u64) -> MtpResult<()> {
        let buf = match inner.write_buf.close(fh) {
            Some(b) => b,
            None => return Ok(()),
        };

        if !buf.is_dirty() {
            return Ok(());
        }

        let inode = buf.inode;
        let mut file = buf.into_file();
        let file_len = file.seek(SeekFrom::End(0)).map_err(io_error)?;

        let entry = match inner.inodes.get(inode) {
            Some(e) => e.clone(),
            None => {
                error!("Flush: inode {inode} not found");
                return Err(mtp_rs::Error::NotFound);
            }
        };

        let mut attempts = 0u32;
        self.with_recovery(inner, |fs, inner| {
            let mut attempt = file.try_clone().map_err(io_error)?;
            attempt.seek(SeekFrom::Start(0)).map_err(io_error)?;
            attempts += 1;
            fs.flush_once(inner, inode, &entry, file_len, attempt, attempts > 1)
        })
    }

    /// One flush attempt against the current session.
    ///
    /// `is_retry` means an earlier attempt died mid-upload, so a half-written
    /// temp object may be sitting in the target directory; it's cleared out
    /// before uploading again.
    #[allow(clippy::too_many_arguments)]
    fn flush_once(
        &self,
        inner: &mut Inner,
        inode: u64,
        entry: &InodeEntry,
        size: u64,
        file: std::fs::File,
        is_retry: bool,
    ) -> MtpResult<()> {
        let handle = self.file_handle(inner, inode)?;
        let storage_idx = Self::find_storage_index(inner, inode).ok_or(mtp_rs::Error::NotFound)?;
        let parent_handle = match self.parent_handle(inner, entry.parent) {
            Ok(handle) => handle,
            Err(e) => {
                error!("Flush: no parent directory for inode {inode}: {e}");
                return Err(e);
            }
        };

        let supports_rename = self.device.lock().unwrap().supports_rename();

        if is_retry && supports_rename {
            self.purge_leftover(
                inner,
                storage_idx,
                parent_handle,
                &temp_upload_name(&entry.name),
            );
        }

        if supports_rename {
            self.flush_safe(
                inner,
                inode,
                handle,
                storage_idx,
                parent_handle,
                entry,
                size,
                file,
            )
        } else {
            warn!(
                "Flush: device does not support rename, using delete-then-upload \
                 (data loss possible if upload fails)"
            );
            self.flush_unsafe(
                inner,
                inode,
                handle,
                storage_idx,
                parent_handle,
                entry,
                size,
                file,
            )
        }
    }

    /// Safe flush: upload with temp name, delete old, rename new.
    ///
    /// Only the upload returns an error to the caller: after it lands, the bytes
    /// are safe on the device and a retry would upload them twice, so the later
    /// steps report what happened and return `Ok`.
    #[allow(clippy::too_many_arguments)]
    fn flush_safe(
        &self,
        inner: &mut Inner,
        inode: u64,
        old_handle: ObjectHandle,
        storage_idx: usize,
        parent_handle: Option<ObjectHandle>,
        entry: &InodeEntry,
        size: u64,
        file: std::fs::File,
    ) -> MtpResult<()> {
        let storage = &inner.storages[storage_idx];
        let temp_name = temp_upload_name(&entry.name);

        // Step 1: Upload new data with a temp name.
        let info = NewObjectInfo::file(&temp_name, size);
        let stream = file_stream(file);
        let new_handle = match self
            .rt
            .block_on(storage.upload(parent_handle, info, stream))
        {
            Ok(h) => h,
            Err(e) => {
                error!("Flush: upload failed (original file untouched): {e}");
                return Err(e.into());
            }
        };

        // Step 2: Delete old object.
        if let Err(e) = self.rt.block_on(storage.delete(old_handle)) {
            error!("Flush: failed to delete old object (new data saved as '{temp_name}'): {e}");
            if let Some(e) = inner.inodes.get_mut(inode) {
                e.kind = InodeKind::File { handle: new_handle };
                e.name = temp_name;
                e.size = size;
                e.mtime = SystemTime::now();
            }
            return Ok(());
        }

        // Step 3: Rename temp to original name.
        if let Err(e) = self.rt.block_on(storage.rename(new_handle, &entry.name)) {
            warn!(
                "Flush: rename from '{temp_name}' to '{}' failed: {e}",
                entry.name
            );
            if let Some(e) = inner.inodes.get_mut(inode) {
                e.kind = InodeKind::File { handle: new_handle };
                e.name = temp_name;
                e.size = size;
                e.mtime = SystemTime::now();
            }
            return Ok(());
        }

        if let Some(e) = inner.inodes.get_mut(inode) {
            e.kind = InodeKind::File { handle: new_handle };
            e.size = size;
            e.mtime = SystemTime::now();
        }
        Ok(())
    }

    /// Unsafe flush: delete old object, then upload. Data is lost if upload fails.
    #[allow(clippy::too_many_arguments)]
    fn flush_unsafe(
        &self,
        inner: &mut Inner,
        inode: u64,
        old_handle: ObjectHandle,
        storage_idx: usize,
        parent_handle: Option<ObjectHandle>,
        entry: &InodeEntry,
        size: u64,
        file: std::fs::File,
    ) -> MtpResult<()> {
        let storage = &inner.storages[storage_idx];

        if let Err(e) = self.rt.block_on(storage.delete(old_handle)) {
            error!("Flush: failed to delete old object: {e}");
            return Err(e);
        }

        let info = NewObjectInfo::file(&entry.name, size);
        let stream = file_stream(file);

        match self
            .rt
            .block_on(storage.upload(parent_handle, info, stream))
        {
            Ok(new_handle) => {
                if let Some(e) = inner.inodes.get_mut(inode) {
                    e.kind = InodeKind::File { handle: new_handle };
                    e.size = size;
                    e.mtime = SystemTime::now();
                }
                Ok(())
            }
            Err(e) => {
                error!("Flush: upload failed after delete (data lost): {e}");
                Err(e.into())
            }
        }
    }

    /// Best-effort removal of a leftover object by name, used to clear a
    /// half-uploaded temp file before retrying a flush.
    fn purge_leftover(
        &self,
        inner: &mut Inner,
        storage_idx: usize,
        parent_handle: Option<ObjectHandle>,
        name: &str,
    ) {
        let storage = &inner.storages[storage_idx];
        let objects = match self.rt.block_on(storage.list_objects(parent_handle)) {
            Ok(objects) => objects,
            Err(e) => {
                debug!("Flush retry: couldn't list the target directory: {e}");
                return;
            }
        };
        for obj in objects.into_iter().filter(|o| o.filename == name) {
            if let Err(e) = self.rt.block_on(storage.delete(obj.handle)) {
                warn!("Flush retry: couldn't remove leftover '{name}': {e}");
            }
        }
    }

    /// Start the one whole-object filler owned by this cache generation.
    ///
    /// Opening the download blocks this thread while it waits for the device's
    /// single MTP session, and it does so holding `inner`. That is deliberate:
    /// `fuser::Config::default()` leaves `n_threads` at 1, so one event-loop
    /// thread dispatches every callback anyway and releasing the lock earlier
    /// would buy nothing. Raise `n_threads` first if you ever want to change it
    /// (the same note applies to uploads and to the reconnect wait).
    fn ensure_full_fill_running(&self, inode: u64, cache: &Arc<SharedSparseCache>) {
        if !cache.start_fill() {
            return;
        }
        if !self.fills.register(Arc::clone(cache)) {
            cache.fail(FillFailure::new(
                "the mount is going away, so no new whole-object download was started",
                false,
            ));
            return;
        }

        let download = {
            let mut inner = self.inner.lock().unwrap();
            self.with_recovery(&mut inner, |fs, inner| {
                let handle = fs.file_handle(inner, inode)?;
                let storage_idx =
                    Self::find_storage_index(inner, inode).ok_or(mtp_rs::Error::NotFound)?;
                fs.rt
                    .block_on(inner.storages[storage_idx].download(handle, ByteRange::Full))
            })
        };

        let download = match download {
            Ok(download) => download,
            Err(error) => {
                cache.fail(FillFailure::new(error.to_string(), is_link_lost(&error)));
                self.fills.finished(cache);
                return;
            }
        };

        self.full_fill_counter.fetch_add(1, Ordering::Relaxed);
        let cache = Arc::clone(cache);
        let fills = Arc::clone(&self.fills);
        let shared_inner = Arc::clone(&self.inner);
        self.rt.spawn(async move {
            // This task owns the FileDownload until end of transfer. A reader
            // seek, an interrupted read(), and close() only drop their Arc on
            // the cache; none of them can reach the stream. Only teardown can,
            // and it cancels rather than drops (see `crate::fill`).
            fill_from_stream(Arc::clone(&cache), FullObjectStream(download), is_link_lost).await;
            fills.finished(&cache);

            // The cache outlives the last close() while its fill is running, so
            // the filler is the one that drops it once nothing holds the file
            // open any more.
            let mut inner = shared_inner.lock().unwrap();
            if !inner.fh_to_inode.values().any(|&open| open == inode) {
                inner.read_cache.remove(&inode);
            }
        });
    }

    /// Re-establish the session after an unavoidable full-stream link loss.
    ///
    /// A successful recovery makes the failed cache eligible for one new
    /// session's full stream. This is never called for seeks or reader
    /// lifecycle, so a healthy stream cannot be restarted from byte zero.
    fn recover_full_fill_link(&self, inode: u64) -> MtpResult<()> {
        let mut inner = self.inner.lock().unwrap();
        self.with_recovery(&mut inner, |fs, inner| {
            let handle = fs.file_handle(inner, inode)?;
            let storage_idx =
                Self::find_storage_index(inner, inode).ok_or(mtp_rs::Error::NotFound)?;
            fs.rt
                .block_on(inner.storages[storage_idx].get_object_info(handle))
                .map(|_| ())
        })
    }

    pub fn mount_options(&self) -> Vec<MountOption> {
        let mut opts = vec![
            MountOption::FSName("mtp-mount".to_string()),
            MountOption::Subtype("mtp".to_string()),
            MountOption::DefaultPermissions,
            MountOption::NoDev,
            MountOption::NoSuid,
        ];
        if self.read_only {
            opts.push(MountOption::RO);
        } else {
            opts.push(MountOption::RW);
        }
        opts
    }

    /// Starts the event monitor for the current session.
    fn spawn_event_loop(&self, device: MtpDevice, epoch: u64) {
        let inner = Arc::clone(&self.inner);
        let current_epoch = Arc::clone(&self.event_epoch);
        self.rt.spawn(async move {
            Self::event_loop(device, inner, current_epoch, epoch).await;
        });
    }

    /// Background event loop that polls the device for MTP events and invalidates
    /// cached directory listings when objects change on the device side.
    ///
    /// Exits when its session is gone: either the device stopped answering, or a
    /// reconnect moved the mount to a newer session (`epoch`), which starts its
    /// own loop. A disconnect here doesn't tear the mount down; the next FUSE
    /// operation is what drives the reconnect.
    async fn event_loop(
        device: MtpDevice,
        inner: Arc<Mutex<Inner>>,
        current_epoch: Arc<AtomicU64>,
        epoch: u64,
    ) {
        loop {
            if current_epoch.load(Ordering::SeqCst) != epoch {
                debug!("Event loop: superseded by a reconnect");
                return;
            }
            match tokio::time::timeout(Duration::from_millis(200), device.next_event()).await {
                Ok(Ok(event)) => {
                    Self::handle_event(&inner, &event);
                }
                Ok(Err(mtp_rs::Error::Timeout)) => continue,
                Ok(Err(e)) if is_link_lost(&e) => {
                    debug!("Event loop: device disconnected");
                    break;
                }
                Ok(Err(e)) => {
                    warn!("Event loop error: {e}");
                    break;
                }
                Err(_) => continue, // tokio timeout elapsed, loop again
            }
        }
    }

    /// Process a single device event by invalidating the relevant cache entries.
    fn handle_event(inner: &Mutex<Inner>, event: &DeviceEvent) {
        match event {
            DeviceEvent::ObjectAdded { handle } => {
                debug!("Event: object added {:?}", handle);
                let mut inner = inner.lock().unwrap();
                // The new object might be in any directory. If we can find its parent
                // in the inode table (the parent dir was already cached), invalidate
                // just that directory. Otherwise, invalidate all directories.
                if let Some(parent_ino) = inner.inodes.find_parent_by_handle(*handle) {
                    inner.dirs_loaded.remove(&parent_ino);
                } else {
                    Self::invalidate_all_dirs(&mut inner);
                }
            }
            DeviceEvent::ObjectRemoved { handle } => {
                debug!("Event: object removed {:?}", handle);
                let mut inner = inner.lock().unwrap();
                if let Some(parent_ino) = inner.inodes.find_parent_by_handle(*handle) {
                    inner.dirs_loaded.remove(&parent_ino);
                } else {
                    Self::invalidate_all_dirs(&mut inner);
                }
            }
            DeviceEvent::ObjectInfoChanged { handle } => {
                debug!("Event: object info changed {:?}", handle);
                let mut inner = inner.lock().unwrap();
                // Invalidate the parent directory and clear any read cache for this file.
                if let Some(parent_ino) = inner.inodes.find_parent_by_handle(*handle) {
                    inner.dirs_loaded.remove(&parent_ino);
                }
                // Drop the cached bytes for this object: they describe the
                // file as it was. A fill still running keeps its own Arc, so it
                // finishes into a cache nothing reads any more rather than being
                // abandoned mid-transfer.
                let stale: Vec<u64> = inner
                    .read_cache
                    .keys()
                    .copied()
                    .filter(|&ino| {
                        inner.inodes.get(ino).is_some_and(
                            |e| matches!(&e.kind, InodeKind::File { handle: h } if *h == *handle),
                        )
                    })
                    .collect();
                for ino in stale {
                    inner.read_cache.remove(&ino);
                }
            }
            DeviceEvent::StoreAdded { .. }
            | DeviceEvent::StoreRemoved { .. }
            | DeviceEvent::StorageInfoChanged { .. } => {
                debug!("Event: storage change {:?}", event);
                // Storage-level changes: invalidate everything.
                let mut inner = inner.lock().unwrap();
                Self::invalidate_all_dirs(&mut inner);
            }
            _ => {
                debug!("Event: unhandled {:?}", event);
            }
        }
    }

    /// Mark all cached directories as stale so they're re-fetched on next access.
    fn invalidate_all_dirs(inner: &mut Inner) {
        inner.dirs_loaded.retain(|&k, _| k == FUSE_ROOT_INODE);
    }
}

impl Filesystem for MtpFs {
    fn init(&mut self, _req: &Request, _config: &mut KernelConfig) -> io::Result<()> {
        let storages = self
            .rt
            .block_on(self.device.lock().unwrap().storages())
            .map_err(|e: mtp_rs::Error| io::Error::other(e.to_string()))?;

        let mut inner = self.inner.lock().unwrap();
        for storage in &storages {
            inner
                .inodes
                .add_storage(storage.id(), storage_name(storage));
        }
        inner.dirs_loaded.insert(FUSE_ROOT_INODE, true);
        inner.storages = storages;
        drop(inner);

        // Spawn a background task that monitors device events and invalidates
        // cached directory listings when objects are added, removed, or changed.
        let event_device = self.device.lock().unwrap().clone();
        self.spawn_event_loop(event_device, self.event_epoch.load(Ordering::SeqCst));

        debug!(
            "MtpFs initialized with {} storages + event monitor",
            self.inner.lock().unwrap().storages.len()
        );
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let parent_ino = parent.0;
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let mut inner = self.inner.lock().unwrap();
        self.load_dir(&mut inner, parent_ino);

        match inner.inodes.lookup(parent_ino, name_str) {
            Some(ino) => {
                let entry = inner.inodes.get(ino).unwrap();
                let attr = inode_to_file_attr(entry);
                reply.entry(&TTL, &attr, Generation(0));
            }
            None => {
                reply.error(Errno::ENOENT);
            }
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let inner = self.inner.lock().unwrap();
        match inner.inodes.get(ino.0) {
            Some(entry) => {
                let mut attr = inode_to_file_attr(entry);
                for (&fh, &inode) in &inner.fh_to_inode {
                    if inode == ino.0 {
                        if let Some(size) = inner.write_buf.size(fh) {
                            attr.size = size;
                            attr.blocks = size.div_ceil(512);
                        }
                        break;
                    }
                }
                reply.attr(&TTL, &attr);
            }
            None => {
                reply.error(Errno::ENOENT);
            }
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let ino_val = ino.0;

        let mut inner = self.inner.lock().unwrap();
        self.load_dir(&mut inner, ino_val);

        let parent_ino = inner
            .inodes
            .get(ino_val)
            .map(|e| e.parent)
            .unwrap_or(FUSE_ROOT_INODE);

        let mut entries: Vec<(u64, INodeNo, FileType, String)> = vec![
            (1, INodeNo(ino_val), FileType::Directory, ".".to_string()),
            (
                2,
                INodeNo(parent_ino),
                FileType::Directory,
                "..".to_string(),
            ),
        ];

        let children = inner.inodes.children(ino_val);
        for (i, child_ino) in children.iter().enumerate() {
            if let Some(child) = inner.inodes.get(*child_ino) {
                let kind = if child.is_dir() {
                    FileType::Directory
                } else {
                    FileType::RegularFile
                };
                entries.push((i as u64 + 3, INodeNo(*child_ino), kind, child.name.clone()));
            }
        }

        for (i, (off, ino, kind, name)) in entries.iter().enumerate() {
            if i as u64 >= offset && reply.add(*ino, *off, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let mut inner = self.inner.lock().unwrap();
        match inner.inodes.get(ino.0) {
            Some(entry) if !entry.is_dir() => {
                // Refuse an unreadable object here rather than at the first
                // read: `cp` would otherwise have created its destination file
                // before finding out, and a sparse cache would have been
                // allocated for bytes that are never coming.
                //
                // Write-only opens are none of this check's business. An
                // overwrite replaces the object without reading a byte of it,
                // and refusing that would make a big file unwritable too.
                if flags.acc_mode() != OpenAccMode::O_WRONLY
                    && self.read_strategy_for(entry.size) == ReadStrategy::TooLargeForSequentialFull
                {
                    error!(
                        "Cannot read '{}': this device has no partial-read operation, so reading \
                         it means holding the MTP session for the whole {} bytes, above the \
                         --full-download-limit of {}. Raise the limit (0 lifts it) to allow it.",
                        entry.name, entry.size, self.full_download_limit
                    );
                    reply.error(Errno::EFBIG);
                    return;
                }
                let fh = self.alloc_fh();
                inner.fh_to_inode.insert(fh, ino.0);
                reply.opened(FileHandle(fh), FopenFlags::empty());
            }
            Some(_) => {
                reply.error(Errno::EISDIR);
            }
            None => {
                reply.error(Errno::ENOENT);
            }
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let fh_val = fh.0;
        let mut inner = self.inner.lock().unwrap();

        // If there's a write buffer open for this fh, read from it.
        if inner.write_buf.is_open(fh_val) {
            match inner.write_buf.read(fh_val, offset as i64, size) {
                Ok(data) => reply.data(&data),
                Err(e) => {
                    error!("Read from write buffer failed: {e}");
                    reply.error(Errno::EIO);
                }
            }
            return;
        }

        // Resolve the MTP object and its storage.
        let entry = match inner.inodes.get(ino.0) {
            Some(e) => e.clone(),
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        if entry.is_dir() {
            reply.error(Errno::EISDIR);
            return;
        }

        // POSIX EOF and zero-length reads never need a cache or an MTP request.
        if size == 0 || offset >= entry.size {
            reply.data(&[]);
            return;
        }

        // Lazily create the sparse cache for this object. Keyed by inode, so a
        // second file descriptor on the same file joins the bytes (and, on the
        // whole-object path, the one download) instead of starting its own.
        use std::collections::hash_map::Entry;
        let spool_dir = inner.spool_dir.clone();
        let cache = match inner.read_cache.entry(ino.0) {
            Entry::Occupied(slot) => Arc::clone(slot.get()),
            Entry::Vacant(slot) => {
                let cache = match SharedSparseCache::new(entry.size, &spool_dir) {
                    Ok(cache) => Arc::new(cache),
                    Err(e) => {
                        error!("Failed to create sparse cache: {e}");
                        reply.error(Errno::EIO);
                        return;
                    }
                };
                slot.insert(Arc::clone(&cache));
                cache
            }
        };

        let strategy = self.read_strategy_for(entry.size);
        if strategy == ReadStrategy::TooLargeForSequentialFull {
            // `open` already refuses this, so getting here means a descriptor
            // that predates the object growing past the limit.
            error!(
                "Cannot read '{}': {} bytes is above the --full-download-limit of {} and this \
                 device has no partial-read operation",
                entry.name, entry.size, self.full_download_limit
            );
            reply.error(Errno::EFBIG);
            return;
        }
        if strategy == ReadStrategy::SequentialFull {
            // The filler owns only per-cache state and its FileDownload. Do not hold
            // Inner while waiting: the background task must be able to publish
            // chunks, and unrelated bookkeeping must remain accessible.
            drop(inner);
            let mut link_retries_remaining = 1u8;
            loop {
                self.ensure_full_fill_running(ino.0, &cache);
                match cache.wait_and_read(offset, size as u64) {
                    Ok(data) => {
                        reply.data(&data);
                        return;
                    }
                    Err(failure) if failure.is_link_lost() => {
                        if link_retries_remaining == 0 {
                            error!(
                                "Full-object fill lost its session again after retry: {}",
                                failure.message()
                            );
                            reply.error(Errno::EIO);
                            return;
                        }
                        debug!(
                            "Full-object fill lost its session; recovering before a new-session retry: {}",
                            failure.message()
                        );
                        if self.recover_full_fill_link(ino.0).is_ok()
                            && cache.reset_after_link_loss()
                        {
                            link_retries_remaining -= 1;
                            continue;
                        }
                        error!("Full-object fill recovery failed: {}", failure.message());
                        reply.error(Errno::EIO);
                        return;
                    }
                    Err(failure) => {
                        error!("Full-object fill failed: {}", failure.message());
                        reply.error(Errno::EIO);
                        return;
                    }
                }
            }
        }

        // Partial-capable responders keep the existing ranged-read path.
        let missing = cache.missing_ranges(offset, size as u64);

        // Fetch missing ranges. `read_range` uses the 64-bit partial-read op to
        // support offsets beyond 4 GB. Each USB transfer is capped at 1 MB to keep
        // latency reasonable.
        // Each chunk resolves the object handle again inside `with_recovery`,
        // so a read that spans a cable glitch picks up the new session's handle
        // and carries on from the byte it stopped at.
        const CHUNK: u64 = 1024 * 1024;
        for range in missing {
            let mut cursor = range.start;
            while cursor < range.end {
                let chunk_size = (range.end - cursor).min(CHUNK) as u32;
                self.fetch_counter.fetch_add(1, Ordering::Relaxed);
                let bytes = match self.with_recovery(&mut inner, |fs, inner| {
                    let handle = fs.file_handle(inner, ino.0)?;
                    let storage_idx =
                        Self::find_storage_index(inner, ino.0).ok_or(mtp_rs::Error::NotFound)?;
                    fs.rt.block_on(
                        inner.storages[storage_idx].read_range(handle, cursor, chunk_size),
                    )
                }) {
                    Ok(b) => b,
                    Err(e) => {
                        error!("MTP read_range failed at offset {cursor}: {e}");
                        reply.error(Errno::EIO);
                        return;
                    }
                };
                let bytes_len = bytes.len() as u64;
                // A responder may hand back more than it was asked for, and
                // `mtp-rs` passes the response through as it arrived. Keep only
                // what fits inside the object: the cache rejects a write past
                // the advertised size, and turning a device quirk into an EIO on
                // the last chunk of every file would be a poor trade.
                let writable = usize::try_from(entry.size.saturating_sub(cursor))
                    .unwrap_or(usize::MAX)
                    .min(bytes.len());
                if writable < bytes.len() {
                    warn!(
                        "Device returned {} bytes at offset {cursor} for '{}', past its advertised \
                         size of {}; keeping the {writable} that fit",
                        bytes.len(),
                        entry.name,
                        entry.size
                    );
                }
                if let Err(e) = cache.write_at(cursor, &bytes[..writable]) {
                    error!("Sparse cache write failed: {e}");
                    reply.error(Errno::EIO);
                    return;
                }
                // Short read from device — the object is smaller than reported;
                // stop fetching to avoid an infinite loop. The validity check
                // below rejects the still-missing tail instead of reading the
                // sparse tempfile's zero-filled hole as device data.
                if bytes_len == 0 {
                    break;
                }
                cursor += bytes_len;
            }
        }

        if !cache.missing_ranges(offset, size as u64).is_empty() {
            error!("MTP ranged read ended before the requested bytes arrived");
            reply.error(Errno::EIO);
            return;
        }

        match cache.read_at(offset, size as u64) {
            Ok(buf) => reply.data(&buf),
            Err(e) => {
                error!("Sparse cache read failed: {e}");
                reply.error(Errno::EIO);
            }
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let fh_val = fh.0;
        let mut inner = self.inner.lock().unwrap();

        let flushed = if inner.write_buf.is_open(fh_val) {
            self.flush_to_mtp(&mut inner, fh_val)
        } else {
            Ok(())
        };

        if let Some(inode) = inner.fh_to_inode.remove(&fh_val) {
            Self::drop_read_cache_if_unused(&mut inner, inode);
        }

        // A failed flush means the bytes never reached the device, so `close()`
        // has to say so instead of pretending the write worked.
        match flushed {
            Ok(()) => reply.ok(),
            Err(e) => {
                error!("Flush on close failed: {e}");
                reply.error(Errno::EIO);
            }
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        if self.read_only {
            reply.error(Errno::EROFS);
            return;
        }

        let fh_val = fh.0;
        let mut inner = self.inner.lock().unwrap();

        if !inner.write_buf.is_open(fh_val) {
            let original_size = inner.inodes.get(ino.0).map(|e| e.size).unwrap_or(0);
            if let Err(e) = inner.write_buf.open(fh_val, ino.0, original_size) {
                error!("Failed to open write buffer: {e}");
                reply.error(Errno::EIO);
                return;
            }
        }

        match inner.write_buf.write(fh_val, offset as i64, data) {
            Ok(written) => reply.written(written),
            Err(e) => {
                error!("Write failed: {e}");
                reply.error(Errno::EIO);
            }
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        if self.read_only {
            reply.error(Errno::EROFS);
            return;
        }

        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let parent_ino = parent.0;
        let mut inner = self.inner.lock().unwrap();

        if !inner.inodes.get(parent_ino).is_some_and(|e| e.is_dir()) {
            reply.error(Errno::ENOTDIR);
            return;
        }

        let handle = match self.with_recovery(&mut inner, |fs, inner| {
            let mtp_parent = fs.parent_handle(inner, parent_ino)?;
            let storage_idx =
                Self::find_storage_index(inner, parent_ino).ok_or(mtp_rs::Error::NotFound)?;
            let info = NewObjectInfo::file(name_str, 0);
            let stream = bytes_stream(Vec::new());
            fs.rt
                .block_on(inner.storages[storage_idx].upload(mtp_parent, info, stream))
                .map_err(mtp_rs::Error::from)
        }) {
            Ok(h) => h,
            Err(e) => {
                error!("MTP create failed: {e}");
                reply.error(Errno::EIO);
                return;
            }
        };

        let now = SystemTime::now();
        let ino = inner
            .inodes
            .add_object(parent_ino, handle, name_str.to_string(), false, 0, now);

        let fh = self.alloc_fh();
        inner.fh_to_inode.insert(fh, ino);
        if let Err(e) = inner.write_buf.open(fh, ino, 0) {
            error!("Failed to open write buffer: {e}");
            reply.error(Errno::EIO);
            return;
        }

        let entry = inner.inodes.get(ino).unwrap();
        let attr = inode_to_file_attr(entry);
        reply.created(
            &TTL,
            &attr,
            Generation(0),
            FileHandle(fh),
            FopenFlags::empty(),
        );
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        if self.read_only {
            reply.error(Errno::EROFS);
            return;
        }

        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let parent_ino = parent.0;
        let mut inner = self.inner.lock().unwrap();

        if !inner.inodes.get(parent_ino).is_some_and(|e| e.is_dir()) {
            reply.error(Errno::ENOTDIR);
            return;
        }

        let handle = match self.with_recovery(&mut inner, |fs, inner| {
            let mtp_parent = fs.parent_handle(inner, parent_ino)?;
            let storage_idx =
                Self::find_storage_index(inner, parent_ino).ok_or(mtp_rs::Error::NotFound)?;
            fs.rt
                .block_on(inner.storages[storage_idx].create_folder(mtp_parent, name_str))
        }) {
            Ok(h) => h,
            Err(e) => {
                error!("MTP mkdir failed: {e}");
                reply.error(Errno::EIO);
                return;
            }
        };

        let now = SystemTime::now();
        let ino = inner
            .inodes
            .add_object(parent_ino, handle, name_str.to_string(), true, 0, now);

        let entry = inner.inodes.get(ino).unwrap();
        let attr = inode_to_file_attr(entry);
        reply.entry(&TTL, &attr, Generation(0));
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if self.read_only {
            reply.error(Errno::EROFS);
            return;
        }

        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let parent_ino = parent.0;
        let mut inner = self.inner.lock().unwrap();

        let child_ino = match inner.inodes.lookup(parent_ino, name_str) {
            Some(i) => i,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        if inner.inodes.get(child_ino).is_some_and(|e| e.is_dir()) {
            reply.error(Errno::EISDIR);
            return;
        }

        if let Err(e) = self.with_recovery(&mut inner, |fs, inner| {
            let handle = fs.file_handle(inner, child_ino)?;
            let storage_idx =
                Self::find_storage_index(inner, child_ino).ok_or(mtp_rs::Error::NotFound)?;
            fs.rt.block_on(inner.storages[storage_idx].delete(handle))
        }) {
            error!("MTP delete failed: {e}");
            reply.error(Errno::EIO);
            return;
        }

        inner.inodes.remove(child_ino);
        reply.ok();
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if self.read_only {
            reply.error(Errno::EROFS);
            return;
        }

        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let parent_ino = parent.0;
        let mut inner = self.inner.lock().unwrap();

        let child_ino = match inner.inodes.lookup(parent_ino, name_str) {
            Some(i) => i,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        if !matches!(
            inner.inodes.get(child_ino).map(|e| &e.kind),
            Some(InodeKind::Directory { .. })
        ) {
            reply.error(Errno::ENOTDIR);
            return;
        }

        if let Err(e) = self.with_recovery(&mut inner, |fs, inner| {
            let handle = fs.object_handle(inner, child_ino)?;
            let storage_idx =
                Self::find_storage_index(inner, child_ino).ok_or(mtp_rs::Error::NotFound)?;
            fs.rt.block_on(inner.storages[storage_idx].delete(handle))
        }) {
            error!("MTP rmdir failed: {e}");
            reply.error(Errno::EIO);
            return;
        }

        inner.inodes.remove(child_ino);
        reply.ok();
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        if self.read_only {
            reply.error(Errno::EROFS);
            return;
        }

        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let newname_str = match newname.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let parent_ino = parent.0;
        let newparent_ino = newparent.0;
        let mut inner = self.inner.lock().unwrap();

        let child_ino = match inner.inodes.lookup(parent_ino, name_str) {
            Some(i) => i,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        if !matches!(
            inner.inodes.get(child_ino).map(|e| &e.kind),
            Some(InodeKind::File { .. } | InodeKind::Directory { .. })
        ) {
            reply.error(Errno::EINVAL);
            return;
        }
        if parent_ino != newparent_ino
            && !inner.inodes.get(newparent_ino).is_some_and(|e| e.is_dir())
        {
            reply.error(Errno::ENOTDIR);
            return;
        }

        if name_str != newname_str {
            if let Err(e) = self.with_recovery(&mut inner, |fs, inner| {
                let handle = fs.object_handle(inner, child_ino)?;
                let storage_idx =
                    Self::find_storage_index(inner, child_ino).ok_or(mtp_rs::Error::NotFound)?;
                fs.rt
                    .block_on(inner.storages[storage_idx].rename(handle, newname_str))
            }) {
                error!("MTP rename failed: {e}");
                reply.error(Errno::EIO);
                return;
            }
        }

        if parent_ino != newparent_ino {
            if let Err(e) = self.with_recovery(&mut inner, |fs, inner| {
                let handle = fs.object_handle(inner, child_ino)?;
                let storage_idx =
                    Self::find_storage_index(inner, child_ino).ok_or(mtp_rs::Error::NotFound)?;
                let new_mtp_parent = fs
                    .parent_handle(inner, newparent_ino)?
                    .unwrap_or(ObjectHandle::ROOT);
                fs.rt.block_on(inner.storages[storage_idx].move_object(
                    handle,
                    new_mtp_parent,
                    None,
                ))
            }) {
                error!("MTP move failed: {e}");
                reply.error(Errno::EIO);
                return;
            }
        }

        inner
            .inodes
            .rename(child_ino, newparent_ino, newname_str.to_string());
        reply.ok();
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        if let Some(new_size) = size {
            if self.read_only {
                reply.error(Errno::EROFS);
                return;
            }

            if let Some(fh) = fh {
                let fh_val = fh.0;
                let mut inner = self.inner.lock().unwrap();

                if !inner.write_buf.is_open(fh_val) {
                    let original_size = inner.inodes.get(ino.0).map(|e| e.size).unwrap_or(0);
                    if let Err(e) = inner.write_buf.open(fh_val, ino.0, original_size) {
                        error!("Failed to open write buffer: {e}");
                        reply.error(Errno::EIO);
                        return;
                    }
                }

                if new_size == 0 {
                    inner.write_buf.close(fh_val);
                    if let Err(e) = inner.write_buf.open(fh_val, ino.0, 0) {
                        error!("Failed to open write buffer: {e}");
                        reply.error(Errno::EIO);
                        return;
                    }
                }
            }
        }

        let inner = self.inner.lock().unwrap();
        match inner.inodes.get(ino.0) {
            Some(entry) => {
                let mut attr = inode_to_file_attr(entry);
                if let Some(new_size) = size {
                    attr.size = new_size;
                    attr.blocks = new_size.div_ceil(512);
                }
                reply.attr(&TTL, &attr);
            }
            None => {
                reply.error(Errno::ENOENT);
            }
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let inner = self.inner.lock().unwrap();
        let block_size: u64 = 4096;

        let mut total_bytes: u64 = 0;
        let mut free_bytes: u64 = 0;
        for storage in &inner.storages {
            total_bytes = total_bytes.saturating_add(storage.info().total_capacity);
            free_bytes = free_bytes.saturating_add(storage.info().free_space);
        }

        let blocks = total_bytes / block_size;
        let bfree = free_bytes / block_size;

        reply.statfs(blocks, bfree, bfree, 0, 0, block_size as u32, 255, 0);
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let mut inner = self.inner.lock().unwrap();
        match inner.inodes.get(ino.0) {
            Some(entry) if entry.is_dir() => {
                let fh = self.alloc_fh();
                inner.dirs_loaded.remove(&ino.0);
                reply.opened(FileHandle(fh), FopenFlags::empty());
            }
            Some(_) => {
                reply.error(Errno::ENOTDIR);
            }
            None => {
                reply.error(Errno::ENOENT);
            }
        }
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;
    use std::io::Write as _;

    const CHUNK: usize = UPLOAD_CHUNK;

    /// A temp file holding `content`, rewound to the start.
    fn spool_file(content: &[u8]) -> std::fs::File {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(content).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file
    }

    #[test]
    fn file_stream_reads_lazily() {
        let file = spool_file(&vec![0xABu8; CHUNK * 4]);
        // `try_clone` dups the fd, so the clone shares the read cursor and shows
        // how far the stream has actually read.
        let mut cursor = file.try_clone().unwrap();

        let mut stream = file_stream(file);
        let first = futures::executor::block_on(stream.next()).unwrap().unwrap();

        assert_eq!(first.len(), CHUNK);
        assert_eq!(
            cursor.stream_position().unwrap(),
            CHUNK as u64,
            "one poll must read one chunk, not the whole file"
        );
    }

    #[test]
    fn file_stream_yields_the_whole_file_then_ends() {
        // Two full chunks plus a short tail, so the partial-read path is covered.
        let content = vec![0xCDu8; CHUNK * 2 + 17];

        let chunks: Vec<_> =
            futures::executor::block_on(file_stream(spool_file(&content)).collect());

        let sizes: Vec<_> = chunks.iter().map(|c| c.as_ref().unwrap().len()).collect();
        assert_eq!(sizes, vec![CHUNK, CHUNK, 17]);
        let joined: Vec<u8> = chunks
            .into_iter()
            .flat_map(|c| c.unwrap().to_vec())
            .collect();
        assert_eq!(joined, content);
    }

    #[test]
    fn file_stream_ends_after_a_read_error() {
        // A write-only fd fails every read with EBADF, and the stream has to stop
        // there rather than re-emitting the error forever.
        let path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();

        let mut stream = file_stream(file);
        assert!(futures::executor::block_on(stream.next()).unwrap().is_err());
        assert!(futures::executor::block_on(stream.next()).is_none());
    }

    const LIMIT: u64 = DEFAULT_FULL_DOWNLOAD_LIMIT;

    #[test]
    fn partial_capability_always_keeps_the_ranged_path() {
        assert_eq!(read_strategy(true, 1, LIMIT), ReadStrategy::Ranged);
        assert_eq!(
            read_strategy(true, LIMIT + 1, LIMIT),
            ReadStrategy::Ranged,
            "the whole-object bound must not affect DBI/Android-style responders"
        );
    }

    #[test]
    fn no_partial_capability_uses_a_bounded_sequential_fill() {
        assert_eq!(
            read_strategy(false, LIMIT, LIMIT),
            ReadStrategy::SequentialFull,
            "an object exactly at the limit is still allowed"
        );
        assert_eq!(
            read_strategy(false, LIMIT + 1, LIMIT),
            ReadStrategy::TooLargeForSequentialFull
        );
    }

    #[test]
    fn a_zero_limit_lifts_the_ceiling_entirely() {
        assert_eq!(
            read_strategy(false, u64::MAX, 0),
            ReadStrategy::SequentialFull
        );
        assert_eq!(
            read_strategy(false, u64::MAX, 1),
            ReadStrategy::TooLargeForSequentialFull,
            "0 means no limit; every other value is a real ceiling"
        );
    }
}
