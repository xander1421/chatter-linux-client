//! Storage Access Framework bridge for encrypted message backup files.

use std::path::PathBuf;
use std::sync::Mutex;

use jni::objects::{JObject, JString, JValue};
use tokio::sync::oneshot;

use super::activity;

static CREATE_PENDING: Mutex<Option<oneshot::Sender<bool>>> = Mutex::new(None);
static OPEN_PENDING: Mutex<Option<oneshot::Sender<Option<PathBuf>>>> = Mutex::new(None);

fn call_picker(method: &str) -> Result<(), String> {
    activity::with_jni_env(|env| {
        let act = activity::activity()?;
        let result = env.call_method(
            act,
            jni::strings::JNIString::from(method),
            jni::jni_sig!("()V"),
            &[],
        );
        if result.is_err() {
            activity::clear_exception(env);
        }
        result.map_err(|e| e.to_string())?;
        Ok(())
    })
}

pub async fn create() -> Result<bool, String> {
    let (tx, rx) = oneshot::channel();
    *CREATE_PENDING.lock().unwrap() = Some(tx);
    if let Err(e) = call_picker("launchBackupCreate") {
        CREATE_PENDING.lock().unwrap().take();
        return Err(e);
    }
    Ok(rx.await.unwrap_or(false))
}

pub async fn open() -> Result<Option<PathBuf>, String> {
    let (tx, rx) = oneshot::channel();
    *OPEN_PENDING.lock().unwrap() = Some(tx);
    if let Err(e) = call_picker("launchBackupOpen") {
        OPEN_PENDING.lock().unwrap().take();
        return Err(e);
    }
    Ok(rx.await.unwrap_or(None))
}

pub fn write_document(path: &std::path::Path) -> Result<(), String> {
    activity::with_jni_env(|env| {
        let act = activity::activity()?;
        let path = env
            .new_string(path.to_string_lossy().as_ref())
            .map_err(|e| e.to_string())?;
        let result = env.call_method(
            act,
            jni::jni_str!("writeBackupDocument"),
            jni::jni_sig!("(Ljava/lang/String;)Z"),
            &[JValue::Object(&path)],
        );
        let success = result.and_then(|v| v.z()).unwrap_or(false);
        activity::clear_exception(env);
        if success { Ok(()) } else { Err("Could not write selected document".into()) }
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeOnBackupDestination(
    mut env: jni::EnvUnowned<'_>,
    _this: JObject,
    selected: jni::sys::jboolean,
) {
    env.with_env(|_| -> jni::errors::Result<()> {
        if let Some(tx) = CREATE_PENDING.lock().unwrap().take() {
            let _ = tx.send(selected);
        }
        Ok(())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeOnBackupDocument(
    mut env: jni::EnvUnowned<'_>,
    _this: JObject,
    path: JString,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let path: String = path.try_to_string(&env)?.into();
        if let Some(tx) = OPEN_PENDING.lock().unwrap().take() {
            let _ = tx.send((!path.is_empty()).then(|| PathBuf::from(path)));
        }
        Ok(())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}
