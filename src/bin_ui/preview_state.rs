//! Global file preview state.
//!
//! Tracks the currently previewed file path for "open in external editor" functionality.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Global state tracking the current preview file path.
static PREVIEW_STATE: OnceLock<Mutex<PreviewState>> = OnceLock::new();

/// File preview state.
#[derive(Clone, Debug, Default)]
pub struct PreviewState {
    /// Path to the currently previewed code file, if any.
    pub code_file_path: Option<PathBuf>,
    /// Path to the currently previewed PDF file, if any.
    pub pdf_file_path: Option<PathBuf>,
}

/// Get or initialize the global preview state.
fn get_state() -> &'static Mutex<PreviewState> {
    PREVIEW_STATE.get_or_init(|| Mutex::new(PreviewState::default()))
}

/// Set the current code preview file path.
pub fn set_code_preview_path(path: PathBuf) {
    if let Ok(mut state) = get_state().lock() {
        state.code_file_path = Some(path);
    }
}

/// Get the current code preview file path.
pub fn get_code_preview_path() -> Option<PathBuf> {
    get_state().lock().ok()?.code_file_path.clone()
}

/// Clear the code preview path when closing.
pub fn clear_code_preview_path() {
    if let Ok(mut state) = get_state().lock() {
        state.code_file_path = None;
    }
}

/// Set the current PDF preview file path.
pub fn set_pdf_preview_path(path: PathBuf) {
    if let Ok(mut state) = get_state().lock() {
        state.pdf_file_path = Some(path);
    }
}

/// Get the current PDF preview file path.
pub fn get_pdf_preview_path() -> Option<PathBuf> {
    get_state().lock().ok()?.pdf_file_path.clone()
}

/// Clear the PDF preview path when closing.
pub fn clear_pdf_preview_path() {
    if let Ok(mut state) = get_state().lock() {
        state.pdf_file_path = None;
    }
}
