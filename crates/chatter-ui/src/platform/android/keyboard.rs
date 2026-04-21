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
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayInsets {
    pub top: f32,
    pub bottom: f32,
}

/// Query current display insets from Android via JNI.
pub fn query_display_insets() -> Option<DisplayInsets> {
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

    let window = env
        .call_method(&activity, "getWindow", "()Landroid/view/Window;", &[])
        .ok()?.l().ok()?;
    let decor_view = env
        .call_method(&window, "getDecorView", "()Landroid/view/View;", &[])
        .ok()?.l().ok()?;

    let root_view = env
        .call_method(&decor_view, "getRootView", "()Landroid/view/View;", &[])
        .ok()?.l().ok()?;
    let root_height = env
        .call_method(&root_view, "getHeight", "()I", &[])
        .ok()?.i().ok()?;

    if root_height <= 0 {
        return None;
    }

    if let Some(insets) = query_via_window_insets(&mut env, &decor_view, density) {
        return Some(insets);
    }

    query_via_visible_frame(&mut env, &decor_view, root_height, density)
}

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

    let sys_top_px = env
        .call_method(&insets, "getSystemWindowInsetTop", "()I", &[])
        .and_then(|v| v.i())
        .unwrap_or(0);
    let sys_bottom_px = env
        .call_method(&insets, "getSystemWindowInsetBottom", "()I", &[])
        .and_then(|v| v.i())
        .unwrap_or(0);

    let cutout_top_px = query_display_cutout_top(env, &insets).unwrap_or(0);

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

fn query_display_cutout_top(env: &mut JNIEnv, insets: &JObject) -> Option<i32> {
    let cutout = match env.call_method(
        insets,
        "getDisplayCutout",
        "()Landroid/view/DisplayCutout;",
        &[],
    ) {
        Ok(val) => val.l().ok()?,
        Err(_) => return None,
    };
    if cutout.is_null() {
        return None;
    }
    match env.call_method(&cutout, "getSafeInsetTop", "()I", &[]) {
        Ok(val) => val.i().ok(),
        Err(_) => None,
    }
}

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
