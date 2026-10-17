//! Authentication feature callbacks.
//!
//! Handles login, register, logout, and domain event processing.

pub mod inbox;
pub(crate) mod initial_data;
pub mod link_device;
mod login;
mod logout;
mod recovery;
mod register;
mod shield;

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
    link_device::wire_link_device(window, ui_ctx);
    shield::wire_shield_before_login(window, ui_ctx);
}
