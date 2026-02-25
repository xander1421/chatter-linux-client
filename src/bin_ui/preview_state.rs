//! Global file preview state.
//!
//! Tracks the currently previewed file content for "open in external editor" functionality.
//! Stores decrypted content in memory - only written to disk on explicit user export.
//! Content is zeroized when cleared for security.

use std::sync::{Mutex, OnceLock};
use zeroize::Zeroize;

/// Global state tracking the current preview content.
static PREVIEW_STATE: OnceLock<Mutex<PreviewState>> = OnceLock::new();

/// File preview state - stores content in memory for secure handling.
#[derive(Clone, Debug, Default)]
pub struct PreviewState {
    /// Decrypted content of the currently previewed code file, if any.
    pub code_content: Option<PreviewContent>,
    /// Decrypted content of the currently previewed PDF file, if any.
    pub pdf_content: Option<PreviewContent>,
}

/// Content for a previewed file.
#[derive(Clone, Debug)]
pub struct PreviewContent {
    /// The filename (for Save As dialog default).
    pub filename: String,
    /// The decrypted file content.
    pub data: Vec<u8>,
}

/// Get or initialize the global preview state.
fn get_state() -> &'static Mutex<PreviewState> {
    PREVIEW_STATE.get_or_init(|| Mutex::new(PreviewState::default()))
}

/// Set the current code preview content.
pub fn set_code_preview_content(filename: String, data: Vec<u8>) {
    if let Ok(mut state) = get_state().lock() {
        state.code_content = Some(PreviewContent { filename, data });
    }
}

/// Get the current code preview content.
pub fn get_code_preview_content() -> Option<PreviewContent> {
    get_state().lock().ok()?.code_content.clone()
}

/// Clear the code preview content when closing.
///
/// Zeroizes the decrypted content before releasing memory.
pub fn clear_code_preview() {
    if let Ok(mut state) = get_state().lock() {
        if let Some(ref mut content) = state.code_content {
            content.data.zeroize();
        }
        state.code_content = None;
    }
}

/// Set the current PDF preview content.
pub fn set_pdf_preview_content(filename: String, data: Vec<u8>) {
    if let Ok(mut state) = get_state().lock() {
        state.pdf_content = Some(PreviewContent { filename, data });
    }
}

/// Get the current PDF preview content.
pub fn get_pdf_preview_content() -> Option<PreviewContent> {
    get_state().lock().ok()?.pdf_content.clone()
}

/// Clear the PDF preview content when closing.
///
/// Zeroizes the decrypted content before releasing memory.
pub fn clear_pdf_preview() {
    if let Ok(mut state) = get_state().lock() {
        if let Some(ref mut content) = state.pdf_content {
            content.data.zeroize();
        }
        state.pdf_content = None;
    }
}
