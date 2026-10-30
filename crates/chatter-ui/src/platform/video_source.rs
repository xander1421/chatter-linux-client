//! Private, seekable video plaintext. No temporary files or external grants.
#[cfg(any(target_os = "android", test))]
use chatter_core::{prelude::SecureBuffer, services::attachment::CbcRangeDecryptor};
#[cfg(any(target_os = "android", test))]
use std::sync::Arc;

/// Android always uses authenticated ranges, including absent/incorrect size
/// metadata. Desktop retains its existing small-video memory path.
pub fn use_video_ranges(mime: &str, size: u64, android: bool) -> bool {
    mime.starts_with("video/") && (android || size >= 8 * 1024 * 1024)
}

// Android's media and thumbnail decoders are the only readers.
#[cfg(any(target_os = "android", test))]
pub enum VideoSource {
    Memory(SecureBuffer),
    Range(Arc<CbcRangeDecryptor>),
}
#[cfg(any(target_os = "android", test))]
impl VideoSource {
    pub fn len(&self) -> usize {
        match self {
            Self::Memory(b) => b.as_slice().len(),
            Self::Range(d) => d.plaintext_size(),
        }
    }
    #[cfg(test)]
    pub fn read(&self, position: i64, count: usize) -> Option<Vec<u8>> {
        let mut bytes = vec![0; count.min(1024 * 1024)];
        let len = self.read_into(position, &mut bytes)?;
        bytes.truncate(len);
        Some(bytes)
    }
    pub fn read_into(&self, position: i64, output: &mut [u8]) -> Option<usize> {
        let offset = usize::try_from(position).ok()?;
        if offset >= self.len() {
            return Some(0);
        }
        // Bound every native decoder request, including malicious media inputs.
        let count = output.len().min(1024 * 1024).min(self.len() - offset);
        Some(match self {
            Self::Memory(b) => {
                output[..count].copy_from_slice(&b.as_slice()[offset..offset + count]);
                count
            }
            Self::Range(d) => d.read_range_into(offset, &mut output[..count]),
        })
    }
}
/// Independent thumbnail read budget, including empty reads and repeated seeks.
#[cfg(any(target_os = "android", test))]
pub struct ThumbnailSource {
    video: VideoSource,
    deadline: std::time::Instant,
    remaining_bytes: usize,
    remaining_reads: usize,
    owner: Arc<dyn Fn() -> bool + Send + Sync>,
}
#[cfg(any(target_os = "android", test))]
impl ThumbnailSource {
    #[cfg(test)]
    pub fn new(video: VideoSource, deadline: std::time::Instant) -> Self {
        Self::new_guarded(video, deadline, Arc::new(|| true))
    }
    pub fn new_guarded(
        video: VideoSource,
        deadline: std::time::Instant,
        owner: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        Self {
            video,
            deadline,
            remaining_bytes: 32 * 1024 * 1024,
            remaining_reads: 4096,
            owner,
        }
    }
    pub fn len(&self) -> Option<usize> {
        ((self.owner)() && std::time::Instant::now() < self.deadline).then(|| self.video.len())
    }
    #[cfg(test)]
    pub fn read(&mut self, position: i64, count: usize) -> Option<Vec<u8>> {
        let mut bytes = vec![0; count.min(1024 * 1024)];
        let len = self.read_into(position, &mut bytes)?;
        bytes.truncate(len);
        Some(bytes)
    }
    pub fn read_into(&mut self, position: i64, output: &mut [u8]) -> Option<usize> {
        if self.len().is_none() || self.remaining_reads == 0 || self.remaining_bytes == 0 {
            return None;
        }
        self.remaining_reads -= 1;
        let limit = output.len().min(self.remaining_bytes);
        let len = self.video.read_into(position, &mut output[..limit])?;
        self.remaining_bytes -= len;
        Some(len)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn video_thumbnail_source_revokes_reads_when_owner_changes() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let alive = Arc::new(AtomicBool::new(true));
        let guard = alive.clone();
        let mut source = ThumbnailSource::new_guarded(
            VideoSource::Memory(SecureBuffer::new(vec![1, 2])),
            std::time::Instant::now() + std::time::Duration::from_secs(5),
            Arc::new(move || guard.load(Ordering::SeqCst)),
        );
        assert_eq!(source.read(0, 1), Some(vec![1]));
        alive.store(false, Ordering::SeqCst);
        assert_eq!(source.len(), None);
        assert_eq!(source.read(1, 1), None);
    }
    #[test]
    fn video_thumbnail_source_deadline_byte_and_empty_read_budgets() {
        use std::time::{Duration, Instant};
        let mut expired = ThumbnailSource::new(
            VideoSource::Memory(SecureBuffer::new(vec![1])),
            Instant::now(),
        );
        assert_eq!(expired.len(), None);
        assert_eq!(expired.read(0, 1), None);
        let mut source = ThumbnailSource::new(
            VideoSource::Memory(SecureBuffer::new(vec![7; 2 * 1024 * 1024])),
            Instant::now() + Duration::from_secs(5),
        );
        for _ in 0..32 {
            assert_eq!(source.read(0, usize::MAX).unwrap().len(), 1024 * 1024);
        }
        assert_eq!(source.read(0, 1), None);
        let mut empty = ThumbnailSource::new(
            VideoSource::Memory(SecureBuffer::new(vec![1])),
            Instant::now() + Duration::from_secs(5),
        );
        for _ in 0..4096 {
            assert_eq!(empty.read(0, 0), Some(vec![]));
        }
        assert_eq!(empty.read(0, 0), None);
    }
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
