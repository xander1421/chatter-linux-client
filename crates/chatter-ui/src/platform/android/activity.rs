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
    let ctx = ndk_context::android_context();
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

/// Set or clear `FLAG_SECURE` on the Activity window (`MainActivity.setSecure`).
///
/// Bound to the Slint `secure-screen` callback, which `main.slint` fires from
/// its `changed active-modal` handler. Change handlers run before the next
/// frame is rendered, so the flag is requested before the secret is drawn.
pub fn set_secure(secure: bool) {
    let result = (|| -> Result<(), String> {
        let act = activity()?;
        let mut env = jni_env()?;
        let res = env.call_method(act, "setSecure", "(Z)V", &[jni::objects::JValue::Bool(secure.into())]);
        if res.is_err() {
            clear_exception(&mut env);
        }
        res.map(|_| ()).map_err(|e| e.to_string())
    })();
    if let Err(e) = result {
        tracing::error!("Android: setSecure({secure}) failed: {e}");
    }
}

/// Mirror Settings › Appearance › Theme into `MainActivity`
/// (`setThemeMode`), whose `onCreate` paints the pre-first-frame window
/// background from it — it runs before the native side can say anything.
pub fn set_theme_mode(mode: &str) {
    let result = (|| -> Result<(), String> {
        let act = activity()?;
        let mut env = jni_env()?;
        let jmode = env.new_string(mode).map_err(|e| e.to_string())?;
        let res = env.call_method(
            act,
            "setThemeMode",
            "(Ljava/lang/String;)V",
            &[jni::objects::JValue::Object(&jmode)],
        );
        if res.is_err() {
            clear_exception(&mut env);
        }
        res.map(|_| ()).map_err(|e| e.to_string())
    })();
    if let Err(e) = result {
        tracing::error!("Android: setThemeMode({mode}) failed: {e}");
    }
}

/// Dark status/nav-bar icons for the light palette, light ones for the dark
/// (`MainActivity.setLightSystemBars`). The app draws edge-to-edge, so the
/// icons sit on Slint's own background.
pub fn set_light_system_bars(light: bool) {
    let result = (|| -> Result<(), String> {
        let act = activity()?;
        let mut env = jni_env()?;
        let res = env.call_method(
            act,
            "setLightSystemBars",
            "(Z)V",
            &[jni::objects::JValue::Bool(light.into())],
        );
        if res.is_err() {
            clear_exception(&mut env);
        }
        res.map(|_| ()).map_err(|e| e.to_string())
    })();
    if let Err(e) = result {
        tracing::error!("Android: setLightSystemBars({light}) failed: {e}");
    }
}

/// Lift `MainActivity`'s launch cover (the theme colour painted over the
/// SurfaceView until Slint has drawn into it) once the first frame is rendered.
/// Kotlin also lifts it on a timeout, so a frame that never reports here
/// cannot leave the app covered.
pub fn clear_launch_cover_after_first_frame(window: &slint::Window) {
    let mut done = false;
    let res = window.set_rendering_notifier(move |state, _| {
        crate::record_startup_frame(&state);
        if done || !matches!(state, slint::RenderingState::AfterRendering) {
            return;
        }
        done = true;
        let result = (|| -> Result<(), String> {
            let act = activity()?;
            let mut env = jni_env()?;
            let res = env.call_method(act, "firstFrameDrawn", "()V", &[]);
            if res.is_err() {
                clear_exception(&mut env);
            }
            res.map(|_| ()).map_err(|e| e.to_string())
        })();
        if let Err(e) = result {
            tracing::error!("Android: firstFrameDrawn failed: {e}");
        }
    });
    if let Err(e) = res {
        tracing::error!("Android: no rendering notifier for the launch cover: {e:?}");
    }
}

/// JNI entry point: `MainActivity`'s surface view hid the soft keyboard for a
/// Back press (`onKeyPreIme`). Hiding it through the insets controller does
/// not tell Slint, so the TextInput kept focus (caret and focus ring stayed)
/// and Slint still believed the keyboard was wanted — the likely reason it
/// stayed or came straight back over New Channel on both phones (not verified
/// on device). Drop the text focus the same way leaving a screen does, so
/// Slint's state matches what is on screen.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeImeHiddenByBack(
    _env: JNIEnv,
    _class: jni::objects::JClass,
) {
    let Some(weak) = super::call_actions::main_window() else { return };
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.invoke_release_text_focus();
        }
    });
}

/// JNI entry point: the soft keyboard's action key (Done) — see
/// `MainActivity.onEditorAction` for why it does not reach Slint on its own.
/// Delivered as an Enter press, so the focused TextInput's `accepted` fires
/// as it does for Enter on desktop.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeImeEditorAction(
    _env: JNIEnv,
    _class: jni::objects::JClass,
) {
    let Some(weak) = super::call_actions::main_window() else { return };
    let _ = slint::invoke_from_event_loop(move || {
        use slint::ComponentHandle;
        let Some(w) = weak.upgrade() else { return };
        let enter: slint::SharedString = slint::platform::Key::Return.into();
        w.window().dispatch_event(slint::platform::WindowEvent::KeyPressed { text: enter.clone() });
        w.window().dispatch_event(slint::platform::WindowEvent::KeyReleased { text: enter });
    });
}
