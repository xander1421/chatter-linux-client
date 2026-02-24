//! Feature modules for UI callback wiring.
//!
//! Each module handles a specific feature area's callbacks.
//!
//! # Separation of Concerns
//!
//! UI handlers in this layer follow these rules:
//!
//! 1. **Use services for business operations**: All operations that modify server
//!    state (send message, create channel, etc.) MUST go through service layer.
//!
//! 2. **Repository cache updates are allowed**: After a server operation succeeds
//!    or a server event is received, handlers may directly update repository to
//!    sync local cache with server state. This is the "write-back cache" pattern.
//!
//! 3. **Repository reads are allowed**: Handlers may read from repositories for
//!    display purposes (user profiles, message content, etc.).
//!
//! 4. **Settings are UI state**: Settings repository may be accessed directly for
//!    UI preferences (sidebar collapsed, theme, etc.).
//!
//! 5. **Local-only data is allowed**: Some data is purely local (call history logs)
//!    and may be written directly without server involvement.
//!
//! This pattern ensures business logic stays in services while allowing efficient
//! local caching without unnecessary service layer indirection.

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
