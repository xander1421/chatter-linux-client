//! Incoming-call notification actions authenticated before side effects.
//!
//! Accept stays an activity PendingIntent for Android 14+ foregrounding;
//! Decline stays a nonexported headless receiver. Both consume a call/action
//! capability before cancellation. Body/full-screen open use separate one-shot
//! capabilities before changing the UI. Core call/session checks still apply.

use slint::ComponentHandle;
use std::sync::{Arc, OnceLock};

use chatter_core::client::ChatterClient;
use jni::objects::{JClass, JString};

/// Set once at startup so notification-action callbacks can reach the client.
static CALL_CLIENT: OnceLock<Arc<ChatterClient>> = OnceLock::new();

/// Window handle for routing a notification "Accept" through the in-app accept
/// flow (connect + render the active-call screen) once MainActivity foregrounds.
static MAIN_WINDOW: OnceLock<slint::Weak<crate::MainWindow>> = OnceLock::new();

/// Register the client used to accept/decline calls from notification actions.
pub fn register_call_client(client: Arc<ChatterClient>) {
    let _ = CALL_CLIENT.set(client);
}

/// Register the main window so the Accept activity-intent can drive the in-app
/// accept handler.
pub fn register_main_window(window: slint::Weak<crate::MainWindow>) {
    let _ = MAIN_WINDOW.set(window);
}

/// The registered main window, for other JNI entry points that must reach the
/// UI (upgrade it only inside `slint::invoke_from_event_loop`).
pub(crate) fn main_window() -> Option<slint::Weak<crate::MainWindow>> {
    MAIN_WINDOW.get().cloned()
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
            if !super::notifications::consume_and_cancel_call_consent(
                &call_id,
                super::notifications::call_consent::ACCEPT,
                &capability,
            ) {
                return;
            }

            let Some(weak) = MAIN_WINDOW.get().cloned() else {
                tracing::warn!("nativeAcceptIncomingCall: window not registered");
                return;
            };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.global::<crate::CallView>()
                        .invoke_accept_incoming_call(call_id.into());
                }
            });
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
            if !matches!(
                action.as_str(),
                super::notifications::call_consent::OPEN
                    | super::notifications::call_consent::FULL_SCREEN
            ) || !super::notifications::consume_call_consent(&call_id, &action, &capability)
            {
                return;
            }
            let Some(weak) = MAIN_WINDOW.get().cloned() else {
                return;
            };
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
        })())
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
