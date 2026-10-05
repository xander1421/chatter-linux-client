//! User status/presence management UI callbacks.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;

use crate::MainWindow;

/// Wire status change callbacks.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_status_change(window, ui_ctx);
    wire_status_getter(window);
    init_mode(window, ui_ctx);
}

/// Reflect the persisted manual presence mode into the picker on startup.
fn init_mode(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let mode = ui_ctx
        .presence_mode()
        .ok()
        .flatten()
        .unwrap_or_else(|| "auto".to_string());
    window.set_user_presence_mode(mode.into());
}

/// Wire the status change callback.
///
/// The picker only tells core what the user chose. Reflecting it back into
/// `user-presence-status` is not this handler's job: `set_presence` emits a
/// self `UserPresenceChanged`, and the domain handler owns that property for
/// every transition — including the two the picker never sees (auto-away and
/// the return to online on activity).
fn wire_status_change(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();

    window.on_change_user_status(move |status: slint::SharedString| {
        let status = status.to_string();
        let ui_ctx = ui_ctx.clone();

        crate::rt_handle().spawn(async move {
            super::auth::presence_integration::set_user_status(&ui_ctx, &status).await;
        });
    });
}

/// Answer the `get-user-status` callback from the live presence property.
///
/// Slint reads the dot's colour from `user-presence-status`; this exists for
/// callers that want the value pulled rather than bound. It used to return the
/// literal "online" whatever the user's status was.
fn wire_status_getter(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_get_user_status(move || -> slint::SharedString {
        weak_window
            .upgrade()
            .map(|w| w.get_user_presence_status())
            .unwrap_or_else(|| "offline".into())
    });
}
