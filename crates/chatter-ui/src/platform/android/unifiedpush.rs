//! UnifiedPush JNI bridge.
//!
//! When the user has a UnifiedPush distributor app installed (e.g. ntfy), that
//! app holds the single persistent push socket and delivers wakes to Chatter
//! through `UnifiedPushReceiver` (Kotlin), which calls these entry points. The
//! core then drops its own ntfy SSE socket (see `push_config.rs`), so the device
//! no longer pays the battery cost of a second persistent connection. With no
//! distributor installed none of this fires and the client keeps its own socket.
use jni::objects::{JClass, JString};

/// The distributor handed us a new endpoint URL. Register it with the server so
/// wakes are POSTed there, and switch this device off its own SSE socket.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_UnifiedPushReceiver_nativeOnEndpoint(
    mut env: jni::EnvUnowned<'_>,
    _class: JClass,
    endpoint: JString,
) {
    env.with_env(|env| -> jni::errors::Result<_> {
        Ok((|| {
            let Ok(endpoint) = (endpoint).try_to_string(&env).map(String::from) else {
                tracing::warn!("nativeOnEndpoint: bad endpoint string");
                return;
            };
            let Some(client) = crate::android_client() else {
                tracing::warn!("nativeOnEndpoint: core client not ready");
                return;
            };
            crate::rt_handle().spawn(async move {
                if let Err(e) = client.set_unified_push_endpoint(endpoint).await {
                    tracing::warn!(error = %e, "UnifiedPush endpoint registration failed");
                }
            });
        })())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}

/// The distributor delivered a wake. Run the same content-free catch-up as an
/// in-process SSE wake.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_UnifiedPushReceiver_nativeOnMessage(
    mut env: jni::EnvUnowned<'_>,
    _class: JClass,
) {
    env.with_env(|_env| -> jni::errors::Result<_> {
        if let Some(client) = crate::android_client() {
            crate::rt_handle().spawn(async move { client.on_unified_push_message().await });
        }
        Ok(())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}

/// The distributor unregistered (or registration failed). Clear the server-side
/// endpoint and fall back to this device's own SSE socket.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_UnifiedPushReceiver_nativeOnUnregistered(
    mut env: jni::EnvUnowned<'_>,
    _class: JClass,
) {
    env.with_env(|_env| -> jni::errors::Result<_> {
        if let Some(client) = crate::android_client() {
            crate::rt_handle().spawn(async move {
                if let Err(e) = client.disable_unified_push().await {
                    tracing::warn!(error = %e, "UnifiedPush disable failed");
                }
            });
        }
        Ok(())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}
