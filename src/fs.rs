use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::io::{Read as _, Seek, SeekFrom, Write as _};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use fuser::{
    BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, KernelConfig, LockOwner, MountOption, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request,
    TimeOrNow, WriteFlags,
};
use log::{debug, error};
use mtp_rs::mtp::MtpDevice;
use mtp_rs::{NewObjectInfo, ObjectHandle, Storage};

use crate::buffer::WriteBuffer;
use crate::inode::{InodeEntry, InodeKind, InodeTable, FUSE_ROOT_INODE};

const TTL: Duration = Duration::from_secs(1);

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

/// Mutable state protected by `RefCell` so fuser's `&self` callbacks can mutate it.
struct Inner {
    storages: Vec<Storage>,
    inodes: InodeTable,
    write_buf: WriteBuffer,
    read_cache: HashMap<u64, std::fs::File>,
    dirs_loaded: HashMap<u64, bool>,
    fh_to_inode: HashMap<u64, u64>,
}

/// FUSE filesystem backed by an MTP device.
pub struct MtpFs {
    rt: tokio::runtime::Handle,
    device: Mutex<MtpDevice>,
    inner: Mutex<Inner>,
    next_fh: AtomicU64,
    read_only: bool,
}

impl MtpFs {
    pub fn new(device: MtpDevice, read_only: bool, rt: tokio::runtime::Handle) -> Self {
        Self {
            rt,
            device: Mutex::new(device),
            inner: Mutex::new(Inner {
                storages: Vec::new(),
                inodes: InodeTable::new(),
                write_buf: WriteBuffer::new(),
                read_cache: HashMap::new(),
                dirs_loaded: HashMap::new(),
                fh_to_inode: HashMap::new(),
            }),
            next_fh: AtomicU64::new(1),
            read_only,
        }
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

    /// Get the MTP parent handle for a given directory inode.
    fn mtp_parent_handle(inner: &Inner, inode: u64) -> Option<Option<ObjectHandle>> {
        let entry = inner.inodes.get(inode)?;
        match &entry.kind {
            InodeKind::Storage { .. } => Some(None),
            InodeKind::Directory { handle } => Some(Some(*handle)),
            _ => None,
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

        let mtp_parent = match Self::mtp_parent_handle(inner, parent_inode) {
            Some(p) => p,
            None => return,
        };

        let storage_idx = match Self::find_storage_index(inner, parent_inode) {
            Some(i) => i,
            None => return,
        };

        let objects = match self
            .rt
            .block_on(inner.storages[storage_idx].list_objects(mtp_parent))
        {
            Ok(objs) => objs,
            Err(e) => {
                error!("Failed to list MTP objects: {e}");
                return;
            }
        };

        inner.inodes.clear_children(parent_inode);

        for obj in objects {
            let mtime = obj
                .modified
                .as_ref()
                .map(mtp_datetime_to_system_time)
                .unwrap_or(UNIX_EPOCH);
            let is_folder = obj.is_folder();
            inner.inodes.add_object(
                parent_inode,
                obj.handle,
                obj.filename,
                is_folder,
                obj.size,
                mtime,
            );
        }

        inner.dirs_loaded.insert(parent_inode, true);
    }

    /// Flush a dirty write buffer to MTP by deleting the old object and uploading new data.
    fn flush_to_mtp(&self, inner: &mut Inner, fh: u64) {
        let buf = match inner.write_buf.close(fh) {
            Some(b) => b,
            None => return,
        };

        if !buf.is_dirty() {
            return;
        }

        let inode = buf.inode;
        let data = buf.into_data();
        let entry = match inner.inodes.get(inode) {
            Some(e) => e.clone(),
            None => {
                error!("Flush: inode {inode} not found");
                return;
            }
        };

        let handle = match &entry.kind {
            InodeKind::File { handle } => *handle,
            _ => {
                error!("Flush: inode {inode} is not a file");
                return;
            }
        };

        let storage_idx = match Self::find_storage_index(inner, inode) {
            Some(i) => i,
            None => {
                error!("Flush: no storage for inode {inode}");
                return;
            }
        };

        let parent_handle = inner.inodes.get(entry.parent).and_then(|p| match &p.kind {
            InodeKind::Storage { .. } => None,
            InodeKind::Directory { handle } => Some(*handle),
            _ => None,
        });

        let storage = &inner.storages[storage_idx];

        if let Err(e) = self.rt.block_on(storage.delete(handle)) {
            error!("Flush: failed to delete old object: {e}");
            return;
        }

        let size = data.len() as u64;
        let info = NewObjectInfo::file(&entry.name, size);
        let stream = bytes_stream(data);

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
            }
            Err(e) => {
                error!("Flush: failed to upload: {e}");
            }
        }
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
}

impl Filesystem for MtpFs {
    fn init(&mut self, _req: &Request, _config: &mut KernelConfig) -> io::Result<()> {
        let storages = self
            .rt
            .block_on(self.device.lock().unwrap().storages())
            .map_err(|e| io::Error::other(e.to_string()))?;

        let mut inner = self.inner.lock().unwrap();
        for storage in &storages {
            let name = if storage.info().description.is_empty() {
                format!("Storage_{}", storage.id().0)
            } else {
                storage.info().description.clone()
            };
            inner.inodes.add_storage(storage.id(), name);
        }
        inner.dirs_loaded.insert(FUSE_ROOT_INODE, true);
        inner.storages = storages;

        debug!("MtpFs initialized with {} storages", inner.storages.len());
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

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let mut inner = self.inner.lock().unwrap();
        match inner.inodes.get(ino.0) {
            Some(entry) if !entry.is_dir() => {
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

        // Download and cache if not already cached.
        if !inner.read_cache.contains_key(&fh_val) {
            let entry = match inner.inodes.get(ino.0) {
                Some(e) => e,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            };

            let handle = match &entry.kind {
                InodeKind::File { handle } => *handle,
                _ => {
                    reply.error(Errno::EISDIR);
                    return;
                }
            };

            let storage_idx = match Self::find_storage_index(&inner, ino.0) {
                Some(i) => i,
                None => {
                    reply.error(Errno::EIO);
                    return;
                }
            };

            let mut download = match self
                .rt
                .block_on(inner.storages[storage_idx].download_stream(handle))
            {
                Ok(d) => d,
                Err(e) => {
                    error!("MTP download_stream failed: {e}");
                    reply.error(Errno::EIO);
                    return;
                }
            };

            let mut file = match tempfile::tempfile() {
                Ok(f) => f,
                Err(e) => {
                    error!("Failed to create temp file: {e}");
                    reply.error(Errno::EIO);
                    return;
                }
            };

            let write_ok = self.rt.block_on(async {
                while let Some(chunk) = download.next_chunk().await {
                    match chunk {
                        Ok(bytes) => {
                            if let Err(e) = file.write_all(&bytes) {
                                error!("Failed to write to temp file: {e}");
                                return false;
                            }
                        }
                        Err(e) => {
                            error!("MTP download chunk failed: {e}");
                            return false;
                        }
                    }
                }
                true
            });

            if !write_ok {
                reply.error(Errno::EIO);
                return;
            }

            inner.read_cache.insert(fh_val, file);
        }

        let file = inner.read_cache.get_mut(&fh_val).unwrap();
        let file_size = file.seek(SeekFrom::End(0)).unwrap_or(0);
        if offset >= file_size {
            reply.data(&[]);
        } else {
            let read_len = (size as u64).min(file_size - offset) as usize;
            let mut buf = vec![0u8; read_len];
            if let Err(e) = file.seek(SeekFrom::Start(offset)) {
                error!("Seek failed: {e}");
                reply.error(Errno::EIO);
                return;
            }
            match file.read_exact(&mut buf) {
                Ok(()) => reply.data(&buf),
                Err(e) => {
                    error!("Read from temp file failed: {e}");
                    reply.error(Errno::EIO);
                }
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

        if inner.write_buf.is_open(fh_val) {
            self.flush_to_mtp(&mut inner, fh_val);
        }

        inner.read_cache.remove(&fh_val);
        inner.fh_to_inode.remove(&fh_val);
        reply.ok();
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
            inner.write_buf.open(fh_val, ino.0, original_size);
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

        let storage_idx = match Self::find_storage_index(&inner, parent_ino) {
            Some(i) => i,
            None => {
                reply.error(Errno::EIO);
                return;
            }
        };

        let mtp_parent = match Self::mtp_parent_handle(&inner, parent_ino) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOTDIR);
                return;
            }
        };

        let info = NewObjectInfo::file(name_str, 0);
        let stream = bytes_stream(Vec::new());

        let handle = match self
            .rt
            .block_on(inner.storages[storage_idx].upload(mtp_parent, info, stream))
        {
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
        inner.write_buf.open(fh, ino, 0);

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

        let storage_idx = match Self::find_storage_index(&inner, parent_ino) {
            Some(i) => i,
            None => {
                reply.error(Errno::EIO);
                return;
            }
        };

        let mtp_parent = match Self::mtp_parent_handle(&inner, parent_ino) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOTDIR);
                return;
            }
        };

        let handle = match self
            .rt
            .block_on(inner.storages[storage_idx].create_folder(mtp_parent, name_str))
        {
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

        let handle = match inner.inodes.get(child_ino).and_then(|e| match &e.kind {
            InodeKind::File { handle } => Some(*handle),
            _ => None,
        }) {
            Some(h) => h,
            None => {
                reply.error(Errno::EISDIR);
                return;
            }
        };

        let storage_idx = match Self::find_storage_index(&inner, child_ino) {
            Some(i) => i,
            None => {
                reply.error(Errno::EIO);
                return;
            }
        };

        if let Err(e) = self.rt.block_on(inner.storages[storage_idx].delete(handle)) {
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

        let handle = match inner.inodes.get(child_ino).and_then(|e| match &e.kind {
            InodeKind::Directory { handle } => Some(*handle),
            _ => None,
        }) {
            Some(h) => h,
            None => {
                reply.error(Errno::ENOTDIR);
                return;
            }
        };

        let storage_idx = match Self::find_storage_index(&inner, child_ino) {
            Some(i) => i,
            None => {
                reply.error(Errno::EIO);
                return;
            }
        };

        if let Err(e) = self.rt.block_on(inner.storages[storage_idx].delete(handle)) {
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

        let handle = match inner.inodes.get(child_ino).and_then(|e| match &e.kind {
            InodeKind::File { handle } | InodeKind::Directory { handle } => Some(*handle),
            _ => None,
        }) {
            Some(h) => h,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let storage_idx = match Self::find_storage_index(&inner, child_ino) {
            Some(i) => i,
            None => {
                reply.error(Errno::EIO);
                return;
            }
        };

        if name_str != newname_str {
            if let Err(e) = self
                .rt
                .block_on(inner.storages[storage_idx].rename(handle, newname_str))
            {
                error!("MTP rename failed: {e}");
                reply.error(Errno::EIO);
                return;
            }
        }

        if parent_ino != newparent_ino {
            let new_mtp_parent = match Self::mtp_parent_handle(&inner, newparent_ino) {
                Some(Some(h)) => h,
                Some(None) => ObjectHandle::ROOT,
                None => {
                    reply.error(Errno::ENOTDIR);
                    return;
                }
            };

            if let Err(e) = self.rt.block_on(inner.storages[storage_idx].move_object(
                handle,
                new_mtp_parent,
                None,
            )) {
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
                    inner.write_buf.open(fh_val, ino.0, original_size);
                }

                if new_size == 0 {
                    inner.write_buf.close(fh_val);
                    inner.write_buf.open(fh_val, ino.0, 0);
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
        reply.statfs(0, 0, 0, 0, 0, 4096, 255, 0);
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
