use std::collections::HashMap;
use std::time::SystemTime;

use mtp_rs::{ObjectHandle, StorageId};

/// FUSE root inode number.
pub const FUSE_ROOT_INODE: u64 = 1;

/// What kind of entry an inode represents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InodeKind {
    Root,
    Storage { storage_id: StorageId },
    Directory { handle: ObjectHandle },
    File { handle: ObjectHandle },
}

/// Metadata cached for a single inode.
#[derive(Debug, Clone)]
pub struct InodeEntry {
    pub inode: u64,
    pub parent: u64,
    pub name: String,
    pub kind: InodeKind,
    pub size: u64,
    pub mtime: SystemTime,
    pub atime: SystemTime,
    /// Link generation the handle in `kind` was resolved against. MTP handles
    /// are session-scoped, so a handle from an older generation is a stale
    /// token that has to be re-resolved by path before it's used again.
    pub generation: u64,
}

impl InodeEntry {
    pub fn is_dir(&self) -> bool {
        matches!(
            self.kind,
            InodeKind::Root | InodeKind::Storage { .. } | InodeKind::Directory { .. }
        )
    }
}

/// Bidirectional mapping between FUSE inodes and MTP objects, with cached metadata.
#[derive(Debug)]
pub struct InodeTable {
    entries: HashMap<u64, InodeEntry>,
    /// (parent_inode, child_name) -> child_inode
    name_index: HashMap<(u64, String), u64>,
    /// parent_inode -> list of child inodes
    children_index: HashMap<u64, Vec<u64>>,
    next_inode: u64,
    /// Bumped on every reconnect; see [`InodeEntry::generation`].
    generation: u64,
}

/// One child as the device reports it, for [`InodeTable::sync_children`].
#[derive(Debug, Clone)]
pub struct ChildInfo {
    pub handle: ObjectHandle,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: SystemTime,
}

impl Default for InodeTable {
    fn default() -> Self {
        Self::new()
    }
}

impl InodeTable {
    /// Creates a new table with only the root inode (inode 1).
    pub fn new() -> Self {
        let root = InodeEntry {
            inode: FUSE_ROOT_INODE,
            parent: FUSE_ROOT_INODE,
            name: String::new(),
            kind: InodeKind::Root,
            size: 0,
            mtime: SystemTime::UNIX_EPOCH,
            atime: SystemTime::UNIX_EPOCH,
            generation: 0,
        };
        let mut entries = HashMap::new();
        entries.insert(FUSE_ROOT_INODE, root);

        Self {
            entries,
            name_index: HashMap::new(),
            children_index: HashMap::new(),
            next_inode: 2,
            generation: 0,
        }
    }

    /// Marks every cached object handle as stale, without touching inode
    /// numbers, names, or the tree shape. Called after a reconnect: the kernel
    /// and any open file descriptor keep referring to the same inodes, while
    /// the handles behind them get re-resolved by path on next use.
    pub fn bump_generation(&mut self) {
        self.generation += 1;
    }

    /// Whether this inode's handle was resolved against the current session.
    /// Storage and root entries carry no session-scoped handle, so they're
    /// always fresh (their storage IDs are re-mapped eagerly on reconnect).
    pub fn is_fresh(&self, inode: u64) -> bool {
        match self.entries.get(&inode) {
            Some(entry) => match entry.kind {
                InodeKind::Root | InodeKind::Storage { .. } => true,
                InodeKind::Directory { .. } | InodeKind::File { .. } => {
                    entry.generation == self.generation
                }
            },
            None => false,
        }
    }

    /// Points an inode at a freshly resolved handle, keeping its inode number.
    pub fn set_handle(&mut self, inode: u64, handle: ObjectHandle) {
        let generation = self.generation;
        let Some(entry) = self.entries.get_mut(&inode) else {
            return;
        };
        entry.kind = match entry.kind {
            InodeKind::Directory { .. } => InodeKind::Directory { handle },
            InodeKind::File { .. } => InodeKind::File { handle },
            _ => return,
        };
        entry.generation = generation;
    }

    /// Re-points a storage inode at a storage ID from the current session.
    pub fn set_storage_id(&mut self, inode: u64, storage_id: StorageId) {
        if let Some(entry) = self.entries.get_mut(&inode) {
            if matches!(entry.kind, InodeKind::Storage { .. }) {
                entry.kind = InodeKind::Storage { storage_id };
            }
        }
    }

    fn alloc_inode(&mut self) -> u64 {
        let ino = self.next_inode;
        self.next_inode += 1;
        ino
    }

    fn insert(&mut self, entry: InodeEntry) -> u64 {
        let ino = entry.inode;
        let parent = entry.parent;
        let name = entry.name.clone();

        self.entries.insert(ino, entry);
        self.name_index.insert((parent, name), ino);
        self.children_index.entry(parent).or_default().push(ino);
        ino
    }

    /// Adds a storage as a child of root. Returns the new inode number.
    pub fn add_storage(&mut self, storage_id: StorageId, name: String) -> u64 {
        let ino = self.alloc_inode();
        let now = SystemTime::now();
        self.insert(InodeEntry {
            inode: ino,
            parent: FUSE_ROOT_INODE,
            name,
            kind: InodeKind::Storage { storage_id },
            size: 0,
            mtime: now,
            atime: now,
            generation: self.generation,
        })
    }

    /// Adds a file or directory under the given parent. Returns the new inode number.
    pub fn add_object(
        &mut self,
        parent_inode: u64,
        handle: ObjectHandle,
        name: String,
        is_dir: bool,
        size: u64,
        mtime: SystemTime,
    ) -> u64 {
        let ino = self.alloc_inode();
        let kind = if is_dir {
            InodeKind::Directory { handle }
        } else {
            InodeKind::File { handle }
        };
        self.insert(InodeEntry {
            inode: ino,
            parent: parent_inode,
            name,
            kind,
            size,
            mtime,
            atime: mtime,
            generation: self.generation,
        })
    }

    /// Replaces a directory's children with what the device just reported,
    /// **reusing the inode number** of every child that's still there under the
    /// same name and kind.
    ///
    /// Inode numbers must survive a re-listing: the kernel caches them and open
    /// file descriptors refer to them, so handing out a fresh number for a file
    /// that didn't go anywhere breaks reads on an already-open fd. Only the
    /// handle, size, and mtime are refreshed in place. Children the device no
    /// longer reports are removed.
    pub fn sync_children(&mut self, parent_inode: u64, children: &[ChildInfo]) {
        let generation = self.generation;

        for child in children {
            match self.lookup(parent_inode, &child.name) {
                Some(ino)
                    if self
                        .entries
                        .get(&ino)
                        .is_some_and(|e| e.is_dir() == child.is_dir) =>
                {
                    let entry = self.entries.get_mut(&ino).expect("looked up above");
                    entry.kind = if child.is_dir {
                        InodeKind::Directory {
                            handle: child.handle,
                        }
                    } else {
                        InodeKind::File {
                            handle: child.handle,
                        }
                    };
                    entry.size = child.size;
                    entry.mtime = child.mtime;
                    entry.generation = generation;
                }
                // A name that flipped between file and directory is a different
                // object; drop the old inode and allocate a new one.
                Some(ino) => {
                    self.remove(ino);
                    self.add_object(
                        parent_inode,
                        child.handle,
                        child.name.clone(),
                        child.is_dir,
                        child.size,
                        child.mtime,
                    );
                }
                None => {
                    self.add_object(
                        parent_inode,
                        child.handle,
                        child.name.clone(),
                        child.is_dir,
                        child.size,
                        child.mtime,
                    );
                }
            }
        }

        let gone: Vec<u64> = self
            .children(parent_inode)
            .into_iter()
            .filter(|ino| {
                self.entries
                    .get(ino)
                    .is_some_and(|e| !children.iter().any(|c| c.name == e.name))
            })
            .collect();
        for ino in gone {
            self.remove(ino);
        }
    }

    /// Looks up an entry by inode number.
    pub fn get(&self, inode: u64) -> Option<&InodeEntry> {
        self.entries.get(&inode)
    }

    /// Mutable lookup by inode number.
    pub fn get_mut(&mut self, inode: u64) -> Option<&mut InodeEntry> {
        self.entries.get_mut(&inode)
    }

    /// Finds a child inode by parent inode and name.
    pub fn lookup(&self, parent_inode: u64, name: &str) -> Option<u64> {
        self.name_index
            .get(&(parent_inode, name.to_string()))
            .copied()
    }

    /// Returns the inodes of all children of the given parent.
    pub fn children(&self, parent_inode: u64) -> Vec<u64> {
        self.children_index
            .get(&parent_inode)
            .cloned()
            .unwrap_or_default()
    }

    /// Removes an entry and its index entries. Does not remove descendants.
    pub fn remove(&mut self, inode: u64) -> Option<InodeEntry> {
        let entry = self.entries.remove(&inode)?;
        self.name_index.remove(&(entry.parent, entry.name.clone()));
        if let Some(siblings) = self.children_index.get_mut(&entry.parent) {
            siblings.retain(|&i| i != inode);
        }
        // Also remove any children index for this inode (but not the children themselves).
        self.children_index.remove(&inode);
        Some(entry)
    }

    /// Updates an entry's parent and name (for rename/move operations).
    pub fn rename(&mut self, inode: u64, new_parent: u64, new_name: String) {
        let Some(entry) = self.entries.get_mut(&inode) else {
            return;
        };
        let old_parent = entry.parent;
        let old_name = entry.name.clone();

        // Update the entry itself.
        entry.parent = new_parent;
        entry.name = new_name.clone();

        // Update name index.
        self.name_index.remove(&(old_parent, old_name));
        self.name_index.insert((new_parent, new_name), inode);

        // Update children index.
        if let Some(siblings) = self.children_index.get_mut(&old_parent) {
            siblings.retain(|&i| i != inode);
        }
        self.children_index
            .entry(new_parent)
            .or_default()
            .push(inode);
    }

    /// Finds the parent inode of an entry identified by its MTP object handle.
    /// Returns `None` if the handle is not in the table.
    pub fn find_parent_by_handle(&self, handle: ObjectHandle) -> Option<u64> {
        self.entries.values().find_map(|e| match &e.kind {
            InodeKind::File { handle: h } | InodeKind::Directory { handle: h } if *h == handle => {
                Some(e.parent)
            }
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_has_root() {
        let table = InodeTable::new();
        let root = table.get(FUSE_ROOT_INODE).expect("root must exist");
        assert_eq!(root.inode, FUSE_ROOT_INODE);
        assert_eq!(root.kind, InodeKind::Root);
        assert!(root.is_dir());
    }

    #[test]
    fn test_add_storage() {
        let mut table = InodeTable::new();
        let ino = table.add_storage(StorageId(1), "Internal".into());
        assert_eq!(ino, 2);

        let entry = table.get(ino).unwrap();
        assert_eq!(entry.name, "Internal");
        assert_eq!(
            entry.kind,
            InodeKind::Storage {
                storage_id: StorageId(1)
            }
        );
        assert_eq!(entry.parent, FUSE_ROOT_INODE);
        assert!(entry.is_dir());
    }

    #[test]
    fn test_add_object_file() {
        let mut table = InodeTable::new();
        let storage_ino = table.add_storage(StorageId(1), "Internal".into());
        let mtime = SystemTime::UNIX_EPOCH;

        let file_ino = table.add_object(
            storage_ino,
            ObjectHandle(100),
            "photo.jpg".into(),
            false,
            4096,
            mtime,
        );

        let entry = table.get(file_ino).unwrap();
        assert_eq!(entry.name, "photo.jpg");
        assert_eq!(
            entry.kind,
            InodeKind::File {
                handle: ObjectHandle(100)
            }
        );
        assert_eq!(entry.size, 4096);
        assert_eq!(entry.parent, storage_ino);
        assert!(!entry.is_dir());
    }

    #[test]
    fn test_add_object_directory() {
        let mut table = InodeTable::new();
        let storage_ino = table.add_storage(StorageId(1), "Internal".into());
        let mtime = SystemTime::UNIX_EPOCH;

        let dir_ino = table.add_object(
            storage_ino,
            ObjectHandle(200),
            "DCIM".into(),
            true,
            0,
            mtime,
        );

        let entry = table.get(dir_ino).unwrap();
        assert_eq!(
            entry.kind,
            InodeKind::Directory {
                handle: ObjectHandle(200)
            }
        );
        assert!(entry.is_dir());
    }

    #[test]
    fn test_lookup_by_name() {
        let mut table = InodeTable::new();
        let storage_ino = table.add_storage(StorageId(1), "Internal".into());
        let mtime = SystemTime::UNIX_EPOCH;
        let file_ino = table.add_object(
            storage_ino,
            ObjectHandle(100),
            "photo.jpg".into(),
            false,
            1024,
            mtime,
        );

        assert_eq!(table.lookup(storage_ino, "photo.jpg"), Some(file_ino));
        assert_eq!(table.lookup(FUSE_ROOT_INODE, "Internal"), Some(storage_ino));
    }

    #[test]
    fn test_lookup_nonexistent() {
        let table = InodeTable::new();
        assert_eq!(table.lookup(FUSE_ROOT_INODE, "nope"), None);
        assert!(table.get(999).is_none());
    }

    #[test]
    fn test_children() {
        let mut table = InodeTable::new();
        let s1 = table.add_storage(StorageId(1), "Internal".into());
        let s2 = table.add_storage(StorageId(2), "SD Card".into());

        let root_children = table.children(FUSE_ROOT_INODE);
        assert_eq!(root_children, vec![s1, s2]);

        let mtime = SystemTime::UNIX_EPOCH;
        let f1 = table.add_object(s1, ObjectHandle(10), "a.txt".into(), false, 100, mtime);
        let f2 = table.add_object(s1, ObjectHandle(11), "b.txt".into(), false, 200, mtime);

        let storage_children = table.children(s1);
        assert_eq!(storage_children, vec![f1, f2]);

        assert!(table.children(s2).is_empty());
    }

    #[test]
    fn test_remove() {
        let mut table = InodeTable::new();
        let storage_ino = table.add_storage(StorageId(1), "Internal".into());
        let mtime = SystemTime::UNIX_EPOCH;
        let file_ino = table.add_object(
            storage_ino,
            ObjectHandle(100),
            "photo.jpg".into(),
            false,
            1024,
            mtime,
        );

        let removed = table.remove(file_ino).expect("should remove");
        assert_eq!(removed.name, "photo.jpg");
        assert!(table.get(file_ino).is_none());
        assert_eq!(table.lookup(storage_ino, "photo.jpg"), None);
        assert!(table.children(storage_ino).is_empty());
    }

    #[test]
    fn test_rename() {
        let mut table = InodeTable::new();
        let s1 = table.add_storage(StorageId(1), "Internal".into());
        let mtime = SystemTime::UNIX_EPOCH;
        let dir_ino = table.add_object(s1, ObjectHandle(200), "DCIM".into(), true, 0, mtime);
        let file_ino = table.add_object(s1, ObjectHandle(100), "old.txt".into(), false, 512, mtime);

        // Move file from storage root into DCIM and rename it.
        table.rename(file_ino, dir_ino, "new.txt".into());

        assert_eq!(table.lookup(s1, "old.txt"), None);
        assert_eq!(table.lookup(dir_ino, "new.txt"), Some(file_ino));

        let entry = table.get(file_ino).unwrap();
        assert_eq!(entry.parent, dir_ino);
        assert_eq!(entry.name, "new.txt");

        assert!(!table.children(s1).contains(&file_ino));
        assert!(table.children(dir_ino).contains(&file_ino));
    }

    fn child(handle: u64, name: &str, is_dir: bool, size: u64) -> ChildInfo {
        ChildInfo {
            handle: ObjectHandle(handle),
            name: name.into(),
            is_dir,
            size,
            mtime: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn test_sync_children_keeps_inode_numbers_stable() {
        let mut table = InodeTable::new();
        let storage_ino = table.add_storage(StorageId(1), "Internal".into());
        table.sync_children(
            storage_ino,
            &[
                child(10, "a.txt", false, 100),
                child(11, "b.txt", false, 200),
            ],
        );
        let a = table.lookup(storage_ino, "a.txt").unwrap();

        // Re-listing the same directory with new handles (a fresh session) must
        // keep the inode number: open fds and the kernel cache depend on it.
        table.sync_children(
            storage_ino,
            &[
                child(77, "a.txt", false, 150),
                child(78, "b.txt", false, 200),
            ],
        );

        assert_eq!(table.lookup(storage_ino, "a.txt"), Some(a));
        let entry = table.get(a).unwrap();
        assert_eq!(
            entry.kind,
            InodeKind::File {
                handle: ObjectHandle(77)
            }
        );
        assert_eq!(entry.size, 150);
    }

    #[test]
    fn test_sync_children_adds_and_removes() {
        let mut table = InodeTable::new();
        let storage_ino = table.add_storage(StorageId(1), "Internal".into());
        table.sync_children(
            storage_ino,
            &[
                child(10, "stays.txt", false, 1),
                child(11, "goes.txt", false, 2),
            ],
        );
        let goes = table.lookup(storage_ino, "goes.txt").unwrap();

        table.sync_children(
            storage_ino,
            &[
                child(10, "stays.txt", false, 1),
                child(12, "new.txt", false, 3),
            ],
        );

        assert!(table.get(goes).is_none());
        assert_eq!(table.lookup(storage_ino, "goes.txt"), None);
        assert!(table.lookup(storage_ino, "new.txt").is_some());
        assert_eq!(table.children(storage_ino).len(), 2);
    }

    #[test]
    fn test_sync_children_replaces_when_kind_flips() {
        let mut table = InodeTable::new();
        let storage_ino = table.add_storage(StorageId(1), "Internal".into());
        table.sync_children(storage_ino, &[child(10, "thing", false, 5)]);
        let file_ino = table.lookup(storage_ino, "thing").unwrap();

        table.sync_children(storage_ino, &[child(11, "thing", true, 0)]);

        let dir_ino = table.lookup(storage_ino, "thing").unwrap();
        assert_ne!(
            dir_ino, file_ino,
            "a file replaced by a dir is a new object"
        );
        assert!(table.get(dir_ino).unwrap().is_dir());
    }

    #[test]
    fn test_generation_marks_handles_stale() {
        let mut table = InodeTable::new();
        let storage_ino = table.add_storage(StorageId(1), "Internal".into());
        table.sync_children(storage_ino, &[child(10, "a.txt", false, 100)]);
        let a = table.lookup(storage_ino, "a.txt").unwrap();
        assert!(table.is_fresh(a));

        table.bump_generation();

        assert!(!table.is_fresh(a), "handles from an old session are stale");
        assert!(
            table.is_fresh(storage_ino),
            "storage inodes carry no session-scoped handle"
        );
        assert!(table.is_fresh(FUSE_ROOT_INODE));

        table.set_handle(a, ObjectHandle(999));
        assert!(table.is_fresh(a));
        assert_eq!(
            table.get(a).unwrap().kind,
            InodeKind::File {
                handle: ObjectHandle(999)
            }
        );
    }

    #[test]
    fn test_set_storage_id_remaps_in_place() {
        let mut table = InodeTable::new();
        let storage_ino = table.add_storage(StorageId(1), "Internal".into());
        table.set_storage_id(storage_ino, StorageId(42));
        assert_eq!(
            table.get(storage_ino).unwrap().kind,
            InodeKind::Storage {
                storage_id: StorageId(42)
            }
        );
    }

    #[test]
    fn test_inode_uniqueness() {
        let mut table = InodeTable::new();
        let mtime = SystemTime::UNIX_EPOCH;
        let mut inodes = vec![FUSE_ROOT_INODE];
        inodes.push(table.add_storage(StorageId(1), "A".into()));
        inodes.push(table.add_storage(StorageId(2), "B".into()));
        inodes.push(table.add_object(inodes[1], ObjectHandle(1), "x".into(), false, 0, mtime));
        inodes.push(table.add_object(inodes[1], ObjectHandle(2), "y".into(), true, 0, mtime));

        let unique: std::collections::HashSet<u64> = inodes.iter().copied().collect();
        assert_eq!(unique.len(), inodes.len(), "all inodes must be unique");
    }

    #[test]
    fn test_nested_directories() {
        let mut table = InodeTable::new();
        let storage_ino = table.add_storage(StorageId(1), "Internal".into());
        let mtime = SystemTime::UNIX_EPOCH;

        let dcim = table.add_object(storage_ino, ObjectHandle(1), "DCIM".into(), true, 0, mtime);
        let camera = table.add_object(dcim, ObjectHandle(2), "Camera".into(), true, 0, mtime);
        let photo = table.add_object(
            camera,
            ObjectHandle(3),
            "IMG_001.jpg".into(),
            false,
            8192,
            mtime,
        );

        // Verify the chain: root -> storage -> DCIM -> Camera -> photo
        assert!(table.children(FUSE_ROOT_INODE).contains(&storage_ino));
        assert!(table.children(storage_ino).contains(&dcim));
        assert!(table.children(dcim).contains(&camera));
        assert!(table.children(camera).contains(&photo));

        // Lookup through the chain.
        assert_eq!(table.lookup(FUSE_ROOT_INODE, "Internal"), Some(storage_ino));
        assert_eq!(table.lookup(storage_ino, "DCIM"), Some(dcim));
        assert_eq!(table.lookup(dcim, "Camera"), Some(camera));
        assert_eq!(table.lookup(camera, "IMG_001.jpg"), Some(photo));

        let photo_entry = table.get(photo).unwrap();
        assert_eq!(photo_entry.parent, camera);
        assert_eq!(photo_entry.size, 8192);
    }

    #[test]
    fn test_get_mut() {
        let mut table = InodeTable::new();
        let storage_ino = table.add_storage(StorageId(1), "Internal".into());
        let mtime = SystemTime::UNIX_EPOCH;
        let file_ino = table.add_object(
            storage_ino,
            ObjectHandle(1),
            "f.txt".into(),
            false,
            100,
            mtime,
        );

        table.get_mut(file_ino).unwrap().size = 999;
        assert_eq!(table.get(file_ino).unwrap().size, 999);
    }
}
