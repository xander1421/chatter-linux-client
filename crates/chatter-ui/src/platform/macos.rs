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
use objc2_app_kit::{NSView, NSWindowStyleMask, NSWindowTitleVisibility};
use raw_window_handle::HasWindowHandle;

/// The standard macOS unified-toolbar titlebar height (traffic-light row),
/// in logical points — matches every stock macOS app (Finder, Messages,
/// Safari) and doesn't change with window size or Retina scaling.
pub const TITLEBAR_INSET: f32 = 28.0;

/// Apply the unified-toolbar titlebar style to the window backing `window`.
///
/// Call once, right after the Slint window is shown (the underlying
/// `NSWindow` only exists once the platform window has actually been
/// created — see `slint::Window::window_handle`'s docs). Safe to call more
/// than once; each property set is idempotent. Returns whether it succeeded
/// — on failure the caller should not apply [`TITLEBAR_INSET`], since the
/// titlebar is still opaque and content would then render under it instead
/// of under the (in that case still-drawn) title text.
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
    true
}
