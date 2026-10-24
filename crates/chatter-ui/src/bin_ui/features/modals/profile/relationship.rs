use super::*;

const REMOVE_FAILED: &str = "Not removed — you are still friends. Try again.";

/// Say how a relationship change on the profile went: the button flipping
/// label alone was the only feedback (REVIEW-R4 P1-12). UI thread only.
fn show_toast(w: &MainWindow, (message, variant): (&str, &str)) {
    w.set_toast_message(message.into());
    w.set_toast_variant(variant.into());
    w.set_toast_visible(true);
}

pub(super) fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Add friend
    let weak_window = window.as_weak();
    let ui_ctx_add = ui_ctx.clone();

    window
        .global::<crate::ModalHostState>()
        .on_profile_add_friend(move || {
            let weak = weak_window.clone();
            if weak.upgrade().is_some_and(|w| {
                w.global::<crate::ModalHostState>()
                    .get_profile_scoped_member()
            }) {
                return;
            }
            let ui_ctx = ui_ctx_add.clone();

            let profile_user_id = weak.upgrade().map(|w| {
                w.global::<crate::ModalHostState>()
                    .get_profile_user_id()
                    .to_string()
            });
            let profile_user_id = match profile_user_id {
                Some(id) if !id.is_empty() => id,
                _ => {
                    tracing::error!("Could not get profile user ID");
                    return;
                }
            };

            tokio::spawn(async move {
                let weak_loading = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_loading.upgrade() {
                        w.global::<crate::ModalHostState>()
                            .set_profile_loading(true);
                    }
                });

                let friend_id: UserId = match profile_user_id.parse() {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::error!("Invalid user ID: {}", e);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.global::<crate::ModalHostState>()
                                    .set_profile_loading(false);
                            }
                        });
                        return;
                    }
                };

                let notice = match tokio::time::timeout(
                    crate::bin_ui::ui_helpers::UI_SERVICE_TIMEOUT,
                    ui_ctx.send_friend_request(&friend_id, None),
                )
                .await
                {
                    Ok(Ok(_)) => {
                        tracing::info!("Friend request sent to {}", friend_id);
                        // Core emits `FriendRequestsUpdated` after send — the
                        // domain-handler dispatcher refreshes the list. No
                        // manual list_friend_requests + reshape here.
                        ("Friend request sent", "success")
                    }
                    Ok(Err(e)) => {
                        tracing::error!("Failed to send friend request: {}", e);
                        ("The friend request was not sent. Try again.", "error")
                    }
                    Err(_) => {
                        tracing::error!("Friend request timed out");
                        ("The friend request was not sent. Try again.", "error")
                    }
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.global::<crate::ModalHostState>()
                            .set_profile_loading(false);
                        show_toast(&w, notice);
                    }
                });
            });
        });

    // Remove friend
    let weak_window = window.as_weak();
    let ui_ctx_remove = ui_ctx.clone();

    window
        .global::<crate::ModalHostState>()
        .on_profile_remove_friend(move || {
            let weak = weak_window.clone();
            if weak.upgrade().is_some_and(|w| {
                w.global::<crate::ModalHostState>()
                    .get_profile_scoped_member()
            }) {
                return;
            }
            let ui_ctx = ui_ctx_remove.clone();

            let profile_user_id = weak.upgrade().map(|w| {
                w.global::<crate::ModalHostState>()
                    .get_profile_user_id()
                    .to_string()
            });
            let profile_user_id = match profile_user_id {
                Some(id) if !id.is_empty() => id,
                _ => {
                    tracing::error!("Could not get profile user ID");
                    return;
                }
            };

            tokio::spawn(async move {
                let weak_loading = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_loading.upgrade() {
                        w.global::<crate::ModalHostState>()
                            .set_profile_loading(true);
                    }
                });

                let friend_id: UserId = match profile_user_id.parse() {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::error!("Invalid user ID: {}", e);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.global::<crate::ModalHostState>()
                                    .set_profile_loading(false);
                            }
                        });
                        return;
                    }
                };

                match tokio::time::timeout(
                    crate::bin_ui::ui_helpers::UI_SERVICE_TIMEOUT,
                    ui_ctx.remove_friend(&friend_id),
                )
                .await
                {
                    Ok(Ok(())) => {
                        let display_name = ui_ctx.display_name(&friend_id);
                        let initials = crate::bin_ui::converters::initials_from_name(&display_name);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.global::<crate::ModalHostState>()
                                    .set_profile_loading(false);
                                w.global::<crate::ModalHostState>()
                                    .set_profile_is_friend(false);
                                w.global::<crate::ModalHostState>()
                                    .set_profile_contact_name("".into());
                                w.global::<crate::ModalHostState>()
                                    .set_profile_display_name(display_name.into());
                                w.global::<crate::ModalHostState>()
                                    .set_profile_initials(initials.into());
                                show_toast(&w, ("Removed from friends", "success"));
                                tracing::info!("Removed friend: {}", friend_id);
                            }
                        });
                    }
                    Ok(Err(e)) => {
                        tracing::error!("Failed to remove friend: {}", e);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.global::<crate::ModalHostState>()
                                    .set_profile_loading(false);
                                show_toast(&w, (REMOVE_FAILED, "error"));
                            }
                        });
                    }
                    Err(_) => {
                        tracing::error!("Remove friend timed out");
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.global::<crate::ModalHostState>()
                                    .set_profile_loading(false);
                                show_toast(&w, (REMOVE_FAILED, "error"));
                            }
                        });
                    }
                }
            });
        });

    // Block user
    let weak_window = window.as_weak();
    let ui_ctx_block = ui_ctx.clone();

    window
        .global::<crate::ModalHostState>()
        .on_profile_block_user(move || {
            let weak = weak_window.clone();
            if weak.upgrade().is_some_and(|w| {
                w.global::<crate::ModalHostState>()
                    .get_profile_scoped_member()
            }) {
                return;
            }
            let ui_ctx = ui_ctx_block.clone();

            let profile_user_id = weak.upgrade().map(|w| {
                w.global::<crate::ModalHostState>()
                    .get_profile_user_id()
                    .to_string()
            });
            let profile_user_id = match profile_user_id {
                Some(id) if !id.is_empty() => id,
                _ => {
                    tracing::error!("Could not get profile user ID");
                    return;
                }
            };

            tokio::spawn(async move {
                let weak_loading = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_loading.upgrade() {
                        w.global::<crate::ModalHostState>()
                            .set_profile_loading(true);
                    }
                });

                let user_id: UserId = match profile_user_id.parse() {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::error!("Invalid user ID: {}", e);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.global::<crate::ModalHostState>()
                                    .set_profile_loading(false);
                            }
                        });
                        return;
                    }
                };

                match ui_ctx.block_profile_user(&user_id).await {
                    Ok(()) => {
                        crate::bin_ui::features::auth::initial_data::refresh_friends_list(
                            weak.clone(),
                            ui_ctx.clone(),
                        );
                        crate::bin_ui::features::modals::friends::refresh_blocked_users(
                            weak.clone(),
                            &ui_ctx,
                        );
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.global::<crate::ModalHostState>()
                                    .set_profile_loading(false);
                                w.global::<crate::ModalHostState>()
                                    .set_profile_is_blocked(true);
                                w.global::<crate::ModalHostState>()
                                    .set_profile_is_friend(false);
                                // Their DM leaves the sidebar (core hides blocked
                                // contacts); history stays for an unblock.
                                crate::bin_ui::features::sidebar::refresh_chat_list(&w, &ui_ctx);
                                tracing::info!("Blocked user: {}", user_id);
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!("Failed to block user: {}", e);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.global::<crate::ModalHostState>()
                                    .set_profile_loading(false);
                            }
                        });
                    }
                }
            });
        });
}
