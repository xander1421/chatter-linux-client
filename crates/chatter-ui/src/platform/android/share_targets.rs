//! Direct Share: push the chats chatter-core allows in the system share sheet
//! to Android as sharing shortcuts (`MainActivity.setShareTargets`).
//!
//! The policy — which chats, whether any at all (signed out, app lock enabled,
//! the private default notification level, Shield mode) — is core's
//! (`ChatterClient::share_targets`). This module only delivers the answer, and
//! only when it changed: Android rate-limits shortcut updates.
//!
//! Properties it must keep:
//! * **Read order is publish order.** Refreshes run one at a time (`REFRESH`),
//!   each reading the chat list and publishing inside the same critical
//!   section, so a refresh that read later can never be overwritten by one
//!   that read earlier.
//! * **A sign-out is never overtaken.** [`clear`] flips `SIGNED_OUT` before
//!   anything else; every refresh checks it before reading and again right
//!   before publishing. And a queued removal is skipped if the account signed
//!   in again meanwhile ([`signed_in`]): the new account's own refresh sets
//!   what Android should hold, and the removal must not erase it.
//! * **Callers do not wait.** Refreshes and removals run on blocking tasks;
//!   [`clear`] never waits on the (slow) chat-list query, since only `REFRESH`
//!   is held across that. The one inline case is [`clear`] with no async
//!   runtime at all, where removing is done on the spot rather than skipped.
//! * **Bursts coalesce.** Requests arriving while a refresh is already waiting
//!   are folded into it (it reads the state when it runs).

use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Arc, Mutex, MutexGuard};

use chatter_core::client::{ChatterClient, ShareTarget};
use jni::objects::{JObject, JValue};
use jni::sys::jsize;

use super::activity;

/// What Android currently holds. `None` until the first successful push.
/// Held only around a push, never across the chat-list query.
static PUBLISHED: Mutex<Option<Vec<ShareTarget>>> = Mutex::new(None);
/// Serialises refreshes: held across the query *and* the publish.
static REFRESH: Mutex<()> = Mutex::new(());
/// Set by [`clear`] on sign-out, reset by [`signed_in`].
static SIGNED_OUT: AtomicBool = AtomicBool::new(false);
/// A refresh is waiting to run; a further request would be redundant.
static QUEUED: AtomicBool = AtomicBool::new(false);

/// Lock, recovering a poisoned guard. A panic in a query or a JNI push must not
/// leave every later refresh silently returning: both locks guard values that
/// are valid whichever way a holder ended (`()`, and the last-known list).
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

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
        let _serial = lock(&REFRESH);
        // The state is read from here on, so this run covers every request
        // made before it; a request from now on queues another run.
        QUEUED.store(false, SeqCst);
        if SIGNED_OUT.load(SeqCst) {
            return;
        }
        let targets = client.share_targets();
        let mut published = lock(&PUBLISHED);
        if SIGNED_OUT.load(SeqCst) || published.as_ref() == Some(&targets) {
            return;
        }
        match push(&targets) {
            Ok(()) => *published = Some(targets),
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

/// Remove every published target (sign-out). Returns at once when an async
/// runtime is available; with none it removes inline rather than skip it.
pub fn clear() {
    SIGNED_OUT.store(true, SeqCst);
    let remove = || {
        let mut published = lock(&PUBLISHED);
        // Signed in again before this ran: the new account's refresh owns the
        // state now, and an unconditional removal would erase what it published.
        if !SIGNED_OUT.load(SeqCst) {
            return;
        }
        match push(&[]) {
            Ok(()) => *published = Some(Vec::new()),
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
