//! Code preview callbacks.
//!
//! Plain-text code viewing. Syntax-highlighted rendering (previously via
//! `slint-code-view`) has been removed; only copy/export actions remain.

use slint::ComponentHandle;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
use chatter_core::media::{FilePicker, FilePickerConfig};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use chatter_core::system::ExternalOpener;

use crate::MainWindow;
use crate::bin_ui::preview_state;

/// Wire code preview callbacks.
pub fn wire_code_preview_callbacks(window: &MainWindow) {
    // Close code preview
    window.on_close_code_preview(|| {
        super::media_viewer::invalidate();
        preview_state::clear_code_preview();
        tracing::debug!("Code preview closed");
    });

    // Copy all code to clipboard
    let weak_window = window.as_weak();
    window.on_copy_code(move || {
        if let Some(w) = weak_window.upgrade() {
            let content = w.get_code_preview_plain_text().to_string();
            let line_count = w.get_code_preview_line_count();
            #[cfg(target_os = "android")]
            {
                let result = crate::platform::android::clipboard::set_text("Code", &content);
                let (message, variant) = match result {
                    Ok(()) => (format!("Copied {line_count} lines"), "success"),
                    Err(error) => {
                        tracing::error!(%error, "Code copy failed");
                        ("Unable to copy code".to_string(), "error")
                    }
                };
                w.set_toast_message(message.into());
                w.set_toast_variant(variant.into());
                w.set_toast_visible(true);
            }
            #[cfg(target_os = "ios")]
            let _ = (content, line_count);
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
    let weak = window.as_weak();
    window.on_open_code_external(move || {
        #[cfg(target_os = "android")]
        if let Some(mut content) = preview_state::get_code_preview_content() {
            let weak = weak.clone();
            let request = super::media_viewer::current_request();
            std::thread::spawn(move || {
                let result = crate::platform::android::media_export::prepare_document(
                    &content.filename,
                    &content.data,
                    "text/plain",
                );
                zeroize::Zeroize::zeroize(&mut content.data);
                let _ = weak.upgrade_in_event_loop(move |w| {
                    if !super::media_viewer::is_current(request) || !w.get_code_preview_visible() {
                        return;
                    }
                    if result.and_then(|session| session.finish()).is_err() {
                        w.set_toast_message(
                            "Unable to open this file. Install a text viewer and try again.".into(),
                        );
                        w.set_toast_variant("error".into());
                        w.set_toast_visible(true);
                    }
                });
            });
        } else if let Some(w) = weak.upgrade() {
            w.set_toast_message("Reopen the attachment to view it externally.".into());
            w.set_toast_variant("error".into());
            w.set_toast_visible(true);
        }
        #[cfg(not(target_os = "android"))]
        let _ = &weak;
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        if let Some(content) = preview_state::get_code_preview_content() {
            let filename = super::media_viewer::export_basename(&content.filename);
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
                    match super::media_viewer::save_export(&destination, &data) {
                        Ok(destination) => {
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
        #[cfg(target_os = "ios")]
        tracing::warn!("Open external not supported on mobile");
    });
}
