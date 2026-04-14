use std::collections::HashMap;
use std::io;
use std::io::{Read, Seek, SeekFrom, Write};

use crate::error::MountError;

/// Per-file write buffer backed by a temp file.
pub struct FileBuffer {
    pub inode: u64,
    #[allow(dead_code)]
    pub original_size: u64,
    file: std::fs::File,
    len: u64,
    dirty: bool,
}

impl FileBuffer {
    pub fn new(inode: u64, original_size: u64) -> io::Result<Self> {
        Ok(Self {
            inode,
            original_size,
            file: tempfile::tempfile()?,
            len: 0,
            dirty: false,
        })
    }

    /// Write `data` at `offset`, growing the file and zero-filling gaps as needed.
    /// Returns the number of bytes written.
    pub fn write_at(&mut self, offset: i64, data: &[u8]) -> io::Result<u32> {
        let offset = offset as u64;
        let end = offset + data.len() as u64;

        // Zero-fill gaps if writing past current end
        if offset > self.len {
            self.file.seek(SeekFrom::Start(self.len))?;
            let gap = offset - self.len;
            let zeros = vec![0u8; gap as usize];
            self.file.write_all(&zeros)?;
        }

        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(data)?;

        if end > self.len {
            self.len = end;
        }
        self.dirty = true;
        Ok(data.len() as u32)
    }

    /// Read up to `size` bytes starting at `offset`. Returns fewer bytes if
    /// the offset is near or past the end.
    pub fn read_at(&mut self, offset: i64, size: u32) -> io::Result<Vec<u8>> {
        let offset = offset as u64;
        if offset >= self.len {
            return Ok(Vec::new());
        }
        let available = (self.len - offset).min(size as u64) as usize;
        let mut buf = vec![0u8; available];
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(&mut buf)?;
        Ok(buf)
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Consume the buffer and return the backing file.
    pub fn into_file(self) -> std::fs::File {
        self.file
    }
}

/// Manages in-progress file writes, mapping file handles to their buffers.
pub struct WriteBuffer {
    buffers: HashMap<u64, FileBuffer>,
}

impl Default for WriteBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl WriteBuffer {
    pub fn new() -> Self {
        Self {
            buffers: HashMap::new(),
        }
    }

    /// Register a new write buffer for the given file handle.
    pub fn open(&mut self, fh: u64, inode: u64, original_size: u64) -> io::Result<&mut FileBuffer> {
        use std::collections::hash_map::Entry;
        match self.buffers.entry(fh) {
            Entry::Occupied(e) => Ok(e.into_mut()),
            Entry::Vacant(e) => {
                let fb = FileBuffer::new(inode, original_size)?;
                Ok(e.insert(fb))
            }
        }
    }

    /// Write data at offset into the buffer for `fh`.
    pub fn write(&mut self, fh: u64, offset: i64, data: &[u8]) -> Result<u32, MountError> {
        let buf = self
            .buffers
            .get_mut(&fh)
            .ok_or_else(|| MountError::Other(format!("no buffer for file handle {fh}")))?;
        Ok(buf.write_at(offset, data)?)
    }

    /// Read data from the buffer for `fh`.
    pub fn read(&mut self, fh: u64, offset: i64, size: u32) -> Result<Vec<u8>, MountError> {
        let buf = self
            .buffers
            .get_mut(&fh)
            .ok_or_else(|| MountError::Other(format!("no buffer for file handle {fh}")))?;
        Ok(buf.read_at(offset, size)?)
    }

    /// Remove and return the buffer for flushing to MTP.
    pub fn flush(&mut self, fh: u64) -> Option<FileBuffer> {
        self.buffers.remove(&fh)
    }

    /// Alias for `flush` -- used at file close time.
    pub fn close(&mut self, fh: u64) -> Option<FileBuffer> {
        self.flush(fh)
    }

    pub fn is_open(&self, fh: u64) -> bool {
        self.buffers.contains_key(&fh)
    }

    /// Current buffered size for the given file handle.
    pub fn size(&self, fh: u64) -> Option<u64> {
        self.buffers.get(&fh).map(|b| b.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_buffer_empty() {
        let wb = WriteBuffer::new();
        assert!(!wb.is_open(1));
        assert!(wb.size(1).is_none());
    }

    #[test]
    fn test_open_creates_buffer() {
        let mut wb = WriteBuffer::new();
        wb.open(1, 100, 0).unwrap();
        assert!(wb.is_open(1));
        assert_eq!(wb.size(1), Some(0));
    }

    #[test]
    fn test_write_sequential() {
        let mut wb = WriteBuffer::new();
        wb.open(1, 100, 0).unwrap();
        wb.write(1, 0, b"hello").unwrap();
        wb.write(1, 5, b" world").unwrap();
        assert_eq!(wb.size(1), Some(11));
        let data = wb.read(1, 0, 11).unwrap();
        assert_eq!(&data, b"hello world");
    }

    #[test]
    fn test_write_at_offset() {
        let mut wb = WriteBuffer::new();
        wb.open(1, 100, 0).unwrap();
        wb.write(1, 5, b"abc").unwrap();
        assert_eq!(wb.size(1), Some(8));
        let data = wb.read(1, 0, 8).unwrap();
        assert_eq!(&data, b"\0\0\0\0\0abc");
    }

    #[test]
    fn test_write_overwrite() {
        let mut wb = WriteBuffer::new();
        wb.open(1, 100, 0).unwrap();
        wb.write(1, 0, b"hello").unwrap();
        wb.write(1, 1, b"ELL").unwrap();
        let data = wb.read(1, 0, 5).unwrap();
        assert_eq!(&data, b"hELLo");
    }

    #[test]
    fn test_read_back() {
        let mut wb = WriteBuffer::new();
        wb.open(1, 100, 0).unwrap();
        wb.write(1, 0, b"test data").unwrap();
        let data = wb.read(1, 5, 4).unwrap();
        assert_eq!(&data, b"data");
    }

    #[test]
    fn test_read_past_end() {
        let mut wb = WriteBuffer::new();
        wb.open(1, 100, 0).unwrap();
        wb.write(1, 0, b"short").unwrap();
        let data = wb.read(1, 3, 100).unwrap();
        assert_eq!(&data, b"rt");
    }

    #[test]
    fn test_read_empty() {
        let mut wb = WriteBuffer::new();
        wb.open(1, 100, 0).unwrap();
        let data = wb.read(1, 0, 10).unwrap();
        assert!(data.is_empty());
    }

    #[test]
    fn test_flush_returns_data() {
        let mut wb = WriteBuffer::new();
        wb.open(1, 100, 0).unwrap();
        wb.write(1, 0, b"flush me").unwrap();
        let fb = wb.flush(1).unwrap();
        assert_eq!(fb.inode, 100);
        let mut file = fb.into_file();
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).unwrap();
        assert_eq!(&contents, b"flush me");
    }

    #[test]
    fn test_flush_removes_buffer() {
        let mut wb = WriteBuffer::new();
        wb.open(1, 100, 0).unwrap();
        wb.write(1, 0, b"data").unwrap();
        wb.flush(1);
        assert!(!wb.is_open(1));
    }

    #[test]
    fn test_dirty_tracking() {
        let mut wb = WriteBuffer::new();
        let fb = wb.open(1, 100, 0).unwrap();
        assert!(!fb.is_dirty());
        wb.write(1, 0, b"x").unwrap();
        // Need to access via flush since we can't borrow after write through wb
        let fb = wb.flush(1).unwrap();
        assert!(fb.is_dirty());
    }

    #[test]
    fn test_multiple_files() {
        let mut wb = WriteBuffer::new();
        wb.open(1, 100, 0).unwrap();
        wb.open(2, 200, 0).unwrap();
        wb.write(1, 0, b"file1").unwrap();
        wb.write(2, 0, b"file2").unwrap();
        assert!(wb.is_open(1));
        assert!(wb.is_open(2));
        assert_eq!(wb.read(1, 0, 5).unwrap(), b"file1");
        assert_eq!(wb.read(2, 0, 5).unwrap(), b"file2");
    }

    #[test]
    fn test_write_nonexistent_fh() {
        let mut wb = WriteBuffer::new();
        let result = wb.write(999, 0, b"nope");
        assert!(result.is_err());
    }

    #[test]
    fn test_large_write() {
        let mut wb = WriteBuffer::new();
        wb.open(1, 100, 0).unwrap();
        let big = vec![0xABu8; 2 * 1024 * 1024]; // 2 MB
        let written = wb.write(1, 0, &big).unwrap();
        assert_eq!(written, 2 * 1024 * 1024);
        assert_eq!(wb.size(1), Some(2 * 1024 * 1024));
    }

    #[test]
    fn test_sparse_write() {
        let mut wb = WriteBuffer::new();
        wb.open(1, 100, 0).unwrap();
        wb.write(1, 1000, b"sparse").unwrap();
        assert_eq!(wb.size(1), Some(1006));
        let prefix = wb.read(1, 0, 1000).unwrap();
        assert!(prefix.iter().all(|&b| b == 0));
        let data = wb.read(1, 1000, 6).unwrap();
        assert_eq!(&data, b"sparse");
    }
}
