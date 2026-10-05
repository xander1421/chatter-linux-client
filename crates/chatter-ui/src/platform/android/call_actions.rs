//! Incoming-call notification actions (Accept / Decline) bridged from the
//! `CallActionReceiver` BroadcastReceiver back into the Rust call layer.
//!
//! The notification's action buttons fire PendingIntent broadcasts; the Kotlin
//! receiver forwards them here via `nativeOnCallAction(call_id, accept)`. We
//! run the accept/decline on the shared tokio runtime (the JNI thread has no
//! runtime context) and dismiss the ring notification.
//!
//! Decline is fully headless. Accept connects the call (audio goes live
//! immediately); the receiver separately brings the activity forward so the
//! in-app active-call screen can render.

use std::sync::{Arc, OnceLock};

use chatter_core::client::ChatterClient;
use jni::JNIEnv;
use jni::objects::{JClass, JString};
use jni::sys::jboolean;

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
    mut env: JNIEnv,
    _class: JClass,
    call_id: JString,
) {
    let call_id: String = match env.get_string(&call_id) {
        Ok(s) => s.into(),
        Err(e) => {
            tracing::warn!(error = %e, "nativeAcceptIncomingCall: bad call_id");
            return;
        }
    };
    super::notifications::cancel_call_notification(&call_id);

    let Some(weak) = MAIN_WINDOW.get().cloned() else {
        tracing::warn!("nativeAcceptIncomingCall: window not registered");
        return;
    };
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.invoke_accept_incoming_call(call_id.into());
        }
    });
}

/// JNI entry point: `MainActivity.onNewIntent` when the ringing notification's
/// body (not Accept/Decline) is tapped: show the ring screen again if Back hid
/// it. A call that is no longer ringing (or another call) changes nothing.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeShowIncomingCall(
    mut env: JNIEnv,
    _class: JClass,
    call_id: JString,
) {
    let call_id: String = match env.get_string(&call_id) {
        Ok(s) => s.into(),
        Err(e) => {
            tracing::warn!(error = %e, "nativeShowIncomingCall: bad call_id");
            return;
        }
    };
    let Some(weak) = MAIN_WINDOW.get().cloned() else {
        return;
    };
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            if w.get_incoming_call_visible()
                && w.get_incoming_call_data().call_id == call_id.as_str()
            {
                w.set_incoming_call_minimized(false);
            }
        }
    });
}

/// JNI entry point: `CallActionReceiver` calls this with the call_id and whether
/// the user tapped Accept (`true`) or Decline (`false`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_CallActionReceiver_nativeOnCallAction(
    mut env: JNIEnv,
    _class: JClass,
    call_id: JString,
    accept: jboolean,
) {
    let call_id: String = match env.get_string(&call_id) {
        Ok(s) => s.into(),
        Err(e) => {
            tracing::warn!(error = %e, "nativeOnCallAction: bad call_id");
            return;
        }
    };
    let accept = accept != 0;

    // Dismiss the ring notification immediately on either action.
    super::notifications::cancel_call_notification(&call_id);

    let Some(client) = CALL_CLIENT.get().cloned() else {
        tracing::warn!("nativeOnCallAction: call client not registered");
        return;
    };

    crate::rt_handle().spawn(async move {
        let result = if accept {
            // replaces_active = false: a notification accept assumes no other
            // active call. The worker connects the room; audio goes live.
            client
                .accept_presented_incoming_call(&call_id, false)
                .await
                .map(|_| ())
        } else {
            client.decline_presented_incoming_call(&call_id).await
        };
        if let Err(e) = result {
            tracing::warn!(error = %e, accept, call_id, "call notification action failed");
        } else {
            tracing::info!(accept, call_id, "call notification action handled");
        }
    });
}
