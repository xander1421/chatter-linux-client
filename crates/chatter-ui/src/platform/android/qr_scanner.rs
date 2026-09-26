//! Android in-app QR scanner.
//!
//! `MainActivity.scanQrCode` opens `QrScanner.kt` (CameraX preview + ZXing),
//! which reports once through `nativeOnQrScanned` with the decoded text, or
//! null on cancel / camera failure. This only returns the raw text: callers
//! feed it to the same core parser the paste field uses and show the result
//! for confirmation — nothing acts on a scan by itself.

use std::sync::Mutex;

use jni::objects::{JObject, JString};
use jni::JNIEnv;
use tokio::sync::oneshot;

use super::{activity, permissions};

/// Single in-flight scan. A newer scan drops the older sender, ending that
/// waiter as a cancel.
static PENDING: Mutex<Option<oneshot::Sender<Option<String>>>> = Mutex::new(None);

pub enum ScanOutcome {
    Scanned(String),
    /// Backed out, or the camera could not open (Kotlin shows a toast).
    Cancelled,
    CameraDenied,
}

/// Ask for CAMERA if needed, open the scanner, and wait for its answer.
pub async fn scan() -> ScanOutcome {
    if !permissions::ensure(&[permissions::CAMERA]).await {
        return ScanOutcome::CameraDenied;
    }
    let (tx, rx) = oneshot::channel();
    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    if let Err(e) = launch() {
        tracing::error!("QR scanner: failed to open: {e}");
        return ScanOutcome::Cancelled;
    }
    match rx.await {
        Ok(Some(text)) => ScanOutcome::Scanned(text),
        _ => ScanOutcome::Cancelled,
    }
}

fn launch() -> Result<(), String> {
    let mut env = activity::jni_env()?;
    let act = activity::activity()?;
    let res = env.call_method(act, "scanQrCode", "()V", &[]);
    if res.is_err() {
        activity::clear_exception(&mut env);
    }
    res.map_err(|e| e.to_string()).map(|_| ())
}

/// JNI callback from `QrScanner.nativeOnQrScanned(String?)`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_QrScanner_nativeOnQrScanned(
    mut env: JNIEnv,
    _this: JObject,
    text: JString,
) {
    let text = if text.is_null() {
        None
    } else {
        match env.get_string(&text) {
            Ok(s) => Some(String::from(s)),
            Err(e) => {
                tracing::error!("QR scanner: bad callback payload: {e}");
                None
            }
        }
    };
    if let Some(tx) = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = tx.send(text);
    }
}
