//! Persistent audio player strip.
//!
//! Playback outlives the conversation it started in, so the strip in the app
//! chrome — not the message bubble — is what stays reachable. This module owns
//! the strip's callbacks and the state it renders from, and chains to the next
//! voice note in the source conversation when a track ends.

use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

use slint::{ComponentHandle, Model};

use chatter_core::client::ChatterClient;
use chatter_core::domain::message::ConversationType;

use crate::bin_ui::audio_state::{self, NowPlaying};
use crate::MainWindow;

thread_local! {
    /// Context for a track that autoplay is about to start. The click path
    /// reads a track's message context off the visible message model, but an
    /// autoplayed track's message usually isn't on screen — the user has
    /// navigated elsewhere, which is the whole point of the strip. Storage
    /// already told us the context when we looked the track up, so we park it
    /// here for the play path to pick up rather than looking it up twice.
    static PENDING_CONTEXT: RefCell<Option<NowPlaying>> = const { RefCell::new(None) };
}

/// Wire the strip's callbacks.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_toggle(window);
    wire_dismiss(window);
    wire_jump(window, ui_ctx);
    wire_track_finished(window, ui_ctx);
}

/// A track hit its end (raised by the progress timer).
fn wire_track_finished(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_audio_track_finished(move || {
        if let Some(w) = weak_window.upgrade() {
            track_finished(&w, &ui_ctx);
        }
    });
}

/// Play/pause from the strip.
///
/// Routed through the attachment click handler so pausing from the strip and
/// pausing from the message bubble go down the same path — that handler
/// already toggles when the requested attachment is the loaded one.
fn wire_toggle(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_audio_toggle_playback(move || {
        let Some(w) = weak_window.upgrade() else { return };
        let Some(track) = audio_state::get_now_playing() else { return };
        w.invoke_view_attachment(track.attachment_id.into());
    });
}

/// Dismiss the strip: stop playback and forget the track.
fn wire_dismiss(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_audio_dismiss(move || {
        audio_state::stop_playback();
        if let Some(w) = weak_window.upgrade() {
            super::timers::stop_audio_progress_timer();
            clear_ui(&w);
        }
    });
}

/// Jump to the message the loaded track came from, opening its conversation
/// first when the user has navigated away.
fn wire_jump(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_audio_jump_to_source(move || {
        let Some(w) = weak_window.upgrade() else { return };
        let Some(track) = audio_state::get_now_playing() else { return };

        let already_open = ui_ctx
            .active_conversation()
            .channel_id()
            .zip(track.conversation_id.clone())
            .map(|(active, source)| active == source)
            .unwrap_or(false);

        if !already_open {
            let (Some(conversation_id), Some(conversation_type)) =
                (track.conversation_id.clone(), track.conversation_type)
            else {
                tracing::debug!("Audio strip: track has no conversation to jump to");
                return;
            };
            if !open_conversation(&w, &conversation_id.to_string(), conversation_type) {
                return;
            }
        }

        // `jump_to_message` pages older history and re-resolves on a timer, so
        // it copes with the conversation still loading underneath it.
        super::message_actions::jump_to_message(&w, &track.message_id);
    });
}

/// Open a conversation by id and type. Returns whether a route existed.
fn open_conversation(w: &MainWindow, conversation_id: &str, conversation_type: ConversationType) -> bool {
    match conversation_type {
        ConversationType::Channel | ConversationType::SavedMessages => {
            w.invoke_channel_selected(conversation_id.into());
            true
        }
        ConversationType::ServerDm | ConversationType::MailboxDm | ConversationType::AnonDm => {
            w.invoke_dm_selected(conversation_id.into());
            true
        }
        ConversationType::AnonGroup => {
            w.invoke_private_group_selected(conversation_id.into());
            true
        }
        // Mesh conversations carry no attachments, so nothing can be playing
        // from one.
        ConversationType::Mesh | ConversationType::MeshDm => {
            tracing::debug!(?conversation_type, "Audio strip: no jump route");
            false
        }
    }
}

/// Record a freshly loaded track and show the strip. Call on the UI thread
/// once playback has actually started.
pub fn track_started(w: &MainWindow, attachment_id: &str, filename: &str, total_duration: Duration) {
    let track = PENDING_CONTEXT
        .with(|cell| cell.borrow_mut().take())
        .filter(|pending| pending.attachment_id == attachment_id)
        .map(|pending| NowPlaying {
            total_duration,
            is_playing: true,
            ..pending
        })
        .unwrap_or_else(|| context_from_model(w, attachment_id, filename, total_duration));

    audio_state::start_track(track);
    apply_state(w);
    super::timers::start_audio_progress_timer(w);
}

/// Build track context from the visible message model — the click path, where
/// the message being played is on screen by definition. Falls back to bare
/// context (no jump target, no autoplay) if it somehow isn't.
fn context_from_model(
    w: &MainWindow,
    attachment_id: &str,
    filename: &str,
    total_duration: Duration,
) -> NowPlaying {
    let mut track = NowPlaying {
        attachment_id: attachment_id.to_string(),
        filename: filename.to_string(),
        sender_name: String::new(),
        message_id: String::new(),
        sent_at: 0,
        conversation_id: None,
        conversation_type: None,
        total_duration,
        is_playing: true,
    };

    let messages = w.get_messages();
    'outer: for i in 0..messages.row_count() {
        let Some(message) = messages.row_data(i) else { continue };
        for attachment in message.attachments.iter() {
            if attachment.id.as_str() != attachment_id {
                continue;
            }
            track.sender_name = message.sender_name.to_string();
            track.message_id = message.message_id.to_string();
            track.sent_at = message.timestamp_ns.parse().unwrap_or(0);
            break 'outer;
        }
    }

    track
}

/// Push the current playback state onto the window properties the strip and
/// the in-message players both render from.
pub fn apply_state(w: &MainWindow) {
    match audio_state::get_now_playing() {
        Some(track) => {
            w.set_audio_player_active(true);
            w.set_current_playing_attachment_id(track.attachment_id.clone().into());
            w.set_is_media_playing(track.is_playing);
            w.set_audio_title(track.filename.clone().into());
            w.set_audio_sender(track.sender_name.clone().into());
            w.set_audio_duration_display(
                audio_state::format_duration(track.total_duration).into(),
            );
        }
        None => clear_ui(w),
    }
}

/// Reset every property the strip renders from.
pub fn clear_ui(w: &MainWindow) {
    w.set_audio_player_active(false);
    w.set_current_playing_attachment_id("".into());
    w.set_is_media_playing(false);
    w.set_audio_progress(0.0);
    w.set_audio_elapsed("0:00".into());
    w.set_audio_title("".into());
    w.set_audio_sender("".into());
    w.set_audio_duration_display("".into());
}

/// A track finished: chain to the next voice note in its conversation, or
/// clear the strip when there isn't one.
pub fn track_finished(w: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let Some(finished) = audio_state::get_now_playing() else {
        clear_ui(w);
        return;
    };

    let next = finished
        .conversation_id
        .as_ref()
        .zip(finished.conversation_type)
        .and_then(|(conversation_id, conversation_type)| {
            ui_ctx
                .next_audio_after(conversation_id, &conversation_type, finished.sent_at)
                .map_err(|error| tracing::warn!(%error, "Audio strip: next-track lookup failed"))
                .ok()
                .flatten()
        });

    let Some(next) = next else {
        audio_state::clear_state();
        clear_ui(w);
        return;
    };

    tracing::info!(attachment = %next.attachment_id, "Audio strip: chaining to next track");

    // Duration is unknown until the decoder reports it; `track_started` fills
    // it in when playback actually begins.
    PENDING_CONTEXT.with(|cell| {
        *cell.borrow_mut() = Some(NowPlaying {
            attachment_id: next.attachment_id.to_string(),
            filename: next.filename,
            sender_name: next.sender_name,
            message_id: next.message_id.to_string(),
            sent_at: next.sent_at,
            conversation_id: Some(next.conversation_id),
            conversation_type: Some(next.conversation_type),
            total_duration: Duration::ZERO,
            is_playing: true,
        });
    });

    // Clear first: the click handler toggles pause when asked for the track
    // that is already loaded, and the finished one still is.
    audio_state::clear_state();
    w.invoke_view_attachment(next.attachment_id.to_string().into());
}
