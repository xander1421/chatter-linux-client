//! PDF viewer.
//!
//! PDF viewing that delegates to system PDF viewers.
//!
//! Rationale: Embedding a full PDF renderer (like pdf-rs or pdfium) adds
//! significant complexity. For v1, we delegate to the system's default PDF
//! viewer (evince, okular, etc.) while providing basic metadata extraction.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::error::MediaError;

/// PDF viewer that delegates to system PDF viewer.
///
/// This is a stateless struct that provides methods for opening PDFs
/// in external applications and extracting basic metadata.
/// Follows the same pattern as VideoPlayer.
pub struct PdfViewer;

/// PDF magic bytes: %PDF
const PDF_MAGIC: &[u8; 4] = b"%PDF";

impl PdfViewer {
    /// Opens a PDF file in the system's default PDF viewer.
    ///
    /// Uses `xdg-open` on Linux to launch the appropriate application.
    /// The call is non-blocking - the PDF viewer runs in a separate process.
    ///
    /// # Errors
    ///
    /// Returns an error if the file doesn't exist or cannot be opened.
    pub fn open(path: &Path) -> Result<(), MediaError> {
        if !path.exists() {
            return Err(MediaError::FileRead(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("PDF file not found: {}", path.display()),
            )));
        }

        // Use open::that_in_background to not block the UI thread
        open::that_in_background(path);

        Ok(())
    }

    /// Opens a PDF file with a specific application.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the PDF file.
    /// * `app` - Application name (e.g., "evince", "okular", "zathura").
    ///
    /// # Errors
    ///
    /// Returns an error if the file doesn't exist or cannot be opened.
    pub fn open_with_app(path: &Path, app: &str) -> Result<(), MediaError> {
        if !path.exists() {
            return Err(MediaError::FileRead(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("PDF file not found: {}", path.display()),
            )));
        }

        open::with_in_background(path, app);

        Ok(())
    }

    /// Checks if a file is a PDF by examining magic bytes.
    ///
    /// PDF files start with "%PDF" (0x25504446).
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file to check.
    ///
    /// # Returns
    ///
    /// `true` if the file starts with PDF magic bytes.
    pub fn is_pdf(path: &Path) -> bool {
        let Ok(mut file) = File::open(path) else {
            return false;
        };

        let mut magic = [0u8; 4];
        if file.read_exact(&mut magic).is_err() {
            return false;
        }

        &magic == PDF_MAGIC
    }

    /// Extracts the page count from a PDF file.
    ///
    /// This uses a minimal parsing approach to avoid needing a full PDF library:
    /// 1. Read the last ~1KB of the file (trailer is at end)
    /// 2. Search for `/Count N` pattern which indicates total pages
    ///
    /// Note: This is a heuristic and may not work for all PDFs (especially
    /// linearized or encrypted ones), but works for most common cases.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or page count cannot be determined.
    pub fn get_page_count(path: &Path) -> Result<usize, MediaError> {
        let mut file = File::open(path)?;

        // First verify it's actually a PDF
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic)?;
        if &magic != PDF_MAGIC {
            return Err(MediaError::PreviewError(
                "not a valid PDF file".to_string(),
            ));
        }

        // Read the last 4KB to find the trailer/catalog
        // Most PDFs have the page count info near the end
        let file_size = file.metadata()?.len();
        let read_size = std::cmp::min(4096, file_size as usize);
        let offset = file_size.saturating_sub(read_size as u64);

        file.seek(SeekFrom::Start(offset))?;
        let mut buffer = vec![0u8; read_size];
        file.read_exact(&mut buffer)?;

        // Convert to string for pattern matching (lossy is fine for PDF ASCII)
        let content = String::from_utf8_lossy(&buffer);

        // Look for /Count pattern which gives total page count
        // Format: /Count 123 or /Count 123/
        Self::extract_count(&content)
            .ok_or_else(|| MediaError::PreviewError("could not determine page count".to_string()))
    }

    /// Extracts page count from PDF content string.
    fn extract_count(content: &str) -> Option<usize> {
        // Look for /Count followed by a number
        // This appears in the Pages dictionary
        let mut search_pos = 0;
        while let Some(pos) = content[search_pos..].find("/Count") {
            let count_start = search_pos + pos + 6; // Skip "/Count"

            // Find the number after /Count
            let remainder = &content[count_start..];
            let trimmed = remainder.trim_start();

            // Extract digits
            let num_str: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();

            if let Ok(count) = num_str.parse::<usize>() {
                if count > 0 {
                    return Some(count);
                }
            }

            search_pos = count_start;
        }

        None
    }
}
