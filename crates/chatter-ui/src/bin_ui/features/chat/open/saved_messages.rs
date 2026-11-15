//! Opening Saved Messages from the sidebar.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::ChannelId;

use crate::MainWindow;

use super::channel::load_channel_into_ui_owned;
use super::stop_audio_playback;

pub(crate) fn wire_saved_messages(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx_clone = ui_ctx.clone();

    window
        .global::<crate::SidebarPane>()
        .on_saved_messages_clicked(move || {
            let weak = weak_window.clone();
            super::show_chat_pane(&weak);
            let ui_ctx = ui_ctx_clone.clone();

            // Stop any playing audio when switching channels
            stop_audio_playback();

            // Clear inline GIF animations to free memory
            crate::bin_ui::gif_state::clear_inline_gifs();

            // Saved Messages is the channel whose id is the user's own id.
            let channel_id = match ui_ctx.auth_snapshot().current_user_id {
                Some(id) => ChannelId::from(id),
                None => {
                    tracing::warn!("Cannot open saved messages: not logged in");
                    return;
                }
            };

            // Commit active-conversation state SYNCHRONOUSLY on the UI thread
            // (same race fix as dm.rs — see comment there).
            ui_ctx.select_channel(channel_id);
            if let Some(w) = weak.upgrade() {
                w.invoke_refresh_screen_protection();
            }

            if let Some(w) = weak.upgrade() {
                crate::bin_ui::features::composer::sync_composer_draft(
                    &w,
                    &ui_ctx,
                    ui_ctx.chat_snapshot().attachment_revision,
                );
            }

            let owner = crate::bin_ui::features::history_owner::HistoryOwner::begin(
                &ui_ctx,
                chatter_core::state::ActiveConversation::Channel(channel_id),
            );
            crate::run_history_ui!(weak, owner, |w| {
                w.global::<crate::ChatPane>().set_loading_messages(true);

                // Clear pinned bar immediately
                w.global::<crate::ChatPane>().set_has_pinned_messages(false);
                w.global::<crate::ChatPane>()
                    .set_pinned_message_preview("".into());
                w.global::<crate::ChatPane>().set_pinned_total_count(0);
            });

            tokio::spawn(async move {
                if !owner.is_current() {
                    return;
                }
                // Active conversation already committed pre-spawn.
                let ui_ctx_for_sel = ui_ctx.clone();
                crate::run_history_ui!(weak, owner, |w| crate::bin_ui::features::sidebar::refresh_selection(
                    &w,
                    &ui_ctx_for_sel
                ));

                // Ensure the saved messages channel exists locally
                if ui_ctx.get_channel_local(&channel_id).is_none() {
                    if let Err(e) = ui_ctx.get_channel(&channel_id).await {
                        tracing::error!(
                            "Failed to fetch saved messages channel from server: {}",
                            e
                        );
                        crate::run_history_ui!(weak, owner, |w| {
                            w.global::<crate::ChatPane>().set_loading_messages(false);
                            w.set_toast_message(
                                chatter_i18n::tr(
                                    "Couldn't open Saved messages. Check your connection and try again.",
                                )
                                .into(),
                            );
                            w.set_toast_variant("error".into());
                            w.set_toast_visible(true);
                        });
                        return;
                    }
                }

                // Load the saved messages channel and its messages
                // Saved Messages is your own notepad: nothing in it is ever
                // "unread", so it always opens at the newest entry.
                load_channel_into_ui_owned(
                    weak,
                    channel_id,
                    ui_ctx,
                    Default::default(),
                    owner,
                    None,
                )
                .await;
            });
        });
}
