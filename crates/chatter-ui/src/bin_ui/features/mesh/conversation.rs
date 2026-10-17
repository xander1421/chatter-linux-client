//! Opening a mesh conversation, and keeping it current.
//!
//! There is no mesh message list: a mesh conversation is loaded into the same
//! `messages` model every channel and DM uses, from the same local store, via
//! the same converters. The only mesh-specific parts are which id is loaded,
//! whose messages count as "mine" (the mesh identity, not the chatter one),
//! and what the composer says it is about to publish.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::services::mesh::MeshMessage;
use chatter_core::state::ActiveConversation;

use crate::bin_ui::converters::messages_to_display_batch;
use crate::{MainWindow, MeshGlobal, MessageItemData};

use super::core_api::{self, MeshRow};

/// Open the mesh conversation with this derived conversation id.
pub fn open(window: &MainWindow, ui_ctx: &Arc<ChatterClient>, id: &str) {
    let Some(row) = core_api::rows(ui_ctx).into_iter().find(|row| row.id == id) else {
        tracing::warn!(conversation = %id, "mesh: no conversation with that id");
        return;
    };

    super::set_active(Some(row.conversation.clone()));

    // Nothing server-backed is on screen any more. Leaving a stale
    // `ActiveConversation` behind would send the next message to a channel the
    // user is no longer looking at.
    ui_ctx.select_conversation(ActiveConversation::None);
    window.invoke_refresh_screen_protection();

    crate::bin_ui::features::sidebar::stop_audio_playback();
    crate::bin_ui::gif_state::clear_inline_gifs();

    // The mesh carries no attachments, and the composer hides the attach
    // button here. Anything staged in another conversation is dropped rather
    // than left armed to vanish on the next send.
    ui_ctx.chat_state().clear_pending_attachments();
    window.set_pending_attachments(ModelRc::new(
        VecModel::<crate::PendingAttachmentData>::from(Vec::new()),
    ));

    // Reveal the message pane the way every other conversation does; on a
    // phone this is the only thing that leaves the chat list.
    crate::bin_ui::features::sidebar::show_chat_pane(&window.as_weak());

    // Give the None-backed mesh scope a distinct composer owner before any
    // header clear. This also parks a server quote before leaving its view.
    window
        .global::<MeshGlobal>()
        .set_active_id(row.id.clone().into());
    crate::bin_ui::features::composer::sync_composer_draft(
        window,
        ui_ctx,
        ui_ctx.chat_state().get().attachment_revision,
    );
    apply_header(window, ui_ctx, &row);
    super::refresh_rows(window, ui_ctx);

    let owner = crate::bin_ui::features::history_owner::HistoryOwner::begin(
        ui_ctx,
        ActiveConversation::None,
    );
    let conversation = row.conversation.clone();
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    tokio::spawn(async move {
        if !owner.is_current() {
            return;
        }
        let local_user = super::local_user_id(&ui_ctx);
        // Oldest first, the same page size every other conversation loads.
        let messages = match core_api::history(
            &ui_ctx,
            &conversation,
            chatter_core::config::MESSAGE_PAGE_SIZE as u32,
        ) {
            Ok(messages) => messages,
            Err(error) => {
                tracing::error!(%error, "mesh: could not load the conversation");
                crate::run_history_ui!(weak, owner, |w| w.set_loading_messages(false));
                return;
            }
        };

        if !owner.is_current() {
            return;
        }
        let display = messages_to_display_batch(
            &messages,
            // Mesh senders, not chatter accounts — see `core_api::sender_display_name`.
            |ms| {
                ms.iter()
                    .map(|m| core_api::sender_display_name(&ui_ctx, m))
                    .collect()
            },
            // No chatter profile behind a mesh sender.
            |_| None,
            crate::bin_ui::converters::attachment_display_resolver(&ui_ctx),
            |m| ui_ctx.reply_author_display_name(m),
            |m| ui_ctx.reply_parent_missing_for(m),
            |m| ui_ctx.reply_parent_media(m),
        );

        crate::run_history_ui!(weak, owner, |w| {
            let items: Vec<MessageItemData> = display
                .iter()
                .map(|m| m.to_slint_with_user(&local_user))
                .collect();
            w.set_messages(ModelRc::new(VecModel::from(items)));
            w.set_loading_messages(false);
            w.set_is_at_bottom(true);
        });

        // Everything on screen has been read. Local only — telling the peer
        // costs airtime and is a separate act.
        if !owner.is_current() {
            return;
        }
        if let Err(error) = core_api::mark_read(&ui_ctx, &conversation) {
            tracing::debug!(%error, "mesh: could not mark the conversation read");
        }
        let weak_rows = weak.clone();
        let ui_ctx_rows = ui_ctx.clone();
        crate::run_history_ui!(weak_rows, owner, |w| super::refresh_rows(&w, &ui_ctx_rows));
    });
}

/// Header, composer and the mesh strip, for the conversation being opened.
fn apply_header(window: &MainWindow, ui_ctx: &ChatterClient, row: &MeshRow) {
    window.set_loading_messages(true);
    window.set_channel_id(row.id.clone().into());
    window.set_channel_name(row.title.clone().into());
    let conversation_type = if row.kind == "dm" {
        chatter_core::domain::message::ConversationType::MeshDm
    } else {
        chatter_core::domain::message::ConversationType::Mesh
    };
    crate::bin_ui::converters::set_conversation_kind(window, ui_ctx, &conversation_type, false);
    window.set_is_dm(row.kind == "dm");
    // Public scopes have no membership. A private channel gets its signed
    // roster count from core, where admission and rotation are enforced.
    window.set_member_count(if row.kind == "private-channel" {
        ui_ctx
            .mesh_private_channel_member_count(&row.scope)
            .unwrap_or(0) as i32
    } else {
        0
    });
    window.set_topic("".into());
    window.set_header_presence_status("offline".into());
    window.set_channel_avatar_source(slint::Image::default());
    window.set_channel_has_avatar(false);

    // The composer names the exact scope, because that scope is published
    // alongside whatever gets typed into it.
    window.set_composer_placeholder(
        match row.kind {
            "geohash" => format!("Message #{} — public to this area", row.scope),
            "room" => "Message everyone in radio range".to_string(),
            "private-channel" => format!("Message invited devices in {}", row.title),
            _ => format!("Message {} over the mesh", row.title),
        }
        .into(),
    );

    // Composer ownership was transferred synchronously above; history and
    // headers must never clear the newly restored draft. Mesh has no pins.

    window.set_has_pinned_messages(false);
    window.set_pinned_message_preview("".into());
    window.set_pinned_total_count(0);

    let global = window.global::<MeshGlobal>();
    global.set_private_leave_pending(false);
    global.set_active_id(row.id.clone().into());
    global.set_active_kind(row.kind.into());
    global.set_active_title(row.title.clone().into());
    global.set_active_scope(row.scope.clone().into());
    global.set_private_rekey_pending(
        row.kind == "private-channel"
            && ui_ctx
                .mesh_private_channel_rekey_pending(&row.scope)
                .unwrap_or(false),
    );
    global.set_active_reachable(row.reachable);
    global.set_active_verified(row.verified);
    global.set_identity_panel_open(false);
    global.set_binding_qr_visible(false);
    global.set_binding_scan_visible(false);
    global.set_private_invite_visible(false);
    global.set_private_remove_pending("".into());
    global.set_private_can_manage(
        row.kind == "private-channel"
            && ui_ctx
                .mesh_private_channel_is_creator(&row.scope)
                .unwrap_or(false),
    );
    global.set_active_signal_enabled(
        row.kind == "dm"
            && super::core_api::signal_opted_in(ui_ctx, row.scope.as_str()).unwrap_or(false),
    );
    global.set_active_protection_label(if row.kind == "private-channel" {
        "Private epoch key · invited devices".into()
    } else {
        super::core_api::signal_protection_label(ui_ctx, row.scope.as_str())
            .unwrap_or("Noise (bitchat)")
            .into()
    });
    global.set_notice("".into());
}

/// Append a mesh message that just arrived (or that we just sent — the service
/// mirrors our own sends back through the same stream, so there is exactly one
/// path that builds a mesh row).
pub(super) fn append_incoming(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    message: &MeshMessage,
) {
    let id = message.message.id.to_string();

    let model = window.get_messages();
    let mut items: Vec<MessageItemData> = model.iter().collect();
    // A courier resend maps onto the id it already had, so the same message
    // can legitimately arrive twice. Show it once.
    if items
        .iter()
        .any(|item| item.message_id.as_str() == id.as_str())
    {
        return;
    }

    let nickname = message
        .message
        .sender_id
        .as_ref()
        .and_then(|sender| ui_ctx.mesh_sender_display_name(sender))
        .unwrap_or_else(|| message.nickname.clone());
    let display = messages_to_display_batch(
        std::slice::from_ref(&message.message),
        // Core supplies collision suffixes; retain the live nickname if the
        // sender has no peer record (for example our own local echo).
        |ms| vec![nickname.clone(); ms.len()],
        |_| None,
        crate::bin_ui::converters::attachment_display_resolver(&ui_ctx),
        |m| ui_ctx.reply_author_display_name(m),
        |m| ui_ctx.reply_parent_missing_for(m),
        |m| ui_ctx.reply_parent_media(m),
    );

    let local_user = super::local_user_id(ui_ctx);
    for item in display.iter() {
        items.push(item.to_slint_with_user(&local_user));
    }
    window.set_messages(ModelRc::new(VecModel::from(items)));
    window.set_is_at_bottom(true);
}

/// Move a message this device sent to its new delivery state.
///
/// `label` is one of the strings `message_item.slint` knows: the ordinary
/// "sent"/"delivered"/"read", or the mesh-only "waiting"/"carried"/
/// "undelivered" that render as words instead of ticks.
pub(super) fn apply_delivery(window: &MainWindow, message_id: &str, label: &str) {
    let model = window.get_messages();
    let mut changed = false;
    let items: Vec<MessageItemData> = model
        .iter()
        .map(|mut item| {
            if item.message_id.as_str() == message_id && item.delivery_status.as_str() != label {
                item.delivery_status = label.into();
                changed = true;
            }
            item
        })
        .collect();
    if changed {
        window.set_messages(ModelRc::new(VecModel::from(items)));
    }
}
