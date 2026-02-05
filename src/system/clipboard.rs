//! Clipboard operations.
//!
//! System clipboard integration using arboard with native Wayland support.
//! Falls back to X11 automatically when Wayland is unavailable.

use crate::error::SystemError;

/// Image data for clipboard operations.
///
/// Represents RGBA pixel data that can be copied to/from the clipboard.
#[derive(Debug, Clone)]
pub struct ImageData {
    /// Image width in pixels.
    pub width: usize,
    /// Image height in pixels.
    pub height: usize,
    /// RGBA pixel data (4 bytes per pixel).
    pub bytes: Vec<u8>,
}

impl ImageData {
    /// Creates new image data.
    ///
    /// # Arguments
    ///
    /// * `width` - Image width in pixels
    /// * `height` - Image height in pixels
    /// * `bytes` - RGBA pixel data (must be width * height * 4 bytes)
    pub fn new(width: usize, height: usize, bytes: Vec<u8>) -> Self {
        Self {
            width,
            height,
            bytes,
        }
    }
}

/// Clipboard manager for text and image operations.
///
/// Wraps arboard::Clipboard with Wayland support via wayland-data-control.
/// Falls back to X11 when Wayland is unavailable.
///
/// # Note
///
/// On Linux, clipboard contents are "hosted" by the application that set them.
/// When the Clipboard instance is dropped, contents may become unavailable
/// to other applications. Create instances as needed rather than holding
/// long-lived references.
///
/// # Example
///
/// ```no_run
/// use chatter_linux::system::Clipboard;
///
/// let mut clipboard = Clipboard::new()?;
/// clipboard.set_text("Hello, world!")?;
/// let text = clipboard.get_text()?;
/// # Ok::<(), chatter_linux::error::SystemError>(())
/// ```
pub struct Clipboard {
    inner: arboard::Clipboard,
}

impl Clipboard {
    /// Creates a new clipboard instance.
    ///
    /// # Errors
    ///
    /// Returns `SystemError::ClipboardInit` if clipboard initialization fails.
    /// This can happen if no display server (Wayland or X11) is available.
    pub fn new() -> Result<Self, SystemError> {
        let inner = arboard::Clipboard::new()
            .map_err(|e| SystemError::ClipboardInit(e.to_string()))?;
        Ok(Self { inner })
    }

    /// Reads text from the clipboard.
    ///
    /// # Errors
    ///
    /// Returns `SystemError::ClipboardRead` if the clipboard cannot be read
    /// or does not contain text.
    pub fn get_text(&mut self) -> Result<String, SystemError> {
        self.inner
            .get_text()
            .map_err(|e| SystemError::ClipboardRead(e.to_string()))
    }

    /// Writes text to the clipboard.
    ///
    /// # Arguments
    ///
    /// * `text` - The text to write to the clipboard
    ///
    /// # Errors
    ///
    /// Returns `SystemError::ClipboardWrite` if the clipboard cannot be written to.
    pub fn set_text(&mut self, text: &str) -> Result<(), SystemError> {
        self.inner
            .set_text(text)
            .map_err(|e| SystemError::ClipboardWrite(e.to_string()))
    }

    /// Reads an image from the clipboard.
    ///
    /// # Errors
    ///
    /// Returns `SystemError::ClipboardRead` if the clipboard cannot be read
    /// or does not contain an image.
    pub fn get_image(&mut self) -> Result<ImageData, SystemError> {
        let image = self
            .inner
            .get_image()
            .map_err(|e| SystemError::ClipboardRead(e.to_string()))?;

        Ok(ImageData {
            width: image.width,
            height: image.height,
            bytes: image.bytes.into_owned(),
        })
    }

    /// Writes an image to the clipboard.
    ///
    /// # Arguments
    ///
    /// * `data` - The image data to write (RGBA format)
    ///
    /// # Errors
    ///
    /// Returns `SystemError::ClipboardWrite` if the clipboard cannot be written to.
    pub fn set_image(&mut self, data: &ImageData) -> Result<(), SystemError> {
        let image = arboard::ImageData {
            width: data.width,
            height: data.height,
            bytes: std::borrow::Cow::Borrowed(&data.bytes),
        };

        self.inner
            .set_image(image)
            .map_err(|e| SystemError::ClipboardWrite(e.to_string()))
    }

    /// Clears the clipboard contents.
    ///
    /// # Errors
    ///
    /// Returns `SystemError::ClipboardWrite` if the clipboard cannot be cleared.
    pub fn clear(&mut self) -> Result<(), SystemError> {
        self.inner
            .clear()
            .map_err(|e| SystemError::ClipboardWrite(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore] // Requires display server
    fn test_clipboard_text_roundtrip() {
        let mut clipboard = Clipboard::new().expect("Should create clipboard");
        let test_text = "Hello from Chatter clipboard test!";

        clipboard.set_text(test_text).expect("Should set text");
        let retrieved = clipboard.get_text().expect("Should get text");

        assert_eq!(retrieved, test_text);
    }

    #[test]
    fn test_image_data_new() {
        let data = ImageData::new(2, 2, vec![0; 16]);
        assert_eq!(data.width, 2);
        assert_eq!(data.height, 2);
        assert_eq!(data.bytes.len(), 16);
    }
}
