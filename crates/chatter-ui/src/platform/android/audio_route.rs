//! Android communication device discovery, selection and acknowledgement.
//! Core owns default/fallback policy; AudioManager applies paired mic/output routes.

use std::sync::{Mutex, OnceLock};

use chatter_core::calls::audio_route::{AudioRoute, CommunicationDevice, CommunicationRouting};
use jni::objects::{JObject, JString, JValue};
use slint::ComponentHandle;

use super::activity::{activity, clear_exception, with_jni_env};
use crate::MainWindow;

/// The live call's routing state; `None` between calls.
static ROUTING: Mutex<Option<CommunicationRouting>> = Mutex::new(None);
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
        .on_call_audio_video(|video| {
            update(|r| {
                r.policy.set_video(video);
            })
        });
    window
        .global::<crate::CallView>()
        .on_call_toggle_speaker(|| {
            update(|r| {
                r.select("");
                r.policy.toggle_speaker();
            })
        });
}

fn start(video: bool) {
    {
        let mut guard = ROUTING.lock().unwrap_or_else(|e| e.into_inner());
        // Repeated session activation during reconnect must retain user intent.
        if guard.is_none() {
            *guard = Some(CommunicationRouting::new(video));
        }
    }
    call_activity("startCallAudio", "()V", &[]);
}

fn stop() {
    ROUTING.lock().unwrap_or_else(|e| e.into_inner()).take();
    call_activity("stopCallAudio", "()V", &[]);
    publish();
}

fn update(f: impl FnOnce(&mut CommunicationRouting)) {
    let target = {
        let mut guard = ROUTING.lock().unwrap_or_else(|e| e.into_inner());
        let Some(r) = guard.as_mut() else { return };
        f(r);
        r.target()
            .filter(|d| Some(d.id.as_str()) != r.applied())
            .map(|d| d.id.clone())
    };
    if let Some(id) = target {
        apply(&id);
    }
    publish();
}

fn apply(id: &str) {
    let result = (|| -> Result<(), String> {
        let act = activity()?;
        with_jni_env(|env| {
            let id = env.new_string(id).map_err(|e| e.to_string())?;
            env.call_method(
                act,
                jni::jni_str!("applyCommunicationDevice"),
                jni::signature::RuntimeMethodSignature::from_str("(Ljava/lang/String;)V")
                    .map_err(|e| e.to_string())?
                    .method_signature(),
                &[JValue::Object(&id)],
            )
            .map(|_| ())
            .map_err(|e| {
                clear_exception(env);
                e.to_string()
            })
        })
    })();
    if let Err(error) = result {
        tracing::error!(%error, "Could not request call route");
    }
}

pub fn select(id: &str) {
    update(|r| {
        r.select(id);
    });
}

pub fn show_picker() {
    publish();
    if let Some(w) = WINDOW.get().and_then(|w| w.upgrade()) {
        w.global::<crate::CallView>()
            .set_device_picker_type("audio_output".into());
        w.global::<crate::CallView>()
            .set_device_picker_visible(true);
    }
}

fn publish() {
    let Some(weak) = WINDOW.get().cloned() else {
        return;
    };
    let snapshot = ROUTING.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            let view = w.global::<crate::CallView>();
            let Some(r) = snapshot else {
                view.set_call_speaker_on(false);
                return;
            };
            let rows: Vec<crate::AudioDeviceData> = r
                .picker_devices()
                .into_iter()
                .map(|(id, name, is_default)| crate::AudioDeviceData {
                    id: id.into(),
                    name: name.into(),
                    is_default,
                })
                .collect();
            let model = slint::ModelRc::new(slint::VecModel::from(rows));
            view.set_audio_devices(model.clone());
            view.set_audio_output_devices(model);
            // A tick always describes the route acknowledged by AudioManager.
            let selected = r.applied().unwrap_or("unavailable");
            view.set_selected_audio_device_id(selected.into());
            view.set_selected_audio_output_id(selected.into());
            view.set_call_speaker_on(
                r.devices()
                    .iter()
                    .any(|d| Some(d.id.as_str()) == r.applied() && d.route == AudioRoute::Speaker),
            );
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

/// Device discovery and acknowledgement from Android's main looper.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_CallAudioRouter_nativeOnCommunicationDevices(
    mut env: jni::EnvUnowned<'_>,
    _this: JObject,
    snapshot: JString,
    applied: JString,
) {
    env.with_env(|env| -> jni::errors::Result<_> {
        let snapshot = snapshot.try_to_string(env)?;
        let applied = applied.try_to_string(env)?;
        let Ok(rows) = serde_json::from_str::<Vec<(String, String, i32)>>(&snapshot) else {
            return Ok(());
        };
        let devices = rows
            .into_iter()
            .filter_map(|(id, name, kind)| {
                let route = match kind {
                    0 => AudioRoute::Earpiece,
                    1 => AudioRoute::Speaker,
                    2 => AudioRoute::WiredHeadset,
                    3 => AudioRoute::Bluetooth,
                    _ => return None,
                };
                Some(CommunicationDevice { id, name, route })
            })
            .collect();
        // Acknowledgements must not retry a refused route in a callback loop.
        {
            let mut guard = ROUTING.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(r) = guard.as_mut() {
                r.refresh(
                    devices,
                    (!applied.is_empty()).then_some(applied.to_string()),
                );
            }
        }
        publish();
        Ok(())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}

/// Device inventory changed; apply fallback or restore a reconnected selection.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_CallAudioRouter_nativeOnCommunicationDevicesChanged(
    mut env: jni::EnvUnowned<'_>,
    _this: JObject,
) {
    env.with_env(|_| -> jni::errors::Result<_> {
        update(|_| {});
        Ok(())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}
