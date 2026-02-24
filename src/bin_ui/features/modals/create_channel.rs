//! Create channel modal callbacks.
//!
//! Uses ChannelService for all channel operations - no direct network calls.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_linux::app::AppContext;
use chatter_linux::domain::channel::{ChannelConfig, ChannelType, MessagePersistence};
use chatter_linux::network::grpc::IntegrationClient;
use chatter_linux::proto::chat_v1::IntegrationType;
use chatter_linux::services::ai_service::AIService;

use crate::bin_ui::converters::channel_to_slint;
use crate::bin_ui::features::sidebar::load_channel_into_ui;
use crate::{AIModelOption, ChannelListItemData, CreateChannelConfig, MainWindow};

pub fn wire_create_channel(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let grpc_pool = ctx.grpc_pool.clone();
    let settings_repo = ctx.settings_repo.clone();
    let channel_service = Arc::new(ctx.channel_service());
    let channel_repo = ctx.channel_repo.clone();
    let message_repo = ctx.message_repo.clone();
    let user_repo = ctx.user_repo.clone();
    let attachment_repo = ctx.attachment_repo.clone();
    let chat_state = ctx.chat_state.clone();

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
            let channel_service = channel_service.clone();
            let channel_repo = channel_repo.clone();
            let message_repo = message_repo.clone();
            let user_repo = user_repo.clone();
            let attachment_repo = attachment_repo.clone();
            let chat_state = chat_state.clone();
            let name = name.to_string();
            let description = description.to_string();
            let template = template.to_string();
            let ai_provider = ai_provider.to_string();
            let ai_model = ai_model.to_string();

            // Extract config values
            let is_private = config.is_private;
            let is_ephemeral = config.is_ephemeral;
            let ttl_hours = config.ttl_hours;
            let max_members = config.max_members;
            let password = config.password.to_string();

            // Use tokio::spawn - ChannelService handles all network calls
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
                let domain_config = ChannelConfig {
                    persistence: if is_ephemeral {
                        MessagePersistence::Ephemeral
                    } else {
                        MessagePersistence::Permanent
                    },
                    max_members: if max_members > 0 { Some(max_members as u32) } else { None },
                    default_ttl_seconds: if is_ephemeral {
                        Some((ttl_hours as u32) * 3600)
                    } else {
                        None
                    },
                    require_encryption: is_private,
                    ..Default::default()
                };

                // Create channel using ChannelService (handles stream + mailbox setup)
                match channel_service
                    .create_channel(
                        &name,
                        channel_type,
                        Some(domain_config),
                        description_opt,
                        None, // img_url
                        password_opt,
                        vec![], // initial_members - TODO: support inviting users
                        is_private, // setup_mailbox for private channels
                    )
                    .await
                {
                    Ok((channel, _mailbox_setup)) => {
                        let channel_id = channel.id;
                        let channel_id_str: SharedString = channel_id.to_string().into();

                        // Set the created channel as active
                        chat_state.set_active_channel(channel_id);

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
                                "ollama" => IntegrationType::Custom,
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

        // AIService doesn't use non-Send types, so we can use tokio::spawn directly
        tokio::spawn(async move {
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
}
