//! Navigation callbacks.
//!
//! Handles switching between auth screens.

use slint::ComponentHandle;

use crate::MainWindow;

/// Wire navigation callbacks (switch between auth screens).
pub fn wire(window: &MainWindow) {
    wire_go_to_register(window);
    wire_go_to_login(window);
    wire_go_to_recovery(window);
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
