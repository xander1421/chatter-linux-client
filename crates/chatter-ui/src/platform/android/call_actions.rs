//! Incoming-call notification actions authenticated before side effects.
//!
//! Accept stays an activity PendingIntent for Android 14+ foregrounding;
//! Decline stays a nonexported headless receiver. Both consume a call/action
//! capability before acting. Body/full-screen open use separate call-bound
//! capabilities before changing the UI. Core call/session checks still apply.

use slint::ComponentHandle;
use std::sync::{Arc, Mutex, OnceLock};

use chatter_core::client::ChatterClient;
use jni::objects::{JClass, JString};

/// Set once at startup so notification-action callbacks can reach the client.
static CALL_CLIENT: OnceLock<Arc<ChatterClient>> = OnceLock::new();

/// Window handle for routing a notification "Accept" through the in-app accept
/// flow (connect + render the active-call screen) once MainActivity foregrounds.
#[derive(Default)]
struct CallWindow {
    window: Option<slint::Weak<crate::MainWindow>>,
    pending_accept: Option<String>,
    full_screen_call: Option<String>,
    covered_until_frame: Option<String>,
}

static MAIN_WINDOW: Mutex<CallWindow> = Mutex::new(CallWindow {
    window: None,
    pending_accept: None,
    full_screen_call: None,
    covered_until_frame: None,
});

/// Register the client used to accept/decline calls from notification actions.
pub fn register_call_client(client: Arc<ChatterClient>) {
    let _ = CALL_CLIENT.set(client);
}

/// Register the main window so the Accept activity-intent can drive the in-app
/// accept handler.
pub fn register_main_window(window: slint::Weak<crate::MainWindow>) {
    let (pending, full_screen) = {
        let mut state = MAIN_WINDOW.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.window = Some(window.clone());
        (state.pending_accept.clone(), state.full_screen_call.clone())
    };
    if let Some(call_id) = pending {
        dispatch_accept(window.clone(), call_id);
    }
    if let Some(call_id) = full_screen {
        dispatch_full_screen(window, call_id);
    }
}

/// The registered main window, for other JNI entry points that must reach the
/// UI (upgrade it only inside `slint::invoke_from_event_loop`).
pub(crate) fn main_window() -> Option<slint::Weak<crate::MainWindow>> {
    MAIN_WINDOW.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).window.clone()
}

fn dispatch_accept(weak: slint::Weak<crate::MainWindow>, call_id: String) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            let should_accept = {
                let mut state = MAIN_WINDOW.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                if state.pending_accept.as_deref() == Some(call_id.as_str()) {
                    state.pending_accept = None;
                    true
                } else {
                    false
                }
            };
            if should_accept {
                w.global::<crate::CallView>()
                    .invoke_accept_incoming_call(call_id.into());
            }
        }
    });
}

/// JNI entry point: MainActivity received an Accept activity-intent (the app is
/// now foreground). Run the in-app accept flow for `call_id` — it connects the
/// call and paints the active-call screen, same as the in-app Accept button.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeAcceptIncomingCall(
    mut env: jni::EnvUnowned<'_>,
    _class: JClass,
    call_id: JString,
    capability: JString,
) {
    env.with_env(|env| -> jni::errors::Result<_> {
        Ok((|| {
            let call_id: String = match (call_id).try_to_string(&env) {
                Ok(s) => s.into(),
                Err(e) => {
                    tracing::warn!(error = %e, "nativeAcceptIncomingCall: bad call_id");
                    return;
                }
            };
            let Ok(capability) = (capability).try_to_string(&env).map(String::from) else {
                return;
            };
            if !super::notifications::consume_call_consent(
                &call_id,
                super::notifications::call_consent::ACCEPT,
                &capability,
            ) {
                return;
            }

            let weak = {
                let mut state = MAIN_WINDOW.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                state.pending_accept = Some(call_id.clone());
                state.window.clone()
            };
            if let Some(weak) = weak {
                dispatch_accept(weak, call_id);
            }
        })())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}

/// JNI entry point: `MainActivity.onNewIntent` when the ringing notification's
/// body (not Accept/Decline) is tapped: show the ring screen again if Back hid
/// it. A call that is no longer ringing (or another call) changes nothing.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeShowIncomingCall(
    mut env: jni::EnvUnowned<'_>,
    _class: JClass,
    call_id: JString,
    action: JString,
    capability: JString,
) {
    env.with_env(|env| -> jni::errors::Result<_> {
        Ok((|| {
            let call_id: String = match (call_id).try_to_string(&env) {
                Ok(s) => s.into(),
                Err(e) => {
                    tracing::warn!(error = %e, "nativeShowIncomingCall: bad call_id");
                    return;
                }
            };
            let Ok(action) = (action).try_to_string(&env).map(String::from) else {
                return;
            };
            let Ok(capability) = (capability).try_to_string(&env).map(String::from) else {
                return;
            };
            if action == super::notifications::call_consent::FULL_SCREEN {
                super::notifications::authorize_full_screen(&call_id, &capability, || {
                    let weak = {
                        let mut state = MAIN_WINDOW.lock().unwrap_or_else(|e| e.into_inner());
                        if state.full_screen_call.as_deref() == Some(call_id.as_str()) {
                            return;
                        }
                        state.full_screen_call = Some(call_id.clone());
                        state.window.clone()
                    };
                    if let Some(weak) = weak {
                        dispatch_full_screen(weak, call_id.clone());
                    }
                });
                return;
            }
            if action != super::notifications::call_consent::OPEN
                || !super::notifications::consume_call_consent(&call_id, &action, &capability)
            {
                return;
            }
            if let Some(weak) = main_window() {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        if w.global::<crate::CallView>().get_incoming_call_visible()
                            && w.global::<crate::CallView>()
                                .get_incoming_call_data()
                                .call_id
                                == call_id.as_str()
                        {
                            w.global::<crate::CallView>()
                                .set_incoming_call_minimized(false);
                        }
                    }
                });
            }
        })())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}

fn dispatch_full_screen(weak: slint::Weak<crate::MainWindow>, call_id: String) {
    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };
        let mut state = MAIN_WINDOW.lock().unwrap_or_else(|e| e.into_inner());
        if state.full_screen_call.as_deref() != Some(call_id.as_str()) {
            return;
        }
        state.covered_until_frame = Some(call_id.clone());
        drop(state);
        let view = w.global::<crate::CallView>();
        view.set_keyguard_call_id(call_id.clone().into());
        super::activity::update_call_window(jni::jni_str!("showVerifiedCallWindow"), &call_id);
    });
}

pub(super) fn call_window_frame_presented() {
    let call_id = MAIN_WINDOW
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .covered_until_frame
        .take();
    if let Some(call_id) = call_id {
        super::activity::update_call_window(jni::jni_str!("verifiedCallFrameDrawn"), &call_id);
    }
}

/// Notification cancellation also happens on answer. The matching active
/// call screen then owns cleanup through its own presentation lifecycle.
pub(crate) fn ring_notification_cancelled(call_id: &str) {
    let call_id = call_id.to_owned();
    let Some(weak) = main_window() else {
        forget_call_window(&call_id);
        return;
    };
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            let view = w.global::<crate::CallView>();
            if view.get_active_call_visible() && view.get_active_call_data().call_id == call_id.as_str() {
                return;
            }
        }
        super::activity::clear_incoming_call_window(&call_id);
    });
}

pub(super) fn forget_call_window(call_id: &str) {
    let mut state = MAIN_WINDOW.lock().unwrap_or_else(|e| e.into_inner());
    if state.full_screen_call.as_deref() == Some(call_id) {
        state.full_screen_call = None;
    }
    if state.covered_until_frame.as_deref() == Some(call_id) {
        state.covered_until_frame = None;
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeCallWindowCleared(
    mut env: jni::EnvUnowned<'_>,
    _class: JClass,
    call_id: JString,
    navigate: jni::sys::jboolean,
) {
    env.with_env(|env| -> jni::errors::Result<_> {
        let call_id = call_id.try_to_string(&env)?;
        forget_call_window(&call_id);
        if let Some(weak) = main_window() {
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                let view = w.global::<crate::CallView>();
                if view.get_keyguard_call_id() != call_id.as_str() {
                    return;
                }
                if navigate != jni::sys::JNI_FALSE {
                    if view.get_active_call_visible() && view.get_active_call_data().call_id == call_id.as_str() {
                        view.set_active_call_fullscreen(false);
                        view.invoke_restore_call_route();
                    } else if view.get_incoming_call_data().call_id == call_id.as_str() {
                        view.set_incoming_call_minimized(true);
                    }
                }
                view.set_keyguard_call_id("".into());
            });
        }
        Ok(())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}

/// JNI entry point: authenticated, single-use headless Decline.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_CallActionReceiver_nativeOnCallAction(
    mut env: jni::EnvUnowned<'_>,
    _class: JClass,
    call_id: JString,
    capability: JString,
) {
    env.with_env(|env| -> jni::errors::Result<_> {
        Ok((|| {
            let call_id: String = match (call_id).try_to_string(&env) {
                Ok(s) => s.into(),
                Err(e) => {
                    tracing::warn!(error = %e, "nativeOnCallAction: bad call_id");
                    return;
                }
            };
            let Ok(capability) = (capability).try_to_string(&env).map(String::from) else {
                return;
            };
            if !super::notifications::consume_and_cancel_call_consent(
                &call_id,
                super::notifications::call_consent::DECLINE,
                &capability,
            ) {
                return;
            }

            let Some(client) = CALL_CLIENT.get().cloned() else {
                tracing::warn!("nativeOnCallAction: call client not registered");
                return;
            };

            crate::rt_handle().spawn(async move {
                let result = client.decline_presented_incoming_call(&call_id).await;
                if let Err(e) = result {
                    tracing::warn!(error = %e, call_id, "call notification action failed");
                } else {
                    tracing::info!(call_id, "call notification action handled");
                }
            });
        })())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}
