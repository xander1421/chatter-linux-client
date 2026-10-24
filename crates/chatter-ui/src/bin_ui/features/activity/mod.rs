//! Activity Center model and navigation callbacks.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chatter_core::client::ChatterClient;
use slint::{ComponentHandle, ModelRc, VecModel};

use crate::{ActivityGlobal, ActivityItemData, MainWindow};

static REFRESH_REQUEST: AtomicU64 = AtomicU64::new(0);

fn refresh_status(window: &MainWindow, client: &ChatterClient) {
    let status = client.activity_status();
    let global = window.global::<ActivityGlobal>();
    global.set_push_status(status.push.into());
    global.set_last_connected(status.last_connected
        .map(|time| time.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "Never".into()).into());
    global.set_server_host(status.server_host.into());
    global.set_tor_bootstrap(status.tor_bootstrap_percent
        .map(|percent| format!("{percent}%"))
        .unwrap_or_default().into());
}

fn refresh(window: &MainWindow, client: &Arc<ChatterClient>) {
    let request = REFRESH_REQUEST.fetch_add(1, Ordering::Relaxed) + 1;
    let weak = window.as_weak();
    let client = client.clone();
    tokio::task::spawn_blocking(move || {
        let items = client.activity_items();
        let count = client.activity_unread_count().min(i32::MAX as u32) as i32;
        let _ = slint::invoke_from_event_loop(move || {
            if REFRESH_REQUEST.load(Ordering::Relaxed) != request { return; }
            let Some(window) = weak.upgrade() else { return };
            let rows = items.into_iter().map(|item| ActivityItemData {
                id: item.id.into(),
                kind: item.kind.into(),
                title: item.title.into(),
                preview: item.preview.into(),
                time_label: item.time_label.into(),
                section: item.section.into(),
                unread: item.unread,
            }).collect::<Vec<_>>();
            let global = window.global::<ActivityGlobal>();
            global.set_items(ModelRc::new(VecModel::from(rows)));
            global.set_unread_count(count);
            global.set_loading(false);
        });
    });
}

pub fn wire(window: &MainWindow, client: &Arc<ChatterClient>) {
    let mut updates = client.subscribe_activity();
    let mut auth = client.subscribe_auth_snapshots();
    client.start_activity_listener();
    refresh(window, client);
    refresh_status(window, client);

    let weak = window.as_weak();
    let client_for_status = client.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            interval.tick().await;
            let weak = weak.clone();
            let client = client_for_status.clone();
            if slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() { refresh_status(&window, &client); }
            }).is_err() { break; }
        }
    });

    let weak = window.as_weak();
    let client_for_updates = client.clone();
    tokio::spawn(async move {
        loop {
            let update = tokio::select! {
                update = updates.recv() => update.map(|_| ()),
                update = auth.recv() => update.map(|_| ()),
            };
            match update {
                Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let weak = weak.clone();
                    let client = client_for_updates.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(window) = weak.upgrade() { refresh(&window, &client); }
                    });
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let weak = window.as_weak();
    let client_for_open = client.clone();
    window.global::<ActivityGlobal>().on_open_item(move |id| {
        let weak = weak.clone();
        let client = client_for_open.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let target = client.activity_target(&id);
            client.activity_mark_read(&id);
            let _ = slint::invoke_from_event_loop(move || {
                let Some(window) = weak.upgrade() else { return };
                if let Some(target) = target {
                    let modal = window.global::<crate::ModalHostState>();
                    let sidebar = window.global::<crate::SidebarPane>();
                    match target.kind.as_str() {
                        "contact-request" => {
                            super::back::reset_to_chats(&window);
                            modal.set_friends_modal_tab("requests".into());
                            modal.set_friends_requests_sub_tab("received".into());
                            sidebar.set_active_tab("contacts".into());
                        }
                        "invite" => {
                            super::back::reset_to_chats(&window);
                            modal.set_friends_modal_tab("requests".into());
                            modal.set_friends_requests_sub_tab(if target.conversation_type.as_deref() == Some("anon_group") { "groups" } else { "channels" }.into());
                            sidebar.set_active_tab("contacts".into());
                        }
                        "mesh-invite" => {
                            super::back::reset_to_chats(&window);
                            sidebar.set_active_tab("mesh".into());
                        }
                        "missed-call" => {
                            super::back::reset_to_chats(&window);
                            sidebar.set_active_tab("calls".into());
                        }
                        "device-linked" => {
                            super::back::reset_to_chats(&window);
                            modal.invoke_nav_push("settings".into());
                            modal.invoke_nav_push("sessions".into());
                        }
                        "server" | "server-ok" => {}
                        _ => {
                            if let (Some(conversation_type), Some(conversation_id)) =
                                (target.conversation_type.as_deref(), target.conversation_id.as_deref())
                            {
                                if let Some(conversation) = client.call_history_conversation(conversation_type, conversation_id) {
                                    super::back::reset_to_chats(&window);
                                    sidebar.set_active_tab("chats".into());
                                    sidebar.set_mobile_show_chat(true);
                                    if let Some(message_id) = target.message_id {
                                        super::header::jump_to_conversation_message(&window, &client, conversation, &message_id);
                                    } else {
                                        super::sidebar::select_conversation(&window, conversation);
                                    }
                                }
                            }
                        }
                    }
                }
                refresh(&window, &client);
            });
        });
    });

    let weak = window.as_weak();
    let client_for_all = client.clone();
    window.global::<ActivityGlobal>().on_mark_all_read(move || {
        let weak = weak.clone();
        let client = client_for_all.clone();
        tokio::task::spawn_blocking(move || {
            client.activity_mark_all_read();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() { refresh(&window, &client); }
            });
        });
    });

    let weak = window.as_weak();
    let client_for_opened = client.clone();
    window.global::<ActivityGlobal>().on_opened(move || {
        if let Some(window) = weak.upgrade() { refresh(&window, &client_for_opened); }
    });
}
