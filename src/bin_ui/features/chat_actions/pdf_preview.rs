//! PDF preview callbacks.
//!
//! Handles PDF file viewing and export.

use chatter_linux::media::{FilePicker, FilePickerConfig};
use chatter_linux::system::ExternalOpener;

use crate::bin_ui::preview_state;
use crate::MainWindow;

/// Wire PDF preview callbacks.
pub fn wire_pdf_preview_callbacks(window: &MainWindow) {
    // Close PDF preview
    window.on_close_pdf_preview(|| {
        preview_state::clear_pdf_preview();
        tracing::debug!("PDF preview closed");
    });

    // Open PDF in external viewer - uses Save As for secure export
    window.on_open_pdf_external(|| {
        if let Some(content) = preview_state::get_pdf_preview_content() {
            // Show Save As dialog then open with system handler
            let filename = content.filename.clone();
            let data = content.data.clone();
            tokio::spawn(async move {
                let config = FilePickerConfig::new()
                    .with_title("Save PDF File")
                    .with_default_path(
                        dirs::download_dir()
                            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
                            .join(&filename),
                    );

                if let Some(destination) = FilePicker::save_file(config).await {
                    match std::fs::write(&destination, &data) {
                        Ok(()) => {
                            tracing::info!("Exported PDF to {}", destination.display());
                            let opener = ExternalOpener;
                            if let Err(e) = opener.open_file(&destination) {
                                tracing::warn!("Could not open exported file: {}", e);
                            }
                        }
                        Err(e) => tracing::error!("Failed to save PDF file: {}", e),
                    }
                }
            });
        } else {
            tracing::warn!("No PDF content available for external open");
        }
    });
}
