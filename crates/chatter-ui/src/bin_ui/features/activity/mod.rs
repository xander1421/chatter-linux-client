//! Activity Center model and navigation callbacks.

use std::sync::Arc;

use chatter_core::client::ChatterClient;
use slint::{ComponentHandle, ModelRc, VecModel};

use crate::{ActivityGlobal, ActivityItemData, MainWindow};

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

fn refresh(window: &MainWindow, client: &ChatterClient) {
    let rows = client.activity_items().into_iter().map(|item| ActivityItemData {
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
    global.set_unread_count(client.activity_unread_count().min(i32::MAX as u32) as i32);
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
        let Some(window) = weak.upgrade() else { return };
        if let Some(target) = client_for_open.activity_target(id.as_str()) {
            if let Some(conversation) = client_for_open.call_history_conversation(&target.conversation_type, &target.conversation_id) {
                if let Some(message_id) = target.message_id {
                    super::header::jump_to_conversation_message(&window, &client_for_open, conversation, &message_id);
                } else {
                    super::sidebar::select_conversation(&window, conversation);
                }
            }
        }
        client_for_open.activity_mark_read(id.as_str());
        refresh(&window, &client_for_open);
    });

    let weak = window.as_weak();
    let client_for_all = client.clone();
    window.global::<ActivityGlobal>().on_mark_all_read(move || {
        client_for_all.activity_mark_all_read();
        if let Some(window) = weak.upgrade() { refresh(&window, &client_for_all); }
    });

    let weak = window.as_weak();
    let client_for_opened = client.clone();
    window.global::<ActivityGlobal>().on_opened(move || {
        if let Some(window) = weak.upgrade() { refresh(&window, &client_for_opened); }
    });
}
