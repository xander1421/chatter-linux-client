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
    wire_continue_without_account(window, ui_ctx);
    wire_calls_tab(window, ui_ctx);
}

/// Enter the main view with no account.
///
/// This is the way out of the sign-in screens when there is no server to sign in
/// to. It is not a login: no session is created, no identity is faked, nothing is
/// written. `is_signed_in` stays false and the main view says so — the account
/// state is exactly what it was, and signing in later is unaffected.
///
/// What is behind it is the mesh, which needs no account by construction. So the
/// route is offered only when core says the mesh is actually running
/// (`has_account_free_surfaces`); the check is repeated here rather than trusted
/// from the UI property, because the property was pushed at wire time and the
/// mesh could have stopped since.
fn wire_continue_without_account(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_continue_without_account(move || {
        let Some(w) = weak_window.upgrade() else { return };

        if !ui_ctx.has_account_free_surfaces() {
            // Nothing to go to. Say so where the user is looking instead of
            // moving them to an empty room.
            tracing::warn!("continue-without-account: no account-free surfaces are up");
            w.set_account_free_surfaces(false);
            return;
        }

        tracing::info!("entering the main view with no account (mesh only)");
        w.set_is_signed_in(false);
        w.set_current_view("main".into());
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

