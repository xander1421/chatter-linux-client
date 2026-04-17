//! Notification wiring.
//!
//! Thin passthrough — all notification routing lives in
//! `ChatterClient::start_notification_listener`. This module only exists
//! so the UI boot sequence has a single `wire()` call per feature.

use std::sync::Arc;

use chatter_core::client::ChatterClient;
use crate::MainWindow;

pub fn wire(_window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    ui_ctx.start_notification_listener();
}
