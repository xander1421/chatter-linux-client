//! Login callback wiring.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_linux::app::UIContext;

use super::channel_events::spawn_channel_events_handler;
use super::initial_data::load_initial_data;
use crate::MainWindow;

pub fn wire_login(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    window.on_login(move |username: slint::SharedString, password: slint::SharedString| {
        let ui_ctx = ui_ctx.clone();
        let weak = weak_window.clone();
        let username = username.to_string();
        let password = password.to_string();

        tokio::spawn(async move {
            // Set loading state
            let weak_for_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_for_loading.upgrade() {
                    w.set_login_loading(true);
                    w.set_login_error("".into());
                }
            });

            // Use pre-built auth service from UIContext
            let auth_service = &ui_ctx.auth_service;

            // Attempt login
            match auth_service
                .do_login(&username, &password, "linux-client")
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

                    // Spawn channel events handler for member join/leave notifications
                    let _channel_events_handle = spawn_channel_events_handler(
                        weak.clone(),
                        &ui_ctx,
                        ui_ctx.channel_event_service.clone(),
                    );
                    tracing::debug!("Channel events handler spawned");

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

                    // NOW start inbox provider - handlers are already subscribed
                    {
                        let weak_for_inbox = weak.clone();
                        let ui_ctx_for_inbox = ui_ctx.clone();
                        tokio::spawn(async move {
                            if let Some(mut event_rx) = ui_ctx_for_inbox.start_inbox_provider().await {
                                tokio::spawn(async move {
                                    loop {
                                        match event_rx.recv().await {
                                            Ok(event) => {
                                                super::handle_inbox_event(event, &weak_for_inbox, &ui_ctx_for_inbox).await;
                                            }
                                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                                // Channel buffer overflow - some events were dropped
                                                // Log and continue receiving new events
                                                tracing::warn!(
                                                    dropped_count = n,
                                                    "Inbox event receiver lagged - {} events dropped due to slow processing",
                                                    n
                                                );
                                            }
                                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                                // Channel closed, exit the loop
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
                    load_initial_data(weak, &ui_ctx).await;
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
