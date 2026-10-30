use super::*;
use slint::Model;

pub(super) fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let client = ui_ctx.clone();
    window.global::<crate::ModalHostState>().on_profile_share_contact(move || {
        let Some(w) = weak.upgrade() else { return };
        let state = w.global::<crate::ModalHostState>();
        if state.get_profile_is_self() || state.get_profile_is_anonymous() || state.get_profile_scoped_member() {
            return;
        }
        let Ok(peer) = state.get_profile_user_id().parse::<UserId>() else { return };
        match client.share_contact_text(&peer) {
            Some(text) => crate::bin_ui::features::chat::open_contact_share_modal(&w, &client, text),
            None => {
                w.set_toast_message("No shareable contact link or handle is available".into());
                w.set_toast_variant("error".into());
                w.set_toast_visible(true);
            }
        }
    });

    let weak = window.as_weak();
    let client = ui_ctx.clone();
    window.global::<crate::ModalHostState>().on_profile_edit_contact(move |name| {
        let Some(w) = weak.upgrade() else { return };
        let state = w.global::<crate::ModalHostState>();
        if state.get_profile_is_self() || state.get_profile_is_anonymous() || state.get_profile_scoped_member() {
            return;
        }
        let Ok(peer) = state.get_profile_user_id().parse::<UserId>() else { return };
        if let Err(error) = client.set_contact_name(&peer, name.as_str()) {
            tracing::error!(%error, "contact name save failed");
            w.set_toast_message("Could not save contact name".into());
            w.set_toast_variant("error".into());
            w.set_toast_visible(true);
            return;
        }
        let resolved = client.display_name(&peer);
        state.set_profile_contact_name(client.contact_name(&peer).unwrap_or_default().into());
        state.set_profile_display_name(resolved.clone().into());
        state.set_profile_initials(crate::bin_ui::converters::initials_from_name(&resolved).into());
        if client.active_conversation() == (chatter_core::state::ActiveConversation::ServerDm { peer_id: peer })
            || matches!(client.active_conversation(), chatter_core::state::ActiveConversation::AnonDm(id)
                if client.get_anon_dm_conversation(&id).is_some_and(|dm| dm.peer_id == peer)) {
            w.global::<crate::ChatPane>().set_channel_name(resolved.into());
        }
        let messages = w.global::<crate::ChatPane>().get_messages();
        let resolved = client.display_name(&peer);
        let initials = crate::bin_ui::converters::initials_from_name(&resolved);
        for i in 0..messages.row_count() {
            if let Some(mut row) = messages.row_data(i) {
                if row.sender_id.as_str() == peer.to_string() && !row.is_system {
                    row.sender_name = resolved.clone().into();
                    row.sender_initials = initials.clone().into();
                    messages.set_row_data(i, row);
                }
            }
        }
        crate::bin_ui::features::auth::initial_data::refresh_friends_list_local(w.as_weak(), client.clone());
        let weak = w.as_weak();
        let client = client.clone();
        tokio::spawn(async move {
            crate::bin_ui::features::sidebar::rebuild_chat_list(&weak, &client);
        });
    });
}
