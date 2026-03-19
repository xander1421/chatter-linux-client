//! Code preview callbacks.
//!
//! Handles syntax-highlighted code file viewing with selection support.

use std::cell::RefCell;

use slint::ComponentHandle;

use slint_code_view::CodeViewState;
#[cfg(not(target_os = "android"))]
use chatter_core::media::{FilePicker, FilePickerConfig};
#[cfg(not(target_os = "android"))]
use chatter_core::system::ExternalOpener;

use crate::bin_ui::preview_state;
use crate::MainWindow;

thread_local! {
    /// Thread-local code view state (must be on UI thread).
    pub(super) static CODE_VIEW_STATE: RefCell<Option<CodeViewState>> = const { RefCell::new(None) };
}

/// Update the code preview UI from the current CodeViewState.
pub fn update_code_preview_ui(window: &MainWindow) {
    CODE_VIEW_STATE.with(|state_cell| {
        if let Some(ref mut state) = *state_cell.borrow_mut() {
            if state.needs_render() {
                // Render and update image
                let image = state.render();
                window.set_code_preview_image(image);

                // Update selection state
                let has_selection = state.has_text_selection() || !state.selected_lines().is_empty();
                window.set_code_preview_has_selection(has_selection);

                if let Some(text) = state.selected_text() {
                    window.set_code_preview_selected_text(text.into());
                } else if !state.selected_lines().is_empty() {
                    let text = state.text_for_lines(&state.selected_lines());
                    window.set_code_preview_selected_text(text.into());
                } else {
                    window.set_code_preview_selected_text("".into());
                }
            }
        }
    });
}

/// Set the code view state (called from attachment_view when opening code files).
pub fn set_code_view_state(state: CodeViewState) {
    CODE_VIEW_STATE.with(|state_cell| {
        *state_cell.borrow_mut() = Some(state);
    });
}

/// Wire code preview callbacks.
pub fn wire_code_preview_callbacks(window: &MainWindow) {
    // Close code preview
    window.on_close_code_preview(|| {
        preview_state::clear_code_preview();
        CODE_VIEW_STATE.with(|state_cell| {
            *state_cell.borrow_mut() = None;
        });
        tracing::debug!("Code preview closed");
    });

    // Copy all code to clipboard
    let weak_window = window.as_weak();
    window.on_copy_code(move || {
        if let Some(w) = weak_window.upgrade() {
            let content = w.get_code_preview_plain_text().to_string();
            let line_count = w.get_code_preview_line_count();
            // Move clipboard operation to background thread
            #[cfg(not(target_os = "android"))]
            std::thread::spawn(move || {
                if let Ok(mut clipboard) = arboard::Clipboard::new() {
                    if let Err(e) = clipboard.set_text(&content) {
                        tracing::error!("Failed to copy to clipboard: {}", e);
                    } else {
                        tracing::info!("Copied {} lines to clipboard", line_count);
                    }
                }
            });
        }
    });

    // Copy selection to clipboard
    let weak_window = window.as_weak();
    window.on_copy_code_selection(move || {
        if let Some(w) = weak_window.upgrade() {
            let content = w.get_code_preview_selected_text().to_string();
            if !content.is_empty() {
                // Move clipboard operation to background thread
                #[cfg(not(target_os = "android"))]
                std::thread::spawn(move || {
                    if let Ok(mut clipboard) = arboard::Clipboard::new() {
                        if let Err(e) = clipboard.set_text(&content) {
                            tracing::error!("Failed to copy selection to clipboard: {}", e);
                        } else {
                            tracing::info!("Copied selection to clipboard");
                        }
                    }
                });
            }
        }
    });

    // Pointer events for selection
    let weak_window = window.as_weak();
    window.on_code_preview_pointer_down(move |x, y, shift, ctrl| {
        CODE_VIEW_STATE.with(|state_cell| {
            if let Some(ref mut state) = *state_cell.borrow_mut() {
                state.on_pointer_down(x as f32, y as f32, shift, ctrl);
            }
        });
        if let Some(w) = weak_window.upgrade() {
            update_code_preview_ui(&w);
        }
    });

    let weak_window = window.as_weak();
    window.on_code_preview_pointer_move(move |x, y| {
        CODE_VIEW_STATE.with(|state_cell| {
            if let Some(ref mut state) = *state_cell.borrow_mut() {
                state.on_pointer_move(x as f32, y as f32);
            }
        });
        if let Some(w) = weak_window.upgrade() {
            update_code_preview_ui(&w);
        }
    });

    let weak_window = window.as_weak();
    window.on_code_preview_pointer_up(move |x, y| {
        CODE_VIEW_STATE.with(|state_cell| {
            if let Some(ref mut state) = *state_cell.borrow_mut() {
                state.on_pointer_up(x as f32, y as f32);
            }
        });
        if let Some(w) = weak_window.upgrade() {
            update_code_preview_ui(&w);
        }
    });

    // Open code in external editor - uses Save As for secure export
    window.on_open_code_external(|| {
        #[cfg(not(target_os = "android"))]
        if let Some(content) = preview_state::get_code_preview_content() {
            let filename = content.filename.clone();
            let data = content.data.clone();
            tokio::spawn(async move {
                let config = FilePickerConfig::new()
                    .with_title("Save Code File")
                    .with_default_path(
                        dirs::download_dir()
                            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
                            .join(&filename),
                    );

                if let Some(destination) = FilePicker::save_file(config).await {
                    match std::fs::write(&destination, &data) {
                        Ok(()) => {
                            tracing::info!("Exported code to {}", destination.display());
                            let opener = ExternalOpener;
                            if let Err(e) = opener.open_file(&destination) {
                                tracing::warn!("Could not open exported file: {}", e);
                            }
                        }
                        Err(e) => tracing::error!("Failed to save code file: {}", e),
                    }
                }
            });
        } else {
            tracing::warn!("No code content available for external open");
        }
        #[cfg(target_os = "android")]
        tracing::warn!("Open external not supported on Android");
    });
}
