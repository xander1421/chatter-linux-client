//! Emoji picker toggle for the composer.
//!
//! Category changes, search, and selection are handled by `chat::emoji_loader`.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;

use crate::MainWindow;
use crate::bin_ui::features::chat::emoji_loader;

pub(crate) fn wire_emoji_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    // Only the composer's smiley button comes through here: it opens the
    // drawer from closed (fresh picker state, both queries cleared) or closes
    // it. The drawer's own Emoji/GIF tabs switch mode without touching this.
    window.on_emoji_clicked(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        tracing::debug!("Emoji picker clicked");

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                // Visibility is handled by Slint ChatLayout.
                if w.get_composer_emoji_picker_visible() {
                    emoji_loader::show_composer_picker_start(&w, &ui_ctx);
                    w.set_composer_gif_picker_search("".into());
                }
            }
        });
    });
}
