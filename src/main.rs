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
    if let Err(e) = ctx.grpc_pool.connect().await {
        tracing::warn!("Could not connect to server: {} (offline mode)", e);
    }

    // Create main window
    let window = MainWindow::new().expect("Failed to create window");

    // Set initial view based on login state
    let auth_service = ctx.auth_service();
    if auth_service.is_logged_in() {
        window.set_current_view("main".into());

        // Start inbox provider and load initial data
        let inbox_provider = ctx.device_inbox_provider.clone();
        let weak_window = window.as_weak();
        let channel_repo = ctx.channel_repo.clone();
        let message_repo = ctx.message_repo.clone();
        let user_repo = ctx.user_repo.clone();
        let settings_repo = ctx.settings_repo.clone();
        let auth_state = ctx.auth_state.clone();

        tokio::spawn(async move {
            // Start inbox provider
            if let Err(e) = inbox_provider.start().await {
                tracing::error!("Failed to start inbox provider on startup: {}", e);
            }

            // Load initial data (channels, DMs, messages)
            bin_ui::features::auth::load_initial_data(
                weak_window,
                channel_repo,
                message_repo,
                user_repo,
                settings_repo,
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

    tracing::info!("Application closed");
}
