//! Android runtime permissions via JNI.
//!
//! Android 6+ requires *dangerous* permissions (camera, microphone) and
//! Android 13+ requires POST_NOTIFICATIONS to be granted at runtime — declaring
//! them in the manifest is not enough. Slint/Rust can't show the system grant
//! dialog itself, so we bridge to Kotlin: `MainActivity.requestPermissions`
//! fires the grant dialog and reports back through `nativeOnPermissionsResult`.
//!
//! `request()` is fire-and-forget (the dialog is async and the user drives it);
//! the result callback updates logging/telemetry. Call `has()` first to skip
//! re-prompting when already granted. `ensure()` is the awaitable form, for an
//! action that must not start until the user has answered (placing a call).

use jni::JNIEnv;
use jni::objects::{JObject, JObjectArray, JString, JValue};
use jni::sys::jsize;
use std::sync::Mutex;

use super::activity;

/// The `ensure()` call waiting on the dialog, woken by the result callback.
/// One at a time: a newer request drops the older sender, which wakes that
/// waiter too, and it then reads the grant state like any other.
static WAITER: Mutex<Option<tokio::sync::oneshot::Sender<()>>> = Mutex::new(None);

/// Common dangerous / special permissions we request.
pub const CAMERA: &str = "android.permission.CAMERA";
pub const RECORD_AUDIO: &str = "android.permission.RECORD_AUDIO";
pub const POST_NOTIFICATIONS: &str = "android.permission.POST_NOTIFICATIONS";

/// True if `permission` is already granted (ContextCompat.checkSelfPermission).
pub fn has(permission: &str) -> bool {
    match check(permission) {
        Ok(granted) => granted,
        Err(e) => {
            tracing::error!("Android permissions: hasPermission({permission}) failed: {e}");
            false
        }
    }
}

/// Request the given permissions. Fire-and-forget — the system dialog is shown
/// and the outcome is delivered later to `nativeOnPermissionsResult`. Already
/// granted permissions are filtered out so the user isn't re-prompted.
pub fn request(permissions: &[&str]) {
    let pending: Vec<&str> = permissions.iter().copied().filter(|p| !has(p)).collect();
    if pending.is_empty() {
        tracing::debug!("Android permissions: all already granted, no prompt");
        return;
    }
    if let Err(e) = launch_request(&pending) {
        tracing::error!("Android permissions: request failed: {e}");
    }
}

/// Request any of `permissions` not yet granted and wait for the user's answer.
/// True when all of them are granted afterwards.
pub async fn ensure(permissions: &[&str]) -> bool {
    let pending: Vec<&str> = permissions.iter().copied().filter(|p| !has(p)).collect();
    if pending.is_empty() {
        return true;
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    *WAITER.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    if let Err(e) = launch_request(&pending) {
        tracing::error!("Android permissions: request failed: {e}");
        return false;
    }
    let _ = rx.await;
    pending.iter().all(|p| has(p))
}

fn check(permission: &str) -> Result<bool, String> {
    let mut env = activity::jni_env()?;
    let act = activity::activity()?;
    let perm = env.new_string(permission).map_err(|e| e.to_string())?;
    let res = env.call_method(
        act,
        "hasPermission",
        "(Ljava/lang/String;)Z",
        &[JValue::Object(&perm)],
    );
    if res.is_err() {
        activity::clear_exception(&mut env);
    }
    res.map_err(|e| e.to_string())?
        .z()
        .map_err(|e| e.to_string())
}

fn launch_request(permissions: &[&str]) -> Result<(), String> {
    let mut env = activity::jni_env()?;
    let act = activity::activity()?;
    let string_class = env
        .find_class("java/lang/String")
        .map_err(|e| e.to_string())?;
    let arr = env
        .new_object_array(permissions.len() as jsize, &string_class, JObject::null())
        .map_err(|e| e.to_string())?;
    for (i, p) in permissions.iter().enumerate() {
        let s = env.new_string(p).map_err(|e| e.to_string())?;
        env.set_object_array_element(&arr, i as jsize, &s)
            .map_err(|e| e.to_string())?;
    }
    let arr_obj: JObject = arr.into();
    let res = env.call_method(
        act,
        "requestPermissions",
        "([Ljava/lang/String;)V",
        &[JValue::Object(&arr_obj)],
    );
    if res.is_err() {
        activity::clear_exception(&mut env);
    }
    res.map_err(|e| e.to_string())?;
    tracing::info!("Android permissions: requested {:?}", permissions);
    Ok(())
}

/// JNI callback from `MainActivity.nativeOnPermissionsResult`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeOnPermissionsResult(
    mut env: JNIEnv,
    _this: JObject,
    granted: JObjectArray,
    denied: JObjectArray,
) {
    let g = read_string_array(&mut env, &granted).unwrap_or_default();
    let d = read_string_array(&mut env, &denied).unwrap_or_default();
    if !g.is_empty() {
        tracing::info!("Android permissions granted: {:?}", g);
    }
    if !d.is_empty() {
        tracing::warn!("Android permissions denied: {:?}", d);
    }
    if let Some(waiter) = WAITER.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = waiter.send(());
    }
}

fn read_string_array(env: &mut JNIEnv, arr: &JObjectArray) -> Result<Vec<String>, String> {
    let len = env.get_array_length(arr).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(len as usize);
    for i in 0..len {
        let elem = env
            .get_object_array_element(arr, i)
            .map_err(|e| e.to_string())?;
        let s: String = env
            .get_string(&JString::from(elem))
            .map_err(|e| e.to_string())?
            .into();
        out.push(s);
    }
    Ok(out)
}

// Requested only from mesh user actions, never from startup.
pub async fn ensure_mesh() -> bool {
    let sdk = activity::jni_env().ok().and_then(|mut env| {
        env.get_static_field("android/os/Build$VERSION", "SDK_INT", "I")
            .ok()?
            .i()
            .ok()
    });
    match sdk {
        Some(31..) => {
            ensure(&[
                "android.permission.BLUETOOTH_SCAN",
                "android.permission.BLUETOOTH_ADVERTISE",
                "android.permission.BLUETOOTH_CONNECT",
            ])
            .await
        }
        Some(_) => ensure(&["android.permission.ACCESS_FINE_LOCATION"]).await,
        None => false,
    }
}

pub fn mesh_location_enabled() -> bool {
    let Ok(mut env) = activity::jni_env() else {
        return false;
    };
    let Ok(act) = activity::activity() else {
        return false;
    };
    env.call_method(act, "meshLocationEnabled", "()Z", &[])
        .ok()
        .and_then(|value| value.z().ok())
        .unwrap_or(false)
}
