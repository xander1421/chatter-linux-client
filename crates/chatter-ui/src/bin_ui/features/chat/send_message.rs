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

use slint::{ComponentHandle, Model};

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
/// The composer is cleared the instant send is tapped. Core may store the
/// message before the network round trip returns, so this row bridges that
/// interval with the text and staged files as they will look. Its
/// `delivery-status` is "sending",
/// which the bubble draws as the word plus a thin progress line. It claims
/// nothing about arrival; a failure with no stored row removes it and puts
/// the draft back, while a stored row replaces it in place with its status.
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
            || ui_ctx.active_conversation() != conversation
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
        let kind = ui_ctx.conversation_type_of(&conversation);
        let members = kind.as_ref().zip(conversation.channel_id())
            .map(|(kind, id)| ui_ctx.mention_members(kind, &id)).unwrap_or_default();
        let broadcast = matches!(kind, Some(chatter_core::domain::message::ConversationType::Channel | chatter_core::domain::message::ConversationType::AnonGroup));
        let item = super::helpers::build_own_message_item(
            &row,
            &user_id,
            &user_name,
            &text,
            &members,
            broadcast,
            &crate::bin_ui::converters::format_timestamp(now),
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
        let draft = w.global::<crate::ChatPane>().get_composer_text();
        super::helpers::append_message_to_ui(w, item);
        w.global::<crate::ChatPane>().set_composer_text(draft);
    }
    w.global::<crate::ChatPane>().set_is_at_bottom(true);
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
    let messages = w.global::<crate::ChatPane>().get_messages();
    for i in 0..messages.row_count() {
        if let Some(mut m) = messages.row_data(i)
            && m.message_id.as_str() == pending_id
        {
            m.delivery_status = "unconfirmed".into();
            messages.set_row_data(i, m);
        }
    }
}

/// A Tor send can spend its time on directory and hidden-service work before
/// the server sees a deposit. Keep the same in-flight attempt, but name that
/// wait on the bubble so it never appears stuck at an unqualified "sending".
fn mark_pending_waiting_for_tor(w: &MainWindow, send: &ComposerSend) {
    use slint::Model;
    let pending_id = pending_message_id(send);
    let messages = w.global::<crate::ChatPane>().get_messages();
    for i in 0..messages.row_count() {
        if let Some(mut m) = messages.row_data(i)
            && m.message_id.as_str() == pending_id
            && m.delivery_status.as_str() == "sending"
        {
            m.delivery_status = "waiting-tor".into();
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
        local_path: (!attachment
            .path
            .components()
            .any(|component| component.as_os_str() == "composer-staging"))
        .then(|| attachment.path.to_string_lossy().into_owned()),
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
        is_download_failed: false,
        progress: 0.0,
        thumbnail_bytes: std::sync::Arc::<[u8]>::from(Vec::new()),
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
    if ui_ctx.chat_snapshot().active_channel_id != Some(channel_id) {
        return;
    }
    let read_only = ui_ctx.is_former_channel(&channel_id);
    let can_rejoin = ui_ctx.can_rejoin_channel(&channel_id);
    let blocked = ui_ctx.blocked_dm_peer(ui_ctx.active_conversation()).is_some();
    let notice = if blocked { "You blocked this person.".to_string() } else { notice };
    // A role change (`ChannelMemberUpdated`) lands here too, so the header's
    // invite button follows promotion and demotion.
    let actions = ui_ctx.conversation_actions(ui_ctx.active_conversation());
    // The membership banner and public badge travel with the notice: both say
    // whether (and why) this channel can be posted to.
    let membership = ui_ctx.channel_membership_header(&channel_id);
    let answer = !blocked && ui_ctx
        .secret_chat_gate(&chatter_core::domain::anon_dm::AnonDmId::from_uuid(
            *channel_id.as_uuid(),
        ))
        .is_some_and(|g| g.offers_answer());
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.global::<crate::ChatPane>()
                .set_composer_notice(notice.into());
            w.global::<crate::ChatPane>().set_is_blocked(blocked);
            w.global::<crate::ChatPane>().set_can_rejoin(can_rejoin);
            w.global::<crate::ChatPane>()
                .set_secret_request_actions(answer);
            w.global::<crate::ChatPane>()
                .set_conversation_read_only(read_only);
            w.global::<crate::ChatPane>()
                .set_header_can_call(actions.can_call);
            w.global::<crate::ChatPane>()
                .set_header_can_invite(actions.can_invite);
            super::membership::apply_header(&w, &membership);
        }
    });
}

/// Every conversation switch goes through `ChatState` (see the draft watcher
/// in `features::composer`): clear the notice on leaving, recompute it for a
/// channel on entering. Live mute changes arrive as `MemberUpdated`
/// ([`show_send_notice`]); the open re-reads the roster in case one was missed.
fn wire_send_notice(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let mut current = ui_ctx.chat_snapshot().active_channel_id;
    let mut rx = ui_ctx.subscribe_chat_snapshots();
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
                    w.global::<crate::ChatPane>().set_composer_notice("".into());
                    w.global::<crate::ChatPane>().set_is_blocked(false);
                    w.global::<crate::ChatPane>()
                        .set_secret_request_actions(false);
                    w.global::<crate::ChatPane>()
                        .set_conversation_read_only(false);
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
    let unblock_ctx = ui_ctx.clone();
    let unblock_weak = window.as_weak();
    window.global::<crate::ChatPane>().on_unblock_clicked(move || {
        let Some(peer) = unblock_ctx.blocked_dm_peer(unblock_ctx.active_conversation()) else { return };
        let ctx = unblock_ctx.clone();
        let weak = unblock_weak.clone();
        tokio::spawn(async move {
            if let Err(error) = ctx.unblock_profile_user(&peer).await {
                tracing::error!(%error, "unblock from chat failed");
                return;
            }
            let channel = ctx.chat_snapshot().active_channel_id;
            if let Some(channel) = channel {
                show_send_notice(&weak, &ctx, channel);
            }
            crate::bin_ui::features::auth::initial_data::refresh_friends_list(weak.clone(), ctx.clone());
            crate::bin_ui::features::modals::friends::refresh_blocked_users(weak.clone(), &ctx);
            crate::run_ui!(weak, |w| {
                crate::bin_ui::features::sidebar::refresh_chat_list(&w, &ctx);
                if w.global::<crate::ModalHostState>().get_profile_user_id().as_str() == peer.to_string() {
                    w.global::<crate::ModalHostState>().set_profile_is_blocked(false);
                }
            });
        });
    });
    let rejoin_ctx = ui_ctx.clone();
    let rejoin_weak = window.as_weak();
    window.global::<crate::ChatPane>().on_rejoin_clicked(move || {
        let Some(channel) = rejoin_ctx.chat_snapshot().active_channel_id else { return };
        let ctx = rejoin_ctx.clone();
        let weak = rejoin_weak.clone();
        tokio::spawn(async move {
            if let Err(error) = ctx.join_channel(&channel, None).await {
                tracing::warn!(%error, "rejoin from chat failed");
                let message = error.user_message();
                crate::run_ui!(weak, |w| {
                    w.set_toast_message(message.into());
                    w.set_toast_variant("error".into());
                    w.set_toast_visible(true);
                });
                return;
            }
            refresh_send_notice(&weak, &ctx, channel);
            crate::bin_ui::features::sidebar::rebuild_chat_list(&weak, &ctx);
        });
    });
    let chat_state = ui_ctx.clone();
    let ui_ctx_jarvis = ui_ctx.clone();

    let weak_window = window.as_weak();

    window
        .global::<crate::ChatPane>()
        .on_send(move |text: slint::SharedString| {
            let send_started = std::time::Instant::now();
            let text = text.to_string();
            let pending_attachments = chat_state.pending_attachments();
            let has_attachments = !pending_attachments.is_empty();

            if text.trim().is_empty() && !has_attachments {
                return false;
            }

            // Mesh conversation open: serverless, so it must not reach the gateway
            // pipeline — or the Jarvis interceptions below, which would answer a
            // slash command instead of broadcasting it. Core picks the transport
            // (public room / geohash cell / peer).
            #[cfg(feature = "mesh")]
            if crate::bin_ui::features::mesh::intercept_send(
                &ui_ctx_jarvis,
                &text,
                weak_window.clone(),
            ) {
                return true;
            }

            // Local AI channel: every send routes to Jarvis, nothing networks.
            #[cfg(feature = "jarvis")]
            if let Some(id) = chat_state.chat_snapshot().active_channel_id {
                if chatter_core::client::is_jarvis_channel(&id) {
                    if text.trim().is_empty() {
                        return false;
                    }
                    let ui_ctx = ui_ctx_jarvis.clone();
                    let weak = weak_window.clone();
                    tokio::spawn(async move {
                        crate::bin_ui::features::jarvis::handle_jarvis_channel_send(
                            ui_ctx, text, weak,
                        )
                        .await;
                    });
                    return true;
                }
            }

            // Check for Jarvis slash commands first (/ask, /summarize, /knowledge, /remember)
            #[cfg(feature = "jarvis")]
            if let Some(cmd) = crate::bin_ui::features::jarvis::parse_slash_command(&text) {
                let ui_ctx = ui_ctx_jarvis.clone();
                let conversation = chat_state.active_conversation();
                let auth = ui_ctx.auth_snapshot();
                let owner =
                    auth.current_user_id
                        .map(|user_id| chatter_core::client::JarvisSessionOwner {
                            user_id,
                            revision: auth.session_revision,
                        });
                let view_owner = crate::bin_ui::features::history_owner::HistoryOwner::capture(
                    &ui_ctx,
                    conversation,
                );
                let weak = weak_window.clone();
                tokio::spawn(async move {
                    handle_jarvis_slash_command(
                        &ui_ctx,
                        cmd,
                        conversation,
                        owner,
                        view_owner,
                        weak,
                    )
                    .await;
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

            let Some(w) = weak_window.upgrade() else {
                return false;
            };
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
            w.global::<crate::ChatPane>().set_composer_text("".into());
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
            crate::bin_ui::features::sidebar::refresh_draft_rows(&w, &ui_ctx_jarvis);

            // Clear outgoing typing state (Signal: clearTypingTimers on message send)
            clear_outgoing_typing(&ui_ctx_jarvis);

            // Track user activity for presence updates
            {
                let ui_ctx = ui_ctx_jarvis.clone();
                tokio::spawn(async move {
                    ui_ctx.record_presence_activity().await;
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
    if !ui_ctx.composer_session_is_current(send.session_revision) {
        return;
    }
    let has_attachments = !send.attachments.is_empty();
    if let Some(send) = ui_ctx.recover_composer_send(send, composer_is_empty(w)) {
        restore_composer(w, ui_ctx, send);
    }
    if has_attachments {
        w.set_toast_message("Attachment not sent. Try again.".into());
        w.set_toast_variant("error".into());
        w.set_toast_visible(true);
        w.global::<crate::Haptics>().invoke_error();
    }
}

/// The reply bar's target, if the composer is replying.
pub fn composer_reply(w: &MainWindow) -> Option<ComposerReply> {
    if !w.global::<crate::ChatPane>().get_composer_is_replying() {
        return None;
    }
    let reply = ComposerReply {
        sender_id: w
            .global::<crate::ChatPane>()
            .get_composer_reply_to_sender()
            .to_string(),
        timestamp: w
            .global::<crate::ChatPane>()
            .get_composer_reply_to_timestamp()
            .to_string(),
        sender_name: w
            .global::<crate::ChatPane>()
            .get_composer_reply_to_sender_name()
            .to_string(),
        preview: w
            .global::<crate::ChatPane>()
            .get_composer_reply_preview()
            .to_string(),
        thumbnail: w
            .global::<crate::ChatPane>()
            .get_composer_reply_has_thumbnail()
            .then(|| {
                w.global::<crate::ChatPane>()
                    .get_composer_reply_thumbnail()
                    .to_rgba8()
            })
            .flatten()
            .map(|pixels| (pixels.as_bytes().to_vec(), pixels.width(), pixels.height())),
    };
    (!reply.sender_id.is_empty() && !reply.timestamp.is_empty()).then_some(reply)
}

/// Prepare the same quote intent for main and thread composer sends.
pub(crate) fn quote_intent(reply: Option<&ComposerReply>) -> Option<gateway::QuoteIntent> {
    let parsed = super::helpers::parse_reply_context(&reply?.sender_id, &reply?.timestamp)?;
    Some(gateway::QuoteIntent {
        target_author_id: parsed.sender_id,
        target_timestamp: parsed.timestamp,
        target_preview: None,
    })
}

pub(crate) fn clear_reply(w: &MainWindow) {
    w.global::<crate::ChatPane>()
        .set_composer_is_replying(false);
    w.global::<crate::ChatPane>()
        .set_composer_reply_to_sender("".into());
    w.global::<crate::ChatPane>()
        .set_composer_reply_to_sender_name("".into());
    w.global::<crate::ChatPane>()
        .set_composer_reply_to_timestamp("".into());
    w.global::<crate::ChatPane>()
        .set_composer_reply_preview("".into());
    w.global::<crate::ChatPane>()
        .set_composer_reply_has_thumbnail(false);
    w.global::<crate::ChatPane>()
        .set_composer_reply_thumbnail(slint::Image::default());
    w.global::<crate::ChatPane>()
        .set_composer_placeholder("Type a message...".into());
    w.global::<crate::ChatPane>()
        .invoke_composer_draft_changed();
}

/// Whether the composer holds nothing a restored send could overwrite.
/// A voice note being recorded counts: its send would take restored files.
pub fn composer_is_empty(w: &MainWindow) -> bool {
    let voice = w.global::<crate::VoiceRecording>();
    w.global::<crate::ChatPane>().get_composer_text().is_empty()
        && !w.invoke_composer_has_preedit()
        && !w.global::<crate::ChatPane>().get_composer_is_replying()
        && !w.global::<crate::ChatPane>().get_composer_is_editing()
        && !voice.get_active()
        && !voice.get_busy()
}

/// Freeze the composer into a send (core decides whether it may go) and
/// clear what it took. `Err` is what to tell the user; nothing was consumed.
pub fn take_composer_send(w: &MainWindow, ui_ctx: &Arc<ChatterClient>) -> Result<ComposerSend, String> {
    if w.global::<crate::ChatPane>().get_composer_is_editing() {
        return Err("Finish editing first.".into());
    }
    w.invoke_settle_composer();
    let send = ui_ctx.begin_composer_send(
        w.global::<crate::ChatPane>()
            .get_composer_text()
            .to_string(),
        composer_reply(w),
    )?;
    w.global::<crate::ChatPane>().set_composer_text("".into());
    #[cfg(target_os = "android")]
    crate::platform::android::text_input::consume();
    if send.reply.is_some() {
        clear_reply(w);
    }
    crate::bin_ui::features::composer::render_attachments(w, &[]);
    // The accepted send took the draft: its persisted copy goes with it now,
    // not after the autosave delay, so a restart cannot resurrect sent text.
    crate::bin_ui::features::composer::autosave_composer_draft(w, ui_ctx);
    crate::bin_ui::features::sidebar::refresh_draft_rows(w, ui_ctx);
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
    w.global::<crate::ChatPane>()
        .set_composer_reply_to_sender(reply.sender_id.into());
    w.global::<crate::ChatPane>()
        .set_composer_reply_to_timestamp(reply.timestamp.into());
    w.global::<crate::ChatPane>()
        .set_composer_reply_to_sender_name(reply.sender_name.into());
    w.global::<crate::ChatPane>()
        .set_composer_reply_preview(reply.preview.into());
    w.global::<crate::ChatPane>()
        .set_composer_reply_has_thumbnail(thumbnail.is_some());
    w.global::<crate::ChatPane>()
        .set_composer_reply_thumbnail(thumbnail.unwrap_or_default());
    w.global::<crate::ChatPane>()
        .set_composer_placeholder("Type your reply...".into());
    w.global::<crate::ChatPane>().set_composer_is_replying(true);
    w.global::<crate::ChatPane>()
        .invoke_composer_draft_changed();
}

/// Put a failed send back into the (empty) composer it came from.
pub fn restore_composer(w: &MainWindow, ui_ctx: &ChatterClient, send: ComposerSend) {
    let chat = ui_ctx.chat_snapshot();
    if !ui_ctx.composer_session_is_current(send.session_revision)
        || chat.active_conversation != send.conversation
    {
        return;
    }
    let pane = w.global::<crate::ChatPane>();
    pane.set_composer_text(send.text.into());
    pane.set_composer_caret_end_seq(pane.get_composer_caret_end_seq() + 1);
    // Android: the accepted send consumed the IME's copy; the text going
    // back in is a newer state than the emptied one it may still report.
    #[cfg(target_os = "android")]
    crate::platform::android::text_input::restore();
    if let Some(reply) = send.reply {
        show_reply(w, reply);
    }
    for attachment in send.attachments {
        ui_ctx.add_pending_attachment_for(send.conversation, chat.attachment_revision, attachment);
    }
    crate::bin_ui::features::composer::render_attachments(w, &ui_ctx.pending_attachments());
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
    let mut parts = send.ordered_parts().into_iter();
    let mut first = true;
    while let Some(part) = parts.next() {
        let frozen = part.clone();
        match deliver_inner(weak.clone(), ui_ctx.clone(), part, send_started).await {
            PartOutcome::Sent => {
                first = false;
                continue;
            }
            PartOutcome::Retained => {
                let mut remaining = String::new();
                for part in parts {
                    remaining.push_str(&part.text);
                }
                if !remaining.is_empty() {
                    let mut unsent = frozen;
                    unsent.attempt_id = uuid::Uuid::new_v4();
                    unsent.text = remaining;
                    unsent.reply = None;
                    unsent.attachments.clear();
                    let weak = weak.clone();
                    let ui_ctx = ui_ctx.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            recover_failed_send(&w, &ui_ctx, unsent);
                        }
                    });
                }
                return true;
            }
            PartOutcome::Unsent => {
                let remaining = if first {
                    None // The original caller restores the whole unsent draft.
                } else {
                    let mut unsent = frozen.clone();
                    for part in parts {
                        unsent.text.push_str(&part.text);
                    }
                    Some(unsent)
                };
                let weak = weak.clone();
                let ui_ctx = ui_ctx.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        remove_pending_bubble(&w, &frozen);
                        if let Some(unsent) = remaining {
                            recover_failed_send(&w, &ui_ctx, unsent);
                        }
                    }
                });
                return !first;
            }
        }
    }
    true
}

enum PartOutcome { Sent, Retained, Unsent }

async fn deliver_inner(
    weak: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    send: ComposerSend,
    send_started: std::time::Instant,
) -> PartOutcome {
    if !ui_ctx.composer_session_is_current(send.session_revision) {
        return PartOutcome::Unsent;
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
    if !ui_ctx.has_auth_identity() {
        return PartOutcome::Unsent;
    }
    let auth = ui_ctx.auth_snapshot();
    let current_user_id = auth
        .current_user_id
        .map(|id| id.to_string())
        .unwrap_or_default();
    let current_user_name = auth.display_name.unwrap_or_else(|| "Me".to_string());
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
    let quote = quote_intent(reply.as_ref());
    if reply.is_some() && quote.is_none() { return PartOutcome::Unsent; }

    tracing::info!(
        ?conversation,
        text_len = text.len(),
        "[UI_SEND] entering send_message_ui"
    );
    let send_client = ui_ctx.clone();
    let sending = send_client.send_composer_message(&send_for_delivery, quote);
    tokio::pin!(sending);
    let tor_send = ui_ctx.anon_mode_enabled()
        || matches!(
            conversation,
            ActiveConversation::AnonDm(_) | ActiveConversation::AnonGroup(_)
        );
    let outcome = if tor_send {
        tokio::select! {
            result = &mut sending => result,
            _ = tokio::time::sleep(std::time::Duration::from_secs(12)) => {
                let weak = weak.clone();
                let client = ui_ctx.clone();
                let pending = send_for_delivery.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if client.composer_session_is_current(pending.session_revision) {
                        if let Some(w) = weak.upgrade() {
                            mark_pending_waiting_for_tor(&w, &pending);
                        }
                    }
                });
                sending.await
            }
        }
    } else {
        sending.await
    };
    if !ui_ctx.composer_session_is_current(session_revision) {
        return PartOutcome::Unsent;
    }
    let ui_result = match outcome {
        None => return PartOutcome::Unsent,
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
                return PartOutcome::Retained;
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
                ui_ctx.release_staged_files(&pending_attachments);
                return PartOutcome::Retained;
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
            return PartOutcome::Unsent;
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
            }
            crate::bin_ui::converters::decode_inline_thumbnails(&mut attachment_display);
            attachment_display
        })
        .await
        .unwrap_or_default();
    }
    #[cfg(not(target_os = "android"))]
    crate::bin_ui::converters::decode_inline_thumbnails(&mut attachment_display);

    let ui_ctx_for_gifs = ui_ctx.clone();
    let sent_text = text.clone();
    let timestamp_str = ui_result.timestamp_display.clone();
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
    let stored_failure = delivery_status == "failed";

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
        if ui_ctx.active_conversation() == conversation {
            // A peer can receipt a secret mailbox post before this send
            // returns and its own bubble is appended. The receipt event then
            // finds only the provisional row (with a different timestamp).
            // Read the persisted status now so that early receipts survive
            // the replacement of that provisional row.
            let delivery_status = msg_id
                .parse::<chatter_core::domain::ids::MessageId>()
                .ok()
                .and_then(|id| ui_ctx.get_message_local(&id))
                .map(|message| {
                    chatter_core::services::message::display_delivery_status(
                        &message.delivery_status,
                        message.failure_reason.as_deref(),
                    )
                    .to_owned()
                })
                .unwrap_or_else(|| delivery_status.to_owned());
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
            let kind = ui_ctx.conversation_type_of(&conversation);
            let members = kind.as_ref().zip(conversation.channel_id())
                .map(|(kind, id)| ui_ctx.mention_members(kind, &id)).unwrap_or_default();
            let broadcast = matches!(kind, Some(chatter_core::domain::message::ConversationType::Channel | chatter_core::domain::message::ConversationType::AnonGroup));
            let mut msg_item = super::helpers::build_own_message_item(
                &msg_id,
                &current_user_id,
                &current_user_name,
                &sent_text,
                &members,
                broadcast,
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
                &delivery_status,
            );
            // The sender's copy is armed when it is stored: give the new
            // bubble its timer glyph now rather than on the next load.
            msg_item.disappearing = ui_ctx
                .disappearing_message_ids(std::slice::from_ref(&msg_id))
                .contains(msg_id.as_str());
            {
                use slint::Model;
                tracing::info!(
                    msg_id = %msg_id,
                    existing = w.global::<crate::ChatPane>().get_messages().row_count(),
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
                    after = w.global::<crate::ChatPane>().get_messages().row_count(),
                    "[UI_SEND] append_message_to_ui done"
                );
            }

            // Group fan-out can be pending too, but it does not establish a
            // DM channel and must leave the group composer available.
            if bootstrap_initiated && !matches!(conversation, ActiveConversation::AnonGroup(_)) {
                w.global::<crate::ChatPane>()
                    .set_composer_placeholder(BOOTSTRAP_PLACEHOLDER.into());
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
    });
    if stored_failure { PartOutcome::Retained } else { PartOutcome::Sent }
}

/// Handle Jarvis slash commands (/ask, /summarize, /knowledge, /remember).
///
/// Dispatch + state recording lives in `ChatterClient::jarvis_execute`.
/// This function only manages the UI thinking-placeholder lifecycle.
#[cfg(feature = "jarvis")]
async fn handle_jarvis_slash_command(
    ui_ctx: &Arc<ChatterClient>,
    cmd: chatter_core::services::jarvis::JarvisCommand,
    conversation: ActiveConversation,
    session: Option<chatter_core::client::JarvisSessionOwner>,
    owner: crate::bin_ui::features::history_owner::HistoryOwner,
    weak_window: slint::Weak<MainWindow>,
) {
    let Some(session) = session else { return };
    if !owner.is_current() {
        return;
    }

    // Surface the thinking placeholder before kicking off the core call.
    let thinking_id = {
        let weak = weak_window.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let owner = owner.clone();
        let jarvis_state = ui_ctx.jarvis_state().clone();
        let _ = slint::invoke_from_event_loop(move || {
            if owner.is_current() {
                if let Some(w) = weak.upgrade() {
                    jarvis_state.set_thinking(true);
                    w.global::<crate::JarvisPane>().set_jarvis_thinking(true);
                    let id = crate::bin_ui::features::jarvis::add_jarvis_thinking_placeholder(&w);
                    let _ = tx.send(id);
                }
            }
        });
        rx.await.ok()
    };
    if thinking_id.is_none() {
        return;
    }

    let (query_display, result) = ui_ctx
        .jarvis_execute_for_conversation(cmd, conversation, session)
        .await;
    let response_text = match &result {
        Ok(response) => response.clone(),
        Err(e) => format!("Error: {}", e),
    };

    let jarvis_state = ui_ctx.jarvis_state().clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak_window.upgrade() {
            if let Some(tid) = thinking_id {
                if owner.is_current() {
                    crate::bin_ui::features::jarvis::replace_thinking_with_response(
                        &w,
                        &tid,
                        &query_display,
                        &response_text,
                    );
                } else {
                    crate::bin_ui::features::jarvis::remove_jarvis_thinking_placeholder(&w, &tid);
                }
                let still_thinking = w
                    .global::<crate::ChatPane>()
                    .get_messages()
                    .iter()
                    .any(|row| row.delivery_status.as_str() == "thinking");
                w.global::<crate::JarvisPane>()
                    .set_jarvis_thinking(still_thinking);
                jarvis_state.set_thinking(still_thinking);
            }
        } else {
            jarvis_state.set_thinking(false);
        }
    });
}
