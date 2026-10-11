//! Send message callback for channels and DMs.
//!
//! All conversation types follow the same pipeline:
//! 1. Auth check + user info
//! 2. Upload attachments (all types pre-upload)
//! 3. Build quote context
//! 4. Build OutgoingIntent per conversation type
//! 5. Route through gateway
//! 6. Handle result + update UI

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::{ChatterClient, ComposerReply, ComposerSend};
use chatter_core::services::gateway;
use chatter_core::state::ActiveConversation;

use super::typing::clear_outgoing_typing;
use crate::MainWindow;
use crate::bin_ui::converters::attachment_to_display_data;

/// Composer placeholder while a Shield key exchange holds the sent messages.
/// Only the bootstrap's own settlement may clear it, and only it.
pub(crate) const BOOTSTRAP_PLACEHOLDER: &str = "Setting up secure channel...";

/// The row a send occupies in the open list while it is on the wire.
///
/// The composer is cleared the instant send is tapped and the message is not
/// stored until the round trip returns — a window of up to 10s + 30s per
/// attachment. This row is the evidence that something is happening: the text
/// and the staged files as they will look, with `delivery-status` "sending",
/// which the bubble draws as the word plus a thin progress line. It claims
/// nothing about arrival; a send that fails removes it and puts the draft
/// back, and a send that lands replaces it in place with the stored message.
///
/// It used to be a label in the composer. That label grew the composer by a
/// row on every send and shrank it again on the append, and on a phone that
/// geometry change landed under the soft keyboard, so the composer visibly
/// jumped twice per message.
fn pending_message_id(send: &ComposerSend) -> String {
    format!("pending:{}", send.attempt_id)
}

/// Put the pending row for `send` into the open list, if that is the
/// conversation it is going to.
///
/// One enqueue, made before the network attempt starts, and nothing after it:
/// every later change to this row (the stored message replacing it, or its
/// removal on failure) is enqueued after this one, from the same task, once an
/// await has returned. `invoke_from_event_loop` is a FIFO on every backend we
/// ship (winit: `EventLoopProxy::send_event`; android-activity: a locked
/// `Vec` push), so the row is always there before it is replaced or removed.
/// A decode-then-insert callback would not have that property: it could run
/// after the removal and leave a "sending" row behind for good.
///
/// Thumbnails: `to_slint` loads an image by its local path through Slint's
/// image cache, which the attachment tray filled when the file was staged,
/// so the common case decodes nothing here.
pub(crate) fn show_pending_bubble(
    weak: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    send: &ComposerSend,
    user_id: &str,
    user_name: &str,
) {
    let attachments: Vec<_> = send
        .attachments
        .iter()
        .map(pending_attachment_to_display_data)
        .collect();
    let (reply_sender_name, reply_preview) = send
        .reply
        .as_ref()
        .map(|r| (r.sender_name.clone(), r.preview.clone()))
        .unwrap_or_default();
    let reply_thumbnail = send.reply.as_ref().and_then(|r| r.thumbnail.clone());
    let row = pending_message_id(send);
    let text = send.text.clone();
    let conversation = send.conversation;
    let session_revision = send.session_revision;
    let user_id = user_id.to_string();
    let user_name = user_name.to_string();
    let weak = weak.clone();
    let ui_ctx = ui_ctx.clone();
    // The row's own clock, not 0: the live-receive path orders rows by
    // (timestamp, sender) and a 0 at the tail would mislead it. No stored
    // message carries this exact value, so a lookup by (me, this) finds
    // nothing, which is what a context action on a pending row should do.
    let now = chrono::Utc::now();
    let now_ns = now.timestamp_nanos_opt().unwrap_or_default().max(0) as u64;
    let _ = slint::invoke_from_event_loop(move || {
        if !ui_ctx.composer_session_is_current(session_revision)
            || ui_ctx.chat_state().active_conversation() != conversation
        {
            return;
        }
        let Some(w) = weak.upgrade() else { return };
        let (thumb, has_thumb, thumb_w, thumb_h) = reply_thumbnail
            .map(|(rgba, tw, th)| {
                (
                    crate::bin_ui::slint_conversions::rgba_to_slint_image(&rgba, tw, th),
                    true,
                    tw as i32,
                    th as i32,
                )
            })
            .unwrap_or_else(|| (slint::Image::default(), false, 0, 0));
        let item = super::helpers::build_own_message_item(
            &row,
            &user_id,
            &user_name,
            &text,
            &chatter_core::ui::chat_data::format_timestamp(now),
            now_ns,
            attachments.iter().map(|a| a.to_slint()).collect(),
            !reply_sender_name.is_empty(),
            &reply_sender_name,
            &reply_preview,
            thumb,
            has_thumb,
            thumb_w,
            thumb_h,
            &Default::default(),
            "sending",
        );
        upsert_message_in_ui(&w, &row, item);
    });
}

/// Replace the row `pending_id` with `item`, or append `item` when the row is
/// not there (the list was reloaded by a conversation switch meanwhile). If
/// `item`'s own id is already in the list — the server echo or a sync landed
/// it through the receive path first — the pending row is dropped instead,
/// so the message is never shown twice.
fn upsert_message_in_ui(w: &MainWindow, pending_id: &str, item: crate::MessageItemData) {
    use slint::Model;
    let mut item = Some(item);
    let mut drop_pending = false;
    super::helpers::edit_messages(w, |model| {
        let row_of = |id: &str| {
            (0..model.row_count()).rev().find(|&i| {
                model
                    .row_data(i)
                    .is_some_and(|m| m.message_id.as_str() == id)
            })
        };
        let stored_id = item
            .as_ref()
            .map(|m| m.message_id.to_string())
            .unwrap_or_default();
        let already_shown = stored_id != pending_id && row_of(&stored_id).is_some();
        match (row_of(pending_id), already_shown) {
            (Some(_), true) => {
                drop_pending = true;
                item = None;
            }
            (Some(i), false) => model.set_row_data(i, item.take().expect("set once")),
            (None, true) => item = None,
            (None, false) => {}
        }
    });
    if drop_pending {
        // Through the shared removal so a selection on the row is released.
        super::helpers::remove_messages_where(w, |m| m.message_id.as_str() == pending_id);
    }
    if let Some(item) = item {
        // `append_message_to_ui` clears the composer; the next draft the
        // user has started typing meanwhile is theirs to keep.
        let draft = w.get_composer_text();
        super::helpers::append_message_to_ui(w, item);
        w.set_composer_text(draft);
    }
    w.set_is_at_bottom(true);
}

/// Take the pending row for `send` out of the open list: the send did not go.
pub(crate) fn remove_pending_bubble(w: &MainWindow, send: &ComposerSend) {
    let pending_id = pending_message_id(send);
    super::helpers::remove_messages_where(w, |m| m.message_id.as_str() == pending_id);
}

/// The pending row of a send core took but could not confirm.
fn mark_pending_unconfirmed(w: &MainWindow, send: &ComposerSend) {
    use slint::Model;
    let pending_id = pending_message_id(send);
    let messages = w.get_messages();
    for i in 0..messages.row_count() {
        if let Some(mut m) = messages.row_data(i)
            && m.message_id.as_str() == pending_id
        {
            m.delivery_status = "unconfirmed".into();
            messages.set_row_data(i, m);
        }
    }
}

/// A staged file as the pending row shows it: the local file stands in for
/// the stored attachment until the send returns one.
fn pending_attachment_to_display_data(
    attachment: &chatter_core::state::PendingAttachment,
) -> crate::bin_ui::converters::AttachmentDisplayData {
    use crate::bin_ui::converters::{
        category_to_icon_and_color, format_file_size, mime_to_category,
    };
    use chatter_core::domain::attachment::FileCategory;
    let category = mime_to_category(&attachment.mime_type);
    let (icon, icon_color) = category_to_icon_and_color(category);
    crate::bin_ui::converters::AttachmentDisplayData {
        id: attachment.id.to_string(),
        filename: attachment.filename.clone(),
        size_display: format_file_size(attachment.size),
        mime_type: attachment.mime_type.clone(),
        category: category.to_string(),
        thumbnail_url: String::new(),
        download_url: String::new(),
        local_path: Some(attachment.path.to_string_lossy().into_owned()),
        is_image: attachment.mime_type.starts_with("image/"),
        is_video: FileCategory::from_mime_and_filename(&attachment.mime_type, &attachment.filename)
            == FileCategory::Video,
        width: 0,
        height: 0,
        is_playing: false,
        icon: icon.to_string(),
        icon_color,
        waveform: vec![10; 32],
        duration_display: String::new(),
        is_uploading: false,
        is_downloading: false,
        progress: 0.0,
        thumbnail_bytes: Vec::new(),
        thumbnail_pixels: None,
    }
}

/// Tell the user a send did not happen.
///
/// Every early return in the send pipeline has already cleared the composer, the
/// reply context and the staged attachments — which is fine when the message went
/// out, and silent data loss when it did not. This is not optional politeness:
/// with the server unreachable, a failed send is the *expected* outcome, and it
/// has to be visible.
///
/// The message text itself comes from chatter-core (`format_send_error`); this
/// only carries it to the surface.
fn report_send_failure(
    weak: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    session_revision: u64,
    message: String,
) {
    let weak = weak.clone();
    let ui_ctx = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if !ui_ctx.composer_session_is_current(session_revision) {
            return;
        }
        let Some(w) = weak.upgrade() else { return };
        w.set_toast_message(message.into());
        w.set_toast_variant("error".into());
        w.set_toast_visible(true);
        w.global::<crate::Haptics>().invoke_error();
    });
}

/// A plain toast about a send still in flight (not an error: nothing failed).
fn notice(
    weak: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    session_revision: u64,
    message: &str,
) {
    let weak = weak.clone();
    let ui_ctx = ui_ctx.clone();
    let message = message.to_string();
    let _ = slint::invoke_from_event_loop(move || {
        if !ui_ctx.composer_session_is_current(session_revision) {
            return;
        }
        if let Some(w) = weak.upgrade() {
            w.set_toast_message(message.into());
            w.set_toast_variant("info".into());
            w.set_toast_visible(true);
        }
    });
}

/// Show `ChatterClient::channel_send_notice` in place of the composer for
/// `channel_id`: the stored answer at once, then the server's.
pub fn refresh_send_notice(
    weak: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    channel_id: chatter_core::domain::ids::ChannelId,
) {
    let weak = weak.clone();
    let ui_ctx = ui_ctx.clone();
    tokio::spawn(async move {
        set_send_notice(
            &weak,
            &ui_ctx,
            channel_id,
            ui_ctx.channel_send_notice(&channel_id),
        );
        let notice = ui_ctx.refresh_channel_send_notice(&channel_id).await;
        set_send_notice(&weak, &ui_ctx, channel_id, notice);
    });
}

/// Re-render the notice from the stored membership, e.g. after a live
/// `MemberUpdated` (mute, unmute, mute expiry) already refreshed it.
pub fn show_send_notice(
    weak: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    channel_id: chatter_core::domain::ids::ChannelId,
) {
    set_send_notice(
        weak,
        ui_ctx,
        channel_id,
        ui_ctx.channel_send_notice(&channel_id),
    );
}

/// Apply a notice only while its channel is still the one on screen.
fn set_send_notice(
    weak: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    channel_id: chatter_core::domain::ids::ChannelId,
    notice: String,
) {
    if ui_ctx.chat_state().get().active_channel_id != Some(channel_id) {
        return;
    }
    let read_only = ui_ctx.is_former_channel(&channel_id);
    // The membership banner and public badge travel with the notice: both say
    // whether (and why) this channel can be posted to.
    let membership = ui_ctx.channel_membership_header(&channel_id);
    let answer = ui_ctx
        .secret_chat_gate(&chatter_core::domain::anon_dm::AnonDmId::from_uuid(
            *channel_id.as_uuid(),
        ))
        .is_some_and(|g| g.offers_answer());
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.set_composer_notice(notice.into());
            w.set_secret_request_actions(answer);
            w.set_conversation_read_only(read_only);
            super::membership::apply_header(&w, &membership);
        }
    });
}

/// Every conversation switch goes through `ChatState` (see the draft watcher
/// in `features::composer`): clear the notice on leaving, recompute it for a
/// channel on entering. Live mute changes arrive as `MemberUpdated`
/// ([`show_send_notice`]); the open re-reads the roster in case one was missed.
fn wire_send_notice(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let mut current = ui_ctx.chat_state().get().active_channel_id;
    let mut rx = ui_ctx.chat_state().subscribe();
    let weak = window.as_weak();
    let ctx = ui_ctx.clone();
    tokio::spawn(async move {
        loop {
            let snapshot = match rx.recv().await {
                Ok(snap) => snap,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            if snapshot.active_channel_id == current {
                continue;
            }
            current = snapshot.active_channel_id;
            let weak_clear = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_clear.upgrade() {
                    w.set_composer_notice("".into());
                    w.set_secret_request_actions(false);
                    w.set_conversation_read_only(false);
                    super::membership::apply_header(&w, &Default::default());
                }
            });
            if let Some(channel_id) = current {
                refresh_send_notice(&weak, &ctx, channel_id);
            }
        }
    });
}

pub fn wire_send(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_send_notice(window, ui_ctx);
    let chat_state = ui_ctx.chat_state().clone();
    let ui_ctx_jarvis = ui_ctx.clone();

    let weak_window = window.as_weak();

    window.on_send(move |text: slint::SharedString| {
        let send_started = std::time::Instant::now();
        let text = text.to_string();
        let pending_attachments = chat_state.get_pending_attachments();
        let has_attachments = !pending_attachments.is_empty();

        if text.trim().is_empty() && !has_attachments {
            return false;
        }

        // Mesh conversation open: serverless, so it must not reach the gateway
        // pipeline — or the Jarvis interceptions below, which would answer a
        // slash command instead of broadcasting it. Core picks the transport
        // (public room / geohash cell / peer).
        #[cfg(feature = "mesh")]
        if crate::bin_ui::features::mesh::intercept_send(&ui_ctx_jarvis, &text, weak_window.clone())
        {
            return true;
        }

        // Local AI channel: every send routes to Jarvis, nothing networks.
        #[cfg(feature = "jarvis")]
        if let Some(id) = chat_state.get().active_channel_id {
            if chatter_core::client::is_jarvis_channel(&id) {
                if text.trim().is_empty() {
                    return false;
                }
                let ui_ctx = ui_ctx_jarvis.clone();
                let weak = weak_window.clone();
                tokio::spawn(async move {
                    crate::bin_ui::features::jarvis::handle_jarvis_channel_send(ui_ctx, text, weak)
                        .await;
                });
                return true;
            }
        }

        // Check for Jarvis slash commands first (/ask, /summarize, /knowledge, /remember)
        #[cfg(feature = "jarvis")]
        if let Some(cmd) = crate::bin_ui::features::jarvis::parse_slash_command(&text) {
            let ui_ctx = ui_ctx_jarvis.clone();
            let channel_id = chat_state.get().active_channel_id;
            let weak = weak_window.clone();
            tokio::spawn(async move {
                handle_jarvis_slash_command(&ui_ctx, cmd, channel_id.as_ref(), weak).await;
            });
            return true;
        }

        // @Jarvis request? Core decides: a whole-word mention in a
        // conversation where AI is on. Anything else is sent as written.
        #[cfg(feature = "jarvis")]
        if let Some((context, query)) =
            ui_ctx_jarvis.jarvis_mention(&chat_state.active_conversation(), &text)
        {
            let ui_ctx = ui_ctx_jarvis.clone();
            let weak = weak_window.clone();
            tokio::spawn(async move {
                crate::bin_ui::features::jarvis::handle_jarvis_mention(
                    &ui_ctx, &query, context, weak,
                )
                .await;
            });
            return true;
        }

        let Some(w) = weak_window.upgrade() else { return false };
        let send = match ui_ctx_jarvis.begin_composer_send(text, composer_reply(&w)) {
            Ok(send) => send,
            Err(message) => {
                w.set_toast_message(message.into());
                w.set_toast_variant("error".into());
                w.set_toast_visible(true);
                return false;
            }
        };
        // The composer clears itself once this returns true; clear it here
        // first so the persisted draft goes with the accepted send now, not
        // after the autosave delay (see `take_composer_send`).
        w.set_composer_text("".into());
        // Android: the IME's own copy of the composer is consumed as of now,
        // in order with the keyboard's operations (the producer gate), so
        // nothing it still holds of the sent text can come back.
        #[cfg(target_os = "android")]
        crate::platform::android::text_input::consume();
        if send.reply.is_some() {
            clear_reply(&w);
        }
        crate::bin_ui::features::composer::render_attachments(&w, &[]);
        crate::bin_ui::features::composer::autosave_composer_draft(&w, &ui_ctx_jarvis);

        // Clear outgoing typing state (Signal: clearTypingTimers on message send)
        clear_outgoing_typing(&ui_ctx_jarvis);

        // Track user activity for presence updates
        {
            let ui_ctx = ui_ctx_jarvis.clone();
            tokio::spawn(async move {
                crate::bin_ui::features::auth::presence_integration::track_activity(&ui_ctx).await;
            });
        }

        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_jarvis.clone();

        tokio::spawn(async move {
            let frozen = send.clone();
            if !deliver(weak.clone(), ui_ctx.clone(), send, send_started).await {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        recover_failed_send(&w, &ui_ctx, frozen);
                    }
                });
            }
        });
        true
    });
}

/// Recover a definite failure into its own empty composer, or park it there
/// for later. Expired sessions and newer input are handled by core.
pub fn recover_failed_send(w: &MainWindow, ui_ctx: &ChatterClient, send: ComposerSend) {
    if let Some(send) = ui_ctx.recover_composer_send(send, composer_is_empty(w)) {
        restore_composer(w, ui_ctx, send);
    }
}

/// The reply bar's target, if the composer is replying.
pub fn composer_reply(w: &MainWindow) -> Option<ComposerReply> {
    if !w.get_composer_is_replying() {
        return None;
    }
    let reply = ComposerReply {
        sender_id: w.get_composer_reply_to_sender().to_string(),
        timestamp: w.get_composer_reply_to_timestamp().to_string(),
        sender_name: w.get_composer_reply_to_sender_name().to_string(),
        preview: w.get_composer_reply_preview().to_string(),
        thumbnail: w
            .get_composer_reply_has_thumbnail()
            .then(|| w.get_composer_reply_thumbnail().to_rgba8())
            .flatten()
            .map(|pixels| (pixels.as_bytes().to_vec(), pixels.width(), pixels.height())),
    };
    (!reply.sender_id.is_empty() && !reply.timestamp.is_empty()).then_some(reply)
}

pub(crate) fn clear_reply(w: &MainWindow) {
    w.set_composer_is_replying(false);
    w.set_composer_reply_to_sender("".into());
    w.set_composer_reply_to_sender_name("".into());
    w.set_composer_reply_to_timestamp("".into());
    w.set_composer_reply_preview("".into());
    w.set_composer_reply_has_thumbnail(false);
    w.set_composer_reply_thumbnail(slint::Image::default());
    w.set_composer_placeholder("Type a message...".into());
    w.invoke_composer_draft_changed();
}

/// Whether the composer holds nothing a restored send could overwrite.
/// A voice note being recorded counts: its send would take restored files.
pub fn composer_is_empty(w: &MainWindow) -> bool {
    let voice = w.global::<crate::VoiceRecording>();
    w.get_composer_text().is_empty()
        && !w.invoke_composer_has_preedit()
        && !w.get_composer_is_replying()
        && !w.get_composer_is_editing()
        && !voice.get_active()
        && !voice.get_busy()
}

/// Freeze the composer into a send (core decides whether it may go) and
/// clear what it took. `Err` is what to tell the user; nothing was consumed.
pub fn take_composer_send(w: &MainWindow, ui_ctx: &ChatterClient) -> Result<ComposerSend, String> {
    if w.get_composer_is_editing() {
        return Err("Finish editing first.".into());
    }
    w.invoke_settle_composer();
    let send = ui_ctx.begin_composer_send(w.get_composer_text().to_string(), composer_reply(w))?;
    w.set_composer_text("".into());
    #[cfg(target_os = "android")]
    crate::platform::android::text_input::consume();
    if send.reply.is_some() {
        clear_reply(w);
    }
    crate::bin_ui::features::composer::render_attachments(w, &[]);
    // The accepted send took the draft: its persisted copy goes with it now,
    // not after the autosave delay, so a restart cannot resurrect sent text.
    crate::bin_ui::features::composer::autosave_composer_draft(w, ui_ctx);
    Ok(send)
}

/// Show `reply` in the composer's reply bar.
pub(crate) fn show_reply(w: &MainWindow, reply: ComposerReply) {
    let thumbnail = reply.thumbnail.as_ref().map(|(bytes, width, height)| {
        slint::Image::from_rgba8(
            slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(bytes, *width, *height),
        )
    });
    show_reply_image(w, reply, thumbnail);
}

/// The one way into reply mode: every reply-bar setter goes through here so
/// the draft autosave sees the new target. `thumbnail` is the already decoded
/// image when the caller has one (a bubble's attachment); `reply.thumbnail`
/// is ignored.
pub(crate) fn show_reply_image(
    w: &MainWindow,
    reply: ComposerReply,
    thumbnail: Option<slint::Image>,
) {
    w.set_composer_reply_to_sender(reply.sender_id.into());
    w.set_composer_reply_to_timestamp(reply.timestamp.into());
    w.set_composer_reply_to_sender_name(reply.sender_name.into());
    w.set_composer_reply_preview(reply.preview.into());
    w.set_composer_reply_has_thumbnail(thumbnail.is_some());
    w.set_composer_reply_thumbnail(thumbnail.unwrap_or_default());
    w.set_composer_placeholder("Type your reply...".into());
    w.set_composer_is_replying(true);
    w.invoke_composer_draft_changed();
}

/// Put a failed send back into the (empty) composer it came from.
pub fn restore_composer(w: &MainWindow, ui_ctx: &ChatterClient, send: ComposerSend) {
    let chat = ui_ctx.chat_state().get();
    if !ui_ctx.composer_session_is_current(send.session_revision)
        || chat.active_conversation != send.conversation
    {
        return;
    }
    w.set_composer_text(send.text.into());
    // Android: the accepted send consumed the IME's copy; the text going
    // back in is a newer state than the emptied one it may still report.
    #[cfg(target_os = "android")]
    crate::platform::android::text_input::restore();
    if let Some(reply) = send.reply {
        show_reply(w, reply);
    }
    for attachment in send.attachments {
        ui_ctx.chat_state().add_pending_attachment_for(
            send.conversation,
            chat.attachment_revision,
            attachment,
        );
    }
    crate::bin_ui::features::composer::render_attachments(
        w,
        &ui_ctx.chat_state().get_pending_attachments(),
    );
}

/// Send `send` to the conversation it names. `true` once core stored it
/// (confirmed, or retained as unconfirmed without retry); `false` when it was not
/// sent at all, the failure already reported.
pub async fn deliver(
    weak: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    send: ComposerSend,
    send_started: std::time::Instant,
) -> bool {
    let frozen = send.clone();
    let sent = deliver_inner(weak.clone(), ui_ctx.clone(), send, send_started).await;
    if !sent {
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                remove_pending_bubble(&w, &frozen);
            }
        });
    }
    sent
}

async fn deliver_inner(
    weak: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    send: ComposerSend,
    send_started: std::time::Instant,
) -> bool {
    if !ui_ctx.composer_session_is_current(send.session_revision) {
        return false;
    }
    let send_for_delivery = send.clone();
    let ComposerSend {
        attempt_id: _,
        session_revision,
        conversation,
        text,
        reply,
        attachments: pending_attachments,
    } = send;
    let auth_state = ui_ctx.auth_state();
    if !auth_state.is_logged_in() {
        return false;
    }
    let current_user_id = auth_state
        .get()
        .current_user_id
        .map(|id| id.to_string())
        .unwrap_or_default();
    let current_user_name = auth_state
        .get()
        .display_name
        .unwrap_or_else(|| "Me".to_string());
    let pending_id = pending_message_id(&send_for_delivery);
    show_pending_bubble(
        &weak,
        &ui_ctx,
        &send_for_delivery,
        &current_user_id,
        &current_user_name,
    );

    // Quote reference — preview text is filled in by core.
    let is_reply = reply.is_some();
    let (reply_sender_name, reply_preview_text) = reply
        .as_ref()
        .map(|r| (r.sender_name.clone(), r.preview.clone()))
        .unwrap_or_default();
    let quote = match &reply {
        Some(r) => {
            let parsed = match super::helpers::parse_reply_context(&r.sender_id, &r.timestamp) {
                Some(p) => p,
                None => return false,
            };
            Some(gateway::QuoteIntent {
                target_author_id: parsed.sender_id,
                target_timestamp: parsed.timestamp,
                target_preview: None,
            })
        }
        None => None,
    };

    // Scale timeout for attachments: base 10s + 30s per attachment.
    let send_timeout = if matches!(conversation, ActiveConversation::Channel(_)) {
        std::time::Duration::from_secs(35 + 30 * pending_attachments.len() as u64)
    } else if pending_attachments.is_empty() {
        crate::bin_ui::ui_helpers::UI_SERVICE_TIMEOUT
    } else {
        std::time::Duration::from_secs(10 + 30 * pending_attachments.len() as u64)
    };

    tracing::info!(
        ?conversation,
        text_len = text.len(),
        "[UI_SEND] entering send_message_ui"
    );
    let outcome = chatter_core::client::await_send_with_notice(
        ui_ctx.send_composer_message(&send_for_delivery, quote),
        send_timeout,
        || {
            // The pending row keeps showing "sending"; this says why it is
            // taking so long, once.
            notice(
                &weak,
                &ui_ctx,
                session_revision,
                "Delivery is taking longer. Waiting for the original attempt; do not resend.",
            )
        },
    )
    .await;
    if !ui_ctx.composer_session_is_current(session_revision) {
        return false;
    }
    let ui_result = match outcome {
        None => return false,
        Some(Ok(r)) => {
            tracing::info!(msg_id = %r.message_id_effective, sent_at = r.sent_at, "[UI_SEND] send_message_ui returned Ok");
            // Core stored the message with its uploads: the sheltered copies
            // behind the attachments are done. A user's original is untouched.
            ui_ctx.release_staged_files(&pending_attachments);
            r
        }
        Some(Err(e)) => {
            tracing::error!(error = %e, "[UI_SEND] send_message_ui returned Err");
            if matches!(&e, gateway::SendError::SendInProgress) {
                let weak = weak.clone();
                let ui_ctx = ui_ctx.clone();
                let frozen = send_for_delivery.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if !ui_ctx.composer_session_is_current(frozen.session_revision) {
                        return;
                    }
                    if let Some(w) = weak.upgrade() {
                        // Not taken: the earlier attempt still owns the wire.
                        remove_pending_bubble(&w, &frozen);
                        if let Some(send) =
                            ui_ctx.recover_composer_send(frozen, composer_is_empty(&w))
                        {
                            restore_composer(&w, &ui_ctx, send);
                        }
                        w.set_toast_message(
                            "Earlier delivery is unconfirmed. Your new draft is kept.".into(),
                        );
                        w.set_toast_visible(true);
                    }
                });
                return true;
            }
            if matches!(
                &e,
                gateway::SendError::DeliveryUnconfirmed
                    | gateway::SendError::AttemptAlreadyConfirmed
            ) {
                // Taken by core, outcome unresolved: the pending row reads
                // "not confirmed" (no retry), which is the truth. In
                // particular GIF/voice wrappers must not restore it as a new
                // composer send. The retained attempt never re-reads the
                // files, so the sheltered copies are done here too.
                if matches!(&e, gateway::SendError::DeliveryUnconfirmed) {
                    let (weak, frozen) = (weak.clone(), send_for_delivery.clone());
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            mark_pending_unconfirmed(&w, &frozen);
                        }
                    });
                }
                notice(&weak, &ui_ctx, session_revision, &e.to_string());
                ui_ctx.release_staged_files(&pending_attachments);
                return true;
            }

            // `format_send_error` exists to produce exactly this string
            // and its return value used to be dropped on the floor, so a
            // send against a dead server cleared the composer and
            // reported nothing at all.
            report_send_failure(
                &weak,
                &ui_ctx,
                session_revision,
                ui_ctx.format_send_error(&e, "send"),
            );
            // A refusal means we are muted or were removed, and no
            // event told us: re-read the roster and swap the composer
            // for the notice.
            if e.is_refused() {
                if let ActiveConversation::Channel(channel_id) = conversation {
                    refresh_send_notice(&weak, &ui_ctx, channel_id);
                }
            }
            return false;
        }
    };

    // Decode attachment thumbnails off the UI thread.
    let mut attachment_display: Vec<_> = ui_result
        .attachments
        .iter()
        .map(attachment_to_display_data)
        .collect();
    #[cfg(target_os = "android")]
    {
        let client = ui_ctx.clone();
        attachment_display = tokio::task::spawn_blocking(move || {
            for (index, att) in attachment_display.iter_mut().enumerate() {
                if index < 4 {
                    att.derive_video_thumbnail(&client);
                }
                att.decode_thumbnail();
            }
            attachment_display
        })
        .await
        .unwrap_or_default();
    }
    #[cfg(not(target_os = "android"))]
    for att in &mut attachment_display {
        att.decode_thumbnail();
    }

    let ui_ctx_for_gifs = ui_ctx.clone();
    let ui_ctx_for_sidebar = ui_ctx.clone();
    let sent_text = text.clone();
    let timestamp_str = ui_result.timestamp_display.clone();
    let sidebar_preview = ui_result.sidebar_preview.clone();
    let reply_thumbnail_rgba = ui_result
        .reply_thumbnail_bytes
        .as_ref()
        .and_then(|bytes| image::load_from_memory(bytes).ok())
        .map(|img| img.to_rgba8());
    let reply_media = ui_result.reply_parent_media.clone();
    let msg_id = ui_result.message_id_effective.clone();
    let delivery_status = ui_result.delivery_status;
    let bootstrap_initiated = ui_result.bootstrap_initiated;
    let sent_at = ui_result.sent_at;
    let failure_notice = ui_result.failure_notice.clone();
    let failure_reason = ui_result.failure_reason.clone().unwrap_or_default();

    tracing::info!(msg_id = %msg_id, "[UI_SEND] scheduling invoke_from_event_loop for append");
    let _ = slint::invoke_from_event_loop(move || {
        if !ui_ctx.composer_session_is_current(session_revision) {
            return;
        }
        let Some(w) = weak.upgrade() else {
            tracing::error!("[UI_SEND] weak.upgrade() returned None; own bubble dropped");
            return;
        };
        // Only into the conversation it was sent to. A send that outlived a
        // switch (an upload takes up to 30s per file) is in the store, and
        // shows when its conversation is opened again.
        if ui_ctx.chat_state().active_conversation() == conversation {
            let slint_attachments: Vec<_> =
                attachment_display.iter().map(|a| a.to_slint()).collect();
            let (reply_thumbnail, reply_has_thumbnail, reply_thumb_w, reply_thumb_h) =
                reply_thumbnail_rgba
                    .as_ref()
                    .map(|rgba| {
                        let (w, h) = rgba.dimensions();
                        (
                            crate::bin_ui::slint_conversions::rgba_to_slint_image(
                                rgba.as_raw(),
                                w,
                                h,
                            ),
                            true,
                            w as i32,
                            h as i32,
                        )
                    })
                    .unwrap_or_else(|| (slint::Image::default(), false, 0, 0));
            let mut msg_item = super::helpers::build_own_message_item(
                &msg_id,
                &current_user_id,
                &current_user_name,
                &sent_text,
                &timestamp_str,
                sent_at,
                slint_attachments,
                is_reply,
                &reply_sender_name,
                &reply_preview_text,
                reply_thumbnail,
                reply_has_thumbnail,
                reply_thumb_w,
                reply_thumb_h,
                &reply_media,
                delivery_status,
            );
            {
                use slint::Model;
                tracing::info!(
                    msg_id = %msg_id,
                    existing = w.get_messages().row_count(),
                    "[UI_SEND] calling append_message_to_ui"
                );
            }
            msg_item.failure_reason = failure_reason.into();
            // In place of the pending row, so the bubble does not move.
            upsert_message_in_ui(&w, &pending_id, msg_item);
            tracing::debug!(
                elapsed_ms = send_started.elapsed().as_millis(),
                "send: bubble appended"
            );
            {
                use slint::Model;
                tracing::info!(
                    after = w.get_messages().row_count(),
                    "[UI_SEND] append_message_to_ui done"
                );
            }

            if bootstrap_initiated {
                w.set_composer_placeholder(BOOTSTRAP_PLACEHOLDER.into());
            }

            crate::bin_ui::gif_state::auto_register_gifs_from_messages(
                w.as_weak(),
                ui_ctx_for_gifs,
            );
        }

        // Stored but not delivered: the bubble says "not sent"
        // (tap to retry); the reason is told once, here.
        if let Some(notice) = failure_notice {
            w.set_toast_message(notice.into());
            w.set_toast_variant("error".into());
            w.set_toast_visible(true);
            w.global::<crate::Haptics>().invoke_error();
        }

        crate::bin_ui::features::sidebar::update_sidebar_preview(
            &w,
            &ui_ctx_for_sidebar,
            conversation,
            &sidebar_preview,
            &timestamp_str,
        );
    });
    true
}

/// Handle Jarvis slash commands (/ask, /summarize, /knowledge, /remember).
///
/// Dispatch + state recording lives in `ChatterClient::jarvis_execute`.
/// This function only manages the UI thinking-placeholder lifecycle.
#[cfg(feature = "jarvis")]
async fn handle_jarvis_slash_command(
    ui_ctx: &Arc<ChatterClient>,
    cmd: chatter_core::services::jarvis::JarvisCommand,
    channel_id: Option<&chatter_core::domain::ids::ChannelId>,
    weak_window: slint::Weak<MainWindow>,
) {
    use chatter_core::domain::jarvis::JarvisContext;

    let context = match channel_id {
        Some(id) => JarvisContext::channel(id.clone()),
        None => {
            let placeholder_id: chatter_core::domain::ids::ChannelId = uuid::Uuid::nil().into();
            JarvisContext::dm(placeholder_id)
        }
    };
    let author_id = ui_ctx.auth_state().get().current_user_id;

    // Surface the thinking placeholder before kicking off the core call.
    ui_ctx.jarvis_state().set_thinking(true);
    let thinking_id = {
        let weak = weak_window.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_jarvis_thinking(true);
                let id = crate::bin_ui::features::jarvis::add_jarvis_thinking_placeholder(&w);
                let _ = tx.send(id);
            }
        });
        rx.recv().ok()
    };

    let (query_display, result) = ui_ctx
        .jarvis_execute(cmd, &context, author_id.as_ref())
        .await;
    let response_text = match &result {
        Ok(response) => response.clone(),
        Err(e) => format!("Error: {}", e),
    };

    let jarvis_state = ui_ctx.jarvis_state().clone();
    let _ = slint::invoke_from_event_loop(move || {
        jarvis_state.set_thinking(false);
        if let Some(w) = weak_window.upgrade() {
            w.set_jarvis_thinking(false);
            if let Some(tid) = thinking_id {
                crate::bin_ui::features::jarvis::replace_thinking_with_response(
                    &w,
                    &tid,
                    &query_display,
                    &response_text,
                );
            }
        }
    });
}
