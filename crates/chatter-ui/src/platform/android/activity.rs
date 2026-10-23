//! Shared handle to the Kotlin `MainActivity` for JNI calls.
//!
//! `ndk_context::android_context().context()` returns the *Application*, not the
//! Activity — fine for framework calls (NotificationManager etc.) but it has no
//! `MainActivity` methods (`launchFilePicker`, `requestPermissions`). So the
//! Activity registers itself with us in `onCreate` via `nativeRegisterActivity`,
//! and we keep a process-lifetime `Global<JObject<'static>>` to call back into it.

use std::sync::OnceLock;

use jni::objects::{Global, JObject};
use jni::{Env, JavaVM};

static ACTIVITY: OnceLock<Global<JObject<'static>>> = OnceLock::new();

/// Run a JNI operation inside a managed thread attachment and local frame.
pub fn with_jni_env<T>(body: impl FnOnce(&mut Env<'_>) -> Result<T, String>) -> Result<T, String> {
    let ctx = ndk_context::android_context();
    let raw_vm = ctx.vm();
    if raw_vm.is_null() {
        return Err("Android runtime has no JavaVM".to_owned());
    }
    // SAFETY: ndk-context supplies the process VM; null is rejected above.
    let vm = unsafe { JavaVM::from_raw(raw_vm.cast()) };
    vm.attach_current_thread_for_scope(|env| Ok::<_, jni::errors::Error>(body(env)))
        .map_err(|e| e.to_string())?
}

/// The registered `MainActivity`, or an error if `onCreate` hasn't run yet.
pub fn activity() -> Result<&'static Global<JObject<'static>>, String> {
    ACTIVITY
        .get()
        .ok_or_else(|| "MainActivity not registered yet".to_string())
}

/// Handoff only a digest-verified APK in app-private staging to PackageInstaller.
pub fn install_verified_update(path: &str, version_code: i64) -> Result<(), String> {
    let act = activity()?;
    with_jni_env(|env| {
        let jpath = env.new_string(path).map_err(|e| e.to_string())?;
        let result = env.call_method(
            act,
            jni::jni_str!("installVerifiedUpdate"),
            jni::jni_sig!("(Ljava/lang/String;J)Z"),
            &[
                jni::objects::JValue::Object(&jpath),
                jni::objects::JValue::Long(version_code),
            ],
        );
        if result.is_err() {
            clear_exception(env);
        }
        match result.and_then(|value| value.z()) {
            Ok(true) => Ok(()),
            Ok(false) => Err("Allow Chatter to install updates, then tap Update again".into()),
            Err(error) => Err(error.to_string()),
        }
    })
}

pub fn update_network_metered() -> Result<bool, String> {
    let act = activity()?;
    with_jni_env(|env| {
        let result = env.call_method(
            act,
            jni::jni_str!("isUpdateNetworkMetered"),
            jni::jni_sig!("()Z"),
            &[],
        );
        if result.is_err() {
            clear_exception(env);
        }
        result
            .and_then(|value| value.z())
            .map_err(|e| e.to_string())
    })
}

pub fn is_play_installed() -> Result<bool, String> {
    let act = activity()?;
    with_jni_env(|env| {
        let result = env.call_method(
            act,
            jni::jni_str!("isPlayInstalled"),
            jni::jni_sig!("()Z"),
            &[],
        );
        if result.is_err() {
            clear_exception(env);
        }
        result
            .and_then(|value| value.z())
            .map_err(|e| e.to_string())
    })
}

pub fn open_play_store_update() -> Result<(), String> {
    let act = activity()?;
    with_jni_env(|env| {
        let url = env
            .new_string("https://play.google.com/store/apps/details?id=com.chatter.client")
            .map_err(|e| e.to_string())?;
        let result = env.call_method(
            act,
            jni::jni_str!("openWebLink"),
            jni::jni_sig!("(Ljava/lang/String;)V"),
            &[jni::objects::JValue::Object(&url)],
        );
        if result.is_err() {
            clear_exception(env);
        }
        result.map(|_| ()).map_err(|e| e.to_string())
    })
}

/// Clear any pending JNI exception so a failed call can't abort the next one
/// (ART's CheckJNI SIGABRTs if a JNI call is made with an exception pending).
pub fn clear_exception(env: &mut Env) {
    if env.exception_check() {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
}

/// JNI: `MainActivity.onCreate` calls this so Rust can hold the Activity.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeRegisterActivity(
    mut env: jni::EnvUnowned<'_>,
    this: JObject,
) {
    env.with_env(|env| -> jni::errors::Result<_> {
        Ok((|| match env.new_global_ref(&this) {
            Ok(gref) => {
                let _ = ACTIVITY.set(gref);
                chatter_core::system::messaging_background::register_platform(
                    set_messaging_signed_in,
                );
                tracing::info!("Android: MainActivity registered for JNI callbacks");
            }
            Err(e) => tracing::error!("Android: failed to hold MainActivity ref: {e}"),
        })())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
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
    mut env: jni::EnvUnowned<'_>,
    _this: JObject,
) {
    env.with_env(|env| -> jni::errors::Result<_> {
        Ok((|| match env.get_java_vm() {
            Ok(vm) => {
                let legacy_vm = unsafe { jni_021::JavaVM::from_raw(vm.get_raw().cast()) }
                    .expect("the initialized JavaVM is non-null");
                livekit::webrtc::android::initialize_android(&legacy_vm);
                tracing::info!("LiveKit webrtc android runtime initialized");
            }
            Err(e) => tracing::error!("webrtc init: get_java_vm failed: {e}"),
        })())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}

/// QA preview builds only: device testing drives the app over adb and needs
/// screenshots of recovery, secret-chat, invite and video screens. A release
/// build (no debug_assertions) always keeps screens secure.
pub fn qa_allows_capture() -> bool {
    cfg!(debug_assertions) && option_env!("CHATTER_QA_ALLOW_CAPTURE") == Some("1")
}

/// Set or clear `FLAG_SECURE` on the Activity window (`MainActivity.setSecure`).
///
/// Receives only the core-owned decision, recomputed at navigation and mode
/// transitions and when the Activity resumes.
pub fn set_secure(secure: bool) {
    let secure = secure && !qa_allows_capture();
    let result = (|| -> Result<(), String> {
        let act = activity()?;
        with_jni_env(|env| {
            let res = env.call_method(
                act,
                jni::jni_str!("setSecure"),
                jni::jni_sig!("(Z)V"),
                &[jni::objects::JValue::Bool(secure.into())],
            );
            if res.is_err() {
                clear_exception(env);
            }
            res.map(|_| ()).map_err(|e| e.to_string())
        })
    })();
    if let Err(e) = result {
        tracing::error!("Android: setSecure({secure}) failed: {e}");
    }
}

/// Shield's foreground service keeps the CPU active, but Android Doze also
/// gates the network. Ask for the user-controlled network exemption on enable.
pub fn request_shield_background_network() {
    let result = (|| -> Result<(), String> {
        let act = activity()?;
        with_jni_env(|env| {
            let result = env.call_method(
                act,
                jni::jni_str!("requestShieldBackgroundNetwork"),
                jni::jni_sig!("()V"),
                &[],
            );
            if result.is_err() {
                clear_exception(env);
            }
            result.map(|_| ()).map_err(|error| error.to_string())
        })
    })();
    if result.is_err() {
        tracing::warn!("Android Shield background network request unavailable");
    }
}

/// Refresh-rate vote (`MainActivity.setUiMoving`): the vendored Android
/// backend calls this when the UI starts moving and again once it is at rest
/// (`AndroidPlatform::with_refresh_rate_vote`). Returns the panel's top rate
/// in Hz (0 if unknown), or `None` if the call did not land.
pub fn set_ui_moving(moving: bool) -> Option<f32> {
    let result = (|| -> Result<f32, String> {
        let act = activity()?;
        with_jni_env(|env| {
            let res = env.call_method(
                act,
                jni::jni_str!("setUiMoving"),
                jni::jni_sig!("(Z)F"),
                &[jni::objects::JValue::Bool(moving.into())],
            );
            if res.is_err() {
                clear_exception(env);
            }
            res.and_then(|v| v.f()).map_err(|e| e.to_string())
        })
    })();
    result
        .inspect_err(|e| tracing::warn!("Android: setUiMoving({moving}) failed: {e}"))
        .ok()
}

/// Mirror Settings › Appearance › Theme into `MainActivity`
/// (`setThemeMode`), whose `onCreate` paints the pre-first-frame window
/// background from it — it runs before the native side can say anything.
pub fn set_theme_mode(mode: &str) {
    let result = (|| -> Result<(), String> {
        let act = activity()?;
        with_jni_env(|env| {
            let jmode = env.new_string(mode).map_err(|e| e.to_string())?;
            let res = env.call_method(
                act,
                jni::jni_str!("setThemeMode"),
                jni::jni_sig!("(Ljava/lang/String;)V"),
                &[jni::objects::JValue::Object(&jmode)],
            );
            if res.is_err() {
                clear_exception(env);
            }
            res.map(|_| ()).map_err(|e| e.to_string())
        })
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
        with_jni_env(|env| {
            let res = env.call_method(
                act,
                jni::jni_str!("setLightSystemBars"),
                jni::jni_sig!("(Z)V"),
                &[jni::objects::JValue::Bool(light.into())],
            );
            if res.is_err() {
                clear_exception(env);
            }
            res.map(|_| ()).map_err(|e| e.to_string())
        })
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
            with_jni_env(|env| {
                let res = env.call_method(
                    act,
                    jni::jni_str!("firstFrameDrawn"),
                    jni::jni_sig!("()V"),
                    &[],
                );
                if res.is_err() {
                    clear_exception(env);
                }
                res.map(|_| ()).map_err(|e| e.to_string())
            })
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
    mut _env: jni::EnvUnowned<'_>,
    _class: jni::objects::JClass,
) {
    _env.with_env(|_env| -> jni::errors::Result<_> {
        Ok((|| {
            let Some(weak) = super::call_actions::main_window() else {
                return;
            };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.invoke_release_text_focus();
                }
            });
        })())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}

/// Android Back is delivered by the Activity so navigation does not depend on
/// Slint's focused item surviving a screen or modal transition.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeBackPressed(
    mut env: jni::EnvUnowned<'_>,
    _class: jni::objects::JClass,
) {
    env.with_env(|_| -> jni::errors::Result<_> {
        let Some(weak) = super::call_actions::main_window() else {
            return Ok(());
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = weak.upgrade() {
                window.invoke_back_pressed();
            }
        });
        Ok(())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}

/// JNI entry point: the soft keyboard's action key (Done) — see
/// `MainActivity.onEditorAction` for why it does not reach Slint on its own.
/// Delivered as an Enter press, so the focused TextInput's `accepted` fires
/// as it does for Enter on desktop.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeImeEditorAction(
    mut _env: jni::EnvUnowned<'_>,
    _class: jni::objects::JClass,
) {
    _env.with_env(|_env| -> jni::errors::Result<_> {
        Ok((|| {
            let Some(weak) = super::call_actions::main_window() else {
                return;
            };
            let _ = slint::invoke_from_event_loop(move || {
                use slint::ComponentHandle;
                let Some(w) = weak.upgrade() else { return };
                let enter: slint::SharedString = slint::platform::Key::Return.into();
                w.window()
                    .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                        text: enter.clone(),
                    });
                w.window()
                    .dispatch_event(slint::platform::WindowEvent::KeyReleased { text: enter });
            });
        })())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}

/// Thin platform hook: account policy is owned by chatter-core.
fn set_messaging_signed_in(signed_in: bool) {
    let result = (|| -> Result<(), String> {
        let act = activity()?;
        with_jni_env(|env| {
            let result = env.call_method(
                act,
                jni::jni_str!("setMessagingSignedIn"),
                jni::jni_sig!("(Z)V"),
                &[jni::objects::JValue::Bool(signed_in.into())],
            );
            if result.is_err() {
                clear_exception(env);
            }
            result.map(|_| ()).map_err(|e| e.to_string())
        })
    })();
    if result.is_err() {
        tracing::warn!("Android messaging service lifecycle unavailable");
    }
}

/// "samsung SM-S938B · Android 16" from android.os.Build, for the device name
/// other sessions see. Manufacturer, model and OS version only.
pub fn device_label() -> Option<String> {
    with_jni_env(|env| {
        let mut field = |class: &'static jni::strings::JNIStr,
                         name: &'static jni::strings::JNIStr|
         -> Result<String, String> {
            let value = env
                .get_static_field(class, name, jni::jni_sig!("Ljava/lang/String;"))
                .and_then(|v| v.l())
                .map_err(|e| e.to_string())?;
            let s: String = env
                .cast_local::<jni::objects::JString>(value)
                .map_err(|e| e.to_string())?
                .try_to_string(&env)
                .map_err(|e| e.to_string())?
                .into();
            Ok(s)
        };
        let maker = field(
            jni::jni_str!("android/os/Build"),
            jni::jni_str!("MANUFACTURER"),
        )?;
        let model = field(jni::jni_str!("android/os/Build"), jni::jni_str!("MODEL"))?;
        let release = field(
            jni::jni_str!("android/os/Build$VERSION"),
            jni::jni_str!("RELEASE"),
        )?;
        let device = if model.to_lowercase().starts_with(&maker.to_lowercase()) {
            model
        } else {
            format!("{maker} {model}")
        };
        Ok(format!("{device} · Android {release}"))
    })
    .ok()
}
