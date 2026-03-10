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

/// JPEG quality for thumbnail compression (0-100).
const JPEG_QUALITY: u8 = 85;

/// Image processor for thumbnails and metadata.
///
/// Provides stateless image operations for thumbnail generation and format conversion.
pub struct ImageProcessor;

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

        reader
            .decode()
            .map_err(|e| MediaError::InvalidFormat(format!("cannot decode image from memory: {}", e)))
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

    /// Convert a DynamicImage to Slint Image for display.
    ///
    /// Converts the image to RGBA8 format compatible with Slint.
    #[must_use]
    pub fn to_slint_image(image: &DynamicImage) -> slint::Image {
        let rgba = image.to_rgba8();
        let width = rgba.width();
        let height = rgba.height();
        let data = rgba.into_raw();

        slint::Image::from_rgba8(slint::SharedPixelBuffer::clone_from_slice(
            &data,
            width,
            height,
        ))
    }

    /// Load an image from memory and convert directly to Slint Image.
    ///
    /// Convenience method combining load_from_memory and to_slint_image.
    pub fn memory_to_slint_image(data: &[u8]) -> Result<slint::Image, MediaError> {
        let image = Self::load_from_memory(data)?;
        Ok(Self::to_slint_image(&image))
    }

    /// Generate thumbnail bytes from in-memory image data.
    ///
    /// Returns WebP-encoded thumbnail bytes with dimensions.
    pub fn generate_thumbnail_from_memory(data: &[u8]) -> Result<(Vec<u8>, u32, u32), MediaError> {
        const THUMBNAIL_MAX_SIZE: u32 = 400;

        let image = Self::load_from_memory(data)?;
        let thumbnail = image.thumbnail(THUMBNAIL_MAX_SIZE, THUMBNAIL_MAX_SIZE);

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
    /// Creates a thumbnail (max 400px) compressed as WebP.
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
        const THUMBNAIL_MAX_SIZE: u32 = 400;
        const LARGE_FILE_THRESHOLD: u64 = 10 * 1024 * 1024; // 10MB

        // Check file size to decide approach
        let file_size = std::fs::metadata(source)
            .map(|m| m.len())
            .unwrap_or(0);

        if file_size > LARGE_FILE_THRESHOLD {
            // Large file: use ffmpeg for streaming decode (memory efficient)
            tracing::debug!(
                "Using ffmpeg for large image thumbnail: {} ({} bytes)",
                source.display(),
                file_size
            );
            return Self::generate_thumbnail_ffmpeg(source, THUMBNAIL_MAX_SIZE);
        }

        // Small file: use in-memory decode (faster)
        let image = Self::load_image(source)?;
        let thumbnail = image.thumbnail(THUMBNAIL_MAX_SIZE, THUMBNAIL_MAX_SIZE);

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
    fn generate_thumbnail_ffmpeg(source: &Path, max_size: u32) -> Result<(Vec<u8>, u32, u32), MediaError> {
        use std::process::Command;

        // ffmpeg scale filter: scale to fit within max_size while preserving aspect ratio
        // -vf "scale='min(400,iw)':'min(400,ih)':force_original_aspect_ratio=decrease"
        let scale_filter = format!(
            "scale='min({max_size},iw)':'min({max_size},ih)':force_original_aspect_ratio=decrease"
        );

        let output = Command::new("ffmpeg")
            .args([
                "-i", source.to_str().unwrap_or(""),
                "-vf", &scale_filter,
                "-frames:v", "1",
                "-f", "image2pipe",
                "-vcodec", "png",
                "-y",
                "-"  // Output to stdout
            ])
            .output()
            .map_err(|e| MediaError::ThumbnailGeneration(format!("ffmpeg failed to start: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MediaError::ThumbnailGeneration(format!(
                "ffmpeg failed: {}",
                stderr.lines().last().unwrap_or("unknown error")
            )));
        }

        let png_bytes = output.stdout;
        if png_bytes.is_empty() {
            return Err(MediaError::ThumbnailGeneration("ffmpeg produced no output".into()));
        }

        // Decode PNG to get dimensions and convert to WebP
        let image = image::load_from_memory(&png_bytes)
            .map_err(|e| MediaError::ThumbnailGeneration(format!("failed to decode ffmpeg output: {}", e)))?;

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
    use super::*;

    #[test]
    fn test_is_supported_image() {
        assert!(ImageProcessor::is_supported_image(Path::new("photo.png")));
        assert!(ImageProcessor::is_supported_image(Path::new("photo.jpg")));
        assert!(ImageProcessor::is_supported_image(Path::new("photo.JPEG")));
        assert!(ImageProcessor::is_supported_image(Path::new("photo.gif")));
        assert!(ImageProcessor::is_supported_image(Path::new("photo.webp")));
        assert!(!ImageProcessor::is_supported_image(Path::new("video.mp4")));
        assert!(!ImageProcessor::is_supported_image(Path::new("document.pdf")));
    }

    #[test]
    fn test_default_thumbnail_size() {
        assert_eq!(DEFAULT_THUMBNAIL_SIZE, 256);
    }
}
