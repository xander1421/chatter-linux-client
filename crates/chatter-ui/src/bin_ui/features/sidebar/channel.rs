//! Channel selection and message loading.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::channel::ChannelType;
use chatter_core::domain::ids::ChannelId;

use crate::bin_ui::converters::messages_to_display_batch_deferred;
use crate::{MainWindow, run_ui};

use super::stop_audio_playback;
use crate::bin_ui::features::history_owner::HistoryOwner;

/// Wire channel selected callback.
pub fn wire_channel_selected(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let refresh_client = ui_ctx.clone();
    let refresh_window = window.as_weak();
    window
        .global::<crate::SidebarPane>()
        .on_refresh_channel_call(move || {
            if let Some(w) = refresh_window.upgrade() {
                if let chatter_core::state::ActiveConversation::Channel(id) =
                    refresh_client.active_conversation()
                {
                    refresh_channel_call(w.as_weak(), refresh_client.clone(), id.to_string());
                }
            }
        });
    let ui_ctx_clone = ui_ctx.clone();
    let weak_window = window.as_weak();

    window
        .global::<crate::SidebarPane>()
        .on_channel_selected(move |channel_id: SharedString| {
            let open_started = std::time::Instant::now();
            crate::trace_chat_open_started(open_started, "channel");
            tracing::info!(
                kind = "channel",
                stage = "tap",
                elapsed_ms = 0,
                "conversation_open"
            );
            if let Ok(id) = channel_id.parse::<ChannelId>() {
                super::super::header::cancel_pending_jump_for(
                    chatter_core::state::ActiveConversation::Channel(id),
                );
            }
            let channel_id_str = channel_id.to_string();
            let weak = weak_window.clone();
            super::show_chat_pane(&weak);
            tracing::info!(
                kind = "channel",
                stage = "slide_requested",
                elapsed_ms = open_started.elapsed().as_millis(),
                "conversation_open"
            );
            let ui_ctx = ui_ctx_clone.clone();

            // Stop any playing audio when switching channels
            stop_audio_playback();

            // Clear inline GIF animations to free memory
            crate::bin_ui::gif_state::clear_inline_gifs();

            // Device-stored assistant history: opening submits no remote prompt — load Jarvis
            // history and stop.
            #[cfg(feature = "jarvis")]
            if crate::bin_ui::features::jarvis::is_jarvis_channel_str(&channel_id_str) {
                if let Ok(parsed) = channel_id_str.parse::<ChannelId>() {
                    ui_ctx.select_channel(parsed);
                }
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
                let ui_ctx_for_sel = ui_ctx.clone();
                run_ui!(weak, |w| {
                    w.global::<crate::ChatPane>().set_loading_messages(true);
                    super::refresh_selection(&w, &ui_ctx_for_sel);
                });
                let weak = weak.clone();
                let ui_ctx = ui_ctx.clone();
                tokio::spawn(async move {
                    crate::bin_ui::features::jarvis::load_jarvis_channel_into_ui(weak, ui_ctx)
                        .await;
                });
                return;
            }

            // Commit active-conversation state SYNCHRONOUSLY on the UI thread
            // (same race fix as dm.rs — see comment there).
            if let Ok(parsed) = channel_id_str.parse::<ChannelId>() {
                ui_ctx.select_channel(parsed);
            }
            if let Some(w) = weak.upgrade() {
                w.global::<crate::CallView>()
                    .set_active_channel_call_id("".into());
                refresh_channel_call(weak.clone(), ui_ctx.clone(), channel_id_str.clone());
            }
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

            let owner = HistoryOwner::begin(&ui_ctx, ui_ctx.active_conversation());
            let channel_id_str_for_ui = channel_id_str.clone();
            let ui_ctx_for_ui = ui_ctx.clone();
            crate::run_history_ui!(weak, owner, |w| {
                w.global::<crate::ChatPane>().set_loading_messages(true);

                // Clear unread count in sidebar
                crate::bin_ui::features::auth::inbox::domain_handler::update_unread_count(
                    &w,
                    &ui_ctx_for_ui,
                    &channel_id_str_for_ui,
                    0,
                );

                // Clear pinned bar immediately while loading new channel's pins
                w.global::<crate::ChatPane>().set_has_pinned_messages(false);
                w.global::<crate::ChatPane>()
                    .set_pinned_message_preview("".into());
                w.global::<crate::ChatPane>().set_pinned_total_count(0);
            });

            tokio::spawn(async move {
                if !owner.is_current() {
                    return;
                }
                // Parse channel ID
                let channel_id: ChannelId = match channel_id_str.parse() {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::error!("Invalid channel ID: {}", e);
                        return;
                    }
                };

                // Active conversation already committed pre-spawn.
                let ui_ctx_for_sel = ui_ctx.clone();
                crate::run_history_ui!(weak, owner, |w| super::refresh_selection(
                    &w,
                    &ui_ctx_for_sel
                ));

                // Mark as read
                let handle = ui_ctx
                    .conversation(chatter_core::state::ActiveConversation::Channel(channel_id));
                // Where the user left off. This is the only moment it exists: the
                // call that returns it is the call that clears `is_read`. The
                // receipts go out for the page `load_channel_into_ui` loads.
                let anchor = handle.mark_read_local().unwrap_or_default();
                tracing::info!(
                    kind = "channel",
                    stage = "read_marked",
                    elapsed_ms = open_started.elapsed().as_millis(),
                    "conversation_open"
                );
                // Reading a foldered channel has to clear its folder badge too.
                let ui_ctx_badges = ui_ctx.clone();
                crate::run_history_ui!(weak, owner, |w| super::refresh_section_badges(
                    &w,
                    &ui_ctx_badges
                ));

                load_channel_into_ui_owned(
                    weak,
                    channel_id,
                    ui_ctx.clone(),
                    anchor,
                    Some(handle),
                    owner,
                    Some(open_started),
                )
                .await;
            });
        });
}

fn refresh_channel_call(
    weak: slint::Weak<MainWindow>,
    client: Arc<ChatterClient>,
    channel: String,
) {
    if channel.is_empty() || channel == "00000000-0000-0000-0000-000000000000" {
        return;
    }
    tokio::spawn(async move {
        let lookup = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.active_channel_call(&channel),
        )
        .await;
        run_ui!(weak, |w| {
            if !matches!(
                client.active_conversation(),
                chatter_core::state::ActiveConversation::Channel(id) if id.to_string() == channel
            ) {
                return;
            }
            match lookup {
                Ok(Ok(Some((id, video, participants)))) => {
                    w.global::<crate::CallView>()
                        .set_active_channel_call_id(id.into());
                    w.global::<crate::CallView>()
                        .set_active_channel_call_video(video);
                    w.global::<crate::CallView>()
                        .set_active_channel_call_participants(participants);
                }
                Ok(Ok(None)) => w
                    .global::<crate::CallView>()
                    .set_active_channel_call_id("".into()),
                Ok(Err(error)) => tracing::debug!(%error, "active channel call lookup failed"),
                Err(_) => tracing::debug!("active channel call lookup timed out"),
            }
        });
    });
}

/// Metadata-only reload shared by channel opens, settings Save, and inbox events.
/// Read/decode off the UI thread; apply only after the caller's ownership guard.
pub(crate) struct ChannelMetadata {
    id: String,
    name: String,
    topic: String,
    icon: Option<(Vec<u8>, u32, u32)>,
}

impl ChannelMetadata {
    pub(crate) fn from_channel(channel: &chatter_core::domain::channel::Channel) -> Self {
        let is_saved = matches!(channel.channel_type, ChannelType::SavedMessages);
        Self {
            id: channel.id.to_string(),
            name: if is_saved {
                "Saved messages".into()
            } else {
                channel.name.clone()
            },
            topic: if is_saved {
                String::new()
            } else {
                channel.description.clone().unwrap_or_default()
            },
            icon: channel
                .icon
                .as_deref()
                .filter(|b| !b.is_empty())
                .and_then(|bytes| image::load_from_memory(bytes).ok())
                .map(|img| {
                    let rgba = img.to_rgba8();
                    let (width, height) = rgba.dimensions();
                    (rgba.into_raw(), width, height)
                }),
        }
    }

    pub(crate) fn apply(self, w: &MainWindow) {
        // Channel settings match their channel to the open one by this id;
        // left unset, the settings sheet never showed the channel's icon.
        w.global::<crate::ChatPane>().set_channel_id(self.id.into());
        w.global::<crate::ChatPane>()
            .set_channel_name(self.name.into());
        w.global::<crate::ChatPane>().set_topic(self.topic.into());
        if let Some((rgba, width, height)) = self.icon {
            w.global::<crate::ChatPane>().set_channel_avatar_source(
                crate::bin_ui::slint_conversions::rgba_to_slint_image(&rgba, width, height),
            );
            w.global::<crate::ChatPane>().set_channel_has_avatar(true);
        } else {
            w.global::<crate::ChatPane>()
                .set_channel_avatar_source(slint::Image::default());
            w.global::<crate::ChatPane>().set_channel_has_avatar(false);
        }
    }
}

/// Load a channel into the UI.
///
/// `anchor` is the unread boundary the open captured on its way in (see
/// `ConversationHandle::mark_as_read`); paths with no unread concept — a
/// freshly created channel, Saved Messages — pass `UnreadAnchor::default()`,
/// which opens at the newest message.
///
/// `receipts`: the handle whose `mark_read_local` produced `anchor`; its read
/// receipts are sent for the page loaded here rather than a second query.
pub async fn load_channel_into_ui(
    weak: slint::Weak<MainWindow>,
    channel_id: ChannelId,
    ui_ctx: Arc<ChatterClient>,
    anchor: chatter_core::domain::message::UnreadAnchor,
    receipts: Option<chatter_core::services::conversation::ConversationHandle>,
) -> Vec<String> {
    let owner = HistoryOwner::begin(
        &ui_ctx,
        chatter_core::state::ActiveConversation::Channel(channel_id),
    );
    load_channel_into_ui_owned(weak, channel_id, ui_ctx, anchor, receipts, owner, None).await
}

pub(crate) async fn load_channel_into_ui_owned(
    weak: slint::Weak<MainWindow>,
    channel_id: ChannelId,
    ui_ctx: Arc<ChatterClient>,
    anchor: chatter_core::domain::message::UnreadAnchor,
    receipts: Option<chatter_core::services::conversation::ConversationHandle>,
    owner: HistoryOwner,
    open_started: Option<std::time::Instant>,
) -> Vec<String> {
    if !owner.is_current() {
        return Vec::new();
    }
    if let Some(started) = open_started {
        tracing::info!(
            kind = "channel",
            stage = "pins_loaded",
            elapsed_ms = started.elapsed().as_millis(),
            "conversation_open"
        );
    }
    // 1. Get channel metadata
    let channel = match ui_ctx.get_channel_local(&channel_id) {
        Some(c) => c,
        None => {
            tracing::error!("Channel {} not found locally", channel_id);
            crate::run_history_ui!(weak, owner, |w| w
                .global::<crate::ChatPane>()
                .set_loading_messages(false));
            return Vec::new();
        }
    };

    // Pinned bar. Here, not in each caller: Saved Messages, join and
    // create open through this function too and never loaded their pins.
    // Load pinned messages for this channel from DB, in the channel's own
    // conversation-type bucket (the vault is `SavedMessages`, not `Channel`).
    let (msg_ids, preview) = match ui_ctx
        .load_pinned_for_channel(&channel_id, &ui_ctx.channel_conversation_type(&channel_id))
        .await
    {
        Ok(pinned) if !pinned.is_empty() => {
            let ids = pinned.iter().map(|p| p.message_id).collect();
            let preview = pinned[0].preview.clone();
            (ids, preview)
        }
        _ => (vec![], String::new()),
    };
    if !owner.is_current() {
        return Vec::new();
    }
    super::apply_pinned_ui_owned(&weak, &ui_ctx, msg_ids, preview, &owner);

    // 2. Load messages — use the channel's actual conversation type so
    // SavedMessages (stored as ConversationType::SavedMessages) round-trip correctly.
    let conv_type = channel.conversation_type();
    let messages = match anchor
        .opening_page(chatter_core::config::MESSAGE_PAGE_SIZE as u32, |limit| {
            ui_ctx.get_messages_local_asc(&channel_id, &conv_type, None, limit)
        }) {
        Ok(msgs) => msgs,
        Err(e) => {
            tracing::error!("Failed to list channel messages: {}", e);
            crate::run_history_ui!(weak, owner, |w| w
                .global::<crate::ChatPane>()
                .set_loading_messages(false));
            return Vec::new();
        }
    };
    if let Some(started) = open_started {
        tracing::info!(
            kind = "channel",
            stage = "page_loaded",
            rows = messages.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "conversation_open"
        );
    }

    if !owner.is_current() {
        return Vec::new();
    }
    if let Some(handle) = receipts {
        handle.send_read_receipts_for_page(&messages);
    }

    let message_timestamps: Vec<String> = messages.iter().map(|m| m.sent_at.to_string()).collect();

    // 3. Convert to display format
    let mut display_messages = messages_to_display_batch_deferred(
        &messages,
        |ms| ui_ctx.message_sender_display_names(ms),
        |m| ui_ctx.message_sender_avatar(m),
        crate::bin_ui::converters::attachment_display_resolver(&ui_ctx),
        |m| ui_ctx.reply_author_display_name(m),
        |m| ui_ctx.reply_parent_missing_for(m),
        |m| ui_ctx.reply_parent_media(m),
        |m| ui_ctx.mention_members(&m.conversation_type, &m.conversation_id),
    )
    .await;
    crate::bin_ui::converters::mark_disappearing(&ui_ctx, &mut display_messages);
    crate::bin_ui::converters::apply_reply_counts(
        &mut display_messages,
        &ui_ctx.thread_reply_counts(&channel_id, &conv_type),
    );
    if let Some(started) = open_started {
        tracing::info!(
            kind = "channel",
            stage = "display_built",
            rows = display_messages.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "conversation_open"
        );
    }

    if !owner.is_current() {
        return Vec::new();
    }
    // 4. Update UI
    let metadata = ChannelMetadata::from_channel(&channel);
    let conversation_type = channel.conversation_type();
    let private_channel = matches!(channel.channel_type, ChannelType::Private);
    let member_count = channel.member_count as i32;
    let current_user_id = ui_ctx
        .session_current_user_id()
        .map(|id| id.to_string())
        .unwrap_or_default();
    let ui_ctx_gifs = ui_ctx.clone();

    let anchor = ui_ctx_gifs.opening_unread_anchor(anchor);
    let unread_index = anchor.divider_index(&messages);
    crate::run_history_ui!(weak, owner, |w| {
        let pane = w.global::<crate::ChatPane>();
        pane.set_unread_index(unread_index.map_or(-1, |i| i as i32));
        pane.set_unread_count(anchor.unread_count as i32);
        pane.set_is_at_bottom(unread_index.is_none());
        metadata.apply(&w);
        crate::bin_ui::converters::set_conversation_kind(
            &w,
            &ui_ctx_gifs,
            &conversation_type,
            private_channel,
        );
        w.global::<crate::ChatPane>().set_is_dm(false);
        w.global::<crate::ChatPane>().set_member_count(member_count);

        w.global::<crate::ChatPane>()
            .set_header_presence_status("offline".into());
        let slint_messages: Vec<_> = display_messages
            .iter()
            .map(|m| m.to_slint_preview_with_user(&current_user_id))
            .collect();
        let model = crate::bin_ui::message_model::MessageModel::from(slint_messages);
        if conversation_type == chatter_core::domain::message::ConversationType::Channel {
            crate::bin_ui::features::chat::recompute_channel_continuations(&model);
        }
        w.global::<crate::ChatPane>()
            .set_messages(ModelRc::new(model));
        if let Some(started) = open_started {
            tracing::info!(
                kind = "channel",
                stage = "model_replaced",
                rows = display_messages.len(),
                elapsed_ms = started.elapsed().as_millis(),
                "conversation_open"
            );
        }
        if open_started.is_some() {
            crate::trace_chat_open_model(display_messages.len(), 0);
        }
        w.global::<crate::ChatPane>().set_loading_messages(false);
        super::super::header::finish_pending_jump(&w, &ui_ctx);
        super::super::header::refresh_pinned_ui(&w, &ui_ctx);

        // Auto-register GIF animations for inline playback
        crate::bin_ui::gif_state::auto_register_gifs_from_messages(w.as_weak(), ui_ctx_gifs);
    });

    message_timestamps
}
