//! Android clipboard via JNI.
//!
//! `arboard` (the desktop clipboard crate) has no Android backend, so every
//! Copy button in the app was compiled out on mobile behind
//! `#[cfg(any(target_os = "android", target_os = "ios"))]` and replaced with a
//! `tracing::warn!("Clipboard not supported on mobile")`. The friend-code button
//! then reported "Copied to clipboard!" to the user anyway — it had done nothing
//! (OPEN-ITEMS §112f).
//!
//! Copying matters here: the friend code IS the way you add someone, and the
//! safety number is what you compare out of band to verify a peer. So this uses
//! the framework `ClipboardManager` directly rather than removing the buttons.
//!
//! The Application context is enough for `CLIPBOARD_SERVICE` (no Activity
//! needed), and Slint's Android backend runs UI callbacks on the Java main
//! thread, which is where `setPrimaryClip` expects to be called.

use jni::objects::{JObject, JString, JValue};

use super::activity;

/// Put `text` on the system clipboard under `label`.
pub fn set_text(label: &str, text: &str) -> Result<(), String> {
    let mut env = activity::jni_env()?;

    let ctx = ndk_context::android_context();
    let context = unsafe { JObject::from_raw(ctx.context().cast()) };

    let service: JString = env
        .new_string("clipboard")
        .map_err(|e| format!("service name: {e}"))?;

    let manager = env
        .call_method(
            &context,
            "getSystemService",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            &[JValue::Object(&service)],
        )
        .map_err(|e| {
            activity::clear_exception(&mut env);
            format!("getSystemService(clipboard): {e}")
        })?
        .l()
        .map_err(|e| format!("manager cast: {e}"))?;

    let jlabel: JString = env
        .new_string(label)
        .map_err(|e| format!("label: {e}"))?;
    let jtext: JString = env
        .new_string(text)
        .map_err(|e| format!("text: {e}"))?;

    let clip = env
        .call_static_method(
            "android/content/ClipData",
            "newPlainText",
            "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Landroid/content/ClipData;",
            &[JValue::Object(&jlabel), JValue::Object(&jtext)],
        )
        .map_err(|e| {
            activity::clear_exception(&mut env);
            format!("ClipData.newPlainText: {e}")
        })?
        .l()
        .map_err(|e| format!("clip cast: {e}"))?;

    env.call_method(
        &manager,
        "setPrimaryClip",
        "(Landroid/content/ClipData;)V",
        &[JValue::Object(&clip)],
    )
    .map_err(|e| {
        activity::clear_exception(&mut env);
        format!("setPrimaryClip: {e}")
    })?;

    Ok(())
}
