//! Mentions autocomplete for the composer.

use std::sync::Arc;

use slint::{ComponentHandle, SharedString};

use chatter_core::client::ChatterClient;
use chatter_core::state::ChatState;

use crate::{MainWindow, MentionSuggestion};

pub(crate) fn wire_mention_triggered(
    window: &MainWindow,
    ui_ctx: Arc<ChatterClient>,
    chat_state: ChatState,
) {
    let weak_window = window.as_weak();

    window.on_mention_triggered(move |query: SharedString| {
        let query_str = query.to_string();
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        let ui_ctx = ui_ctx.clone();

        if query_str.is_empty() {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_mention_autocomplete_visible(false);
                }
            });
            return;
        }

        tracing::debug!("Mention triggered with query: {}", query_str);

        // Spawn async task to fetch channel members
        tokio::spawn(async move {
            // Fetch raw suggestion data (Send-safe)
            let raw_suggestions = fetch_mention_suggestions_raw(
                &query_str,
                &ui_ctx,
                &chat_state,
            ).await;

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    if raw_suggestions.is_empty() {
                        w.set_mention_autocomplete_visible(false);
                    } else {
                        let suggestions: Vec<MentionSuggestion> = raw_suggestions
                            .into_iter()
                            .map(|(id, name, mention_type, has_avatar)| MentionSuggestion {
                                id: id.into(),
                                name: name.into(),
                                mention_type: mention_type.into(),
                                avatar: slint::Image::default(),
                                has_avatar,
                            })
                            .collect();

                        w.set_mention_suggestions(slint::ModelRc::new(slint::VecModel::from(suggestions)));
                        w.set_mention_selected_index(0);
                        w.set_mention_autocomplete_visible(true);
                        w.set_composer_emoji_picker_visible(false);
                        w.set_composer_gif_picker_visible(false);
                    }
                }
            });
        });
    });
}

pub(crate) fn wire_mention_selected(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_mention_suggestion_selected(move |_id: SharedString, display: SharedString| {
        let weak = weak_window.clone();
        let display_str = display.to_string();
        tracing::debug!("Mention selected: {}", display_str);

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let current_text = w.get_composer_text().to_string();
                if let Some(at_pos) = current_text.rfind('@') {
                    let new_text = format!("{}{} ", &current_text[..at_pos], display_str);
                    w.set_composer_text(new_text.into());
                }
                w.set_mention_autocomplete_visible(false);
            }
        });
    });
}

type RawMentionSuggestion = (String, String, String, bool);

async fn fetch_mention_suggestions_raw(
    query: &str,
    ui_ctx: &ChatterClient,
    chat_state: &ChatState,
) -> Vec<RawMentionSuggestion> {
    let query_lower = query.to_lowercase();
    let mut suggestions: Vec<RawMentionSuggestion> = Vec::new();

    // Add special mentions first
    if "here".starts_with(&query_lower) {
        suggestions.push(("here".to_string(), "@here".to_string(), "here".to_string(), false));
    }
    if "all".starts_with(&query_lower) || "everyone".starts_with(&query_lower) {
        suggestions.push(("all".to_string(), "@all".to_string(), "all".to_string(), false));
    }

    let channel_id = match chat_state.get().active_channel_id.clone() {
        Some(id) => id,
        None => {
            tracing::debug!("No active channel for mention search");
            return suggestions;
        }
    };

    match ui_ctx.get_channel_members(&channel_id, 50, 0).await {
        Ok(members) => {
            for member in &members[..std::cmp::min(20, members.len())] {
                match ui_ctx.get_user_profile(&member.user_id).await {
                    Ok(profile) => {
                        let display_name = if profile.display_name.is_empty() {
                            member.user_id.to_string()
                        } else {
                            profile.display_name.clone()
                        };

                        let name_lower = display_name.to_lowercase();
                        let user_id_str = member.user_id.to_string();

                        if name_lower.contains(&query_lower)
                            || user_id_str.to_lowercase().contains(&query_lower)
                        {
                            suggestions.push((
                                user_id_str,
                                format!("@{}", display_name),
                                "user".to_string(),
                                profile.avatar_url.is_some(),
                            ));
                        }
                    }
                    Err(e) => {
                        tracing::debug!(
                            "Failed to fetch profile for user {}: {}",
                            member.user_id,
                            e
                        );
                    }
                }

                if suggestions.len() >= 8 {
                    break;
                }
            }
        }
        Err(e) => {
            tracing::warn!("Failed to fetch channel members for mentions: {}", e);
        }
    }

    suggestions.truncate(8);
    suggestions
}
