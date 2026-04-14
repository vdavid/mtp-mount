use std::collections::HashMap;

use crate::error::MountError;

/// Per-file write buffer holding data until flush/close.
pub struct FileBuffer {
    pub inode: u64,
    pub original_size: u64,
    data: Vec<u8>,
    dirty: bool,
}

impl FileBuffer {
    pub fn new(inode: u64, original_size: u64) -> Self {
        Self {
            inode,
            original_size,
            data: Vec::new(),
            dirty: false,
        }
    }

    /// Write `data` at `offset`, growing the buffer and zero-filling gaps as needed.
    /// Returns the number of bytes written.
    pub fn write_at(&mut self, offset: i64, data: &[u8]) -> u32 {
        let offset = offset as usize;
        let end = offset + data.len();
        if end > self.data.len() {
            self.data.resize(end, 0);
        }
        self.data[offset..end].copy_from_slice(data);
        self.dirty = true;
        data.len() as u32
    }

    /// Read up to `size` bytes starting at `offset`. Returns fewer bytes if
    /// the offset is near or past the end.
    pub fn read_at(&self, offset: i64, size: u32) -> Vec<u8> {
        let offset = offset as usize;
        if offset >= self.data.len() {
            return Vec::new();
        }
        let end = (offset + size as usize).min(self.data.len());
        self.data[offset..end].to_vec()
    }

    pub fn len(&self) -> u64 {
        self.data.len() as u64
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Consume the buffer and return the raw data.
    pub fn into_data(self) -> Vec<u8> {
        self.data
    }
}

/// Manages in-progress file writes, mapping file handles to their buffers.
pub struct WriteBuffer {
    buffers: HashMap<u64, FileBuffer>,
}

impl WriteBuffer {
    pub fn new() -> Self {
        Self {
            buffers: HashMap::new(),
        }
    }

    /// Register a new write buffer for the given file handle.
    pub fn open(&mut self, fh: u64, inode: u64, original_size: u64) -> &mut FileBuffer {
        self.buffers
            .entry(fh)
            .or_insert_with(|| FileBuffer::new(inode, original_size))
    }

    /// Write data at offset into the buffer for `fh`.
    pub fn write(&mut self, fh: u64, offset: i64, data: &[u8]) -> Result<u32, MountError> {
        let buf = self
            .buffers
            .get_mut(&fh)
            .ok_or_else(|| MountError::Other(format!("no buffer for file handle {fh}")))?;
        Ok(buf.write_at(offset, data))
    }

    /// Read data from the buffer for `fh`.
    pub fn read(&self, fh: u64, offset: i64, size: u32) -> Result<Vec<u8>, MountError> {
        let buf = self
            .buffers
            .get(&fh)
            .ok_or_else(|| MountError::Other(format!("no buffer for file handle {fh}")))?;
        Ok(buf.read_at(offset, size))
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
        wb.open(1, 100, 0);
        assert!(wb.is_open(1));
        assert_eq!(wb.size(1), Some(0));
    }

    #[test]
    fn test_write_sequential() {
        let mut wb = WriteBuffer::new();
        wb.open(1, 100, 0);
        wb.write(1, 0, b"hello").unwrap();
        wb.write(1, 5, b" world").unwrap();
        assert_eq!(wb.size(1), Some(11));
        let data = wb.read(1, 0, 11).unwrap();
        assert_eq!(&data, b"hello world");
    }

    #[test]
    fn test_write_at_offset() {
        let mut wb = WriteBuffer::new();
        wb.open(1, 100, 0);
        wb.write(1, 5, b"abc").unwrap();
        assert_eq!(wb.size(1), Some(8));
        let data = wb.read(1, 0, 8).unwrap();
        assert_eq!(&data, b"\0\0\0\0\0abc");
    }

    #[test]
    fn test_write_overwrite() {
        let mut wb = WriteBuffer::new();
        wb.open(1, 100, 0);
        wb.write(1, 0, b"hello").unwrap();
        wb.write(1, 1, b"ELL").unwrap();
        let data = wb.read(1, 0, 5).unwrap();
        assert_eq!(&data, b"hELLo");
    }

    #[test]
    fn test_read_back() {
        let mut wb = WriteBuffer::new();
        wb.open(1, 100, 0);
        wb.write(1, 0, b"test data").unwrap();
        let data = wb.read(1, 5, 4).unwrap();
        assert_eq!(&data, b"data");
    }

    #[test]
    fn test_read_past_end() {
        let mut wb = WriteBuffer::new();
        wb.open(1, 100, 0);
        wb.write(1, 0, b"short").unwrap();
        let data = wb.read(1, 3, 100).unwrap();
        assert_eq!(&data, b"rt");
    }

    #[test]
    fn test_read_empty() {
        let mut wb = WriteBuffer::new();
        wb.open(1, 100, 0);
        let data = wb.read(1, 0, 10).unwrap();
        assert!(data.is_empty());
    }

    #[test]
    fn test_flush_returns_data() {
        let mut wb = WriteBuffer::new();
        wb.open(1, 100, 0);
        wb.write(1, 0, b"flush me").unwrap();
        let fb = wb.flush(1).unwrap();
        assert_eq!(fb.inode, 100);
        assert_eq!(fb.into_data(), b"flush me");
    }

    #[test]
    fn test_flush_removes_buffer() {
        let mut wb = WriteBuffer::new();
        wb.open(1, 100, 0);
        wb.write(1, 0, b"data").unwrap();
        wb.flush(1);
        assert!(!wb.is_open(1));
    }

    #[test]
    fn test_dirty_tracking() {
        let mut wb = WriteBuffer::new();
        let fb = wb.open(1, 100, 0);
        assert!(!fb.is_dirty());
        wb.write(1, 0, b"x").unwrap();
        // Need to access via flush since we can't borrow after write through wb
        let fb = wb.flush(1).unwrap();
        assert!(fb.is_dirty());
    }

    #[test]
    fn test_multiple_files() {
        let mut wb = WriteBuffer::new();
        wb.open(1, 100, 0);
        wb.open(2, 200, 0);
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
        wb.open(1, 100, 0);
        let big = vec![0xABu8; 2 * 1024 * 1024]; // 2 MB
        let written = wb.write(1, 0, &big).unwrap();
        assert_eq!(written, 2 * 1024 * 1024);
        assert_eq!(wb.size(1), Some(2 * 1024 * 1024));
    }

    #[test]
    fn test_sparse_write() {
        let mut wb = WriteBuffer::new();
        wb.open(1, 100, 0);
        wb.write(1, 1000, b"sparse").unwrap();
        assert_eq!(wb.size(1), Some(1006));
        let prefix = wb.read(1, 0, 1000).unwrap();
        assert!(prefix.iter().all(|&b| b == 0));
        let data = wb.read(1, 1000, 6).unwrap();
        assert_eq!(&data, b"sparse");
    }
}
