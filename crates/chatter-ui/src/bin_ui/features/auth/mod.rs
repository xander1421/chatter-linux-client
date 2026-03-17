//! Authentication feature callbacks.
//!
//! Handles login, register, logout, and inbox event processing.

pub mod inbox;
mod inbox_events;
mod initial_data;
mod login;
mod logout;
pub mod presence_integration;
mod register;

pub use inbox::{handle_inbox_event, update_reaction_summary};
pub use initial_data::load_initial_data;

use std::sync::Arc;

use chatter_core::client::ChatterClient;

use crate::MainWindow;

/// Wire authentication callbacks (login, register, logout).
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    login::wire_login(window, ui_ctx);
    register::wire_register(window, ui_ctx);
    logout::wire_logout(window, ui_ctx);
}
