//! Thread panel callbacks.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::MessageId;
use chatter_core::domain::message::Message;

use super::chat::recompute_channel_continuations;
use super::history_owner::HistoryOwner;
use crate::bin_ui::converters::messages_to_display_batch;
use crate::{MainWindow, MessageItemData};

pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_close_thread(window, ui_ctx);
    wire_send_thread_reply(window, ui_ctx);
    wire_load_more_thread_replies(window, ui_ctx);
}

fn wire_close_thread(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let client = ui_ctx.clone();
    window
        .global::<crate::ChatPane>()
        .on_close_thread_clicked(move || {
            let owner = HistoryOwner::capture(&client, client.active_conversation());
            let chat = client.clone();
            let weak = weak_window.clone();
            if let Some(w) = weak_window.upgrade() {
                // The text stays with its thread; root first, so the text
                // change's autosave has no thread to overwrite it under.
                crate::bin_ui::features::composer::autosave_thread_draft(&w, &client);
                w.global::<crate::ChatPane>()
                    .set_thread_root_message_id(SharedString::default());
                w.global::<crate::ChatPane>()
                    .set_thread_composer_text(SharedString::default());
            }
            if let Some(thread) = client.active_thread() {
                client.close_thread(thread.root_message_id);
            }
            let _ = slint::invoke_from_event_loop(move || {
                if !owner.is_current() || chat.active_thread().is_some() {
                    return;
                }
                if let Some(w) = weak.upgrade() {
                    w.global::<crate::ChatPane>()
                        .set_thread_panel_visible(false);
                    w.global::<crate::ChatPane>()
                        .set_thread_replies(ModelRc::new(VecModel::default()));
                }
            });
        });
}

fn wire_send_thread_reply(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window
        .global::<crate::ChatPane>()
        .on_send_thread_reply(move |text: SharedString| {
            let text = text.to_string();
            if text.trim().is_empty() {
                return;
            }
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx.clone();
            let Some(thread) = ui_ctx.active_thread() else {
                return;
            };
            // The panel cleared its composer on tap: the persisted draft goes
            // now, not after the autosave delay, so a restart cannot resend it.
            // A reply that then fails is given back below.
            ui_ctx.set_thread_draft(&thread.root_message_id, "");
            let session_revision = ui_ctx.auth_snapshot().session_revision;
            let owner = HistoryOwner::thread(&ui_ctx, thread.root_message_id, false);
            tokio::spawn(async move {
                if !owner.is_current() {
                    // Not sent, but the draft is already cleared: give it back.
                    ui_ctx.park_failed_thread_reply(
                        &thread.root_message_id,
                        &text,
                        session_revision,
                    );
                    return;
                }
                let sent = ui_ctx
                    .send_channel_message(
                        None,
                        &thread.channel_id,
                        &text,
                        vec![],
                        vec![],
                        Some(&thread.root_message_id),
                        vec![],
                        false,
                    )
                    .await;
                if let Err(e) = &sent {
                    // The panel cleared its composer on tap. A reply that did not
                    // leave must say so and give the text back — it used to vanish.
                    tracing::error!(error = %e, "thread reply send failed");
                    let root = thread.root_message_id;
                    let _ = slint::invoke_from_event_loop(move || {
                        if !ui_ctx.composer_session_is_current(session_revision) {
                            return;
                        }
                        let Some(w) = weak.upgrade() else { return };
                        // Still this thread's panel with nothing typed since:
                        // straight back into the box, and its autosave follows.
                        // Any other panel, or newer text in this one: back to the
                        // thread's own draft, where opening it finds the text —
                        // never over what was typed since (core keeps the newer).
                        let shown = owner.is_current()
                            && w.global::<crate::ChatPane>()
                                .get_thread_root_message_id()
                                .as_str()
                                == root.to_string();
                        let message = if shown
                            && w.global::<crate::ChatPane>()
                                .get_thread_composer_text()
                                .is_empty()
                        {
                            ui_ctx.park_failed_thread_reply(&root, &text, session_revision);
                            if let Some(restored) = ui_ctx.take_failed_thread_reply(&root) {
                                w.global::<crate::ChatPane>()
                                    .set_thread_composer_text(restored.into());
                            }
                            "Couldn't send your reply. Try again."
                        } else {
                            ui_ctx.park_failed_thread_reply(&root, &text, session_revision);
                            if ui_ctx.has_failed_thread_reply(&root) {
                                // Parked behind newer text; it comes back once
                                // the box and the draft are empty.
                                "Couldn't send your reply. It's kept with this thread."
                            } else {
                                "Couldn't send your reply. It's saved as this thread's draft."
                            }
                        };
                        w.set_toast_message(message.into());
                        w.set_toast_variant("error".into());
                        w.set_toast_visible(true);
                    });
                    return;
                }
                if !owner.is_current() {
                    return;
                }
                if let Ok(msg) = sent {
                    ui_ctx
                        .update_thread_reply_count(thread.root_message_id, thread.reply_count + 1);
                    // Show our own reply in the panel immediately — the send path
                    // stores it locally but emits no MessageReceived event, so
                    // without this it only appeared after re-opening the thread.
                    let reply = thread_reply_display(&ui_ctx, &msg);
                    let current_user_id = ui_ctx
                        .session_current_user_id()
                        .map(|id| id.to_string())
                        .unwrap_or_default();
                    let root_id = thread.root_message_id;
                    let _ = slint::invoke_from_event_loop(move || {
                        if !owner.is_current() {
                            return;
                        }
                        if let Some(w) = weak.upgrade() {
                            // The panel cleared its box on tap; clearing it again
                            // here would wipe a reply typed since.
                            apply_live_thread_reply(
                                &w,
                                &root_id,
                                reply.to_slint_with_user(&current_user_id),
                                true,
                            );
                            // The box is free again: an earlier reply that did
                            // not leave comes back into it, never sent on its own.
                            if w.global::<crate::ChatPane>()
                                .get_thread_root_message_id()
                                .as_str()
                                == root_id.to_string()
                                && w.global::<crate::ChatPane>()
                                    .get_thread_composer_text()
                                    .is_empty()
                            {
                                offer_failed_thread_reply(&w, &ui_ctx, &root_id);
                            }
                        }
                    });
                }
            });
        });
}

/// Put the failed reply parked for `root` into the empty thread box and say
/// so. Core moves it only when the thread's draft is empty too; the user
/// still taps send — nothing is resent on its own.
fn offer_failed_thread_reply(w: &MainWindow, ui_ctx: &ChatterClient, root: &MessageId) {
    let Some(text) = ui_ctx.take_failed_thread_reply(root) else {
        return;
    };
    w.global::<crate::ChatPane>()
        .set_thread_composer_text(text.into());
    w.set_toast_message("Your earlier reply that didn't send is back in the box.".into());
    w.set_toast_variant("info".into());
    w.set_toast_visible(true);
}

fn wire_load_more_thread_replies(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window
        .global::<crate::ChatPane>()
        .on_load_more_thread_replies(move || {
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx.clone();
            // Cursor = the oldest reply on screen. Read on the UI thread: a Slint
            // Weak never upgrades inside tokio::spawn.
            let Some(w) = weak.upgrade() else { return };
            let Some(oldest) = w
                .global::<crate::ChatPane>()
                .get_thread_replies()
                .row_data(0)
                .and_then(|row| row.message_id.parse::<MessageId>().ok())
            else {
                return;
            };
            let Some(thread) = ui_ctx.active_thread() else {
                return;
            };
            let root = thread.root_message_id;
            let owner = HistoryOwner::thread(&ui_ctx, root, false);
            tokio::spawn(async move {
                if !owner.is_current() {
                    return;
                }
                let Ok(page) = ui_ctx.list_thread_replies(&root, Some(&oldest), 50) else {
                    return;
                };
                if page.is_empty() {
                    return;
                }
                let display = messages_to_display_batch(
                    &page,
                    |ms| ui_ctx.message_sender_display_names(ms),
                    |m| ui_ctx.message_sender_avatar(m),
                    crate::bin_ui::converters::attachment_display_resolver(&ui_ctx),
                    |m| ui_ctx.reply_author_display_name(m),
                    |m| ui_ctx.reply_parent_missing_for(m),
                    |m| ui_ctx.reply_parent_media(m),
                );
                let current_user_id = ui_ctx
                    .session_current_user_id()
                    .map(|id| id.to_string())
                    .unwrap_or_default();
                let _ = slint::invoke_from_event_loop(move || {
                    if !owner.is_current() {
                        return;
                    }
                    let Some(w) = weak.upgrade() else { return };
                    // The user may have switched threads while this page loaded.
                    if w.global::<crate::ChatPane>().get_thread_root_message_id()
                        != root.to_string().as_str()
                    {
                        return;
                    }
                    let current = w.global::<crate::ChatPane>().get_thread_replies();
                    let mut rows: Vec<MessageItemData> = display
                        .iter()
                        .map(|d| d.to_slint_with_user(&current_user_id))
                        .collect();
                    rows.extend(current.iter());
                    let model = VecModel::from(rows);
                    recompute_channel_continuations(&model);
                    w.global::<crate::ChatPane>()
                        .set_thread_replies(ModelRc::new(model));
                });
            });
        });
}

/// Route a thread reply into the live UI (must run on the UI thread).
///
/// Both halves of "a reply just happened": append it to the thread panel when
/// that thread is the one on screen, and bump the "N replies" pill on the root
/// bubble in the main list. Used by the receive path and by our own send, so a
/// reply shows up without re-navigating the channel.
pub fn apply_live_thread_reply(
    w: &MainWindow,
    root_message_id: &MessageId,
    reply: MessageItemData,
    panel_shows_this_thread: bool,
) {
    if panel_shows_this_thread {
        let replies = w.global::<crate::ChatPane>().get_thread_replies();
        let already_present = replies.iter().any(|m| m.message_id == reply.message_id);
        if !already_present {
            let own_reply = reply.is_own;
            match replies.as_any().downcast_ref::<VecModel<MessageItemData>>() {
                Some(model) => {
                    model.push(reply);
                    recompute_channel_continuations(model);
                }
                None => {
                    let mut all: Vec<MessageItemData> = replies.iter().collect();
                    all.push(reply);
                    let model = VecModel::from(all);
                    recompute_channel_continuations(&model);
                    w.global::<crate::ChatPane>()
                        .set_thread_replies(ModelRc::new(model));
                }
            }
            if own_reply {
                request_thread_tail(w);
            }
            w.global::<crate::ChatPane>()
                .set_thread_reply_count(w.global::<crate::ChatPane>().get_thread_reply_count() + 1);
        }
    }

    bump_root_reply_count(w, root_message_id);
}

/// Presentation intent survives both a model replacement and deferred row layout.
fn request_thread_tail(w: &MainWindow) {
    let pane = w.global::<crate::ChatPane>();
    pane.set_thread_tail_serial(pane.get_thread_tail_serial().wrapping_add(1));
}

/// Patch an edited message into the open thread panel (must run on the UI
/// thread). The panel keeps its own model, so the main-list patch never reaches
/// it; without this an edited reply kept its old text until the thread was
/// reopened. Also refreshes the root preview when the root itself was edited.
/// The caller has already checked the edit is for the conversation on screen.
pub fn apply_thread_edit(
    w: &MainWindow,
    message_id: &str,
    author: &str,
    target_sent_timestamp: u64,
    new_text: &str,
) {
    patch_edited_row(
        &w.global::<crate::ChatPane>().get_thread_replies(),
        message_id,
        author,
        target_sent_timestamp,
        new_text,
    );
    if !message_id.is_empty()
        && w.global::<crate::ChatPane>()
            .get_thread_root_message_id()
            .as_str()
            == message_id
    {
        w.global::<crate::ChatPane>()
            .set_thread_root_content_preview(
                chatter_core::domain::message::truncate_preview_text(new_text, 100).into(),
            );
    }
}

/// Find the edited row — by (author, sent timestamp), the Signal-style key,
/// or by message id only when there is no timestamp — and apply the edit.
/// Only the author's own row matches: a timestamp alone let one member
/// rewrite another's reply by reusing its timestamp. Returns whether a row
/// matched.
fn patch_edited_row(
    rows: &ModelRc<MessageItemData>,
    message_id: &str,
    author: &str,
    target_sent_timestamp: u64,
    new_text: &str,
) -> bool {
    let ts = target_sent_timestamp.to_string();
    let hit = (0..rows.row_count()).find(|&i| {
        rows.row_data(i).is_some_and(|m| {
            m.sender_id.as_str() == author
                && if target_sent_timestamp > 0 {
                    m.timestamp_ns.as_str() == ts
                } else {
                    m.message_id.as_str() == message_id
                }
        })
    });
    let (Some(i), Some(model)) = (
        hit,
        rows.as_any().downcast_ref::<VecModel<MessageItemData>>(),
    ) else {
        return false;
    };
    let Some(mut row) = model.row_data(i) else {
        return false;
    };
    crate::bin_ui::converters::apply_edit(&mut row, new_text);
    model.set_row_data(i, row);
    true
}

/// Increment the "N replies" indicator on a thread root bubble in the main list.
fn bump_root_reply_count(w: &MainWindow, root_message_id: &MessageId) {
    let root_id = root_message_id.to_string();
    let messages = w.global::<crate::ChatPane>().get_messages();
    let Some(model) = messages
        .as_any()
        .downcast_ref::<VecModel<MessageItemData>>()
    else {
        return;
    };
    for i in 0..messages.row_count() {
        let Some(msg) = messages.row_data(i) else {
            continue;
        };
        if msg.message_id.as_str() != root_id {
            continue;
        }
        let mut updated = msg;
        updated.reply_count += 1;
        updated.reply_count_text =
            crate::bin_ui::converters::format_count(updated.reply_count, "reply", "replies").into();
        model.set_row_data(i, updated);
        return;
    }
}

/// Convert a freshly sent thread reply for display in the panel.
///
/// Returns the thread-safe display data — `MessageItemData` holds a
/// `slint::Image` and can't cross the thread hop.
pub fn thread_reply_display(
    ui_ctx: &Arc<ChatterClient>,
    message: &Message,
) -> crate::bin_ui::converters::MessageDisplayData {
    let mut display = messages_to_display_batch(
        std::slice::from_ref(message),
        |ms| ui_ctx.message_sender_display_names(ms),
        |m| ui_ctx.message_sender_avatar(m),
        crate::bin_ui::converters::attachment_display_resolver(&ui_ctx),
        |m| ui_ctx.reply_author_display_name(m),
        |m| ui_ctx.reply_parent_missing_for(m),
        |m| ui_ctx.reply_parent_media(m),
    );
    display.remove(0)
}

pub fn load_thread_data(
    window: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    root_message_id: MessageId,
) {
    let owner = HistoryOwner::thread(&ui_ctx, root_message_id, true);
    tokio::spawn(async move {
        if !owner.is_current() {
            return;
        }
        if let Some(root_message) = ui_ctx.get_message_local(&root_message_id) {
            let sender_name = root_message
                .sender_id
                .as_ref()
                .map(|id| ui_ctx.display_name(id))
                .unwrap_or_else(|| "Anonymous".to_string());
            let content_preview = root_message
                .as_text()
                .map(|t| chatter_core::domain::message::truncate_preview_text(t, 100))
                .unwrap_or_else(|| "[Attachment]".to_string());
            let replies = ui_ctx
                .list_thread_replies(&root_message_id, None, 50)
                .unwrap_or_default();
            // The true total, not the page size: the panel offers "earlier
            // replies" while it shows fewer than this.
            let total_replies = ui_ctx.thread_reply_count(&root_message_id);
            let display = messages_to_display_batch(
                &replies,
                |ms| ui_ctx.message_sender_display_names(ms),
                |m| ui_ctx.message_sender_avatar(m),
                crate::bin_ui::converters::attachment_display_resolver(&ui_ctx),
                |m| ui_ctx.reply_author_display_name(m),
                |m| ui_ctx.reply_parent_missing_for(m),
                |m| ui_ctx.reply_parent_media(m),
            );
            let current_user_id = ui_ctx
                .session_current_user_id()
                .map(|id| id.to_string())
                .unwrap_or_default();
            let draft = ui_ctx.thread_draft(&root_message_id);
            let ctx = ui_ctx.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if !owner.is_current() {
                    return;
                }
                if let Some(w) = window.upgrade() {
                    // Switching threads: what was typed for the one shown
                    // until now stays its draft; this thread's comes back.
                    // Reloading the same thread keeps what is being typed.
                    let root_id = root_message_id.to_string();
                    let switching = w
                        .global::<crate::ChatPane>()
                        .get_thread_root_message_id()
                        .as_str()
                        != root_id;
                    crate::bin_ui::features::composer::autosave_thread_draft(&w, &ctx);
                    w.global::<crate::ChatPane>().set_thread_panel_visible(true);
                    w.global::<crate::ChatPane>()
                        .set_thread_root_message_id(root_id.into());
                    if switching {
                        w.global::<crate::ChatPane>()
                            .set_thread_composer_text(draft.into());
                        // Nothing typed for this thread: a reply that failed
                        // behind newer text, if any, comes back into the box.
                        if w.global::<crate::ChatPane>()
                            .get_thread_composer_text()
                            .is_empty()
                        {
                            offer_failed_thread_reply(&w, &ctx, &root_message_id);
                        }
                    }
                    w.global::<crate::ChatPane>()
                        .set_thread_root_sender_name(sender_name.into());
                    w.global::<crate::ChatPane>()
                        .set_thread_root_content_preview(content_preview.into());
                    w.global::<crate::ChatPane>()
                        .set_thread_reply_count(total_replies as i32);
                    w.global::<crate::ChatPane>().set_thread_loading(false);
                    let slint_replies: Vec<_> = display
                        .iter()
                        .map(|d| d.to_slint_with_user(&current_user_id))
                        .collect();
                    let model = VecModel::from(slint_replies);
                    recompute_channel_continuations(&model);
                    w.global::<crate::ChatPane>()
                        .set_thread_replies(ModelRc::new(model));
                    request_thread_tail(&w);
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, ts: &str, text: &str) -> MessageItemData {
        MessageItemData {
            message_id: id.into(),
            sender_id: "alice".into(),
            timestamp_ns: ts.into(),
            content: text.into(),
            ..Default::default()
        }
    }

    #[test]
    fn edit_patches_the_matching_thread_reply_only() {
        let rows: ModelRc<MessageItemData> = ModelRc::new(VecModel::from(vec![
            row("a", "100", "one"),
            row("b", "200", "two"),
        ]));

        // Another member reusing the reply's timestamp does not rewrite it.
        assert!(!patch_edited_row(&rows, "b", "mallory", 200, "forged"));
        assert_eq!(rows.row_data(1).unwrap().content, "two");

        assert!(patch_edited_row(&rows, "b", "alice", 200, "two edited"));
        assert_eq!(rows.row_data(1).unwrap().content, "two edited");
        assert!(rows.row_data(1).unwrap().is_edited);
        assert_eq!(rows.row_data(0).unwrap().content, "one");

        // Timestamp is the identity: a message-id match on another row is not enough.
        assert!(!patch_edited_row(&rows, "a", "alice", 999, "wrong"));
        assert_eq!(rows.row_data(0).unwrap().content, "one");
    }
}
