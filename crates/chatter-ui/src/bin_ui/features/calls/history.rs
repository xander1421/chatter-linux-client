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

thread_local! {
    /// Filter and read time of the painted list — what "Clear call history"
    /// clears, so a call that arrived after the paint is never deleted unseen.
    /// UI-thread only, like the model it describes.
    static PAINTED: std::cell::Cell<Option<(CallHistoryFilter, chrono::DateTime<chrono::Utc>)>> =
        const { std::cell::Cell::new(None) };
    /// Where the next page of the painted list starts; `None` = every call
    /// is loaded. Taken when a page is requested, so one approach to the
    /// end asks once, and set again by the paint that lands it.
    static NEXT_CURSOR: std::cell::RefCell<Option<CallHistoryCursor>> =
        const { std::cell::RefCell::new(None) };
    /// Consecutive pages that landed with no visible row but a cursor (core
    /// drops this process's live calls from a Missed page after reading it).
    /// Each one is followed by a request for the next, so an empty page can
    /// never strand the cursor — bounded by `MAX_EMPTY_PAGES`, reset by a
    /// page with rows or a fresh first-page read.
    static EMPTY_PAGES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Longest chain of empty pages followed automatically: a first page plus
/// this many follow-ups, 9 × `HISTORY_PAGE` stored rows read. An expired
/// incoming offer is stored closed (call_history_recorder.rs
/// `IncomingMissed`), so the only open rows a Missed page hides are this
/// process's calls still ringing or connected — few, but the chain is kept
/// as a guard. Past it the cursor stays set, the screen shows
/// `paging-paused` with a "Load older calls" control, and that control
/// restarts the budget from the saved cursor (`resume_paging`).
const MAX_EMPTY_PAGES: u32 = 8;

/// How a landed page meets the model: the first page replaces it, a later
/// one is appended under the rows already on screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Paint {
    Replace,
    Append,
}

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

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::client::calls::{CallHistoryCursor, CallHistoryPage};
use chatter_core::domain::call::CallHistoryFilter;

use crate::bin_ui::decode_cache::{self, Pixels};
use crate::run_ui;
use crate::{CallHistoryRow, MainWindow};

/// Rows per page: what is read before anything is shown, not a ceiling on
/// the list. The screen asks for the next page as it nears its loaded end
/// (`load_more`), following the repository's `(started_at, id)` cursor.
const HISTORY_PAGE: u32 = 50;

/// Send-safe page prepared off the event loop. Images are created only in paint.
#[derive(Default)]
struct PreparedHistoryPage {
    page: CallHistoryPage,
    avatars: Vec<Option<Pixels>>,
}

fn prepare_page(_client: &ChatterClient, page: CallHistoryPage) -> PreparedHistoryPage {
    let avatars = page
        .rows
        .iter()
        .map(|row| {
            row.avatar_bytes.as_deref().and_then(decode_cache::avatar)
        })
        .collect();
    PreparedHistoryPage { page, avatars }
}

/// Wire the Calls tab and everything on the screen it opens.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_tab(window, ui_ctx);
    wire_filter(window, ui_ctx);
    wire_open(window, ui_ctx);
    wire_redial(window, ui_ctx);
    wire_clear(window, ui_ctx);
    wire_selection(window, ui_ctx);
    let weak = window.as_weak();
    let client = ui_ctx.clone();
    window.on_call_history_retry(move || {
        if let Some(w) = weak.upgrade() {
            reload(&w, &client);
        }
    });
    // The user asked for older calls: the list is within a screen of its
    // loaded end, or the "Load older calls" control after a paused chain
    // (`load-more` in call_history_screen.slint, forwarded by app.slint).
    let weak = window.as_weak();
    let client = ui_ctx.clone();
    window.on_call_history_load_more(move || {
        if let Some(w) = weak.upgrade() {
            resume_paging(&w, &client);
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
        // Nothing painted (still loading, or a new session) = nothing the
        // user confirmed clearing.
        let Some((filter, snapshot_at)) = PAINTED.with(|p| p.get()) else {
            return;
        };
        let weak = weak.clone();
        let ui_ctx = ui_ctx.clone();
        let request = HistoryRequest::begin(&ui_ctx);
        tokio::spawn(async move {
            let result = ui_ctx.clear_call_history(filter, snapshot_at).await;
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

/// Row selection and deleting the selected rows. The marks live on the
/// painted model's rows; a repaint (reload, filter, clear) drops them.
fn wire_selection(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    window.on_call_history_toggle_select(move |id: SharedString| {
        let Some(w) = weak.upgrade() else { return };
        // Entering selection starts from nothing marked but the held row.
        let entering = !w.get_call_history_selecting();
        mark_rows(&w, |row| {
            if row.id == id {
                !(row.selected && !entering)
            } else {
                row.selected && !entering
            }
        });
    });

    let weak = window.as_weak();
    window.on_call_history_select_all(move |all| {
        if let Some(w) = weak.upgrade() {
            mark_rows(&w, |_| all);
        }
    });

    let weak = window.as_weak();
    window.on_call_history_end_selection(move || {
        if let Some(w) = weak.upgrade() {
            mark_rows(&w, |_| false);
            w.set_call_history_selecting(false);
        }
    });

    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.on_call_history_delete_selected(move || {
        let Some(w) = weak.upgrade() else { return };
        // Snapshot the ids now: exactly what the confirmation counted.
        let ids: Vec<String> = w
            .get_call_history_rows()
            .iter()
            .filter(|row| row.selected)
            .map(|row| row.id.to_string())
            .collect();
        if ids.is_empty() {
            return;
        }
        let weak = weak.clone();
        let ui_ctx = ui_ctx.clone();
        let request = HistoryRequest::begin(&ui_ctx);
        tokio::spawn(async move {
            let wanted = ids.len() as u64;
            let result = ui_ctx.delete_call_history_entries(ids).await;
            let client = ui_ctx.clone();
            run_ui!(weak, |w| {
                if request.session != client.auth_state().get().session_revision {
                    return;
                }
                match result {
                    Ok(deleted) => {
                        if deleted < wanted {
                            toast(&w, "A call in progress was kept.", "info");
                        }
                        refresh_in_place(&w, &client);
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to delete call history rows");
                        toast(&w, "Could not delete those calls. Try again.", "error");
                    }
                }
            });
        });
    });
}

/// Set every painted row's mark from `selected`, enter selection, and
/// publish the count.
fn mark_rows(window: &MainWindow, selected: impl Fn(&CallHistoryRow) -> bool) {
    let rows = window.get_call_history_rows();
    let mut count = 0;
    for i in 0..rows.row_count() {
        let Some(mut row) = rows.row_data(i) else { continue };
        let mark = selected(&row);
        count += i32::from(mark);
        if row.selected != mark {
            row.selected = mark;
            rows.set_row_data(i, row);
        }
    }
    window.set_call_history_selected_count(count);
    window.set_call_history_selecting(true);
}

fn toast(window: &MainWindow, message: &str, variant: &str) {
    window.set_toast_message(message.into());
    window.set_toast_variant(variant.into());
    window.set_toast_visible(true);
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

/// Re-read the list from core and repaint it, badge included: the first
/// page, from the newest call, behind the loading state.
///
/// The read itself runs on core's blocking pool; only the model swap happens
/// on the event loop.
pub(crate) fn reload(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    window.set_call_history_loading(true);
    request_page(window, ui_ctx, HISTORY_PAGE, None, Paint::Replace);
}

/// After rows were deleted: re-read the span the user had loaded and swap it
/// in place. `loading` is not raised, so the list element survives and the
/// viewport stays where it was rather than jumping back to the newest call.
fn refresh_in_place(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let loaded = window.get_call_history_rows().row_count() as u32;
    request_page(window, ui_ctx, loaded.max(HISTORY_PAGE), None, Paint::Replace);
}

/// A user request for older calls (scroll near the end, or the control shown
/// while paused): a fresh empty-page budget, then the saved cursor. Only
/// user actions come here; the automatic chain calls `load_more` directly
/// and keeps counting. Nothing to do once the last page is in, or while one
/// is in flight — `loading` is raised only when a page will actually be
/// asked for, so no spinner can outlive its request.
fn resume_paging(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    if NEXT_CURSOR.with(|c| c.borrow().is_none()) {
        return;
    }
    EMPTY_PAGES.with(|n| n.set(0));
    window.set_call_history_paging_paused(false);
    if window.get_call_history_rows().row_count() == 0 {
        // Nothing on screen to append under: show the read as a load.
        window.set_call_history_loading(true);
    }
    load_more(window, ui_ctx);
}

/// Request the page after the saved cursor. Nothing to do once the last
/// page is in, or while one is in flight.
fn load_more(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let Some(cursor) = NEXT_CURSOR.with(|c| c.borrow_mut().take()) else {
        return;
    };
    request_page(window, ui_ctx, HISTORY_PAGE, Some(cursor), Paint::Append);
}

fn request_page(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    limit: u32,
    cursor: Option<CallHistoryCursor>,
    paint: Paint,
) {
    let filter = match window.get_call_history_filter().as_str() {
        "missed" => CallHistoryFilter::Missed,
        _ => CallHistoryFilter::All,
    };
    let request = HistoryRequest::begin(ui_ctx);
    if HISTORY_MODEL_SESSION.swap(request.session, Ordering::SeqCst) != request.session {
        // Successful data belongs to its auth session, never to a new login.
        window.set_call_history_rows(ModelRc::new(VecModel::<CallHistoryRow>::default()));
        window.set_missed_call_count(0);
        PAINTED.with(|p| p.set(None));
        NEXT_CURSOR.with(|c| c.borrow_mut().take());
    }
    if cursor.is_none() {
        // A read from the newest call starts the empty-page budget over.
        EMPTY_PAGES.with(|n| n.set(0));
    }
    // Any new request ends a pause; the page that lands decides again.
    window.set_call_history_paging_paused(false);
    window.set_call_history_error("".into());

    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    tokio::spawn(async move {
        let page = load_page(&ui_ctx, filter, limit, cursor).await;
        let client = ui_ctx.clone();
        run_ui!(weak, |w| {
            let continue_paging = complete_load(
                &w,
                request,
                HISTORY_GENERATION.load(Ordering::SeqCst),
                client.auth_state().get().session_revision,
                page,
                paint,
            );
            // The page landed without a visible row but with a cursor: no
            // ListView exists to ask for the next one (an empty first page)
            // or its arming never fired (an appended page of zero rows), so
            // ask here. One hop per landed page — a chain of event-loop
            // tasks, not a loop — each a fresh request behind the same
            // generation/session fence, each strictly older by the
            // repository's cursor, each dropping the page before it.
            if continue_paging {
                load_more(&w, &client);
            }
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

/// Load core's page, then read configured icons and decode cached avatars on
/// the blocking pool. Only Slint Image creation and model paint run on the UI.
async fn load_page(
    ui_ctx: &Arc<ChatterClient>,
    filter: CallHistoryFilter,
    limit: u32,
    cursor: Option<CallHistoryCursor>,
) -> Result<PreparedHistoryPage, chatter_core::client::ClientError> {
    let page = ui_ctx.call_history_page(filter, limit, cursor).await?;
    let client = ui_ctx.clone();
    tokio::task::spawn_blocking(move || prepare_page(&client, page))
        .await
        .map_err(|error| {
            chatter_core::client::ClientError::Inbox(format!(
                "call history avatar preparation: {error}"
            ))
        })
}

/// Applies the page if it is still the current request. Returns whether the
/// caller should ask for the next page at once (see `paint`).
fn complete_load(
    window: &MainWindow,
    request: HistoryRequest,
    generation: u64,
    session: u64,
    page: Result<PreparedHistoryPage, chatter_core::client::ClientError>,
    paint: Paint,
) -> bool {
    request.matches(generation, session) && apply_page(window, page, paint)
}

fn apply_page(
    window: &MainWindow,
    page: Result<PreparedHistoryPage, chatter_core::client::ClientError>,
    how: Paint,
) -> bool {
    match page {
        Ok(page) => {
            window.set_call_history_error("".into());
            paint(window, page, how)
        }
        Err(error) => {
            tracing::error!(%error, "Failed to load call history");
            window.set_call_history_loading(false);
            window.set_call_history_error("Load failed; showing previous calls.".into());
            false
        }
    }
}

/// Returns true when the page had no visible row but names a next one and
/// the empty-page budget allows following it: the caller requests it, and
/// `loading` stays up meanwhile so an empty first page does not flash the
/// empty state before the rows behind the hidden live calls arrive.
fn paint(window: &MainWindow, prepared: PreparedHistoryPage, how: Paint) -> bool {
    let page = prepared.page;
    let filter = match window.get_call_history_filter().as_str() {
        "missed" => CallHistoryFilter::Missed,
        _ => CallHistoryFilter::All,
    };
    if how == Paint::Replace {
        // A later page keeps the first paint's time: clearing "this list"
        // must not reach a call that arrived while the user paged down.
        PAINTED.with(|p| p.set(Some((filter, page.snapshot_at))));
    }
    let continue_paging = if page.rows.is_empty() && page.next_cursor.is_some() {
        let empty = EMPTY_PAGES.with(|n| n.replace(n.get() + 1) + 1);
        let paused = empty > MAX_EMPTY_PAGES;
        if paused {
            tracing::warn!(
                empty,
                "call history: empty-page budget spent with older calls left; paused"
            );
        }
        // Paused: cursor kept below, budget kept spent until the user asks.
        window.set_call_history_paging_paused(paused);
        !paused
    } else {
        // Rows, or the end of the list: nothing is paused.
        EMPTY_PAGES.with(|n| n.set(0));
        window.set_call_history_paging_paused(false);
        false
    };
    NEXT_CURSOR.with(|c| *c.borrow_mut() = page.next_cursor);
    let rows: Vec<CallHistoryRow> = page
        .rows
        .into_iter()
        .zip(prepared.avatars)
        .map(|(r, avatar)| {
            let name = if r.name.is_empty() { "Unknown" } else { &r.name };
            CallHistoryRow {
                avatar_source: avatar.map(slint::Image::from_rgba8).unwrap_or_default(),
                id: r.id.into(),
                conversation_id: r.conversation_id.into(),
                conversation_type: r.conversation_type.into(),
                name: name.into(),
                initials: crate::bin_ui::converters::initials_from_name(name).into(),
                timestamp: r.started_at.map(crate::bin_ui::converters::sidebar_time).unwrap_or_default().into(),
                detail: if r.in_progress { "In progress".to_string() } else { crate::bin_ui::converters::call_detail(r.result, r.is_incoming, r.duration_secs) }.into(),
                is_missed: r.is_missed,
                is_video: r.is_video,
                is_incoming: r.is_incoming,
                can_call_back: r.can_call_back,
                selected: false,
            }
        })
        .collect();
    match how {
        Paint::Replace => {
            window.set_call_history_rows(ModelRc::new(VecModel::from(rows)));
            // New rows carry no marks, so any selection is over.
            window.set_call_history_selecting(false);
            window.set_call_history_selected_count(0);
        }
        Paint::Append => {
            // Under the rows on screen, in the model they are bound to, so
            // the list and its marks stay put. The painted model is always
            // the VecModel above; a foreign model (a test's) is rebuilt.
            let model = window.get_call_history_rows();
            match model.as_any().downcast_ref::<VecModel<CallHistoryRow>>() {
                Some(vec) => vec.extend(rows),
                None => {
                    let mut all: Vec<CallHistoryRow> = model.iter().collect();
                    all.extend(rows);
                    window.set_call_history_rows(ModelRc::new(VecModel::from(all)));
                }
            }
        }
    }
    if !continue_paging {
        window.set_call_history_loading(false);
    }
    window.set_missed_call_count(page.missed_count as i32);
    continue_paging
}

#[cfg(test)]
mod tests {
    use super::*;
    const MARKER: &str = "CHATTER_CALL_HISTORY_FIXTURE_CHILD";
    fn isolated_fixture(name: &str) -> bool {
        if std::env::var(MARKER).ok().as_deref() != Some(name) {
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
            return false;
        }
        true
    }

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
    fn configured_channel_icons_load_and_paint_without_crossing_identity_boundaries() {
        use chatter_core::domain::call::{CallDirection, CallHistoryEntry, CallResult};
        use chatter_core::domain::{
            ChannelType,
            ids::{ChannelId, UserId},
            message::ConversationType,
        };
        use chatter_core::storage::repositories::{
            CallHistoryRepository, channel_repo::ChannelRepository,
        };
        use slint::Model;
        let name = format!(
            "{}::configured_channel_icons_load_and_paint_without_crossing_identity_boundaries",
            module_path!().split_once("::").unwrap().1
        );
        if !isolated_fixture(&name) {
            return;
        }
        unsafe {
            std::env::set_var("CHATTER_NO_KEYRING", "1");
            std::env::set_var("CHATTER_DB_PASSPHRASE", "calls-icon-fixture");
            std::env::set_var("CHATTER_HEADLESS", "1");
        }
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let fixture = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .data_dir(fixture.path().to_string_lossy().into_owned())
            .build()
            .unwrap();
        let path = chatter_core::system::Directories::with_base_dir(fixture.path().to_path_buf())
            .unwrap()
            .database_path();
        let db =
            Arc::new(chatter_core::storage::Database::open(&path, "calls-icon-fixture").unwrap());
        let channels = ChannelRepository::new(db.clone());
        let calls = CallHistoryRepository::new(db);
        let id = ChannelId::new();
        let missing = ChannelId::new();
        let mut channel = chatter_core::domain::channel::Channel::new(
            id,
            "Current channel name".into(),
            ChannelType::Private,
            UserId::new(),
        );
        let png = |width, color| {
            let mut encoded = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                width,
                2,
                image::Rgba(color),
            ))
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
            encoded.into_inner()
        };
        channel.icon = Some(png(3, [0, 255, 0, 255]));
        channels.insert(&channel).unwrap();
        for (call_id, key, kind) in [
            ("channel", id, ConversationType::Channel),
            ("deleted", missing, ConversationType::Channel),
            ("anonymous-group", id, ConversationType::AnonGroup),
        ] {
            calls
                .insert_entry(&CallHistoryEntry {
                    id: call_id.into(),
                    conversation_id: key.to_string(),
                    conversation_type: kind,
                    peer_name: "Name at call time".into(),
                    started_at: chrono::Utc::now(),
                    ended_at: None,
                    direction: CallDirection::Incoming,
                    result: CallResult::Missed,
                    duration_secs: None,
                    has_video: false,
                })
                .unwrap();
        }
        // Exercise the actual async loader and stored channel metadata, not a
        // second test-only lookup; a same-UUID anon group must retain initials.
        let prepared = runtime
            .block_on(load_page(&client, CallHistoryFilter::All, HISTORY_PAGE, None))
            .unwrap();
        let channel_index = prepared
            .page
            .rows
            .iter()
            .position(|r| r.id == "channel")
            .unwrap();
        assert!(prepared.avatars[channel_index].is_some());
        assert_eq!(prepared.page.rows[channel_index].name, "Name at call time");
        for id in ["deleted", "anonymous-group"] {
            let index = prepared.page.rows.iter().position(|r| r.id == id).unwrap();
            assert!(prepared.avatars[index].is_none());
        }
        // DM peer sources remain core's chosen bytes even with a matching
        // channel UUID; anonymized identities never borrow its configured icon.
        let dm_bytes = png(4, [255, 0, 0, 255]);
        let page = CallHistoryPage {
            rows: [ConversationType::ServerDm, ConversationType::AnonDm]
                .into_iter()
                .map(|kind| chatter_core::ui::CallHistoryRowData {
                    conversation_id: id.to_string(),
                    conversation_type: kind.as_str().into(),
                    avatar_bytes: Some(dm_bytes.clone()),
                    ..Default::default()
                })
                .collect(),
            missed_count: 0,
            snapshot_at: chrono::Utc::now(),
            next_cursor: None,
        };
        // Production avatar cache normalizes to its 128px row budget, including
        // upscaling these deliberately tiny encoded fixtures. Bytes/identity
        // assertions above and changed-aspect checks below remain independent.
        let dm = prepare_page(&client, page);
        assert!(
            dm.page
                .rows
                .iter()
                .all(|r| r.avatar_bytes.as_ref() == Some(&dm_bytes))
        );
        assert!(
            dm.avatars
                .iter()
                .all(|pixels| (pixels.as_ref().unwrap().width(), pixels.as_ref().unwrap().height()) == (128, 64))
        );
        i_slint_backend_testing::init_no_event_loop();
        let w = MainWindow::new().unwrap();
        let request = HistoryRequest {
            generation: 11,
            session: 7,
        };
        complete_load(&w, request, 12, 7, Ok(prepared), Paint::Replace);
        assert_eq!(
            w.get_call_history_rows().row_count(),
            0,
            "newer filter rejects decoded old page"
        );
        let prepared = runtime
            .block_on(load_page(&client, CallHistoryFilter::Missed, HISTORY_PAGE, None))
            .unwrap();
        complete_load(&w, request, 11, 8, Ok(prepared), Paint::Replace);
        assert_eq!(
            w.get_call_history_rows().row_count(),
            0,
            "new login rejects decoded old page"
        );
        let prepared = runtime
            .block_on(load_page(&client, CallHistoryFilter::All, HISTORY_PAGE, None))
            .unwrap();
        complete_load(&w, request, 11, 7, Ok(prepared), Paint::Replace);
        let row = w
            .get_call_history_rows()
            .iter()
            .find(|r| r.id == "channel")
            .unwrap();
        assert_eq!((row.avatar_source.size().width, row.avatar_source.size().height), (128, 85));
        assert_eq!(row.name.as_str(), "Name at call time");
        for id in ["deleted", "anonymous-group"] {
            let row = w
                .get_call_history_rows()
                .iter()
                .find(|r| r.id == id)
                .unwrap();
            assert_eq!((row.avatar_source.size().width, row.avatar_source.size().height), (0, 0));
        }
        // Byte-keyed cache must observe a configured icon changed between loads.
        channel.icon = Some(png(5, [255, 255, 0, 255]));
        channels.update(&channel).unwrap();
        let prepared = runtime
            .block_on(load_page(&client, CallHistoryFilter::All, HISTORY_PAGE, None))
            .unwrap();
        complete_load(&w, request, 11, 7, Ok(prepared), Paint::Replace);
        let row = w
            .get_call_history_rows()
            .iter()
            .find(|r| r.id == "channel")
            .unwrap();
        assert_eq!((row.avatar_source.size().width, row.avatar_source.size().height), (128, 51));
        channel.icon = Some(vec![1, 2, 3]);
        channels.update(&channel).unwrap();
        let prepared = runtime
            .block_on(load_page(&client, CallHistoryFilter::All, HISTORY_PAGE, None))
            .unwrap();
        complete_load(&w, request, 11, 7, Ok(prepared), Paint::Replace);
        let row = w
            .get_call_history_rows()
            .iter()
            .find(|r| r.id == "channel")
            .unwrap();
        assert_eq!((row.avatar_source.size().width, row.avatar_source.size().height), (0, 0));
        println!("history fixture completed");
    }

    #[test]
    fn read_failure_preserves_rows_and_unread_then_retry_replaces_them() {
        let name = format!(
            "{}::read_failure_preserves_rows_and_unread_then_retry_replaces_them",
            module_path!().split_once("::").unwrap().1
        );
        if !isolated_fixture(&name) {
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
        complete_load(&w, old_all, 2, 2, Ok(PreparedHistoryPage::default()), Paint::Replace);
        complete_load(
            &w,
            old_all,
            2,
            2,
            Err(chatter_core::client::ClientError::Validation(
                "stale error".into(),
            )),
            Paint::Replace,
        );
        complete_load(
            &w,
            HistoryRequest {
                generation: 2,
                session: 1,
            },
            2,
            2,
            Ok(PreparedHistoryPage::default()),
            Paint::Replace,
        );
        assert!(w.get_call_history_loading());
        assert!(w.get_call_history_error().is_empty());
        assert_eq!(w.get_missed_call_count(), 4);
        apply_page(
            &w,
            Err(chatter_core::client::ClientError::Validation(
                "controlled read failure".into(),
            )),
            Paint::Replace,
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
        apply_page(&w, Ok(PreparedHistoryPage::default()), Paint::Replace);
        assert_eq!(w.get_call_history_rows().row_count(), 0);
        assert!(w.get_call_history_error().is_empty());

        // Paging: a later page lands under the rows on screen, in the same
        // model, and the cursor it carries is what the next request takes.
        let page = |ids: &[&str], next: Option<&str>| PreparedHistoryPage {
            page: CallHistoryPage {
                rows: ids
                    .iter()
                    .map(|id| chatter_core::ui::CallHistoryRowData {
                        id: (*id).into(),
                        ..Default::default()
                    })
                    .collect(),
                missed_count: 0,
                snapshot_at: chrono::Utc::now(),
                next_cursor: next.map(|id| (chrono::Utc::now(), id.to_string())),
            },
            avatars: ids.iter().map(|_| None).collect(),
        };
        apply_page(&w, Ok(page(&["newest", "newer"], Some("newer"))), Paint::Replace);
        let first = w.get_call_history_rows();
        assert_eq!(
            NEXT_CURSOR.with(|c| c.borrow().as_ref().map(|c| c.1.clone())),
            Some("newer".to_string())
        );
        apply_page(&w, Ok(page(&["older"], None)), Paint::Append);
        let ids: Vec<String> = w
            .get_call_history_rows()
            .iter()
            .map(|r| r.id.to_string())
            .collect();
        assert_eq!(ids, ["newest", "newer", "older"]);
        assert_eq!(first.row_count(), 3, "appended into the bound model, not a new one");
        assert!(NEXT_CURSOR.with(|c| c.borrow().is_none()), "last page ends paging");
        println!("history fixture completed");
    }
}
