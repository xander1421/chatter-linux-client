//! iOS staticlib entry point.
//!
//! Two modes:
//!   * default — a smoke harness that exercises the load-bearing parts of
//!     chatter-core (HPKE/X25519, SQLCipher, tokio) and hands a report string
//!     back to the ObjC shell to render. Proves the core actually runs on the
//!     device.
//!   * `ui` feature — hands control to chatter-ui's Slint event loop.
//!
//! ponytail: the smoke harness exists only because the Slint UI's iOS port is
//! unfinished. Delete this file's `smoke` half once `--features ui` links.

use std::ffi::{CString, c_char};
use std::path::PathBuf;

/// Report line: `ok`/`FAIL` plus what was checked.
fn check(name: &str, r: Result<String, String>) -> String {
    match r {
        Ok(detail) => format!("[ok]   {name} — {detail}"),
        Err(e) => format!("[FAIL] {name} — {e}"),
    }
}

/// HPKE round-trip through chatter_core::crypto::ecies.
fn check_crypto() -> Result<String, String> {
    use chatter_core::crypto::ecies;
    let (sk, pk) = ecies::generate_x25519_keypair();
    let msg = b"chatter on iphone";
    let sealed = ecies::encrypt(&pk, msg).map_err(|e| format!("{e:?}"))?;
    let opened = ecies::decrypt(&sk, &sealed).map_err(|e| format!("{e:?}"))?;
    if opened != msg {
        return Err("round-trip mismatch".into());
    }
    Ok(format!("X25519 + HPKE round-trip, {} byte ct", sealed.ciphertext.len()))
}

/// SQLCipher open + write + read inside the app sandbox.
fn check_storage(dir: &PathBuf) -> Result<String, String> {
    use chatter_core::storage::database::Database;
    let path = dir.join("smoke.db");
    let _ = std::fs::remove_file(&path);
    let db = Database::open(&path, "smoke-passphrase").map_err(|e| format!("{e:?}"))?;
    let conn = db.open_reader_connection().map_err(|e| format!("{e:?}"))?;
    let v: String = conn
        .query_row("SELECT sqlite_version()", [], |r| r.get(0))
        .map_err(|e| format!("{e:?}"))?;
    Ok(format!("SQLCipher opened at {}, sqlite {v}", path.display()))
}

/// Keychain via keyring/apple-native — the one dep iOS resolves differently.
/// Goes through chatter_core::system::Keyring so we're exercising the real
/// backend selection, not a copy of it.
fn check_keychain(dir: &PathBuf) -> Result<String, String> {
    use chatter_core::system::Keyring;
    let k = Keyring::new(dir.join("smoke.db"));
    k.set_passphrase("hello-keychain").map_err(|e| format!("{e:?}"))?;
    let got = k.get_passphrase().map_err(|e| format!("{e:?}"))?;
    let _ = k.delete_passphrase();
    if got != "hello-keychain" {
        return Err(format!("readback mismatch: {got:?}"));
    }
    Ok("Keychain set/get/delete".into())
}

/// Tokio multi-thread runtime actually schedules on device.
fn check_tokio() -> Result<String, String> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    let n = rt.block_on(async {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        21 * 2
    });
    Ok(format!("multi-thread runtime awaited a timer, got {n}"))
}

/// Run the smoke checks. `docs_dir` is the app's Documents directory, passed in
/// from ObjC (Rust can't get at NSSearchPathForDirectoriesInDomains cheaply).
///
/// Returns a newly allocated C string the caller must free with
/// `chatter_ios_string_free`.
///
/// # Safety
/// `docs_dir` must be a valid NUL-terminated UTF-8 C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn chatter_ios_smoke(docs_dir: *const c_char) -> *mut c_char {
    let dir = if docs_dir.is_null() {
        std::env::temp_dir()
    } else {
        PathBuf::from(
            unsafe { std::ffi::CStr::from_ptr(docs_dir) }
                .to_string_lossy()
                .into_owned(),
        )
    };

    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .try_init();

    let lines = [
        format!("chatter-core on {} / {}", std::env::consts::OS, std::env::consts::ARCH),
        String::new(),
        check("crypto", check_crypto()),
        check("storage", check_storage(&dir)),
        check("keychain", check_keychain(&dir)),
        check("tokio", check_tokio()),
    ];
    let report = lines.join("\n");
    tracing::info!("{report}");
    CString::new(report).unwrap_or_default().into_raw()
}

/// Free a string returned by this library.
///
/// # Safety
/// `s` must be a pointer previously returned by `chatter_ios_smoke` and not
/// already freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn chatter_ios_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}

/// Hand control to the Slint UI. Never returns (winit owns UIApplicationMain).
#[cfg(feature = "ui")]
#[unsafe(no_mangle)]
pub extern "C" fn chatter_ios_run_ui() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(chatter_ui::app_main());
}
