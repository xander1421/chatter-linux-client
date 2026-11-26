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

use jni::Env;
use jni::objects::{JObject, JObjectArray, JString, JValue};
use jni::sys::{jlong, jsize};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

use super::activity;

// Fire-and-forget requests have no waiter. Give every awaitable request its own
// ID so older results, including cancelled requests, cannot wake a newer call.
const NO_WAITER: jlong = 0;
static NEXT_REQUEST_ID: AtomicI64 = AtomicI64::new(1);

struct PermissionWaiter {
    request_id: jlong,
    // False when the platform could not show a dialog or cancelled it.
    answered: tokio::sync::oneshot::Sender<bool>,
}

static WAITER: Mutex<Option<PermissionWaiter>> = Mutex::new(None);
// Another awaitable request checks the grant state after the current one ends.
static REQUEST_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn take_waiter(request_id: jlong) -> Option<PermissionWaiter> {
    let mut waiter = WAITER.lock().unwrap_or_else(|e| e.into_inner());
    if waiter.as_ref().is_some_and(|waiter| waiter.request_id == request_id) {
        waiter.take()
    } else {
        None
    }
}

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
    if let Err(e) = launch_request(&pending, NO_WAITER) {
        tracing::error!("Android permissions: request failed: {e}");
    }
}

/// Request any of `permissions` not yet granted and wait for the user's answer.
/// True when all of them are granted afterwards.
pub async fn ensure(permissions: &[&str]) -> bool {
    ensure_answered(permissions).await.unwrap_or(false)
}

/// Like `ensure`, but `None` when the dialog never got an answer (the system
/// cancelled it). A caller that would act on a refusal must not treat that
/// as one: the user was never asked.
pub async fn ensure_answered(permissions: &[&str]) -> Option<bool> {
    let _gate = REQUEST_GATE.lock().await;
    let pending: Vec<&str> = permissions.iter().copied().filter(|p| !has(p)).collect();
    if pending.is_empty() {
        return Some(true);
    }
    let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = tokio::sync::oneshot::channel();
    *WAITER.lock().unwrap_or_else(|e| e.into_inner()) = Some(PermissionWaiter {
        request_id,
        answered: tx,
    });
    if let Err(e) = launch_request(&pending, request_id) {
        take_waiter(request_id);
        tracing::error!("Android permissions: request failed: {e}");
        return Some(false);
    }
    let answered = rx.await.unwrap_or(false);
    let granted = pending.iter().all(|p| has(p));
    (answered || granted).then_some(granted)
}

/// Explain a denied call permission and offer the Android app-settings page.
pub fn show_call_permission_denied(permission: &str) {
    if let Err(e) = activity::with_jni_env(|env| {
        let act = activity::activity()?;
        let name = env.new_string(permission).map_err(|e| e.to_string())?;
        let result = env.call_method(
            act,
            jni::jni_str!("showCallPermissionDenied"),
            jni::jni_sig!("(Ljava/lang/String;)V"),
            &[JValue::Object(&name)],
        );
        if result.is_err() {
            activity::clear_exception(env);
        }
        result.map_err(|e| e.to_string())?;
        Ok(())
    }) {
        tracing::error!("Android permissions: denied-call explanation failed: {e}");
    }
}

fn check(permission: &str) -> Result<bool, String> {
    activity::with_jni_env(|env| {
        let act = activity::activity()?;
        let perm = env.new_string(permission).map_err(|e| e.to_string())?;
        let res = env.call_method(
            act,
            jni::jni_str!("hasPermission"),
            jni::jni_sig!("(Ljava/lang/String;)Z"),
            &[JValue::Object(&perm)],
        );
        if res.is_err() {
            activity::clear_exception(env);
        }
        res.map_err(|e| e.to_string())?
            .z()
            .map_err(|e| e.to_string())
    })
}

fn launch_request(permissions: &[&str], request_id: jlong) -> Result<(), String> {
    activity::with_jni_env(|env| {
        let act = activity::activity()?;
        let string_class = env
            .find_class(jni::jni_str!("java/lang/String"))
            .map_err(|e| e.to_string())?;
        let arr = env
            .new_object_array(permissions.len() as jsize, &string_class, JObject::null())
            .map_err(|e| e.to_string())?;
        for (i, p) in permissions.iter().enumerate() {
            let s = env.new_string(p).map_err(|e| e.to_string())?;
            arr.set_element(env, i, &s).map_err(|e| e.to_string())?;
        }
        let arr_obj: JObject = arr.into();
        let res = env.call_method(
            act,
            jni::jni_str!("requestPermissions"),
            jni::jni_sig!("([Ljava/lang/String;J)V"),
            &[JValue::Object(&arr_obj), JValue::Long(request_id)],
        );
        if res.is_err() {
            activity::clear_exception(env);
        }
        res.map_err(|e| e.to_string())?;
        tracing::info!("Android permissions: requested {:?}", permissions);
        Ok(())
    })
}

/// JNI callback from `MainActivity.nativeOnPermissionsResult`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeOnPermissionsResult(
    mut env: jni::EnvUnowned<'_>,
    _this: JObject,
    request_id: jlong,
    granted: JObjectArray,
    denied: JObjectArray,
) {
    env.with_env(|env| -> jni::errors::Result<_> {
        Ok((|| {
            let g = read_string_array(env, &granted).unwrap_or_default();
            let d = read_string_array(env, &denied).unwrap_or_default();
            if !g.is_empty() {
                tracing::info!("Android permissions granted: {:?}", g);
            }
            if !d.is_empty() {
                tracing::warn!("Android permissions denied: {:?}", d);
            }
            if request_id != NO_WAITER && let Some(waiter) = take_waiter(request_id) {
                let _ = waiter.answered.send(!g.is_empty() || !d.is_empty());
            }
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

// Requested only from mesh user actions, never from startup.
pub async fn ensure_mesh() -> bool {
    let sdk = activity::with_jni_env(|env| {
        env.get_static_field(
            jni::jni_str!("android/os/Build$VERSION"),
            jni::jni_str!("SDK_INT"),
            jni::jni_sig!("I"),
        )
        .and_then(|value| value.i())
        .map_err(|e| e.to_string())
    })
    .ok();
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
    activity::with_jni_env(|env| {
        let act = activity::activity()?;
        env.call_method(
            act,
            jni::jni_str!("meshLocationEnabled"),
            jni::jni_sig!("()Z"),
            &[],
        )
        .and_then(|value| value.z())
        .map_err(|e| e.to_string())
    })
    .unwrap_or(false)
}
