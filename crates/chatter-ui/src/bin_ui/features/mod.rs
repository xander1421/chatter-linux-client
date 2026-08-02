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
/// Android hardware/gesture Back. Compiled everywhere; inert on desktop.
pub mod back;
#[cfg(feature = "call-ui")]
pub mod calls;
pub mod chat;
pub mod connection;
pub mod chat_actions;
pub mod composer;
pub mod header;
#[cfg(feature = "jarvis")]
pub mod jarvis;
/// bitchat mesh surfaces. Gated exactly like `jarvis`: the Slint side is
/// always compiled and stays inert until `MeshGlobal.enabled` is flipped here.
#[cfg(feature = "mesh")]
pub mod mesh;
pub mod modals;
pub mod navigation;
pub mod notifications;
pub mod sidebar;
pub mod status;
pub mod thread;
pub mod unified_search;

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;
use crate::MainWindow;

/// Wire all UI callbacks to their handlers.
pub fn wire_all(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    auth::wire(window, ui_ctx);
    back::wire(window);
    navigation::wire(window, ui_ctx);
    chat::wire(window, ui_ctx);
    sidebar::wire(window, ui_ctx);
    chat_actions::wire(window, ui_ctx);
    header::wire(window, ui_ctx);
    composer::wire(window, ui_ctx);
    modals::wire(window, ui_ctx);
    thread::wire(window, ui_ctx);
    #[cfg(feature = "call-ui")]
    calls::wire(window, ui_ctx);
    #[cfg(feature = "jarvis")]
    jarvis::memory_view::wire(window, ui_ctx);
    #[cfg(feature = "mesh")]
    mesh::wire(window, ui_ctx);
    status::wire(window, ui_ctx);
    connection::wire(window, ui_ctx);
    unified_search::wire(window, ui_ctx);
    notifications::wire(window, ui_ctx);

    // Initialize inline GIF animation system
    crate::bin_ui::gif_state::init_inline_animations(window.as_weak());
}
