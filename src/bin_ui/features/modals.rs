//! Modal callbacks.
//!
//! Handles modal dialogs (channel creation, profile actions, friend requests,
//! settings, sessions, join channel).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tokio::time::{sleep, Duration};

use chatter_linux::app::AppContext;
use chatter_linux::domain::channel::ChannelType;
use chatter_linux::domain::ids::{ChannelId, UserId};
use chatter_linux::network::grpc::IntegrationClient;
use chatter_linux::proto::chat_v1::IntegrationType;
use chatter_linux::services::ai_service::AIService;
use chatter_linux::services::channel_service::ChannelService;
use chatter_linux::services::user_service::UserService;

use crate::bin_ui::converters::channel_to_slint;
use crate::bin_ui::features::sidebar::load_channel_into_ui;
use crate::{ChannelListItemData, CreateChannelConfig, MainWindow, SearchResultChannel, SessionInfo};

/// Wire modal callbacks.
pub fn wire(window: &MainWindow, ctx: &AppContext) {
    wire_create_channel(window, ctx);
    wire_join_channel(window, ctx);
    wire_profile_actions(window, ctx);
    wire_friend_requests(window, ctx);
    wire_settings_callbacks(window, ctx);
    wire_session_callbacks(window, ctx);
}

fn wire_create_channel(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let grpc_pool = ctx.grpc_pool.clone();
    let settings_repo = ctx.settings_repo.clone();
    let channel_repo = ctx.channel_repo.clone();
    let message_repo = ctx.message_repo.clone();
    let user_repo = ctx.user_repo.clone();
    let attachment_repo = ctx.attachment_repo.clone();
    let chat_state = ctx.chat_state.clone();
    let signal_service = ctx.signal_service.clone();

    window.on_create_channel(
        move |name: SharedString,
              description: SharedString,
              template: SharedString,
              config: CreateChannelConfig,
              ai_provider: SharedString,
              ai_model: SharedString| {
            let weak = weak_window.clone();
            let grpc_pool = grpc_pool.clone();
            let settings_repo = settings_repo.clone();
            let channel_repo = channel_repo.clone();
            let message_repo = message_repo.clone();
            let user_repo = user_repo.clone();
            let attachment_repo = attachment_repo.clone();
            let chat_state = chat_state.clone();
            let signal_service = signal_service.clone();
            let name = name.to_string();
            let description = description.to_string();
            let template = template.to_string();
            let ai_provider = ai_provider.to_string();
            let ai_model = ai_model.to_string();

            // Extract config values
            let is_private = config.is_private;
            let is_encrypted = config.is_encrypted;
            let is_ephemeral = config.is_ephemeral;
            let ttl_hours = config.ttl_hours;
            let max_members = config.max_members;
            let password = config.password.to_string();
            let _friends_only = config.friends_only; // TODO: pass to backend when API supports it

            tokio::spawn(async move {
                let weak_loading = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_loading.upgrade() {
                        w.set_create_channel_loading(true);
                        w.set_create_channel_error("".into());
                    }
                });

                // Determine channel type from config
                let channel_type = if is_private {
                    ChannelType::Private
                } else {
                    ChannelType::Public
                };

                let channel_service = ChannelService::new(settings_repo.clone(), channel_repo.clone(), signal_service);

                let mut channel_client =
                    chatter_linux::network::grpc::channel_client::ChannelClient::new(
                        grpc_pool.channel(),
                        grpc_pool.auth_clone(),
                    );

                let description_opt = if description.is_empty() {
                    None
                } else {
                    Some(description.as_str())
                };

                let password_opt = if password.is_empty() {
                    None
                } else {
                    Some(password.as_str())
                };

                // Build domain ChannelConfig from UI config
                let domain_config = chatter_linux::domain::channel::ChannelConfig {
                    persistence: if is_ephemeral {
                        chatter_linux::domain::channel::MessagePersistence::Ephemeral
                    } else {
                        chatter_linux::domain::channel::MessagePersistence::Permanent
                    },
                    max_members: if max_members > 0 {
                        Some(max_members as u32)
                    } else {
                        None
                    },
                    default_ttl_seconds: if is_ephemeral {
                        Some((ttl_hours as u32) * 3600)
                    } else {
                        None
                    },
                    allow_reactions: true,
                    allow_threads: true,
                    allow_attachments: true,
                    require_encryption: is_encrypted,
                };

                match channel_service
                    .create_channel(
                        &mut channel_client,
                        &name,
                        channel_type,
                        Some(domain_config),
                        description_opt,
                        None,
                        password_opt,
                        vec![],
                    )
                    .await
                {
                    Ok(channel) => {
                        let channel_id = channel.id;
                        let channel_id_str: SharedString = channel_id.to_string().into();

                        // If AI template, create and attach integration
                        if template == "ai-assistant" && !ai_provider.is_empty() && !ai_model.is_empty() {
                            let ai_service = AIService::new(settings_repo.clone());
                            let mut integration_client = IntegrationClient::new(
                                grpc_pool.channel(),
                                grpc_pool.auth_clone(),
                            );

                            // Convert provider string to IntegrationType
                            let provider_type = match ai_provider.as_str() {
                                "claude" => IntegrationType::Claude,
                                "openai" => IntegrationType::Openai,
                                "ollama" => IntegrationType::Custom, // Ollama uses custom
                                _ => IntegrationType::Custom,
                            };

                            // Create the integration
                            let integration_name = format!("{} Assistant", name);
                            match ai_service
                                .create_integration(
                                    &mut integration_client,
                                    &integration_name,
                                    provider_type,
                                    Some(chatter_linux::proto::chat_v1::IntegrationConfig {
                                        model: ai_model.clone(),
                                        ..Default::default()
                                    }),
                                )
                                .await
                            {
                                Ok(integration) => {
                                    // Attach integration to channel
                                    match ai_service
                                        .add_integration_to_channel(
                                            &mut integration_client,
                                            &integration.id,
                                            &channel_id.to_string(),
                                        )
                                        .await
                                    {
                                        Ok(()) => {
                                            tracing::info!(
                                                "AI integration {} attached to channel {}",
                                                integration.id,
                                                channel_id
                                            );
                                        }
                                        Err(e) => {
                                            tracing::error!(
                                                "Failed to attach AI integration to channel: {}",
                                                e
                                            );
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::error!("Failed to create AI integration: {}", e);
                                }
                            }
                        }

                        // Set the newly created channel as active
                        chat_state.set_active_channel(channel_id);

                        // Convert channel to Slint format for sidebar
                        let slint_channel = channel_to_slint(&channel);

                        // Close modal and add channel to sidebar
                        let weak_modal = weak.clone();
                        let channel_id_for_ui = channel_id_str.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak_modal.upgrade() {
                                // Close modal and clear form
                                w.set_create_channel_loading(false);
                                w.set_create_channel_name("".into());
                                w.set_create_channel_description("".into());
                                w.set_create_channel_template("".into());
                                w.set_create_channel_ai_provider("".into());
                                w.set_create_channel_ai_model("".into());
                                w.set_active_modal("".into());

                                // Add new channel to sidebar list
                                let current_channels = w.get_channels();
                                let mut channels: Vec<ChannelListItemData> = (0..current_channels.row_count())
                                    .filter_map(|i| current_channels.row_data(i))
                                    .collect();
                                channels.push(slint_channel);
                                w.set_channels(ModelRc::new(VecModel::from(channels)));

                                // Set loading state for channel switch
                                w.set_loading_messages(true);
                                w.set_selected_channel_id(channel_id_for_ui);
                                w.set_selected_dm_id("".into());
                            }
                        });

                        // Load the new channel properly (clears messages, sets header)
                        load_channel_into_ui(
                            weak,
                            &channel_id,
                            channel_repo.clone(),
                            message_repo.clone(),
                            user_repo.clone(),
                            attachment_repo.clone(),
                        )
                        .await;

                        tracing::info!("Created channel: {}", channel_id);
                    }
                    Err(e) => {
                        let error_msg = e.to_string();
                        tracing::error!("Failed to create channel: {}", error_msg);

                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_create_channel_loading(false);
                                w.set_create_channel_error(error_msg.into());
                            }
                        });
                    }
                }
            });
        },
    );

    // AI provider selected callback - loads models from server
    let weak_window = window.as_weak();
    let grpc_pool = ctx.grpc_pool.clone();
    let settings_repo = ctx.settings_repo.clone();

    window.on_create_channel_provider_selected(move |provider: SharedString| {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool.clone();
        let settings_repo = settings_repo.clone();
        let provider_str = provider.to_string();

        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create runtime for provider selection");

            rt.block_on(async {
                // Convert provider string to IntegrationType
                let provider_type = match provider_str.as_str() {
                    "claude" => IntegrationType::Claude,
                    "openai" => IntegrationType::Openai,
                    "ollama" => IntegrationType::Custom,
                    _ => {
                        // Unknown provider, clear models
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_create_channel_ai_models(ModelRc::new(VecModel::from(Vec::new())));
                            }
                        });
                        return;
                    }
                };

                // Create AI service and client
                let ai_service = AIService::new(settings_repo);
                let mut integration_client = IntegrationClient::new(
                    grpc_pool.channel(),
                    grpc_pool.auth_clone(),
                );

                // Fetch models from server
                match ai_service.list_available_models(&mut integration_client, provider_type).await {
                    Ok(available_models) => {
                        use crate::AIModelOption;
                        let models: Vec<AIModelOption> = available_models
                            .into_iter()
                            .map(|m| AIModelOption {
                                id: m.id.into(),
                                name: m.display_name.into(),
                                provider: provider_str.clone().into(),
                            })
                            .collect();

                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_create_channel_ai_models(ModelRc::new(VecModel::from(models)));
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("Failed to fetch models for provider {}: {}", provider_str, e);
                        // Fall back to static models for common providers
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                use crate::AIModelOption;
                                let fallback_models = match provider_str.as_str() {
                                    "claude" => vec![
                                        AIModelOption {
                                            id: "claude-3-opus".into(),
                                            name: "Claude 3 Opus".into(),
                                            provider: "claude".into(),
                                        },
                                        AIModelOption {
                                            id: "claude-3-sonnet".into(),
                                            name: "Claude 3 Sonnet".into(),
                                            provider: "claude".into(),
                                        },
                                        AIModelOption {
                                            id: "claude-3-haiku".into(),
                                            name: "Claude 3 Haiku".into(),
                                            provider: "claude".into(),
                                        },
                                    ],
                                    "openai" => vec![
                                        AIModelOption {
                                            id: "gpt-4".into(),
                                            name: "GPT-4".into(),
                                            provider: "openai".into(),
                                        },
                                        AIModelOption {
                                            id: "gpt-4-turbo".into(),
                                            name: "GPT-4 Turbo".into(),
                                            provider: "openai".into(),
                                        },
                                        AIModelOption {
                                            id: "gpt-3.5-turbo".into(),
                                            name: "GPT-3.5 Turbo".into(),
                                            provider: "openai".into(),
                                        },
                                    ],
                                    "ollama" => vec![
                                        AIModelOption {
                                            id: "llama2".into(),
                                            name: "Llama 2".into(),
                                            provider: "ollama".into(),
                                        },
                                        AIModelOption {
                                            id: "mistral".into(),
                                            name: "Mistral".into(),
                                            provider: "ollama".into(),
                                        },
                                    ],
                                    _ => vec![],
                                };
                                w.set_create_channel_ai_models(ModelRc::new(VecModel::from(fallback_models)));
                            }
                        });
                    }
                }
            });
        });
    });
}

fn wire_join_channel(window: &MainWindow, ctx: &AppContext) {
    // Debounce counter for search
    let search_counter = Arc::new(AtomicU64::new(0));

    // Search channels callback (with debounce)
    let weak_window = window.as_weak();
    let grpc_pool = ctx.grpc_pool.clone();
    let settings_repo = ctx.settings_repo.clone();
    let channel_repo = ctx.channel_repo.clone();
    let signal_service = ctx.signal_service.clone();
    let search_counter_clone = search_counter.clone();

    window.on_search_channels(move |query: SharedString| {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool.clone();
        let settings_repo = settings_repo.clone();
        let channel_repo = channel_repo.clone();
        let signal_service = signal_service.clone();
        let query = query.to_string();
        let counter = search_counter_clone.clone();

        // Increment counter for this search
        let search_id = counter.fetch_add(1, Ordering::SeqCst) + 1;

        tokio::spawn(async move {
            // Debounce: wait 300ms before searching
            sleep(Duration::from_millis(300)).await;

            // Check if a newer search was triggered
            if counter.load(Ordering::SeqCst) != search_id {
                return;
            }

            // Empty query - clear results
            if query.trim().is_empty() {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_join_channel_results(ModelRc::new(VecModel::from(
                            Vec::<SearchResultChannel>::new(),
                        )));
                        w.set_join_channel_searching(false);
                        w.set_join_channel_error("".into());
                    }
                });
                return;
            }

            // Set searching state
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_join_channel_searching(true);
                    w.set_join_channel_error("".into());
                }
            });

            // Check again after setting loading state
            if counter.load(Ordering::SeqCst) != search_id {
                return;
            }

            // Perform search
            let channel_service = ChannelService::new(settings_repo, channel_repo, signal_service);
            let mut channel_client =
                chatter_linux::network::grpc::channel_client::ChannelClient::new(
                    grpc_pool.channel(),
                    grpc_pool.auth_clone(),
                );

            match channel_service
                .search_channels(
                    &mut channel_client,
                    &query,
                    20,
                    0,
                    vec![ChannelType::Public], // Only public channels
                )
                .await
            {
                Ok(channels) => {
                    // Check if still the latest search
                    if counter.load(Ordering::SeqCst) != search_id {
                        return;
                    }

                    let results: Vec<SearchResultChannel> = channels
                        .into_iter()
                        .map(|ch| SearchResultChannel {
                            id: ch.id.to_string().into(),
                            name: ch.name.into(),
                            description: ch.description.unwrap_or_default().into(),
                            member_count: 0, // Would need additional API call
                        })
                        .collect();

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_join_channel_results(ModelRc::new(VecModel::from(results)));
                            w.set_join_channel_searching(false);
                        }
                    });
                }
                Err(e) => {
                    if counter.load(Ordering::SeqCst) != search_id {
                        return;
                    }

                    let error_msg = format!("Search failed: {}", e);
                    tracing::error!("{}", error_msg);

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_join_channel_searching(false);
                            w.set_join_channel_error(error_msg.into());
                        }
                    });
                }
            }
        });
    });

    // Join channel callback
    let weak_window = window.as_weak();
    let grpc_pool = ctx.grpc_pool.clone();
    let settings_repo = ctx.settings_repo.clone();
    let channel_repo = ctx.channel_repo.clone();
    let message_repo = ctx.message_repo.clone();
    let user_repo = ctx.user_repo.clone();
    let attachment_repo = ctx.attachment_repo.clone();
    let chat_state = ctx.chat_state.clone();
    let signal_service = ctx.signal_service.clone();

    window.on_join_channel(move |channel_id_str: SharedString| {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool.clone();
        let settings_repo = settings_repo.clone();
        let channel_repo = channel_repo.clone();
        let message_repo = message_repo.clone();
        let user_repo = user_repo.clone();
        let attachment_repo = attachment_repo.clone();
        let chat_state = chat_state.clone();
        let signal_service = signal_service.clone();
        let channel_id_str = channel_id_str.to_string();

        tokio::spawn(async move {
            // Set joining state
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_join_channel_joining(true);
                    w.set_join_channel_error("".into());
                }
            });

            // Parse channel ID
            let channel_id: ChannelId = match channel_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    let error_msg = format!("Invalid channel ID: {}", e);
                    tracing::error!("{}", error_msg);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_join_channel_joining(false);
                            w.set_join_channel_error(error_msg.into());
                        }
                    });
                    return;
                }
            };

            // Join channel
            let channel_service = ChannelService::new(settings_repo, channel_repo.clone(), signal_service);
            let mut channel_client =
                chatter_linux::network::grpc::channel_client::ChannelClient::new(
                    grpc_pool.channel(),
                    grpc_pool.auth_clone(),
                );

            match channel_service
                .join_channel(&mut channel_client, &channel_id, None)
                .await
            {
                Ok(channel) => {
                    let channel_id = channel.id;
                    let channel_id_str: SharedString = channel_id.to_string().into();

                    // Set the joined channel as active
                    chat_state.set_active_channel(channel_id);

                    // Convert channel to Slint format for sidebar
                    let slint_channel = channel_to_slint(&channel);

                    // Close modal and add channel to sidebar
                    let weak_modal = weak.clone();
                    let channel_id_for_ui = channel_id_str.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_modal.upgrade() {
                            // Close modal and clear form
                            w.set_join_channel_joining(false);
                            w.set_join_channel_search_query("".into());
                            w.set_join_channel_selected_id("".into());
                            w.set_join_channel_results(ModelRc::new(VecModel::from(
                                Vec::<SearchResultChannel>::new(),
                            )));
                            w.set_active_modal("".into());

                            // Add joined channel to sidebar list
                            let current_channels = w.get_channels();
                            let mut channels: Vec<ChannelListItemData> = (0..current_channels.row_count())
                                .filter_map(|i| current_channels.row_data(i))
                                .collect();
                            channels.push(slint_channel);
                            w.set_channels(ModelRc::new(VecModel::from(channels)));

                            // Set loading state for channel switch
                            w.set_loading_messages(true);
                            w.set_selected_channel_id(channel_id_for_ui);
                            w.set_selected_dm_id("".into());
                        }
                    });

                    // Load the joined channel properly (loads messages, sets header)
                    load_channel_into_ui(
                        weak,
                        &channel_id,
                        channel_repo,
                        message_repo,
                        user_repo,
                        attachment_repo,
                    )
                    .await;

                    tracing::info!("Joined channel: {}", channel_id);
                }
                Err(e) => {
                    let error_msg = if e.to_string().contains("not found") {
                        "Channel not found or is private".to_string()
                    } else {
                        format!("Failed to join: {}", e)
                    };
                    tracing::error!("Failed to join channel: {}", e);

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_join_channel_joining(false);
                            w.set_join_channel_error(error_msg.into());
                        }
                    });
                }
            }
        });
    });
}

fn wire_profile_actions(window: &MainWindow, ctx: &AppContext) {
    let grpc_pool = ctx.grpc_pool.clone();
    let settings_repo = ctx.settings_repo.clone();
    let user_repo = ctx.user_repo.clone();

    // Add friend
    let weak_window = window.as_weak();
    let grpc_pool_add = grpc_pool.clone();
    let settings_repo_add = settings_repo.clone();
    let user_repo_add = user_repo.clone();

    window.on_profile_add_friend(move || {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_add.clone();
        let settings_repo = settings_repo_add.clone();
        let user_repo = user_repo_add.clone();

        let profile_user_id = weak.upgrade().map(|w| w.get_profile_user_id().to_string());
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
                    w.set_profile_loading(true);
                }
            });

            let friend_id: UserId = match profile_user_id.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid user ID: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                    return;
                }
            };

            let user_service = UserService::new(settings_repo, user_repo);
            let mut user_client = chatter_linux::network::grpc::user_client::UserClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match user_service
                .send_friend_request(&mut user_client, &friend_id, None)
                .await
            {
                Ok(_) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                            tracing::info!("Friend request sent to {}", friend_id);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to send friend request: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                }
            }
        });
    });

    // Remove friend
    let weak_window = window.as_weak();
    let grpc_pool_remove = grpc_pool.clone();
    let settings_repo_remove = settings_repo.clone();
    let user_repo_remove = user_repo.clone();

    window.on_profile_remove_friend(move || {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_remove.clone();
        let settings_repo = settings_repo_remove.clone();
        let user_repo = user_repo_remove.clone();

        let profile_user_id = weak.upgrade().map(|w| w.get_profile_user_id().to_string());
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
                    w.set_profile_loading(true);
                }
            });

            let friend_id: UserId = match profile_user_id.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid user ID: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                    return;
                }
            };

            let user_service = UserService::new(settings_repo, user_repo);
            let mut user_client = chatter_linux::network::grpc::user_client::UserClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match user_service.remove_friend(&mut user_client, &friend_id).await {
                Ok(()) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                            w.set_profile_is_friend(false);
                            tracing::info!("Removed friend: {}", friend_id);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to remove friend: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                }
            }
        });
    });

    // Block user
    let weak_window = window.as_weak();
    let grpc_pool_block = grpc_pool.clone();
    let settings_repo_block = settings_repo.clone();
    let user_repo_block = user_repo.clone();

    window.on_profile_block_user(move || {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_block.clone();
        let settings_repo = settings_repo_block.clone();
        let user_repo = user_repo_block.clone();

        let profile_user_id = weak.upgrade().map(|w| w.get_profile_user_id().to_string());
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
                    w.set_profile_loading(true);
                }
            });

            let user_id: UserId = match profile_user_id.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid user ID: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                    return;
                }
            };

            let user_service = UserService::new(settings_repo, user_repo);
            let mut user_client = chatter_linux::network::grpc::user_client::UserClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match user_service.block_user(&mut user_client, &user_id).await {
                Ok(()) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                            w.set_profile_is_blocked(true);
                            w.set_profile_is_friend(false);
                            tracing::info!("Blocked user: {}", user_id);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to block user: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                }
            }
        });
    });

    // Unblock user
    let weak_window = window.as_weak();
    let grpc_pool_unblock = grpc_pool.clone();
    let settings_repo_unblock = settings_repo.clone();
    let user_repo_unblock = user_repo.clone();

    window.on_profile_unblock_user(move || {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_unblock.clone();
        let settings_repo = settings_repo_unblock.clone();
        let user_repo = user_repo_unblock.clone();

        let profile_user_id = weak.upgrade().map(|w| w.get_profile_user_id().to_string());
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
                    w.set_profile_loading(true);
                }
            });

            let user_id: UserId = match profile_user_id.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid user ID: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                    return;
                }
            };

            let user_service = UserService::new(settings_repo, user_repo);
            let mut user_client = chatter_linux::network::grpc::user_client::UserClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match user_service.unblock_user(&mut user_client, &user_id).await {
                Ok(()) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                            w.set_profile_is_blocked(false);
                            tracing::info!("Unblocked user: {}", user_id);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to unblock user: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                }
            }
        });
    });

    // Send message (open DM)
    let weak_window = window.as_weak();
    let mailbox_manager = ctx.mailbox_manager.clone();
    let user_repo_dm = user_repo.clone();
    let chat_state = ctx.chat_state.clone();

    window.on_profile_send_message(move || {
        let weak = weak_window.clone();
        let mailbox_manager = mailbox_manager.clone();
        let user_repo = user_repo_dm.clone();
        let _chat_state = chat_state.clone(); // Reserved for future DM state tracking

        // Get profile user ID synchronously before spawning
        let profile_user_id = weak
            .upgrade()
            .map(|w| w.get_profile_user_id().to_string())
            .unwrap_or_default();

        if profile_user_id.is_empty() {
            tracing::error!("No profile user ID for DM");
            return;
        }

        tokio::spawn(async move {
            // Show loading state and close modal
            let weak_loading = weak.clone();
            let profile_id_for_ui = profile_user_id.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_active_modal("".into());
                    w.set_loading_messages(true);
                }
            });

            // Parse user ID
            let peer_id: UserId = match profile_user_id.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid user ID for DM: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_loading_messages(false);
                        }
                    });
                    return;
                }
            };

            // Use peer_id as conversation_id for 1:1 DMs
            let conversation_id = peer_id.to_string();

            // Initialize mailbox manager if needed
            if let Err(e) = mailbox_manager.initialize().await {
                tracing::warn!("Failed to initialize mailbox manager: {} (may already be initialized)", e);
            }

            // Ensure we have tokens available
            if mailbox_manager.token_count().await == 0 {
                if let Err(e) = mailbox_manager.acquire_tokens(5).await {
                    tracing::error!("Failed to acquire mailbox tokens: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_loading_messages(false);
                        }
                    });
                    return;
                }
            }

            // Get or create mailbox for this DM conversation
            // TTL: 7 days (604800 seconds)
            match mailbox_manager.get_or_create_mailbox(&conversation_id, 604800).await {
                Ok(mailbox) => {
                    tracing::info!(
                        "DM mailbox ready for conversation with {}: mailbox_id={}",
                        peer_id,
                        mailbox.mailbox_id
                    );

                    // Get peer display name
                    let peer_name = user_repo
                        .get_profile(&peer_id)
                        .ok()
                        .flatten()
                        .map(|p| p.display_name)
                        .or_else(|| {
                            user_repo
                                .get(&peer_id)
                                .ok()
                                .flatten()
                                .map(|u| u.username)
                        })
                        .unwrap_or_else(|| "Unknown".to_string());

                    // Update chat state to active DM
                    // Note: For DMs, we use the peer_id as a pseudo-channel ID
                    // A proper implementation would have a DM-specific state

                    // Update UI to show the DM
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_loading_messages(false);
                            w.set_selected_dm_id(profile_id_for_ui.into());
                            w.set_selected_channel_id("".into());
                            w.set_channel_name(peer_name.into());
                            w.set_channel_type("dm".into());
                            w.set_member_count(2);
                            w.set_topic("".into());
                            // Clear messages for new conversation
                            use slint::ModelRc;
                            use slint::VecModel;
                            let empty: Vec<crate::MessageItemData> = vec![];
                            w.set_messages(ModelRc::new(VecModel::from(empty)));
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to create DM mailbox: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_loading_messages(false);
                        }
                    });
                }
            }
        });
    });
}

fn wire_friend_requests(window: &MainWindow, ctx: &AppContext) {
    let grpc_pool = ctx.grpc_pool.clone();
    let settings_repo = ctx.settings_repo.clone();
    let user_repo = ctx.user_repo.clone();

    // Switch tab
    let weak_window = window.as_weak();
    window.on_friend_requests_switch_tab(move |tab: SharedString| {
        let weak = weak_window.clone();
        let tab_str = tab.to_string();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_friend_requests_tab(tab_str.into());
                tracing::debug!("Switched to friend requests tab");
            }
        });
    });

    // Accept friend request
    let weak_window = window.as_weak();
    let grpc_pool_accept = grpc_pool.clone();
    let settings_repo_accept = settings_repo.clone();
    let user_repo_accept = user_repo.clone();

    window.on_friend_request_accept(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_accept.clone();
        let settings_repo = settings_repo_accept.clone();
        let user_repo = user_repo_accept.clone();
        let user_id_str = user_id.to_string();

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_friend_requests_loading(true);
                }
            });

            let friend_id: UserId = match user_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid user ID: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friend_requests_loading(false);
                        }
                    });
                    return;
                }
            };

            let user_service = UserService::new(settings_repo, user_repo);
            let mut user_client = chatter_linux::network::grpc::user_client::UserClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match user_service
                .respond_friend_request(&mut user_client, &friend_id, true)
                .await
            {
                Ok(_) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friend_requests_loading(false);
                            tracing::info!("Accepted friend request from {}", friend_id);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to accept friend request: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friend_requests_loading(false);
                        }
                    });
                }
            }
        });
    });

    // Decline friend request
    let weak_window = window.as_weak();
    let grpc_pool_decline = grpc_pool.clone();
    let settings_repo_decline = settings_repo.clone();
    let user_repo_decline = user_repo.clone();

    window.on_friend_request_decline(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_decline.clone();
        let settings_repo = settings_repo_decline.clone();
        let user_repo = user_repo_decline.clone();
        let user_id_str = user_id.to_string();

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_friend_requests_loading(true);
                }
            });

            let friend_id: UserId = match user_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid user ID: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friend_requests_loading(false);
                        }
                    });
                    return;
                }
            };

            let user_service = UserService::new(settings_repo, user_repo);
            let mut user_client = chatter_linux::network::grpc::user_client::UserClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match user_service
                .respond_friend_request(&mut user_client, &friend_id, false)
                .await
            {
                Ok(_) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friend_requests_loading(false);
                            tracing::info!("Declined friend request from {}", friend_id);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to decline friend request: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friend_requests_loading(false);
                        }
                    });
                }
            }
        });
    });

    // Cancel friend request
    let weak_window = window.as_weak();
    let grpc_pool_cancel = grpc_pool.clone();
    let settings_repo_cancel = settings_repo.clone();
    let user_repo_cancel = user_repo.clone();

    window.on_friend_request_cancel(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_cancel.clone();
        let settings_repo = settings_repo_cancel.clone();
        let user_repo = user_repo_cancel.clone();
        let user_id_str = user_id.to_string();

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_friend_requests_loading(true);
                }
            });

            let friend_id: UserId = match user_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid user ID: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friend_requests_loading(false);
                        }
                    });
                    return;
                }
            };

            let user_service = UserService::new(settings_repo, user_repo);
            let mut user_client = chatter_linux::network::grpc::user_client::UserClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match user_service.remove_friend(&mut user_client, &friend_id).await {
                Ok(()) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friend_requests_loading(false);
                            tracing::info!("Cancelled friend request to {}", friend_id);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to cancel friend request: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friend_requests_loading(false);
                        }
                    });
                }
            }
        });
    });

    // Forward message
    let weak_window = window.as_weak();
    window.on_forward_message(move |target_id: SharedString| {
        let weak = weak_window.clone();
        let target_id_str = target_id.to_string();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_forward_loading(false);
                w.set_active_modal("".into());
                tracing::info!(
                    "Forward message to: {} (forwarding not yet implemented)",
                    target_id_str
                );
            }
        });
    });
}

fn wire_settings_callbacks(window: &MainWindow, ctx: &AppContext) {
    let grpc_pool = ctx.grpc_pool.clone();
    let settings_repo = ctx.settings_repo.clone();
    let key_store = ctx.key_store.clone();
    let auth_interceptor = ctx.auth_interceptor.clone();

    // Change password
    let weak_window = window.as_weak();
    let grpc_pool_pw = grpc_pool.clone();
    let settings_repo_pw = settings_repo.clone();
    let auth_interceptor_pw = auth_interceptor.clone();

    window.on_settings_change_password(move |current: SharedString, new: SharedString| {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_pw.clone();
        let settings_repo = settings_repo_pw.clone();
        let auth_interceptor = auth_interceptor_pw.clone();
        let current = current.to_string();
        let new = new.to_string();

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_settings_password_changing(true);
                    w.set_settings_password_error("".into());
                }
            });

            use chatter_linux::services::account_service::AccountService;
            let account_service = AccountService::new(settings_repo.clone(), auth_interceptor);

            let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match account_service
                .change_password(&mut auth_client, &current, &new)
                .await
            {
                Ok(()) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_settings_password_changing(false);
                            w.set_settings_current_password("".into());
                            w.set_settings_new_password("".into());
                            w.set_settings_confirm_password("".into());
                        }
                    });
                    tracing::info!("Password changed successfully");
                }
                Err(e) => {
                    let error_msg = e.to_string();
                    tracing::error!("Failed to change password: {}", error_msg);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_settings_password_changing(false);
                            w.set_settings_password_error(error_msg.into());
                        }
                    });
                }
            }
        });
    });

    // Read receipts toggle
    let settings_repo_read = settings_repo.clone();
    window.on_settings_read_receipts_changed(move |enabled: bool| {
        let settings_repo = settings_repo_read.clone();
        if let Err(e) = settings_repo.set(
            chatter_linux::storage::settings_keys::SEND_READ_RECEIPTS,
            &enabled,
        ) {
            tracing::error!("Failed to save read receipts setting: {}", e);
        } else {
            tracing::debug!("Read receipts setting changed to: {}", enabled);
        }
    });

    // Typing indicators toggle
    let settings_repo_typing = settings_repo.clone();
    window.on_settings_typing_indicators_changed(move |enabled: bool| {
        let settings_repo = settings_repo_typing.clone();
        if let Err(e) = settings_repo.set(
            chatter_linux::storage::settings_keys::SHOW_TYPING_INDICATORS,
            &enabled,
        ) {
            tracing::error!("Failed to save typing indicators setting: {}", e);
        } else {
            tracing::debug!("Typing indicators setting changed to: {}", enabled);
        }
    });

    // Copy fingerprint
    let key_store_fp = key_store.clone();
    window.on_settings_copy_fingerprint(move || {
        let key_store = key_store_fp.clone();

        use chatter_linux::crypto::signal::IdentityKeyManager;
        let identity_manager = IdentityKeyManager::new(key_store);

        let fingerprint = match identity_manager.get_public_key() {
            Ok(public_key) => {
                let bytes = public_key.serialize();
                bytes
                    .chunks(4)
                    .map(|chunk| chunk.iter().map(|b| format!("{:02X}", b)).collect::<String>())
                    .collect::<Vec<_>>()
                    .join(" ")
            }
            Err(e) => {
                tracing::error!("Failed to get identity key for fingerprint: {}", e);
                return;
            }
        };

        use chatter_linux::system::Clipboard;
        match Clipboard::new() {
            Ok(mut clipboard) => {
                if let Err(e) = clipboard.set_text(&fingerprint) {
                    tracing::error!("Failed to copy fingerprint to clipboard: {}", e);
                } else {
                    tracing::info!("Fingerprint copied to clipboard");
                }
            }
            Err(e) => {
                tracing::error!("Failed to access clipboard: {}", e);
            }
        }
    });

    // Sealed sender toggle
    let settings_repo_sealed = settings_repo.clone();
    window.on_settings_sealed_sender_changed(move |unrestricted: bool| {
        let settings_repo = settings_repo_sealed.clone();
        if let Err(e) = settings_repo.set("privacy.sealed_sender_unrestricted", &unrestricted) {
            tracing::error!("Failed to save sealed sender setting: {}", e);
        } else {
            tracing::debug!(
                "Sealed sender unrestricted setting changed to: {}",
                unrestricted
            );
        }
    });

    // Refresh prekeys
    let weak_window = window.as_weak();
    let grpc_pool_keys = grpc_pool.clone();
    let key_store_refresh = key_store.clone();

    window.on_settings_refresh_keys(move || {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_keys.clone();
        let key_store = key_store_refresh.clone();

        tokio::spawn(async move {
            use chatter_linux::crypto::signal::PreKeyManager;
            use chatter_linux::proto::chat_v1::signal_pre_key_bundle::PreKeyRecord;
            use rand::SeedableRng;

            let prekey_manager = PreKeyManager::new(key_store);

            let mut rng = rand::rngs::StdRng::from_os_rng();

            let start_id = match prekey_manager.get_next_prekey_id() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Failed to get next prekey ID: {}", e);
                    return;
                }
            };

            let prekeys = match prekey_manager.generate_prekeys(&mut rng, start_id, 100) {
                Ok(pks) => pks,
                Err(e) => {
                    tracing::error!("Failed to generate prekeys: {}", e);
                    return;
                }
            };

            let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            let proto_prekeys: Vec<PreKeyRecord> = prekeys
                .iter()
                .filter_map(|pk| {
                    Some(PreKeyRecord {
                        id: u32::from(pk.id().ok()?),
                        public_key: pk.public_key().ok()?.serialize().to_vec(),
                    })
                })
                .collect();

            match auth_client.refresh_pre_keys(proto_prekeys).await {
                Ok(()) => match auth_client.get_pre_key_stats(0).await {
                    Ok(stats) => {
                        let count = stats.available_count;
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_settings_prekey_count(count);
                            }
                        });
                        tracing::info!("Refreshed prekeys, {} now available", count);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to get prekey stats after refresh: {}", e);
                        tracing::info!("Prekeys refreshed successfully");
                    }
                },
                Err(e) => {
                    tracing::error!("Failed to upload prekeys to server: {}", e);
                }
            }
        });
    });

    // Edit profile
    let weak_window = window.as_weak();
    window.on_settings_edit_profile(move || {
        let weak = weak_window.clone();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_profile_is_self(true);
                w.set_active_modal("user-profile".into());
            }
        });
    });

    // Delete account (placeholder)
    let weak_window = window.as_weak();
    window.on_settings_delete_account(move || {
        let weak = weak_window.clone();
        tracing::info!("Delete account requested - showing confirmation would be needed");

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                // Future: Show delete confirmation modal
            }
        });
    });
}

fn wire_session_callbacks(window: &MainWindow, ctx: &AppContext) {
    let grpc_pool = ctx.grpc_pool.clone();
    let settings_repo = ctx.settings_repo.clone();

    // View sessions
    let weak_window = window.as_weak();
    let grpc_pool_sessions = grpc_pool.clone();
    let settings_repo_sessions = settings_repo.clone();

    window.on_settings_view_sessions(move || {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_sessions.clone();
        let settings_repo = settings_repo_sessions.clone();

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_sessions_loading(true);
                    w.set_active_modal("sessions".into());
                }
            });

            use chatter_linux::services::session_service::SessionService;
            let session_service = SessionService::new(settings_repo);

            let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match session_service.list_sessions(&mut auth_client).await {
                Ok(sessions) => {
                    let slint_sessions: Vec<SessionInfo> = sessions
                        .into_iter()
                        .map(|s| SessionInfo {
                            device_id: s.device_id as i32,
                            device_info: s.device_info.into(),
                            ip_address: s.ip_address.into(),
                            last_used: s.last_used.format("%Y-%m-%d %H:%M").to_string().into(),
                            created_at: s.created_at.format("%Y-%m-%d").to_string().into(),
                            is_current: s.is_current,
                        })
                        .collect();

                    let session_count = slint_sessions.len() as i32;

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_sessions(ModelRc::new(VecModel::from(slint_sessions)));
                            w.set_sessions_loading(false);
                            w.set_settings_session_count(session_count);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to list sessions: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_sessions_loading(false);
                        }
                    });
                }
            }
        });
    });

    // Revoke single session
    let weak_window = window.as_weak();
    let grpc_pool_revoke = grpc_pool.clone();
    let settings_repo_revoke = settings_repo.clone();

    window.on_revoke_session(move |device_id: i32| {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_revoke.clone();
        let settings_repo = settings_repo_revoke.clone();

        tokio::spawn(async move {
            let weak_revoking = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_revoking.upgrade() {
                    w.set_sessions_revoking(true);
                }
            });

            use chatter_linux::services::session_service::SessionService;
            let session_service = SessionService::new(settings_repo.clone());

            let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match session_service
                .revoke_session(&mut auth_client, device_id as u32)
                .await
            {
                Ok(()) => {
                    match session_service.list_sessions(&mut auth_client).await {
                        Ok(sessions) => {
                            let slint_sessions: Vec<SessionInfo> = sessions
                                .into_iter()
                                .map(|s| SessionInfo {
                                    device_id: s.device_id as i32,
                                    device_info: s.device_info.into(),
                                    ip_address: s.ip_address.into(),
                                    last_used: s
                                        .last_used
                                        .format("%Y-%m-%d %H:%M")
                                        .to_string()
                                        .into(),
                                    created_at: s.created_at.format("%Y-%m-%d").to_string().into(),
                                    is_current: s.is_current,
                                })
                                .collect();

                            let session_count = slint_sessions.len() as i32;

                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.set_sessions(ModelRc::new(VecModel::from(slint_sessions)));
                                    w.set_sessions_revoking(false);
                                    w.set_settings_session_count(session_count);
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("Failed to refresh sessions after revoke: {}", e);
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.set_sessions_revoking(false);
                                }
                            });
                        }
                    }
                    tracing::info!("Revoked session for device {}", device_id);
                }
                Err(e) => {
                    tracing::error!("Failed to revoke session: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_sessions_revoking(false);
                        }
                    });
                }
            }
        });
    });

    // Revoke all other sessions
    let weak_window = window.as_weak();
    let grpc_pool_revoke_all = grpc_pool.clone();
    let settings_repo_revoke_all = settings_repo.clone();

    window.on_revoke_all_sessions(move || {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_revoke_all.clone();
        let settings_repo = settings_repo_revoke_all.clone();

        tokio::spawn(async move {
            let weak_revoking = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_revoking.upgrade() {
                    w.set_sessions_revoking(true);
                }
            });

            use chatter_linux::services::session_service::SessionService;
            let session_service = SessionService::new(settings_repo.clone());

            let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match session_service
                .revoke_all_other_sessions(&mut auth_client)
                .await
            {
                Ok(()) => {
                    match session_service.list_sessions(&mut auth_client).await {
                        Ok(sessions) => {
                            let slint_sessions: Vec<SessionInfo> = sessions
                                .into_iter()
                                .map(|s| SessionInfo {
                                    device_id: s.device_id as i32,
                                    device_info: s.device_info.into(),
                                    ip_address: s.ip_address.into(),
                                    last_used: s
                                        .last_used
                                        .format("%Y-%m-%d %H:%M")
                                        .to_string()
                                        .into(),
                                    created_at: s.created_at.format("%Y-%m-%d").to_string().into(),
                                    is_current: s.is_current,
                                })
                                .collect();

                            let session_count = slint_sessions.len() as i32;

                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.set_sessions(ModelRc::new(VecModel::from(slint_sessions)));
                                    w.set_sessions_revoking(false);
                                    w.set_settings_session_count(session_count);
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("Failed to refresh sessions after revoke all: {}", e);
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.set_sessions_revoking(false);
                                }
                            });
                        }
                    }
                    tracing::info!("Revoked all other sessions");
                }
                Err(e) => {
                    tracing::error!("Failed to revoke all sessions: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_sessions_revoking(false);
                        }
                    });
                }
            }
        });
    });
}
