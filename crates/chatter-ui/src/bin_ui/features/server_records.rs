//! Thin presentation glue for core-owned server record pages.
use crate::{MainWindow, NotificationViewRow, ServerRecordRow, ServerRecordsGlobal, run_ui};
use chatter_core::client::ChatterClient;
use slint::{ComponentHandle, ModelRc, VecModel};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

pub fn wire(window: &MainWindow, client: &Arc<ChatterClient>) {
    // Each request supersedes previous results, including a close/reopen into
    // another channel. Weak handles are upgraded only on the event loop.
    let generation = Arc::new(AtomicU64::new(0));
    let loaded = Arc::new(Mutex::new(chatter_core::services::server_records::RecordPage::default()));
    let weak = window.as_weak();
    let loaded_on_open = loaded.clone();
    window
        .global::<ServerRecordsGlobal>()
        .on_open(move |kind, channel| {
            if let Some(w) = weak.upgrade() {
                *loaded_on_open.lock().unwrap() = Default::default();
                let g = w.global::<ServerRecordsGlobal>();
                g.set_kind(kind);
                g.set_channel(channel);
                g.set_rows(ModelRc::default());
                g.set_notifications(ModelRc::default());
                g.set_detail_title("".into());
                g.set_detail_body("".into());
                g.set_detail_id("".into());
                g.set_next("".into());
                g.set_loading(false);
                g.invoke_load(false);
            }
        });
    let weak = window.as_weak();
    let ctx = client.clone();
    window.global::<ServerRecordsGlobal>().on_details(move |id| {
        if let Some(w) = weak.upgrade() {
            let g = w.global::<ServerRecordsGlobal>();
            let kind = g.get_kind().to_string();
            let channel = g.get_channel().to_string();
            g.set_detail_id(id.clone());
            let id = id.to_string();
            let ctx = ctx.clone();
            let weak = w.as_weak();
            g.set_detail_title("Loading details…".into());
            g.set_detail_body("".into());
            tokio::spawn(async move {
                let result = ctx.server_record_detail(&kind, &id).await;
                run_ui!(weak, |w| {
                    let g = w.global::<ServerRecordsGlobal>();
                    if g.get_kind().as_str() != kind.as_str()
                        || g.get_channel().as_str() != channel.as_str()
                        || g.get_detail_id().as_str() != id.as_str() {
                        return;
                    }
                    match result {
                        Ok(detail) => {
                            g.set_detail_title(detail.title.into());
                            let body = if detail.created_at_ns > 0 {
                                format!("{}\n{}", detail.body, crate::bin_ui::converters::server_record_time(detail.created_at_ns))
                            } else { detail.body };
                            g.set_detail_body(body.into());
                        }
                        Err(error) => {
                            g.set_detail_title("Details unavailable".into());
                            g.set_detail_body(error.to_string().into());
                        }
                    }
                });
            });
        }
    });
    let weak = window.as_weak();
    let ctx = client.clone();
    let seq = generation.clone();
    let loaded_for_load = loaded.clone();
    window.global::<ServerRecordsGlobal>().on_load(move |more| {
        if let Some(w) = weak.upgrade() {
            request(&w, &ctx, &seq, &loaded_for_load, more, None);
        }
    });
    let weak = window.as_weak();
    let ctx = client.clone();
    let loaded_for_act = loaded.clone();
    window
        .global::<ServerRecordsGlobal>()
        .on_act(move |id, action| {
            if let Some(w) = weak.upgrade() {
                if !w.global::<ServerRecordsGlobal>().get_loading() {
                    request(
                        &w,
                        &ctx,
                        &generation,
                        &loaded_for_act,
                        false,
                        Some((id.to_string(), action.to_string())),
                    );
                }
            }
        });
}

fn request(
    w: &MainWindow,
    client: &Arc<ChatterClient>,
    generation: &Arc<AtomicU64>,
    loaded: &Arc<Mutex<chatter_core::services::server_records::RecordPage>>,
    more: bool,
    action: Option<(String, String)>,
) {
    let g = w.global::<ServerRecordsGlobal>();
    let kind = g.get_kind().to_string();
    let channel = g.get_channel().to_string();
    let cursor = if more {
        g.get_next().to_string()
    } else {
        String::new()
    };
    g.set_loading(true);
    g.set_error("".into());
    let token = generation.fetch_add(1, Ordering::SeqCst) + 1;
    let generation = generation.clone();
    let client = client.clone();
    let loaded = loaded.clone();
    let weak = w.as_weak();
    tokio::spawn(async move {
        let result = async {
            if let Some((id, action)) = action {
                client
                    .server_records_action(&kind, &channel, &id, &action)
                    .await?;
            }
            client.server_records_page(&kind, &channel, &cursor).await
        }
        .await;
        run_ui!(weak, |w| {
            if generation.load(Ordering::SeqCst) != token {
                return;
            }
            let g = w.global::<ServerRecordsGlobal>();
            g.set_loading(false);
            match result {
                Ok(next) => {
                    let page = if more {
                        chatter_core::services::server_records::merge_pages(
                            loaded.lock().unwrap().clone(), next)
                    } else { next };
                    *loaded.lock().unwrap() = page.clone();
                    let notifications = page.notifications.into_iter().map(|r| NotificationViewRow {
                        id: r.id.into(), title: r.title.into(), body: r.body.into(),
                        kind: r.kind.into(), actor_id: r.actor_id.into(),
                        channel_id: r.channel_id.into(), when: r.when.into(),
                        avatar: r.avatar.as_deref().and_then(crate::bin_ui::converters::avatar_bytes_to_image).unwrap_or_default(),
                        read: r.read, status: r.status.into(),
                    }).collect::<Vec<_>>();
                    g.set_notifications(ModelRc::new(VecModel::from(notifications)));
                    let rows = page.rows.into_iter().map(|r| ServerRecordRow {
                        id: r.id.into(), title: r.title.into(),
                        body: if r.created_at_ns > 0 { format!("{} · {}", r.body, crate::bin_ui::converters::server_record_time(r.created_at_ns)).into() } else { r.body.into() },
                        primary: r.primary.into(), secondary: r.secondary.into(),
                    }).collect::<Vec<_>>();
                    g.set_rows(ModelRc::new(VecModel::from(rows)));
                    g.set_next(page.next.into());
                }
                Err(error) => g.set_error(error.to_string().into()),
            }
        });
    });
}
