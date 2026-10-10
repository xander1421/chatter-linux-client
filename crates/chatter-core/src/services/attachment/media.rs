//! Media processing for attachments.
//!
//! Decryption + decoding for GIFs and images. Keeps crypto/media work in service layer.

use std::sync::{Arc, OnceLock};
use std::time::Duration;
#[cfg(target_os = "android")]
static VIDEO_WORK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Platform extractor receives only a source authenticated by the attachment service.
pub type ThumbnailOwner = Arc<dyn Fn() -> bool + Send + Sync>;
pub type VideoThumbnailExtractor =
    fn(Arc<super::CbcRangeDecryptor>, ThumbnailOwner) -> Result<ImageDecodeResult, String>;
static VIDEO_THUMBNAIL_EXTRACTOR: OnceLock<VideoThumbnailExtractor> = OnceLock::new();
pub fn register_video_thumbnail_extractor(extractor: VideoThumbnailExtractor) {
    let _ = VIDEO_THUMBNAIL_EXTRACTOR.set(extractor);
}

#[cfg(any(target_os = "android", test))]
fn validate_video_thumbnail(result: &ImageDecodeResult) -> Result<(), AttachmentError> {
    if result.image_bytes.is_empty()
        || result.image_bytes.len() > 256 * 1024
        || result.width == 0
        || result.height == 0
        || result.width > 384
        || result.height > 384
    {
        return Err(AttachmentError::InvalidAttachment(
            "invalid video thumbnail bounds".into(),
        ));
    }
    let dimensions =
        ImageProcessor::get_dimensions_from_memory(&result.image_bytes).map_err(|_| {
            AttachmentError::InvalidAttachment("invalid video thumbnail encoding".into())
        })?;
    let mut reader = image::ImageReader::new(std::io::Cursor::new(&result.image_bytes))
        .with_guessed_format()
        .map_err(|_| AttachmentError::InvalidAttachment("invalid video thumbnail format".into()))?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(384);
    limits.max_image_height = Some(384);
    limits.max_alloc = Some(1024 * 1024);
    reader.limits(limits);
    reader
        .decode()
        .map_err(|_| AttachmentError::InvalidAttachment("malformed video thumbnail".into()))?;
    if dimensions != (result.width, result.height) {
        return Err(AttachmentError::InvalidAttachment(
            "video thumbnail dimensions mismatch".into(),
        ));
    }
    Ok(())
}

use crate::domain::ids::AttachmentId;
use crate::media::ImageProcessor;
use crate::media::gif::{GifFrame, decode_first_frame_only};

use super::{AttachmentError, AttachmentService};

/// Result of GIF decryption and first-frame decode.
pub struct GifDecodeResult {
    /// First frame - available immediately.
    pub first_frame: GifFrame,
    /// Decrypted GIF bytes for full decode (owned, will be moved to background thread).
    pub gif_bytes: Vec<u8>,
}

/// Result of image decryption with dimensions.
pub struct ImageDecodeResult {
    /// Decrypted image bytes.
    pub image_bytes: Vec<u8>,
    /// Image width.
    pub width: u32,
    /// Image height.
    pub height: u32,
}

/// All frames from a fully decoded GIF.
pub struct GifFullDecodeResult {
    /// All decoded frames with RGBA data and delays.
    pub frames: Vec<GifFrame>,
}

impl AttachmentService {
    /// Decrypt attachment and decode first GIF frame.
    ///
    /// Returns the first frame immediately for instant display, plus the raw
    /// bytes for background decoding of remaining frames.
    ///
    /// This keeps crypto in the service layer - UI only receives decoded pixels.
    pub fn decrypt_and_decode_gif_first_frame(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<Option<GifDecodeResult>, AttachmentError> {
        // Decrypt attachment
        let secure_buffer = self.decrypt_auto(attachment_id)?;
        let data = secure_buffer.as_slice().to_vec();

        // Decode first frame only (fast, ~10-20ms)
        match decode_first_frame_only(&data) {
            Ok(Some(first_frame)) => {
                tracing::debug!(
                    width = first_frame.width,
                    height = first_frame.height,
                    "Decoded first GIF frame"
                );
                Ok(Some(GifDecodeResult {
                    first_frame,
                    gif_bytes: data,
                }))
            }
            Ok(None) => {
                tracing::debug!("Not a GIF");
                Ok(None)
            }
            Err(e) => {
                tracing::error!(
                    error = ?e,
                    "Failed to decode GIF first frame"
                );
                Err(AttachmentError::DecryptionError(format!(
                    "GIF decode failed: {}",
                    e
                )))
            }
        }
    }

    /// Decrypt attachment and get image dimensions.
    ///
    /// Returns decrypted image bytes and dimensions for thumbnail generation.
    /// This keeps crypto in the service layer.
    pub fn decrypt_and_get_image_info(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<ImageDecodeResult, AttachmentError> {
        // Decrypt attachment
        let secure_buffer = self.decrypt_auto(attachment_id)?;
        let image_bytes = secure_buffer.as_slice().to_vec();

        // Get dimensions
        let (width, height) =
            ImageProcessor::get_dimensions_from_memory(&image_bytes).map_err(|e| {
                AttachmentError::DecryptionError(format!("Failed to get image dimensions: {:?}", e))
            })?;

        tracing::debug!(
            width = width,
            height = height,
            "Got image dimensions"
        );

        Ok(ImageDecodeResult {
            image_bytes,
            width,
            height,
        })
    }

    /// Decrypt a video attachment and extract a thumbnail frame.
    ///
    /// Server-managed DMs strip the inline thumbnail before sealing (sealed
    /// envelope is padded to ≤4096-byte buckets for metadata privacy), so the
    /// receiver derives one from the downloaded video. Returns the encoded
    /// thumbnail bytes + dims, reusing [`ImageDecodeResult`] (`image_bytes` =
    /// the thumbnail).
    ///
    /// Blocking platform decode — call from a background thread. Android uses
    /// an authenticated, bounded range source; desktop keeps GStreamer.
    pub fn decrypt_and_extract_video_thumbnail(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<ImageDecodeResult, AttachmentError> {
        self.decrypt_and_extract_video_thumbnail_guarded(attachment_id, Arc::new(|| true))
    }

    /// Revoke decoder reads immediately when the requesting account owner changes.
    pub fn decrypt_and_extract_video_thumbnail_guarded(
        &self,
        attachment_id: &AttachmentId,
        owner: ThumbnailOwner,
    ) -> Result<ImageDecodeResult, AttachmentError> {
        #[cfg(not(target_os = "android"))]
        let _ = owner;
        #[cfg(target_os = "android")]
        {
            let _work = VIDEO_WORK
                .try_lock()
                .map_err(|_| AttachmentError::InvalidAttachment("video preview busy".into()))?;
            let extractor = VIDEO_THUMBNAIL_EXTRACTOR.get().ok_or_else(|| {
                AttachmentError::InvalidAttachment("video preview extractor unavailable".into())
            })?;
            if !owner() {
                return Err(AttachmentError::InvalidAttachment(
                    "video preview owner changed".into(),
                ));
            }
            let owner_after = owner.clone();
            let result = self.extract_authenticated_video_thumbnail(attachment_id, |source| {
                if !owner() {
                    return Err("video preview owner changed".into());
                }
                let result = extractor(source, owner.clone())?;
                if !owner() {
                    return Err("video preview owner changed".into());
                }
                Ok(result)
            });
            if !owner_after() {
                return Err(AttachmentError::InvalidAttachment(
                    "video preview owner changed".into(),
                ));
            }
            return result;
        }
        #[cfg(all(not(target_os = "android"), feature = "desktop-gstreamer"))]
        let secure_buffer = self.decrypt_auto(attachment_id)?;
        #[cfg(all(not(target_os = "android"), feature = "desktop-gstreamer"))]
        {
            let (thumbnail, width, height, _duration_ms) =
                crate::media::extract_video_thumbnail_from_memory(secure_buffer.as_slice())
                    .map_err(|e| {
                        AttachmentError::DecryptionError(format!(
                            "Video thumbnail extraction failed: {}",
                            e
                        ))
                    })?;

            tracing::debug!(
                width = width,
                height = height,
                "Extracted video thumbnail on receive"
            );

            Ok(ImageDecodeResult {
                image_bytes: thumbnail,
                width,
                height,
            })
        }
        // Unsupported platforms keep the visible video placeholder.
        #[cfg(all(not(target_os = "android"), not(feature = "desktop-gstreamer")))]
        {
            Err(AttachmentError::DecryptionError(
                "video thumbnail extraction unavailable on this platform".into(),
            ))
        }
    }

    /// Shared Android path; tested with generated encrypted fixtures and an injected decoder.
    #[cfg(any(target_os = "android", test))]
    fn extract_authenticated_video_thumbnail(
        &self,
        attachment_id: &AttachmentId,
        extractor: impl FnOnce(Arc<super::CbcRangeDecryptor>) -> Result<ImageDecodeResult, String>,
    ) -> Result<ImageDecodeResult, AttachmentError> {
        let attachment = self
            .get_metadata(attachment_id)?
            .ok_or_else(|| AttachmentError::InvalidAttachment("video attachment missing".into()))?;
        if !attachment.metadata.is_video() {
            return Err(AttachmentError::InvalidAttachment(
                "attachment is not video".into(),
            ));
        }
        // Bound the authentication pass as well as decoder reads.
        if std::fs::metadata(super::attachment_file_path(attachment_id))
            .map_err(|e| AttachmentError::FileError(e.to_string()))?
            .len()
            > 128 * 1024 * 1024
        {
            return Err(AttachmentError::InvalidAttachment(
                "video preview exceeds work budget".into(),
            ));
        }
        let source = Arc::new(self.open_range_decryptor(attachment_id)?);
        let result = extractor(source).map_err(AttachmentError::DecryptionError)?;
        validate_video_thumbnail(&result)?;
        // Update only this item's thumbnail, fenced by encryption identity: don't overwrite
        // concurrent transfer/status changes or resurrect a deleted/replaced attachment.
        if !self
            .attachment_repo
            .cache_video_thumbnail(&attachment, &result.image_bytes)
            .map_err(|e| AttachmentError::StorageError(e.to_string()))?
        {
            return Err(AttachmentError::InvalidAttachment(
                "video preview source replaced".into(),
            ));
        }
        Ok(result)
    }

    /// Decode all frames from GIF bytes (blocking, for background thread).
    ///
    /// Call this from a background thread with the `gif_bytes` from `GifDecodeResult`.
    /// Returns all frames ready for animation registration.
    ///
    /// Fast path: a plain CPU GStreamer decode ([`decode_all_frames_cpu`]) —
    /// pulls every frame immediately and honors the buffer stride. Measured
    /// ~0.15s vs ~2.3s for the pure-Rust `image` crate on a 93-frame GIF, and
    /// avoids the flaky GL pipeline that produced black frames. Falls back to
    /// the `image` crate if GStreamer is unavailable or fails.
    pub fn decode_gif_all_frames(gif_bytes: &[u8]) -> Result<GifFullDecodeResult, AttachmentError> {
        #[cfg(feature = "desktop-gstreamer")]
        {
            match crate::media::gif::decode_all_frames_cpu(gif_bytes) {
                Ok(frames) if frames.len() > 1 => return Ok(GifFullDecodeResult { frames }),
                Ok(_) => {} // 0/1 frame — fall through to the image crate
                Err(e) => {
                    tracing::warn!("GStreamer GIF decode failed ({e}), using image crate");
                }
            }
        }

        Self::decode_gif_all_frames_image(gif_bytes)
    }

    /// Pure-Rust `image` crate GIF decode — fallback for `decode_gif_all_frames`.
    fn decode_gif_all_frames_image(
        gif_bytes: &[u8],
    ) -> Result<GifFullDecodeResult, AttachmentError> {
        use image::AnimationDecoder;
        use image::codecs::gif::GifDecoder;
        use std::io::Cursor;

        let cursor = Cursor::new(gif_bytes);
        let decoder = GifDecoder::new(cursor)
            .map_err(|e| AttachmentError::DecryptionError(format!("GIF decoder error: {}", e)))?;

        let mut frames = Vec::new();
        for frame_result in decoder.into_frames() {
            let frame = match frame_result {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("GIF frame decode error: {}", e);
                    continue;
                }
            };

            let (numer, denom) = frame.delay().numer_denom_ms();
            let delay_ms = if denom == 0 {
                100
            } else {
                ((numer as u64) / (denom as u64)).max(20)
            };

            let buffer = frame.into_buffer();
            let (width, height) = buffer.dimensions();
            frames.push(GifFrame {
                data: buffer.into_raw(),
                width,
                height,
                delay: Duration::from_millis(delay_ms),
            });
        }

        Ok(GifFullDecodeResult { frames })
    }
}

#[cfg(test)]
mod video_thumbnail_tests {
    use super::*;
    fn thumbnail(width: u32, height: u32) -> ImageDecodeResult {
        let mut cursor = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(width, height)
            .write_to(&mut cursor, image::ImageFormat::Png)
            .unwrap();
        ImageDecodeResult {
            image_bytes: cursor.into_inner(),
            width,
            height,
        }
    }
    #[test]
    fn video_thumbnail_rejects_malformed_oversized_and_dimension_mismatch() {
        assert!(validate_video_thumbnail(&thumbnail(384, 216)).is_ok());
        assert!(validate_video_thumbnail(&thumbnail(385, 216)).is_err());
        let mut result = thumbnail(32, 18);
        result.width = 33;
        assert!(validate_video_thumbnail(&result).is_err());
        result.width = 32;
        result.image_bytes = vec![7; 256 * 1024 + 1];
        assert!(validate_video_thumbnail(&result).is_err());
        result.image_bytes = vec![0, 1, 2];
        assert!(validate_video_thumbnail(&result).is_err());
        result.image_bytes.clear();
        assert!(validate_video_thumbnail(&result).is_err());
    }
    #[test]
    fn video_thumbnail_authenticates_owned_ciphertext_before_decoder() {
        use crate::crypto::attachment_crypto::AttachmentCrypto;
        use crate::domain::attachment::{
            Attachment, AttachmentStatus, ContentLocation, FileMetadata,
        };
        use crate::storage::repositories::AttachmentRepository;
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            crate::storage::Database::open(&dir.path().join("video-auth.db"), "owned-video-test")
                .unwrap(),
        );
        db.run_migrations().unwrap();
        let repo = Arc::new(AttachmentRepository::new(db));
        let service = AttachmentService::new_local(repo.clone());
        let plaintext = b"generated-owned-video-range-fixture";
        let encrypted = AttachmentCrypto::encrypt(plaintext).unwrap();
        let source = Attachment {
            id: AttachmentId::new(),
            metadata: FileMetadata::new(
                "fixture.mp4".into(),
                "video/mp4".into(),
                plaintext.len() as u64,
            ),
            status: AttachmentStatus::Ready,
            content_location: ContentLocation::OnFilesystem,
            remote_url: Some("owned-encrypted-fixture".into()),
            encryption_key: Some(encrypted.key.to_vec()),
            encryption_iv: Some(encrypted.digest.to_vec()),
            created_at: chrono::Utc::now(),
            secret_chat: Some(false),
        };
        repo.insert(&source).unwrap();
        super::super::ensure_attachments_dir().unwrap();
        let path = super::super::attachment_file_path(&source.id);
        std::fs::write(&path, &encrypted.ciphertext).unwrap();
        let result = service
            .extract_authenticated_video_thumbnail(&source.id, |ranges| {
                assert_eq!(ranges.read_range(0, plaintext.len()), plaintext);
                Ok(thumbnail(32, 18))
            })
            .unwrap();
        assert_eq!(
            repo.get(&source.id)
                .unwrap()
                .unwrap()
                .metadata
                .thumbnail
                .unwrap(),
            result.image_bytes
        );
        let mut tampered = encrypted.ciphertext.clone();
        tampered[16] ^= 1;
        std::fs::write(&path, tampered).unwrap();
        assert!(
            service
                .extract_authenticated_video_thumbnail(&source.id, |_| panic!(
                    "decoder received unauthenticated media"
                ))
                .is_err()
        );
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn video_thumbnail_cache_preserves_transfer_state_and_rejects_replaced_item() {
        use crate::domain::attachment::{
            Attachment, AttachmentStatus, ContentLocation, FileMetadata,
        };
        use crate::storage::repositories::AttachmentRepository;
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            crate::storage::Database::open(&dir.path().join("video.db"), "owned-video-test")
                .unwrap(),
        );
        db.run_migrations().unwrap();
        let repo = AttachmentRepository::new(db);
        let mut source = Attachment {
            id: AttachmentId::new(),
            metadata: FileMetadata::new("fixture.mp4".into(), "video/mp4".into(), 128),
            status: AttachmentStatus::Ready,
            content_location: ContentLocation::OnFilesystem,
            remote_url: Some("owned-encrypted-fixture".into()),
            encryption_key: Some(vec![1; 64]),
            encryption_iv: Some(vec![2; 32]),
            created_at: chrono::Utc::now(),
            secret_chat: Some(false),
        };
        repo.insert(&source).unwrap();
        let mut concurrent = source.clone();
        concurrent.status = AttachmentStatus::Downloading;
        repo.update(&concurrent).unwrap();
        let bytes = thumbnail(32, 18).image_bytes;
        assert!(repo.cache_video_thumbnail(&source, &bytes).unwrap());
        let saved = repo.get(&source.id).unwrap().unwrap();
        assert_eq!(saved.status, AttachmentStatus::Downloading);
        assert_eq!(saved.metadata.thumbnail.as_deref(), Some(bytes.as_slice()));
        source.encryption_iv = Some(vec![3; 32]);
        assert!(!repo.cache_video_thumbnail(&source, &[1, 2]).unwrap());
        assert_eq!(
            repo.get(&source.id)
                .unwrap()
                .unwrap()
                .metadata
                .thumbnail
                .unwrap(),
            bytes
        );
    }
}
