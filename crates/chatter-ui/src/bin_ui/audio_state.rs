//! Global audio playback state.
//!
//! Playback outlives the conversation it started in: the player keeps going
//! when the user navigates elsewhere, and a strip in the chrome keeps the
//! controls reachable. That means this state carries not just *what* is
//! playing but *where it came from* — enough to label the strip, jump back to
//! the source message, and find the next voice note when this one ends.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use chatter_core::domain::ids::ChannelId;
use chatter_core::domain::message::ConversationType;
use chatter_core::media::AudioPlayer;

/// Global audio player instance.
static AUDIO_PLAYER: OnceLock<Arc<Mutex<Option<AudioPlayer>>>> = OnceLock::new();

/// Global state tracking what is currently loaded, if anything.
static NOW_PLAYING: OnceLock<Arc<Mutex<Option<NowPlaying>>>> = OnceLock::new();

/// The loaded track plus the context needed to control it from outside its
/// conversation. Present whenever the player strip should be visible —
/// including while paused.
#[derive(Clone, Debug)]
pub struct NowPlaying {
    /// ID of the loaded audio attachment.
    pub attachment_id: String,
    /// File name, shown as the strip's title.
    pub filename: String,
    /// Sender's display name, shown next to the title. May be empty.
    pub sender_name: String,
    /// Message the attachment hangs off — the tap-to-jump target.
    pub message_id: String,
    /// Sender timestamp of that message; the cursor for finding the next track.
    pub sent_at: u64,
    /// Conversation the track came from. `None` for conversations that don't
    /// register as an active server conversation (e.g. mesh, which carries no
    /// attachments anyway); that only disables autoplay-next and jump, never
    /// playback itself.
    pub conversation_id: Option<ChannelId>,
    /// Conversation type, needed alongside the id for any storage lookup.
    pub conversation_type: Option<ConversationType>,
    /// Total duration of the track.
    pub total_duration: Duration,
    /// Whether it is playing (vs paused).
    pub is_playing: bool,
}

/// Get or initialize the global audio player.
pub fn get_audio_player() -> Arc<Mutex<Option<AudioPlayer>>> {
    AUDIO_PLAYER
        .get_or_init(|| {
            let player = AudioPlayer::new().ok();
            Arc::new(Mutex::new(player))
        })
        .clone()
}

/// Get or initialize the global now-playing slot.
fn slot() -> Arc<Mutex<Option<NowPlaying>>> {
    NOW_PLAYING
        .get_or_init(|| Arc::new(Mutex::new(None)))
        .clone()
}

/// Snapshot of what is loaded, if anything.
pub fn get_now_playing() -> Option<NowPlaying> {
    slot().lock().ok().and_then(|state| state.clone())
}

/// Check if a specific attachment is currently playing.
///
/// This is the main function used by converters when building attachment data.
pub fn is_attachment_playing(attachment_id: &str) -> bool {
    get_now_playing()
        .map(|track| track.is_playing && track.attachment_id == attachment_id)
        .unwrap_or(false)
}

/// Install a newly loaded track as the current one.
pub fn start_track(track: NowPlaying) {
    if let Ok(mut state) = slot().lock() {
        *state = Some(track);
    }
}

/// Update the audio state when playback pauses.
pub fn set_paused() {
    if let Ok(mut state) = slot().lock() {
        if let Some(ref mut track) = *state {
            track.is_playing = false;
        }
    }
}

/// Update the audio state when playback resumes.
pub fn set_resumed() {
    if let Ok(mut state) = slot().lock() {
        if let Some(ref mut track) = *state {
            track.is_playing = true;
        }
    }
}

/// Stop playback outright and forget the track.
///
/// Playback deliberately survives conversation switches, so the only things
/// that end it are the user dismissing the player and the session itself
/// ending (logout).
pub fn stop_playback() {
    if let Ok(mut guard) = get_audio_player().lock() {
        if let Some(ref mut player) = *guard {
            player.stop();
        }
    }
    clear_state();
}

/// Clear the audio state when playback stops.
pub fn clear_state() {
    if let Ok(mut state) = slot().lock() {
        *state = None;
    }
}

/// Get the ID of the currently loaded attachment, if any.
pub fn get_current_playing_id() -> Option<String> {
    get_now_playing().map(|track| track.attachment_id)
}

/// Get the current playback progress as a float from 0.0 to 1.0.
///
/// Returns 0.0 if nothing is loaded or duration is unknown.
pub fn get_progress() -> f32 {
    if get_now_playing().is_none() {
        return 0.0;
    }

    let player_arc = get_audio_player();
    let Ok(player_guard) = player_arc.lock() else {
        return 0.0;
    };
    let Some(ref player) = *player_guard else {
        return 0.0;
    };
    let Some(duration) = player.duration() else {
        return 0.0;
    };
    if duration.as_secs_f32() <= 0.0 {
        return 0.0;
    }

    (player.position().as_secs_f32() / duration.as_secs_f32()).clamp(0.0, 1.0)
}

/// Get the current playback position.
///
/// Returns Duration::ZERO if nothing is playing.
pub fn get_position() -> Duration {
    let player_arc = get_audio_player();

    if let Ok(player_guard) = player_arc.lock() {
        if let Some(ref player) = *player_guard {
            return player.position();
        }
    }

    Duration::ZERO
}

/// Get the total duration of the currently loaded audio.
pub fn get_current_duration() -> Option<Duration> {
    get_now_playing()
        .map(|track| track.total_duration)
        .filter(|duration| *duration > Duration::ZERO)
}

/// Format duration as "M:SS" string.
pub fn format_duration(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    let minutes = total_secs / 60;
    let seconds = total_secs % 60;
    format!("{}:{:02}", minutes, seconds)
}
