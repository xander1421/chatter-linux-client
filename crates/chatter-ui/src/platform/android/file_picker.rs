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

use super::activity;

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
    let mut env = activity::jni_env()?;
    let act = activity::activity()?;

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
    let res = env.call_method(
        act,
        "launchFilePicker",
        "([Ljava/lang/String;)V",
        &[JValue::Object(&arr_obj)],
    );
    if res.is_err() {
        activity::clear_exception(&mut env);
    }
    res.map_err(|e| e.to_string())?;
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

// ── Saving (ACTION_CREATE_DOCUMENT) ─────────────────────────────────────────
//
// Inverted relative to picking: SAF gives a content:// URI, but core code
// writes to plain paths. So Kotlin stages a cache file, `save_file` hands that
// path back, the caller writes to it, and `finish_save` copies it into the URI.
// Two calls rather than one because the write happens in between.

static PENDING_SAVE: Mutex<Option<oneshot::Sender<Option<PathBuf>>>> = Mutex::new(None);

/// Open the "create document" chooser and await a writable staging path.
///
/// `None` means the user cancelled or the chooser failed. On `Some(path)`,
/// write the file there and then call [`finish_save`] with the same path —
/// without that second call nothing reaches the location the user chose.
pub async fn save_file(suggested_name: &str) -> Option<PathBuf> {
    let (tx, rx) = oneshot::channel();
    *PENDING_SAVE.lock().unwrap() = Some(tx);

    if let Err(e) = launch_saver(suggested_name) {
        tracing::error!("Android file saver: failed to launch chooser: {e}");
        *PENDING_SAVE.lock().unwrap() = None;
        return None;
    }

    rx.await.ok().flatten()
}

/// JNI: invoke `MainActivity.launchFileSaver(String suggestedName)`.
fn launch_saver(suggested_name: &str) -> Result<(), String> {
    let mut env = activity::jni_env()?;
    let act = activity::activity()?;

    let name = env.new_string(suggested_name).map_err(|e| e.to_string())?;
    let res = env.call_method(
        act,
        "launchFileSaver",
        "(Ljava/lang/String;)V",
        &[JValue::Object(&name)],
    );
    if res.is_err() {
        activity::clear_exception(&mut env);
    }
    res.map_err(|e| e.to_string())?;
    Ok(())
}

/// Copy the written staging file into the URI the user chose.
///
/// Returns false if the copy failed or no save was pending — callers must not
/// report success on a false.
pub fn finish_save(staging: &std::path::Path) -> bool {
    match finish_save_inner(staging) {
        Ok(ok) => ok,
        Err(e) => {
            tracing::error!("Android file saver: finish failed: {e}");
            false
        }
    }
}

fn finish_save_inner(staging: &std::path::Path) -> Result<bool, String> {
    let mut env = activity::jni_env()?;
    let act = activity::activity()?;

    let path = env
        .new_string(staging.to_string_lossy().as_ref())
        .map_err(|e| e.to_string())?;
    let res = env.call_method(
        act,
        "finishFileSave",
        "(Ljava/lang/String;)Z",
        &[JValue::Object(&path)],
    );
    if res.is_err() {
        activity::clear_exception(&mut env);
    }
    res.map_err(|e| e.to_string())?.z().map_err(|e| e.to_string())
}

/// JNI callback from `MainActivity.nativeOnSaveTargetChosen`. An empty string
/// means the user cancelled.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeOnSaveTargetChosen(
    mut env: JNIEnv,
    _this: JObject,
    staging_path: JString,
) {
    let path: Option<PathBuf> = match env.get_string(&staging_path) {
        Ok(s) => {
            let s: String = s.into();
            if s.is_empty() { None } else { Some(PathBuf::from(s)) }
        }
        Err(e) => {
            tracing::error!("Android file saver: bad callback payload: {e}");
            None
        }
    };

    tracing::info!("Android file saver: target {}", if path.is_some() { "chosen" } else { "cancelled" });
    if let Some(tx) = PENDING_SAVE.lock().unwrap().take() {
        let _ = tx.send(path);
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
