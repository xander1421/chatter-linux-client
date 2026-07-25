//! Rust → Kotlin: the outbound half of the bridge.
//!
//! Everything here funnels through one process-global [`Bridge`], installed
//! once from `JNI_OnLoad` (see [`crate::callbacks`]) and never rebuilt. It
//! caches five things, and each of them for a specific reason:
//!
//! - the **`JavaVM`**, because mesh tasks run on threads the JVM has never
//!   seen, and the only way to get a `JNIEnv` on such a thread is to attach it
//!   through the VM;
//! - a **global ref to the bridge class**, because a `JMethodID` is only valid
//!   while its class is loaded, and holding the class is what guarantees that;
//! - the **method IDs**, resolved at load time. `GetMethodID` on every send is
//!   both slow and a silent failure point — a descriptor that does not match
//!   fails at the first send rather than at library load, which is precisely
//!   the failure mode this crate is built to avoid;
//! - the **`INSTANCE` field id**, for the same reason;
//! - a **global ref to the singleton itself**, because a local ref is invalid
//!   the moment the call that produced it returns, and reusing one is a hard
//!   crash rather than an error.
//!
//! # `MeshBleBridge` is a Kotlin `object`
//!
//! There is no constructor to call. The singleton is reachable as the static
//! field `INSTANCE`, and every one of the eight bridge methods is an *instance*
//! method on it — `@JvmStatic` is deliberately not used on the Kotlin side,
//! because on an `external fun` it generates a static bridge over an instance
//! native that `RegisterNatives` cannot bind.
//!
//! The field id is resolved at load along with the method ids, but the object
//! itself is fetched on first use. `GetStaticFieldID` does not initialise the
//! class; `GetStaticObjectField` does, and the Kotlin object's initialiser
//! calls `System.loadLibrary`. Running that from inside `JNI_OnLoad` for the
//! very library being loaded is re-entrancy nobody needs to reason about.
//!
//! # `attach` before `start`
//!
//! The Kotlin side can find an application `Context` reflectively through
//! `ActivityThread.currentApplication`, but those accessors are greylisted:
//! they work today and are not promised to keep working. In a Rust-owned
//! process there is no Activity to inherit a context from, so [`start`] hands
//! one over explicitly first. A failure to do so is logged rather than fatal —
//! the fallback still exists, and losing the mesh over it would be a worse
//! trade than relying on a greylisted API for one more release.
//!
//! # Local references
//!
//! Every call here runs inside `PushLocalFrame`/`PopLocalFrame`. When Java
//! calls into native code the JVM reclaims local refs as the native method
//! returns, but these calls go the other way — out of a Rust-owned thread that
//! never returns to Java — so without an explicit frame every string and byte
//! array allocated for a send would live until the thread exits. On a mesh
//! sending continuously, that is an unbounded leak of JVM references.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use chatter_mesh_link::LinkError;
use jni::objects::{GlobalRef, JClass, JMethodID, JObject, JStaticFieldID, JString, JValue};
use jni::signature::{JavaType, Primitive, ReturnType};
use jni::sys::{JNI_FALSE, JNI_TRUE, jvalue};
use jni::{JNIEnv, JavaVM};

use crate::error::{self, Failure, Op};
use crate::{BRIDGE_CLASS, BRIDGE_METHODS};

/// The static field holding the Kotlin singleton.
const INSTANCE_FIELD: &str = "INSTANCE";

/// How many local references each bridge call may allocate.
///
/// Generous on purpose: `GetStringUTFChars` by way of `JNIEnv::get_string`
/// looks up `java/lang/String` and the argument's class, so even a call that
/// looks like it creates one reference creates several.
const LOCAL_FRAME_CAPACITY: i32 = 8;

/// The bridge, once `JNI_OnLoad` has installed it.
static BRIDGE: OnceLock<Bridge> = OnceLock::new();

/// Method IDs for every entry in [`BRIDGE_METHODS`], resolved once at load.
struct Methods {
    attach: JMethodID,
    start: JMethodID,
    stop: JMethodID,
    set_scanning: JMethodID,
    connect: JMethodID,
    disconnect: JMethodID,
    send: JMethodID,
    max_write: JMethodID,
}

impl Methods {
    fn resolve(env: &mut JNIEnv<'_>, class: &GlobalRef) -> Result<Self, String> {
        Ok(Self {
            attach: method_id(env, class, "attach")?,
            start: method_id(env, class, "start")?,
            stop: method_id(env, class, "stop")?,
            set_scanning: method_id(env, class, "setScanning")?,
            connect: method_id(env, class, "connect")?,
            disconnect: method_id(env, class, "disconnect")?,
            send: method_id(env, class, "send")?,
            max_write: method_id(env, class, "maxWrite")?,
        })
    }
}

/// The descriptor [`BRIDGE_METHODS`] gives for `name`.
///
/// Descriptors are never written out at a call site: they are looked up from
/// the frozen table, so there is exactly one place a descriptor can be wrong
/// and one test that checks it against the compiled Kotlin.
fn descriptor(name: &str) -> Option<&'static str> {
    BRIDGE_METHODS.iter().find(|(method, _)| *method == name).map(|(_, sig)| *sig)
}

/// The JNI type descriptor of the bridge class, `Lcom/chatter/client/…;`.
///
/// Derived from [`BRIDGE_CLASS`] rather than written out, so moving the package
/// cannot leave this behind.
fn instance_descriptor() -> String {
    format!("L{BRIDGE_CLASS};")
}

fn method_id(env: &mut JNIEnv<'_>, class: &GlobalRef, name: &'static str) -> Result<JMethodID, String> {
    let sig = descriptor(name)
        .ok_or_else(|| format!("{name} is not listed in BRIDGE_METHODS, so it has no descriptor"))?;
    match env.get_method_id(class, name, sig) {
        Ok(id) => Ok(id),
        Err(err) => {
            // A NoSuchMethodError is pending at this point; leaving it there
            // would abort the process on the next JNI call.
            let described = error::drain_exception(env, &format!("GetMethodID {name}{sig}"));
            Err(format!(
                "{name}{sig} is missing from the Kotlin bridge: {err}{}",
                described.map(|text| format!(" ({text})")).unwrap_or_default()
            ))
        }
    }
}

/// The outbound half of the bridge: everything needed to call Kotlin from any
/// thread.
pub struct Bridge {
    vm: JavaVM,
    /// Holding the class keeps the cached ids valid — method and field ids are
    /// invalidated when their class is unloaded.
    class: GlobalRef,
    methods: Methods,
    instance_field: JStaticFieldID,
    /// The singleton, fetched on first use. See the module docs for why this is
    /// not resolved at load time with everything else.
    instance: Mutex<Option<GlobalRef>>,
    /// Whether `attach(Context)` has already succeeded.
    attached: AtomicBool,
}

impl Bridge {
    /// A `JNIEnv` for the calling thread, attaching it first if needed.
    ///
    /// Attached as a *daemon* so that a lingering mesh thread cannot keep the
    /// JVM from exiting, and permanently rather than scoped because these
    /// threads make many calls and attach/detach is expensive.
    fn env(&self) -> Result<JNIEnv<'_>, Failure> {
        self.vm
            .attach_current_thread_as_daemon()
            .map_err(|err| Failure::Jni(format!("AttachCurrentThreadAsDaemon failed: {err}")))
    }

    /// `MeshBleBridge.INSTANCE`, fetched on first use and held globally after.
    fn instance(&self, env: &mut JNIEnv<'_>) -> Result<GlobalRef, Failure> {
        let mut slot = self.instance.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(existing) = slot.as_ref() {
            return Ok(existing.clone());
        }

        // The descriptor string is ignored for an object field — the id already
        // says which field this is — but naming it correctly keeps the call
        // readable next to the `GetStaticFieldID` that produced the id.
        let fetched = env.get_static_field_unchecked(
            &self.class,
            self.instance_field,
            JavaType::Object(BRIDGE_CLASS.to_string()),
        );
        let value = match fetched {
            Ok(value) => value,
            Err(err) => {
                return Err(error::jni_failure(env, "GetStaticObjectField(INSTANCE)", &err));
            }
        };

        let object = value.l()?;
        if object.as_raw().is_null() {
            return Err(Failure::Jni(format!(
                "{BRIDGE_CLASS}.{INSTANCE_FIELD} is null, which a Kotlin object's singleton never is"
            )));
        }

        let global = match env.new_global_ref(&object) {
            Ok(global) => global,
            Err(err) => return Err(error::jni_failure(env, "NewGlobalRef(INSTANCE)", &err)),
        };
        *slot = Some(global.clone());
        error::log_info("chatter mesh: MeshBleBridge.INSTANCE resolved");
        Ok(global)
    }
}

/// The Android `Context` published by `ndk-context`.
///
/// Returns `None` off-device or before the runtime has published one, rather
/// than propagating the panic `ndk_context::android_context` raises in that
/// case — a missing context means "no radio here", which is a condition the
/// mesh already handles, not a reason to take the process down.
fn app_context() -> Option<JObject<'static>> {
    let context = std::panic::catch_unwind(ndk_context::android_context).ok()?;
    let raw: jni::sys::jobject = context.context().cast();
    if raw.is_null() {
        return None;
    }
    // SAFETY: `ndk-context` publishes a JNI global reference to the
    // Activity/Application that lives for the whole process. It is only ever
    // passed on as an argument here and never deleted, which is what a
    // borrowed global reference allows.
    Some(unsafe { JObject::from_raw(raw) })
}

/// Cache the class, its method ids and the `INSTANCE` field id. Called once,
/// from `JNI_OnLoad`.
///
/// Failing here disables the mesh but must not disable the app: the caller logs
/// and carries on, so every later bridge call reports
/// [`Failure::NotInitialised`] instead.
pub fn install(env: &mut JNIEnv<'_>, class: &JClass<'_>) -> Result<(), String> {
    let vm = env.get_java_vm().map_err(|err| format!("GetJavaVM failed: {err}"))?;
    let class = env
        .new_global_ref(class)
        .map_err(|err| format!("NewGlobalRef on the bridge class failed: {err}"))?;
    let methods = Methods::resolve(env, &class)?;

    let descriptor = instance_descriptor();
    let instance_field = match env.get_static_field_id(&class, INSTANCE_FIELD, &descriptor) {
        Ok(id) => id,
        Err(err) => {
            let described =
                error::drain_exception(env, &format!("GetStaticFieldID {INSTANCE_FIELD}"));
            return Err(format!(
                "{INSTANCE_FIELD} {descriptor} is missing from the Kotlin bridge, so the \
                 singleton cannot be reached: {err}{}",
                described.map(|text| format!(" ({text})")).unwrap_or_default()
            ));
        }
    };

    BRIDGE
        .set(Bridge {
            vm,
            class,
            methods,
            instance_field,
            instance: Mutex::new(None),
            attached: AtomicBool::new(false),
        })
        .map_err(|_| "the bridge is already installed; JNI_OnLoad ran twice".to_string())
}

/// Has `JNI_OnLoad` installed a usable bridge?
///
/// Used to keep "turn scanning off" a no-op on a device with no radio, the way
/// the duty cycle expects, rather than an error it has to special-case.
pub fn is_installed() -> bool {
    BRIDGE.get().is_some()
}

/// Run `body` against the bridge and its singleton, inside a local reference
/// frame, mapping any failure onto the link layer's error type.
fn with_bridge<T>(
    op: Op,
    subject: &str,
    body: impl FnOnce(&mut JNIEnv<'_>, &Bridge, &GlobalRef) -> Result<T, Failure>,
) -> Result<T, LinkError> {
    let to_error = |failure| error::link_error(op, subject, failure);

    let bridge = BRIDGE.get().ok_or(Failure::NotInitialised).map_err(to_error)?;
    let mut env = bridge.env().map_err(to_error)?;
    let instance = bridge.instance(&mut env).map_err(to_error)?;
    env.with_local_frame(LOCAL_FRAME_CAPACITY, |env| body(env, bridge, &instance)).map_err(to_error)
}

/// Map a failed JNI call, draining any exception it left pending.
///
/// Always used in two steps — evaluate the call, then classify — because the
/// call needs `&mut JNIEnv` and the classification needs `&JNIEnv`.
fn classify<T>(env: &JNIEnv<'_>, during: &str, result: jni::errors::Result<T>) -> Result<T, Failure> {
    result.map_err(|err| error::jni_failure(env, during, &err))
}

/// Hand the bridge an application `Context`, once per process.
///
/// [`start`] calls this, so there is normally no reason to call it directly.
/// See the module docs for why it exists at all when Kotlin has a fallback.
pub fn attach() -> Result<(), LinkError> {
    with_bridge(Op::Attach, "", |env, bridge, instance| {
        if bridge.attached.load(Ordering::Acquire) {
            return Ok(());
        }

        let Some(context) = app_context() else {
            return Err(Failure::NotInitialised);
        };
        let args = [JValue::Object(&context).as_jni()];

        // SAFETY: `attach` was resolved from BRIDGE_METHODS as
        // `(Landroid/content/Context;)V` on this exact class, and is called with
        // one object and read as void.
        let called = unsafe {
            env.call_method_unchecked(
                instance,
                bridge.methods.attach,
                ReturnType::Primitive(Primitive::Void),
                &args,
            )
        };
        classify(env, "MeshBleBridge.attach", called)?.v()?;
        bridge.attached.store(true, Ordering::Release);
        Ok(())
    })
}

/// Begin advertising, publish the GATT server, and accept inbound connections.
///
/// Both roles start together: a device that only scans is invisible to peers
/// scanning for it, which removes a bridge the rest of the mesh may be relying
/// on rather than merely halving its own connectivity.
pub fn start() -> Result<(), LinkError> {
    // Best effort, and deliberately not fatal — Kotlin's reflective fallback
    // still stands. Doing it here rather than leaving it to the caller means no
    // future caller can forget the ordering.
    if let Err(err) = attach() {
        error::log_warn(&format!(
            "mesh: attach(Context) failed, leaving Kotlin to find one reflectively: {err}"
        ));
    }

    with_bridge(Op::Start, "", |env, bridge, instance| {
        // SAFETY: `start` was resolved from BRIDGE_METHODS as `()Z` on this
        // exact class, and is called with no arguments and read as a boolean.
        let called = unsafe {
            env.call_method_unchecked(
                instance,
                bridge.methods.start,
                ReturnType::Primitive(Primitive::Boolean),
                &[],
            )
        };
        let started = classify(env, "MeshBleBridge.start", called)?.z()?;
        if started { Ok(()) } else { Err(Failure::Rejected) }
    })
}

/// Stop advertising, stop scanning, and drop every connection.
pub fn stop() -> Result<(), LinkError> {
    with_bridge(Op::Stop, "", |env, bridge, instance| {
        // SAFETY: `stop` was resolved from BRIDGE_METHODS as `()V` on this
        // exact class, and is called with no arguments and read as void.
        let called = unsafe {
            env.call_method_unchecked(
                instance,
                bridge.methods.stop,
                ReturnType::Primitive(Primitive::Void),
                &[],
            )
        };
        classify(env, "MeshBleBridge.stop", called)?.v()?;
        Ok(())
    })
}

/// Turn scanning on or off without disturbing the peripheral role.
pub fn set_scanning(on: bool) -> Result<(), LinkError> {
    with_bridge(Op::SetScanning, "", |env, bridge, instance| {
        // A `jvalue` is built directly rather than through `JValue::Bool`,
        // whose object type parameter is unconstrained for a primitive-only
        // argument list. Constructing a union is safe; only reading is not.
        let args = [jvalue { z: if on { JNI_TRUE } else { JNI_FALSE } }];
        // SAFETY: `setScanning` was resolved from BRIDGE_METHODS as `(Z)Z` on
        // this exact class, and is called with one boolean and read as one.
        let called = unsafe {
            env.call_method_unchecked(
                instance,
                bridge.methods.set_scanning,
                ReturnType::Primitive(Primitive::Boolean),
                &args,
            )
        };
        let accepted = classify(env, "MeshBleBridge.setScanning", called)?.z()?;
        if accepted { Ok(()) } else { Err(Failure::Rejected) }
    })
}

/// Ask Kotlin to connect outward to `address`, taking the central role.
///
/// Returns the link id Kotlin will use to name this connection in every later
/// callback — `c:AA:BB:CC:DD:EE:FF`, role-prefixed and uppercased. The
/// connection is **not** writable yet: `nativeOnConnected` follows once service
/// discovery and the notification subscription have both completed, and
/// `nativeOnDisconnected` follows if a twenty-second watchdog fires first, so
/// an id handed back here always resolves one way or the other.
///
/// Calling this twice for the same address is idempotent on the Kotlin side and
/// returns the same id.
///
/// `null` means the attempt could not even be started, which is a
/// [`LinkError::ConnectFailed`] rather than a handle naming nothing.
pub fn connect(address: &str) -> Result<String, LinkError> {
    with_bridge(Op::Connect, address, |env, bridge, instance| {
        let created = env.new_string(address);
        let peer = classify(env, "NewStringUTF(address)", created)?;
        let args = [JValue::Object(&peer).as_jni()];

        // SAFETY: `connect` was resolved from BRIDGE_METHODS as
        // `(Ljava/lang/String;)Ljava/lang/String;` on this exact class, and is
        // called with one string and read as an object.
        let called = unsafe {
            env.call_method_unchecked(instance, bridge.methods.connect, ReturnType::Object, &args)
        };
        let returned = classify(env, "MeshBleBridge.connect", called)?.l()?;
        if returned.as_raw().is_null() {
            return Err(Failure::Rejected);
        }

        let handle = JString::from(returned);
        let read = env.get_string(&handle);
        Ok(String::from(classify(env, "GetStringUTFChars(link id)", read)?))
    })
}

/// Drop one connection. Kotlin ignores an id it does not recognise.
pub fn disconnect(handle: &str) -> Result<(), LinkError> {
    with_bridge(Op::Disconnect, handle, |env, bridge, instance| {
        let created = env.new_string(handle);
        let target = classify(env, "NewStringUTF(link id)", created)?;
        let args = [JValue::Object(&target).as_jni()];

        // SAFETY: `disconnect` was resolved from BRIDGE_METHODS as
        // `(Ljava/lang/String;)V` on this exact class, and is called with one
        // string and read as void.
        let called = unsafe {
            env.call_method_unchecked(
                instance,
                bridge.methods.disconnect,
                ReturnType::Primitive(Primitive::Void),
                &args,
            )
        };
        classify(env, "MeshBleBridge.disconnect", called)?.v()?;
        Ok(())
    })
}

/// Write one payload to one connection.
///
/// `false` from Kotlin means the write did not go out: an unknown link, a link
/// that is not writable yet, an oversized payload, or an ATT operation already
/// in flight. Android permits exactly one outstanding operation per connection,
/// so the last of those happens constantly under load and is retryable — never
/// a reason to tear the link down.
pub fn send(handle: &str, bytes: &[u8]) -> Result<(), LinkError> {
    with_bridge(Op::Send, handle, |env, bridge, instance| {
        let created = env.new_string(handle);
        let target = classify(env, "NewStringUTF(link id)", created)?;
        let allocated = env.byte_array_from_slice(bytes);
        let payload = classify(env, "NewByteArray(payload)", allocated)?;
        let args = [JValue::Object(&target).as_jni(), JValue::Object(&payload).as_jni()];

        // SAFETY: `send` was resolved from BRIDGE_METHODS as
        // `(Ljava/lang/String;[B)Z` on this exact class, and is called with one
        // string and one byte array and read as a boolean.
        let called = unsafe {
            env.call_method_unchecked(
                instance,
                bridge.methods.send,
                ReturnType::Primitive(Primitive::Boolean),
                &args,
            )
        };
        let accepted = classify(env, "MeshBleBridge.send", called)?.z()?;
        if accepted { Ok(()) } else { Err(Failure::Rejected) }
    })
}

/// The largest single write Kotlin will currently accept on this connection.
///
/// This is the *only* route by which a renegotiated MTU reaches Rust: there is
/// no `nativeOnMtuChanged`, and a peer may renegotiate after the link is
/// already writable. Without polling it, a link that later grew to 517 bytes
/// would keep being fragmented at the 20-byte default for the rest of its life.
///
/// Kotlin returns **zero** for a link that is unknown, gone, or not writable
/// yet — the JNI boundary has no nullable `Int`, so zero stands in for it. That
/// is reported here as `None`, never as a ceiling: the caller keeps whatever it
/// had rather than guessing, since guessing high means packets the controller
/// drops with no error anywhere.
pub fn max_write(handle: &str) -> Option<i32> {
    let queried = with_bridge(Op::MaxWrite, handle, |env, bridge, instance| {
        let created = env.new_string(handle);
        let target = classify(env, "NewStringUTF(link id)", created)?;
        let args = [JValue::Object(&target).as_jni()];

        // SAFETY: `maxWrite` was resolved from BRIDGE_METHODS as
        // `(Ljava/lang/String;)I` on this exact class, and is called with one
        // string and read as an int.
        let called = unsafe {
            env.call_method_unchecked(
                instance,
                bridge.methods.max_write,
                ReturnType::Primitive(Primitive::Int),
                &args,
            )
        };
        Ok(classify(env, "MeshBleBridge.maxWrite", called)?.i()?)
    });

    match queried {
        Ok(reported) => usable_max_write(reported),
        Err(err) => {
            error::log_warn(&format!("mesh: maxWrite({handle}) failed, keeping the cached size: {err}"));
            None
        }
    }
}

/// Interpret what `maxWrite` reported.
///
/// Zero is Kotlin's stand-in for null, and a negative number is not something
/// the Kotlin side can produce — both mean "no answer", which is not the same
/// as "this link carries nothing".
fn usable_max_write(reported: i32) -> Option<i32> {
    (reported > 0).then_some(reported)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every method this module resolves an ID for, in `Methods::resolve` order.
    const RESOLVED: [&str; 8] =
        ["attach", "start", "stop", "setScanning", "connect", "disconnect", "send", "maxWrite"];

    #[test]
    fn every_descriptor_comes_from_the_frozen_table() {
        // Descriptors are never written out at a call site. If one were, this
        // is the check that would stop mattering — so it asserts the lookup
        // rather than the strings.
        assert_eq!(descriptor("attach"), Some("(Landroid/content/Context;)V"));
        assert_eq!(descriptor("start"), Some("()Z"));
        assert_eq!(descriptor("connect"), Some("(Ljava/lang/String;)Ljava/lang/String;"));
        assert_eq!(descriptor("send"), Some("(Ljava/lang/String;[B)Z"));
        assert_eq!(descriptor("maxWrite"), Some("(Ljava/lang/String;)I"));
    }

    #[test]
    fn an_unknown_method_has_no_descriptor_rather_than_a_plausible_one() {
        // `method_id` turns this into a load-time error. Returning something
        // plausible instead would produce a NoSuchMethodError at first use, on
        // a phone, which is the failure mode this crate exists to avoid.
        assert_eq!(descriptor("setScanningMode"), None);
        assert_eq!(descriptor(""), None);
        assert_eq!(descriptor("nativeOnData"), None, "natives go the other way");
    }

    #[test]
    fn the_bridge_resolves_exactly_the_frozen_method_table() {
        // A method added to BRIDGE_METHODS and not resolved here is dead
        // weight; one resolved here and not in the table cannot be resolved at
        // all. Both are caught without a JVM.
        assert_eq!(RESOLVED.len(), BRIDGE_METHODS.len());
        for name in RESOLVED {
            assert!(descriptor(name).is_some(), "{name} is resolved but not in BRIDGE_METHODS");
        }
        for (name, _) in BRIDGE_METHODS {
            assert!(RESOLVED.contains(name), "{name} is in BRIDGE_METHODS but never resolved");
        }
    }

    #[test]
    fn the_instance_descriptor_follows_the_class_it_names() {
        // Written out by hand it would survive a package move silently, and
        // GetStaticFieldID would then fail at load with a confusing message.
        assert_eq!(instance_descriptor(), "Lcom/chatter/client/MeshBleBridge;");
        assert!(instance_descriptor().starts_with('L'));
        assert!(instance_descriptor().ends_with(';'));
        assert!(instance_descriptor().contains(BRIDGE_CLASS));
    }

    #[test]
    fn zero_from_max_write_means_no_answer_rather_than_a_zero_byte_link() {
        // JNI has no nullable Int, so Kotlin reports zero for a link that is
        // unknown, gone, or not writable yet. Reading that as a ceiling would
        // make every payload look oversized and stall the link permanently.
        assert_eq!(usable_max_write(0), None);
        assert_eq!(usable_max_write(-1), None);
        assert_eq!(usable_max_write(i32::MIN), None);
        assert_eq!(usable_max_write(20), Some(20));
        assert_eq!(usable_max_write(244), Some(244));
        assert_eq!(usable_max_write(514), Some(514));
    }

    #[test]
    fn nothing_is_installed_before_jni_onload_runs() {
        // Every public call in this module reports NotInitialised in that
        // state rather than dereferencing a null bridge, and the tests here
        // never install one.
        assert!(!is_installed());
    }
}
