//! Chatter Linux Client
//!
//! Native Linux desktop client for Chatter secure messaging.

use chatter_linux::events::EventBus;
use chatter_linux::state::{AuthState, UIState};
use chatter_linux::ui::UiBridge;

// Include Slint-generated code in the binary
slint::include_modules!();

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

    // Create state stores
    let ui_state = UIState::new();
    let auth_state = AuthState::new();
    let event_bus = EventBus::new();

    // Create UI bridge
    let bridge = UiBridge::new(ui_state, auth_state, event_bus);

    // Create the main window
    let window = MainWindow::new().expect("Failed to create window");

    // Setup UI callbacks (pass reference to our window, not the bridge's internal one)
    // Note: The bridge uses its own slint::include_modules!() so we need to pass
    // a reference to connect callbacks. For now this is a placeholder.
    let _ = &bridge; // Bridge holds state references for later use

    // Spawn state sync task (runs in background)
    // Note: The bridge's start_state_sync expects its own MainWindow type.
    // For now, we skip the sync since no properties need syncing yet.
    // This will be properly wired when we add actual Slint properties.

    // Run Slint event loop (blocks until window closes)
    window.run().expect("Failed to run application");

    tracing::info!("Application closed");
}
