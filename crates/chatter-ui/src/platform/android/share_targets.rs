//! Direct Share: push the chats chatter-core allows in the system share sheet
//! to Android as sharing shortcuts (`MainActivity.setShareTargets`).
//!
//! The policy — which chats, whether any at all (signed out, app lock enabled,
//! the private default notification level, Shield mode) — is core's
//! (`ChatterClient::share_targets`). This module only delivers the answer, and
//! only when it changed: Android rate-limits shortcut updates.
//!
//! Three properties it must keep:
//! * a sign-out can never be overtaken: [`clear`] flips a flag *before* doing
//!   anything else, and every refresh checks it, so a refresh already queued
//!   behind a sign-out cannot republish names;
//! * nothing blocks a caller: refreshes run on a blocking task, [`clear`] never
//!   waits for the lock on the calling thread, and the (slow) chat-list query
//!   runs outside the lock;
//! * a bursty chat list costs one rebuild, not one per update: requests
//!   arriving while one is already waiting are folded into it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use std::sync::{Arc, Mutex};

use chatter_core::client::{ChatterClient, ShareTarget};
use jni::objects::{JObject, JValue};
use jni::sys::jsize;

use super::activity;

struct Published {
    /// What Android currently holds. `None` until the first successful push.
    targets: Option<Vec<ShareTarget>>,
    /// Sequence number of the newest request that has decided the state, so a
    /// slower, older refresh cannot overwrite a newer one.
    last_seq: u64,
}

static PUBLISHED: Mutex<Published> = Mutex::new(Published {
    targets: None,
    last_seq: 0,
});
static SEQ: AtomicU64 = AtomicU64::new(0);
/// Set by [`clear`] on sign-out, reset by [`signed_in`].
static SIGNED_OUT: AtomicBool = AtomicBool::new(false);
/// A refresh is waiting to run; a further request would be redundant.
static QUEUED: AtomicBool = AtomicBool::new(false);

/// The account is signed in (again): refreshes may publish.
pub fn signed_in() {
    SIGNED_OUT.store(false, SeqCst);
}

/// Re-ask core and push the result if it differs from what Android holds.
/// Cheap at the private default (core answers "none" before building any row).
pub fn refresh(client: &Arc<ChatterClient>) {
    if SIGNED_OUT.load(SeqCst) || QUEUED.swap(true, SeqCst) {
        return;
    }
    // Lifecycle hooks can fire before the runtime is entered on this thread;
    // a missed refresh is repaired by the next chat-list publish, a panic is not.
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        QUEUED.store(false, SeqCst);
        return;
    };
    let client = client.clone();
    runtime.spawn_blocking(move || {
        // From here a new request queues another run; this one reads the state
        // as of now, so it already covers everything requested before it began.
        QUEUED.store(false, SeqCst);
        let seq = SEQ.fetch_add(1, SeqCst) + 1;
        if SIGNED_OUT.load(SeqCst) {
            return;
        }
        let targets = client.share_targets();
        let Ok(mut published) = PUBLISHED.lock() else {
            return;
        };
        if SIGNED_OUT.load(SeqCst) || seq < published.last_seq {
            return;
        }
        published.last_seq = seq;
        if published.targets.as_ref() == Some(&targets) {
            return;
        }
        match push(&targets) {
            Ok(()) => published.targets = Some(targets),
            // Left as "not published", so the next refresh tries again.
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

/// Remove every published target (sign-out). Returns at once: the sign-out
/// path is async and must not wait on a lock a refresh may be holding.
pub fn clear() {
    SIGNED_OUT.store(true, SeqCst);
    let seq = SEQ.fetch_add(1, SeqCst) + 1;
    let remove = move || {
        let Ok(mut published) = PUBLISHED.lock() else {
            return;
        };
        published.last_seq = published.last_seq.max(seq);
        match push(&[]) {
            Ok(()) => published.targets = Some(Vec::new()),
            Err(error) => tracing::warn!("share targets not cleared: {error}"),
        }
    };
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => {
            runtime.spawn_blocking(remove);
        }
        Err(_) => remove(),
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
            jni::jni_sig!("([Ljava/lang/String;[Ljava/lang/String;)Z"),
            &[JValue::Object(&arrays[0]), JValue::Object(&arrays[1])],
        );
        if result.is_err() {
            activity::clear_exception(env);
        }
        // Android may refuse (rate-limited, OEM quirk): report it, don't assume.
        let accepted = result
            .and_then(|value| value.z())
            .map_err(|e| e.to_string())?;
        if accepted {
            Ok(())
        } else {
            Err("Android did not accept the shortcuts".into())
        }
    })
}
