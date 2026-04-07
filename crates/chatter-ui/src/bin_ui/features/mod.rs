//! Feature modules for UI callback wiring.
//!
//! Each module handles a specific feature area's callbacks.
//!
//! # Service-Based Architecture
//!
//! UI handlers use ChatterClient which provides:
//! - Pre-built services for all business operations
//! - State stores for reactive UI binding
//! - Event bus for domain events
//!
//! Services encapsulate all repository access - UI code never touches repos directly.

pub mod auth;
pub mod calls;
pub mod chat;
pub mod connection;
pub mod chat_actions;
pub mod composer;
pub mod header;
pub mod jarvis;
pub mod modals;
pub mod navigation;
pub mod notifications;
pub mod quick_switcher;
pub mod sidebar;
pub mod status;
pub mod thread;

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;
use crate::MainWindow;

/// Wire all UI callbacks to their handlers.
pub fn wire_all(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    auth::wire(window, ui_ctx);
    navigation::wire(window);
    chat::wire(window, ui_ctx);
    sidebar::wire(window, ui_ctx);
    chat_actions::wire(window, ui_ctx);
    header::wire(window, ui_ctx);
    composer::wire(window, ui_ctx);
    modals::wire(window, ui_ctx);
    thread::wire(window, ui_ctx);
    calls::wire(window, ui_ctx);
    jarvis::wire_jarvis_panel(window, ui_ctx);
    status::wire(window, ui_ctx);
    connection::wire(window, ui_ctx);
    quick_switcher::wire(window, ui_ctx);
    notifications::wire(window, ui_ctx);

    // Initialize inline GIF animation system
    crate::bin_ui::gif_state::init_inline_animations(window.as_weak());
}
