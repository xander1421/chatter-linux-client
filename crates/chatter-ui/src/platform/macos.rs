//! Native macOS titlebar customization.
//!
//! Slint (via winit) gives every window a plain, opaque native titlebar —
//! winit's `WindowExtMacOS` has no transparency/full-size-content-view API
//! (checked against its docs; it only covers fullscreen mode, tabbing, and a
//! handful of unrelated behaviors). The unified-toolbar look every native
//! macOS chat app uses (traffic lights floating over app content, no
//! separate plain title strip) requires setting three `NSWindow` properties
//! directly: `titlebarAppearsTransparent`, `titleVisibility`, and the
//! `fullSizeContentView` style mask bit.

use objc2::rc::Retained;
use objc2_app_kit::{NSView, NSWindow, NSWindowButton, NSWindowStyleMask, NSWindowTitleVisibility};
use raw_window_handle::HasWindowHandle;

/// Height of the transparent titlebar overlay, in logical points — the
/// clearance for the inset native controls. Logical points stay constant
/// across window sizes and Retina scaling.
///
/// The root layout reserves this band plus a small gap for all routes, so
/// native window controls never overlap app controls or page headings.
pub const TITLEBAR_HEIGHT: f32 = 34.0;

/// Leading clearance for the inset traffic lights, including trailing air.
pub const TRAFFIC_LIGHTS_WIDTH: f32 = 88.0;

/// Apply the unified-toolbar titlebar style to the window backing `window`.
///
/// Call once, right after the Slint window is shown (the underlying
/// `NSWindow` only exists once the platform window has actually been
/// created — see `slint::Window::window_handle`'s docs). Safe to call more
/// than once; each property set is idempotent. Returns whether it succeeded
/// — on failure the caller must not apply [`TRAFFIC_LIGHTS_WIDTH`] /
/// [`TITLEBAR_HEIGHT`], since the titlebar is then still an opaque strip of
/// its own and the app would be indenting its top row to clear something
/// that is not overlapping it.
pub fn apply_unified_titlebar(window: &slint::Window) -> bool {
    let slint_handle = window.window_handle();
    let Ok(handle) = slint_handle.window_handle() else {
        tracing::warn!("macOS titlebar: could not get raw window handle");
        return false;
    };
    let raw_window_handle::RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        tracing::warn!("macOS titlebar: window handle is not AppKit");
        return false;
    };

    // `AppKitWindowHandle::ns_view` is an `NSView*` borrowed from winit's open
    // window, valid for the duration of this call per raw-window-handle's
    // contract. `Retained::retain` bumps the refcount so we can safely
    // message it without racing winit's own ownership of it.
    let ns_view_ptr: *mut NSView = appkit.ns_view.as_ptr().cast();
    // SAFETY: `ns_view_ptr` is a live, non-null `NSView*` per the above.
    let Some(view) = (unsafe { Retained::retain(ns_view_ptr) }) else {
        tracing::warn!("macOS titlebar: null NSView");
        return false;
    };
    let Some(ns_window) = view.window() else {
        tracing::warn!("macOS titlebar: NSView has no owning NSWindow");
        return false;
    };

    ns_window.setTitlebarAppearsTransparent(true);
    ns_window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
    let mask = ns_window.styleMask() | NSWindowStyleMask::FullSizeContentView;
    ns_window.setStyleMask(mask);
    inset_buttons(&ns_window);
    true
}

/// AppKit may lay the buttons out again after resizing or leaving full screen.
/// Reapply absolute insets so repeated calls never accumulate an offset.
pub fn refresh_window_button_insets(window: &slint::Window) {
    let slint_handle = window.window_handle();
    let Ok(handle) = slint_handle.window_handle() else {
        return;
    };
    let raw_window_handle::RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return;
    };
    let Some(view) = (unsafe { Retained::retain(appkit.ns_view.as_ptr().cast::<NSView>()) }) else {
        return;
    };
    if let Some(window) = view.window() {
        inset_buttons(&window);
    }
}

fn inset_buttons(window: &NSWindow) {
    let Some(close) = window.standardWindowButton(NSWindowButton::CloseButton) else {
        return;
    };
    let horizontal_offset = 18.0 - close.frame().origin.x;
    for kind in [
        NSWindowButton::CloseButton,
        NSWindowButton::MiniaturizeButton,
        NSWindowButton::ZoomButton,
    ] {
        let Some(button) = window.standardWindowButton(kind) else {
            continue;
        };
        // SAFETY: this is AppKit's live standard button, accessed only from
        // the main-thread window callback; its retained parent stays alive.
        let Some(container) = (unsafe { button.superview() }) else {
            continue;
        };
        let mut origin = button.frame().origin;
        origin.x += horizontal_offset;
        origin.y = if container.isFlipped() {
            14.0
        } else {
            container.bounds().size.height - button.frame().size.height - 14.0
        };
        button.setFrameOrigin(origin);
    }
}

/// Point GStreamer at the plugins `macos/bundle.sh` staged in the .app.
///
/// Double-click and `open` have no shell in which to export
/// `GST_PLUGIN_SYSTEM_PATH`. Without it, each plugin dlopens Homebrew's
/// GLib while the bundle has already loaded its own copy. `cargo run` has
/// no bundled plugin directory, so the variable is left alone. A value
/// already set by the caller is also left alone.
pub fn prepare_bundled_gstreamer() {
    if std::env::var_os("GST_PLUGIN_SYSTEM_PATH").is_some() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(macos_dir) = exe.parent() else {
        return;
    };
    let plugins = macos_dir.join("../Resources/gstreamer-1.0-plugins");
    if !plugins.is_dir() {
        return;
    }
    let path = match plugins.canonicalize() {
        Ok(canonical) => canonical,
        Err(_) => plugins,
    };
    tracing::info!(path = %path.display(), "using bundled GStreamer plugins");
    let scanner = macos_dir.join("../Helpers/gst-plugin-scanner");
    // Safe: runs once at process startup, before any thread that might read the
    // environment is spawned. set_var is unsafe as of the 2024 edition.
    unsafe {
        std::env::set_var("GST_PLUGIN_SYSTEM_PATH", &path);
        if std::env::var_os("GST_PLUGIN_SCANNER").is_none() && scanner.is_file() {
            std::env::set_var("GST_PLUGIN_SCANNER", &scanner);
            tracing::info!(path = %scanner.display(), "using bundled GStreamer plugin scanner");
        }
    }
}
