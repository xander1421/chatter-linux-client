//! Android keyboard (IME) and system bar inset detection via JNI.
//!
//! Queries system bar and display cutout insets to calculate safe area
//! positioning in Slint.
//!
//! NativeActivity (used by android-activity crate) renders a full-screen GL
//! surface that extends behind the status bar and navigation bar. We detect
//! the system bar sizes via WindowInsets and pass them to Slint as safe-area
//! properties so the UI content avoids overlapping system chrome.

use jni::objects::{JObject, JValue};
use jni::JNIEnv;

/// Current Android display insets in density-independent pixels (dp).
///
/// Values are suitable for direct use as Slint logical pixel lengths.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayInsets {
    /// Top inset in dp (status bar / display cutout).
    pub top: f32,
    /// Bottom inset in dp (nav bar + keyboard when visible).
    pub bottom: f32,
}

/// Query current display insets from Android via JNI.
///
/// Returns the top (status bar / cutout) and bottom (nav bar + keyboard)
/// insets in density-independent pixels, suitable for Slint `length` properties.
///
/// Uses `getRootWindowInsets()` (API 23+) as the primary method, which
/// reliably reports system bar sizes for NativeActivity in edge-to-edge mode.
/// Falls back to `getWindowVisibleDisplayFrame()` if WindowInsets is unavailable.
pub fn query_display_insets() -> Option<DisplayInsets> {
    // Catch JNI panics to avoid crashing the app
    let result = std::panic::catch_unwind(query_display_insets_inner);
    match result {
        Ok(insets) => insets,
        Err(_) => {
            tracing::warn!("JNI panic in query_display_insets");
            None
        }
    }
}

fn query_display_insets_inner() -> Option<DisplayInsets> {
    let mut env = get_jni_env()?;
    let ctx = unsafe { ndk_context::android_context() };
    let activity = unsafe { JObject::from_raw(ctx.context().cast()) };

    // Get density for px -> dp conversion
    let resources = env
        .call_method(&activity, "getResources", "()Landroid/content/res/Resources;", &[])
        .ok()?.l().ok()?;
    let display_metrics = env
        .call_method(&resources, "getDisplayMetrics", "()Landroid/util/DisplayMetrics;", &[])
        .ok()?.l().ok()?;
    let density = env.get_field(&display_metrics, "density", "F").ok()?.f().ok()?;

    if density <= 0.0 {
        return None;
    }

    // Get the window's decor view
    let window = env
        .call_method(&activity, "getWindow", "()Landroid/view/Window;", &[])
        .ok()?.l().ok()?;
    let decor_view = env
        .call_method(&window, "getDecorView", "()Landroid/view/View;", &[])
        .ok()?.l().ok()?;

    // Root view height — needed for fallback path and for the "view laid out" check
    let root_view = env
        .call_method(&decor_view, "getRootView", "()Landroid/view/View;", &[])
        .ok()?.l().ok()?;
    let root_height = env
        .call_method(&root_view, "getHeight", "()I", &[])
        .ok()?.i().ok()?;

    if root_height <= 0 {
        // View not laid out yet — will get valid values on next poll
        return None;
    }

    // ── Primary: WindowInsets API (API 23+, our min SDK is 26) ───────────
    // getRootWindowInsets() reliably reports system bar sizes for NativeActivity,
    // unlike getWindowVisibleDisplayFrame() which may report top=0 when the GL
    // surface extends behind the status bar.
    if let Some(insets) = query_via_window_insets(&mut env, &decor_view, density) {
        return Some(insets);
    }

    // ── Fallback: getWindowVisibleDisplayFrame() ─────────────────────────
    // Used when getRootWindowInsets() returns null (view not yet attached).
    query_via_visible_frame(&mut env, &decor_view, root_height, density)
}

/// Query insets using the WindowInsets API (API 23+).
///
/// This is the preferred path: `getSystemWindowInsetTop/Bottom()` reports
/// system bar sizes directly, and `getDisplayCutout()` (API 28+) handles
/// notch/punch-hole cameras.
fn query_via_window_insets(
    env: &mut JNIEnv,
    decor_view: &JObject,
    density: f32,
) -> Option<DisplayInsets> {
    let insets = env
        .call_method(
            decor_view,
            "getRootWindowInsets",
            "()Landroid/view/WindowInsets;",
            &[],
        )
        .ok()?
        .l()
        .ok()?;

    if insets.is_null() {
        tracing::trace!("getRootWindowInsets() returned null — view not attached yet");
        return None;
    }

    // System window insets include status bar (top) and nav bar + keyboard (bottom).
    // These methods are available API 20+ (our min is 26). Deprecated in API 30
    // but still functional on all current Android versions.
    let sys_top_px = env
        .call_method(&insets, "getSystemWindowInsetTop", "()I", &[])
        .and_then(|v| v.i())
        .unwrap_or(0);
    let sys_bottom_px = env
        .call_method(&insets, "getSystemWindowInsetBottom", "()I", &[])
        .and_then(|v| v.i())
        .unwrap_or(0);

    // Display cutout / notch (API 28+).
    // On API 26-27, getDisplayCutout() throws NoSuchMethodError which the
    // jni crate catches and clears, so we gracefully get 0.
    let cutout_top_px = query_display_cutout_top(env, &insets).unwrap_or(0);

    // Use the larger of system inset and cutout — the cutout may extend
    // beyond the status bar on some devices (e.g., wide notch).
    let top_px = sys_top_px.max(cutout_top_px);

    tracing::trace!(
        sys_top_px,
        cutout_top_px,
        sys_bottom_px,
        density,
        top_dp = top_px as f32 / density,
        bottom_dp = sys_bottom_px as f32 / density,
        "WindowInsets: system bar insets (px)"
    );

    Some(DisplayInsets {
        top: top_px as f32 / density,
        bottom: sys_bottom_px as f32 / density,
    })
}

/// Query the display cutout's safe inset top (API 28+).
///
/// Returns `None` on API < 28 or if no cutout exists. The jni crate
/// automatically clears the `NoSuchMethodError` thrown on older APIs.
fn query_display_cutout_top(env: &mut JNIEnv, insets: &JObject) -> Option<i32> {
    let cutout = match env.call_method(
        insets,
        "getDisplayCutout",
        "()Landroid/view/DisplayCutout;",
        &[],
    ) {
        Ok(val) => val.l().ok()?,
        Err(_) => return None, // API < 28
    };
    if cutout.is_null() {
        return None; // No cutout on this device
    }
    match env.call_method(&cutout, "getSafeInsetTop", "()I", &[]) {
        Ok(val) => val.i().ok(),
        Err(_) => None,
    }
}

/// Fallback: query insets via `getWindowVisibleDisplayFrame()`.
///
/// This works on all API levels but may under-report insets on NativeActivity
/// when the GL surface extends behind system bars. Used only when
/// `getRootWindowInsets()` is unavailable.
fn query_via_visible_frame(
    env: &mut JNIEnv,
    decor_view: &JObject,
    root_height: i32,
    density: f32,
) -> Option<DisplayInsets> {
    let rect = env.new_object("android/graphics/Rect", "()V", &[]).ok()?;
    env.call_method(
        decor_view,
        "getWindowVisibleDisplayFrame",
        "(Landroid/graphics/Rect;)V",
        &[JValue::Object(&rect)],
    )
    .ok()?;

    let visible_top = env.get_field(&rect, "top", "I").ok()?.i().ok()?;
    let visible_bottom = env.get_field(&rect, "bottom", "I").ok()?.i().ok()?;

    // Convert physical pixels to dp (Slint logical pixels)
    let top_dp = visible_top as f32 / density;
    let bottom_dp = ((root_height - visible_bottom).max(0)) as f32 / density;

    tracing::trace!(
        visible_top,
        visible_bottom,
        root_height,
        density,
        top_dp,
        bottom_dp,
        "VisibleFrame fallback: display insets"
    );

    Some(DisplayInsets {
        top: top_dp,
        bottom: bottom_dp,
    })
}

/// Get JNI environment for the current thread.
///
/// Caches the JavaVM wrapper in a static to avoid repeated allocations
/// (this function may be called frequently from a polling timer).
fn get_jni_env() -> Option<JNIEnv<'static>> {
    use std::sync::OnceLock;

    static VM: OnceLock<jni::JavaVM> = OnceLock::new();

    let vm = VM.get_or_init(|| {
        let ctx = unsafe { ndk_context::android_context() };
        unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }
            .expect("Failed to get JavaVM from android context")
    });

    vm.get_env()
        .or_else(|_| vm.attach_current_thread_permanently())
        .ok()
}
