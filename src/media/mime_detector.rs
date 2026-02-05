//! MIME type detector.
//!
//! Detect MIME types from file content using magic bytes and extension guessing.

use crate::error::MediaError;
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Default MIME type for unknown files.
const DEFAULT_MIME: &str = "application/octet-stream";

/// Number of bytes to read for magic byte detection.
const MAGIC_BYTE_READ_SIZE: usize = 16;

/// MIME type detector.
///
/// Detects MIME types using magic bytes (file headers) with extension-based fallback.
/// Stateless - all methods can be called without state.
pub struct MimeDetector;

impl MimeDetector {
    /// Detect MIME type from a file path.
    ///
    /// Strategy:
    /// 1. Read first 16 bytes and check magic bytes
    /// 2. Fall back to extension-based detection
    /// 3. Return "application/octet-stream" if unknown
    ///
    /// # Errors
    ///
    /// Returns `MediaError::FileRead` if the file cannot be opened or read.
    pub fn detect_from_path(path: &Path) -> Result<String, MediaError> {
        // Read first bytes for magic detection
        let mut file = File::open(path)?;
        let mut buffer = [0u8; MAGIC_BYTE_READ_SIZE];
        let bytes_read = file.read(&mut buffer)?;

        // Get filename for extension fallback
        let filename = path.file_name().and_then(|s| s.to_str());

        // Use detect_from_bytes with filename fallback
        Ok(Self::detect_from_bytes(&buffer[..bytes_read], filename))
    }

    /// Guess MIME type from filename extension only.
    ///
    /// Returns `None` if the extension is unknown.
    #[must_use]
    pub fn detect_from_extension(filename: &str) -> Option<String> {
        mime_guess::from_path(filename)
            .first()
            .map(|mime| mime.essence_str().to_string())
    }

    /// Detect MIME type from raw bytes with optional filename fallback.
    ///
    /// Checks magic bytes first, then falls back to extension-based detection.
    /// Returns "application/octet-stream" if detection fails.
    #[must_use]
    pub fn detect_from_bytes(bytes: &[u8], filename: Option<&str>) -> String {
        // Try magic byte detection first
        if let Some(mime) = Self::detect_from_magic(bytes) {
            return mime.to_string();
        }

        // Fall back to extension-based detection
        if let Some(name) = filename {
            if let Some(mime) = Self::detect_from_extension(name) {
                return mime;
            }
        }

        // Default fallback
        DEFAULT_MIME.to_string()
    }

    /// Detect MIME type from magic bytes (file header).
    ///
    /// Returns `None` if no known magic bytes match.
    #[must_use]
    fn detect_from_magic(bytes: &[u8]) -> Option<&'static str> {
        if bytes.len() < 4 {
            return None;
        }

        // PNG: 89 50 4E 47 0D 0A 1A 0A
        if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
            return Some("image/png");
        }

        // JPEG: FF D8 FF
        if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            return Some("image/jpeg");
        }

        // GIF: GIF87a or GIF89a
        if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            return Some("image/gif");
        }

        // WEBP: RIFF....WEBP
        if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
            return Some("image/webp");
        }

        // PDF: %PDF
        if bytes.starts_with(b"%PDF") {
            return Some("application/pdf");
        }

        // MP4/MOV: ....ftyp (at offset 4)
        if bytes.len() >= 8 && &bytes[4..8] == b"ftyp" {
            return Some("video/mp4");
        }

        // MP3: FF FB (MPEG frame sync) or ID3 (ID3 tag)
        if bytes.starts_with(&[0xFF, 0xFB])
            || bytes.starts_with(&[0xFF, 0xFA])
            || bytes.starts_with(b"ID3")
        {
            return Some("audio/mpeg");
        }

        // WAV: RIFF....WAVE
        if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WAVE" {
            return Some("audio/wav");
        }

        // ZIP: PK (50 4B 03 04)
        if bytes.starts_with(&[0x50, 0x4B, 0x03, 0x04]) {
            return Some("application/zip");
        }

        // OGG: OggS
        if bytes.starts_with(b"OggS") {
            return Some("audio/ogg");
        }

        // FLAC: fLaC
        if bytes.starts_with(b"fLaC") {
            return Some("audio/flac");
        }

        None
    }

    /// Check if the MIME type represents an image.
    #[must_use]
    pub fn is_image(mime: &str) -> bool {
        mime.starts_with("image/")
    }

    /// Check if the MIME type represents a video.
    #[must_use]
    pub fn is_video(mime: &str) -> bool {
        mime.starts_with("video/")
    }

    /// Check if the MIME type represents audio.
    #[must_use]
    pub fn is_audio(mime: &str) -> bool {
        mime.starts_with("audio/")
    }

    /// Check if the MIME type can be previewed in the application.
    ///
    /// Returns `true` for images, PDFs, and text/code files.
    #[must_use]
    pub fn is_previewable(mime: &str) -> bool {
        Self::is_image(mime)
            || mime == "application/pdf"
            || mime.starts_with("text/")
            || mime.contains("javascript")
            || mime.contains("json")
            || mime.contains("xml")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_magic_png() {
        let png_header = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(
            MimeDetector::detect_from_bytes(&png_header, None),
            "image/png"
        );
    }

    #[test]
    fn test_magic_jpeg() {
        let jpeg_header = [0xFF, 0xD8, 0xFF, 0xE0];
        assert_eq!(
            MimeDetector::detect_from_bytes(&jpeg_header, None),
            "image/jpeg"
        );
    }

    #[test]
    fn test_magic_gif() {
        assert_eq!(
            MimeDetector::detect_from_bytes(b"GIF89a......", None),
            "image/gif"
        );
        assert_eq!(
            MimeDetector::detect_from_bytes(b"GIF87a......", None),
            "image/gif"
        );
    }

    #[test]
    fn test_magic_webp() {
        let webp = b"RIFF....WEBP....";
        assert_eq!(MimeDetector::detect_from_bytes(webp, None), "image/webp");
    }

    #[test]
    fn test_magic_pdf() {
        assert_eq!(
            MimeDetector::detect_from_bytes(b"%PDF-1.4.....", None),
            "application/pdf"
        );
    }

    #[test]
    fn test_magic_mp4() {
        let mp4 = [0x00, 0x00, 0x00, 0x20, 0x66, 0x74, 0x79, 0x70]; // ....ftyp
        assert_eq!(MimeDetector::detect_from_bytes(&mp4, None), "video/mp4");
    }

    #[test]
    fn test_magic_mp3_id3() {
        assert_eq!(
            MimeDetector::detect_from_bytes(b"ID3.........", None),
            "audio/mpeg"
        );
    }

    #[test]
    fn test_magic_wav() {
        let wav = b"RIFF....WAVE....";
        assert_eq!(MimeDetector::detect_from_bytes(wav, None), "audio/wav");
    }

    #[test]
    fn test_magic_zip() {
        let zip = [0x50, 0x4B, 0x03, 0x04, 0x00, 0x00];
        assert_eq!(
            MimeDetector::detect_from_bytes(&zip, None),
            "application/zip"
        );
    }

    #[test]
    fn test_extension_fallback() {
        // Unknown magic bytes, but known extension
        let unknown = [0x00, 0x00, 0x00, 0x00];
        assert_eq!(
            MimeDetector::detect_from_bytes(&unknown, Some("document.pdf")),
            "application/pdf"
        );
    }

    #[test]
    fn test_default_fallback() {
        let unknown = [0x00, 0x00, 0x00, 0x00];
        assert_eq!(
            MimeDetector::detect_from_bytes(&unknown, Some("file.unknown")),
            "application/octet-stream"
        );
    }

    #[test]
    fn test_detect_from_extension() {
        assert_eq!(
            MimeDetector::detect_from_extension("photo.png"),
            Some("image/png".to_string())
        );
        assert_eq!(
            MimeDetector::detect_from_extension("video.mp4"),
            Some("video/mp4".to_string())
        );
        assert_eq!(MimeDetector::detect_from_extension("file.xyz123"), None);
    }

    #[test]
    fn test_is_image() {
        assert!(MimeDetector::is_image("image/png"));
        assert!(MimeDetector::is_image("image/jpeg"));
        assert!(!MimeDetector::is_image("video/mp4"));
    }

    #[test]
    fn test_is_video() {
        assert!(MimeDetector::is_video("video/mp4"));
        assert!(MimeDetector::is_video("video/webm"));
        assert!(!MimeDetector::is_video("image/png"));
    }

    #[test]
    fn test_is_audio() {
        assert!(MimeDetector::is_audio("audio/mpeg"));
        assert!(MimeDetector::is_audio("audio/wav"));
        assert!(!MimeDetector::is_audio("video/mp4"));
    }

    #[test]
    fn test_is_previewable() {
        assert!(MimeDetector::is_previewable("image/png"));
        assert!(MimeDetector::is_previewable("application/pdf"));
        assert!(MimeDetector::is_previewable("text/plain"));
        assert!(MimeDetector::is_previewable("application/json"));
        assert!(!MimeDetector::is_previewable("application/zip"));
    }
}
