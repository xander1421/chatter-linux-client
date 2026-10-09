//! Create channel modal callbacks.

use std::path::PathBuf;
use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::channel::{ChannelConfig, ChannelType, MessagePersistence};
use chatter_core::domain::ids::UserId;
use chatter_core::state::ActiveConversation;

use crate::{AIModelOption, CreateChannelConfig, MainWindow, PrivateGroupFriend};

/// What owned the UI when a create/join request started. A late completion
/// keeps its server result, but navigates or closes the requesting surface only
/// if the same login session, conversation and modal stack are still showing.
pub(super) struct RequestFence {
    owner: UserId,
    session_revision: u64,
    conversation: ActiveConversation,
    stack: Vec<SharedString>,
}

impl RequestFence {
    /// Call on the UI thread when the request is submitted.
    pub(super) fn capture(w: &MainWindow, client: &ChatterClient) -> Option<Self> {
        let auth = client.auth_state().get();
        Some(Self {
            owner: auth.current_user_id?,
            session_revision: auth.session_revision,
            conversation: client.chat_state().active_conversation(),
            stack: w.get_modal_stack().iter().collect(),
        })
    }

    pub(super) fn session_current(&self, client: &ChatterClient) -> bool {
        let auth = client.auth_state().get();
        auth.current_user_id == Some(self.owner) && auth.session_revision == self.session_revision
    }

    /// Call on the UI thread at completion.
    pub(super) fn owns_ui(&self, w: &MainWindow, client: &ChatterClient) -> bool {
        self.session_current(client)
            && client.chat_state().active_conversation() == self.conversation
            && w.get_modal_stack().iter().eq(self.stack.iter().cloned())
    }
}

/// Success the user navigated away from: say so without moving them.
pub(super) fn notify_background_success(w: &MainWindow, message: String) {
    w.set_toast_message(message.into());
    w.set_toast_variant("success".into());
    w.set_toast_visible(true);
}

fn toggle_selected(friend: &mut PrivateGroupFriend) {
    friend.selected = !friend.selected;
    if friend.selected && friend.role.is_empty() {
        friend.role = "member".into();
    }
}

fn toggle_role(friend: &mut PrivateGroupFriend) {
    if friend.selected {
        friend.role = if friend.role == "admin" {
            "member".into()
        } else {
            "admin".into()
        };
    }
}

/// One editor for the create-channel and invite friend lists.
fn edit_private_friend(
    friends: &ModelRc<PrivateGroupFriend>,
    idx: i32,
    edit: fn(&mut PrivateGroupFriend),
) -> i32 {
    if let Some(mut friend) = friends.row_data(idx as usize) {
        edit(&mut friend);
        friends.set_row_data(idx as usize, friend);
    }
    friends.iter().filter(|f| f.selected).count() as i32
}

pub fn wire_create_channel(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ctx = ui_ctx.clone();
    let chat_state = ctx.chat_state().clone();

    // File picker for channel icon — stores the chosen path so on_create can
    // upload it through AttachmentService. Preview decoding is local UI state only.
    let weak_pick = weak_window.clone();
    window.on_create_channel_pick_icon(move || {
        tracing::info!("[CHANNEL_ICON] pick-channel-icon callback fired");
        let weak = weak_pick.clone();
        tokio::spawn(async move {
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            let picked = {
                let config = chatter_core::media::FilePickerConfig::new()
                    .with_title("Select Channel Icon")
                    .with_filter(chatter_core::media::FileFilter::images());
                chatter_core::media::FilePicker::pick_file(config).await
            };
            // Android has no `rfd` backend; go through the SAF bridge (§112p).
            #[cfg(target_os = "android")]
            let picked = crate::platform::android::file_picker::pick_files(&["image/*"])
                .await
                .into_iter()
                .next();
            #[cfg(target_os = "ios")]
            let picked: Option<std::path::PathBuf> = {
                tracing::warn!("File picker not supported on iOS");
                None
            };

            match picked {
                Some(path) => {
                    tracing::info!(path = %path.display(), "[CHANNEL_ICON] file picked");
                    match std::fs::read(&path) {
                        Ok(data) => {
                            let path_string = path.to_string_lossy().to_string();
                            let _ = slint::invoke_from_event_loop(move || {
                                // Stash picked path on the modal's icon-url property so
                                // on_create can read it (thread_local! is tokio-worker-local,
                                // which race-lost the value between pick and create).
                                if let Some(w) = weak.upgrade() {
                                    w.set_create_channel_icon_url(path_string.into());
                                }
                                match crate::bin_ui::slint_conversions::memory_to_slint_image(&data)
                                {
                                    Ok(slint_img) => {
                                        if let Some(w) = weak.upgrade() {
                                            w.set_create_channel_icon_preview(slint_img);
                                            w.set_create_channel_has_icon_preview(true);
                                            tracing::info!("[CHANNEL_ICON] preview set on modal");
                                        }
                                    }
                                    Err(e) => tracing::error!(
                                        "[CHANNEL_ICON] Failed to decode preview: {}",
                                        e
                                    ),
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("[CHANNEL_ICON] Failed to read picked file: {}", e)
                        }
                    }
                }
                None => tracing::info!("[CHANNEL_ICON] file pick cancelled"),
            }
        });
    });

    window.on_create_channel(
        move |name: SharedString,
              description: SharedString,
              template: SharedString,
              config: CreateChannelConfig,
              ai_provider: SharedString,
              ai_model: SharedString| {
            let weak = weak_window.clone();
            let ui_ctx = ctx.clone();
            let chat_state = chat_state.clone();
            let name = name.to_string();
            let description = description.to_string();
            let template = template.to_string();
            let ai_provider = ai_provider.to_string();
            let ai_model = ai_model.to_string();

            let is_private = config.is_private;
            let is_ephemeral = config.is_ephemeral;
            let ttl_hours = config.ttl_hours;
            let max_members = config.max_members;
            let password = config.password.to_string();

            // Icon path comes in via config.icon_url (set by the file picker).
            let icon_path_str = config.icon_url.to_string();
            let pending_icon: Option<PathBuf> = if icon_path_str.is_empty() {
                None
            } else {
                Some(PathBuf::from(&icon_path_str))
            };
            tracing::info!(has_icon = pending_icon.is_some(), "[CHANNEL_ICON] on_create_channel entry");
            let Some(fence) = weak
                .upgrade()
                .and_then(|w| RequestFence::capture(&w, &ui_ctx))
            else {
                return;
            };

            tokio::spawn(async move {
                let weak_loading = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_loading.upgrade() {
                        w.set_create_channel_loading(true);
                        w.set_create_channel_error("".into());
                    }
                });

                let channel_type = if is_private { ChannelType::Private } else { ChannelType::Public };
                let description_opt = if description.is_empty() { None } else { Some(description.as_str()) };
                let password_opt = if password.is_empty() { None } else { Some(password.as_str()) };

                let domain_config = ChannelConfig {
                    persistence: if is_ephemeral { MessagePersistence::Ephemeral } else { MessagePersistence::Permanent },
                    max_members: if max_members > 0 { Some(max_members as u32) } else { None },
                    default_ttl_seconds: if is_ephemeral { Some((ttl_hours as u32) * 3600) } else { None },
                    require_encryption: true,
                    ..Default::default()
                };

                // Core uploads the icon to CDN internally (if provided) and
                // creates the channel atomically. If upload fails, no channel
                // is created and the error surfaces here.
                match ui_ctx
                    .create_channel_with_icon(
                        &name,
                        channel_type,
                        Some(domain_config),
                        description_opt,
                        pending_icon.as_deref(),
                        password_opt,
                        vec![],
                    )
                    .await
                {
                    Ok(channel) => {
                        let channel_id = channel.id;
                        let channel_id_str: SharedString = channel_id.to_string().into();
                        let channel_name = channel.name.clone();

                        // FEAT-5: this branch is unreachable in a default build
                        // — the "AI assistant" template is gated off on
                        // `AiIntegrationGlobal.channel-template-enabled`, which
                        // mirrors the off-by-default `ai-channel-template` Cargo
                        // feature. It is kept, and correct, for the build that
                        // turns it on.
                        //
                        // The failure used to be swallowed by `tracing::error!`,
                        // so the channel was created, the modal closed, and the
                        // user was told nothing. It always failed: the server
                        // requires `secrets["api_key"]`
                        // (internal/services/ai/providers/factory.go:85) and
                        // nothing on this path can supply one. The channel is
                        // real by this point, so the honest report is a visible
                        // error saying the bot was not added — not silence, and
                        // not an aborted channel.
                        if template == "ai-assistant" && !ai_provider.is_empty() && !ai_model.is_empty() {
                            if let Err(e) = ui_ctx.create_ai_integration(
                                &format!("{} Assistant", name),
                                &ai_provider,
                                &ai_model,
                                &channel_id.to_string(),
                            ).await {
                                tracing::error!(error = %e, "Failed to create AI integration");
                                let detail = e.to_string();
                                let weak_toast = weak.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(w) = weak_toast.upgrade() {
                                        w.set_toast_message(
                                            format!(
                                                "Channel created, but the AI assistant was not added: {detail}"
                                            )
                                            .into(),
                                        );
                                        w.set_toast_variant("error".into());
                                        w.set_toast_visible(true);
                                    }
                                });
                            }
                        }

                        let weak_modal = weak.clone();
                        let channel_id_for_ui = channel_id_str.clone();
                        // The new channel's row comes from core, like every row.
                        crate::bin_ui::features::sidebar::rebuild_chat_list(&weak, &ui_ctx);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak_modal.upgrade() {
                                if !fence.session_current(&ui_ctx) {
                                    return;
                                }
                                w.set_create_channel_loading(false);
                                if !fence.owns_ui(&w, &ui_ctx) {
                                    notify_background_success(
                                        &w,
                                        format!("Channel \"{channel_name}\" created."),
                                    );
                                    return;
                                }
                                chat_state.set_active_channel(channel_id);
                                w.set_create_channel_name("".into());
                                w.set_create_channel_description("".into());
                                w.set_create_channel_icon_url("".into());
                                w.set_create_channel_has_icon_preview(false);
                                crate::bin_ui::features::back::close(&w, "create-channel");
                                w.invoke_channel_selected(channel_id_for_ui);
                            }
                        });

                        // channel_selected owns this history load.
                    }
                    Err(e) => {
                        let error_msg = e.to_string();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                if !fence.session_current(&ui_ctx) {
                                    return;
                                }
                                w.set_create_channel_loading(false);
                                w.set_create_channel_error(error_msg.into());
                            }
                        });
                    }
                }
            });
        },
    );

    let weak_window = window.as_weak();
    let ui_ctx_ai = ui_ctx.clone();
    window.on_create_channel_provider_selected(move |provider: SharedString| {
        let weak = weak_window.clone();
        let ui_ctx_ai = ui_ctx_ai.clone();
        let provider_str = provider.to_string();
        tokio::spawn(async move {
            if !matches!(provider_str.as_str(), "claude" | "openai") {
                return;
            }

            // Set loading state and clear previous error/models
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_create_channel_ai_models_loading(true);
                    w.set_create_channel_ai_models_error("".into());
                    w.set_create_channel_ai_models(ModelRc::new(VecModel::default()));
                }
            });

            match ui_ctx_ai.list_ai_models(&provider_str).await {
                Ok(available_models) => {
                    let models: Vec<AIModelOption> = available_models.into_iter().map(|(id, name)| AIModelOption { id: id.into(), name: name.into(), provider: provider_str.clone().into() }).collect();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_create_channel_ai_models(ModelRc::new(VecModel::from(models)));
                            w.set_create_channel_ai_models_loading(false);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!(error = %e, provider = %provider_str, "Failed to load AI models");
                    let error_msg = format!("Failed to load models: {e}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_create_channel_ai_models_loading(false);
                            w.set_create_channel_ai_models_error(error_msg.into());
                        }
                    });
                }
            }
        });
    });

    let weak_window = window.as_weak();
    window.on_toggle_friend_selection(move |idx: i32| {
        let weak = weak_window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let friends = w.get_create_channel_available_friends();
                let selected_count = edit_private_friend(&friends, idx, toggle_selected);
                w.set_create_channel_selected_friend_count(selected_count);
            }
        });
    });

    // Toggle friend role between member and admin
    let weak_window = window.as_weak();
    window.on_toggle_friend_role(move |idx: i32| {
        let weak = weak_window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let friends = w.get_create_channel_available_friends();
                edit_private_friend(&friends, idx, toggle_role);
            }
        });
    });

    let weak_window = window.as_weak();
    let ui_ctx_group = ui_ctx.clone();
    window.on_create_private_group(
        move |name: SharedString, friends: ModelRc<PrivateGroupFriend>| {
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx_group.clone();
            let name = name.to_string();

            // Extract selected friends with their roles
            let selected_friends: Vec<(UserId, chatter_core::domain::anon_group::AnonGroupRole)> =
                friends
                    .iter()
                    .filter(|f| f.selected)
                    .filter_map(|f| {
                        let user_id = f.user_id.to_string().parse::<UserId>().ok()?;
                        let role = if f.role == "admin" {
                            chatter_core::domain::anon_group::AnonGroupRole::Admin
                        } else {
                            chatter_core::domain::anon_group::AnonGroupRole::Member
                        };
                        Some((user_id, role))
                    })
                    .collect();

            if selected_friends.is_empty() {
                return;
            }
            let Some(fence) = weak
                .upgrade()
                .and_then(|w| RequestFence::capture(&w, &ui_ctx))
            else {
                return;
            };

            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_create_channel_loading(true);
                    w.set_create_channel_error("".into());
                }
            });

            tokio::spawn(async move {
                match ui_ctx
                    .create_anon_group(name.clone(), selected_friends)
                    .await
                {
                    Ok(_group) => {
                        crate::bin_ui::features::sidebar::rebuild_chat_list(&weak, &ui_ctx);
                        let weak_success = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak_success.upgrade() {
                                if !fence.session_current(&ui_ctx) {
                                    return;
                                }
                                w.set_create_channel_loading(false);
                                if !fence.owns_ui(&w, &ui_ctx) {
                                    notify_background_success(
                                        &w,
                                        format!("Private group \"{name}\" created."),
                                    );
                                    return;
                                }
                                crate::bin_ui::features::back::close(&w, "create-channel");
                            }
                        });
                    }
                    Err(e) => {
                        let error_msg = e.to_string();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                if !fence.session_current(&ui_ctx) {
                                    return;
                                }
                                w.set_create_channel_loading(false);
                                w.set_create_channel_error(error_msg.into());
                            }
                        });
                    }
                }
            });
        },
    );

    let weak_window = window.as_weak();
    let ui_ctx_tpl = ui_ctx.clone();
    window.on_create_channel_template_selected(move |template: SharedString| {
        if template.as_str() != "private-group" {
            return;
        }
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_tpl.clone();
        tokio::spawn(async move {
            let candidates: Vec<_> = ui_ctx
                .list_anon_group_invite_candidates(None)
                .into_iter()
                .map(|c| {
                    let avatar = crate::bin_ui::converters::invite_candidate_avatar(&ui_ctx, &c);
                    (c, avatar)
                })
                .collect();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    let friend_data: Vec<PrivateGroupFriend> = candidates
                        .into_iter()
                        .map(|(c, avatar)| {
                            crate::bin_ui::converters::invite_candidate_to_slint(c, avatar)
                        })
                        .collect();
                    w.set_create_channel_available_friends(ModelRc::new(VecModel::from(
                        friend_data,
                    )));
                }
            });
        });
    });
}

/// Wire the invite to private group modal callbacks.
pub fn wire_invite_to_private_group(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Toggle selection for invite modal
    let weak_window = window.as_weak();
    window.on_toggle_invite_friend_selection(move |idx: i32| {
        let weak = weak_window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let friends = w.get_invite_private_group_friends();
                let selected_count = edit_private_friend(&friends, idx, toggle_selected);
                w.set_invite_private_group_selected_count(selected_count);
            }
        });
    });

    // Toggle role for invite modal (member/admin)
    let weak_window = window.as_weak();
    window.on_toggle_invite_friend_role(move |idx: i32| {
        let weak = weak_window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let friends = w.get_invite_private_group_friends();
                edit_private_friend(&friends, idx, toggle_role);
            }
        });
    });

    // Submit invite
    let weak_window = window.as_weak();
    let ui_ctx_invite = ui_ctx.clone();
    window.on_invite_to_private_group(move |friends: ModelRc<PrivateGroupFriend>| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_invite.clone();

        let group_id_str = weak
            .upgrade()
            .map(|w| w.get_invite_private_group_id().to_string())
            .unwrap_or_default();

        // Extract selected friends with their roles
        let selected_friends: Vec<(UserId, chatter_core::domain::anon_group::AnonGroupRole)> =
            friends
                .iter()
                .filter(|f| f.selected)
                .filter_map(|f| {
                    let user_id = f.user_id.to_string().parse::<UserId>().ok()?;
                    let role = if f.role == "admin" {
                        chatter_core::domain::anon_group::AnonGroupRole::Admin
                    } else {
                        chatter_core::domain::anon_group::AnonGroupRole::Member
                    };
                    Some((user_id, role))
                })
                .collect();

        if selected_friends.is_empty() || group_id_str.is_empty() {
            return;
        }

        // Set loading state
        let weak_loading = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak_loading.upgrade() {
                w.set_invite_private_group_loading(true);
            }
        });

        tokio::spawn(async move {
            let group_id: chatter_core::domain::anon_group::AnonGroupId = match group_id_str.parse()
            {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid group ID for invite: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_invite_private_group_loading(false);
                        }
                    });
                    return;
                }
            };

            match ui_ctx
                .invite_to_anon_group(&group_id, selected_friends)
                .await
            {
                Ok(_) => {
                    tracing::info!("Successfully invited members to group {}", group_id);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_invite_private_group_loading(false);
                            crate::bin_ui::features::back::close(&w, "create-channel");
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to invite members: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_invite_private_group_loading(false);
                        }
                    });
                }
            }
        });
    });
}
