//! `InodeTable`, the mount's map between FUSE inodes and MTP objects, driven the way `fs.rs`
//! drives it, with its indexes checked after every step.
//!
//! The input is a program of four-byte steps: `op`, then three argument bytes. Inodes are
//! picked from the ones the program has seen, and names from a small set, so collisions
//! (the interesting case) happen often.
#![no_main]
use std::time::SystemTime;

use libfuzzer_sys::fuzz_target;
use mtp_mount::inode::{ChildInfo, InodeKind, InodeTable, FUSE_ROOT_INODE};
use mtp_rs::{ObjectHandle, StorageId};

const NAMES: [&str; 5] = ["a", "b", "DCIM", "photo.jpg", "a "];

fn name(byte: u8) -> String {
    NAMES[usize::from(byte) % NAMES.len()].to_string()
}

fuzz_target!(|data: &[u8]| {
    let mut table = InodeTable::new();
    // Every inode the table handed out, live or not, so steps can name stale ones too.
    let mut seen = vec![FUSE_ROOT_INODE];
    let pick = |seen: &[u64], byte: u8| seen[usize::from(byte) % seen.len()];
    let epoch = SystemTime::UNIX_EPOCH;

    // At most 256 steps, so a run stays quick however long the input is.
    for step in data.chunks_exact(4).take(256) {
        let (a, b, c) = (step[1], step[2], step[3]);
        match step[0] % 8 {
            0 => {
                // A storage, as the mount adds one per device storage.
                seen.push(table.add_storage_at(StorageId(u64::from(a)), name(b), epoch));
            }
            1 => {
                // create or mkdir: `fs.rs` checks the parent is a directory first.
                let parent = pick(&seen, a);
                if table.get(parent).is_some_and(|e| e.is_dir()) {
                    let handle = ObjectHandle(u64::from(b));
                    seen.push(table.add_object(parent, handle, name(c), c & 1 == 1, 0, epoch));
                }
            }
            2 => {
                // A directory listing: up to four children, each from one byte.
                let parent = pick(&seen, a);
                if table.get(parent).is_some_and(|e| e.is_dir()) {
                    let children: Vec<ChildInfo> = [b, c, b ^ c, b.wrapping_add(c)]
                        .iter()
                        .take(usize::from(a % 5))
                        .map(|&x| ChildInfo {
                            handle: ObjectHandle(u64::from(x)),
                            name: name(x >> 3),
                            is_dir: x & 1 == 1,
                            size: u64::from(x),
                            mtime: epoch,
                        })
                        .collect();
                    table.sync_children(parent, &children);
                    seen.extend(table.children(parent));
                    // Every reported name is there afterwards, and nothing else is.
                    let mut listed: Vec<String> = table
                        .children(parent)
                        .iter()
                        .map(|&i| table.get(i).expect("a listed child exists").name.clone())
                        .collect();
                    let mut reported: Vec<String> =
                        children.iter().map(|c| c.name.clone()).collect();
                    listed.sort();
                    reported.sort();
                    reported.dedup();
                    assert_eq!(
                        listed, reported,
                        "a listing doesn't match what the device sent"
                    );
                }
            }
            3 => {
                // unlink or rmdir, which `fs.rs` only does to files and directories.
                let ino = pick(&seen, a);
                if table.get(ino).is_some_and(|e| has_handle(&e.kind)) {
                    table.remove(ino);
                    assert!(table.get(ino).is_none(), "a removed inode is still there");
                }
            }
            4 => {
                // rename: `fs.rs` only moves files and directories, into a directory, and the
                // kernel refuses to move a directory into itself or below itself.
                let ino = pick(&seen, a);
                let new_parent = pick(&seen, b);
                let movable = table.get(ino).is_some_and(|e| has_handle(&e.kind));
                if movable
                    && table.get(new_parent).is_some_and(|e| e.is_dir())
                    && !is_at_or_below(&table, new_parent, ino)
                {
                    let new_name = name(c);
                    table.rename(ino, new_parent, new_name.clone());
                    assert_eq!(
                        table.lookup(new_parent, &new_name),
                        Some(ino),
                        "a renamed entry can't be found"
                    );
                }
            }
            5 => {
                // A reconnect: every handle goes stale, and re-resolving one makes it fresh.
                table.bump_generation();
                let ino = pick(&seen, a);
                table.set_handle(ino, ObjectHandle(u64::from(b)));
                if table.get(ino).is_some() {
                    // Storages and the root carry no session handle, so they're always fresh.
                    assert!(table.is_fresh(ino), "a re-resolved inode is stale");
                }
            }
            6 => {
                // A device event names a handle; the parent it finds must hold an object with it.
                let handle = ObjectHandle(u64::from(a));
                if let Some(parent) = table.find_parent_by_handle(handle) {
                    let holds = seen.iter().any(|&i| {
                        table.get(i).is_some_and(|e| {
                            e.parent == parent
                                && matches!(e.kind, InodeKind::File { handle: h } | InodeKind::Directory { handle: h } if h == handle)
                        })
                    });
                    assert!(
                        holds,
                        "find_parent_by_handle named a parent without that handle"
                    );
                }
            }
            _ => {
                // A lookup by name agrees with the entry it returns.
                let parent = pick(&seen, a);
                let wanted = name(b);
                if let Some(ino) = table.lookup(parent, &wanted) {
                    let entry = table.get(ino).expect("lookup returned a missing inode");
                    assert_eq!(
                        (entry.parent, entry.name.as_str()),
                        (parent, wanted.as_str())
                    );
                }
            }
        }
        check_indexes(&table, &seen);
    }
});

fn has_handle(kind: &InodeKind) -> bool {
    matches!(kind, InodeKind::File { .. } | InodeKind::Directory { .. })
}

/// Whether `ino` is `ancestor` or sits somewhere below it.
fn is_at_or_below(table: &InodeTable, mut ino: u64, ancestor: u64) -> bool {
    // A chain can't be longer than the table, and the root is its own parent.
    for _ in 0..1024 {
        if ino == ancestor {
            return true;
        }
        match table.get(ino) {
            Some(entry) if entry.parent != ino => ino = entry.parent,
            _ => return false,
        }
    }
    false
}

/// For every live entry under a live parent: `lookup` by its name finds it, and its parent's
/// `children` lists it exactly once. And every listed child is live and names that parent.
/// (An entry whose parent was removed is unreachable by design: `remove` keeps descendants,
/// since an open file descriptor may still use them.)
fn check_indexes(table: &InodeTable, seen: &[u64]) {
    for &ino in seen {
        let Some(entry) = table.get(ino) else {
            continue;
        };
        if ino == FUSE_ROOT_INODE || table.get(entry.parent).is_none() {
            continue;
        }
        assert_eq!(
            table.lookup(entry.parent, &entry.name),
            Some(ino),
            "{ino} can't be looked up by name"
        );
        let count = table
            .children(entry.parent)
            .iter()
            .filter(|&&c| c == ino)
            .count();
        assert_eq!(count, 1, "{ino} is listed {count} times under its parent");
        for child in table.children(ino) {
            let child_entry = table.get(child).expect("a listed child is gone");
            assert_eq!(child_entry.parent, ino, "a child names a different parent");
        }
    }
}
