//! File picker.
//!
//! Native file dialogs using rfd (Rusty File Dialogs).
//!
//! On Linux rfd is built with the `gtk3` backend (in-process
//! `GtkFileChooserDialog` via gtk-sys) rather than the portal/ashpd path.
//! Hyprland and other wlroots-derived compositors removed `xdg_foreign`
//! support, which makes the portal FileChooser unusable: without an
//! exportable parent surface, xdg-desktop-portal-gtk's FileChooser impl
//! hangs forever waiting for one. gtk3 dialogs render their own toplevel
//! and don't need a parent identifier.

use std::path::PathBuf;

/// Configuration for file picker dialogs.
#[derive(Clone, Debug, Default)]
pub struct FilePickerConfig {
    /// Dialog title.
    pub title: Option<String>,
    /// File type filters.
    pub filters: Vec<FileFilter>,
    /// Starting directory.
    pub default_path: Option<PathBuf>,
    /// Allow multiple file selection.
    pub multiple: bool,
}

impl FilePickerConfig {
    /// Create a new config with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the dialog title.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Add a file filter.
    #[must_use]
    pub fn with_filter(mut self, filter: FileFilter) -> Self {
        self.filters.push(filter);
        self
    }

    /// Set multiple filters at once.
    #[must_use]
    pub fn with_filters(mut self, filters: Vec<FileFilter>) -> Self {
        self.filters = filters;
        self
    }

    /// Set the starting directory.
    #[must_use]
    pub fn with_default_path(mut self, path: PathBuf) -> Self {
        self.default_path = Some(path);
        self
    }

    /// Enable multiple file selection.
    #[must_use]
    pub fn with_multiple(mut self, multiple: bool) -> Self {
        self.multiple = multiple;
        self
    }
}

/// File type filter for dialogs.
#[derive(Clone, Debug)]
pub struct FileFilter {
    /// Filter label (e.g., "Images").
    pub name: String,
    /// File extensions without dots (e.g., ["png", "jpg"]).
    pub extensions: Vec<String>,
}

impl FileFilter {
    /// Create a new file filter.
    #[must_use]
    pub fn new(name: impl Into<String>, extensions: Vec<&str>) -> Self {
        Self {
            name: name.into(),
            extensions: extensions.into_iter().map(String::from).collect(),
        }
    }

    /// Filter for image files.
    #[must_use]
    pub fn images() -> Self {
        Self::new(
            "Images",
            vec!["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp"],
        )
    }

    /// Filter for video files.
    #[must_use]
    pub fn videos() -> Self {
        Self::new("Videos", vec!["mp4", "webm", "mov", "avi", "mkv"])
    }

    /// Filter for audio files.
    #[must_use]
    pub fn audio() -> Self {
        Self::new("Audio", vec!["mp3", "wav", "ogg", "flac", "m4a"])
    }

    /// Filter for document files.
    #[must_use]
    pub fn documents() -> Self {
        Self::new("Documents", vec!["pdf", "doc", "docx", "txt", "md", "rtf"])
    }

    /// Filter for all files (no restriction).
    #[must_use]
    pub fn all() -> Self {
        Self::new("All Files", vec!["*"])
    }
}

/// Native file picker using rfd.
///
/// Provides async file dialogs for selecting and saving files.
/// All methods are stateless - configuration is passed per call.
pub struct FilePicker;

fn build_dialog(config: &FilePickerConfig) -> rfd::AsyncFileDialog {
    let mut dialog = rfd::AsyncFileDialog::new();
    if let Some(title) = &config.title {
        dialog = dialog.set_title(title);
    }
    if let Some(path) = &config.default_path {
        dialog = dialog.set_directory(path);
    }
    for filter in &config.filters {
        let extensions: Vec<&str> = filter.extensions.iter().map(String::as_str).collect();
        dialog = dialog.add_filter(&filter.name, &extensions);
    }
    dialog
}

/// Whether a native file dialog can be shown at all.
///
/// `rfd` needs a display server. Under `cargo test`, in CI, or over a plain
/// SSH session there is none, and asking for a dialog either blocks waiting
/// for input nobody can give or pops a window on a developer's desktop in the
/// middle of a test run. Both are wrong, so every entry point below
/// short-circuits to "cancelled" instead.
///
/// `CHATTER_HEADLESS=1` forces this on for environments that have a display
/// but still must not open windows (an unattended integration run on a
/// developer machine).
fn dialogs_available() -> bool {
    if std::env::var_os("CHATTER_HEADLESS").is_some() {
        return false;
    }
    if cfg!(test) {
        return false;
    }
    if cfg!(target_os = "linux") {
        return std::env::var_os("DISPLAY").is_some()
            || std::env::var_os("WAYLAND_DISPLAY").is_some();
    }
    true
}

impl FilePicker {
    /// Open a single file selection dialog.
    ///
    /// Returns `None` if the user cancels the dialog.
    pub async fn pick_file(config: FilePickerConfig) -> Option<PathBuf> {
        if !dialogs_available() {
            tracing::debug!("[file_picker] headless — pick_file resolves to cancelled");
            return None;
        }
        tracing::info!("[file_picker] pick_file — opening rfd dialog");
        let result = build_dialog(&config)
            .pick_file()
            .await
            .map(|f| f.path().to_path_buf());
        tracing::info!(
            picked = result.is_some(),
            "[file_picker] pick_file returned"
        );
        result
    }

    /// Open a multiple file selection dialog.
    ///
    /// Returns an empty vector if the user cancels the dialog.
    pub async fn pick_files(config: FilePickerConfig) -> Vec<PathBuf> {
        if !dialogs_available() {
            tracing::debug!("[file_picker] headless — pick_files resolves to none");
            return Vec::new();
        }
        tracing::info!("[file_picker] pick_files — opening rfd dialog");
        let result: Vec<PathBuf> = build_dialog(&config)
            .pick_files()
            .await
            .map(|files| files.into_iter().map(|f| f.path().to_path_buf()).collect())
            .unwrap_or_default();
        tracing::info!(count = result.len(), "[file_picker] pick_files returned");
        result
    }

    /// Open a folder selection dialog.
    ///
    /// Returns `None` if the user cancels the dialog.
    pub async fn pick_folder(title: Option<&str>) -> Option<PathBuf> {
        if !dialogs_available() {
            tracing::debug!("[file_picker] headless — pick_folder resolves to cancelled");
            return None;
        }
        let mut dialog = rfd::AsyncFileDialog::new();

        if let Some(title) = title {
            dialog = dialog.set_title(title);
        }

        dialog.pick_folder().await.map(|f| f.path().to_path_buf())
    }

    /// Open a save file dialog.
    ///
    /// Returns `None` if the user cancels the dialog.
    pub async fn save_file(config: FilePickerConfig) -> Option<PathBuf> {
        if !dialogs_available() {
            tracing::debug!("[file_picker] headless — save_file resolves to cancelled");
            return None;
        }
        let mut dialog = rfd::AsyncFileDialog::new();

        if let Some(title) = &config.title {
            dialog = dialog.set_title(title);
        }

        if let Some(path) = &config.default_path {
            dialog = dialog.set_directory(path);
        }

        for filter in &config.filters {
            let extensions: Vec<&str> = filter.extensions.iter().map(String::as_str).collect();
            dialog = dialog.add_filter(&filter.name, &extensions);
        }

        dialog.save_file().await.map(|f| f.path().to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_filter_images() {
        let filter = FileFilter::images();
        assert_eq!(filter.name, "Images");
        assert!(filter.extensions.contains(&"png".to_string()));
        assert!(filter.extensions.contains(&"jpg".to_string()));
    }

    #[test]
    fn test_file_filter_videos() {
        let filter = FileFilter::videos();
        assert_eq!(filter.name, "Videos");
        assert!(filter.extensions.contains(&"mp4".to_string()));
    }

    #[test]
    fn test_file_filter_audio() {
        let filter = FileFilter::audio();
        assert_eq!(filter.name, "Audio");
        assert!(filter.extensions.contains(&"mp3".to_string()));
    }

    #[test]
    fn test_file_filter_documents() {
        let filter = FileFilter::documents();
        assert_eq!(filter.name, "Documents");
        assert!(filter.extensions.contains(&"pdf".to_string()));
    }

    #[test]
    fn test_config_builder() {
        let config = FilePickerConfig::new()
            .with_title("Select File")
            .with_filter(FileFilter::images())
            .with_multiple(true);

        assert_eq!(config.title, Some("Select File".to_string()));
        assert!(config.multiple);
        assert_eq!(config.filters.len(), 1);
    }
}

#[cfg(test)]
mod headless_tests {
    use super::*;

    /// Under `cargo test` no dialog may ever be attempted: the run is
    /// unattended, so a dialog either blocks forever or steals focus from
    /// whoever is at the keyboard.
    #[test]
    fn dialogs_are_never_available_under_test() {
        assert!(!dialogs_available());
    }

    #[tokio::test]
    async fn pickers_resolve_to_cancelled_when_headless() {
        assert!(
            FilePicker::pick_file(FilePickerConfig::new())
                .await
                .is_none()
        );
        assert!(
            FilePicker::pick_files(FilePickerConfig::new())
                .await
                .is_empty()
        );
        assert!(FilePicker::pick_folder(None).await.is_none());
        assert!(
            FilePicker::save_file(FilePickerConfig::new())
                .await
                .is_none()
        );
    }
}
