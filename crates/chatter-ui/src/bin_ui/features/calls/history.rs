//! Calls screen (mobile bottom nav "Calls" tab) — UI glue.
//!
//! Core owns the rows: `ChatterClient::call_history_page` reads and shapes
//! them on a blocking worker, and `call_history_conversation` turns a row's
//! `(id, type)` pair back into an `ActiveConversation`. This module only
//! copies the finished rows into the Slint model and routes clicks at the
//! same conversation-open callbacks the sidebar's section rows use — no
//! storage access ever happens on the event loop.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::call::CallHistoryFilter;
use chatter_core::state::ActiveConversation;

use crate::run_ui;
use crate::{CallHistoryRow, MainWindow};

/// One screenful and then some. Pagination exists in the repository but the
/// screen has no "load more" affordance yet, so a fixed page is the whole
/// list as far as the UI is concerned.
// ponytail: fixed page, wire the repo's cursor when someone has 200 calls.
const HISTORY_PAGE: u32 = 200;

/// Wire the Calls tab and everything on the screen it opens.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_tab(window, ui_ctx);
    wire_filter(window, ui_ctx);
    wire_open(window, ui_ctx);
    wire_redial(window, ui_ctx);
    wire_clear(window, ui_ctx);
    // Paint the tab badge at startup — missed calls from a previous session
    // are exactly the thing the badge exists to surface.
    refresh_badge(window, ui_ctx);
}

/// Load the list behind the tab. The screen itself is raised by app.slint;
/// this fills it.
fn wire_tab(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_calls_tab_clicked(move || {
        let Some(w) = weak.upgrade() else { return };
        w.set_call_history_filter("all".into());
        reload(&w, &ui_ctx);
    });
}

fn wire_filter(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_call_history_filter_changed(move |_filter| {
        // The filter property is already committed by the Slint handler;
        // `reload` reads it back rather than taking it twice.
        let Some(w) = weak.upgrade() else { return };
        reload(&w, &ui_ctx);
    });
}

/// Tapping a row opens the conversation the call happened in.
fn wire_open(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_call_history_open(move |id: SharedString, kind: SharedString| {
        let Some(w) = weak.upgrade() else { return };
        open_conversation(&w, &ui_ctx, &id, &kind);
    });
}

/// The redial buttons: open the conversation, then place the call through
/// the same path the conversation header's call buttons use.
fn wire_redial(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_call_history_redial(
        move |id: SharedString, kind: SharedString, is_video: bool| {
            let Some(w) = weak.upgrade() else { return };
            // Opening first is what gives the call view its title and the
            // header its context — and it commits `ActiveConversation`,
            // which is what `start_*_call` dispatches on.
            if !open_conversation(&w, &ui_ctx, &id, &kind) {
                return;
            }
            if is_video {
                w.invoke_start_video_call();
            } else {
                w.invoke_start_voice_call();
            }
        },
    );
}

fn wire_clear(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_call_history_clear(move || {
        let weak = weak.clone();
        let ui_ctx = ui_ctx.clone();
        tokio::spawn(async move {
            if let Err(e) = ui_ctx.clear_call_history().await {
                tracing::error!(error = %e, "Failed to clear call history");
                return;
            }
            let page = load_page(&ui_ctx, CallHistoryFilter::All).await;
            run_ui!(weak, |w| paint(&w, page));
        });
    });
}

/// Route a history row's `(id, type)` pair at the conversation-open callback
/// for its kind. Returns false when the row no longer resolves — a stale
/// conversation should not silently look like a call that failed to place.
fn open_conversation(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    conversation_id: &str,
    conversation_type: &str,
) -> bool {
    match ui_ctx.call_history_conversation(conversation_type, conversation_id) {
        Some(ActiveConversation::Channel(id)) => {
            window.invoke_channel_selected(id.to_string().into());
            true
        }
        Some(ActiveConversation::ServerDm { peer_id }) => {
            window.invoke_dm_selected(peer_id.to_string().into());
            true
        }
        Some(ActiveConversation::AnonDm(id)) => {
            window.invoke_dm_selected(id.to_string().into());
            true
        }
        Some(ActiveConversation::AnonGroup(id)) => {
            window.invoke_private_group_selected(id.to_string().into());
            true
        }
        // `MailboxDm` and `None` never come back out of a history row —
        // `from_routing_key` rejects both.
        _ => {
            tracing::warn!(
                conversation_type,
                conversation_id,
                "Call history row no longer names a reachable conversation"
            );
            window.set_toast_message("That conversation is no longer available.".into());
            window.set_toast_variant("error".into());
            window.set_toast_visible(true);
            false
        }
    }
}

/// Re-read the list from core and repaint it, badge included.
///
/// The read itself runs on core's blocking pool; only the model swap happens
/// on the event loop.
pub(crate) fn reload(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let filter = match window.get_call_history_filter().as_str() {
        "missed" => CallHistoryFilter::Missed,
        _ => CallHistoryFilter::All,
    };
    window.set_call_history_loading(true);

    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    tokio::spawn(async move {
        let page = load_page(&ui_ctx, filter).await;
        run_ui!(weak, |w| paint(&w, page));
    });
}

/// Missed-call count on the bottom nav's Calls tab.
pub(crate) fn refresh_badge(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    tokio::spawn(async move {
        let missed = ui_ctx.missed_call_count().await as i32;
        run_ui!(weak, |w| w.set_missed_call_count(missed));
    });
}

/// Translate core's page into Slint rows. Off the event loop — the `From`
/// conversions and the `Vec` build are done before we hop back.
async fn load_page(
    ui_ctx: &Arc<ChatterClient>,
    filter: CallHistoryFilter,
) -> (Vec<CallHistoryRow>, i32) {
    let page = match ui_ctx.call_history_page(filter, HISTORY_PAGE).await {
        Ok(page) => page,
        Err(e) => {
            tracing::error!(error = %e, "Failed to load call history");
            return (Vec::new(), 0);
        }
    };
    let rows = page
        .rows
        .into_iter()
        .map(|r| CallHistoryRow {
            id: r.id.into(),
            conversation_id: r.conversation_id.into(),
            conversation_type: r.conversation_type.into(),
            name: r.name.into(),
            initials: r.initials.into(),
            timestamp: r.timestamp.into(),
            detail: r.detail.into(),
            is_missed: r.is_missed,
            is_video: r.is_video,
            is_incoming: r.is_incoming,
            can_call_back: r.can_call_back,
        })
        .collect();
    (rows, page.missed_count as i32)
}

fn paint(window: &MainWindow, (rows, missed): (Vec<CallHistoryRow>, i32)) {
    window.set_call_history_rows(ModelRc::new(VecModel::from(rows)));
    window.set_call_history_loading(false);
    window.set_missed_call_count(missed);
}
