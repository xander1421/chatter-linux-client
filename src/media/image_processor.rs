//! Image processor.
//!
//! Image manipulation for thumbnail generation and format conversion.

use crate::error::MediaError;
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat, ImageReader};
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
    fn load_image(path: &Path) -> Result<DynamicImage, MediaError> {
        ImageReader::open(path)
            .map_err(|e| MediaError::InvalidFormat(format!("cannot open image: {}", e)))?
            .with_guessed_format()
            .map_err(|e| MediaError::InvalidFormat(format!("cannot determine format: {}", e)))?
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
