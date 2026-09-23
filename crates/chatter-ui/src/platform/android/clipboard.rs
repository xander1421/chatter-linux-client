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

/// Read the system clipboard's primary text item, if any.
///
/// The Copy half of this module is useless without it: pasting a friend code is
/// how you act on one someone sent you, and the desktop path here shells out to
/// wl-paste / xclip / arboard, none of which exist on Android — so Paste logged
/// "no clipboard tool succeeded" and did nothing (OPEN-ITEMS §112f).
pub fn get_text() -> Result<Option<String>, String> {
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

    let clip = env
        .call_method(
            &manager,
            "getPrimaryClip",
            "()Landroid/content/ClipData;",
            &[],
        )
        .map_err(|e| {
            activity::clear_exception(&mut env);
            format!("getPrimaryClip: {e}")
        })?
        .l()
        .map_err(|e| format!("clip cast: {e}"))?;

    if clip.is_null() {
        return Ok(None);
    }

    let count = env
        .call_method(&clip, "getItemCount", "()I", &[])
        .map_err(|e| {
            activity::clear_exception(&mut env);
            format!("getItemCount: {e}")
        })?
        .i()
        .map_err(|e| format!("count cast: {e}"))?;
    if count <= 0 {
        return Ok(None);
    }

    let item = env
        .call_method(
            &clip,
            "getItemAt",
            "(I)Landroid/content/ClipData$Item;",
            &[JValue::Int(0)],
        )
        .map_err(|e| {
            activity::clear_exception(&mut env);
            format!("getItemAt: {e}")
        })?
        .l()
        .map_err(|e| format!("item cast: {e}"))?;

    // coerceToText rather than getText: it also renders a URI/intent item as
    // text, which is what a code pasted from another app may arrive as.
    let text = env
        .call_method(
            &item,
            "coerceToText",
            "(Landroid/content/Context;)Ljava/lang/CharSequence;",
            &[JValue::Object(&context)],
        )
        .map_err(|e| {
            activity::clear_exception(&mut env);
            format!("coerceToText: {e}")
        })?
        .l()
        .map_err(|e| format!("text cast: {e}"))?;

    if text.is_null() {
        return Ok(None);
    }

    let s = env
        .call_method(&text, "toString", "()Ljava/lang/String;", &[])
        .map_err(|e| {
            activity::clear_exception(&mut env);
            format!("toString: {e}")
        })?
        .l()
        .map_err(|e| format!("string cast: {e}"))?;

    let out: String = env
        .get_string(&JString::from(s))
        .map_err(|e| format!("get_string: {e}"))?
        .into();
    Ok(Some(out))
}
