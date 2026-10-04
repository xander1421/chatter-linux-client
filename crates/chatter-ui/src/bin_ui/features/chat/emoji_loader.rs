//! Emoji picker callbacks for the composer and reactions. The table, search,
//! skin tones and recents are core's (`chatter_core::ui::emoji_data`).

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::ui::emoji_data::{self, PickerEmoji, PEOPLE_CATEGORY};
use chatter_core::ui::recent_emoji::RecentEmojiStore;

use crate::{EmojiData, MainWindow};

fn model(emojis: Vec<PickerEmoji>) -> ModelRc<EmojiData> {
    ModelRc::new(VecModel::from(
        emojis
            .into_iter()
            .map(|e| EmojiData { emoji: e.emoji.into(), name: e.name.into(), category: e.category })
            .collect::<Vec<_>>(),
    ))
}

/// Reset the reaction picker to its first tab (Smileys) as it opens.
pub fn show_reaction_picker_start(w: &MainWindow) {
    w.set_reaction_picker_emojis(model(emoji_data::category_emojis(0)));
}

/// Reset the composer picker to its first tab (Smileys) as it opens.
pub fn show_composer_picker_start(w: &MainWindow) {
    w.set_composer_emoji_list(model(emoji_data::category_emojis(0)));
    w.set_composer_emoji_category(0);
}

/// Wire emoji loader callbacks.
pub fn wire_emoji_loader(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let store = Arc::new(RecentEmojiStore::load(ui_ctx.directories().data_dir()));
    let weak = window.as_weak();

    // Initial load — show smileys (category 0) by default
    if let Some(w) = weak.upgrade() {
        w.set_composer_emoji_list(model(emoji_data::category_emojis(0)));
        w.set_reaction_picker_emojis(model(emoji_data::category_emojis(0)));
    }

    window.on_composer_emoji_category_changed({
        let weak = weak.clone();
        let store = store.clone();
        move |category| {
            if let Some(w) = weak.upgrade() {
                let tone = w.get_composer_emoji_skin_tone();
                w.set_composer_emoji_list(model(emoji_data::listing(category, tone, &store)));
            }
        }
    });

    window.on_composer_emoji_search_changed({
        let weak = weak.clone();
        let store = store.clone();
        move |query| {
            if let Some(w) = weak.upgrade() {
                let emojis = if query.is_empty() {
                    emoji_data::listing(w.get_composer_emoji_category(), 0, &store)
                } else {
                    emoji_data::search(&query)
                };
                w.set_composer_emoji_list(model(emojis));
            }
        }
    });

    // Reaction picker emoji callbacks
    window.on_reaction_picker_category_changed({
        let weak = weak.clone();
        let store = store.clone();
        move |category| {
            if let Some(w) = weak.upgrade() {
                w.set_reaction_picker_emojis(model(emoji_data::listing(category, 0, &store)));
            }
        }
    });

    window.on_reaction_picker_search_changed({
        let weak = weak.clone();
        let store = store.clone();
        move |query| {
            if let Some(w) = weak.upgrade() {
                let emojis = if query.is_empty() {
                    emoji_data::listing(w.get_reaction_picker_selected_category(), 0, &store)
                } else {
                    emoji_data::search(&query)
                };
                w.set_reaction_picker_emojis(model(emojis));
            }
        }
    });

    // Track emoji usage + insert into composer text + close picker
    window.on_composer_emoji_selected({
        let weak = weak.clone();
        let store = store.clone();
        move |emoji| {
            store.record(&emoji);
            let emoji_str = emoji.to_string();
            if let Some(w) = weak.upgrade() {
                let current_text = w.get_composer_text().to_string();
                let new_text = format!("{current_text}{emoji_str}");
                w.set_composer_text(new_text.into());
                // Deliberately left OPEN. Picking an emoji for the composer is
                // usually not the last thing you do — closing after each one
                // made "😀😀😀" four taps (§112u). Back, or the emoji button,
                // closes it. The REACTION picker still closes on pick.
            }
        }
    });

    // Skin tone changed — reload People category with selected tone
    window.on_composer_emoji_skin_tone_changed({
        let weak = weak.clone();
        move |tone| {
            if let Some(w) = weak.upgrade() {
                let category = w.get_composer_emoji_category();
                if category == PEOPLE_CATEGORY {
                    w.set_composer_emoji_list(model(emoji_data::listing(category, tone, &store)));
                }
            }
        }
    });
}

