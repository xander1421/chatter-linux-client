# Windows: native crash opening the SQLCipher DB

## Symptom

On the Windows desktop build, the app dies silently during startup, right
after the DB passphrase step. Last logs before exit:

```
INFO chatter_core::app::infrastructure: Verified: passphrase successfully stored and retrievable
```

Then the process exits — **no Rust panic line**, no error. With an existing
DB it *hangs*; with a fresh DB it *exits*. Either way it never reaches
`run_migrations` or "Infrastructure initialized".

`RUST_LOG=trace` adds nothing past that line — the failure is in C code below
Rust. The absence of a `panicked at ...` line means it is a **native crash
(access violation)**, not a Rust error. Confirm with the exit code: a value of
`-1073741819` (`0xC0000005`) is an access violation.

## Where

`crates/chatter-core/src/app/infrastructure.rs` → `Database::open()`
(`crates/chatter-core/src/storage/database.rs`). The crash is the first
SQLCipher crypto operation — `PRAGMA key` (PBKDF2 key derivation) — at
`database.rs:193`.

## Root cause

This binary links **two** crypto libraries:

- **SQLCipher** (rusqlite `bundled-sqlcipher`) — compiled against and expecting
  **vcpkg OpenSSL** (the CI installs `openssl:x64-windows-static` and sets
  `OPENSSL_DIR`).
- **webrtc-sys** (pulled by `livekit`, the `desktop` → `desktop-media` feature)
  — statically bundles **BoringSSL**.

Both export the same symbol names (`EVP_*`, `HMAC`, `PKCS5_PBKDF2_HMAC`,
`AES_*`, …). The MSVC link is forced through with `/FORCE:MULTIPLE`
(`.cargo/config.toml`, `[target.x86_64-pc-windows-msvc]`), which keeps the
**first** definition the linker sees and silently drops the duplicate.

If SQLCipher's calls resolve to **BoringSSL** instead of the OpenSSL it was
compiled against, the struct layouts / ABI differ → memory corruption → crash
on the first key-derivation call.

The `.cargo/config.toml` comment assumed the BoringSSL paths "only run inside
WebRTC DTLS/SRTP and don't share runtime state" — but SQLCipher uses those same
crypto symbols, so they *do* overlap. That assumption is wrong on Windows.

(Android already avoids this: `android/build-android.sh` points SQLCipher's
`OPENSSL_*` at webrtc's bundled BoringSSL so there is exactly **one** libcrypto.
Windows still uses a separate vcpkg OpenSSL → collision.)

## Fix options

### A. Drop LiveKit on the Windows build (fast, no calls)
Build without `livekit`/webrtc-sys → no BoringSSL → vcpkg OpenSSL is the only
libcrypto → no collision. The project already has the feature set for this:

- `desktop-gstreamer` and the `windows-cross` feature
  (`crates/chatter-ui/Cargo.toml`) are GStreamer-only, no LiveKit.
- CI change: build with a no-livekit feature set instead of default `desktop`.

GStreamer media still works; voice/video **calls are disabled on Windows**.

### B. Build SQLCipher against BoringSSL (keeps calls)
Mirror the Android approach on Windows: point rusqlite's `OPENSSL_DIR` /
`OPENSSL_LIB_DIR` / `OPENSSL_INCLUDE_DIR` at webrtc-sys's bundled BoringSSL
(headers + import lib) instead of vcpkg OpenSSL, so the whole binary has one
libcrypto (BoringSSL). SQLCipher 4.5.x is BoringSSL-compatible. Fiddly on
Windows (locating the prebuilt `libwebrtc` + BoringSSL headers under `target/`),
untested, expect several CI iterations.

## Debugging notes

- Repro is **build-only** — can't reproduce from Linux (`cargo` cfg's out the
  MSVC link). Each CI build is ~1.5h.
- To confirm A fixes it without a full CI loop: a local MSVC machine can
  `cargo run -p chatter-ui --no-default-features --features <no-livekit set>`
  and check the DB opens.
- DB path on Windows: `%APPDATA%\chatter\<id>\data\chatter.db` (+ `-wal`,
  `-shm`). Delete the whole `%APPDATA%\chatter` folder to retry from clean.
- `EnvFilter::from_default_env()` (`crates/chatter-ui/src/lib.rs`) → `RUST_LOG`
  controls verbosity; default floor is INFO.
