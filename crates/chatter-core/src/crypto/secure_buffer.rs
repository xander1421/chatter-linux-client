//! Secure memory buffer with automatic zeroization.
//!
//! Provides a buffer type that securely wipes its contents on drop,
//! preventing sensitive data (like decrypted attachments) from persisting
//! in memory after use.

use std::io::{Read, Seek};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Buffer that securely wipes contents when dropped.
///
/// Used for holding decrypted attachment data in memory. The buffer
/// automatically zeroizes its contents when it goes out of scope,
/// ensuring no plaintext data persists in memory.
///
/// # Example
///
/// ```ignore
/// let buffer = SecureBuffer::new(decrypted_data);
/// // Use buffer for playback
/// audio_player.load_from_memory(buffer.as_slice())?;
/// // buffer dropped here → memory zeroized automatically
/// ```
#[derive(ZeroizeOnDrop)]
pub struct SecureBuffer {
    #[zeroize(drop)]
    data: Vec<u8>,
}

impl SecureBuffer {
    /// Create a new secure buffer from data.
    ///
    /// Takes ownership of the data to prevent copies.
    #[must_use]
    pub fn new(data: Vec<u8>) -> Self {
        Self { data }
    }

    /// Create an empty secure buffer.
    #[must_use]
    pub fn empty() -> Self {
        Self { data: Vec::new() }
    }

    /// Get the buffer contents as a slice.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.data
    }

    /// Get the length of the buffer in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Check if the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Create a cursor for reading the buffer.
    ///
    /// Useful for APIs that require `Read + Seek` traits. Returns a
    /// [`SecureCursor`] (zeroizes its `Vec<u8>` on drop) rather than a
    /// plain `std::io::Cursor`, so the clone of decrypted material doesn't
    /// leak into freed allocator memory on `Drop`.
    ///
    /// Note: the cursor contains a clone of the data; only the clone is
    /// zeroized when the cursor drops. The original `SecureBuffer` still
    /// owns + zeroizes its own copy.
    #[must_use]
    pub fn cursor(&self) -> SecureCursor {
        SecureCursor::new(self.data.clone())
    }

    /// Convert to a cursor, consuming the buffer.
    ///
    /// This transfers ownership of the data to the cursor.
    /// The cursor will NOT automatically zeroize on drop!
    /// Use this only when the consuming API will handle cleanup.
    #[must_use]
    pub fn into_cursor(self) -> SecureCursor {
        // Prevent the automatic zeroize by taking ownership
        let data = std::mem::ManuallyDrop::new(self);
        // SAFETY: We're taking ownership and wrapping in SecureCursor which will zeroize
        let inner = unsafe { std::ptr::read(&data.data) };
        SecureCursor::new(inner)
    }

    /// Explicitly zeroize the buffer contents.
    ///
    /// Called automatically on drop, but can be called manually
    /// to clear the buffer earlier.
    pub fn zeroize(&mut self) {
        self.data.zeroize();
    }
}

impl AsRef<[u8]> for SecureBuffer {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

impl From<Vec<u8>> for SecureBuffer {
    fn from(data: Vec<u8>) -> Self {
        Self::new(data)
    }
}

/// A cursor that wraps a secure buffer and zeroizes on drop.
///
/// Implements `Read + Seek` for use with audio/video decoders.
#[derive(ZeroizeOnDrop)]
pub struct SecureCursor {
    #[zeroize(drop)]
    data: Vec<u8>,
    position: usize,
}

impl SecureCursor {
    /// Create a new secure cursor from data.
    fn new(data: Vec<u8>) -> Self {
        Self { data, position: 0 }
    }

    /// Get the current position.
    #[must_use]
    pub fn position(&self) -> u64 {
        self.position as u64
    }

    /// Get the total length.
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Check if empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

impl Read for SecureCursor {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.data.len().saturating_sub(self.position);
        let to_read = buf.len().min(remaining);
        if to_read == 0 {
            return Ok(0);
        }
        buf[..to_read].copy_from_slice(&self.data[self.position..self.position + to_read]);
        self.position += to_read;
        Ok(to_read)
    }
}

impl Seek for SecureCursor {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let new_pos = match pos {
            std::io::SeekFrom::Start(n) => n as i64,
            std::io::SeekFrom::End(n) => self.data.len() as i64 + n,
            std::io::SeekFrom::Current(n) => self.position as i64 + n,
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

    #[test]
    fn test_secure_buffer_basic() {
        let data = vec![1, 2, 3, 4, 5];
        let buffer = SecureBuffer::new(data);
        assert_eq!(buffer.len(), 5);
        assert_eq!(buffer.as_slice(), &[1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_secure_buffer_empty() {
        let buffer = SecureBuffer::empty();
        assert!(buffer.is_empty());
        assert_eq!(buffer.len(), 0);
    }

    #[test]
    fn test_secure_cursor_read() {
        let data = vec![1, 2, 3, 4, 5];
        let buffer = SecureBuffer::new(data);
        let mut cursor = buffer.into_cursor();

        let mut buf = [0u8; 3];
        let n = cursor.read(&mut buf).unwrap();
        assert_eq!(n, 3);
        assert_eq!(&buf, &[1, 2, 3]);

        let n = cursor.read(&mut buf).unwrap();
        assert_eq!(n, 2);
        assert_eq!(&buf[..2], &[4, 5]);
    }

    #[test]
    fn test_secure_cursor_seek() {
        let data = vec![1, 2, 3, 4, 5];
        let buffer = SecureBuffer::new(data);
        let mut cursor = buffer.into_cursor();

        cursor.seek(std::io::SeekFrom::Start(2)).unwrap();
        assert_eq!(cursor.position(), 2);

        cursor.seek(std::io::SeekFrom::Current(1)).unwrap();
        assert_eq!(cursor.position(), 3);

        cursor.seek(std::io::SeekFrom::End(-1)).unwrap();
        assert_eq!(cursor.position(), 4);
    }
}
