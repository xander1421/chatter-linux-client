# chatter-mesh-android

The Android radio for the bitchat mesh: a `chatter_mesh_link::MeshLink`
implementation backed by Android Bluetooth LE, over JNI.

## Why half of this is Kotlin

Android's BLE **peripheral** APIs — `BluetoothLeAdvertiser`, `BluetoothGattServer` —
are Java-only. There is no NDK equivalent, so no amount of Rust reaches them
directly. A mesh node has to advertise as well as scan, so the radio has to be
Kotlin and the bridge has to be JNI.

The unusual part is the direction. The app is Slint on GameActivity, so the
**process is Rust-owned** and Kotlin is the guest rather than the host — the
opposite of a normal Android app. Rust drives; Kotlin serves.

```
chatter_mesh_link::MeshLink        (trait)
        ▲ implements
AndroidLink                        Rust, cfg(target_os = "android")
        │  JNI calls ───────────►  MeshBleBridge.kt      Kotlin
        ◄──── native callbacks ─┘  (advertiser, GATT server, scanner, GATT client)
```

`src/lib.rs` names the class once (`BRIDGE_CLASS`) and both descriptor tables
(`BRIDGE_METHODS`, `NATIVE_METHODS`). **Those constants are the contract.** Nothing
else in the repo may restate them; the signature test imports them.

## The failure mode this crate is organised around

Everything else here fails at compile time. A JNI descriptor mismatch does not. A
wrong descriptor, a renamed class, or a moved package produces `NoSuchMethodError`
at *first call* — on a user's phone, in the field, with the mesh silently dead and
no crash until a peer happens to appear.

Three guards, in order of value:

1. **`RegisterNatives` in `JNI_OnLoad`,** not lazy symbol lookup, so a mismatch
   fails when the library loads instead of at first use.
2. **`tests/jni_signatures.rs`** — runs `javap -s -p` over the class Gradle
   compiled and asserts every descriptor byte for byte, in both directions. No
   device needed. This is the only check that catches the whole class of error
   without hardware.
3. **One class name in one Rust constant,** so a rename is a Rust compile error
   rather than silent drift.

## Commands

**Rust side** (pure Rust — cross-compiles without a C toolchain or the NDK):

```bash
rustup target add aarch64-linux-android          # once
cargo check -p chatter-mesh-android --target aarch64-linux-android
cargo test  -p chatter-mesh-android              # host: contract + parser tests
```

**Kotlin side** (needs the Android SDK; `JAVA_HOME` must point at a JDK — Android
Studio's bundled one works):

```bash
cd linux-client/android
JAVA_HOME=/opt/android-studio/jbr ./gradlew :app:compileDebugKotlin
```

**The signature check** — after Gradle has compiled the Kotlin at least once:

```bash
cargo test -p chatter-mesh-android --test jni_signatures -- --show-output
```

In CI, make the skip impossible:

```bash
CHATTER_JNI_SIGNATURES_REQUIRED=1 cargo test -p chatter-mesh-android --test jni_signatures
```

Full APK (cargo-ndk + Gradle + optional install and adb reverse tunnels):

```bash
cd linux-client/android && ./build-android.sh [install]
```

### About the skip

With no compiled `MeshBleBridge.class` on disk there is nothing to inspect, so the
test skips instead of failing — a fresh clone that has never run Gradle should not
have a red test. The skip prints exactly what to run to make the check real. It is
still a skip: **a skipped signature check verifies nothing**, and until Gradle has
run, a mismatch will reach a device. Set `CHATTER_JNI_SIGNATURES_REQUIRED=1` to turn
every skip into a hard failure; CI must do this, or the check quietly becomes
decorative. The same applies when the compiled class is older than
`MeshBleBridge.kt` — the test refuses to certify a stale build.

Environment variables:

| Variable | Effect |
| --- | --- |
| `CHATTER_JNI_SIGNATURES_REQUIRED=1` | Every skip becomes a failure. Set this in CI. |
| `CHATTER_ANDROID_BUILD_DIR` | Where to look for compiled classes. Default `linux-client/android/app/build`. |
| `JAVA_HOME` | Used to find `javap` when it is not on `PATH`. |

## What is verifiable without hardware, and what is not

**Verifiable on any machine, no device:**

- Descriptor agreement between Rust and Kotlin (`tests/jni_signatures.rs`).
- Everything pure: the `LinkId` table, event marshalling, MTU arithmetic, error
  mapping.
- That the Kotlin compiles against the installed SDK, and that the Rust
  cross-compiles for `aarch64-linux-android`.

**Not verifiable without two physical phones:**

- Advertising, scanning, connecting, and any actual byte transfer.
- MTU negotiation, and therefore whether `max_write` is truthful.
- Anything about range, background behaviour, or multi-hop relaying.

> **Emulators have no Bluetooth radio.** An AVD can run the app, exercise the
> runtime-permission flow, and show that the foreground service starts and survives
> backgrounding — nothing more. `BluetoothAdapter` on an AVD either is absent or
> never produces a scan result. **Emulator success is not radio verification and
> must never be reported as such.** If a change touches advertising, scanning,
> GATT, or MTU, it is unverified until two real devices have talked to each other.

## Manual on-device checklist

Two physical Android phones, A and B, ideally on different OS versions — one
Android 11 or below and one Android 12 or above, so both permission eras are
covered. Build with `./build-android.sh install` on each.

1. **Load.** App starts and stays up. No `UnsatisfiedLinkError` or
   `NoSuchMethodError` in `adb logcat` — `RegisterNatives` runs at load, so a
   descriptor mismatch shows here, immediately, before any Bluetooth activity.
2. **Permissions.** On 12+: `BLUETOOTH_SCAN`, `BLUETOOTH_ADVERTISE`,
   `BLUETOOTH_CONNECT` are requested and granted. On 11 and below:
   `ACCESS_FINE_LOCATION` is granted — without it, scans return nothing and it
   looks exactly like "no peers nearby". Deny each one once: the app must report
   the radio unavailable, not crash.
3. **Advertise + discover.** With both apps open and roughly a metre apart, each
   phone discovers the other within ~10 s. Verify from both sides — advertising
   and scanning are separate code paths and one can work while the other does not.
4. **Connect means writable.** A peer reported `Connected` accepts a send
   immediately. If the first send after connect fails, the connected-state gate is
   firing too early (before service discovery and the CCCD write completed).
5. **MTU.** Send a payload larger than 20 bytes but under `max_write`; it must
   arrive whole. Then send exactly `max_write` bytes. Silent truncation means
   `max_write` is optimistic — it must be the negotiated ATT MTU minus 3, taken
   from `onMtuChanged`, not the requested value.
6. **Both roles.** Repeat 3–5 with the connection initiated from the other phone,
   so both central and peripheral paths are exercised.
7. **Background.** Lock phone A. Messages from B still arrive: the foreground
   service is holding the radio. Without it, Android stops the radio on
   backgrounding and a mesh node that only works while you stare at it is not a
   mesh node.
8. **Radio toggle.** Turn Bluetooth off on A: B sees a disconnect, A reports the
   adapter as off rather than hanging. Turn it back on: the link re-establishes
   without restarting the app.
9. **Range and recovery.** Walk out of range until the link drops, walk back, and
   confirm rediscovery and reconnection with no app restart.
10. **Soak.** Leave both phones connected for 30 minutes with periodic traffic. No
    growth in the event queue, no dropped-event warnings in logcat, no adapter
    wedge.

Steps 3–10 have no automated equivalent and no emulator equivalent. Record which
device models and OS versions were used when reporting a pass.
