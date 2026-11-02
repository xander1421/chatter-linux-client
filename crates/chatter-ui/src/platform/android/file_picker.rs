//! Android file picker via the Storage Access Framework.
//!
//! Slint/Rust has no native file chooser on Android (the desktop `rfd` path has
//! no Android backend), so we bridge to Kotlin: `MainActivity.launchFilePicker`
//! fires `ACTION_OPEN_DOCUMENT` (multi-select), copies the chosen content URIs
//! into the app cache dir, and calls back into Rust via `nativeOnFilesPicked`.
//! SAF grants per-file read access, so no storage permission is required.
//!
//! Flow:
//!   Rust `pick_files()` → JNI call `launchFilePicker` → (user picks) →
//!   Kotlin copies URIs → JNI `nativeOnFilesPicked(String[])` → oneshot → Rust.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

use jni::Env;
use jni::objects::{JObject, JObjectArray, JString, JValue};
use jni::sys::jsize;
use tokio::sync::oneshot;

use super::activity;

/// Single in-flight picker request. A new pick replaces (and thereby cancels)
/// any prior pending one — the UI only ever opens one chooser at a time.
static PENDING: Mutex<Option<(i64, oneshot::Sender<Vec<PathBuf>>)>> = Mutex::new(None);
static NEXT_REQUEST: AtomicI64 = AtomicI64::new(1);

/// Open the Android document picker and await the chosen files.
///
/// Returns the cache-dir paths Kotlin copied the selections into, or an empty
/// vec if the user cancelled or anything went wrong (errors are logged, not
/// propagated — the caller treats empty as "nothing to attach").
pub async fn pick_files(mime_types: &[&str]) -> Vec<PathBuf> {
    let (tx, rx) = oneshot::channel();
    let request = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
    *PENDING.lock().unwrap() = Some((request, tx));

    if let Err(e) = launch(mime_types, request) {
        tracing::error!("Android file picker: failed to launch chooser: {e}");
        let mut pending = PENDING.lock().unwrap();
        if pending.as_ref().is_some_and(|(id, _)| *id == request) {
            pending.take();
        }
        return Vec::new();
    }

    // Sender is dropped (→ Err) if a newer pick replaces this one.
    rx.await.unwrap_or_default()
}

/// Open the Telegram-style attachment source chooser (gallery photo picker /
/// document picker / in-app camera) and await the files the chosen source
/// produced.
///
/// Every source delivers through the same `nativeOnFilesPicked` callback the
/// plain document picker uses, so the result funnels into one staging pipeline
/// and the caller treats a chooser pick exactly like a file pick. Empty on
/// cancel (of either the chooser or the chosen source).
pub async fn pick_via_chooser() -> Vec<PathBuf> {
    let (tx, rx) = oneshot::channel();
    let request = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
    *PENDING.lock().unwrap() = Some((request, tx));

    if let Err(e) = launch_chooser(request) {
        tracing::error!("Android attachment chooser: failed to open: {e}");
        let mut pending = PENDING.lock().unwrap();
        if pending.as_ref().is_some_and(|(id, _)| *id == request) {
            pending.take();
        }
        return Vec::new();
    }

    // Sender is dropped (→ Err) if a newer pick replaces this one.
    rx.await.unwrap_or_default()
}

/// JNI: invoke `MainActivity.launchAttachmentChooser(long request)`.
fn launch_chooser(request: i64) -> Result<(), String> {
    activity::with_jni_env(|env| {
        let act = activity::activity()?;
        let res = env.call_method(
            act,
            jni::jni_str!("launchAttachmentChooser"),
            jni::jni_sig!("(J)V"),
            &[JValue::Long(request)],
        );
        if res.is_err() {
            activity::clear_exception(env);
        }
        res.map_err(|e| e.to_string())?;
        Ok(())
    })
}

/// JNI: invoke `MainActivity.launchFilePicker(String[] mimeTypes)`.
fn launch(mime_types: &[&str], request: i64) -> Result<(), String> {
    activity::with_jni_env(|env| {
        let act = activity::activity()?;

        let string_class = env
            .find_class(jni::jni_str!("java/lang/String"))
            .map_err(|e| e.to_string())?;
        let arr = env
            .new_object_array(mime_types.len() as jsize, &string_class, JObject::null())
            .map_err(|e| e.to_string())?;
        for (i, m) in mime_types.iter().enumerate() {
            let s = env.new_string(m).map_err(|e| e.to_string())?;
            arr.set_element(env, i, &s).map_err(|e| e.to_string())?;
        }

        let arr_obj: JObject = arr.into();
        let res = env.call_method(
            act,
            jni::jni_str!("launchFilePicker"),
            jni::jni_sig!("([Ljava/lang/String;J)V"),
            &[JValue::Object(&arr_obj), JValue::Long(request)],
        );
        if res.is_err() {
            activity::clear_exception(env);
        }
        res.map_err(|e| e.to_string())?;
        Ok(())
    })
}

/// JNI callback from `MainActivity.nativeOnFilesPicked`. Kotlin passes the
/// cache-dir absolute paths of the files it copied (empty array on cancel).
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeOnFilesPicked(
    mut env: jni::EnvUnowned<'_>,
    _this: JObject,
    request: jni::sys::jlong,
    paths: JObjectArray,
) -> jni::sys::jboolean {
    env.with_env(|env| -> jni::errors::Result<_> {
        Ok((|| {
            let files = read_string_array(env, &paths)
                .map(|v| v.into_iter().map(PathBuf::from).collect::<Vec<_>>())
                .unwrap_or_else(|e| {
                    tracing::error!("Android file picker: bad callback payload: {e}");
                    Vec::new()
                });

            tracing::info!("Android file picker: {} file(s) returned", files.len());
            let mut pending = PENDING.lock().unwrap();
            if pending.as_ref().is_some_and(|(id, _)| *id == request) {
                let (_, tx) = pending.take().unwrap();
                return tx.send(files).is_ok() as jni::sys::jboolean;
            }
            jni::sys::JNI_FALSE
        })())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}

fn read_string_array(env: &mut Env, arr: &JObjectArray) -> Result<Vec<String>, String> {
    let len = arr.len(&env).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(len as usize);
    for i in 0..len {
        let elem = arr.get_element(env, i).map_err(|e| e.to_string())?;
        let s: String = (env.cast_local::<JString>(elem).map_err(|e| e.to_string())?)
            .try_to_string(&env)
            .map_err(|e| e.to_string())?
            .into();
        out.push(s);
    }
    Ok(out)
}
