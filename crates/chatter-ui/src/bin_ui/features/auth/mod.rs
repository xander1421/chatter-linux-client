//! Authentication feature callbacks.
//!
//! Handles login, register, logout, and domain event processing.

pub mod inbox;
mod initial_data;
mod login;
mod logout;
pub mod presence_integration;
mod recovery;
mod register;

pub use initial_data::load_initial_data;

use std::sync::Arc;

use chatter_core::client::ChatterClient;

use crate::MainWindow;

/// Wire authentication callbacks (login, register, logout, recovery).
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    login::wire_login(window, ui_ctx);
    register::wire_register(window, ui_ctx);
    logout::wire_logout(window, ui_ctx);
    recovery::wire_recovery(window, ui_ctx);
}
