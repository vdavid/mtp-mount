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
        };
        let mut entries = HashMap::new();
        entries.insert(FUSE_ROOT_INODE, root);

        Self {
            entries,
            name_index: HashMap::new(),
            children_index: HashMap::new(),
            next_inode: 2,
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
        })
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

    /// Removes all children of the given parent (for cache invalidation).
    pub fn clear_children(&mut self, parent_inode: u64) {
        let child_inodes = self
            .children_index
            .remove(&parent_inode)
            .unwrap_or_default();
        for child_ino in child_inodes {
            if let Some(entry) = self.entries.remove(&child_ino) {
                self.name_index.remove(&(parent_inode, entry.name));
            }
            // Recursively clear grandchildren index entries (but not deeply).
            self.children_index.remove(&child_ino);
        }
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

    #[test]
    fn test_clear_children() {
        let mut table = InodeTable::new();
        let storage_ino = table.add_storage(StorageId(1), "Internal".into());
        let mtime = SystemTime::UNIX_EPOCH;
        let f1 = table.add_object(
            storage_ino,
            ObjectHandle(10),
            "a.txt".into(),
            false,
            100,
            mtime,
        );
        let f2 = table.add_object(
            storage_ino,
            ObjectHandle(11),
            "b.txt".into(),
            false,
            200,
            mtime,
        );

        table.clear_children(storage_ino);

        assert!(table.children(storage_ino).is_empty());
        assert!(table.get(f1).is_none());
        assert!(table.get(f2).is_none());
        assert_eq!(table.lookup(storage_ino, "a.txt"), None);
        assert_eq!(table.lookup(storage_ino, "b.txt"), None);
        // The storage itself should still exist.
        assert!(table.get(storage_ino).is_some());
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
