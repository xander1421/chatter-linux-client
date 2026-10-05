//! Private, seekable video plaintext. No temporary files or external grants.
use chatter_core::{prelude::SecureBuffer, services::attachment::CbcRangeDecryptor};
use std::sync::Arc;

/// Android always uses authenticated ranges, including absent/incorrect size
/// metadata. Desktop retains its existing small-video memory path.
pub fn use_video_ranges(mime: &str, size: u64, android: bool) -> bool {
    mime.starts_with("video/") && (android || size >= 8 * 1024 * 1024)
}

pub enum VideoSource {
    Memory(SecureBuffer),
    Range(Arc<CbcRangeDecryptor>),
}
impl VideoSource {
    pub fn len(&self) -> usize {
        match self {
            Self::Memory(b) => b.as_slice().len(),
            Self::Range(d) => d.plaintext_size(),
        }
    }
    pub fn read(&self, position: i64, count: usize) -> Option<Vec<u8>> {
        let offset = usize::try_from(position).ok()?;
        if offset >= self.len() {
            return Some(Vec::new());
        }
        // Bound every native decoder request, including malicious media inputs.
        let count = count.min(1024 * 1024).min(self.len() - offset);
        Some(match self {
            Self::Memory(b) => b.as_slice()[offset..offset + count].to_vec(),
            Self::Range(d) => d.read_range(offset, count),
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn random_reads_seek_eof_and_invalid_offsets() {
        let source = VideoSource::Memory(SecureBuffer::new(vec![1, 2, 3, 4]));
        assert_eq!(source.read(2, 99), Some(vec![3, 4]));
        assert_eq!(source.read(0, 2), Some(vec![1, 2]));
        assert_eq!(source.read(4, 1), Some(vec![]));
        assert_eq!(source.read(i64::MAX, usize::MAX), Some(vec![]));
        assert_eq!(source.read(-1, 1), None);
        assert_eq!(source.read(0, 0), Some(vec![]));
    }
    #[test]
    fn android_video_uses_ranges_regardless_of_advertised_size() {
        for size in [0, 1, 64 * 1024 * 1024, 2 * 1024 * 1024 * 1024] {
            assert!(use_video_ranges("video/mp4", size, true));
        }
        assert!(!use_video_ranges("image/png", u64::MAX, true));
        assert!(!use_video_ranges("video/mp4", 1, false));
        assert!(use_video_ranges("video/mp4", 8 * 1024 * 1024, false));
    }
    #[test]
    fn requests_are_bounded() {
        let source = VideoSource::Memory(SecureBuffer::new(vec![7; 2 * 1024 * 1024]));
        assert_eq!(source.read(0, usize::MAX).unwrap().len(), 1024 * 1024);
    }
}
