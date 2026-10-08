//! The native half of the text-input producer gate
//! (`com.google.androidgamesdk.ChatterTextInputGate`).
//!
//! The vendored Android backend hands every outbound Slint text state and
//! every accepted-send consumption to `MainActivity`, which serializes them on
//! the UI thread with the IME's own operations; the gate tags every snapshot
//! the IME produces with the owner and generation it was produced under and
//! reports it here. See the backend's `TextInputBridge`.

use i_slint_backend_android_activity::{InboundTextInput, TextInputBridge, TextInputPush};
use jni::JNIEnv;
use jni::objects::{JClass, JString, JValue};
use jni::sys::{jint, jlong};

use super::activity::{activity, clear_exception, jni_env};

/// `MainActivity` as the backend's bridge.
pub struct Bridge;

impl TextInputBridge for Bridge {
    fn push(&self, push: &TextInputPush) -> bool {
        let result = (|| -> Result<(), String> {
            let act = activity()?;
            let mut env = jni_env()?;
            let text = env.new_string(&push.text).map_err(|e| e.to_string())?;
            let (comp_start, comp_end) = push
                .compose
                .map(|(s, e)| (s as jint, e as jint))
                .unwrap_or((-1, -1));
            let res = env.call_method(
                act,
                "textInputPush",
                "(Ljava/lang/String;IIIIJJ)V",
                &[
                    JValue::Object(&text),
                    JValue::Int(push.selection.0 as jint),
                    JValue::Int(push.selection.1 as jint),
                    JValue::Int(comp_start),
                    JValue::Int(comp_end),
                    JValue::Long(push.owner as jlong),
                    JValue::Long(push.generation as jlong),
                ],
            );
            if res.is_err() {
                clear_exception(&mut env);
            }
            res.map(|_| ()).map_err(|e| e.to_string())
        })();
        match result {
            Ok(()) => true,
            Err(e) => {
                tracing::error!("Android: textInputPush failed: {e}");
                false
            }
        }
    }

    fn consume(&self, owner: u64, generation: u64) -> bool {
        let result = (|| -> Result<(), String> {
            let act = activity()?;
            let mut env = jni_env()?;
            let res = env.call_method(
                act,
                "textInputConsume",
                "(JJ)V",
                &[
                    JValue::Long(owner as jlong),
                    JValue::Long(generation as jlong),
                ],
            );
            if res.is_err() {
                clear_exception(&mut env);
            }
            res.map(|_| ()).map_err(|e| e.to_string())
        })();
        match result {
            Ok(()) => true,
            Err(e) => {
                tracing::error!("Android: textInputConsume failed: {e}");
                false
            }
        }
    }
}

/// An accepted send took the composer's text: consume the IME's copy too.
/// Call right after the composer is cleared, on the Slint thread.
pub fn consume() {
    i_slint_backend_android_activity::consume_text_input();
}

/// The app replaced the focused composer's text (the draft an edit held
/// aside or a failed send's text put back after a `consume`, the message
/// text an edit opens on): replace the IME's copy as of a new generation, so
/// no snapshot of the field as it was before can land on top of it. Call
/// right after the composer is set, on the Slint thread.
pub fn restore() {
    i_slint_backend_android_activity::restore_text_input();
}

/// JNI: `ChatterTextInputGate.stateChanged` (UI thread). A snapshot the IME
/// produced, tagged with the owner and generation applied when it was made.
/// Offsets are UTF-16; the backend converts them.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_google_androidgamesdk_ChatterTextInputGate_nativeTextInputState(
    mut env: JNIEnv,
    _class: JClass,
    text: JString,
    sel_start: jint,
    sel_end: jint,
    comp_start: jint,
    comp_end: jint,
    owner: jlong,
    generation: jlong,
) {
    let text: String = match env.get_string(&text) {
        Ok(s) => s.into(),
        Err(e) => {
            tracing::error!("Android: text input snapshot unreadable: {e}");
            return;
        }
    };
    let compose = (comp_start >= 0 && comp_end > comp_start)
        .then(|| (comp_start as usize, comp_end as usize));
    i_slint_backend_android_activity::deliver_text_input_state(InboundTextInput {
        text,
        selection: (sel_start.max(0) as usize, sel_end.max(0) as usize),
        compose,
        owner: owner as u64,
        generation: generation as u64,
    });
}

/// JNI: `ChatterTextInputGate.applyReset` (UI thread). A reset was applied
/// for `(owner, generation)`. Carries no text: it only tells the backend the
/// IME's copy now reflects that generation.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_google_androidgamesdk_ChatterTextInputGate_nativeTextInputAck(
    _env: JNIEnv,
    _class: JClass,
    owner: jlong,
    generation: jlong,
) {
    i_slint_backend_android_activity::deliver_text_input_ack(owner as u64, generation as u64);
}
