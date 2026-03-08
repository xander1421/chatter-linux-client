//! Emoji picker for the composer.

use slint::{ComponentHandle, SharedString};

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

pub(crate) fn wire_emoji_selected(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_composer_emoji_selected(move |emoji: SharedString| {
        let weak = weak_window.clone();
        let emoji_str = emoji.to_string();
        tracing::debug!("Emoji selected: {}", emoji_str);

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                // Insert emoji into composer text
                let current_text = w.get_composer_text().to_string();
                let new_text = format!("{}{}", current_text, emoji_str);
                w.set_composer_text(new_text.into());
                // Close the picker
                w.set_composer_emoji_picker_visible(false);
            }
        });
    });
}

pub(crate) fn wire_emoji_category_changed(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_composer_emoji_category_changed(move |category: i32| {
        let weak = weak_window.clone();
        tracing::debug!("Emoji category changed: {}", category);

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let emojis = emoji_loader::get_emoji_list(category);
                w.set_composer_emoji_list(slint::ModelRc::new(slint::VecModel::from(emojis)));
            }
        });
    });
}

pub(crate) fn wire_emoji_search_changed(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_composer_emoji_search_changed(move |query: SharedString| {
        let weak = weak_window.clone();
        let query_str = query.to_string().to_lowercase();
        tracing::debug!("Emoji search: {}", query_str);

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                if query_str.is_empty() {
                    // Return to category view
                    let category = w.get_composer_emoji_category();
                    let emojis = emoji_loader::get_emoji_list(category);
                    w.set_composer_emoji_list(slint::ModelRc::new(slint::VecModel::from(emojis)));
                } else {
                    // Search all emojis
                    let emojis = emoji_loader::search_emojis(&query_str);
                    w.set_composer_emoji_list(slint::ModelRc::new(slint::VecModel::from(emojis)));
                }
            }
        });
    });
}
