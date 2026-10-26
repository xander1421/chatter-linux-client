//! Mentions autocomplete for the composer.

use std::sync::Arc;

use slint::{ComponentHandle, SharedString};

use chatter_core::client::ChatterClient;

use crate::{MainWindow, MentionSuggestion};

pub(crate) fn wire_mention_triggered(window: &MainWindow, ui_ctx: Arc<ChatterClient>) {
    let weak_window = window.as_weak();

    window
        .global::<crate::ChatPane>()
        .on_mention_triggered(move |query: SharedString| {
            let query_str = query.to_string();
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx.clone();

            if query_str.is_empty() {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.global::<crate::ChatPane>()
                            .set_mention_autocomplete_visible(false);
                    }
                });
                return;
            }

            tracing::debug!("Mention triggered with query: {}", query_str);

            // Spawn async task to fetch enriched suggestions from core.
            tokio::spawn(async move {
                let channel_id = match ui_ctx.chat_snapshot().active_channel_id {
                    Some(id) => id,
                    None => return,
                };
                let suggestions = ui_ctx.search_mention_suggestions(&channel_id, &query_str).await;

                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        if suggestions.is_empty() {
                            w.global::<crate::ChatPane>()
                                .set_mention_autocomplete_visible(false);
                        } else {
                            let suggestions: Vec<MentionSuggestion> = suggestions
                                .into_iter()
                                .map(|suggestion| MentionSuggestion {
                                    id: suggestion.id.into(),
                                    name: suggestion.name.into(),
                                    mention_type: suggestion.kind.as_str().into(),
                                    avatar: suggestion.avatar_bytes.as_deref()
                                        .and_then(crate::bin_ui::converters::avatar_bytes_to_image)
                                        .unwrap_or_default(),
                                })
                                .collect();

                            w.global::<crate::ChatPane>().set_mention_suggestions(
                                slint::ModelRc::new(slint::VecModel::from(suggestions)),
                            );
                            w.global::<crate::ChatPane>().set_mention_selected_index(0);
                            w.global::<crate::ChatPane>()
                                .set_mention_autocomplete_visible(true);
                            w.global::<crate::ChatPane>()
                                .set_composer_emoji_picker_visible(false);
                            w.global::<crate::ChatPane>()
                                .set_composer_gif_picker_visible(false);
                        }
                    }
                });
            });
        });
}

pub(crate) fn wire_mention_selected(window: &MainWindow) {
    let weak_window = window.as_weak();

    window
        .global::<crate::ChatPane>()
        .on_mention_suggestion_selected(move |_id: SharedString, display: SharedString| {
            let weak = weak_window.clone();
            let display_str = display.to_string();
            tracing::debug!("Mention selected: {}", display_str);

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    let current_text = w
                        .global::<crate::ChatPane>()
                        .get_composer_text()
                        .to_string();
                    if let Some(at_pos) = current_text.rfind('@') {
                        let new_text = format!("{}{} ", &current_text[..at_pos], display_str);
                        let pane = w.global::<crate::ChatPane>();
                        pane.set_composer_text(new_text.into());
                        pane.set_composer_caret_end_seq(pane.get_composer_caret_end_seq() + 1);
                    }
                    w.global::<crate::ChatPane>()
                        .set_mention_autocomplete_visible(false);
                }
            });
        });
}

