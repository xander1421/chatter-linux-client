//! Feature modules for UI callback wiring.
//!
//! Each module handles a specific feature area's callbacks.

pub mod auth;
pub mod calls;
pub mod chat;
pub mod chat_actions;
pub mod composer;
pub mod header;
pub mod modals;
pub mod navigation;
pub mod sidebar;
pub mod thread;

use slint::ComponentHandle;

use chatter_linux::app::AppContext;
use crate::MainWindow;

/// Wire all UI callbacks to their handlers.
pub fn wire_all(window: &MainWindow, ctx: &AppContext) {
    auth::wire(window, ctx);
    navigation::wire(window);
    chat::wire(window, ctx);
    sidebar::wire(window, ctx);
    chat_actions::wire(window, ctx);
    header::wire(window, ctx);
    composer::wire(window, ctx);
    modals::wire(window, ctx);
    thread::wire(window, ctx);
    calls::wire(window, ctx);

    // Initialize inline GIF animation system
    crate::bin_ui::gif_state::init_inline_animations(window.as_weak());
}
