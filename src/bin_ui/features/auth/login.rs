//! Login callback wiring.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_linux::app::UIContext;

use super::initial_data::load_initial_data;
use crate::MainWindow;

pub fn wire_login(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    window.on_login(move |username: slint::SharedString, password: slint::SharedString, server_url: slint::SharedString| {
        let ui_ctx = ui_ctx.clone();
        let weak = weak_window.clone();
        let username = username.to_string();
        let password = password.to_string();
        let server_url = server_url.to_string();

        tokio::spawn(async move {
            // Set loading state
            let weak_for_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_for_loading.upgrade() {
                    w.set_login_loading(true);
                    w.set_login_error("".into());
                }
            });

            // Update server URL and connect if different from current
            if ui_ctx.grpc_pool.endpoint() != server_url {
                if let Err(e) = ui_ctx.grpc_pool.set_url(&server_url) {
                    let error_msg = format!("Invalid Server URL: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_login_loading(false);
                            w.set_login_error(error_msg.into());
                        }
                    });
                    return;
                }
            }

            // Ensure we are connected to the selected server
            if !ui_ctx.grpc_pool.is_connected() {
                if let Err(e) = ui_ctx.grpc_pool.connect().await {
                    let error_msg = format!("Failed to connect to server: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_login_loading(false);
                            w.set_login_error(error_msg.into());
                        }
                    });
                    return;
                }
            }

            // Use pre-built auth service from UIContext
            let auth_service = &ui_ctx.auth_service;

            // Attempt login
            match auth_service
                .do_login(&username, &password, "linux-client", None)
                .await
            {
                Ok(result) => {
                    // Update auth state
                    ui_ctx.auth_state.set_logged_in(
                        result.user_id,
                        result.device_id,
                        result.display_name.clone(),
                    );

                    // IMPORTANT: Spawn domain event handlers BEFORE starting inbox provider
                    // to avoid race condition where inbox emits events before handlers subscribe.

                    // Spawn DM bootstrap handler for processing incoming key exchanges
                    let _bootstrap_handle = chatter_linux::inbox::spawn_dm_bootstrap_handler_from_bus(
                        ui_ctx.database.clone(),
                        ui_ctx.grpc_pool.clone(),
                        &ui_ctx.event_bus,
                    );
                    tracing::debug!("DM bootstrap handler spawned");

                    // Restart channel event stream to use new authentication credentials
                    // The old stream may fail to authenticate with expired credentials
                    let _channel_events = ui_ctx.channel_event_service.clone().start_event_stream();
                    tracing::debug!("Channel event stream restarted for new login session");

                    // Spawn anon group invite handler for processing incoming invites
                    let _anon_invite_handle = crate::bin_ui::features::modals::spawn_anon_group_invite_handler(
                        weak.clone(),
                        &ui_ctx,
                    );
                    tracing::debug!("Anon group invite handler spawned");

                    // Spawn anon group event handler for member joined/left/kicked events
                    let _anon_group_events_handle = crate::bin_ui::features::auth::inbox::spawn_anon_group_event_handler(
                        weak.clone(),
                        &ui_ctx,
                    );
                    tracing::debug!("Anon group event handler spawned");
                    
                    // Spawn pin persistence handler to save pin events to database
                    let _pin_persistence_handle = crate::bin_ui::features::auth::inbox::spawn_pin_persistence_handler(
                        ui_ctx.event_bus.clone(),
                        ui_ctx.message_service.clone(),
                    );
                    tracing::debug!("Pin persistence handler spawned");

                    // Spawn anon DM request handler for processing incoming message requests
                    let _anon_dm_handle = crate::bin_ui::features::modals::spawn_anon_dm_request_handler(
                        weak.clone(),
                        &ui_ctx,
                    );
                    tracing::debug!("Anon DM request handler spawned");

                    // Spawn Jarvis background observer and maintenance
                    let _jarvis_handles = chatter_linux::services::JarvisBackgroundBuilder::new(
                        ui_ctx.jarvis_service.clone(),
                    )
                    .with_event_bus(ui_ctx.event_bus.clone())
                    .spawn();
                    tracing::debug!("Jarvis background handlers spawned");

                    // Auto-load local AI model if previously configured
                    {
                        let local_ai = ui_ctx.local_ai_service.clone();
                        tokio::spawn(async move {
                            match local_ai.load_preferred_model().await {
                                Ok(Some(info)) => {
                                    tracing::info!("Auto-loaded local AI model: {}", info.model_id);
                                }
                                Ok(None) => {
                                    tracing::debug!("No local AI model configured for auto-load");
                                }
                                Err(e) => {
                                    tracing::warn!("Failed to auto-load local AI model: {}", e);
                                }
                            }
                        });
                    }

                    // Start inbox provider with InboxEventProcessor
                    // Ensure post-boot services are initialized (may have been
                    // skipped if initial server connection failed on startup)
                    ui_ctx.ensure_call_service();

                    // The processor handles protocol concerns (receipts, convergence, sender keys)
                    // and emits DomainEvent for UI updates
                    {
                        let weak_for_inbox = weak.clone();
                        let ui_ctx_for_inbox = ui_ctx.clone();
                        tokio::spawn(async move {
                            // Start the inbox provider with the processor
                            // This handles InboxEvent → DomainEvent conversion
                            if !ui_ctx_for_inbox.start_inbox_with_processor().await {
                                tracing::warn!("Failed to start inbox with processor");
                            }

                            // Subscribe to DomainEvent for UI updates
                            let mut domain_rx = ui_ctx_for_inbox.event_bus.subscribe_domain();
                            let weak_for_domain = weak_for_inbox.clone();
                            let ui_ctx_for_domain = ui_ctx_for_inbox.clone();
                            tokio::spawn(async move {
                                loop {
                                    match domain_rx.recv().await {
                                        Ok(event) => {
                                            super::inbox::domain_handler::handle_domain_event(event, &weak_for_domain, &ui_ctx_for_domain).await;
                                        }
                                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                            tracing::warn!(
                                                dropped_count = n,
                                                "Domain event receiver lagged - {} events dropped",
                                                n
                                            );
                                        }
                                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                            tracing::info!("Domain event channel closed");
                                            break;
                                        }
                                    }
                                }
                            });

                            // Also subscribe to raw InboxEvent for events not yet migrated
                            // TODO: Remove this once all events are handled via DomainEvent
                            if let Some(mut event_rx) = ui_ctx_for_inbox.start_inbox_provider().await {
                                tokio::spawn(async move {
                                    loop {
                                        match event_rx.recv().await {
                                            Ok(event) => {
                                                super::handle_inbox_event(event, &weak_for_inbox, &ui_ctx_for_inbox).await;
                                            }
                                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                                tracing::warn!(
                                                    dropped_count = n,
                                                    "Inbox event receiver lagged - {} events dropped",
                                                    n
                                                );
                                            }
                                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                                tracing::info!("Inbox event channel closed");
                                                break;
                                            }
                                        }
                                    }
                                });
                            }
                        });
                    }

                    // Switch to main view and stop animation
                    let weak_for_view = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_for_view.upgrade() {
                            w.set_login_loading(false);
                            w.set_current_view("main".into());
                        }
                    });

                    // Load initial data
                    load_initial_data(weak.clone(), &ui_ctx).await;
                    
                    // Initialize presence (send online status)
                    super::presence_integration::initialize_presence(&ui_ctx).await;
                }
                Err(e) => {
                    let error_msg = e.to_string();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_login_loading(false);
                            w.set_login_error(error_msg.into());
                        }
                    });
                }
            }
        });
    });
}
