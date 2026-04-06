//! Emoji picker toggle for the composer.
//!
//! Category changes, search, and selection are handled by `chat::emoji_loader`.

use slint::ComponentHandle;

use crate::bin_ui::features::chat::emoji_loader;
use crate::MainWindow;

pub(crate) fn wire_emoji_clicked(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_emoji_clicked(move || {
        let weak = weak_window.clone();
        tracing::debug!("Emoji picker clicked");

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                // Visibility is handled by Slint ChatLayout.
                // Load initial emoji list if opening
                if w.get_composer_emoji_picker_visible() {
                    let emojis = emoji_loader::get_emoji_list(0); // Category 0 = Smileys
                    w.set_composer_emoji_list(slint::ModelRc::new(slint::VecModel::from(emojis)));
                    w.set_composer_emoji_category(0);
                    w.set_composer_emoji_search("".into());
                }
            }
        });
    });
}
