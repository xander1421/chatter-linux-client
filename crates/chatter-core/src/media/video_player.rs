//! Video player.
//!
//! Video playback integration that delegates to system video players.
//!
//! Rationale: Slint doesn't have built-in video playback, and embedding
//! GStreamer/ffmpeg adds significant complexity. For v1, we delegate to
//! the system's default video player (matches "ship first" philosophy).

use std::path::Path;

use crate::error::MediaError;

/// Video player that delegates to system video player.
///
/// This is a stateless struct that provides methods for opening videos
/// in external applications. It follows the same pattern as other media
/// processors in this module (FilePicker, MimeDetector, ImageProcessor).
pub struct VideoPlayer;

impl VideoPlayer {
    /// Opens a video file in the system's default video player.
    ///
    /// Uses `xdg-open` on Linux to launch the appropriate application.
    /// The call is non-blocking - the video player runs in a separate process.
    ///
    /// # Errors
    ///
    /// Returns an error if the file doesn't exist or cannot be opened.
    pub fn play(path: &Path) -> Result<(), MediaError> {
        if !path.exists() {
            return Err(MediaError::FileRead(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("video file not found: {}", path.display()),
            )));
        }

        // Use open::that_in_background to not block the UI thread
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            open::that_in_background(path);
        }
        #[cfg(any(target_os = "android", target_os = "ios"))]
        {
            tracing::warn!("opening video via system player is not supported on Android: {}", path.display());
        }

        Ok(())
    }

    /// Opens a video file with a specific application.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the video file.
    /// * `app` - Application name (e.g., "mpv", "vlc", "totem").
    ///
    /// # Errors
    ///
    /// Returns an error if the file doesn't exist or cannot be opened.
    pub fn play_with_app(path: &Path, app: &str) -> Result<(), MediaError> {
        if !path.exists() {
            return Err(MediaError::FileRead(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("video file not found: {}", path.display()),
            )));
        }

        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            open::with_in_background(path, app);
        }
        #[cfg(any(target_os = "android", target_os = "ios"))]
        {
            let _ = app;
            tracing::warn!("opening video with specific app is not supported on Android: {}", path.display());
        }

        Ok(())
    }

    /// Checks if a MIME type is a supported video format.
    ///
    /// # Arguments
    ///
    /// * `mime_type` - MIME type string (e.g., "video/mp4").
    ///
    /// # Returns
    ///
    /// `true` if the MIME type is a supported video format.
    pub fn is_supported_format(mime_type: &str) -> bool {
        matches!(
            mime_type,
            "video/mp4"
                | "video/webm"
                | "video/x-matroska"  // mkv
                | "video/quicktime"   // mov
                | "video/x-msvideo"   // avi
                | "video/mpeg"
                | "video/ogg"
                | "video/3gpp"
                | "video/x-flv"
        )
    }
}
