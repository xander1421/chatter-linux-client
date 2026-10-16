//! Private group selection and loading.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::anon_group::AnonGroupId;

use crate::MainWindow;

use super::stop_audio_playback;
use crate::bin_ui::features::history_owner::HistoryOwner;
use std::str::FromStr;

/// Load and display messages for a private group.
/// This can be called both when a group is selected and when member profiles are updated.
pub fn load_private_group_messages(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    group_id_str: &str,
) {
    let group_id = match AnonGroupId::from_str(group_id_str) {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("Invalid group ID '{}': {}", group_id_str, e);
            return;
        }
    };

    let ui_ctx = ui_ctx.clone();
    let auth_state = ui_ctx.auth_state().clone();
    let weak_window = window.as_weak();
    let group_id_str = group_id_str.to_string();

    let owner = HistoryOwner::begin(
        &ui_ctx,
        chatter_core::state::ActiveConversation::AnonGroup(group_id),
    );
    tokio::spawn(async move {
        if !owner.is_current() {
            return;
        }
        // Get group details
        let group = match ui_ctx.get_anon_group(&group_id) {
            Some(g) => g,
            None => {
                tracing::error!("Private group {} not found", group_id);
                return;
            }
        };

        // Get member count for header display
        let members = ui_ctx.get_anon_group_members(&group_id);
        let member_count = members.iter().filter(|m| m.mailbox_pk.is_some()).count() as i32;
        let invited_count = members.iter().filter(|m| m.mailbox_pk.is_none()).count() as i32;

        // Get current user ID for is_own calculation
        let current_user_id = auth_state.get().current_user_id;
        let current_user_id_str = current_user_id.map(|id| id.to_string()).unwrap_or_default();

        // Load messages for this group (reaction counts populated in core).
        let messages = match ui_ctx.get_anon_group_messages_asc(
            &group_id,
            None,
            chatter_core::config::MESSAGE_PAGE_SIZE,
        ) {
            Ok(msgs) => msgs,
            Err(e) => {
                tracing::error!(
                    "Failed to load messages for private group {}: {}",
                    group_id,
                    e
                );
                Vec::new()
            }
        };

        let display_messages = crate::bin_ui::converters::messages_to_display_batch(
            &messages,
            |ms| ui_ctx.message_sender_display_names(ms),
            |m| ui_ctx.message_sender_avatar(m),
            crate::bin_ui::converters::attachment_display_resolver(&ui_ctx),
            |m| ui_ctx.reply_author_display_name(m),
            |m| ui_ctx.reply_parent_missing_for(m),
            |m| ui_ctx.reply_parent_media(m),
        );

        // Update UI to show selected group and messages
        let group_name = group.name.clone();
        let ui_ctx_gifs = ui_ctx.clone();
        let ui_ctx_for_ui = ui_ctx.clone();
        crate::run_history_ui!(weak_window, owner, |w| {
            // Clear unread count in sidebar
            crate::bin_ui::features::auth::inbox::domain_handler::update_unread_count(
                &w,
                &ui_ctx_for_ui,
                &group_id_str,
                0,
            );

            // Set chat header info for private group
            w.set_channel_name(group_name.into());
            crate::bin_ui::converters::set_conversation_kind(
                &w,
                &ui_ctx_for_ui,
                &chatter_core::domain::message::ConversationType::AnonGroup,
                false,
            );
            w.set_is_dm(false);
            w.set_member_count(member_count);
            w.set_invited_count(invited_count);
            w.set_topic("".into());
            w.set_header_presence_status("offline".into());
            w.set_channel_has_avatar(false);
            w.set_channel_avatar_source(slint::Image::default());

            // Convert and set messages
            let slint_messages: Vec<_> = display_messages
                .iter()
                .map(|m| m.to_slint_with_user(&current_user_id_str))
                .collect();
            w.set_messages(ModelRc::new(VecModel::from(slint_messages)));
            w.set_loading_messages(false);
            super::super::header::finish_pending_jump(&w, &ui_ctx);
            super::super::header::refresh_pinned_ui(&w, &ui_ctx);

            // Auto-register GIF animations
            crate::bin_ui::gif_state::auto_register_gifs_from_messages(w.as_weak(), ui_ctx_gifs);
        });
    });
}

pub(crate) fn wire_private_group_selected(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    window.on_private_group_selected(move |group_id: SharedString| {
        if let Ok(id) = group_id.parse::<AnonGroupId>() {
            super::super::header::cancel_pending_jump_for(
                chatter_core::state::ActiveConversation::AnonGroup(id),
            );
        }
        let group_id_str = group_id.to_string();
        let weak = weak_window.clone();
        super::show_chat_pane(&weak);
        let ui_ctx = ui_ctx.clone();

        // Stop any playing audio when switching
        stop_audio_playback();
        crate::bin_ui::gif_state::clear_inline_gifs();

        // Commit active-conversation state SYNCHRONOUSLY on the UI thread
        // (same race fix as dm.rs — see comment there).
        if let Ok(parsed) = group_id_str.parse::<AnonGroupId>() {
            ui_ctx.chat_state().set_active_conversation(
                chatter_core::state::ActiveConversation::AnonGroup(parsed),
            );
        }
        if let Some(w) = weak.upgrade() {
            w.invoke_refresh_screen_protection();
        }

        let owner = HistoryOwner::begin(&ui_ctx, ui_ctx.chat_state().active_conversation());
        tokio::spawn(async move {
            if !owner.is_current() {
                return;
            }
            // Parse group ID
            let group_id: AnonGroupId = match group_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid private group ID: {}", e);
                    return;
                }
            };

            // Active conversation already committed pre-spawn.
            let ui_ctx_for_sel = ui_ctx.clone();
            crate::run_history_ui!(weak, owner, |w| super::refresh_selection(
                &w,
                &ui_ctx_for_sel
            ));

            // Load pinned messages
            let pinned = ui_ctx.list_anon_group_pinned(&group_id);
            let (msg_ids, preview) = if pinned.is_empty() {
                (vec![], String::new())
            } else {
                let ids = pinned.iter().map(|m| m.id).collect();
                let preview = pinned[0]
                    .as_text()
                    .filter(|t| !t.is_empty())
                    .map(|t| ui_ctx.truncate_message_preview(&t))
                    .unwrap_or_else(|| "[Pinned message]".to_string());
                (ids, preview)
            };
            if !owner.is_current() {
                return;
            }
            super::apply_pinned_ui_owned(&weak, &ui_ctx.chat_state(), msg_ids, preview, &owner);

            // Mark messages as read
            let handle =
                ui_ctx.conversation(chatter_core::state::ActiveConversation::AnonGroup(group_id));
            // Where the user left off — the mark is what destroys it.
            let anchor = handle.mark_as_read().await.unwrap_or_default();
            // Reading a foldered group has to clear its folder badge too.
            let ui_ctx_badges = ui_ctx.clone();
            crate::run_history_ui!(weak, owner, |w| super::refresh_section_badges(
                &w,
                &ui_ctx_badges
            ));

            if !owner.is_current() {
                return;
            }
            load_private_group_messages_internal(weak, group_id, ui_ctx, anchor, owner).await;
        });
    });
}

/// Internal shared loading logic for private groups.
async fn load_private_group_messages_internal(
    weak: slint::Weak<MainWindow>,
    group_id: AnonGroupId,
    ui_ctx: Arc<ChatterClient>,
    anchor: chatter_core::domain::message::UnreadAnchor,
    owner: HistoryOwner,
) {
    if !owner.is_current() {
        return;
    }
    let auth_state = ui_ctx.auth_state().clone();

    // Get group info
    let group = match ui_ctx.get_anon_group(&group_id) {
        Some(g) => g,
        None => {
            crate::run_history_ui!(weak, owner, |w| w.set_loading_messages(false));
            return;
        }
    };

    let members = ui_ctx.get_anon_group_members(&group_id);
    let member_count = members.iter().filter(|m| m.mailbox_pk.is_some()).count() as i32;
    let invited_count = members.iter().filter(|m| m.mailbox_pk.is_none()).count() as i32;

    let current_user_id_str = auth_state
        .get()
        .current_user_id
        .map(|id| id.to_string())
        .unwrap_or_default();

    // Reaction counts populated in core.
    let messages = ui_ctx
        .get_anon_group_messages_asc(&group_id, None, chatter_core::config::MESSAGE_PAGE_SIZE)
        .unwrap_or_default();

    let display_messages = crate::bin_ui::converters::messages_to_display_batch(
        &messages,
        |ms| ui_ctx.message_sender_display_names(ms),
        |m| ui_ctx.message_sender_avatar(m),
        crate::bin_ui::converters::attachment_display_resolver(&ui_ctx),
        |m| ui_ctx.reply_author_display_name(m),
        |m| ui_ctx.reply_parent_missing_for(m),
        |m| ui_ctx.reply_parent_media(m),
    );

    let group_name = group.name.clone();
    let group_id_str = group_id.to_string();
    let ui_ctx_gifs = ui_ctx.clone();
    let ui_ctx_for_ui = ui_ctx.clone();
    let anchor_page = messages.clone();
    crate::run_history_ui!(weak, owner, |w| {
        super::apply_unread_anchor(&w, anchor, &anchor_page);
        // Clear unread count in sidebar
        crate::bin_ui::features::auth::inbox::domain_handler::update_unread_count(
            &w,
            &ui_ctx_for_ui,
            &group_id_str,
            0,
        );

        w.set_channel_name(group_name.into());
        crate::bin_ui::converters::set_conversation_kind(
            &w,
            &ui_ctx_for_ui,
            &chatter_core::domain::message::ConversationType::AnonGroup,
            false,
        );
        w.set_is_dm(false);
        w.set_member_count(member_count);
        w.set_invited_count(invited_count);
        w.set_topic("".into());
        w.set_header_presence_status("offline".into());
        w.set_channel_has_avatar(false);
        w.set_channel_avatar_source(slint::Image::default());

        let slint_messages: Vec<_> = display_messages
            .iter()
            .map(|m| m.to_slint_with_user(&current_user_id_str))
            .collect();
        w.set_messages(ModelRc::new(VecModel::from(slint_messages)));
        w.set_loading_messages(false);
        super::super::header::finish_pending_jump(&w, &ui_ctx);
        super::super::header::refresh_pinned_ui(&w, &ui_ctx);

        crate::bin_ui::gif_state::auto_register_gifs_from_messages(w.as_weak(), ui_ctx_gifs);
    });
}
