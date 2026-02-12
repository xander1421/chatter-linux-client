//! Authentication feature callbacks.
//!
//! Handles login, register, logout, and inbox event processing.

mod inbox_events;
mod initial_data;
mod login;
mod logout;
mod register;

pub use inbox_events::handle_inbox_event;
pub use initial_data::load_initial_data;

use chatter_linux::app::AppContext;

use crate::MainWindow;

/// Wire authentication callbacks (login, register, logout).
pub fn wire(window: &MainWindow, ctx: &AppContext) {
    login::wire_login(window, ctx);
    register::wire_register(window, ctx);
    logout::wire_logout(window, ctx);
}
