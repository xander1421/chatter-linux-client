//! Navigation callbacks.
//!
//! Handles switching between auth screens.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;

use crate::MainWindow;

/// Wire navigation callbacks (switch between auth screens).
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_go_to_register(window);
    wire_go_to_login(window);
    wire_go_to_recovery(window);
    wire_go_to_link_device(window);
    wire_calls_tab(window, ui_ctx);
    wire_welcome(window);
}

/// Welcome page (shown after first registration): all three buttons land in
/// the main view, two of them with a modal already open.
fn wire_welcome(window: &MainWindow) {
    let weak = window.as_weak();
    window.on_welcome_get_started(move || {
        if let Some(w) = weak.upgrade() {
            w.set_current_view("main".into());
        }
    });

    let weak = window.as_weak();
    window.on_welcome_add_friends(move || {
        if let Some(w) = weak.upgrade() {
            w.set_current_view("main".into());
            w.set_friends_modal_tab("find".into());
            w.set_active_modal("friends".into());
        }
    });

    let weak = window.as_weak();
    window.on_welcome_browse_channels(move || {
        if let Some(w) = weak.upgrade() {
            w.set_current_view("main".into());
            w.set_active_modal("create-channel".into());
        }
    });
}

fn wire_go_to_link_device(window: &MainWindow) {
    let weak_window = window.as_weak();
    window.on_go_to_link_device(move || {
        if let Some(w) = weak_window.upgrade() {
            w.set_current_view("link-device".into());
            w.set_link_device_error("".into());
            w.set_link_device_url("".into());
            w.set_link_device_status("Tap \"Generate QR\" to start.".into());
            w.set_link_device_active(false);
        }
    });
}

fn wire_go_to_register(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_go_to_register(move || {
        if let Some(w) = weak_window.upgrade() {
            w.set_current_view("register".into());
            // Clear register form
            w.set_register_display_name("".into());
            w.set_register_username("".into());
            w.set_register_password("".into());
            w.set_register_confirm_password("".into());
            w.set_register_error("".into());
        }
    });
}

fn wire_go_to_login(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_go_to_login(move || {
        if let Some(w) = weak_window.upgrade() {
            w.set_current_view("login".into());
            // Clear login form
            w.set_login_username("".into());
            w.set_login_password("".into());
            w.set_login_error("".into());
        }
    });
}

fn wire_go_to_recovery(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_go_to_recovery(move || {
        if let Some(w) = weak_window.upgrade() {
            w.set_current_view("recovery".into());
            // Clear recovery form
            w.set_recovery_username("".into());
            w.set_recovery_code("".into());
            w.set_recovery_new_password("".into());
            w.set_recovery_confirm_password("".into());
            w.set_recovery_new_pin("".into());
            w.set_recovery_error("".into());
            w.set_recovery_success("".into());
        }
    });
}

/// Mobile "Calls" tab: placeholder until a full call history screen is built.
fn wire_calls_tab(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_calls_tab_clicked(move || {
        if let Some(w) = weak_window.upgrade() {
            // Clear active conversation so the mobile view shows the calls tab state
            w.set_channel_id("".into());
            ui_ctx.chat_state().set_active_conversation(chatter_core::state::ActiveConversation::None);
            crate::bin_ui::features::sidebar::refresh_selection(&w, &ui_ctx);
        }
    });
}

