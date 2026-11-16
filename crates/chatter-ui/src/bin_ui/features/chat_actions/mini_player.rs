//! Mini player: the loaded audio track's controls outside its conversation.
//!
//! Playback survives closing the conversation it started in. Which track is
//! loaded, whether the strip shows and what plays next are core's
//! (`ChatterClient::now_playing`, `advance_audio_queue`); this renders them.

use std::sync::{Arc, Mutex, OnceLock};

use slint::{ComponentHandle, Model};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::{AttachmentId, MessageId};
use chatter_core::state::ActiveConversation;

use crate::MainWindow;
use crate::bin_ui::audio_state;

static CLIENT: OnceLock<Arc<ChatterClient>> = OnceLock::new();
/// Where the last attachment click came from: (attachment, conversation,
/// message). Taken at click time because the audio starts after a decrypt or
/// download, by when another conversation may be on screen.
static CLICK: Mutex<Option<(String, ActiveConversation, Option<MessageId>)>> = Mutex::new(None);

pub(super) fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let _ = CLIENT.set(ui_ctx.clone());
    let pane = window.global::<crate::ChatPane>();

    let weak = window.as_weak();
    pane.on_mini_player_toggle(move || super::attachment_view::toggle_audio_playback(&weak));

    let weak = window.as_weak();
    pane.on_mini_player_close(move || {
        if let Some(w) = weak.upgrade() {
            stop(&w);
        }
    });

    let weak = window.as_weak();
    let client = ui_ctx.clone();
    pane.on_mini_player_open(move || {
        let (Some(w), Some(track)) = (weak.upgrade(), client.now_playing()) else {
            return;
        };
        if let Some(message_id) = track.message_id {
            crate::bin_ui::features::message_list::jump::jump_to_conversation_message(
                &w,
                &client,
                track.conversation,
                &message_id.to_string(),
            );
        }
    });
}

/// Remember where an attachment click came from. UI thread.
pub(super) fn note_click(w: &MainWindow, client: &ChatterClient, attachment_id: &str) {
    let pane = w.global::<crate::ChatPane>();
    let message_id = pane
        .get_messages()
        .iter()
        .chain(pane.get_thread_replies().iter())
        .find(|message| message.attachments.iter().any(|a| a.id == attachment_id))
        .and_then(|message| message.message_id.as_str().parse().ok());
    *CLICK.lock().unwrap_or_else(|poison| poison.into_inner()) = Some((
        attachment_id.to_owned(),
        client.active_conversation(),
        message_id,
    ));
}

/// The player started `attachment_id`: record the track in core (a storage
/// lookup, so on the caller's worker thread) and show it as playing.
pub(super) fn started(weak: slint::Weak<MainWindow>, attachment_id: &str) {
    if let Some(client) = CLIENT.get() {
        let (conversation, message_id) = CLICK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
            .filter(|(clicked, ..)| clicked == attachment_id)
            .map(|(_, conversation, message_id)| (conversation, message_id))
            .unwrap_or((ActiveConversation::None, None));
        let id = attachment_id
            .parse()
            .unwrap_or_else(|_| AttachmentId::from_cdn_key(attachment_id));
        client.audio_track_started(&id, &conversation, message_id.as_ref());
    }
    let attachment_id = attachment_id.to_owned();
    let _ = weak.upgrade_in_event_loop(move |w| {
        let pane = w.global::<crate::ChatPane>();
        pane.set_current_playing_attachment_id(attachment_id.into());
        pane.set_is_media_playing(true);
        show_track(&w);
        super::timers::start_audio_progress_timer(&w);
    });
}

/// Push the loaded track's labels and the strip's visibility. UI thread.
fn show_track(w: &MainWindow) {
    let Some(client) = CLIENT.get() else { return };
    let pane = w.global::<crate::ChatPane>();
    if let Some(track) = client.now_playing() {
        pane.set_mini_player_title(track.title.into());
        pane.set_mini_player_sender(track.sender.into());
    }
    pane.set_mini_player_visible(client.now_playing_elsewhere());
}

/// Progress-timer tick while a track is loaded. UI thread.
pub(super) fn tick(w: &MainWindow) {
    let Some(client) = CLIENT.get() else { return };
    if client.now_playing().is_none() {
        // The session the track belonged to ended (logout, account switch).
        stop(w);
        return;
    }
    // The strip follows navigation: hidden inside the track's conversation.
    w.global::<crate::ChatPane>()
        .set_mini_player_visible(client.now_playing_elsewhere());
    if audio_state::playback_finished() {
        finished(w);
    }
}

/// The track played to its end: continue with the queue or put the player away.
fn finished(w: &MainWindow) {
    let Some(client) = CLIENT.get() else { return };
    let Some(next) = client.advance_audio_queue() else {
        stop(w);
        return;
    };
    audio_state::clear_state();
    let pane = w.global::<crate::ChatPane>();
    pane.set_current_playing_attachment_id("".into());
    pane.set_is_media_playing(false);
    pane.set_audio_progress(0.0);
    pane.set_audio_elapsed("0:00".into());
    show_track(w);
    super::attachment_view::play_queued_audio(w.as_weak(), client.clone(), next.attachment_id);
}

/// Stop playback and put the player away. UI thread.
pub(super) fn stop(w: &MainWindow) {
    if let Ok(mut guard) = audio_state::get_audio_player().lock() {
        if let Some(ref mut player) = *guard {
            player.stop();
        }
    }
    audio_state::clear_state();
    if let Some(client) = CLIENT.get() {
        client.clear_now_playing();
    }
    super::timers::stop_audio_progress_timer();
    let pane = w.global::<crate::ChatPane>();
    pane.set_mini_player_visible(false);
    pane.set_current_playing_attachment_id("".into());
    pane.set_is_media_playing(false);
    pane.set_audio_progress(0.0);
    pane.set_audio_elapsed("0:00".into());
}
