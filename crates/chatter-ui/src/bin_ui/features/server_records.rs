//! Thin presentation glue for core-owned server record pages.
use crate::{MainWindow, ServerRecordRow, ServerRecordsGlobal, run_ui};
use chatter_core::client::ChatterClient;
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

pub fn wire(window: &MainWindow, client: &Arc<ChatterClient>) {
    // Each request supersedes previous results, including a close/reopen into
    // another channel. Weak handles are upgraded only on the event loop.
    let generation = Arc::new(AtomicU64::new(0));
    let weak = window.as_weak();
    window
        .global::<ServerRecordsGlobal>()
        .on_open(move |kind, channel| {
            if let Some(w) = weak.upgrade() {
                let g = w.global::<ServerRecordsGlobal>();
                g.set_kind(kind);
                g.set_channel(channel);
                g.set_rows(ModelRc::default());
                g.set_next("".into());
                g.set_loading(false);
                g.invoke_load(false);
            }
        });
    let weak = window.as_weak();
    let ctx = client.clone();
    let seq = generation.clone();
    window.global::<ServerRecordsGlobal>().on_load(move |more| {
        if let Some(w) = weak.upgrade() {
            request(&w, &ctx, &seq, more, None);
        }
    });
    let weak = window.as_weak();
    let ctx = client.clone();
    window
        .global::<ServerRecordsGlobal>()
        .on_act(move |id, action| {
            if let Some(w) = weak.upgrade() {
                if !w.global::<ServerRecordsGlobal>().get_loading() {
                    request(
                        &w,
                        &ctx,
                        &generation,
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
                Ok(page) => {
                    let mut rows: Vec<ServerRecordRow> = if more {
                        g.get_rows().iter().collect()
                    } else {
                        vec![]
                    };
                    rows.extend(page.rows.into_iter().map(|r| ServerRecordRow {
                        id: r.id.into(),
                        title: r.title.into(),
                        body: r.body.into(),
                        primary: r.primary.into(),
                        secondary: r.secondary.into(),
                    }));
                    g.set_rows(ModelRc::new(VecModel::from(rows)));
                    g.set_next(page.next.into());
                }
                Err(error) => g.set_error(error.to_string().into()),
            }
        });
    });
}
