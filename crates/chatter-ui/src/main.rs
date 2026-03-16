//! Chatter Client
//!
//! Cross-platform client for Chatter secure messaging.
//! Supports Linux, Windows, macOS, and Android.

use chatter_core::app::AppContext;
use chatter_core::system::{init_antidebug, antidebug_check};
use tokio_util::sync::CancellationToken;

// Include Slint-generated code in the binary
slint::include_modules!();

// Binary-only UI modules (require Slint types)
mod bin_ui;

/// Android entry point - called by android-activity glue.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: i_slint_backend_android_activity::AndroidApp) {
    // Set up the Slint Android platform backend
    slint::platform::set_platform(Box::new(
        i_slint_backend_android_activity::AndroidPlatform::new(app),
    ))
    .expect("Failed to set Android platform");

    // Build and run the tokio runtime manually (can't use #[tokio::main] here)
    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
    rt.block_on(app_main());
}

/// Desktop entry point.
#[cfg(not(target_os = "android"))]
#[tokio::main]
async fn main() {
    app_main().await;
}

/// Android: dummy main (android_main is the real entry point).
#[cfg(target_os = "android")]
fn main() {}

/// Shared application logic for all platforms.
async fn app_main() {
    // Create shutdown token for graceful task cancellation
    let shutdown_token = CancellationToken::new();
    // Initialize tracing for logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    tracing::info!("Chatter Client v0.1.0 ({})", std::env::consts::OS);

    // Security: Check for debugger attachment before handling sensitive data
    // This is defense-in-depth - logs warning if debugger detected
    if !init_antidebug() {
        tracing::warn!("Security: Continuing with debugger detected - sensitive data may be exposed");
        // Note: We continue anyway but sensitive key handling is now more aggressive about zeroization
    }

    // Initialize GStreamer for media processing (video decoding, waveform extraction)
    if let Err(e) = gstreamer::init() {
        tracing::warn!("GStreamer initialization failed: {} (media features may not work)", e);
    } else {
        tracing::info!("GStreamer initialized: {}", gstreamer::version_string());
    }

    // Select OpenGL ES backend for hardware-accelerated video rendering (desktop only)
    // On Android, the platform is set up in android_main()
    #[cfg(not(target_os = "android"))]
    if let Err(e) = slint::BackendSelector::new()
        .backend_name("winit".into())
        .require_opengl_es()
        .select()
    {
        tracing::warn!("OpenGL ES backend not available: {} (falling back to default)", e);
    } else {
        tracing::info!("Slint OpenGL ES backend selected for hardware-accelerated video");
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

    // Get UIContext for all UI operations
    let ui_ctx = ctx.ui_context();

    // Restore authentication session from saved credentials
    // This populates the gRPC auth interceptor before we connect
    ui_ctx.auth_service.restore_session();

    // Start background typing indicator cleanup task
    let _typing_cleanup = ui_ctx.typing_service.start_cleanup_task();
    tracing::info!("Typing indicator cleanup task started");

    // Connect to server (continue in offline mode if fails)
    let _connected = match ui_ctx.auth_service.connect().await {
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

    // Initialize hardware-accelerated video rendering (GL on Linux/Android, Metal on iOS)
    if bin_ui::video::init_hw_video(&window) {
        tracing::info!("Hardware-accelerated video enabled");
    } else {
        tracing::info!("Using software video rendering");
    }

    // Set initial view based on login state
    if ui_ctx.auth_service.is_logged_in() {
        // Restore auth_state from stored session (critical for is_own detection in messages)
        if let Some((user_id, device_id)) = ui_ctx.auth_service.current_user() {
            let display_name = ui_ctx.user_service.get_display_name(&user_id);
            ui_ctx.auth_state.set_logged_in(user_id, device_id, display_name);
            tracing::info!("Restored auth state for user {}", user_id);
        }

        window.set_current_view("main".into());

        // Spawn event handlers for DomainEvent processing (NDSS '21 mailboxes, etc.)
        let _event_handlers = ctx.spawn_event_handlers();
        
        // Start channel event stream service (owns gRPC stream, handles reconnection)
        let _channel_events = ctx.start_channel_event_stream();
        
        // Start channel bootstrap service (flushes pending messages to new members)
        let _channel_bootstrap = ctx.start_channel_bootstrap_service();

        // Spawn DM bootstrap handler for processing incoming key exchanges
        let _bootstrap_handle = ctx.spawn_dm_bootstrap_handler();

        // Spawn UI-level domain event handlers (anon DM messages, requests, group events, etc.)
        let weak_window = window.as_weak();
        let _anon_dm_handle = bin_ui::features::modals::spawn_anon_dm_request_handler(
            weak_window.clone(),
            &ui_ctx,
        );
        let _anon_invite_handle = bin_ui::features::modals::spawn_anon_group_invite_handler(
            weak_window.clone(),
            &ui_ctx,
        );
        let _anon_group_events_handle = bin_ui::features::auth::inbox::spawn_anon_group_event_handler(
            weak_window.clone(),
            &ui_ctx,
        );
        // Pin persistence is now spawned by start_inbox()

        // Subscribe to DomainEvent for UI updates (sidebar, message list, etc.)
        {
            let mut domain_rx = ui_ctx.event_bus.subscribe_domain();
            let weak_for_domain = weak_window.clone();
            let ui_ctx_for_domain = ui_ctx.clone();
            tokio::spawn(async move {
                loop {
                    match domain_rx.recv().await {
                        Ok(event) => {
                            bin_ui::features::auth::inbox::domain_handler::handle_domain_event(
                                event,
                                &weak_for_domain,
                                &ui_ctx_for_domain,
                            ).await;
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
        }

        // Start inbox provider and load initial data (only if connected)
        let inbox_provider = ctx.get_device_inbox_provider();
        let ui_ctx_for_events = ui_ctx.clone();
        let weak_for_events = weak_window.clone();
        let shutdown_for_inbox = shutdown_token.clone();

        tokio::spawn(async move {
            // Start inbox provider if available
            if let Some(provider) = inbox_provider {
                if let Err(e) = provider.start().await {
                    tracing::error!("Failed to start inbox provider on startup: {}", e);
                } else {
                    // Subscribe to inbox events for real-time updates (DMs, friend requests, etc.)
                    let mut event_rx = provider.subscribe();
                    let weak_events = weak_for_events.clone();
                    let ui_ctx_events = ui_ctx_for_events.clone();
                    let shutdown_for_events = shutdown_for_inbox.clone();

                    tokio::spawn(async move {
                        loop {
                            tokio::select! {
                                _ = shutdown_for_events.cancelled() => {
                                    tracing::debug!("Inbox event handler shutting down");
                                    break;
                                }
                                result = event_rx.recv() => {
                                    match result {
                                        Ok(event) => {
                                            bin_ui::features::auth::handle_inbox_event(
                                                event,
                                                &weak_events,
                                                &ui_ctx_events,
                                            )
                                            .await;
                                        }
                                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                            tracing::warn!(
                                                dropped_count = n,
                                                "Inbox event receiver lagged - {} events dropped",
                                                n
                                            );
                                            // Continue receiving new events
                                        }
                                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                            tracing::info!("Inbox event channel closed");
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    });
                }
            } else {
                tracing::warn!("Inbox provider not available (offline mode)");
            }

            // Load initial data (channels, DMs, messages)
            bin_ui::features::auth::load_initial_data(weak_window, &ui_ctx_for_events).await;
        });
    } else {
        // Check if user has registered before on this device
        // If not, show register page first; otherwise show login
        let has_registered_before = ui_ctx.auth_service.has_registered_before();
        if has_registered_before {
            window.set_current_view("login".into());
        } else {
            window.set_current_view("register".into());
        }

    }

    // Wire all UI callbacks
    bin_ui::features::wire_all(&window, &ui_ctx);

    // Dev mode: auto-fill and auto-submit registration for fast testing
    if std::env::var("CHATTER_DEV").is_ok() && !ui_ctx.auth_service.is_logged_in() && !ui_ctx.auth_service.has_registered_before() {
        let suffix: u16 = rand::random();
        let username = format!("dev{}", suffix);
        let display_name = format!("Dev User {}", suffix);
        let password = "devpass123";
        let pin = "123456";

        window.set_register_username(username.clone().into());
        window.set_register_display_name(display_name.clone().into());
        window.set_register_password(password.into());
        window.set_register_confirm_password(password.into());
        window.set_register_pin(pin.into());
        window.set_register_server_url("http://localhost:50051".into());

        tracing::info!("DEV MODE: Auto-registering as {} ({})", username, display_name);
        window.invoke_register(
            username.into(),
            password.into(),
            display_name.into(),
            pin.into(),
            "http://localhost:50051".into(),
        );
    }

    // Spawn periodic anti-debugging check (defense-in-depth against runtime attachment)
    let shutdown_for_antidebug = shutdown_token.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
        loop {
            tokio::select! {
                _ = shutdown_for_antidebug.cancelled() => {
                    tracing::debug!("Anti-debug check shutting down");
                    break;
                }
                _ = interval.tick() => {
                    // Check for debugger attachment - logs warning if detected
                    let _ = antidebug_check();
                }
            }
        }
    });

    // Spawn Ctrl+C handler for graceful shutdown (desktop only)
    #[cfg(not(target_os = "android"))]
    {
        let shutdown_for_ctrlc = shutdown_token.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                tracing::info!("Ctrl+C received, initiating shutdown...");
                shutdown_for_ctrlc.cancel();
                // Force quit Slint's event loop
                slint::quit_event_loop().ok();
            }
        });
    }

    // Run Slint event loop (blocks until window closes or Ctrl+C)
    window.run().expect("Failed to run application");

    // Signal all background tasks to shut down
    tracing::info!("Window closed, signaling shutdown...");
    shutdown_token.cancel();

    // Quick cleanup of critical resources
    tracing::info!("Shutting down services...");

    // Send offline presence before shutting down
    if ui_ctx.auth_state.is_logged_in() {
        tracing::info!("Sending offline presence before shutdown...");
        bin_ui::features::auth::presence_integration::shutdown_presence(&ui_ctx).await;
        // Give a moment for presence updates to be sent
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    // Stop inbox provider (has its own shutdown logic)
    if let Some(provider) = ctx.get_device_inbox_provider() {
        if let Err(e) = provider.stop().await {
            tracing::warn!("Failed to stop inbox provider: {}", e);
        }
    }

    // Disconnect from server
    ui_ctx.auth_service.disconnect();
    tracing::debug!("Disconnected from server");

    tracing::info!("Shutdown complete, exiting...");

    // Force exit - some background tasks (gRPC streams, event loops) may still be running
    // This ensures clean termination even if tokio tasks are blocked on channel receives
    std::process::exit(0);
}
