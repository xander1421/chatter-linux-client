//! Route an opaque Android notification tap after the UI and account are ready.

use slint::ComponentHandle;
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
        || !window.global::<crate::AuthFlow>().get_is_signed_in()
        || window.get_current_view().as_str() != "main"
    {
        return;
    }
    let key = PENDING_KEY.lock().ok().and_then(|pending| pending.clone());
    if let Some(ref key) = key {
        if client.notification_tap_belongs_to_current_account(key) == Some(false) {
            if let Ok(mut pending) = PENDING_KEY.lock() {
                if pending.as_ref() == Some(key) {
                    *pending = None;
                }
            }
            return;
        }
    }
    let Some(target) = key
        .as_deref()
        .and_then(|key| client.notification_target_for_key(key))
    else {
        // A cold-start inbox arrival may not be in the sidebar yet. The next
        // list refresh retries this key after the request has been persisted.
        return;
    };
    if let Ok(mut pending) = PENDING_KEY.lock() {
        if *pending == key {
            *pending = None;
        }
    }
    crate::bin_ui::features::back::reset_to_chats(window);
    match target {
        chatter_core::client::NotificationTarget::Conversation(conversation) => {
            window
                .global::<crate::SidebarPane>()
                .set_active_tab("chats".into());
            window
                .global::<crate::SidebarPane>()
                .set_mobile_show_chat(true);
            // A message notification is a request for the newest message.
            // The ordinary sidebar open instead starts at the first unread
            // row, which can be much older than the notification itself.
            crate::bin_ui::features::sidebar::dm::open_from_notification(window, conversation);
        }
        chatter_core::client::NotificationTarget::SecretRequest => {
            window
                .global::<crate::SidebarPane>()
                .set_mobile_show_chat(false);
            crate::bin_ui::features::sidebar::navigation::load_pending_message_requests_count(
                window, &client,
            );
            window
                .global::<crate::ModalHostState>()
                .set_friends_modal_tab("requests".into());
            window
                .global::<crate::ModalHostState>()
                .set_friends_requests_sub_tab("private".into());
            window
                .global::<crate::SidebarPane>()
                .set_active_tab("contacts".into());
        }
    }
}
