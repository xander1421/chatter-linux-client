//! Route an opaque Android notification tap after the UI and account are ready.

use std::sync::{Arc, Mutex};

use chatter_core::client::ChatterClient;
use jni::objects::{JClass, JString};

static CLIENT: Mutex<Option<Arc<ChatterClient>>> = Mutex::new(None);
static WINDOW: Mutex<Option<slint::Weak<crate::MainWindow>>> = Mutex::new(None);
static PENDING_KEY: Mutex<Option<String>> = Mutex::new(None);

pub fn register_client(client: Arc<ChatterClient>) {
    if let Ok(mut current) = CLIENT.lock() {
        *current = Some(client);
    }
}

pub fn register_window(window: slint::Weak<crate::MainWindow>) {
    if let Ok(mut current) = WINDOW.lock() {
        *current = Some(window);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeNotificationOpened(
    mut env: jni::EnvUnowned<'_>,
    _class: JClass,
    key: JString,
) {
    env.with_env(|env| -> jni::errors::Result<_> {
        let Ok(key) = key.try_to_string(&env).map(String::from) else {
            return Ok(());
        };
        if key.is_empty() {
            if let Ok(mut pending) = PENDING_KEY.lock() {
                *pending = None;
            }
            return Ok(());
        }
        if !chatter_core::services::notification_service::is_opaque_notification_key(&key) {
            return Ok(());
        }
        if let Ok(mut pending) = PENDING_KEY.lock() {
            *pending = Some(key);
        }
        if let Some(window) = WINDOW.lock().ok().and_then(|current| current.clone()) {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = window.upgrade() {
                    open_pending(&window);
                }
            });
        }
        Ok(())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}

/// Called after the chat list has been published during startup or sign-in.
pub fn open_pending(window: &crate::MainWindow) {
    let Some(client) = CLIENT.lock().ok().and_then(|current| current.clone()) else {
        return;
    };
    if !client.is_logged_in()
        || !window.get_is_signed_in()
        || window.get_current_view().as_str() != "main"
    {
        return;
    }
    let key = PENDING_KEY
        .lock()
        .ok()
        .and_then(|mut pending| pending.take());
    if let Some(conversation) = key.and_then(|key| client.conversation_for_notification_key(&key)) {
        crate::bin_ui::features::sidebar::select_conversation(window, conversation);
    }
}
