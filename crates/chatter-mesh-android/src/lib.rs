//! Android implementation of [`chatter_mesh_link::MeshLink`], over JNI.
//!
//! Android's BLE peripheral APIs — `BluetoothLeAdvertiser`, `BluetoothGattServer` —
//! are Java-only, with no NDK equivalent. The radio therefore has to be Kotlin and
//! the bridge has to be JNI.
//!
//! Our Android app is Slint on GameActivity, so the **process is Rust-owned** and
//! Kotlin is the guest rather than the host. That inverts the usual Android
//! arrangement and sets the call direction: Rust drives, Kotlin serves.
//!
//! # The whole risk is in the signatures
//!
//! Everything else in this crate fails at compile time. A JNI descriptor mismatch
//! does not — it produces `NoSuchMethodError` the first time a peer appears, on a
//! user's phone, with the mesh silently dead. Three things guard against that:
//!
//! 1. Natives are registered with `RegisterNatives` in `JNI_OnLoad`, so a mismatch
//!    fails when the library loads instead of at first use.
//! 2. `tests/jni_signatures.rs` runs `javap -s` over the compiled Kotlin class and
//!    asserts every descriptor below is present verbatim. It needs no device.
//! 3. The class and package are named once, here, so a rename becomes a Rust
//!    compile error rather than silent drift.
//!
//! # Not verifiable without hardware
//!
//! Emulators have no Bluetooth radio. An AVD can exercise the permission flow and
//! the service lifecycle, but never advertising, scanning, or byte transfer. Do not
//! read emulator success as radio verification.

#![cfg_attr(not(target_os = "android"), allow(unused))]
#![forbid(unsafe_op_in_unsafe_fn)]

#[cfg(any(target_os = "android", test))]
pub mod bridge;
#[cfg(any(target_os = "android", test))]
pub mod callbacks;
#[cfg(any(target_os = "android", test))]
pub mod error;
#[cfg(any(target_os = "android", test))]
pub mod link;

#[cfg(any(target_os = "android", test))]
pub use link::AndroidLink;

/// Fully-qualified JNI name of the Kotlin bridge class.
///
/// Named once so that renaming the class or moving the package breaks the Rust
/// build rather than silently producing a `ClassNotFoundException` at runtime.
/// `tests/jni_signatures.rs` resolves the compiled class from this same constant.
pub const BRIDGE_CLASS: &str = "com/chatter/client/MeshBleBridge";

/// Native methods the Kotlin side calls, with their exact JNI descriptors.
///
/// This table is the contract. It is registered via `RegisterNatives` at load time
/// and asserted against the compiled Kotlin by the signature test — so these
/// strings are checked from both sides rather than trusted.
pub const NATIVE_METHODS: &[(&str, &str)] = &[
    // (name, JNI descriptor)
    // The trailing string is the advertised peer id as hex, or null when the
    // scan response has not been seen. It is what makes a rotated private
    // address recognisable without connecting to find out.
    ("nativeOnDiscovered", "(Ljava/lang/String;ILjava/lang/String;)V"),
    ("nativeOnConnected", "(Ljava/lang/String;ZI)V"),
    ("nativeOnDisconnected", "(Ljava/lang/String;)V"),
    ("nativeOnData", "(Ljava/lang/String;[B)V"),
    ("nativeOnAdapterPowerChanged", "(Z)V"),
];

/// Methods this crate calls on the Kotlin bridge, with their exact descriptors.
///
/// Same contract in the other direction, asserted by the same test.
pub const BRIDGE_METHODS: &[(&str, &str)] = &[
    // Hands the bridge an application Context. Call before `start`.
    //
    // The bridge can fall back to reflection over `ActivityThread.currentApplication`,
    // but those are greylisted APIs that work today and are not guaranteed to keep
    // working. In a Rust-owned process there is no Activity to inherit a Context
    // from, so passing one explicitly is the difference between a supported path and
    // one that breaks on some future Android release.
    ("attach", "(Landroid/content/Context;)V"),
    ("start", "()Z"),
    ("stop", "()V"),
    ("setScanning", "(Z)Z"),
    // The eight-byte peer id as hex, advertised in the scan response so peers
    // can recognise us across a private address rotation. Call before `start`.
    ("setLocalPeer", "(Ljava/lang/String;)V"),
    ("connect", "(Ljava/lang/String;)Ljava/lang/String;"),
    ("disconnect", "(Ljava/lang/String;)V"),
    ("send", "(Ljava/lang/String;[B)Z"),
    ("maxWrite", "(Ljava/lang/String;)I"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_are_well_formed() {
        // A malformed descriptor is accepted by RegisterNatives and then never
        // matches anything, which is the silent-failure mode this crate exists to
        // avoid. Cheap structural check, run everywhere including desktop.
        for (name, descriptor) in NATIVE_METHODS.iter().chain(BRIDGE_METHODS) {
            assert!(!name.is_empty(), "method name must not be empty");
            assert!(
                descriptor.starts_with('('),
                "{name}: descriptor must start with '(' — got {descriptor}"
            );
            assert!(
                descriptor.contains(')'),
                "{name}: descriptor must have a return type — got {descriptor}"
            );
            let (_, ret) = descriptor.rsplit_once(')').unwrap_or_default();
            assert!(
                !ret.is_empty(),
                "{name}: descriptor has no return type — got {descriptor}"
            );
        }
    }

    #[test]
    fn every_callback_returns_void() {
        // Callbacks run on binder threads. A return value would imply the Kotlin
        // side waits for Rust, and a blocked binder callback stalls the entire
        // Bluetooth stack — so "returns void" is a threading invariant, not style.
        for (name, descriptor) in NATIVE_METHODS {
            assert!(
                descriptor.ends_with(")V"),
                "{name} must return void so binder threads never wait on Rust"
            );
        }
    }

    #[test]
    fn the_bridge_class_is_a_jni_internal_name() {
        // Slashes, not dots. FindClass takes the internal form, and the dotted
        // form fails at runtime rather than compile time.
        assert!(!BRIDGE_CLASS.contains('.'), "use slashes, not dots");
        assert!(BRIDGE_CLASS.contains('/'));
        assert!(BRIDGE_CLASS.ends_with("MeshBleBridge"));
    }
}
