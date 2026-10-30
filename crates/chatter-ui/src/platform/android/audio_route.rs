//! Call audio routing on Android: earpiece / speaker / headset.
//!
//! The policy is chatter-core's [`CallAudioRouting`]; this file only drives it.
//! `MainActivity.startCallAudio/stopCallAudio/applyAudioRoute` (Kotlin
//! `CallAudioRouter`) put the phone in communication mode and apply the route;
//! `CallAudioRouter.nativeOnAudioOutputsChanged` reports headsets coming and
//! going. The route that was applied is mirrored into `call-speaker-on`, so the
//! speaker button shows where the audio actually is.
//!
//! Desktop never installs this: there the speaker button opens the output
//! device picker instead (`call-speaker-toggle` stays false).

use std::sync::{Mutex, OnceLock};

use chatter_core::calls::audio_route::{AudioOutputs, AudioRoute, CallAudioRouting};
use jni::objects::{JObject, JValue};
// JNI input is an unsigned byte; normalize nonzero values before Rust bool.
use slint::ComponentHandle;

use super::activity::{activity, clear_exception, with_jni_env};
use crate::MainWindow;

/// The live call's routing state; `None` between calls.
static ROUTING: Mutex<Option<CallAudioRouting>> = Mutex::new(None);
static WINDOW: OnceLock<slint::Weak<MainWindow>> = OnceLock::new();

/// Wire the call screen's audio callbacks. Call once on the UI thread.
pub fn install(window: &MainWindow) {
    let _ = WINDOW.set(window.as_weak());
    window
        .global::<crate::CallView>()
        .set_call_speaker_toggle(true);
    window
        .global::<crate::CallView>()
        .on_call_audio_session(|active, video| if active { start(video) } else { stop() });
    window
        .global::<crate::CallView>()
        .on_call_audio_video(|video| update(|r| r.set_video(video)));
    window
        .global::<crate::CallView>()
        .on_call_toggle_speaker(|| update(|r| r.toggle_speaker()));
}

fn start(video: bool) {
    let route = {
        let mut guard = ROUTING.lock().unwrap_or_else(|e| e.into_inner());
        guard.insert(CallAudioRouting::new(video)).route()
    };
    call_activity("startCallAudio", "()V", &[]);
    apply(route);
}

fn stop() {
    ROUTING.lock().unwrap_or_else(|e| e.into_inner()).take();
    call_activity("stopCallAudio", "()V", &[]);
    publish(false);
}

/// Run `f` on the live call's routing and apply the result. No call, no-op.
fn update(f: impl FnOnce(&mut CallAudioRouting) -> AudioRoute) {
    let route = {
        let mut guard = ROUTING.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(r) => f(r),
            None => return,
        }
    };
    apply(route);
}

fn apply(route: AudioRoute) {
    tracing::info!("call audio route: {route:?}");
    call_activity("applyAudioRoute", "(I)V", &[JValue::Int(route.as_i32())]);
    publish(route == AudioRoute::Speaker);
}

/// Mirror the route into the UI. Callers may be on the JNI (Android main)
/// thread, so the Weak is upgraded on the Slint event loop.
fn publish(speaker_on: bool) {
    let Some(weak) = WINDOW.get().cloned() else {
        return;
    };
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.global::<crate::CallView>()
                .set_call_speaker_on(speaker_on);
        }
    });
}

fn call_activity(method: &str, sig: &str, args: &[JValue]) {
    let result = (|| -> Result<(), String> {
        let act = activity()?;
        with_jni_env(|env| {
            let res = env.call_method(
                act,
                jni::strings::JNIString::from(method),
                jni::signature::RuntimeMethodSignature::from_str(sig)
                    .map_err(|e| e.to_string())?
                    .method_signature(),
                args,
            );
            if res.is_err() {
                clear_exception(env);
            }
            res.map(|_| ()).map_err(|e| e.to_string())
        })
    })();
    if let Err(e) = result {
        tracing::error!("call audio: {method} failed: {e}");
    }
}

/// JNI: `CallAudioRouter` reports the outputs usable for a call (on register
/// and whenever a device is added or removed).
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_CallAudioRouter_nativeOnAudioOutputsChanged(
    mut _env: jni::EnvUnowned<'_>,
    _this: JObject,
    earpiece: u8,
    wired: u8,
    bluetooth: u8,
) {
    _env.with_env(|_env| -> jni::errors::Result<_> {
        Ok((|| {
            let outputs = AudioOutputs {
                earpiece: earpiece != 0,
                wired_headset: wired != 0,
                bluetooth: bluetooth != 0,
            };
            tracing::info!("call audio outputs: {outputs:?}");
            update(|r| r.set_outputs(outputs));
        })())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}
