//! Video recorder.
//!
//! Video message recording that delegates to system camera apps.
//!
//! Rationale: Video recording requires V4L2/PipeWire integration which is complex
//! and varies significantly across desktop environments. Delegating to system
//! camera apps (Cheese, Kamoso, guvcview) provides a consistent user experience
//! and avoids significant implementation complexity for v1.
//!
//! Users can record video via their preferred camera app and share the file
//! through the normal file attachment flow.

use std::process::Command;

use crate::error::MediaError;

/// Known Linux camera applications in preference order.
const CAMERA_APPS: &[&str] = &[
    "cheese",   // GNOME
    "kamoso",   // KDE
    "guvcview", // Generic GTK
];

/// Video recorder that delegates to system camera apps.
///
/// This is a stateless struct that provides methods for launching camera
/// applications. It follows the same "external delegation" pattern as
/// `VideoPlayer` for complex media operations.
pub struct VideoRecorder;

impl VideoRecorder {
    /// Launches the default camera application for video recording.
    ///
    /// Tries to find an available camera app (cheese, kamoso, guvcview)
    /// and launches it in the background.
    ///
    /// # Errors
    ///
    /// Returns an error if no camera application is available.
    pub fn record() -> Result<(), MediaError> {
        let app = Self::find_camera_app().ok_or_else(|| {
            MediaError::RecordingError(
                "no camera app found (install cheese, kamoso, or guvcview)".to_string(),
            )
        })?;

        Self::record_with_app(app)
    }

    /// Launches a specific camera application.
    ///
    /// # Arguments
    ///
    /// * `app` - Application name (e.g., "cheese", "kamoso", "guvcview").
    ///
    /// # Errors
    ///
    /// Returns an error if the application cannot be launched.
    pub fn record_with_app(app: &str) -> Result<(), MediaError> {
        #[cfg(not(target_os = "android"))]
        {
            open::that_in_background(app);
        }
        #[cfg(target_os = "android")]
        {
            tracing::warn!("video recording via system camera apps is not supported on Android");
        }
        Ok(())
    }

    /// Checks if any camera application is available.
    ///
    /// Returns `true` if at least one known camera app is installed.
    pub fn is_camera_available() -> bool {
        Self::find_camera_app().is_some()
    }

    /// Lists available camera applications.
    ///
    /// Returns a list of installed camera apps that can be used for recording.
    pub fn available_apps() -> Vec<&'static str> {
        CAMERA_APPS
            .iter()
            .filter(|&&app| Self::is_app_installed(app))
            .copied()
            .collect()
    }

    /// Finds the first available camera application.
    fn find_camera_app() -> Option<&'static str> {
        CAMERA_APPS
            .iter()
            .find(|&&app| Self::is_app_installed(app))
            .copied()
    }

    /// Checks if an application is installed using `which`.
    fn is_app_installed(app: &str) -> bool {
        Command::new("which")
            .arg(app)
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }
}
