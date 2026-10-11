//! Android and JNI failures, translated into the link layer's vocabulary — and
//! the logcat writer the rest of the crate reports through.
//!
//! # Why the classification is load bearing
//!
//! [`LinkError`] draws distinctions the layer above acts on directly, and the
//! two that matter most are the same two as on Linux:
//!
//! - [`LinkError::WriteRejected`] means *try again*. Android's
//!   `writeCharacteristic` returns `false` constantly under load, because there
//!   is one outstanding GATT operation per connection and the queue is one deep.
//!   Nothing below us retransmits, so a caller that discards on rejection loses
//!   the packet outright.
//! - [`LinkError::LinkGone`] means *stop*. Drop every piece of state keyed on
//!   this link and do not retry.
//!
//! Getting those backwards is invisible in review and produces either a mesh
//! that silently drops traffic under load or one that spins forever writing
//! into a connection whose peer walked out of the room.
//!
//! # Pending exceptions must be cleared
//!
//! ART's CheckJNI aborts the process if any JNI call is made while a Java
//! exception is pending. One undrained exception therefore does not fail the
//! call that caused it — it kills the app on some later, unrelated call, with a
//! stack trace pointing at innocent code. Every path that can leave an
//! exception pending goes through [`drain_exception`], which describes it to
//! logcat and then clears it.

use std::ffi::{CString, c_char, c_int};

use chatter_mesh_link::{LinkError, LinkId};
use jni::Env;

use crate::BRIDGE_CLASS;

// ---------------------------------------------------------------------------
// logcat
// ---------------------------------------------------------------------------

// Android's logging entry point, from bionic's `liblog`. Declared here rather
// than pulled in as a crate because it is three lines and this crate is
// otherwise pure Rust with no C dependency — see the note in `Cargo.toml`.
//
// `__android_log_write` is used in preference to `__android_log_print` because
// the latter is variadic, which Rust cannot call portably. Formatting happens
// on the Rust side anyway.
// liblog exists only in the Android NDK sysroot. The `cfg` is what lets the
// logic in this crate compile and be tested on the host: without it, a desktop
// `cargo test` fails at link time with "library not found: log", and the ~60
// tests below would only ever run on a device.
#[cfg(target_os = "android")]
#[link(name = "log")]
unsafe extern "C" {
    fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
}

const ANDROID_LOG_INFO: c_int = 4;
const ANDROID_LOG_WARN: c_int = 5;
const ANDROID_LOG_ERROR: c_int = 6;

/// Write one line to logcat.
///
/// Silently gives up if the message cannot be turned into a C string. That can
/// only happen for an interior NUL, which is stripped first, so in practice it
/// never happens — and a logging failure must never take a caller down,
/// especially since some callers are binder threads.
fn write(priority: c_int, message: &str) {
    let sanitised = if message.contains('\0') {
        message.replace('\0', "\u{fffd}")
    } else {
        message.to_string()
    };
    let Ok(text) = CString::new(sanitised) else {
        return;
    };
    #[cfg(target_os = "android")]
    {
        // SAFETY: both pointers are valid, NUL-terminated C strings that outlive
        // the call, which is all `__android_log_write` requires of them.
        unsafe {
            __android_log_write(priority, c"chatter-mesh".as_ptr(), text.as_ptr());
        }
    }

    // On the host this code exists only so the tests can run. Nothing consumes
    // the output, so it goes to stderr where a failing test will surface it,
    // rather than being silently discarded.
    #[cfg(not(target_os = "android"))]
    {
        let _ = priority;
        eprintln!("[chatter-mesh] {}", text.to_string_lossy());
    }
}

/// Something worth knowing that is not a fault: the bridge came up, the radio
/// stopped.
pub fn log_info(message: &str) {
    write(ANDROID_LOG_INFO, message);
}

/// Something was lost or degraded but the mesh keeps running: a dropped event,
/// a callback that arrived with no link attached.
pub fn log_warn(message: &str) {
    write(ANDROID_LOG_WARN, message);
}

/// The mesh is broken and will stay broken: the bridge class is missing, a
/// descriptor does not match, a native entry point panicked.
pub fn log_error(message: &str) {
    write(ANDROID_LOG_ERROR, message);
}

// ---------------------------------------------------------------------------
// Java exceptions
// ---------------------------------------------------------------------------

/// Describe and clear any pending Java exception.
///
/// Returns a short description when there was one, or `None` when the JNI call
/// failed for a reason that did not involve Java throwing.
///
/// The exception's own message is deliberately *not* extracted: reading it
/// means calling `Throwable.toString` through JNI immediately after an
/// exception, which is exactly the state where further JNI calls are most
/// likely to go wrong. `ExceptionDescribe` already writes the full stack trace
/// to logcat, which is strictly more information, and then `ExceptionClear`
/// restores the environment to a state where the next call can succeed.
pub fn drain_exception(env: &Env<'_>, during: &str) -> Option<String> {
    if !env.exception_check() {
        return None;
    }
    // Order matters: describe first (it does not clear), then clear.
    let _ = env.exception_describe();
    let _ = env.exception_clear();
    Some(format!(
        "a java exception was thrown during {during}; stack trace is in logcat"
    ))
}

/// Turn a failed JNI call into a [`Failure`], draining any exception it left
/// pending on the way.
pub fn jni_failure(env: &Env<'_>, during: &str, err: &jni::errors::Error) -> Failure {
    match drain_exception(env, during) {
        Some(description) => Failure::Jni(format!("{during}: {err} ({description})")),
        None => Failure::Jni(format!("{during}: {err}")),
    }
}

// ---------------------------------------------------------------------------
// LinkError mapping
// ---------------------------------------------------------------------------

/// Which call into the Kotlin bridge failed.
///
/// The operation, not the failure, decides most of the mapping: "the radio said
/// no" means a retryable write on [`Op::Send`] and a dead advertisement on
/// [`Op::Start`], and those are not interchangeable to the layer above.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Attach,
    Start,
    Stop,
    SetScanning,
    SetLocalPeer,
    Connect,
    Disconnect,
    Send,
    MaxWrite,
}

impl Op {
    /// The Kotlin method this operation calls.
    ///
    /// Every one of these appears in [`crate::BRIDGE_METHODS`], which a test in
    /// this module asserts — so a log line naming a method is naming a method
    /// that actually exists rather than a stale guess.
    pub fn method(self) -> &'static str {
        match self {
            Op::Attach => "attach",
            Op::Start => "start",
            Op::Stop => "stop",
            Op::SetScanning => "setScanning",
            Op::SetLocalPeer => "setLocalPeer",
            Op::Connect => "connect",
            Op::Disconnect => "disconnect",
            Op::Send => "send",
            Op::MaxWrite => "maxWrite",
        }
    }
}

/// How a call into the Kotlin bridge failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// There is no bridge to call: `JNI_OnLoad` did not run, the class or its
    /// `INSTANCE` field was not found, or there is no Android context to hand
    /// over.
    NotInitialised,

    /// The Kotlin method ran and said no — returned `false`, or `null` from
    /// `connect`. The radio is there; it declined.
    Rejected,

    /// The JNI call itself failed, or Java threw. Includes a detached thread, a
    /// descriptor mismatch, and out-of-memory.
    Jni(String),
}

impl From<jni::errors::Error> for Failure {
    /// Fallback conversion for JNI calls whose failure carries no extra context
    /// worth naming — chiefly `PushLocalFrame`/`PopLocalFrame`. Paths where the
    /// operation matters use [`jni_failure`], which also drains the exception.
    fn from(err: jni::errors::Error) -> Self {
        Failure::Jni(err.to_string())
    }
}

/// Map a bridge failure onto the link layer's error type.
///
/// `subject` is whatever the operation was about: the peer address for
/// [`Op::Connect`], the Kotlin connection handle for the per-link operations,
/// and empty for the ones that take no argument.
pub fn link_error(op: Op, subject: &str, failure: Failure) -> LinkError {
    let method = op.method();
    match failure {
        // Without the bridge there is no radio at all, whatever the caller was
        // trying to do. Reporting that as "adapter unavailable" is accurate and
        // is the one error the policy layer already knows how to sit out.
        Failure::NotInitialised => LinkError::AdapterUnavailable(format!(
            "the Kotlin BLE bridge is not available, so {BRIDGE_CLASS}.{method}() cannot be called: \
             JNI_OnLoad did not run, the class or its INSTANCE field was not found, or there is no \
             Android context"
        )),

        Failure::Rejected => match op {
            // Advertising or the GATT server could not be published. Distinct
            // from an adapter fault: the adapter is fine, the role is not.
            Op::Start => LinkError::PeripheralSetup(
                "start() returned false: advertising or the GATT server could not be published \
                 (BLUETOOTH_ADVERTISE or BLUETOOTH_CONNECT refused, or the adapter is off)"
                    .to_string(),
            ),
            // A refused scan on Android is almost always a missing
            // BLUETOOTH_SCAN grant or an adapter that has just been switched
            // off, and both mean the radio is not usable right now.
            Op::SetScanning => LinkError::AdapterUnavailable(
                "setScanning() returned false: BLUETOOTH_SCAN was refused or the adapter is off"
                    .to_string(),
            ),
            // `setLocalPeer` returns void, so it cannot refuse. The arm exists
            // for completeness; failing to publish the id costs deduplication,
            // not the radio.
            Op::SetLocalPeer => LinkError::AdapterUnavailable(
                "setLocalPeer() failed; peers cannot recognise us across an address rotation"
                    .to_string(),
            ),
            Op::Connect => LinkError::ConnectFailed {
                address: subject.to_string(),
                reason: "connect() returned null".to_string(),
            },
            // `disconnect` returns void, so it cannot actually refuse; the arm
            // exists for completeness and maps to the only sensible reading —
            // the handle names nothing.
            Op::Disconnect => LinkError::LinkGone(subject.to_string()),
            // The single most important mapping here. See the module docs.
            Op::Send => LinkError::WriteRejected(subject.to_string()),
            // `attach` and `stop` return void and `maxWrite` returns a size, so
            // none of the three has a way to refuse in the first place.
            Op::Attach | Op::Stop | Op::MaxWrite => {
                LinkError::Backend(format!("{method}() refused"))
            }
        },

        Failure::Jni(detail) => match op {
            Op::Start => LinkError::PeripheralSetup(format!("{method}(): {detail}")),
            // From the caller's point of view "the JVM misbehaved" and "the
            // peer refused" are the same event: no link came up, and the
            // scheduler moves on to another candidate.
            Op::Connect => LinkError::ConnectFailed {
                address: subject.to_string(),
                reason: detail,
            },
            _ => LinkError::Backend(format!("{method}(): {detail}")),
        },
    }
}

/// A connect attempt that failed before or after the bridge call itself, e.g.
/// Kotlin handing back an empty handle.
pub fn connect_failed(address: &str, reason: impl Into<String>) -> LinkError {
    LinkError::ConnectFailed {
        address: address.to_string(),
        reason: reason.into(),
    }
}

/// This link is gone and the caller should drop its state for it.
pub fn link_gone(link: &LinkId) -> LinkError {
    LinkError::LinkGone(link.to_string())
}

/// Does this error mean the caller should tear the link down rather than retry?
///
/// Same predicate as the BlueZ backend's, and for the same reason: `send` uses
/// it to decide whether a failed write also destroys the link.
pub fn is_terminal(err: &LinkError) -> bool {
    matches!(err, LinkError::LinkGone(_))
}

/// The error every operation gets once [`crate::AndroidLink::stop`] has run.
///
/// Stopping is terminal, exactly as on Linux: restarting the radio means
/// constructing a new link object, not calling `start` again.
pub fn stopped() -> LinkError {
    LinkError::Backend(
        "this link layer has been stopped; construct a new AndroidLink to restart".to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BRIDGE_METHODS;

    const ADDRESS: &str = "AA:BB:CC:DD:EE:FF";
    /// A link id as the Kotlin side mints them: role-prefixed and uppercased.
    const HANDLE: &str = "c:AA:BB:CC:DD:EE:FF";

    const EVERY_OP: [Op; 8] = [
        Op::Attach,
        Op::Start,
        Op::Stop,
        Op::SetScanning,
        Op::Connect,
        Op::Disconnect,
        Op::Send,
        Op::MaxWrite,
    ];

    #[test]
    fn every_op_names_a_method_the_bridge_actually_has() {
        // The whole point of BRIDGE_METHODS being a single frozen table is that
        // nothing else may invent a method name. A log line naming a method
        // that does not exist sends the next reader hunting through Kotlin for
        // something that was never there.
        for op in EVERY_OP {
            assert!(
                BRIDGE_METHODS.iter().any(|(name, _)| *name == op.method()),
                "{:?} names {}, which is not in BRIDGE_METHODS",
                op,
                op.method()
            );
        }
    }

    #[test]
    fn op_methods_are_distinct() {
        for (index, op) in EVERY_OP.iter().enumerate() {
            for other in &EVERY_OP[index + 1..] {
                assert_ne!(
                    op.method(),
                    other.method(),
                    "{op:?} and {other:?} share a method name"
                );
            }
        }
    }

    #[test]
    fn a_refused_write_is_retryable_not_fatal() {
        // BLE writes are refused routinely — Android allows one outstanding
        // GATT operation per connection. Treating that as a dead link would
        // tear down every busy peer in the room.
        let err = link_error(Op::Send, HANDLE, Failure::Rejected);
        assert_eq!(err, LinkError::WriteRejected(HANDLE.to_string()));
        assert!(!is_terminal(&err));
    }

    #[test]
    fn a_null_handle_from_connect_is_a_connect_failure_not_an_empty_link() {
        let err = link_error(Op::Connect, ADDRESS, Failure::Rejected);
        match err {
            LinkError::ConnectFailed { address, reason } => {
                assert_eq!(address, ADDRESS);
                assert!(
                    reason.contains("null"),
                    "reason should say what happened: {reason}"
                );
            }
            other => panic!("expected ConnectFailed, got {other:?}"),
        }
    }

    #[test]
    fn a_jni_failure_during_connect_still_reads_as_a_connect_failure() {
        // "The JVM misbehaved" and "the peer refused" are the same event to the
        // scheduler: no link came up, try someone else.
        let err = link_error(
            Op::Connect,
            ADDRESS,
            Failure::Jni("detached thread".to_string()),
        );
        match err {
            LinkError::ConnectFailed { address, reason } => {
                assert_eq!(address, ADDRESS);
                assert!(reason.contains("detached thread"));
            }
            other => panic!("expected ConnectFailed, got {other:?}"),
        }
    }

    #[test]
    fn a_refused_start_blames_the_peripheral_role_not_the_adapter() {
        assert!(matches!(
            link_error(Op::Start, "", Failure::Rejected),
            LinkError::PeripheralSetup(_)
        ));
        assert!(matches!(
            link_error(Op::Start, "", Failure::Jni("boom".to_string())),
            LinkError::PeripheralSetup(_)
        ));
    }

    #[test]
    fn a_refused_scan_reads_as_an_unusable_adapter() {
        // Practically always a missing BLUETOOTH_SCAN grant or an adapter that
        // just went off, and the policy layer already backs off on that error.
        assert!(matches!(
            link_error(Op::SetScanning, "", Failure::Rejected),
            LinkError::AdapterUnavailable(_)
        ));
    }

    #[test]
    fn a_missing_bridge_is_an_unusable_adapter_for_every_operation() {
        // Without the bridge there is no radio, whatever was being attempted.
        for op in EVERY_OP {
            let err = link_error(op, HANDLE, Failure::NotInitialised);
            match err {
                LinkError::AdapterUnavailable(message) => {
                    assert!(message.contains(BRIDGE_CLASS), "{op:?}: {message}");
                    assert!(message.contains(op.method()), "{op:?}: {message}");
                }
                other => panic!("{op:?}: expected AdapterUnavailable, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_unclassified_jni_failure_names_the_method_that_failed() {
        let err = link_error(
            Op::Send,
            HANDLE,
            Failure::Jni("NewByteArray returned null".to_string()),
        );
        match err {
            LinkError::Backend(message) => {
                assert!(message.contains("send()"), "{message}");
                assert!(message.contains("NewByteArray"), "{message}");
            }
            other => panic!("expected Backend, got {other:?}"),
        }
    }

    #[test]
    fn only_a_gone_link_is_terminal() {
        assert!(is_terminal(&LinkError::LinkGone(HANDLE.to_string())));
        assert!(!is_terminal(&LinkError::WriteRejected(HANDLE.to_string())));
        assert!(!is_terminal(&LinkError::PayloadTooLarge {
            actual: 300,
            max: 244
        }));
        assert!(!is_terminal(&LinkError::Backend("whatever".to_string())));
        assert!(!is_terminal(&LinkError::AdapterUnavailable(
            "off".to_string()
        )));
    }

    #[test]
    fn stopping_is_reported_as_terminal_but_not_as_a_dead_link() {
        // `stop` is final: the caller has to build a new AndroidLink. That is
        // not a per-link condition, so it must not look like one.
        let err = stopped();
        assert!(matches!(err, LinkError::Backend(_)));
        assert!(!is_terminal(&err));
    }

    #[test]
    fn connect_failures_carry_the_address_they_were_for() {
        let err = connect_failed(ADDRESS, "empty handle");
        assert_eq!(
            err,
            LinkError::ConnectFailed {
                address: ADDRESS.to_string(),
                reason: "empty handle".to_string()
            }
        );
    }
}
