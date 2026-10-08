//! Freedesktop window identity.
//!
//! Wayland matches `app_id` to the desktop-file id. X11 matches `StartupWMClass`
//! to the WM_CLASS class. Both are `chatter-linux`: the same string as
//! `chatter_core::system::platform::APP_ID` and `assets/chatter-linux.desktop`.
//! Slint reads it when the native window is created, which is after
//! `MainWindow::new` and before the window is shown.

use chatter_core::system::platform::APP_ID;

/// Tell the window manager this process is `chatter-linux.desktop`.
///
/// Failure is logged and ignored. A missing id only affects launcher grouping;
/// the session still starts, and the keyring path is unchanged.
pub fn apply_desktop_app_id() {
    if let Err(error) = slint::set_xdg_app_id(APP_ID) {
        tracing::warn!(
            %error,
            app_id = APP_ID,
            "desktop app id was not applied; the window may not group with chatter-linux.desktop"
        );
    }
}
