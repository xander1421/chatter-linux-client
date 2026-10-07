//! Calls screen (mobile bottom nav "Calls" tab) — UI glue.
//!
//! Core owns the rows: `ChatterClient::call_history_page` reads and shapes
//! them on a blocking worker, and `call_history_conversation` turns a row's
//! `(id, type)` pair back into an `ActiveConversation`. This module only
//! copies the finished rows into the Slint model and routes clicks at the
//! same conversation-open callbacks the sidebar's section rows use — no
//! storage access ever happens on the event loop.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
static HISTORY_GENERATION: AtomicU64 = AtomicU64::new(0);
static HISTORY_MODEL_SESSION: AtomicU64 = AtomicU64::new(u64::MAX);

#[derive(Clone, Copy)]
struct HistoryRequest {
    generation: u64,
    session: u64,
}
impl HistoryRequest {
    fn begin(client: &ChatterClient) -> Self {
        Self {
            generation: HISTORY_GENERATION.fetch_add(1, Ordering::SeqCst) + 1,
            session: client.auth_state().get().session_revision,
        }
    }
    fn matches(self, generation: u64, session: u64) -> bool {
        self.generation == generation && self.session == session
    }
}

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::client::calls::CallHistoryPage;
use chatter_core::domain::call::CallHistoryFilter;

use crate::bin_ui::converters::avatar_bytes_to_image;
use crate::run_ui;
use crate::{CallHistoryRow, MainWindow};

/// One screenful and then some. Pagination exists in the repository but the
/// screen has no "load more" affordance yet, so a fixed page is the whole
/// list as far as the UI is concerned.
// fixed page, wire the repo's cursor when someone has 200 calls.
const HISTORY_PAGE: u32 = 200;

/// Wire the Calls tab and everything on the screen it opens.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_tab(window, ui_ctx);
    wire_filter(window, ui_ctx);
    wire_open(window, ui_ctx);
    wire_redial(window, ui_ctx);
    wire_clear(window, ui_ctx);
    let weak = window.as_weak();
    let client = ui_ctx.clone();
    window.on_call_history_retry(move || {
        if let Some(w) = weak.upgrade() {
            reload(&w, &client);
        }
    });
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
        let request = HistoryRequest::begin(&ui_ctx);
        tokio::spawn(async move {
            let result = ui_ctx.clear_call_history().await;
            let client = ui_ctx.clone();
            run_ui!(weak, |w| {
                complete_clear(
                    &w,
                    request.session,
                    client.auth_state().get().session_revision,
                    result,
                    |w| reload(w, &client),
                );
            });
        });
    });
}

fn complete_clear(
    window: &MainWindow,
    requested_session: u64,
    current_session: u64,
    result: Result<u64, chatter_core::client::ClientError>,
    reload: impl FnOnce(&MainWindow),
) {
    if requested_session != current_session {
        return;
    }
    match result {
        Ok(_) => reload(window),
        Err(e) => {
            tracing::error!(error = %e, "Failed to clear call history");
            window.set_call_history_loading(false);
            window.set_toast_message("Could not clear call history. Try again.".into());
            window.set_toast_variant("error".into());
            window.set_toast_visible(true);
        }
    }
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
    // `None` never comes back out of a history row — `from_routing_key`
    // rejects it.
    let opened = ui_ctx
        .call_history_conversation(conversation_type, conversation_id)
        .is_some_and(|conv| crate::bin_ui::features::sidebar::select_conversation(window, conv));
    if !opened {
        tracing::warn!(
            conversation_type,
            conversation_id,
            "Call history row no longer names a reachable conversation"
        );
        window.set_toast_message("That conversation is no longer available.".into());
        window.set_toast_variant("error".into());
        window.set_toast_visible(true);
    }
    opened
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
    let request = HistoryRequest::begin(ui_ctx);
    if HISTORY_MODEL_SESSION.swap(request.session, Ordering::SeqCst) != request.session {
        // Successful data belongs to its auth session, never to a new login.
        window.set_call_history_rows(ModelRc::new(VecModel::<CallHistoryRow>::default()));
        window.set_missed_call_count(0);
    }
    window.set_call_history_loading(true);
    window.set_call_history_error("".into());

    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    tokio::spawn(async move {
        let page = load_page(&ui_ctx, filter).await;
        let client = ui_ctx.clone();
        run_ui!(weak, |w| {
            complete_load(
                &w,
                request,
                HISTORY_GENERATION.load(Ordering::SeqCst),
                client.auth_state().get().session_revision,
                page,
            );
        });
    });
}

/// Missed-call count on the bottom nav's Calls tab.
pub(crate) fn refresh_badge(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    let generation = HISTORY_GENERATION.load(Ordering::SeqCst);
    let session = ui_ctx.auth_state().get().session_revision;
    tokio::spawn(async move {
        let missed = ui_ctx.try_missed_call_count().await;
        let client = ui_ctx.clone();
        run_ui!(weak, |w| {
            if generation == HISTORY_GENERATION.load(Ordering::SeqCst)
                && session == client.auth_state().get().session_revision
            {
                match missed {
                    Ok(count) => w.set_missed_call_count(count as i32),
                    Err(error) => tracing::error!(%error, "Failed to refresh missed-call badge"),
                }
            }
        });
    });
}

/// Load core's page off the event loop; `paint` turns it into Slint rows
/// there, since a `slint::Image` (the avatar) cannot cross threads.
async fn load_page(
    ui_ctx: &Arc<ChatterClient>,
    filter: CallHistoryFilter,
) -> Result<CallHistoryPage, chatter_core::client::ClientError> {
    ui_ctx.call_history_page(filter, HISTORY_PAGE).await
}

fn complete_load(
    window: &MainWindow,
    request: HistoryRequest,
    generation: u64,
    session: u64,
    page: Result<CallHistoryPage, chatter_core::client::ClientError>,
) {
    if request.matches(generation, session) {
        apply_page(window, page);
    }
}

fn apply_page(
    window: &MainWindow,
    page: Result<CallHistoryPage, chatter_core::client::ClientError>,
) {
    match page {
        Ok(page) => {
            window.set_call_history_error("".into());
            paint(window, page);
        }
        Err(error) => {
            tracing::error!(%error, "Failed to load call history");
            window.set_call_history_loading(false);
            window.set_call_history_error("Load failed; showing previous calls.".into());
        }
    }
}

fn paint(window: &MainWindow, page: CallHistoryPage) {
    let rows: Vec<CallHistoryRow> = page
        .rows
        .into_iter()
        .map(|r| CallHistoryRow {
            avatar_source: r
                .avatar_bytes
                .as_deref()
                .and_then(avatar_bytes_to_image)
                .unwrap_or_default(),
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
    window.set_call_history_rows(ModelRc::new(VecModel::from(rows)));
    window.set_call_history_loading(false);
    window.set_missed_call_count(page.missed_count as i32);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delayed_filter_and_session_results_are_rejected() {
        let all = HistoryRequest {
            generation: 10,
            session: 2,
        };
        let missed = HistoryRequest {
            generation: 11,
            session: 2,
        };
        assert!(!all.matches(11, 2));
        assert!(missed.matches(11, 2));
        assert!(
            !missed.matches(11, 3),
            "same account re-login must invalidate old loads"
        );
        assert!(!missed.matches(12, 2), "clear invalidates pending loads");
    }
    #[test]
    fn read_failure_preserves_rows_and_unread_then_retry_replaces_them() {
        const MARKER: &str = "CHATTER_CALL_HISTORY_FIXTURE_CHILD";
        let name = format!(
            "{}::read_failure_preserves_rows_and_unread_then_retry_replaces_them",
            module_path!().split_once("::").unwrap().1
        );
        if std::env::var(MARKER).ok().as_deref() != Some(name.as_str()) {
            use std::io::{Read, Seek, SeekFrom};
            use std::process::{Command, Stdio};
            use std::time::{Duration, Instant};
            let mut stdout = tempfile::tempfile().unwrap();
            let mut stderr = tempfile::tempfile().unwrap();
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--test-threads=1", "--nocapture"])
                .env(MARKER, &name)
                .stdin(Stdio::null())
                .stdout(Stdio::from(stdout.try_clone().unwrap()))
                .stderr(Stdio::from(stderr.try_clone().unwrap()))
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(45);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("isolated history fixture timed out and was reaped");
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            stdout.seek(SeekFrom::Start(0)).unwrap();
            stderr.seek(SeekFrom::Start(0)).unwrap();
            let mut out = String::new();
            let mut err = String::new();
            stdout.read_to_string(&mut out).unwrap();
            stderr.read_to_string(&mut err).unwrap();
            assert!(
                status.success(),
                "history fixture failed: {status}\n{out}\n{err}"
            );
            assert!(
                out.contains("history fixture completed"),
                "child must execute its assertions"
            );
            return;
        }
        i_slint_backend_testing::init_no_event_loop();
        let w = MainWindow::new().unwrap();
        w.set_call_history_rows(ModelRc::new(VecModel::from(vec![CallHistoryRow {
            id: "original".into(),
            ..Default::default()
        }])));
        w.set_missed_call_count(4);
        w.set_call_history_loading(true);
        let old_all = HistoryRequest {
            generation: 1,
            session: 2,
        };
        // Older All completion (success or failure) must not end the Missed spinner
        // or overwrite its rows/badge. A completion from an old session is inert.
        complete_load(&w, old_all, 2, 2, Ok(CallHistoryPage::default()));
        complete_load(
            &w,
            old_all,
            2,
            2,
            Err(chatter_core::client::ClientError::Validation(
                "stale error".into(),
            )),
        );
        complete_load(
            &w,
            HistoryRequest {
                generation: 2,
                session: 1,
            },
            2,
            2,
            Ok(CallHistoryPage::default()),
        );
        assert!(w.get_call_history_loading());
        assert!(w.get_call_history_error().is_empty());
        assert_eq!(w.get_missed_call_count(), 4);
        apply_page(
            &w,
            Err(chatter_core::client::ClientError::Validation(
                "controlled read failure".into(),
            )),
        );
        use slint::Model;
        assert_eq!(
            w.get_call_history_rows().row_data(0).unwrap().id,
            "original"
        );
        assert_eq!(w.get_missed_call_count(), 4);
        assert!(!w.get_call_history_loading());
        assert!(!w.get_call_history_error().is_empty());
        complete_clear(
            &w,
            2,
            2,
            Err(chatter_core::client::ClientError::Validation(
                "controlled clear failure".into(),
            )),
            |_| panic!("failed clear must not reload or clear rows"),
        );
        assert!(w.get_toast_visible());
        assert_eq!(w.get_toast_variant(), "error");
        assert_eq!(
            w.get_call_history_rows().row_data(0).unwrap().id,
            "original"
        );
        assert_eq!(w.get_missed_call_count(), 4);
        apply_page(&w, Ok(CallHistoryPage::default()));
        assert_eq!(w.get_call_history_rows().row_count(), 0);
        assert!(w.get_call_history_error().is_empty());
        println!("history fixture completed");
    }
}
