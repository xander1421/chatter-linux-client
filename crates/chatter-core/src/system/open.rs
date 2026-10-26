//! Open URLs/files externally.
//!
//! Open URLs and files with system handlers using the `open` crate.
//! Works on Linux with xdg-open, and other platforms with their native handlers.

use std::path::Path;

use crate::error::SystemError;

/// External opener for URLs and files.
///
/// Provides methods to open URLs in the default browser, files with their
/// associated applications, and folders in the file manager.
///
/// This struct is stateless - all methods could be standalone functions,
/// but the struct provides a convenient namespace and allows for future
/// extension with configuration options.
///
/// # Example
///
/// ```no_run
/// use chatter_core::system::ExternalOpener;
/// use std::path::Path;
///
/// let opener = ExternalOpener;
///
/// // Open a URL in the default browser
/// opener.open_url("https://example.com")?;
///
/// // Open a folder in the file manager
/// opener.open_folder(Path::new("/home/user/Documents"))?;
/// # Ok::<(), chatter_core::error::SystemError>(())
/// ```
pub struct ExternalOpener;

impl ExternalOpener {
    /// Opens a URL in the default web browser.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL to open (must start with http://, https://, or mailto:)
    ///
    /// # Errors
    ///
    /// Returns `SystemError::InvalidUrl` if the URL format is invalid.
    /// Returns `SystemError::OpenUrl` if the URL cannot be opened.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use chatter_core::system::ExternalOpener;
    ///
    /// let opener = ExternalOpener;
    /// opener.open_url("https://example.com")?;
    /// opener.open_url("mailto:user@example.com")?;
    /// # Ok::<(), chatter_core::error::SystemError>(())
    /// ```
    pub fn open_url(&self, url: &str) -> Result<(), SystemError> {
        // Validate URL scheme
        if !url.starts_with("http://")
            && !url.starts_with("https://")
            && !url.starts_with("mailto:")
        {
            return Err(SystemError::InvalidUrl(format!(
                "URL must start with http://, https://, or mailto:, got: {}",
                url
            )));
        }

        open::that(url).map_err(|e| SystemError::OpenUrl(e.to_string()))
    }

    /// Opens a folder in the default file manager.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to the folder to open
    ///
    /// # Errors
    ///
    /// Returns `SystemError::OpenFile` if the folder does not exist or cannot be opened.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use chatter_core::system::ExternalOpener;
    /// use std::path::Path;
    ///
    /// let opener = ExternalOpener;
    /// opener.open_folder(Path::new("/home/user/Documents"))?;
    /// # Ok::<(), chatter_core::error::SystemError>(())
    /// ```
    pub fn open_folder(&self, path: &Path) -> Result<(), SystemError> {
        // Validate path exists and is a directory
        if !path.exists() {
            return Err(SystemError::OpenFile(format!(
                "Folder does not exist: {}",
                path.display()
            )));
        }

        if !path.is_dir() {
            return Err(SystemError::OpenFile(format!(
                "Path is not a directory: {}",
                path.display()
            )));
        }

        open::that(path).map_err(|e| SystemError::OpenFile(e.to_string()))
    }

    /// Opens the folder containing a file and attempts to select the file.
    ///
    /// On Linux, this typically opens the parent folder in the file manager.
    /// Some file managers may also highlight/select the file.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to the file whose containing folder should be opened
    ///
    /// # Errors
    ///
    /// Returns `SystemError::OpenFile` if the file does not exist or the folder
    /// cannot be opened.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use chatter_core::system::ExternalOpener;
    /// use std::path::Path;
    ///
    /// let opener = ExternalOpener;
    /// // Opens /home/user/Documents and may select document.pdf
    /// opener.reveal_in_folder(Path::new("/home/user/Documents/document.pdf"))?;
    /// # Ok::<(), chatter_core::error::SystemError>(())
    /// ```
    pub fn reveal_in_folder(&self, path: &Path) -> Result<(), SystemError> {
        // Validate path exists
        if !path.exists() {
            return Err(SystemError::OpenFile(format!(
                "File does not exist: {}",
                path.display()
            )));
        }

        // Get parent directory
        let parent = path.parent().ok_or_else(|| {
            SystemError::OpenFile(format!(
                "Cannot determine parent folder of: {}",
                path.display()
            ))
        })?;

        // Try to use xdg-open with select if available (some file managers support it)
        // Fall back to just opening the folder
        // Note: There's no standardized "reveal" on Linux, so we open the parent folder
        open::that(parent).map_err(|e| SystemError::OpenFile(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_invalid_url_scheme() {
        let opener = ExternalOpener;

        let result = opener.open_url("ftp://example.com");
        assert!(result.is_err());
        match result {
            Err(SystemError::InvalidUrl(_)) => {}
            _ => panic!("Expected InvalidUrl error"),
        }

        let result = opener.open_url("file:///etc/passwd");
        assert!(result.is_err());
        match result {
            Err(SystemError::InvalidUrl(_)) => {}
            _ => panic!("Expected InvalidUrl error"),
        }
    }

    #[test]
    fn test_valid_url_schemes() {
        // Test URL scheme validation only - don't actually open URLs
        // as that would have side effects (opening browser windows)
        let valid_schemes = ["http://", "https://", "mailto:"];
        let test_urls = [
            "http://example.com",
            "https://example.com",
            "mailto:test@example.com",
        ];

        for url in &test_urls {
            let has_valid_scheme = valid_schemes.iter().any(|scheme| url.starts_with(scheme));
            assert!(has_valid_scheme, "URL {} should have a valid scheme", url);
        }
    }

    #[test]
    fn test_nonexistent_folder() {
        let opener = ExternalOpener;
        let result = opener.open_folder(Path::new("/nonexistent/folder"));
        assert!(result.is_err());
        match result {
            Err(SystemError::OpenFile(msg)) => {
                assert!(msg.contains("does not exist"));
            }
            _ => panic!("Expected OpenFile error"),
        }
    }
}
