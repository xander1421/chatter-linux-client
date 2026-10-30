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

/// Include Signal bucket padding as well as IV/MAC overhead in the work cap.
/// A fixed overhead above plaintext size rejects valid near-limit originals.
pub(crate) fn image_preview_ciphertext_limit() -> u64 {
    crate::crypto::attachment_crypto::AttachmentCrypto::padded_size(
        crate::domain::attachment::Attachment::AUTO_DOWNLOAD_IMAGE_LIMIT as usize,
    ) as u64
        + 16
        + 32
}

/// Small inline model pixels; original encoded bytes never cross into UI.
pub struct ImagePreview {
    pub pixels: image::RgbaImage,
    pub width: u32,
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

        // Older senders and failed sidecar uploads leave no durable preview.
        // Once the original is downloaded, derive one locally so reopening the
        // conversation does not bring the placeholder back.
        if let Ok(Some(mut attachment)) = self.attachment_repo.get(attachment_id) {
            if attachment
                .metadata
                .thumbnail
                .as_ref()
                .is_none_or(Vec::is_empty)
            {
                if let Ok((thumbnail, thumb_width, thumb_height)) =
                    ImageProcessor::generate_thumbnail_from_memory(&image_bytes)
                {
                    attachment.metadata.thumbnail = Some(thumbnail);
                    attachment.metadata.thumbnail_width = Some(thumb_width);
                    attachment.metadata.thumbnail_height = Some(thumb_height);
                    if let Err(e) = self.attachment_repo.update(&attachment) {
                        tracing::debug!(error = %e, "Failed to cache downloaded image preview");
                    }
                }
            }
        }

        tracing::debug!(width = width, height = height, "Got image dimensions");

        Ok(ImageDecodeResult {
            image_bytes,
            width,
            height,
        })
    }

    /// Decode an authenticated stored sidecar within the inline pixel budget.
    pub fn decode_image_sidecar(bytes: &[u8]) -> Result<image::RgbaImage, AttachmentError> {
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(AttachmentError::InvalidAttachment(
                "thumbnail exceeds preview budget".into(),
            ));
        }
        super::validate_media(bytes)?;
        ImageProcessor::inline_pixels_from_memory(bytes)
            .map_err(|_| AttachmentError::InvalidAttachment("invalid thumbnail image".into()))
    }

    /// Derive missing previews without returning/retaining full originals in UI.
    /// Caller runs this on the shared attachment worker and fences session use.
    pub fn prepare_image_preview_guarded(
        &self,
        id: &AttachmentId,
        owner: ThumbnailOwner,
    ) -> Result<ImagePreview, AttachmentError> {
        self.prepare_image_preview_with(id, owner, ImageProcessor::bounded_inline_thumbnail)
    }

    fn prepare_image_preview_with(
        &self,
        id: &AttachmentId,
        owner: ThumbnailOwner,
        decode: impl FnOnce(&[u8]) -> Result<(image::DynamicImage, u32, u32), crate::error::MediaError>,
    ) -> Result<ImagePreview, AttachmentError> {
        let stale = || AttachmentError::InvalidAttachment("image preview owner changed".into());
        if !owner() {
            return Err(stale());
        }
        let attachment = self
            .get_metadata(id)?
            .ok_or_else(|| AttachmentError::InvalidAttachment("image attachment missing".into()))?;
        if let Some(bytes) = attachment.metadata.thumbnail.as_deref() {
            if let Ok(pixels) = Self::decode_image_sidecar(bytes) {
                if !owner() {
                    return Err(stale());
                }
                return Ok(ImagePreview {
                    width: attachment
                        .metadata
                        .width
                        .filter(|n| *n > 0)
                        .unwrap_or(pixels.width()),
                    height: attachment
                        .metadata
                        .height
                        .filter(|n| *n > 0)
                        .unwrap_or(pixels.height()),
                    pixels,
                });
            }
        }
        if !attachment.metadata.is_image()
            || attachment.metadata.is_gif()
            || attachment.metadata.size
                > crate::domain::attachment::Attachment::AUTO_DOWNLOAD_IMAGE_LIMIT
            || std::fs::metadata(super::attachment_file_path(id))
                .map_err(|_| AttachmentError::FileError("image unavailable".into()))?
                .len()
                > image_preview_ciphertext_limit()
        {
            return Err(AttachmentError::InvalidAttachment(
                "image exceeds preview budget".into(),
            ));
        }
        // SecureBuffer owns and zeroizes the ONE decrypted original. Authenticate
        // it before the decoder; no whole-file clone or original bytes in result.
        let plaintext = self.decrypt_auto(id)?;
        super::validate_media(plaintext.as_slice())?;
        if !owner() {
            return Err(stale());
        }
        let (thumbnail, width, height) = decode(plaintext.as_slice())
            .map_err(|_| AttachmentError::InvalidAttachment("invalid image preview".into()))?;
        if !owner() {
            return Err(stale());
        }
        let mut encoded = std::io::Cursor::new(Vec::new());
        thumbnail
            .write_to(&mut encoded, image::ImageFormat::WebP)
            .map_err(|_| {
                AttachmentError::InvalidAttachment("image preview encoding failed".into())
            })?;
        let pixels = thumbnail.thumbnail(320, 320).into_rgba8();
        if !owner() {
            return Err(stale());
        }
        if !self
            .attachment_repo
            .cache_image_preview(
                &attachment,
                encoded.get_ref(),
                (width, height),
                (thumbnail.width(), thumbnail.height()),
            )
            .map_err(|e| AttachmentError::StorageError(e.to_string()))?
        {
            return Err(AttachmentError::InvalidAttachment(
                "image preview source replaced".into(),
            ));
        }
        if !owner() {
            return Err(stale());
        }
        Ok(ImagePreview {
            pixels,
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
            .cache_authenticated_thumbnail(&attachment, &result.image_bytes)
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
            secret_chat: false,
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
            secret_chat: false,
        };
        repo.insert(&source).unwrap();
        let mut concurrent = source.clone();
        concurrent.status = AttachmentStatus::Downloading;
        repo.update(&concurrent).unwrap();
        let bytes = thumbnail(32, 18).image_bytes;
        assert!(repo.cache_authenticated_thumbnail(&source, &bytes).unwrap());
        let saved = repo.get(&source.id).unwrap().unwrap();
        assert_eq!(saved.status, AttachmentStatus::Downloading);
        assert_eq!(saved.metadata.thumbnail.as_deref(), Some(bytes.as_slice()));
        source.encryption_iv = Some(vec![3; 32]);
        assert!(!repo.cache_authenticated_thumbnail(&source, &[1, 2]).unwrap());
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

#[cfg(test)]
mod image_preview_tests {
    use super::*;
    use crate::crypto::attachment_crypto::AttachmentCrypto;
    use crate::domain::attachment::{Attachment, AttachmentStatus, ContentLocation, FileMetadata};
    use crate::storage::repositories::AttachmentRepository;

    struct Fixture {
        _dir: tempfile::TempDir,
        service: AttachmentService,
        repo: Arc<AttachmentRepository>,
        source: Attachment,
        path: std::path::PathBuf,
        ciphertext: Vec<u8>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            crate::storage::Database::open(&dir.path().join("image.db"), "image-test").unwrap(),
        );
        db.run_migrations().unwrap();
        let repo = Arc::new(AttachmentRepository::new(db));
        let service = AttachmentService::new_local(repo.clone());
        let mut plaintext = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2048, 1024)
            .write_to(&mut plaintext, image::ImageFormat::Jpeg)
            .unwrap();
        let encrypted = AttachmentCrypto::encrypt(plaintext.get_ref()).unwrap();
        let source = Attachment {
            id: AttachmentId::new(),
            metadata: FileMetadata::new(
                "photo.jpg".into(),
                "image/jpeg".into(),
                plaintext.get_ref().len() as u64,
            ),
            status: AttachmentStatus::Ready,
            content_location: ContentLocation::OnFilesystem,
            remote_url: Some("generated-owned-image".into()),
            encryption_key: Some(encrypted.key.to_vec()),
            encryption_iv: Some(encrypted.digest.to_vec()),
            created_at: chrono::Utc::now(),
            secret_chat: false,
        };
        repo.insert(&source).unwrap();
        super::super::ensure_attachments_dir().unwrap();
        let path = super::super::attachment_file_path(&source.id);
        std::fs::write(&path, &encrypted.ciphertext).unwrap();
        Fixture {
            _dir: dir,
            service,
            repo,
            source,
            path,
            ciphertext: encrypted.ciphertext.clone(),
        }
    }

    #[test]
    fn missing_or_failed_sidecar_retains_only_small_pixels_and_reuses_durable_preview() {
        let f = fixture();
        let result = f
            .service
            .prepare_image_preview_guarded(&f.source.id, Arc::new(|| true))
            .unwrap();
        assert_eq!((result.width, result.height), (2048, 1024));
        assert_eq!(result.pixels.dimensions(), (320, 160));
        assert_eq!(
            std::fs::read(&f.path).unwrap(),
            f.ciphertext,
            "original for viewer changed"
        );
        let cached = f
            .repo
            .get(&f.source.id)
            .unwrap()
            .unwrap()
            .metadata
            .thumbnail
            .unwrap();
        assert_eq!(
            ImageProcessor::get_dimensions_from_memory(&cached).unwrap(),
            (768, 384)
        );
        std::fs::remove_file(&f.path).unwrap();
        let reused = f
            .service
            .prepare_image_preview_with(&f.source.id, Arc::new(|| true), |_| {
                panic!("cached sidecar must skip original decode")
            })
            .unwrap();
        assert_eq!(reused.pixels.dimensions(), (320, 160));
        assert_eq!((reused.width, reused.height), (2048, 1024));
        // A broken fetched sidecar must fall back to the authenticated original.
        let mut broken = f.repo.get(&f.source.id).unwrap().unwrap();
        broken.metadata.thumbnail = Some(b"bad sidecar".to_vec());
        f.repo.update(&broken).unwrap();
        std::fs::write(&f.path, &f.ciphertext).unwrap();
        assert!(
            f.service
                .prepare_image_preview_guarded(&f.source.id, Arc::new(|| true))
                .is_ok()
        );
    }

    #[test]
    fn image_authentication_and_owner_checks_precede_decode_and_persistence() {
        let f = fixture();
        let mut tampered = f.ciphertext.clone();
        *tampered.last_mut().unwrap() ^= 1;
        std::fs::write(&f.path, tampered).unwrap();
        assert!(
            f.service
                .prepare_image_preview_with(&f.source.id, Arc::new(|| true), |_| panic!(
                    "unauthenticated image reached decoder"
                ))
                .is_err()
        );
        assert!(
            f.repo
                .get(&f.source.id)
                .unwrap()
                .unwrap()
                .metadata
                .thumbnail
                .is_none()
        );
        std::fs::write(&f.path, &f.ciphertext).unwrap();
        let valid = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let owner = valid.clone();
        assert!(
            f.service
                .prepare_image_preview_with(
                    &f.source.id,
                    Arc::new(move || owner.load(std::sync::atomic::Ordering::SeqCst)),
                    |_| {
                        valid.store(false, std::sync::atomic::Ordering::SeqCst);
                        Ok((image::DynamicImage::new_rgb8(32, 16), 2048, 1024))
                    }
                )
                .is_err()
        );
        assert!(
            f.repo
                .get(&f.source.id)
                .unwrap()
                .unwrap()
                .metadata
                .thumbnail
                .is_none(),
            "revoked preview persisted"
        );
    }

    #[test]
    fn near_plaintext_limit_accepts_signal_bucket_padding() {
        let f = fixture();
        let mut plaintext = f
            .service
            .decrypt_auto(&f.source.id)
            .unwrap()
            .as_slice()
            .to_vec();
        plaintext.resize(
            crate::domain::attachment::Attachment::AUTO_DOWNLOAD_IMAGE_LIMIT as usize,
            0,
        );
        let encrypted = AttachmentCrypto::encrypt(&plaintext).unwrap();
        assert!(
            encrypted.ciphertext.len() as u64
                > crate::domain::attachment::Attachment::AUTO_DOWNLOAD_IMAGE_LIMIT + 128
        );
        assert_eq!(
            encrypted.ciphertext.len() as u64,
            image_preview_ciphertext_limit()
        );
        let mut source = f.source.clone();
        source.metadata.size = plaintext.len() as u64;
        source.encryption_key = Some(encrypted.key.to_vec());
        source.encryption_iv = Some(encrypted.digest.to_vec());
        f.repo.update(&source).unwrap();
        std::fs::write(&f.path, &encrypted.ciphertext).unwrap();
        assert!(
            f.service
                .prepare_image_preview_with(&source.id, Arc::new(|| true), |_| {
                    Ok((image::DynamicImage::new_rgb8(32, 16), 2048, 1024))
                })
                .is_ok()
        );
    }

    #[test]
    fn preview_persistence_preserves_status_and_rejects_replaced_encryption_identity() {
        for replace in [false, true] {
            let f = fixture();
            let result =
                f.service
                    .prepare_image_preview_with(&f.source.id, Arc::new(|| true), |_| {
                        let mut current = f.repo.get(&f.source.id).unwrap().unwrap();
                        current.status = AttachmentStatus::Downloading;
                        if replace {
                            current.encryption_iv = Some(vec![8; 32]);
                        }
                        f.repo.update(&current).unwrap();
                        Ok((image::DynamicImage::new_rgb8(32, 16), 2048, 1024))
                    });
            assert_eq!(result.is_err(), replace);
            let current = f.repo.get(&f.source.id).unwrap().unwrap();
            assert_eq!(current.status, AttachmentStatus::Downloading);
            assert_eq!(current.metadata.thumbnail.is_none(), replace);
        }
    }

    #[test]
    fn invalid_or_oversized_sidecars_and_originals_leave_no_preview() {
        assert!(AttachmentService::decode_image_sidecar(b"bad sidecar").is_err());
        let mut oversized_dimensions = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(16385, 1)
            .write_to(&mut oversized_dimensions, image::ImageFormat::Png)
            .unwrap();
        assert!(AttachmentService::decode_image_sidecar(oversized_dimensions.get_ref()).is_err());
        assert!(AttachmentService::decode_image_sidecar(&vec![0; 4 * 1024 * 1024 + 1]).is_err());
        let f = fixture();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&f.path)
            .unwrap()
            .set_len(image_preview_ciphertext_limit() + 1)
            .unwrap();
        assert!(
            f.service
                .prepare_image_preview_with(&f.source.id, Arc::new(|| true), |_| panic!(
                    "oversized source reached decoder"
                ))
                .is_err()
        );
        assert!(
            f.repo
                .get(&f.source.id)
                .unwrap()
                .unwrap()
                .metadata
                .thumbnail
                .is_none()
        );
    }
}
