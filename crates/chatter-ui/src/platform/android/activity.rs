//! Shared handle to the Kotlin `MainActivity` for JNI calls.
//!
//! `ndk_context::android_context().context()` returns the *Application*, not the
//! Activity — fine for framework calls (NotificationManager etc.) but it has no
//! `MainActivity` methods (`launchFilePicker`, `requestPermissions`). So the
//! Activity registers itself with us in `onCreate` via `nativeRegisterActivity`,
//! and we keep a process-lifetime `GlobalRef` to call back into it.

use std::sync::OnceLock;

use jni::objects::{GlobalRef, JObject};
use jni::{JNIEnv, JavaVM};

static ACTIVITY: OnceLock<GlobalRef> = OnceLock::new();

/// Attach the current thread and get a `'static` JNIEnv. The JavaVM lives for
/// the whole process, so leaking it is safe (mirrors notifications.rs).
pub fn jni_env() -> Result<JNIEnv<'static>, String> {
    let ctx = unsafe { ndk_context::android_context() };
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) }.map_err(|e| e.to_string())?;
    let vm = Box::leak(Box::new(vm));
    vm.get_env()
        .or_else(|_| vm.attach_current_thread_permanently())
        .map_err(|e| e.to_string())
}

/// The registered `MainActivity`, or an error if `onCreate` hasn't run yet.
pub fn activity() -> Result<&'static GlobalRef, String> {
    ACTIVITY
        .get()
        .ok_or_else(|| "MainActivity not registered yet".to_string())
}

/// Clear any pending JNI exception so a failed call can't abort the next one
/// (ART's CheckJNI SIGABRTs if a JNI call is made with an exception pending).
pub fn clear_exception(env: &mut JNIEnv) {
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
}

/// JNI: `MainActivity.onCreate` calls this so Rust can hold the Activity.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeRegisterActivity(
    env: JNIEnv,
    this: JObject,
) {
    match env.new_global_ref(&this) {
        Ok(gref) => {
            let _ = ACTIVITY.set(gref);
            tracing::info!("Android: MainActivity registered for JNI callbacks");
        }
        Err(e) => tracing::error!("Android: failed to hold MainActivity ref: {e}"),
    }
}

/// JNI: `MainActivity.onCreate` calls this to wire the JavaVM into libwebrtc.
///
/// Must run on the Java main thread (this is invoked from onCreate), NOT the
/// native `android_main` thread: libwebrtc's `InitAndroid` does `FindClass` for
/// the bundled `livekit.org.webrtc` / `jni_zero` classes, which only resolves
/// via the app classloader — a native thread sees only the system classloader
/// and aborts. Required before any `Room::connect`, or calls segfault on join.
#[cfg(feature = "call-ui")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeInitWebrtc(
    env: JNIEnv,
    _this: JObject,
) {
    match env.get_java_vm() {
        Ok(vm) => {
            livekit::webrtc::android::initialize_android(&vm);
            tracing::info!("LiveKit webrtc android runtime initialized");
        }
        Err(e) => tracing::error!("webrtc init: get_java_vm failed: {e}"),
    }
}
