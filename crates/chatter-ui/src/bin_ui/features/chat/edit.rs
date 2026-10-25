//! Message editing callbacks for channels, DMs, and anonymous conversations.
//!
//! Handles entering edit mode, sending edits, and cancelling edits.
//! All operations are properly synchronized using Signal-style identification.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use slint::{ComponentHandle, SharedString};

use crate::MainWindow;
use crate::bin_ui::features::history_owner::HistoryOwner;
use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::UserId;
use chatter_core::state::ActiveConversation;

/// The edit session generation. Bumped by every Edit tap and by every way out
/// of an edit (`end_edit`: send, cancel, opening a conversation, sign-out).
/// A tap's text lookup and a send's completion run off the UI thread and
/// apply only while the generation they captured is still current, so two
/// quick taps end on the second message's text, a lookup cancelled before it
/// returned never re-enters edit mode, and an old send that finishes after
/// the same message was reopened for editing leaves the new session alone.
static EDIT_GENERATION: AtomicU64 = AtomicU64::new(0);

fn current_generation() -> u64 {
    EDIT_GENERATION.load(Ordering::SeqCst)
}

fn next_generation() -> u64 {
    EDIT_GENERATION.fetch_add(1, Ordering::SeqCst) + 1
}

/// Wire edit-message callback - enters edit mode by populating composer.
pub fn wire_edit_message(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.global::<crate::ChatPane>().on_edit_message(
        move |sender_id: SharedString, timestamp_ns: SharedString| {
            let sender_id_str = sender_id.to_string();
            let timestamp_ns_str = timestamp_ns.to_string();
            let ui_ctx = ui_ctx.clone();
            let weak = weak_window.clone();

            let conversation = ui_ctx.active_conversation();
            // Session + conversation epoch at the tap: a logout, a reload or
            // another conversation opened meanwhile retires this request.
            let owner = HistoryOwner::capture(&ui_ctx, conversation);
            let generation = next_generation();

            // Move service lookups to background task
            tokio::spawn(async move {
                if !owner.is_current() {
                    return;
                }
                let target_sender_id: UserId = match sender_id_str.parse() {
                    Ok(id) => id,
                    Err(_) => return,
                };

                let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                    Ok(ts) => ts,
                    Err(_) => return,
                };

                // Get message text via ConversationHandle (polymorphic across all types)
                let handle = ui_ctx.conversation(conversation);
                let text = match handle.get_message_text(&target_sender_id, target_sent_timestamp) {
                    Ok(t) => t,
                    Err(_) => return,
                };

                if let Some(text) = text {
                    let composite_key = format!("{}:{}", sender_id_str, timestamp_ns_str);

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            // Still the same session/conversation and still the
                            // newest edit session: otherwise this text would
                            // replace a newer draft or re-enter a cancelled edit.
                            if !owner.is_current() || current_generation() != generation {
                                return;
                            }
                            // Editing borrows the composer; keep the unsent draft
                            // (it used to be silently replaced and lost).
                            if !w.global::<crate::ChatPane>().get_composer_stash_held() {
                                w.global::<crate::ChatPane>().set_composer_stash(
                                    w.global::<crate::ChatPane>().get_composer_text(),
                                );
                                w.global::<crate::ChatPane>().set_composer_stash_held(true);
                            }
                            w.global::<crate::ChatPane>().set_composer_text(text.into());
                            // Android: the composer keeps its IME client across
                            // the message menu (keyboard stays up), so the
                            // message text replaces the draft at the generation
                            // the IME is still reporting under; a snapshot of
                            // the old draft landing after this line put the
                            // draft back under the edit banner (RC89, S9). The
                            // replacement is a new app state of the field: new
                            // generation, pushed now.
                            #[cfg(target_os = "android")]
                            crate::platform::android::text_input::restore();
                            w.global::<crate::ChatPane>()
                                .set_composer_edit_message_id(composite_key.into());
                            w.global::<crate::ChatPane>().set_composer_is_editing(true);
                            tracing::debug!(
                                sender_id = %sender_id_str,
                                timestamp = %timestamp_ns_str,
                                "Entered edit mode (Signal-style)"
                            );
                        }
                    });
                }
            });
        },
    );
}

/// Wire send-edit callback - sends the edited message content.
///
/// Uses Signal-style (sender_id, timestamp_ns) stored in composer_edit_message_id as "sender_id:timestamp_ns".
pub fn wire_send_edit(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    window.global::<crate::ChatPane>().on_send_edit(
        move |composite_key: SharedString, new_text: SharedString| {
            let composite_key_str = composite_key.to_string();
            let new_text_str = new_text.to_string();

            // Parse composite key "sender_id:timestamp_ns"
            let parts: Vec<&str> = composite_key_str.split(':').collect();
            if parts.len() != 2 {
                tracing::error!("Invalid composite key for edit: {}", composite_key_str);
                return;
            }
            let sender_id_str = parts[0].to_string();
            let timestamp_ns_str = parts[1].to_string();

            let conversation = ui_ctx.active_conversation();
            if matches!(conversation, ActiveConversation::None) {
                return;
            }

            let weak = weak_window.clone();
            let ui_ctx = ui_ctx.clone();
            let owner = HistoryOwner::capture(&ui_ctx, conversation);
            let generation = current_generation();

            let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                Ok(ts) => ts,
                Err(_) => return,
            };

            // The author is part of the key; core refuses any author but us.
            let target_author: UserId = match sender_id_str.parse() {
                Ok(id) => id,
                Err(_) => return,
            };

            // The edit is accepted: take the text out of the composer now, on
            // this thread, in the order a normal send does (`take_composer_send`):
            // clear the field, then let the Android input method drop its own
            // copy (the consume is a no-op while the field still holds text).
            // Slint's `reset-after-send` right after this callback is then an
            // empty reset, and the old text cannot come back into the box.
            if let Some(w) = weak.upgrade() {
                w.global::<crate::ChatPane>().set_composer_text("".into());
            }
            #[cfg(target_os = "android")]
            crate::platform::android::text_input::consume();

            tokio::spawn(async move {
                // The tap bound this edit to its session and conversation; a
                // logout or a switch before the task ran retires it.
                if !owner.is_current() {
                    return;
                }
                let handle = ui_ctx.conversation(conversation);
                if let Err(e) = handle
                    .send_edit(&target_author, target_sent_timestamp, &new_text_str)
                    .await
                {
                    tracing::error!(error = %e, "Failed to send edit");
                    // The text was taken out of the composer when the send was
                    // accepted; give it back to this same, still-empty edit
                    // session so it can be retried, and say why. A session that
                    // has since ended (sign-out, navigation, another edit) gets
                    // neither the text nor the toast.
                    let message = e.to_string();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            if !owner.is_current()
                                || current_generation() != generation
                                || w.global::<crate::ChatPane>()
                                    .get_composer_edit_message_id()
                                    .as_str()
                                    != composite_key_str
                            {
                                return;
                            }
                            if w.global::<crate::ChatPane>().get_composer_text().is_empty() {
                                let pane = w.global::<crate::ChatPane>();
                                pane.set_composer_text(new_text_str.into());
                                pane.set_composer_caret_end_seq(pane.get_composer_caret_end_seq() + 1);
                                #[cfg(target_os = "android")]
                                crate::platform::android::text_input::restore();
                            }
                            w.set_toast_message(format!("Edit not saved: {message}").into());
                            w.set_toast_variant("error".into());
                            w.set_toast_visible(true);
                        }
                    });
                    return;
                }
                // Core emits `DomainEvent::MessageEdited` after persisting, so the
                // existing domain-handler dispatcher refreshes the UI — nothing
                // conversation-kind-specific needed here.

                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        // Only this edit's own session may be closed: the same
                        // session/conversation, edit generation and edit key.
                        // The composer was emptied when this edit was sent;
                        // text typed since is a newer draft and stays, so the
                        // stash is not restored over it.
                        if !owner.is_current()
                            || current_generation() != generation
                            || w.global::<crate::ChatPane>()
                                .get_composer_edit_message_id()
                                .as_str()
                                != composite_key_str
                        {
                            return;
                        }
                        if w.global::<crate::ChatPane>().get_composer_text().is_empty() {
                            end_edit(&w);
                        } else {
                            next_generation();
                            w.global::<crate::ChatPane>().set_composer_stash("".into());
                            w.global::<crate::ChatPane>().set_composer_stash_held(false);
                            w.global::<crate::ChatPane>()
                                .set_composer_edit_message_id("".into());
                            w.global::<crate::ChatPane>().set_composer_is_editing(false);
                        }
                    }
                });
            });
        },
    );
}

/// Wire cancel-edit callback - exits edit mode without sending.
pub fn wire_cancel_edit(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.global::<crate::ChatPane>().on_cancel_edit(move || {
        let weak = weak_window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                end_edit(&w);
                tracing::debug!("Cancelled edit mode");
            }
        });
    });
}

/// Leave edit mode and give the composer back the draft that editing borrowed
/// it from. The one way out of an edit: sending it, cancelling it, and opening
/// a conversation (even the same one again, e.g. Back then re-tap on a phone,
/// which never changes the active conversation) all go through here. Clearing
/// only the flags left the edit text in the box as a plain draft, and Send
/// then posted it as a new message.
pub(crate) fn end_edit(w: &MainWindow) {
    next_generation();
    if w.global::<crate::ChatPane>().get_composer_stash_held() {
        let pane = w.global::<crate::ChatPane>();
        pane.set_composer_text(pane.get_composer_stash());
        pane.set_composer_caret_end_seq(pane.get_composer_caret_end_seq() + 1);
        w.global::<crate::ChatPane>().set_composer_stash_held(false);
        // Android: the accepted edit consumed the input method's copy of
        // the composer; the draft going back in is a newer state of the
        // field than the emptied one the IME may still be reporting.
        #[cfg(target_os = "android")]
        crate::platform::android::text_input::restore();
    }
    w.global::<crate::ChatPane>().set_composer_stash("".into());
    w.global::<crate::ChatPane>()
        .set_composer_edit_message_id("".into());
    w.global::<crate::ChatPane>().set_composer_is_editing(false);
}

/// Apply a persisted edit without changing message or pin identity/order.
#[doc(hidden)]
pub fn apply_message_edit_in_conversation(
    w: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    conversation: ActiveConversation,
    message_id: &str,
    author: &str,
    timestamp: u64,
    new_text: &str,
) {
    use slint::{Model, ModelRc, VecModel};
    if ui_ctx.active_conversation() != conversation {
        return;
    }
    let mut pin_id = message_id.to_string();
    let mut preview_row = crate::MessageItemData::default();
    let kind = ui_ctx.conversation_type_of(&conversation);
    let members = kind.as_ref().zip(conversation.channel_id())
        .map(|(kind, id)| ui_ctx.mention_members(kind, &id)).unwrap_or_default();
    let broadcast = matches!(kind, Some(chatter_core::domain::message::ConversationType::Channel | chatter_core::domain::message::ConversationType::AnonGroup));
    crate::bin_ui::converters::apply_edit(&mut preview_row, new_text, &members, broadcast);
    super::edit_messages(w, |model| {
        for i in 0..model.row_count() {
            let Some(mut row) = model.row_data(i) else {
                continue;
            };
            if row.sender_id.as_str() == author
                && row.timestamp_ns.as_str() == timestamp.to_string()
            {
                pin_id = row.message_id.to_string();
                crate::bin_ui::converters::apply_edit(&mut row, new_text, &members, broadcast);
                preview_row = row.clone();
                model.set_row_data(i, row);
                break;
            }
        }
    });
    let preview =
        crate::bin_ui::features::auth::inbox::utils::generate_preview_from_ui_message(&preview_row);
    let mut pins: Vec<_> = w
        .global::<crate::ChatPane>()
        .get_pinned_items()
        .iter()
        .collect();
    for pin in &mut pins {
        if pin.message_id.as_str() == pin_id {
            pin.preview = preview.clone().into();
        }
    }
    w.global::<crate::ChatPane>()
        .set_pinned_items(ModelRc::new(VecModel::from(pins)));
    if ui_ctx
        .pinned_current_message_id()
        .is_some_and(|id| id.to_string() == pin_id)
    {
        w.global::<crate::ChatPane>()
            .set_pinned_message_preview(preview.into());
    }
}
