//! User status/presence management UI callbacks.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::app::UIContext;

use crate::MainWindow;

/// Wire status change callbacks.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    wire_status_change(window, ui_ctx);
    wire_status_getter(window);
}

/// Wire the status change callback.
fn wire_status_change(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();
    
    window.on_change_user_status(move |status: slint::SharedString| {
        let status = status.to_string();
        let ui_ctx = ui_ctx.clone();
        let weak = weak_window.clone();
        
        tokio::spawn(async move {
            // Change the status
            super::auth::presence_integration::set_user_status(&ui_ctx, &status).await;
            
            // Update UI to reflect new status
            let new_status = super::auth::presence_integration::get_current_status().await;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_user_presence_status(new_status.into());
                }
            });
        });
    });
}

/// Wire the status getter callback.
fn wire_status_getter(window: &MainWindow) {
    let weak_window = window.as_weak();
    
    window.on_get_user_status(move || -> slint::SharedString {
        // This needs to be synchronous for the UI callback
        // We'll use the last known status from when it was set
        // For now, return a default
        "online".into()
    });
    
    // Set initial status on startup
    let weak = weak_window.clone();
    tokio::spawn(async move {
        let status = super::auth::presence_integration::get_current_status().await;
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_user_presence_status(status.into());
            }
        });
    });
}