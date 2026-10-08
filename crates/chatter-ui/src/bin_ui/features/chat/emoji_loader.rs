//! Emoji picker callbacks for the composer and reactions. The table, search,
//! skin tones and recents are core's (`chatter_core::ui::emoji_data`, and
//! `ChatterClient::recent_emoji` for the signed-in account's history).

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::ui::emoji_data::{self, PEOPLE_CATEGORY, PickerEmoji, RECENT_CATEGORY};

use crate::{EmojiData, MainWindow};

fn model(emojis: Vec<PickerEmoji>) -> ModelRc<EmojiData> {
    ModelRc::new(VecModel::from(
        emojis
            .into_iter()
            .map(|e| EmojiData {
                emoji: e.emoji.into(),
                name: e.name.into(),
                category: e.category,
            })
            .collect::<Vec<_>>(),
    ))
}

/// A fresh reaction picker: Recent when the account has picked anything,
/// otherwise Smileys, with the search cleared.
pub fn show_reaction_picker_start(w: &MainWindow, ui_ctx: &ChatterClient) {
    let (category, emojis) = emoji_data::initial_listing(&ui_ctx.recent_emoji(), 0);
    w.set_reaction_picker_selected_category(category);
    w.set_reaction_picker_search_query("".into());
    w.set_reaction_picker_emojis(model(emojis));
}

/// A fresh composer drawer (closed → open): Recent when the account has
/// picked anything, otherwise Smileys, with the search cleared. Switching
/// tabs inside the open drawer does NOT come through here; each mode keeps
/// its query and category until the drawer closes.
pub fn show_composer_picker_start(w: &MainWindow, ui_ctx: &ChatterClient) {
    let tone = w.get_composer_emoji_skin_tone();
    let (category, emojis) = emoji_data::initial_listing(&ui_ctx.recent_emoji(), tone);
    w.set_composer_emoji_category(category);
    w.set_composer_emoji_search("".into());
    w.set_composer_emoji_list(model(emojis));
}

/// Wire emoji loader callbacks.
pub fn wire_emoji_loader(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();

    // Initial load — show smileys (category 0) by default
    if let Some(w) = weak.upgrade() {
        w.set_composer_emoji_list(model(emoji_data::category_emojis(0)));
        w.set_reaction_picker_emojis(model(emoji_data::category_emojis(0)));
    }

    window.on_composer_emoji_category_changed({
        let weak = weak.clone();
        let ui_ctx = ui_ctx.clone();
        move |category| {
            if let Some(w) = weak.upgrade() {
                let tone = w.get_composer_emoji_skin_tone();
                w.set_composer_emoji_list(model(emoji_data::listing(
                    category,
                    tone,
                    &ui_ctx.recent_emoji(),
                )));
            }
        }
    });

    // Clearing the search returns to the selected tab WITH the selected tone:
    // reloading at tone 0 left the swatch row marking a tone the grid no
    // longer showed.
    window.on_composer_emoji_search_changed({
        let weak = weak.clone();
        let ui_ctx = ui_ctx.clone();
        move |query| {
            if let Some(w) = weak.upgrade() {
                let emojis = if query.is_empty() {
                    emoji_data::listing(
                        w.get_composer_emoji_category(),
                        w.get_composer_emoji_skin_tone(),
                        &ui_ctx.recent_emoji(),
                    )
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
        let ui_ctx = ui_ctx.clone();
        move |category| {
            if let Some(w) = weak.upgrade() {
                w.set_reaction_picker_emojis(model(emoji_data::listing(
                    category,
                    0,
                    &ui_ctx.recent_emoji(),
                )));
            }
        }
    });

    window.on_reaction_picker_search_changed({
        let weak = weak.clone();
        let ui_ctx = ui_ctx.clone();
        move |query| {
            if let Some(w) = weak.upgrade() {
                let emojis = if query.is_empty() {
                    emoji_data::listing(
                        w.get_reaction_picker_selected_category(),
                        0,
                        &ui_ctx.recent_emoji(),
                    )
                } else {
                    emoji_data::search(&query)
                };
                w.set_reaction_picker_emojis(model(emojis));
            }
        }
    });

    // Record the pick for the signed-in account and insert at the saved
    // composer caret/selection.
    window.on_composer_emoji_selected({
        let weak = weak.clone();
        let ui_ctx = ui_ctx.clone();
        move |emoji| {
            // The pick belongs to whoever is signed in at this moment.
            if let Some(scope) = ui_ctx.picker_history_scope() {
                ui_ctx.record_recent_emoji(scope, &emoji);
            }

            if let Some(w) = weak.upgrade() {
                w.invoke_insert_composer_emoji(emoji);
                // The Recent tab reorders in place, so a repeat pick needs no
                // tab round trip to show up first.
                if w.get_composer_emoji_category() == RECENT_CATEGORY
                    && w.get_composer_emoji_search().is_empty()
                {
                    w.set_composer_emoji_list(model(emoji_data::listing(
                        RECENT_CATEGORY,
                        0,
                        &ui_ctx.recent_emoji(),
                    )));
                }
                // Deliberately left OPEN. Picking an emoji for the composer is
                // usually not the last thing you do — closing after each one
                // made "😀😀😀" four taps (§112u). Back, or the emoji button,
                // closes it. The REACTION picker still closes on pick.
            }
        }
    });

    // Skin tone changed — reload People category with selected tone
    window.on_composer_emoji_skin_tone_changed({
        let ui_ctx = ui_ctx.clone();
        move |tone| {
            if let Some(w) = weak.upgrade() {
                let category = w.get_composer_emoji_category();
                if category == PEOPLE_CATEGORY && w.get_composer_emoji_search().is_empty() {
                    w.set_composer_emoji_list(model(emoji_data::listing(
                        category,
                        tone,
                        &ui_ctx.recent_emoji(),
                    )));
                }
            }
        }
    });
}
