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

use jni::objects::{JObject, JObjectArray, JString, JValue};
use jni::sys::jsize;
use jni::JNIEnv;
use tokio::sync::oneshot;

/// Single in-flight picker request. A new pick replaces (and thereby cancels)
/// any prior pending one — the UI only ever opens one chooser at a time.
static PENDING: Mutex<Option<oneshot::Sender<Vec<PathBuf>>>> = Mutex::new(None);

/// Open the Android document picker and await the chosen files.
///
/// Returns the cache-dir paths Kotlin copied the selections into, or an empty
/// vec if the user cancelled or anything went wrong (errors are logged, not
/// propagated — the caller treats empty as "nothing to attach").
pub async fn pick_files(mime_types: &[&str]) -> Vec<PathBuf> {
    let (tx, rx) = oneshot::channel();
    *PENDING.lock().unwrap() = Some(tx);

    if let Err(e) = launch(mime_types) {
        tracing::error!("Android file picker: failed to launch chooser: {e}");
        *PENDING.lock().unwrap() = None;
        return Vec::new();
    }

    // Sender is dropped (→ Err) if a newer pick replaces this one.
    rx.await.unwrap_or_default()
}

/// JNI: invoke `MainActivity.launchFilePicker(String[] mimeTypes)`.
fn launch(mime_types: &[&str]) -> Result<(), String> {
    let ctx = unsafe { ndk_context::android_context() };
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }.map_err(|e| e.to_string())?;
    let mut env = vm
        .attach_current_thread_permanently()
        .map_err(|e| e.to_string())?;
    let activity = unsafe { JObject::from_raw(ctx.context().cast()) };

    let string_class = env.find_class("java/lang/String").map_err(|e| e.to_string())?;
    let arr = env
        .new_object_array(mime_types.len() as jsize, &string_class, JObject::null())
        .map_err(|e| e.to_string())?;
    for (i, m) in mime_types.iter().enumerate() {
        let s = env.new_string(m).map_err(|e| e.to_string())?;
        env.set_object_array_element(&arr, i as jsize, &s)
            .map_err(|e| e.to_string())?;
    }

    let arr_obj: JObject = arr.into();
    env.call_method(
        &activity,
        "launchFilePicker",
        "([Ljava/lang/String;)V",
        &[JValue::Object(&arr_obj)],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// JNI callback from `MainActivity.nativeOnFilesPicked`. Kotlin passes the
/// cache-dir absolute paths of the files it copied (empty array on cancel).
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeOnFilesPicked(
    mut env: JNIEnv,
    _this: JObject,
    paths: JObjectArray,
) {
    let files = read_string_array(&mut env, &paths)
        .map(|v| v.into_iter().map(PathBuf::from).collect::<Vec<_>>())
        .unwrap_or_else(|e| {
            tracing::error!("Android file picker: bad callback payload: {e}");
            Vec::new()
        });

    tracing::info!("Android file picker: {} file(s) returned", files.len());
    if let Some(tx) = PENDING.lock().unwrap().take() {
        let _ = tx.send(files);
    }
}

fn read_string_array(env: &mut JNIEnv, arr: &JObjectArray) -> Result<Vec<String>, String> {
    let len = env.get_array_length(arr).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(len as usize);
    for i in 0..len {
        let elem = env.get_object_array_element(arr, i).map_err(|e| e.to_string())?;
        let s: String = env
            .get_string(&JString::from(elem))
            .map_err(|e| e.to_string())?
            .into();
        out.push(s);
    }
    Ok(out)
}
