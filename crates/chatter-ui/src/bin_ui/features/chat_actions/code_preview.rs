//! Code preview callbacks.
//!
//! Plain-text code viewing. Syntax-highlighted rendering (previously via
//! `slint-code-view`) has been removed; only copy/export actions remain.

use slint::ComponentHandle;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
use chatter_core::media::{FilePicker, FilePickerConfig};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use chatter_core::system::ExternalOpener;

use crate::bin_ui::preview_state;
use crate::MainWindow;

/// Wire code preview callbacks.
pub fn wire_code_preview_callbacks(window: &MainWindow) {
    // Close code preview
    window.on_close_code_preview(|| {
        preview_state::clear_code_preview();
        tracing::debug!("Code preview closed");
    });

    // Copy all code to clipboard
    let weak_window = window.as_weak();
    window.on_copy_code(move || {
        if let Some(w) = weak_window.upgrade() {
            let content = w.get_code_preview_plain_text().to_string();
            let line_count = w.get_code_preview_line_count();
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
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

    // Open code in external editor - uses Save As for secure export
    window.on_open_code_external(|| {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
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
        #[cfg(any(target_os = "android", target_os = "ios"))]
        tracing::warn!("Open external not supported on mobile");
    });
}
