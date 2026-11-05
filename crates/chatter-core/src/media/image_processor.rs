//! Image processor.
//!
//! Image manipulation for thumbnail generation and format conversion.

use crate::error::MediaError;
use image::imageops::FilterType;
use image::{DynamicImage, ImageEncoder, ImageFormat, ImageReader};
use std::io::Cursor;
use std::path::Path;

/// Default maximum dimension for thumbnails (width or height).
pub const DEFAULT_THUMBNAIL_SIZE: u32 = 256;

/// Long-edge cap for the inline preview thumbnail shown in the chat bubble.
///
/// The bubble renders at ~280x380 logical px, which is ~560x760 physical on a
/// 2x HiDPI display — so a 768px thumbnail is right-sized for the largest box
/// and holds enough resolution to read small text in screenshots. This is what
/// both sender and receiver see inline (the full image is only fetched on tap).
///
/// WebP-lossless keeps screenshot text crisp but is heavy for photos;
/// if the inline payload gets too big, switch these paths to lossy JPEG q90.
const INLINE_THUMBNAIL_MAX_SIZE: u32 = 768;

/// JPEG quality for thumbnail compression (0-100).
const JPEG_QUALITY: u8 = 85;

/// Image processor for thumbnails and metadata.
///
/// Provides stateless image operations for thumbnail generation and format conversion.
pub struct ImageProcessor;

/// Encoded originals and decoded canvases have independent budgets. Twelve
/// megapixel camera photos fit; pathological aspect ratios cannot evade this.
pub const PREVIEW_ENCODED_BYTES: u64 = 32 * 1024 * 1024;
pub const PREVIEW_PIXELS: u64 = 16 * 1024 * 1024;
pub const PREVIEW_DECODED_BYTES: u64 = 64 * 1024 * 1024;
pub const PREVIEW_ALLOC_BYTES: u64 = 96 * 1024 * 1024;

impl ImageProcessor {
    /// Generate a thumbnail from an image file.
    ///
    /// Creates a thumbnail that fits within `max_size x max_size` pixels while
    /// preserving the original aspect ratio.
    ///
    /// # Arguments
    ///
    /// * `source` - Path to the source image
    /// * `max_size` - Maximum width or height for the thumbnail
    ///
    /// # Errors
    ///
    /// Returns `MediaError::InvalidFormat` if the image cannot be opened or decoded.
    pub fn generate_thumbnail(source: &Path, max_size: u32) -> Result<DynamicImage, MediaError> {
        let image = Self::load_image(source)?;

        // Use triangle filter for speed with acceptable quality
        let thumbnail = image.thumbnail(max_size, max_size);

        Ok(thumbnail)
    }

    /// Save an image to disk with the specified format.
    ///
    /// For JPEG format, uses quality 85. For other formats, uses default settings.
    ///
    /// # Arguments
    ///
    /// * `image` - The image to save
    /// * `path` - Destination path
    /// * `format` - Output format (JPEG recommended for thumbnails)
    ///
    /// # Errors
    ///
    /// Returns `MediaError::ThumbnailGeneration` if the image cannot be saved.
    pub fn save_thumbnail(
        image: &DynamicImage,
        path: &Path,
        format: ImageFormat,
    ) -> Result<(), MediaError> {
        // Create parent directories if they don't exist
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(MediaError::FileRead)?;
        }

        match format {
            ImageFormat::Jpeg => {
                // Use explicit JPEG encoder for quality control
                let rgb = image.to_rgb8();
                let mut output = std::fs::File::create(path).map_err(MediaError::FileRead)?;
                let mut encoder =
                    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, JPEG_QUALITY);
                encoder
                    .encode_image(&rgb)
                    .map_err(|e| MediaError::ThumbnailGeneration(e.to_string()))?;
            }
            _ => {
                image
                    .save_with_format(path, format)
                    .map_err(|e| MediaError::ThumbnailGeneration(e.to_string()))?;
            }
        }

        Ok(())
    }

    /// Get image dimensions without loading the full image.
    ///
    /// Uses the image crate's efficient dimension reading that only parses headers.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the image file
    ///
    /// # Returns
    ///
    /// Tuple of (width, height) in pixels.
    ///
    /// # Errors
    ///
    /// Returns `MediaError::InvalidFormat` if the image cannot be read.
    pub fn get_dimensions(path: &Path) -> Result<(u32, u32), MediaError> {
        let reader = ImageReader::open(path)
            .map_err(|e| MediaError::InvalidFormat(format!("cannot open image: {}", e)))?
            .with_guessed_format()
            .map_err(|e| MediaError::InvalidFormat(format!("cannot determine format: {}", e)))?;

        reader
            .into_dimensions()
            .map_err(|e| MediaError::InvalidFormat(format!("cannot read dimensions: {}", e)))
    }

    /// Load an image from disk.
    ///
    /// Uses guessed format for robust loading of various image types.
    /// Limits memory allocation to 512MB to prevent OOM on very large images.
    fn load_image(path: &Path) -> Result<DynamicImage, MediaError> {
        use image::Limits;

        let mut reader = ImageReader::open(path)
            .map_err(|e| MediaError::InvalidFormat(format!("cannot open image: {}", e)))?
            .with_guessed_format()
            .map_err(|e| MediaError::InvalidFormat(format!("cannot determine format: {}", e)))?;

        // Set limits: max 512MB allocation, max 16384x16384 dimensions
        let mut limits = Limits::default();
        limits.max_alloc = Some(512 * 1024 * 1024); // 512MB
        limits.max_image_width = Some(16384);
        limits.max_image_height = Some(16384);
        reader.limits(limits);

        reader
            .decode()
            .map_err(|e| MediaError::InvalidFormat(format!("cannot decode image: {}", e)))
    }

    /// Check if a file appears to be an image based on extension.
    ///
    /// Does not validate the actual content.
    #[must_use]
    pub fn is_supported_image(path: &Path) -> bool {
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| {
                matches!(
                    ext.to_lowercase().as_str(),
                    "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tiff" | "tif"
                )
            })
            .unwrap_or(false)
    }

    /// Load an image from memory bytes.
    ///
    /// Used for secure in-memory image display without touching filesystem.
    /// Limits memory allocation to 512MB to prevent OOM on very large images.
    ///
    /// # Arguments
    ///
    /// * `data` - Image data bytes (PNG, JPEG, WebP, etc.)
    ///
    /// # Errors
    ///
    /// Returns `MediaError::InvalidFormat` if the image cannot be decoded.
    pub fn load_from_memory(data: &[u8]) -> Result<DynamicImage, MediaError> {
        use image::Limits;

        let cursor = Cursor::new(data);
        let mut reader = ImageReader::new(cursor)
            .with_guessed_format()
            .map_err(|e| MediaError::InvalidFormat(format!("cannot determine format: {}", e)))?;

        // Set limits: max 512MB allocation, max 16384x16384 dimensions
        let mut limits = Limits::default();
        limits.max_alloc = Some(512 * 1024 * 1024); // 512MB
        limits.max_image_width = Some(16384);
        limits.max_image_height = Some(16384);
        reader.limits(limits);

        reader.decode().map_err(|e| {
            MediaError::InvalidFormat(format!("cannot decode image from memory: {}", e))
        })
    }

    /// Get image dimensions from memory bytes.
    ///
    /// Efficiently reads only headers to determine dimensions.
    pub fn get_dimensions_from_memory(data: &[u8]) -> Result<(u32, u32), MediaError> {
        let cursor = Cursor::new(data);
        let reader = ImageReader::new(cursor)
            .with_guessed_format()
            .map_err(|e| MediaError::InvalidFormat(format!("cannot determine format: {}", e)))?;

        reader
            .into_dimensions()
            .map_err(|e| MediaError::InvalidFormat(format!("cannot read dimensions: {}", e)))
    }

    /// Process raw image bytes into a normalised avatar suitable for storage
    /// and inline embedding in encrypted bootstrap / profile-update messages.
    ///
    /// Pipeline:
    ///   1. Decode any supported format (PNG / JPEG / GIF / WebP).
    ///   2. Center-crop to a square (avatars render in a circular mask, so
    ///      cropping to square first prevents asymmetric cropping at display).
    ///   3. Resize to [`AVATAR_SIZE_PX`] × [`AVATAR_SIZE_PX`] with Lanczos3
    ///      for high-quality downscaling.
    ///   4. Re-encode as **WebP lossless** (the only WebP encoder shipped
    ///      with `image` 0.25 — adding lossy WebP would require an extra
    ///      C-FFI dep).
    ///   5. Verify the output is at most
    ///      [`crate::domain::anon_dm_init::MAX_AVATAR_SIZE`] (1 MB),
    ///      matching the wire-format cap baked into the `AnonDmInitMessage`
    ///      v4 schema. A 512×512 photo at WebP-lossless typically lands at
    ///      80-200 KB, comfortably under the cap.
    ///
    /// Used by both clients (Linux desktop + Android via the bridge) so the
    /// processed-avatar bytes are byte-identical regardless of platform.
    /// This is the source of truth for avatar normalisation; the UI layer
    /// must not duplicate decode/resize/encode logic.
    ///
    /// # Errors
    ///
    /// Returns `MediaError::InvalidFormat` if the input cannot be decoded,
    /// or `MediaError::ThumbnailGeneration` if encoding fails or the output
    /// exceeds the `MAX_AVATAR_SIZE` cap.
    pub fn process_avatar_from_memory(data: &[u8]) -> Result<Vec<u8>, MediaError> {
        /// Final encoded avatar dimensions (square, both axes).
        ///
        /// 512 px is large enough for retina hero surfaces (Compose's
        /// `Modifier.size(160.dp)` on a Galaxy S25 is ~360 px, so 512 leaves
        /// headroom for a small zoom) and small enough to compress under
        /// 200 KB at WebP-lossless.
        const AVATAR_SIZE_PX: u32 = 512;

        let image = Self::load_from_memory(data)?;

        // Center-crop to square so the circular display mask doesn't
        // asymmetrically crop the image content.
        let w = image.width();
        let h = image.height();
        let side = w.min(h);
        let x_off = (w - side) / 2;
        let y_off = (h - side) / 2;
        let cropped = image.crop_imm(x_off, y_off, side, side);

        // Resize to the target dimensions (Lanczos3 for high-quality downscale).
        let resized = if cropped.width() != AVATAR_SIZE_PX {
            cropped.resize_exact(AVATAR_SIZE_PX, AVATAR_SIZE_PX, FilterType::Lanczos3)
        } else {
            cropped
        };

        // Encode as WebP lossless (the only WebP encoder in image 0.25).
        // Lossless WebP at 512×512 is typically 80-200 KB for a photo,
        // well under the 1 MB MAX_AVATAR_SIZE cap.
        let rgba = resized.to_rgba8();
        let mut bytes: Vec<u8> = Vec::with_capacity(64 * 1024);
        let encoder = image::codecs::webp::WebPEncoder::new_lossless(&mut bytes);
        encoder
            .write_image(
                rgba.as_raw(),
                AVATAR_SIZE_PX,
                AVATAR_SIZE_PX,
                image::ExtendedColorType::Rgba8,
            )
            .map_err(|e| MediaError::ThumbnailGeneration(e.to_string()))?;

        // Defense in depth: enforce the wire-format cap. In practice this
        // never trips at 512×512 — but a future format change could push
        // a pathological input over and we want to fail loudly here rather
        // than silently in the init-message serializer.
        let cap = crate::domain::anon_dm_init::MAX_AVATAR_SIZE;
        if bytes.len() > cap {
            return Err(MediaError::ThumbnailGeneration(format!(
                "processed avatar is {} bytes, exceeds MAX_AVATAR_SIZE {}",
                bytes.len(),
                cap
            )));
        }

        tracing::info!(
            input_bytes = data.len(),
            output_bytes = bytes.len(),
            output_size = AVATAR_SIZE_PX,
            "Processed avatar (WebP lossless)"
        );

        Ok(bytes)
    }

    /// Small composer pixels, with a separate decoder budget from full-size
    /// media. Invalid/oversized images keep their attachment and generic icon.
    pub fn tray_preview_from_memory(data: &[u8]) -> Result<image::RgbaImage, MediaError> {
        Ok(Self::fit_preview(Self::load_preview_image(data)?, 160))
    }

    /// Decode stored sidecars to the inline model's pixel budget.
    pub fn inline_pixels_from_memory(data: &[u8]) -> Result<image::RgbaImage, MediaError> {
        Ok(Self::fit_preview(Self::load_preview_image(data)?, 320))
    }

    // These are upper bounds, not target sizes. Enlarging a small sidecar adds
    // no detail and multiplies CPU copies and texture uploads. The renderer
    // samples the original pixels at the tile's display size.
    fn fit_preview(image: DynamicImage, edge: u32) -> image::RgbaImage {
        if image.width() > edge || image.height() > edge {
            image.thumbnail(edge, edge).into_rgba8()
        } else {
            image.into_rgba8()
        }
    }

    /// Decode an original once for a durable preview; return its true aspect
    /// dimensions separately from the bounded thumbnail pixels.
    pub fn bounded_inline_thumbnail(data: &[u8]) -> Result<(DynamicImage, u32, u32), MediaError> {
        let image = Self::load_preview_image(data)?;
        let (width, height) = (image.width(), image.height());
        Ok((
            image.resize(
                INLINE_THUMBNAIL_MAX_SIZE,
                INLINE_THUMBNAIL_MAX_SIZE,
                FilterType::Lanczos3,
            ),
            width,
            height,
        ))
    }

    /// Inline original fallback streams encoded bytes instead of copying a file.
    pub fn inline_pixels_from_path(path: &Path) -> Result<image::RgbaImage, MediaError> {
        let file = std::fs::File::open(path).map_err(MediaError::FileRead)?;
        if file.metadata().map_err(MediaError::FileRead)?.len() > PREVIEW_ENCODED_BYTES {
            return Err(MediaError::InvalidFormat(
                "preview byte budget exceeded".into(),
            ));
        }
        Self::decode_preview(std::io::BufReader::new(file))
            .map(|image| Self::fit_preview(image, 320))
    }

    fn load_preview_image(data: &[u8]) -> Result<DynamicImage, MediaError> {
        if data.len() as u64 > PREVIEW_ENCODED_BYTES {
            return Err(MediaError::InvalidFormat(
                "preview byte budget exceeded".into(),
            ));
        }
        Self::decode_preview(Cursor::new(data))
    }

    fn decode_preview<R: std::io::BufRead + std::io::Seek>(
        input: R,
    ) -> Result<DynamicImage, MediaError> {
        let mut reader = ImageReader::new(input)
            .with_guessed_format()
            .map_err(|e| MediaError::InvalidFormat(e.to_string()))?;
        let mut initial_limits = image::Limits::default();
        initial_limits.max_alloc = Some(PREVIEW_ALLOC_BYTES);
        initial_limits.max_image_width = Some(16384);
        initial_limits.max_image_height = Some(16384);
        reader.limits(initial_limits);
        let mut decoder = reader
            .into_decoder()
            .map_err(|e| MediaError::InvalidFormat(e.to_string()))?;
        use image::ImageDecoder;
        let (width, height) = decoder.dimensions();
        if u64::from(width) * u64::from(height) > PREVIEW_PIXELS
            || decoder.total_bytes() > PREVIEW_DECODED_BYTES
        {
            return Err(MediaError::InvalidFormat(
                "preview pixel budget exceeded".into(),
            ));
        }
        let mut limits = image::Limits::default();
        limits.max_alloc = Some(PREVIEW_ALLOC_BYTES);
        limits.max_image_width = Some(16384);
        limits.max_image_height = Some(16384);
        decoder
            .set_limits(limits)
            .map_err(|e| MediaError::InvalidFormat(e.to_string()))?;
        DynamicImage::from_decoder(decoder).map_err(|e| MediaError::InvalidFormat(e.to_string()))
    }

    /// Generate thumbnail bytes from in-memory image data.
    ///
    /// Returns WebP-encoded thumbnail bytes with dimensions.
    pub fn generate_thumbnail_from_memory(data: &[u8]) -> Result<(Vec<u8>, u32, u32), MediaError> {
        let image = Self::load_from_memory(data)?;
        // Lanczos3 (not the fast `.thumbnail()` box filter) keeps text legible.
        let thumbnail = image.resize(
            INLINE_THUMBNAIL_MAX_SIZE,
            INLINE_THUMBNAIL_MAX_SIZE,
            FilterType::Lanczos3,
        );

        let width = thumbnail.width();
        let height = thumbnail.height();

        // Encode to WebP lossless
        let rgba = thumbnail.to_rgba8();
        let mut bytes: Vec<u8> = Vec::new();
        let encoder = image::codecs::webp::WebPEncoder::new_lossless(&mut bytes);
        encoder
            .write_image(
                rgba.as_raw(),
                width,
                height,
                image::ExtendedColorType::Rgba8,
            )
            .map_err(|e| MediaError::ThumbnailGeneration(e.to_string()))?;

        Ok((bytes, width, height))
    }

    /// Resize an image to fit within maximum dimensions.
    ///
    /// Uses Lanczos3 filter for high-quality downscaling.
    ///
    /// # Arguments
    ///
    /// * `image` - Source image
    /// * `max_width` - Maximum width
    /// * `max_height` - Maximum height
    ///
    /// # Returns
    ///
    /// Resized image that fits within the specified dimensions.
    #[must_use]
    pub fn resize_to_fit(image: &DynamicImage, max_width: u32, max_height: u32) -> DynamicImage {
        image.resize(max_width, max_height, FilterType::Lanczos3)
    }

    /// Generate a thumbnail as WebP bytes for embedding in protos.
    ///
    /// Creates a thumbnail (max [`INLINE_THUMBNAIL_MAX_SIZE`]) compressed as WebP.
    /// For large images (>10MB), uses ffmpeg for memory-efficient streaming.
    /// For smaller images, uses in-memory decoding (faster for small files).
    ///
    /// # Arguments
    ///
    /// * `source` - Path to the source image
    ///
    /// # Returns
    ///
    /// Tuple of (webp_bytes, width, height) or error.
    pub fn generate_thumbnail_bytes(source: &Path) -> Result<(Vec<u8>, u32, u32), MediaError> {
        const LARGE_FILE_THRESHOLD: u64 = 10 * 1024 * 1024; // 10MB

        // Check file size to decide approach
        let file_size = std::fs::metadata(source).map(|m| m.len()).unwrap_or(0);

        if file_size > LARGE_FILE_THRESHOLD {
            // Large file: use ffmpeg for streaming decode (memory efficient)
            tracing::debug!(
                "Using ffmpeg for large image thumbnail: {} ({} bytes)",
                source.display(),
                file_size
            );
            return Self::generate_thumbnail_ffmpeg(source, INLINE_THUMBNAIL_MAX_SIZE);
        }

        // Small file: use in-memory decode (faster)
        let image = Self::load_image(source)?;
        // Lanczos3 (not the fast `.thumbnail()` box filter) keeps text legible.
        let thumbnail = image.resize(
            INLINE_THUMBNAIL_MAX_SIZE,
            INLINE_THUMBNAIL_MAX_SIZE,
            FilterType::Lanczos3,
        );

        let width = thumbnail.width();
        let height = thumbnail.height();

        // Encode to WebP lossless
        let rgba = thumbnail.to_rgba8();
        let mut bytes: Vec<u8> = Vec::new();
        let encoder = image::codecs::webp::WebPEncoder::new_lossless(&mut bytes);
        encoder
            .write_image(
                rgba.as_raw(),
                width,
                height,
                image::ExtendedColorType::Rgba8,
            )
            .map_err(|e| MediaError::ThumbnailGeneration(e.to_string()))?;

        tracing::debug!(
            "Generated WebP thumbnail: {}x{}, {} bytes",
            width,
            height,
            bytes.len()
        );

        Ok((bytes, width, height))
    }

    /// Generate thumbnail using ffmpeg (streaming, memory-efficient for large images).
    ///
    /// Uses ffmpeg to scale the image during decode, avoiding full image load.
    /// Output is PNG which we then convert to WebP for consistency.
    fn generate_thumbnail_ffmpeg(
        source: &Path,
        max_size: u32,
    ) -> Result<(Vec<u8>, u32, u32), MediaError> {
        use std::process::Command;

        // ffmpeg scale filter: scale to fit within max_size while preserving aspect ratio
        // -vf "scale='min(400,iw)':'min(400,ih)':force_original_aspect_ratio=decrease"
        let scale_filter = format!(
            "scale='min({max_size},iw)':'min({max_size},ih)':force_original_aspect_ratio=decrease"
        );

        let output = Command::new("ffmpeg")
            .args([
                "-i",
                source.to_str().unwrap_or(""),
                "-vf",
                &scale_filter,
                "-frames:v",
                "1",
                "-f",
                "image2pipe",
                "-vcodec",
                "png",
                "-y",
                "-", // Output to stdout
            ])
            .output()
            .map_err(|e| {
                MediaError::ThumbnailGeneration(format!("ffmpeg failed to start: {}", e))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MediaError::ThumbnailGeneration(format!(
                "ffmpeg failed: {}",
                stderr.lines().last().unwrap_or("unknown error")
            )));
        }

        let png_bytes = output.stdout;
        if png_bytes.is_empty() {
            return Err(MediaError::ThumbnailGeneration(
                "ffmpeg produced no output".into(),
            ));
        }

        // Decode PNG to get dimensions and convert to WebP
        let image = image::load_from_memory(&png_bytes).map_err(|e| {
            MediaError::ThumbnailGeneration(format!("failed to decode ffmpeg output: {}", e))
        })?;

        let width = image.width();
        let height = image.height();

        // Convert to WebP for smaller size
        let rgba = image.to_rgba8();
        let mut webp_bytes: Vec<u8> = Vec::new();
        let encoder = image::codecs::webp::WebPEncoder::new_lossless(&mut webp_bytes);
        encoder
            .write_image(
                rgba.as_raw(),
                width,
                height,
                image::ExtendedColorType::Rgba8,
            )
            .map_err(|e| MediaError::ThumbnailGeneration(e.to_string()))?;

        tracing::debug!(
            "Generated WebP thumbnail via ffmpeg: {}x{}, {} bytes (from {} byte PNG)",
            width,
            height,
            webp_bytes.len(),
            png_bytes.len()
        );

        Ok((webp_bytes, width, height))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn tray_preview_is_small_and_rejects_invalid_images() {
        let source = image::DynamicImage::new_rgba8(640, 320);
        let mut bytes = std::io::Cursor::new(Vec::new());
        source
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let pixels = super::ImageProcessor::tray_preview_from_memory(bytes.get_ref()).unwrap();
        assert_eq!(pixels.dimensions(), (160, 80));
        assert_eq!(pixels.as_raw().len(), 160 * 80 * 4);
        assert!(super::ImageProcessor::tray_preview_from_memory(b"bad photo").is_err());
    }

    use super::*;

    #[test]
    fn test_is_supported_image() {
        assert!(ImageProcessor::is_supported_image(Path::new("photo.png")));
        assert!(ImageProcessor::is_supported_image(Path::new("photo.jpg")));
        assert!(ImageProcessor::is_supported_image(Path::new("photo.JPEG")));
        assert!(ImageProcessor::is_supported_image(Path::new("photo.gif")));
        assert!(ImageProcessor::is_supported_image(Path::new("photo.webp")));
        assert!(!ImageProcessor::is_supported_image(Path::new("video.mp4")));
        assert!(!ImageProcessor::is_supported_image(Path::new(
            "document.pdf"
        )));
    }

    #[test]
    fn test_default_thumbnail_size() {
        assert_eq!(DEFAULT_THUMBNAIL_SIZE, 256);
    }
}

#[cfg(test)]
mod webp_roundtrip_tests {
    use super::*;

    // The inline thumbnail is encoded lossless-WebP by generate_thumbnail_from_memory
    // and decoded back by inline_pixels_from_memory for display. If that round-trip
    // does not preserve pixels, stored thumbnails render as garbage (the grey/noise
    // album tiles). This pins the round-trip.
    #[test]
    fn inline_thumbnail_webp_roundtrip_preserves_pixels() {
        // A deterministic, non-flat gradient so a garbled decode is obvious.
        let mut src = image::RgbaImage::new(200, 120);
        for (x, y, px) in src.enumerate_pixels_mut() {
            *px = image::Rgba([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8, 255]);
        }
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(src)
            .write_to(&mut png, ImageFormat::Png)
            .unwrap();

        let (webp_bytes, w, h) =
            ImageProcessor::generate_thumbnail_from_memory(png.get_ref()).unwrap();
        assert!(w > 0 && h > 0);

        let decoded = ImageProcessor::inline_pixels_from_memory(&webp_bytes).unwrap();
        // The thumbnail is downscaled; sample the center to avoid edge filtering.
        let (dw, dh) = decoded.dimensions();
        let px = decoded.get_pixel(dw / 2, dh / 2);
        // A correct decode of this gradient is never near-black or near-white noise.
        let [r, g, b, a] = px.0;
        assert_eq!(a, 255, "alpha corrupted");
        assert!(
            !(r < 8 && g < 8 && b < 8),
            "center pixel decoded to black — webp round-trip is broken: {:?}",
            px.0
        );
        // Decoded dims must match the reported thumbnail dims (after the 320 bound).
        assert!(dw <= 320 && dh <= 320, "unexpected thumbnail size {dw}x{dh}");
    }

    // Reproduces the cached-preview path exactly: media.rs encodes the derived
    // thumbnail with `DynamicImage::write_to(.., ImageFormat::WebP)` and later
    // decodes it with `inline_pixels_from_memory`. The album tiles decode to a
    // near-uniform grey (mean ~160, tiny structure) — this asserts the real
    // encode/decode pair keeps the image's structure.
    #[test]
    fn cached_preview_webp_write_to_roundtrip_keeps_structure() {
        // Photo-like: strong diagonal gradient + per-pixel detail.
        let mut src = image::RgbaImage::new(320, 240);
        for (x, y, px) in src.enumerate_pixels_mut() {
            let r = ((x * 255) / 320) as u8;
            let g = ((y * 255) / 240) as u8;
            let b = (((x ^ y) * 3) % 256) as u8;
            *px = image::Rgba([r, g, b, 255]);
        }
        let src_dyn = image::DynamicImage::ImageRgba8(src.clone());

        // EXACT media.rs cache encode.
        let mut encoded = std::io::Cursor::new(Vec::new());
        src_dyn.write_to(&mut encoded, ImageFormat::WebP).unwrap();

        // EXACT display decode.
        let decoded = ImageProcessor::inline_pixels_from_memory(encoded.get_ref()).unwrap();
        let (dw, dh) = decoded.dimensions();

        // Compare a downscaled copy of the source at the decoded size: a correct
        // decode is close; a collapsed/garbled decode diverges hugely.
        let ref_small = image::DynamicImage::ImageRgba8(src)
            .thumbnail(dw, dh)
            .into_rgba8();
        let mut max_err = 0i32;
        let mut sum_err = 0i64;
        let mut count = 0i64;
        for (p, q) in decoded.pixels().zip(ref_small.pixels()) {
            for c in 0..3 {
                let e = (p.0[c] as i32 - q.0[c] as i32).abs();
                max_err = max_err.max(e);
                sum_err += e as i64;
                count += 1;
            }
        }
        let mean_err = sum_err as f64 / count as f64;
        assert!(
            mean_err < 12.0 && max_err < 80,
            "webp write_to/decode diverged from source: mean_err={mean_err:.1} max_err={max_err} size={dw}x{dh} (grey-garbage decode)"
        );
    }
}

#[cfg(test)]
mod preview_budget_tests {
    use super::*;

    #[test]
    fn small_sidecars_keep_native_dimensions_and_pixels() {
        let source = image::RgbaImage::from_fn(80, 40, |x, y| {
            image::Rgba([x as u8, y as u8, 210, 127])
        });
        for format in [ImageFormat::Png, ImageFormat::WebP] {
            let mut encoded = Cursor::new(Vec::new());
            DynamicImage::ImageRgba8(source.clone()).write_to(&mut encoded, format).unwrap();
            let inline = ImageProcessor::inline_pixels_from_memory(encoded.get_ref()).unwrap();
            let tray = ImageProcessor::tray_preview_from_memory(encoded.get_ref()).unwrap();
            assert_eq!(inline, source);
            assert_eq!(tray, source);
            let file = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(file.path(), encoded.get_ref()).unwrap();
            assert_eq!(ImageProcessor::inline_pixels_from_path(file.path()).unwrap(), source);
        }
    }

    #[test]
    fn disk_and_memory_previews_have_same_bounded_pixels_and_reject_bad_sources() {
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::new_rgb8(640, 320)
            .write_to(&mut encoded, ImageFormat::Png)
            .unwrap();
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), encoded.get_ref()).unwrap();
        let memory = ImageProcessor::inline_pixels_from_memory(encoded.get_ref()).unwrap();
        let disk = ImageProcessor::inline_pixels_from_path(file.path()).unwrap();
        assert_eq!(memory.dimensions(), (320, 160));
        assert_eq!(memory, disk);
        assert!(ImageProcessor::inline_pixels_from_memory(b"not an image").is_err());
        std::fs::write(file.path(), b"not an image").unwrap();
        assert!(ImageProcessor::inline_pixels_from_path(file.path()).is_err());
        // Sparse source proves encoded admission precedes reading its contents.
        file.as_file().set_len(PREVIEW_ENCODED_BYTES + 1).unwrap();
        let error = ImageProcessor::inline_pixels_from_path(file.path()).unwrap_err();
        assert!(error.to_string().contains("byte budget"));
    }

    #[test]
    fn source_pixel_budget_rejects_small_encoded_large_canvas_before_decode() {
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::new_rgb8(1, 1).write_to(&mut encoded, ImageFormat::Png).unwrap();
        let data = encoded.get_mut();
        data[16..20].copy_from_slice(&8192u32.to_be_bytes());
        data[20..24].copy_from_slice(&4096u32.to_be_bytes());
        // PNG chunk CRC covers the type and header bytes. Keep the tiny
        // fixture valid through header admission; it contains no large pixels.
        let mut crc = !0u32;
        for byte in &data[12..29] {
            crc ^= u32::from(*byte);
            for _ in 0..8 { crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1)); }
        }
        data[29..33].copy_from_slice(&(!crc).to_be_bytes());
        let error = ImageProcessor::inline_pixels_from_memory(data).unwrap_err();
        assert!(error.to_string().contains("pixel budget"), "{error}");
    }
}
