//! Kotlin → Rust: the inbound half of the bridge.
//!
//! The five entry points in [`crate::NATIVE_METHODS`], bound to the Kotlin
//! class with `RegisterNatives` at load time rather than resolved lazily by
//! symbol name. A descriptor that does not match then fails when the library
//! loads, in a log line naming the method, instead of throwing
//! `NoSuchMethodError` the first time a peer walks into range.
//!
//! # These run on binder threads
//!
//! Android delivers Bluetooth callbacks on threads owned by the system's binder
//! pool. Three consequences shape everything in this file:
//!
//! 1. **The thread must be attached before any JNI call.** It is a thread the
//!    JVM has never seen from Rust's side; touching JNI without attaching is a
//!    segfault, not an error. Here that is free — the JVM called *us*, so the
//!    `Env` arrives as an argument — but it is why [`crate::bridge`] attaches
//!    explicitly on the way out.
//! 2. **Nothing may block.** A blocked Bluetooth callback stalls the whole
//!    Android Bluetooth stack, for every app, not just this one. Each function
//!    here does the minimum conversion and hands off to a bounded queue that
//!    drops rather than waits.
//! 3. **Nothing may panic.** Unwinding across the FFI boundary into the JVM is
//!    undefined behaviour. Every entry point runs inside
//!    [`std::panic::catch_unwind`] and reports to logcat instead.
//!
//! # `nativeOnConnected` means writable
//!
//! Kotlin must withhold it until the link can actually carry a write — after
//! `onServicesDiscovered` *and* the notification descriptor write for a central,
//! after the remote central's CCCD subscription for a peripheral. Not at
//! `onConnectionStateChange`. [`chatter_mesh_link::LinkEvent::Connected`] states
//! that as a promise to the layer above, and this is the half of the bridge that
//! has to keep it. The failure mode for getting it wrong is silently dropped
//! writes rather than an error, which is why it is written out on both sides.

use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};

use chatter_mesh_link::{LinkEvent, LinkRole};
use jni::objects::{JByteArray, JClass, JObject, JString};
use jni::sys::{JNI_VERSION_1_6, jint};
use jni::{Env, JavaVM, NativeMethod};

use crate::error;
use crate::link;
use crate::{BRIDGE_CLASS, NATIVE_METHODS};

/// Native handler names, in the order [`handlers`] returns them.
///
/// Kept as data so a test can assert it covers [`NATIVE_METHODS`] exactly, with
/// no JVM: a native listed in the contract with no handler behind it would
/// register nothing and fail silently at the first callback.
const HANDLER_NAMES: [&str; 5] = [
    "nativeOnDiscovered",
    "nativeOnConnected",
    "nativeOnDisconnected",
    "nativeOnData",
    "nativeOnAdapterPowerChanged",
];

/// Entry point the JVM calls when this library is loaded.
///
/// Always reports a supported JNI version, even when the bridge could not be
/// installed. This `.so` is the whole application — Slint, the UI, the client —
/// and returning `JNI_ERR` would make `System.loadLibrary` throw, taking the
/// app down over a Bluetooth-only fault. A device with no working mesh still
/// has the Nostr transport; a device that cannot start has nothing.
#[unsafe(no_mangle)]
pub extern "system" fn JNI_OnLoad(vm: *mut jni::sys::JavaVM, _reserved: *mut c_void) -> jint {
    match catch_unwind(AssertUnwindSafe(|| on_load(vm))) {
        Ok(Ok(())) => error::log_info(&format!(
            "chatter mesh: JNI bridge installed on {BRIDGE_CLASS} ({} natives)",
            NATIVE_METHODS.len()
        )),
        Ok(Err(reason)) => error::log_error(&format!(
            "chatter mesh: the BLE mesh is disabled — {reason}. The app still runs; \
             mesh traffic will have to go over another transport."
        )),
        Err(_) => error::log_error(
            "chatter mesh: JNI_OnLoad panicked; the BLE mesh is disabled for this process",
        ),
    }
    JNI_VERSION_1_6
}

fn on_load(raw_vm: *mut jni::sys::JavaVM) -> Result<(), String> {
    // SAFETY: this is the `JavaVM` the JVM itself passed to `JNI_OnLoad`, which
    // is valid for the life of the process. `from_raw` only null-checks it.
    if raw_vm.is_null() {
        return Err("JNI_OnLoad received a null JavaVM".to_owned());
    }
    let vm = unsafe { JavaVM::from_raw(raw_vm) };
    vm.attach_current_thread(|env| {
        Ok::<_, jni::errors::Error>((|| -> Result<(), String> {
            // FindClass resolves through the classloader of whatever is on the call
            // stack. Doing it here works because `System.loadLibrary` was called from
            // app code; the same call from a native thread later would see only the
            // system classloader and fail. That is the second reason registration
            // happens at load time rather than on demand.
            let class = match env.find_class(jni::strings::JNIString::from(BRIDGE_CLASS)) {
                Ok(class) => class,
                Err(err) => {
                    let described = error::drain_exception(&env, "FindClass");
                    return Err(format!(
                        "{BRIDGE_CLASS} was not found: {err}{}",
                        described
                            .map(|text| format!(" ({text})"))
                            .unwrap_or_default()
                    ));
                }
            };

            crate::bridge::install(env, &class)?;
            register_natives(env, &class)
        })())
    })
    .map_err(|e| e.to_string())?
}

/// Every native handler, paired with the name it answers to.
fn handlers() -> [(&'static str, *mut c_void); 5] {
    [
        (HANDLER_NAMES[0], native_on_discovered as *mut c_void),
        (HANDLER_NAMES[1], native_on_connected as *mut c_void),
        (HANDLER_NAMES[2], native_on_disconnected as *mut c_void),
        (HANDLER_NAMES[3], native_on_data as *mut c_void),
        (
            HANDLER_NAMES[4],
            native_on_adapter_power_changed as *mut c_void,
        ),
    ]
}

/// Bind the native table to the Kotlin class.
///
/// Descriptors come from [`NATIVE_METHODS`] and nowhere else, so there is one
/// place a descriptor can be wrong and one test that checks it against the
/// compiled Kotlin.
fn register_natives(env: &mut Env<'_>, class: &JClass<'_>) -> Result<(), String> {
    let handlers = handlers();
    let mut methods = Vec::with_capacity(NATIVE_METHODS.len());
    let names: Vec<jni::strings::JNIString> = NATIVE_METHODS
        .iter()
        .map(|(name, _)| (*name).into())
        .collect();
    let signatures: Vec<jni::strings::JNIString> = NATIVE_METHODS
        .iter()
        .map(|(_, sig)| (*sig).into())
        .collect();
    for (index, (name, descriptor)) in NATIVE_METHODS.iter().enumerate() {
        let Some((_, fn_ptr)) = handlers.iter().find(|(handler, _)| handler == name) else {
            return Err(format!(
                "{name}{descriptor} has no native handler behind it"
            ));
        };
        // SAFETY: the frozen descriptor/handler table is checked against Kotlin;
        // JNIString storage outlives RegisterNatives and JNI copies the descriptors.
        methods.push(unsafe {
            NativeMethod::from_raw_parts(&names[index], &signatures[index], *fn_ptr)
        });
    }

    // SAFETY: handler signatures match the frozen native table verified against Kotlin.
    match unsafe { env.register_native_methods(class, &methods) } {
        Ok(()) => Ok(()),
        Err(err) => {
            let described = error::drain_exception(env, "RegisterNatives");
            Err(format!(
                "RegisterNatives on {BRIDGE_CLASS} failed: {err}{}",
                described
                    .map(|text| format!(" ({text})"))
                    .unwrap_or_default()
            ))
        }
    }
}

/// Run a native entry point, catching anything that would unwind into the JVM.
///
/// An unwind across the FFI boundary is undefined behaviour, so this is not a
/// tidiness measure: without it, a panic anywhere below — a poisoned lock, an
/// allocation failure, a slice index — is a crash with no diagnosis.
fn guard(entry_point: &str, body: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(body)).is_err() {
        error::log_error(&format!(
            "chatter mesh: {entry_point} panicked; the event was dropped. This is a bug — a \
             native callback must never unwind into the JVM."
        ));
    }
}

/// Read a Java string, or `None` with a log line if it cannot be read.
fn read_string(env: &mut Env<'_>, value: &JString<'_>, during: &str) -> Option<String> {
    if value.as_raw().is_null() {
        error::log_warn(&format!("mesh: {during} was null, dropping the event"));
        return None;
    }
    let read = (value).try_to_string(&env);
    match read {
        Ok(text) => Some(String::from(text)),
        Err(err) => {
            let failure = error::jni_failure(env, during, &err);
            error::log_warn(&format!("mesh: {failure:?}, dropping the event"));
            None
        }
    }
}

/// Copy a Java byte array, or `None` with a log line if it cannot be read.
fn read_bytes(env: &Env<'_>, value: &JByteArray<'_>, during: &str) -> Option<Vec<u8>> {
    if value.as_raw().is_null() {
        error::log_warn(&format!("mesh: {during} was null, dropping the event"));
        return None;
    }
    match env.convert_byte_array(value) {
        Ok(bytes) => Some(bytes),
        Err(err) => {
            let failure = error::jni_failure(env, during, &err);
            error::log_warn(&format!("mesh: {failure:?}, dropping the event"));
            None
        }
    }
}

/// JNI uses an unsigned byte at the native boundary; normalize before Rust bool.
///
/// Kotlin sends 1 for `true`, but reading the byte as `== JNI_TRUE` would turn
/// any other non-zero value into `false`, which is the wrong way round for a
/// flag that decides which role a link took.
fn to_bool(raw: u8) -> bool {
    raw != 0
}

/// Which side of the link we are, from Kotlin's "is peripheral role" flag.
///
/// `true` means a remote central connected to *us*, so we are the peripheral
/// and we send by notifying. `false` means we opened the connection, so we are
/// the central and we send by writing a characteristic. Getting this backwards
/// picks the wrong send path for the link's whole life.
fn role_for(is_peripheral_role: bool) -> LinkRole {
    if is_peripheral_role {
        LinkRole::Peripheral
    } else {
        LinkRole::Central
    }
}

/// Narrow an advertisement's RSSI to the event's width.
///
/// Android reports RSSI as an `Int` in dBm — realistically −127..20 — but the
/// event carries an `i16`, and a scan record with no RSSI can arrive as a
/// sentinel far outside that range. Saturating is the honest conversion; a
/// truncating cast would turn an unknown RSSI into a plausible-looking strong
/// signal and the scheduler would spend a scarce central slot on it.
fn clamp_rssi(raw: jint) -> i16 {
    raw.clamp(i16::MIN as jint, i16::MAX as jint) as i16
}

/// JNI: `MeshBleBridge.nativeOnDiscovered`.
///
/// A device advertising the mesh service came into range. Not a connection —
/// the scheduler decides whether it is worth one of the scarce central slots,
/// and `address` is what [`chatter_mesh_link::MeshLink::connect`] takes.
extern "system" fn native_on_discovered(
    mut env: jni::EnvUnowned<'_>,
    _this: JObject<'_>,
    address: JString<'_>,
    rssi: jint,
    peer: JString<'_>,
) {
    env.with_env(|mut env| -> jni::errors::Result<_> {
        Ok((|| {
            guard("nativeOnDiscovered", || {
                let Some(address) = read_string(&mut env, &address, "nativeOnDiscovered(address)")
                else {
                    return;
                };
                // Null whenever the scan response has not arrived, which is common on the
                // first sighting and never an error. Unparseable is treated the same as
                // absent: the policy layer dials an unknown peer, so the worst case is the
                // behaviour we had before the id existed.
                let peer = (!peer.is_null())
                    .then(|| read_string(&mut env, &peer, "nativeOnDiscovered(peer)"))
                    .flatten()
                    .and_then(|hex| decode_peer_hex(&hex));
                link::dispatch(LinkEvent::Discovered {
                    address,
                    rssi: clamp_rssi(rssi),
                    peer,
                });
            });
        })())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}

/// Sixteen hex characters to the eight bytes they name.
fn decode_peer_hex(hex: &str) -> Option<[u8; 8]> {
    if hex.len() != 16 {
        return None;
    }
    let mut out = [0u8; 8];
    for (byte, pair) in out.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
        *byte = u8::from_str_radix(core::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

/// JNI: `MeshBleBridge.nativeOnConnected`.
///
/// This connection is writable **now** — see the module docs. `handle` names it
/// for every later callback and for every send, `is_peripheral_role` says which
/// direction it was opened in, and `max_write` is the negotiated ATT MTU less
/// the three-byte ATT header, which Kotlin computes from `onMtuChanged`.
extern "system" fn native_on_connected(
    mut env: jni::EnvUnowned<'_>,
    _this: JObject<'_>,
    handle: JString<'_>,
    is_peripheral_role: u8,
    max_write: jint,
) {
    env.with_env(|mut env| -> jni::errors::Result<_> {
        Ok((|| {
            guard("nativeOnConnected", || {
                let Some(handle) = read_string(&mut env, &handle, "nativeOnConnected(handle)")
                else {
                    return;
                };
                link::on_connected(&handle, role_for(to_bool(is_peripheral_role)), max_write);
            });
        })())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}

/// JNI: `MeshBleBridge.nativeOnDisconnected`.
///
/// The connection is gone. State keyed on it should be dropped, not retried.
extern "system" fn native_on_disconnected(
    mut env: jni::EnvUnowned<'_>,
    _this: JObject<'_>,
    handle: JString<'_>,
) {
    env.with_env(|mut env| -> jni::errors::Result<_> {
        Ok((|| {
            guard("nativeOnDisconnected", || {
                let Some(handle) = read_string(&mut env, &handle, "nativeOnDisconnected(handle)")
                else {
                    return;
                };
                link::on_disconnected(&handle);
            });
        })())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}

/// JNI: `MeshBleBridge.nativeOnData`.
///
/// One write or one notification from the peer, which is not necessarily one
/// packet — reassembly happens above. The array is copied here rather than
/// borrowed: it is a local reference belonging to this callback and is invalid
/// the moment it returns, while the event outlives it in the queue.
extern "system" fn native_on_data(
    mut env: jni::EnvUnowned<'_>,
    _this: JObject<'_>,
    handle: JString<'_>,
    bytes: JByteArray<'_>,
) {
    env.with_env(|mut env| -> jni::errors::Result<_> {
        Ok((|| {
            guard("nativeOnData", || {
                let Some(handle) = read_string(&mut env, &handle, "nativeOnData(handle)") else {
                    return;
                };
                let Some(bytes) = read_bytes(&env, &bytes, "nativeOnData(bytes)") else {
                    return;
                };
                link::on_data(&handle, bytes);
            });
        })())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}

/// JNI: `MeshBleBridge.nativeOnAdapterPowerChanged`.
///
/// The adapter was powered on or off underneath us — a system toggle, airplane
/// mode. Every link dies with it, which the link layer reports link by link so
/// the layer above does not have to find out one failed write at a time.
extern "system" fn native_on_adapter_power_changed(
    mut _env: jni::EnvUnowned<'_>,
    _this: JObject<'_>,
    powered: u8,
) {
    _env.with_env(|mut _env| -> jni::errors::Result<_> {
        Ok((|| {
            guard("nativeOnAdapterPowerChanged", || {
                link::dispatch(LinkEvent::AdapterPowerChanged {
                    powered: to_bool(powered),
                });
            });
        })())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_declared_native_has_a_handler_behind_it() {
        // A native in the contract with no handler registers nothing and then
        // fails at the first callback, on a phone. Cheap to rule out here.
        assert_eq!(HANDLER_NAMES.len(), NATIVE_METHODS.len());
        for (name, _) in NATIVE_METHODS {
            assert!(
                HANDLER_NAMES.contains(name),
                "{name} is declared but has no handler"
            );
        }
        for name in HANDLER_NAMES {
            assert!(
                NATIVE_METHODS.iter().any(|(declared, _)| *declared == name),
                "{name} has a handler but is not declared in NATIVE_METHODS"
            );
        }
    }

    #[test]
    fn handler_names_are_listed_in_the_declared_order() {
        // `handlers` pairs each name with its function positionally, so a name
        // out of order would bind the wrong pointer — a mismatch RegisterNatives
        // accepts happily, because both the name and the descriptor still exist.
        for (index, (declared, _)) in NATIVE_METHODS.iter().enumerate() {
            assert_eq!(
                HANDLER_NAMES[index], *declared,
                "handler {index} is out of order"
            );
        }
    }

    #[test]
    fn a_peripheral_role_flag_means_a_remote_central_connected_to_us() {
        // Backwards here picks the wrong send path — writing a characteristic
        // when we should be notifying — for the link's whole life.
        assert_eq!(role_for(true), LinkRole::Peripheral);
        assert_eq!(role_for(false), LinkRole::Central);
    }

    #[test]
    fn any_non_zero_java_boolean_is_true() {
        // JNI only specifies zero versus non-zero, and Kotlin is not the only
        // thing that can reach these entry points.
        assert!(!to_bool(0));
        assert!(to_bool(1));
        assert!(to_bool(2));
        assert!(to_bool(u8::MAX));
    }

    #[test]
    fn rssi_is_narrowed_by_saturating_not_by_truncating() {
        // A truncating cast turns Int.MIN_VALUE into 0 dBm — a signal strong
        // enough for the scheduler to spend a central slot on.
        assert_eq!(clamp_rssi(-70), -70);
        assert_eq!(clamp_rssi(0), 0);
        assert_eq!(clamp_rssi(127), 127);
        assert_eq!(clamp_rssi(i32::MIN), i16::MIN);
        assert_eq!(clamp_rssi(i32::MAX), i16::MAX);
    }

    #[test]
    fn a_discovery_carries_the_address_connect_will_be_given() {
        // The one string in this file that is an address rather than an opaque
        // Kotlin handle: it is what `MeshLink::connect` is called with.
        let event = LinkEvent::Discovered {
            peer: None,
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            rssi: clamp_rssi(-70),
        };
        assert_eq!(
            event,
            LinkEvent::Discovered {
                address: "AA:BB:CC:DD:EE:FF".to_string(),
                rssi: -70,
                peer: None,
            }
        );
    }

    #[test]
    fn adapter_power_marshals_both_ways() {
        assert_eq!(
            LinkEvent::AdapterPowerChanged {
                powered: to_bool(1)
            },
            LinkEvent::AdapterPowerChanged { powered: true }
        );
        assert_eq!(
            LinkEvent::AdapterPowerChanged {
                powered: to_bool(0)
            },
            LinkEvent::AdapterPowerChanged { powered: false }
        );
    }
}
