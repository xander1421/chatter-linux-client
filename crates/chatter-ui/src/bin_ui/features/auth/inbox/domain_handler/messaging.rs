//! Message receive/send/fail, channel member, pin/unpin, and unread count handlers.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, VecModel, Weak};

use crate::MainWindow;
use crate::MessageItemData;
use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::{ChannelId, UserId};
use chatter_core::domain::message::Message;

/// Handle an incoming message (DM, channel, or anonymous).
///
/// Resolves sender name + attachments, inserts into the Slint message model
/// if the conversation is active, updates sidebar preview, and triggers
/// attachment auto-download.
pub(super) fn handle_message_received(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    message: Message,
) {
    let conversation_id = message.conversation_id;
    let sender_id = message.sender_id;
    let timestamp = message.sent_at;
    let conv_type = message.conversation_type.clone();

    // Sender name resolution (conversation-scoped) lives in core.
    let sender_name = ui_ctx.message_sender_display_name(&message);

    // Sender avatar resolution (conversation-scoped) lives in core.
    let sender_avatar = ui_ctx.message_sender_avatar(&message);

    // Resolve attachments from attachment service
    let resolved_attachments: Vec<_> = message
        .attachments
        .iter()
        .filter_map(|att_id| ui_ctx.get_attachment(att_id))
        .collect();

    // Build display data on this thread (before moving to UI thread)
    // Uses unified converter that handles optional avatars
    let mut display_data = crate::bin_ui::converters::message_to_display_data(
        &message,
        &sender_name,
        &resolved_attachments,
        sender_avatar,
    );

    // Reply metadata — mirror the reload batch so a LIVE-received reply shows the
    // quote thumbnail/player immediately instead of a text label until reload.
    // Must run BEFORE decode so the local-thumbnail fallback gets decoded.
    if display_data.is_reply {
        display_data.reply_to_sender_name = ui_ctx.reply_author_display_name(&message);
        display_data.reply_to_not_found = ui_ctx.reply_parent_missing_for(&message);
        crate::bin_ui::converters::apply_reply_parent_media(
            &mut display_data,
            ui_ctx.reply_parent_media(&message),
        );
    }
    display_data.decode_attachment_thumbnails();

    let current_user_id = ui_ctx
        .auth_state()
        .get()
        .current_user_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| {
            ui_ctx
                .session_current_user_id()
                .map(|id| id.to_string())
                .unwrap_or_default()
        });

    // Build sidebar preview + time via the shared core helpers, from the
    // conversation's NEWEST message rather than this one: a late-delivered
    // older message (a resend, an offline backlog) flipped the row's preview
    // and time backwards until the next rebuild put them right again.
    let latest = ui_ctx.latest_message(&conversation_id, &conv_type);
    let (preview, timestamp_str) =
        ui_ctx.sidebar_activity(Some(latest.as_ref().unwrap_or(&message)));

    // Core's one "is this message on screen" rule (mailbox DMs included).
    let is_active = ui_ctx.is_showing(&message);
    tracing::info!(
        msg_conv = %conversation_id,
        is_active,
        ?conv_type,
        "[UI_RECV] handle_message_received"
    );

    // If the user is looking at this conversation, flip the incoming message
    // (and any previously-stored unreads) to read so the sidebar badge doesn't
    // accumulate. Unread count is computed from messages.is_read.
    if is_active {
        if let Err(e) = ui_ctx.mark_conversation_read(&conversation_id, &conv_type) {
            tracing::warn!(error = %e, "Failed to mark active conversation as read on incoming message");
        }
    }

    // Classify into the typed ActiveConversation via the core helper.
    let sidebar_conversation =
        ui_ctx.classify_active_conversation(&conversation_id, &conv_type, sender_id);
    // Absolute badge values, read here on the runtime (after the mark-read
    // above), never counted up on the UI thread.
    let badges = ui_ctx.unread_badges(&sidebar_conversation);

    // Trigger attachment auto-download in background (fetchable = has CDN url + key).
    let downloadable: Vec<_> = resolved_attachments
        .iter()
        .filter(|a| a.is_downloadable())
        .cloned()
        .collect();

    if !downloadable.is_empty() {
        let ui_ctx_dl = ui_ctx.clone();
        let weak_dl = window.clone();
        let conv_id = conversation_id.to_string();
        tokio::spawn(async move {
            super::super::attachment_download::download_attachments(
                &downloadable,
                ui_ctx_dl,
                weak_dl,
                conv_id,
            )
            .await;
        });
    }

    // The message ends its sender's typing: record it so a STARTED sent before
    // it but delivered after it is dropped rather than shown.
    if let Some(sid) = sender_id {
        ui_ctx.update_typing_indicator(conversation_id, sid, timestamp, false);
    }

    let weak = window.clone();
    let ui_ctx_ui = ui_ctx.clone();

    // A thread reply belongs to the thread panel, not the main message list.
    // Route it there (and bump the root bubble's reply pill) instead of
    // inserting it inline, which used to render every reply twice.
    let thread_root = message.thread_id;
    let panel_shows_this_thread = thread_root
        .zip(ui_ctx.chat_state().active_thread())
        .map(|(root, panel)| panel.root_message_id == root)
        .unwrap_or(false);

    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };

        // A message ends its sender's typing. Keyed by conversation, like the
        // typing state itself: the sender's id matched only for DMs, so in a
        // channel the indicator outlived the message it announced.
        let typing_text = ui_ctx_ui.typing_indicator_text(conversation_id);
        if typing_text.is_empty() {
            super::super::typing::clear_typing_for_peer(
                &w,
                &conversation_id.to_string(),
                is_active,
            );
        } else {
            super::super::typing::set_typing_with_timer(
                &w,
                &conversation_id.to_string(),
                ui_ctx_ui.chat_state(),
                conversation_id,
                &typing_text,
            );
        }

        if let Some(root_id) = thread_root {
            if is_active {
                let reply = display_data.to_slint_with_user(&current_user_id);
                crate::bin_ui::features::thread::apply_live_thread_reply(
                    &w,
                    &root_id,
                    reply,
                    panel_shows_this_thread,
                );
            }
            crate::bin_ui::features::sidebar::apply_received_to_sidebar(
                &w,
                &ui_ctx_ui,
                sidebar_conversation,
                &preview,
                &timestamp_str,
                badges,
            );
            return;
        }

        if is_active {
            let was_at_bottom = w.get_is_at_bottom();

            // Build Slint message item
            let mut new_msg = display_data.to_slint_with_user(&current_user_id);

            // Only non-anon messages get auto-delivered status (anon has its own delivery logic)
            if !conv_type.is_anon() {
                new_msg.delivery_status = "delivered".into();
            }

            // Dedup + ordered-insert POLICY lives in core (Signal-style: by
            // timestamp_ns, tiebreak sender_id). Here we only extract the keys
            // and apply the decision to the Slint model.
            let current_model = w.get_messages();
            let new_ts = new_msg.timestamp_ns.to_string().parse::<u64>().unwrap_or(0);
            let new_sender = new_msg.sender_id.to_string();

            let keys: Vec<(u64, String)> = current_model
                .iter()
                .map(|m| {
                    (
                        m.timestamp_ns.to_string().parse::<u64>().unwrap_or(0),
                        m.sender_id.to_string(),
                    )
                })
                .collect();
            let key_refs: Vec<(u64, &str)> = keys.iter().map(|(t, s)| (*t, s.as_str())).collect();

            match chatter_core::domain::message::ordered_insert_position(
                &key_refs,
                new_ts,
                &new_sender,
            ) {
                chatter_core::domain::message::InsertPosition::Duplicate => {
                    tracing::info!(
                        existing = current_model.row_count(),
                        "[UI_RECV] duplicate, skipped"
                    );
                }
                chatter_core::domain::message::InsertPosition::At(insert_pos) => {
                    if let Some(vec_model) = current_model
                        .as_any()
                        .downcast_ref::<VecModel<MessageItemData>>()
                    {
                        vec_model.insert(insert_pos, new_msg);
                        tracing::info!(insert_pos, "[UI_RECV] inserted via VecModel.insert");
                    } else {
                        let mut messages: Vec<MessageItemData> = current_model.iter().collect();
                        messages.insert(insert_pos, new_msg);
                        w.set_messages(ModelRc::new(VecModel::from(messages)));
                        tracing::warn!(insert_pos, "[UI_RECV] downcast failed; replaced model");
                    }

                    crate::bin_ui::gif_state::auto_register_gifs_from_messages(
                        weak.clone(),
                        ui_ctx_ui.clone(),
                    );
                }
            }

            if was_at_bottom {
                w.set_is_at_bottom(true);
            }
        }

        // Always update sidebar preview
        crate::bin_ui::features::sidebar::apply_received_to_sidebar(
            &w,
            &ui_ctx_ui,
            sidebar_conversation,
            &preview,
            &timestamp_str,
            badges,
        );
    });
}

/// Handle DM bootstrap completed event.
///
/// Core emits it once the messages queued behind the key exchange were sent,
/// so this only clears the composer placeholder and flips the queued bubbles
/// ("sending") to "sent".
pub(super) fn handle_dm_bootstrap_completed(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    peer_id: UserId,
    sent_timestamps: Vec<u64>,
) {
    let is_active = ui_ctx.active_conversation().matches_dm_peer(&peer_id);
    crate::run_ui!(window, |w| {
        w.set_composer_placeholder("Type a message...".into());
        if is_active {
            set_own_status_at(&w, &sent_timestamps, "sent");
        }
        tracing::debug!(peer_id = %peer_id, "DM bootstrap complete - composer enabled");
    });
}

/// Set the status of our own bubbles sent at `timestamps` (`timestamp_ns`) in
/// the open message list. Core has already stored the same status.
pub(crate) fn set_own_status_at(w: &MainWindow, timestamps: &[u64], status: &str) {
    let messages = w.get_messages();
    let Some(model) = messages
        .as_any()
        .downcast_ref::<VecModel<MessageItemData>>()
    else {
        return;
    };
    for i in 0..model.row_count() {
        let Some(mut msg) = model.row_data(i) else {
            continue;
        };
        if msg.is_own
            && msg
                .timestamp_ns
                .parse::<u64>()
                .is_ok_and(|ts| timestamps.contains(&ts))
        {
            msg.delivery_status = chatter_core::services::message::advance_delivery_status(
                msg.delivery_status.as_str(),
                status,
            )
            .into();
            model.set_row_data(i, msg);
        }
    }
}

/// Update delivery status checkmarks (✓ → ✓✓) in the active message list.
///
/// Called for both `MessagesDelivered` and `MessagesRead` DomainEvents.
pub(super) fn handle_delivery_status_update(
    window: &Weak<MainWindow>,
    message_timestamps: Vec<u64>,
    new_status: &'static str,
) {
    let weak = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };

        let messages = w.get_messages();
        let timestamp_strs: Vec<String> =
            message_timestamps.iter().map(|ts| ts.to_string()).collect();

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.is_own
                    && timestamp_strs
                        .iter()
                        .any(|ts| msg.timestamp_ns.as_str() == ts.as_str())
                {
                    let mut updated = msg.clone();
                    updated.delivery_status =
                        chatter_core::services::message::advance_delivery_status(
                            msg.delivery_status.as_str(),
                            new_status,
                        )
                        .into();
                    if let Some(model) = messages
                        .as_any()
                        .downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated);
                    }
                }
            }
        }
    });
}

/// Update a reaction on a message in the active message list.
///
/// Matches by Signal-style (sender_id, sent_at_ns) composite key — the canonical
/// message identifier per Signal Protocol spec.
pub(super) fn handle_reaction_update(
    window: &Weak<MainWindow>,
    sender_id: &str,
    sent_at_ns: u64,
    emoji: &str,
    remove: bool,
) {
    let weak = window.clone();
    let sender_id = sender_id.to_string();
    let sent_at_ns_str = sent_at_ns.to_string();
    let emoji = emoji.to_string();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };
        crate::bin_ui::features::chat::update_reaction_in_ui(
            &w,
            &sender_id,
            &sent_at_ns_str,
            &emoji,
            remove,
        );
    });
}

/// Refresh the sidebar preview for the active conversation after a reaction change.
pub(super) fn refresh_sidebar_for_reaction(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    emoji: &str,
    remove: bool,
) {
    let weak = window.clone();
    let conversation = ui_ctx.chat_state().active_conversation();
    let emoji = emoji.to_string();

    if matches!(conversation, chatter_core::state::ActiveConversation::None) {
        return;
    }

    let ui_ctx_for_sidebar = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };
        if remove {
            return;
        }
        let preview = format!("Reacted {}", emoji);
        let now = ui_ctx_for_sidebar.sidebar_time(chrono::Utc::now());
        crate::bin_ui::features::sidebar::update_sidebar_preview(
            &w,
            &ui_ctx_for_sidebar,
            conversation,
            &preview,
            &now,
        );
    });
}

/// Update a message's content after an edit.
///
/// Only in the conversation on screen, and only the bubble whose Signal-style
/// key (author, sent timestamp) matches — a timestamp alone let one member
/// rewrite another member's bubble by reusing its timestamp.
pub(super) fn handle_message_edit(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    conversation_id: ChannelId,
    message_id: &str,
    author_id: UserId,
    target_sent_timestamp: u64,
    new_text: &str,
) {
    // The chat-list row previews the edited text too, open or not.
    crate::bin_ui::features::sidebar::history_changed(window, ui_ctx);
    if !ui_ctx.is_showing_conversation(conversation_id) {
        return;
    }
    let message_id = message_id.to_string();
    let author = author_id.to_string();
    let ts_str = target_sent_timestamp.to_string();
    let new_text = new_text.to_string();

    crate::run_ui!(window, |w| {
        crate::bin_ui::features::thread::apply_thread_edit(
            &w,
            &message_id,
            &author,
            target_sent_timestamp,
            &new_text,
        );
        let messages = w.get_messages();
        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.sender_id.as_str() == author && msg.timestamp_ns.as_str() == ts_str {
                    let mut updated = msg.clone();
                    crate::bin_ui::converters::apply_edit(&mut updated, &new_text);
                    if let Some(model) = messages
                        .as_any()
                        .downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated);
                    }
                    return;
                }
            }
        }
    });
}

/// Remove a message from the open view: delete for everyone (keyed by author
/// + sent timestamp) or a delete-for-me synced from a sibling device (keyed
/// by the local message id core resolved). Core announces a delete only after
/// it matched a stored row, so this never blanks a bubble storage still holds.
pub(super) fn handle_message_delete(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    conversation_id: ChannelId,
    target: DeleteTarget,
) {
    // A deleted last message takes the row's preview with it, open or not.
    crate::bin_ui::features::sidebar::history_changed(window, ui_ctx);
    if !ui_ctx.is_showing_conversation(conversation_id) {
        return;
    }
    crate::run_ui!(window, |w| {
        crate::bin_ui::features::chat::remove_messages_where(&w, |msg| target.matches(msg));
    });
}

/// Which bubble a delete names.
pub(super) enum DeleteTarget {
    /// Delete for everyone: the Signal-style (author, sent timestamp) key.
    AuthorTimestamp { author: UserId, sent_at: u64 },
    /// Delete-for-me from a sibling device: the local message id.
    Id(chatter_core::domain::ids::MessageId),
}

impl DeleteTarget {
    fn matches(&self, msg: &MessageItemData) -> bool {
        match self {
            Self::AuthorTimestamp { author, sent_at } => {
                msg.sender_id.as_str() == author.to_string()
                    && msg.timestamp_ns.as_str() == sent_at.to_string()
            }
            Self::Id(id) => msg.message_id.as_str() == id.to_string(),
        }
    }
}

/// Handle message send failed event — show error toast in the UI.
pub(super) fn handle_message_send_failed(
    window: &Weak<MainWindow>,
    channel_id: ChannelId,
    error: String,
    message_timestamps: Vec<u64>,
) {
    tracing::error!(
        channel_id = %channel_id,
        error = %error,
        "Message send failed"
    );

    let weak = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };
        // The only background send is the post-bootstrap flush; its
        // "Setting up secure channel…" placeholder ends here too.
        w.set_composer_placeholder("Type a message...".into());
        // The bubble says it too: "not sent", never the ticks of a delivery.
        // Only our own bubbles at these timestamps, so another open chat is
        // untouched.
        set_own_status_at(&w, &message_timestamps, "failed");
        w.set_toast_message("Failed to send message. Please try again.".into());
        w.set_toast_variant("error".into());
        w.set_toast_visible(true);
    });
}

// Channel member joined/left system messages are handled via
// handle_message_received() — the DomainEvent carries the Message directly.

/// Update the header member count if the given channel is currently active.
///
/// Called after ChannelMemberJoined/Left events. The local membership repo
/// is already updated by the time this runs, so we query it for the count.
pub(super) fn update_active_channel_member_count(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    channel_id: ChannelId,
) {
    crate::bin_ui::features::modals::refresh_members_if_open(
        window.clone(),
        ui_ctx.clone(),
        channel_id,
    );

    let is_active = ui_ctx
        .chat_state()
        .get()
        .active_channel_id
        .map(|id| id == channel_id)
        .unwrap_or(false);

    if !is_active {
        return;
    }

    let member_count = ui_ctx.list_channel_members_local(&channel_id).len() as i32;

    let weak = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.set_member_count(member_count);
        }
    });
}

/// Render only the persisted aggregate, keyed by message ID (not timestamp).
pub(super) fn handle_persisted_delivery_status(
    window: &Weak<MainWindow>,
    client: &Arc<ChatterClient>,
    message_id: String,
    status: String,
) {
    let details = message_id
        .parse()
        .ok()
        .and_then(|id| client.message_receipt_info(id).ok());
    let weak = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };
        let info = w.global::<crate::MessageInfo>();
        if w.get_active_modal().as_str() == "message-info"
            && info.get_message_id().as_str() == message_id
        {
            if let Some(details) = details {
                info.set_details(details.into());
            }
        }
        let messages = w.get_messages();
        let Some(model) = messages
            .as_any()
            .downcast_ref::<VecModel<MessageItemData>>()
        else {
            return;
        };
        for i in 0..model.row_count() {
            let Some(mut message) = model.row_data(i) else {
                continue;
            };
            if message.message_id.as_str() == message_id {
                message.delivery_status = chatter_core::services::message::advance_delivery_status(
                    message.delivery_status.as_str(),
                    &status,
                )
                .into();
                model.set_row_data(i, message);
            }
        }
    });
}
