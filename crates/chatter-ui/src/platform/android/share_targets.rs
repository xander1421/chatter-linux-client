//! Direct Share: push the chats chatter-core allows in the system share sheet
//! to Android as sharing shortcuts (`MainActivity.setShareTargets`).
//!
//! The policy — which chats, whether any at all (signed out, app-locked, the
//! private default notification level, Shield mode) — is core's
//! (`ChatterClient::share_targets`). This module only delivers the answer, and
//! only when it changed: Android rate-limits shortcut updates.

use std::sync::{Arc, Mutex};

use chatter_core::client::{ChatterClient, ShareTarget};
use jni::objects::{JObject, JValue};
use jni::sys::jsize;

use super::activity;

/// What Android currently holds. `None` until the first push this process.
static PUBLISHED: Mutex<Option<Vec<ShareTarget>>> = Mutex::new(None);

/// Re-ask core and push the result if it differs from what Android holds.
/// Cheap at the private default (core answers "none" before building any row);
/// otherwise it builds the chat list, so it runs off the UI thread. The lock is
/// held across the query and the push, so a slower older refresh can never
/// overwrite a newer one.
pub fn refresh(client: &Arc<ChatterClient>) {
    // Lifecycle hooks can fire before the runtime is entered on this thread;
    // a missed refresh is repaired by the next chat-list publish, a panic is not.
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let client = client.clone();
    runtime.spawn_blocking(move || {
        let Ok(mut published) = PUBLISHED.lock() else {
            return;
        };
        let targets = client.share_targets();
        if published.as_ref() == Some(&targets) {
            return;
        }
        match push(&targets) {
            Ok(()) => *published = Some(targets),
            Err(error) => tracing::warn!("share targets not published: {error}"),
        }
    });
}

/// [`refresh`] for hooks that only hold `&ChatterClient` (app-lock edges).
pub fn refresh_current() {
    if let Some(client) = super::notification_route::current_client() {
        refresh(&client);
    }
}

/// Remove every published target now (sign-out).
pub fn clear() {
    let Ok(mut published) = PUBLISHED.lock() else {
        return;
    };
    match push(&[]) {
        Ok(()) => *published = Some(Vec::new()),
        Err(error) => tracing::warn!("share targets not cleared: {error}"),
    }
}

fn push(targets: &[ShareTarget]) -> Result<(), String> {
    activity::with_jni_env(|env| {
        let act = activity::activity()?;
        let string_class = env
            .find_class(jni::jni_str!("java/lang/String"))
            .map_err(|e| e.to_string())?;
        let keys: Vec<&str> = targets.iter().map(|t| t.key.as_str()).collect();
        let labels: Vec<&str> = targets.iter().map(|t| t.label.as_str()).collect();
        let mut arrays = Vec::with_capacity(2);
        for values in [&keys, &labels] {
            let array = env
                .new_object_array(values.len() as jsize, &string_class, JObject::null())
                .map_err(|e| e.to_string())?;
            for (i, value) in values.iter().enumerate() {
                let value = env.new_string(value).map_err(|e| e.to_string())?;
                array
                    .set_element(env, i, &value)
                    .map_err(|e| e.to_string())?;
            }
            arrays.push(JObject::from(array));
        }
        let result = env.call_method(
            act,
            jni::jni_str!("setShareTargets"),
            jni::jni_sig!("([Ljava/lang/String;[Ljava/lang/String;)V"),
            &[JValue::Object(&arrays[0]), JValue::Object(&arrays[1])],
        );
        if result.is_err() {
            activity::clear_exception(env);
        }
        result.map_err(|e| e.to_string())?;
        Ok(())
    })
}
