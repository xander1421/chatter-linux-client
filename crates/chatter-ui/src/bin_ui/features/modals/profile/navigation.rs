use super::*;

pub(super) fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Mute — persists through the same `mute_conversation` path channel
    // settings uses, keyed on the DM with this peer.
    let weak_window = window.as_weak();
    let ui_ctx_mute = ui_ctx.clone();
    window
        .global::<crate::ModalHostState>()
        .on_profile_toggle_mute(move || {
            let Some(w) = weak_window.upgrade() else {
                return;
            };
            let Ok(peer_id) = w
                .global::<crate::ModalHostState>()
                .get_profile_user_id()
                .parse::<UserId>()
            else {
                return;
            };
            let muted = !w.global::<crate::ModalHostState>().get_profile_is_muted();
            // Reflect immediately, then persist — snapping back on failure.
            w.global::<crate::ModalHostState>()
                .set_profile_is_muted(muted);
            if let Err(e) = ui_ctx_mute.mute_conversation(
                if w.global::<crate::ModalHostState>()
                    .get_profile_scoped_member()
                {
                    SHARED_PAGE
                        .with(|p| p.borrow().conversation)
                        .unwrap_or(chatter_core::state::ActiveConversation::None)
                } else {
                    dm_conversation(&peer_id)
                },
                muted,
            ) {
                tracing::error!(error = %e, "Failed to persist profile mute");
                w.global::<crate::ModalHostState>()
                    .set_profile_is_muted(!muted);
            } else {
                crate::bin_ui::features::sidebar::refresh_mute_rows(&w, &ui_ctx_mute);
            }
        });

    // Call / Video. `start-voice-call` deliberately takes no target — it reads
    // the ACTIVE conversation. So the profile's buttons open the DM first,
    // exactly the way the Calls screen's redial does, and only then place the
    // call.
    let weak_window = window.as_weak();
    window
        .global::<crate::ModalHostState>()
        .on_profile_start_call(move |is_video| {
            let Some(w) = weak_window.upgrade() else {
                return;
            };
            // No calls from a secret chat (the tiles are hidden there too).
            if w.global::<crate::ModalHostState>()
                .get_profile_scoped_member()
                || w.global::<crate::ModalHostState>()
                    .get_profile_is_anonymous()
            {
                return;
            }
            let peer_id = w
                .global::<crate::ModalHostState>()
                .get_profile_user_id()
                .to_string();
            if peer_id.is_empty() {
                return;
            }
            crate::bin_ui::features::back::reset_to_chats(&w);
            // Opening first commits `ActiveConversation`, which is what
            // `start_*_call` dispatches on — same ordering the Calls screen's
            // redial uses.
            w.global::<crate::SidebarPane>()
                .invoke_dm_selected(peer_id.into());
            if is_video {
                w.global::<crate::ChatPane>().invoke_start_video_call();
            } else {
                w.global::<crate::ChatPane>().invoke_start_voice_call();
            }
        });
}

pub(super) fn wire_message(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Send message (open DM). Core picks the conversation: a friend's DM in
    // either mode (Shield sends it over the mailbox); for anyone else in
    // shield mode the anon DM with them, or none at all. Opening goes through
    // the same `dm-selected` path as every other DM entry point.
    let weak_window = window.as_weak();
    let ui_ctx_dm = ui_ctx.clone();
    window
        .global::<crate::ModalHostState>()
        .on_profile_send_message(move || {
            let Some(w) = weak_window.upgrade() else {
                return;
            };
            if w.global::<crate::ModalHostState>()
                .get_profile_scoped_member()
            {
                return;
            }
            let peer = w
                .global::<crate::ModalHostState>()
                .get_profile_user_id()
                .to_string();
            let Some(dm_id) = ui_ctx_dm
                .resolve_dm_conversation(&peer)
                .and_then(|conversation| conversation.sidebar_key())
            else {
                tracing::warn!(
                    "profile: no conversation with this peer is available in the current mode"
                );
                return;
            };
            crate::bin_ui::features::back::reset_to_chats(&w);
            w.global::<crate::SidebarPane>()
                .invoke_dm_selected(dm_id.into());
        });
}
