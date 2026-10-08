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
    attachments::wire_remove_attachment(window, ui_ctx.chat_state().clone());
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
    window.invoke_release_text_focus();
    window.invoke_settle_composer();
    crate::bin_ui::features::chat::edit::end_edit(window);
    window.set_composer_text("".into());
    crate::bin_ui::features::chat::send_message::clear_reply(window);
    window.set_composer_send_status("".into());
    render_attachments(window, &[]);
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
    let mut rx = ui_ctx.chat_state().subscribe();
    let initial = ui_ctx.chat_state().get();
    let mut current = key(&initial);
    let mut current_revision = initial.attachment_revision;
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
    // The send status line belongs to the conversation being left.
    w.set_composer_send_status("".into());
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
    if let Some(send) = ctx.take_parked_send(empty) {
        crate::bin_ui::features::chat::send_message::restore_composer(w, ctx, send);
    }
}
