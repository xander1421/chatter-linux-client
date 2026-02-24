//! Chatter Linux Client
//!
//! Native Linux desktop client for Chatter secure messaging.

use chatter_linux::app::AppContext;

// Include Slint-generated code in the binary
slint::include_modules!();

// Binary-only UI modules (require Slint types)
mod bin_ui;

#[tokio::main]
async fn main() {
    // Initialize tracing for logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    tracing::info!("Chatter Linux Client v0.1.0");

    // Initialize GStreamer for media processing (video decoding, waveform extraction)
    if let Err(e) = gstreamer::init() {
        tracing::warn!("GStreamer initialization failed: {} (media features may not work)", e);
    } else {
        tracing::info!("GStreamer initialized: {}", gstreamer::version_string());
    }

    // Initialize application context (all subsystems)
    let ctx = match AppContext::new() {
        Ok(ctx) => ctx,
        Err(e) => {
            tracing::error!("Failed to initialize application: {}", e);
            eprintln!("Failed to initialize: {}", e);
            std::process::exit(1);
        }
    };

    // Connect to server (continue in offline mode if fails)
    let _connected = match ctx.grpc_pool.connect().await {
        Ok(()) => {
            // Initialize components that require connection
            ctx.init_device_inbox_provider();
            ctx.init_group_fanout();
            ctx.init_call_service();
            true
        }
        Err(e) => {
            tracing::warn!("Could not connect to server: {} (offline mode)", e);
            false
        }
    };

    // Create main window
    let window = MainWindow::new().expect("Failed to create window");

    // Set initial view based on login state
    let auth_service = ctx.auth_service();
    if auth_service.is_logged_in() {
        window.set_current_view("main".into());

        // Spawn event handlers for DomainEvent processing (NDSS '21 mailboxes, etc.)
        let _event_handlers = ctx.spawn_event_handlers();

        // Start inbox provider and load initial data (only if connected)
        let inbox_provider = ctx.get_device_inbox_provider();
        let weak_window = window.as_weak();
        let grpc_pool = ctx.grpc_pool.clone();
        let channel_repo = ctx.channel_repo.clone();
        let message_repo = ctx.message_repo.clone();
        let user_repo = ctx.user_repo.clone();
        let attachment_repo = ctx.attachment_repo.clone();
        let settings_repo = ctx.settings_repo.clone();
        let signal_service = ctx.signal_service.clone();
        let group_coordinator = ctx.group_coordinator.clone();
        let auth_state = ctx.auth_state.clone();

        let user_repo_for_events = user_repo.clone();
        let channel_repo_for_events = channel_repo.clone();
        let attachment_repo_for_events = attachment_repo.clone();
        let grpc_pool_for_events = grpc_pool.clone();
        let message_service_for_events = ctx.message_service.clone();
        let database_for_events = ctx.database.clone();
        let mailbox_manager_for_events = ctx.mailbox_manager.clone();
        let event_bus_for_events = ctx.event_bus.clone();
        let weak_for_events = weak_window.clone();

        tokio::spawn(async move {
            // Start inbox provider if available
            if let Some(provider) = inbox_provider {
                if let Err(e) = provider.start().await {
                    tracing::error!("Failed to start inbox provider on startup: {}", e);
                } else {
                    // Subscribe to inbox events for real-time updates (DMs, friend requests, etc.)
                    let mut event_rx = provider.subscribe();
                    let weak_events = weak_for_events.clone();
                    let user_repo_events = user_repo_for_events.clone();
                    let channel_repo_events = channel_repo_for_events.clone();
                    let attachment_repo_events = attachment_repo_for_events.clone();
                    let grpc_pool_events = grpc_pool_for_events.clone();
                    let message_service_events = message_service_for_events.clone();
                    let database_events = database_for_events.clone();
                    let mailbox_manager_events = mailbox_manager_for_events.clone();
                    let event_bus_events = event_bus_for_events.clone();

                    tokio::spawn(async move {
                        while let Ok(event) = event_rx.recv().await {
                            bin_ui::features::auth::handle_inbox_event(
                                event,
                                &weak_events,
                                &user_repo_events,
                                &channel_repo_events,
                                &attachment_repo_events,
                                &grpc_pool_events,
                                &message_service_events,
                                &database_events,
                                &mailbox_manager_events,
                                &event_bus_events,
                            )
                            .await;
                        }
                    });
                }
            } else {
                tracing::warn!("Inbox provider not available (offline mode)");
            }

            // Load initial data (channels, DMs, messages)
            bin_ui::features::auth::load_initial_data(
                weak_window,
                grpc_pool,
                channel_repo,
                message_repo,
                user_repo,
                attachment_repo,
                settings_repo,
                signal_service,
                group_coordinator,
                auth_state,
            )
            .await;
        });
    } else {
        window.set_current_view("login".into());
    }

    // Wire all UI callbacks
    bin_ui::features::wire_all(&window, &ctx);

    // Run Slint event loop (blocks until window closes)
    window.run().expect("Failed to run application");

    tracing::info!("Application closed, shutting down services...");

    // Graceful shutdown of background services
    if let Some(provider) = ctx.get_device_inbox_provider() {
        if let Err(e) = provider.stop().await {
            tracing::warn!("Failed to stop inbox provider: {}", e);
        } else {
            tracing::debug!("Inbox provider stopped");
        }
    }

    // Disconnect from server
    ctx.grpc_pool.disconnect();
    tracing::debug!("Disconnected from server");

    tracing::info!("Shutdown complete");
}
