//! Composer callbacks.
//!
//! Handles message composer actions (attach, emoji, gif, mentions, drag/drop).

mod attachments;
pub(crate) use attachments::render_attachments;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod drag_drop;
mod emoji;
mod format_text;
pub mod gif;
mod mentions;
mod voice;

use std::sync::Arc;

use chatter_core::client::ChatterClient;

use crate::MainWindow;

/// Wire composer callbacks.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    attachments::wire_attachment_clicked(window, ui_ctx.chat_state().clone(), ui_ctx);
    attachments::wire_remove_attachment(window, ui_ctx);
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    attachments::wire_paste_image(window, ui_ctx.chat_state().clone());
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    drag_drop::wire_drag_drop(window, ui_ctx);
    emoji::wire_emoji_clicked(window, ui_ctx);
    gif::wire_gif_clicked(window, ui_ctx);
    gif::wire_gif_selected(window, ui_ctx);
    gif::wire_gif_search(window, ui_ctx);
    mentions::wire_mention_triggered(window, ui_ctx.clone(), ui_ctx.chat_state().clone());
    mentions::wire_mention_selected(window);
    voice::wire(window, ui_ctx);
    wire_text_format(window);
    wire_per_conversation_drafts(window, ui_ctx);
    wire_draft_autosave(window, ui_ctx);
    wire_thread_draft_autosave(window, ui_ctx);
}

/// Retry failed writes without needing another edit. Keep the timer alive in
/// a window callback; writes still happen only for dirty state, never per key.
fn wire_persistence_retry(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) -> slint::Timer {
    let timer = slint::Timer::default();
    let weak = slint::ComponentHandle::as_weak(window);
    let ctx = ui_ctx.clone();
    let mut warned = false;
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_secs(5),
        move || {
            let Some(w) = weak.upgrade() else { return };
            if ctx.composer_persistence_pending() {
                let pending = ctx.retry_dirty_composer_persistence();
                if pending && !warned {
                    show_persistence_failure(&w);
                }
                warned = pending;
            } else {
                warned = false;
            }
        },
    );
    timer
}

fn show_persistence_failure(w: &MainWindow) {
    w.set_toast_message(
        "Couldn't save drafts on this device. New changes are kept only while the app is open."
            .into(),
    );
    w.set_toast_variant("error".into());
    w.set_toast_visible(true);
}

pub fn flush_drafts(w: &MainWindow, ctx: &ChatterClient) {
    autosave_composer_draft(w, ctx);
    autosave_thread_draft(w, ctx);
    if ctx.flush_composer_persistence() {
        show_persistence_failure(w);
    }
}

/// Store what the composer shows right now under the conversation it belongs
/// to, so an app restart brings it back. Core decides the owner and whether
/// the session is still the one the text was typed under. Edit mode borrows
/// the composer for a message's text; that is never a draft.
pub fn autosave_composer_draft(w: &MainWindow, ctx: &ChatterClient) {
    if w.get_composer_is_editing() {
        return;
    }
    ctx.autosave_composer_draft(chatter_core::client::ComposerDraft {
        text: w.get_composer_text().to_string(),
        reply: crate::bin_ui::features::chat::send_message::composer_reply(w),
    });
}

/// Store the thread panel's text under the thread it is for. Nothing is
/// stored while no thread is loaded (the root id is cleared when the panel
/// closes), so a late tick cannot wipe the draft the close just parked.
pub fn autosave_thread_draft(w: &MainWindow, ctx: &ChatterClient) {
    let Ok(root) = w
        .get_thread_root_message_id()
        .parse::<chatter_core::domain::ids::MessageId>()
    else {
        return;
    };
    ctx.set_thread_draft(&root, &w.get_thread_composer_text());
}

/// The thread panel's text, debounced like the composer's. The thread shown
/// when the change happened is captured with the session: a tick after the
/// panel moved to another thread, or closed, is dropped.
fn wire_thread_draft_autosave(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = slint::ComponentHandle::as_weak(window);
    let ctx = ui_ctx.clone();
    let timer = slint::Timer::default();
    window.on_thread_draft_changed(move || {
        let Some(w) = weak.upgrade() else { return };
        let root = w.get_thread_root_message_id();
        if root.is_empty() {
            return;
        }
        let weak = weak.clone();
        let ctx = ctx.clone();
        let session = crate::bin_ui::features::history_owner::SessionOwner::capture(&ctx);
        timer.start(
            slint::TimerMode::SingleShot,
            std::time::Duration::from_millis(400),
            move || {
                let Some(w) = weak.upgrade() else { return };
                if !session.is_current() || w.get_thread_root_message_id() != root {
                    return;
                }
                autosave_thread_draft(&w, &ctx);
            },
        );
    });
}

/// Every text or reply-bar change schedules one autosave shortly after the
/// typing pauses; a change inside the window restarts the wait. Lifecycle
/// edges (Android pause, desktop exit, send) call the save directly.
///
/// The session and the conversation owning the composer are captured when
/// the change happens. A tick that fires after either moved on (sign-out,
/// account switch, navigation that already parked the text) is dropped
/// rather than storing whatever the composer shows by then; core checks the
/// same two things again under its lock.
fn wire_draft_autosave(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let retry_timer = wire_persistence_retry(window, ui_ctx);
    let weak = slint::ComponentHandle::as_weak(window);
    let ctx = ui_ctx.clone();
    let timer = slint::Timer::default();
    window.on_composer_draft_changed(move || {
        let _ = &retry_timer;
        let weak = weak.clone();
        let ctx = ctx.clone();
        let session = crate::bin_ui::features::history_owner::SessionOwner::capture(&ctx);
        let owner = ctx.composer_owner_key();
        timer.start(
            slint::TimerMode::SingleShot,
            std::time::Duration::from_millis(400),
            move || {
                if !session.is_current() || ctx.composer_owner_key() != owner {
                    return;
                }
                if let Some(w) = weak.upgrade() {
                    autosave_composer_draft(&w, &ctx);
                }
            },
        );
    });
}

/// Clear only composer presentation when its account is being signed out.
/// Core clears the matching drafts, files and pending send intents.
pub fn reset_session(window: &MainWindow, ui_ctx: &ChatterClient) {
    use slint::ComponentHandle;
    // Invalidate old completions synchronously, before clearing presentation.
    // This also removes any draft a queued navigation callback re-parked.
    ui_ctx.clear_composer_session();
    #[cfg(feature = "mesh")]
    crate::bin_ui::features::mesh::clear_active(window);
    window.global::<crate::VoiceRecording>().invoke_cancel();
    window.set_composer_emoji_picker_visible(false);
    window.set_composer_gif_picker_visible(false);
    // The old account's GIF rows and their animation frames do not outlive it.
    window.set_composer_gif_picker_results(slint::ModelRc::new(
        slint::VecModel::<crate::GifData>::default(),
    ));
    window.set_composer_gif_picker_recent(slint::ModelRc::new(
        slint::VecModel::<crate::GifData>::default(),
    ));
    crate::bin_ui::gif_state::clear_picker_gifs();
    window.invoke_release_text_focus();
    window.invoke_settle_composer();
    crate::bin_ui::features::chat::edit::end_edit(window);
    window.set_composer_text("".into());
    crate::bin_ui::features::chat::send_message::clear_reply(window);
    render_attachments(window, &[]);
    // Root first, so the text change's autosave finds no thread to write to.
    window.set_thread_root_message_id("".into());
    window.set_thread_composer_text("".into());
}

/// Wire the pure text operations independently of network/client callbacks.
pub fn wire_text_format(window: &MainWindow) {
    format_text::wire_format_text(window);
}

/// Each conversation keeps its own unsent text (OPEN-ITEMS §112x). Every
/// conversation switch goes through `ChatState`, so watch it: on a change of
/// active conversation, park the composer under the one left and restore the
/// one entered. UI navigation also synchronizes immediately after committing
/// selection, before any header/history work; this subscriber covers other paths.
fn wire_per_conversation_drafts(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let key = |d: &chatter_core::state::ChatStateData| {
        d.active_channel_id
            .map(|id| id.to_string())
            .unwrap_or_default()
    };
    // Which files are staged where. A change rewrites the account's recovery
    // row (rare: a file added, removed, sent or restored — never a keystroke).
    let trays = |d: &chatter_core::state::ChatStateData| {
        let mut ids: Vec<String> = d
            .pending_attachments
            .iter()
            .map(|a| format!("{:?}:{}", d.active_channel_id, a.id))
            .collect();
        for (tray, staged) in &d.attachment_trays {
            ids.extend(staged.iter().map(|a| format!("{tray:?}:{}", a.id)));
        }
        ids.sort();
        ids
    };
    let mut rx = ui_ctx.chat_state().subscribe();
    let initial = ui_ctx.chat_state().get();
    let mut current = key(&initial);
    let mut current_revision = initial.attachment_revision;
    let mut current_trays = trays(&initial);
    // The composer starts out holding `current`'s draft.
    let draft = ui_ctx.swap_composer_draft("", Default::default(), &current);
    if !draft.text.is_empty() {
        window.set_composer_text(draft.text.into());
    }
    if let Some(reply) = draft.reply {
        crate::bin_ui::features::chat::send_message::show_reply(window, reply);
    }
    let weak = slint::ComponentHandle::as_weak(window);
    let ctx = ui_ctx.clone();
    tokio::spawn(async move {
        loop {
            let snapshot = match rx.recv().await {
                Ok(snap) => snap,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    Arc::new(ctx.chat_state().get())
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let staged = trays(&snapshot);
            if staged != current_trays && ctx.persist_composer_recovery() {
                current_trays = staged;
            }
            let entering = key(&snapshot);
            if entering == current && snapshot.attachment_revision == current_revision {
                continue;
            }
            current = entering;
            let revision = snapshot.attachment_revision;
            current_revision = revision;
            let session = crate::bin_ui::features::history_owner::SessionOwner::capture(&ctx);
            let ctx = ctx.clone();
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if !session.is_current() {
                    return;
                }
                if let Some(w) = weak.upgrade() {
                    sync_composer_draft(&w, &ctx, revision);
                }
            });
        }
    });
}

/// Apply queued navigation to the actual current conversation and draft owner.
/// Intermediate snapshots may have been overtaken; old-account callbacks expire.
pub fn sync_composer_draft(w: &MainWindow, ctx: &ChatterClient, revision: u64) {
    let latest = ctx.chat_state().get();
    if latest.attachment_revision != revision {
        return;
    }
    let entering = if let Some(id) = latest.active_channel_id {
        id.to_string()
    } else {
        mesh_draft_key(w)
    };
    let leaving = ctx.composer_owner_key();
    if entering != leaving {
        switch_composer_draft(w, ctx, &leaving, &entering);
    }
}

/// Core represents every mesh scope as None; the UI owns its exact id.
fn mesh_draft_key(w: &MainWindow) -> String {
    #[cfg(feature = "mesh")]
    {
        use slint::ComponentHandle;
        if crate::bin_ui::features::mesh::active().is_some() {
            return format!("mesh:{}", w.global::<crate::MeshGlobal>().get_active_id());
        }
    }
    let _ = w;
    String::new()
}

/// Park the composer under `leaving` and load `entering`'s draft.
pub fn switch_composer_draft(w: &MainWindow, ctx: &ChatterClient, leaving: &str, entering: &str) {
    w.set_composer_emoji_picker_visible(false);
    w.set_composer_gif_picker_visible(false);
    // A word the input method is still composing belongs to the conversation
    // being left; fold it in and end the composition before the text moves.
    // The settle reads a mirror kept by `changed` handlers. Slint runs those
    // at the end of every key/IME event it processes; run any still pending
    // in case the mirrored state changed some other way since.
    i_slint_core::properties::ChangeTracker::run_change_handlers();
    w.invoke_settle_composer();
    // Left mid-edit: the edit is cancelled and the draft it
    // held aside is what gets parked, not the edit text.
    crate::bin_ui::features::chat::edit::end_edit(w);
    // The reply bar's target stays with its conversation, like the text.
    let leaving_draft = chatter_core::client::ComposerDraft {
        text: w.get_composer_text().to_string(),
        reply: crate::bin_ui::features::chat::send_message::composer_reply(w),
    };
    crate::bin_ui::features::chat::send_message::clear_reply(w);
    let entering_draft = ctx.swap_composer_draft(leaving, leaving_draft, entering);
    w.set_composer_text(entering_draft.text.into());
    if let Some(reply) = entering_draft.reply {
        crate::bin_ui::features::chat::send_message::show_reply(w, reply);
    }
    render_attachments(w, &ctx.chat_state().get_pending_attachments());
    // A GIF send that failed while this conversation was not open.
    let empty = crate::bin_ui::features::chat::send_message::composer_is_empty(w);
    if let Some(mut send) = ctx.take_parked_send(empty) {
        // Core installed these trays as part of the durable intent transfer.
        send.attachments.clear();
        crate::bin_ui::features::chat::send_message::restore_composer(w, ctx, send);
    }
}
