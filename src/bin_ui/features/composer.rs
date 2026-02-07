//! Composer callbacks.
//!
//! Handles message composer actions (attach, emoji, gif, mentions).

use slint::{ComponentHandle, SharedString};

use chatter_linux::app::AppContext;

use crate::MainWindow;

/// Wire composer callbacks.
pub fn wire(window: &MainWindow, _ctx: &AppContext) {
    wire_attachment_clicked(window);
    wire_emoji_clicked(window);
    wire_gif_clicked(window);
    wire_mention_triggered(window);
}

fn wire_attachment_clicked(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_attachment_clicked(move || {
        let weak = weak_window.clone();
        tracing::debug!("Attachment clicked");

        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create runtime for file picker");

            rt.block_on(async {
                use chatter_linux::media::file_picker::{FileFilter, FilePicker, FilePickerConfig};

                let config = FilePickerConfig::new()
                    .with_title("Select Attachment")
                    .with_filter(FileFilter::images())
                    .with_filter(FileFilter::documents())
                    .with_filter(FileFilter::videos())
                    .with_filter(FileFilter::audio())
                    .with_filter(FileFilter::all())
                    .with_multiple(true);

                let selected_files = FilePicker::pick_files(config).await;

                if selected_files.is_empty() {
                    tracing::debug!("File picker cancelled or no files selected");
                    return;
                }

                tracing::info!("Selected {} file(s) for attachment", selected_files.len());
                for file in &selected_files {
                    tracing::debug!("  - {:?}", file);
                }

                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        let count = selected_files.len();
                        let placeholder = if count == 1 {
                            "1 file attached - type a message...".to_string()
                        } else {
                            format!("{} files attached - type a message...", count)
                        };
                        w.set_composer_placeholder(placeholder.into());
                    }
                });
            });
        });
    });
}

fn wire_emoji_clicked(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_emoji_clicked(move || {
        let weak = weak_window.clone();
        tracing::debug!("Emoji picker clicked");

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                tracing::info!("Emoji picker toggled (UI component not yet implemented)");
            }
        });
    });
}

fn wire_gif_clicked(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_gif_clicked(move || {
        let weak = weak_window.clone();
        tracing::debug!("GIF picker clicked");

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                tracing::info!("GIF picker toggled (UI component not yet implemented)");
            }
        });
    });
}

fn wire_mention_triggered(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_mention_triggered(move |query: SharedString| {
        let query_str = query.to_string();
        let weak = weak_window.clone();

        if query_str.is_empty() {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(_w) = weak.upgrade() {
                    tracing::trace!("Mention autocomplete closed");
                }
            });
            return;
        }

        tracing::debug!("Mention triggered with query: {}", query_str);

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                tracing::info!(
                    "Mention search for '{}' (autocomplete not yet implemented)",
                    query_str
                );
            }
        });
    });
}
