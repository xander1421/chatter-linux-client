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
use jni::objects::{JClass, JString};
use jni::sys::jboolean;
use jni::JNIEnv;

/// Set once at startup so notification-action callbacks can reach the client.
static CALL_CLIENT: OnceLock<Arc<ChatterClient>> = OnceLock::new();

/// Register the client used to accept/decline calls from notification actions.
pub fn register_call_client(client: Arc<ChatterClient>) {
    let _ = CALL_CLIENT.set(client);
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
