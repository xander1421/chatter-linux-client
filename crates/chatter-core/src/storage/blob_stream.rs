//! Streaming BLOB I/O for SQLite.
//!
//! Provides efficient streaming access to large BLOBs without loading
//! the entire content into memory. Uses SQLite's incremental BLOB I/O API.
//!
//! ## Performance
//!
//! For a 200MB attachment:
//! - Old: Load entire blob into Vec<u8> → 200MB memory, slow
//! - New: Stream in 64KB chunks → 64KB memory, fast startup
//!
//! ## Usage
//!
//! ```ignore
//! let stream = repo.open_encrypted_content_stream(&attachment_id)?;
//! let mut chunk = [0u8; 64 * 1024];
//! while let Ok(n) = stream.read(&mut chunk) {
//!     if n == 0 { break; }
//!     process(&chunk[..n]);
//! }
//! ```

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use rusqlite::blob::Blob;
use rusqlite::Connection;

use crate::error::StorageError;
use crate::storage::Database;

/// A streaming reader for SQLite BLOBs.
///
/// Wraps rusqlite's `Blob` type and implements `Read` + `Seek` for
/// efficient incremental access to large binary content.
pub struct BlobStream<'conn> {
    blob: Blob<'conn>,
    size: usize,
    position: usize,
}

impl<'conn> BlobStream<'conn> {
    /// Open a BLOB for streaming read access.
    ///
    /// # Arguments
    ///
    /// * `conn` - Database connection (must outlive the stream)
    /// * `table` - Table name containing the BLOB
    /// * `column` - Column name of the BLOB
    /// * `rowid` - Row ID of the record
    ///
    /// # Returns
    ///
    /// A `BlobStream` that can be read incrementally.
    pub fn open(
        conn: &'conn Connection,
        table: &str,
        column: &str,
        rowid: i64,
    ) -> Result<Self, StorageError> {
        let blob = conn
            .blob_open(
                rusqlite::DatabaseName::Main,
                table,
                column,
                rowid,
                true, // read-only
            )
            .map_err(StorageError::Database)?;

        let size = blob.len();

        Ok(Self {
            blob,
            size,
            position: 0,
        })
    }

    /// Get the total size of the BLOB in bytes.
    #[inline]
    pub fn size(&self) -> usize {
        self.size
    }

    /// Get the current read position.
    #[inline]
    pub fn position(&self) -> usize {
        self.position
    }

    /// Read a chunk at a specific offset without changing position.
    ///
    /// Useful for random access reads (e.g., seeking in video).
    pub fn read_at(&self, buf: &mut [u8], offset: usize) -> Result<usize, StorageError> {
        if offset >= self.size {
            return Ok(0);
        }

        let available = self.size - offset;
        let to_read = buf.len().min(available);

        self.blob
            .read_at_exact(&mut buf[..to_read], offset)
            .map_err(StorageError::Database)?;

        Ok(to_read)
    }

    /// Check if we've reached the end of the BLOB.
    #[inline]
    pub fn is_eof(&self) -> bool {
        self.position >= self.size
    }

    /// Remaining bytes to read.
    #[inline]
    pub fn remaining(&self) -> usize {
        self.size.saturating_sub(self.position)
    }
}

impl Read for BlobStream<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.position >= self.size {
            return Ok(0);
        }

        let available = self.size - self.position;
        let to_read = buf.len().min(available);

        self.blob
            .read_at_exact(&mut buf[..to_read], self.position)
            .map_err(std::io::Error::other)?;

        self.position += to_read;
        Ok(to_read)
    }
}

impl Seek for BlobStream<'_> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::End(offset) => self.size as i64 + offset,
            SeekFrom::Current(offset) => self.position as i64 + offset,
        };

        if new_pos < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek to negative position",
            ));
        }

        self.position = new_pos as usize;
        Ok(self.position as u64)
    }
}

/// Information about a BLOB needed to open a stream.
#[derive(Debug, Clone)]
pub struct BlobInfo {
    /// SQLite rowid of the record containing the BLOB.
    pub rowid: i64,
    /// Size of the BLOB in bytes.
    pub size: usize,
}

/// An owned blob stream that manages its own database connection.
///
/// This is useful when you need to hold a blob stream across async boundaries
/// or when the connection pool doesn't have enough readers available.
///
/// The connection is opened fresh and closed when this struct is dropped.
pub struct OwnedBlobStream {
    /// The blob handle (must be dropped before connection).
    blob: Option<rusqlite::blob::Blob<'static>>,
    /// The owned connection (kept alive for blob lifetime).
    /// SAFETY: This is a raw pointer because Blob borrows from Connection,
    /// creating a self-referential struct. We ensure:
    /// 1. Connection is never moved after blob is opened
    /// 2. Blob is dropped before connection in Drop impl
    _connection: Box<Connection>,
    size: usize,
    position: usize,
}

// SAFETY: OwnedBlobStream is Send because:
// - Connection is Send
// - Blob is !Send due to lifetime, but we own the connection
// - We never expose the inner connection
unsafe impl Send for OwnedBlobStream {}

impl OwnedBlobStream {
    /// Open an owned blob stream with a dedicated connection.
    ///
    /// Opens a fresh read-only connection to the database specifically for
    /// this blob stream. The connection is closed when the stream is dropped.
    pub fn open(
        database: &Arc<Database>,
        table: &str,
        column: &str,
        rowid: i64,
    ) -> Result<Self, StorageError> {
        // Open a fresh read-only connection for this blob
        let conn = database.open_reader_connection()?;
        let conn = Box::new(conn);

        // SAFETY: We're creating a self-referential struct here.
        // The blob borrows from conn, but we keep conn alive as long as blob exists.
        // We ensure blob is dropped before conn in the Drop impl.
        let conn_ptr = conn.as_ref() as *const Connection;
        let blob = unsafe {
            (*conn_ptr).blob_open(
                rusqlite::DatabaseName::Main,
                table,
                column,
                rowid,
                true, // read-only
            )
        }
        .map_err(StorageError::Database)?;

        let size = blob.len();

        // Transmute the lifetime to 'static since we own the connection
        let blob: rusqlite::blob::Blob<'static> = unsafe { std::mem::transmute(blob) };

        Ok(Self {
            blob: Some(blob),
            _connection: conn,
            size,
            position: 0,
        })
    }

    /// Get the total size of the BLOB in bytes.
    #[inline]
    pub fn size(&self) -> usize {
        self.size
    }

    /// Get the current read position.
    #[inline]
    pub fn position(&self) -> usize {
        self.position
    }

    /// Check if we've reached the end of the BLOB.
    #[inline]
    pub fn is_eof(&self) -> bool {
        self.position >= self.size
    }

    /// Remaining bytes to read.
    #[inline]
    pub fn remaining(&self) -> usize {
        self.size.saturating_sub(self.position)
    }

    /// Read a chunk at a specific offset without changing position.
    pub fn read_at(&self, buf: &mut [u8], offset: usize) -> Result<usize, StorageError> {
        if offset >= self.size {
            return Ok(0);
        }

        let blob = self.blob.as_ref().ok_or_else(|| {
            StorageError::InvalidData("blob used after drop".into())
        })?;
        let available = self.size - offset;
        let to_read = buf.len().min(available);

        blob.read_at_exact(&mut buf[..to_read], offset)
            .map_err(StorageError::Database)?;

        Ok(to_read)
    }
}

impl Drop for OwnedBlobStream {
    fn drop(&mut self) {
        // IMPORTANT: Drop blob before connection to avoid use-after-free
        self.blob.take();
        // _connection is dropped automatically after this
    }
}

impl Read for OwnedBlobStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.position >= self.size {
            return Ok(0);
        }

        let blob = self.blob.as_ref().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "blob used after drop")
        })?;
        let available = self.size - self.position;
        let to_read = buf.len().min(available);

        blob.read_at_exact(&mut buf[..to_read], self.position)
            .map_err(std::io::Error::other)?;

        self.position += to_read;
        Ok(to_read)
    }
}

impl Seek for OwnedBlobStream {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::End(offset) => self.size as i64 + offset,
            SeekFrom::Current(offset) => self.position as i64 + offset,
        };

        if new_pos < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek to negative position",
            ));
        }

        self.position = new_pos as usize;
        Ok(self.position as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE test_blobs (id INTEGER PRIMARY KEY, data BLOB)",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn test_blob_stream_read() {
        let conn = setup_test_db();

        // Insert test data
        let data: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
        conn.execute("INSERT INTO test_blobs (data) VALUES (?1)", [&data])
            .unwrap();

        // Open stream
        let stream = BlobStream::open(&conn, "test_blobs", "data", 1).unwrap();
        assert_eq!(stream.size(), 1000);

        // Read in chunks
        let mut result = Vec::new();
        let mut buf = [0u8; 100];
        let mut stream = stream;

        loop {
            let n = stream.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            result.extend_from_slice(&buf[..n]);
        }

        assert_eq!(result, data);
    }

    #[test]
    fn test_blob_stream_seek() {
        let conn = setup_test_db();

        let data: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
        conn.execute("INSERT INTO test_blobs (data) VALUES (?1)", [&data])
            .unwrap();

        let mut stream = BlobStream::open(&conn, "test_blobs", "data", 1).unwrap();

        // Seek to middle
        stream.seek(SeekFrom::Start(500)).unwrap();
        assert_eq!(stream.position(), 500);

        // Read from middle
        let mut buf = [0u8; 10];
        stream.read_exact(&mut buf).unwrap();
        assert_eq!(buf, data[500..510]);
    }

    #[test]
    fn test_blob_stream_read_at() {
        let conn = setup_test_db();

        let data: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
        conn.execute("INSERT INTO test_blobs (data) VALUES (?1)", [&data])
            .unwrap();

        let stream = BlobStream::open(&conn, "test_blobs", "data", 1).unwrap();

        // Random access read
        let mut buf = [0u8; 10];
        let n = stream.read_at(&mut buf, 750).unwrap();
        assert_eq!(n, 10);
        assert_eq!(buf, data[750..760]);

        // Position unchanged
        assert_eq!(stream.position(), 0);
    }
}
