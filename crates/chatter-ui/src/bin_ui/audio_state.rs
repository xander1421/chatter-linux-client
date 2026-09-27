//! Global audio playback state.
//!
//! Tracks which audio attachment is currently playing so the UI can reflect this.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use chatter_core::media::AudioPlayer;

/// Global audio player instance.
static AUDIO_PLAYER: OnceLock<Arc<Mutex<Option<AudioPlayer>>>> = OnceLock::new();

/// Global state tracking which attachment is currently playing.
static AUDIO_STATE: OnceLock<Arc<Mutex<AudioState>>> = OnceLock::new();

/// Audio playback state.
#[derive(Clone, Debug, Default)]
pub struct AudioState {
    /// ID of the currently loaded/playing audio attachment, if any.
    pub current_id: Option<String>,
    /// Whether audio is currently playing (vs paused).
    pub is_playing: bool,
    /// Total duration of the current audio.
    pub total_duration: Duration,
}

impl AudioState {
    /// Check if a specific attachment ID is currently playing.
    pub fn is_attachment_playing(&self, attachment_id: &str) -> bool {
        self.current_id.as_deref() == Some(attachment_id) && self.is_playing
    }
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

/// Get or initialize the global audio state.
pub fn get_audio_state() -> Arc<Mutex<AudioState>> {
    AUDIO_STATE
        .get_or_init(|| Arc::new(Mutex::new(AudioState::default())))
        .clone()
}

/// Check if a specific attachment is currently playing.
///
/// This is the main function used by converters when building attachment data.
pub fn is_attachment_playing(attachment_id: &str) -> bool {
    if let Ok(state) = get_audio_state().lock() {
        state.is_attachment_playing(attachment_id)
    } else {
        false
    }
}

/// Update the audio state when playback starts with known duration.
pub fn set_playing_with_duration(attachment_id: &str, duration: Duration) {
    if let Ok(mut state) = get_audio_state().lock() {
        state.current_id = Some(attachment_id.to_string());
        state.is_playing = true;
        state.total_duration = duration;
    }
}

/// Update the audio state when playback pauses.
pub fn set_paused() {
    if let Ok(mut state) = get_audio_state().lock() {
        state.is_playing = false;
    }
}

/// Update the audio state when playback resumes.
pub fn set_resumed() {
    if let Ok(mut state) = get_audio_state().lock() {
        state.is_playing = true;
    }
}

/// Clear the audio state when playback stops.
pub fn clear_state() {
    if let Ok(mut state) = get_audio_state().lock() {
        state.current_id = None;
        state.is_playing = false;
        state.total_duration = Duration::ZERO;
    }
}

/// Get the ID of the currently playing attachment, if any.
pub fn get_current_playing_id() -> Option<String> {
    if let Ok(state) = get_audio_state().lock() {
        state.current_id.clone()
    } else {
        None
    }
}

/// Query the player's end-of-stream state; rodio resets position when its
/// queue drains, so duration progress alone cannot detect completion.
pub fn playback_finished() -> bool {
    let player_arc = get_audio_player();
    let state_arc = get_audio_state();
    if let (Ok(player), Ok(state)) = (player_arc.lock(), state_arc.lock()) {
        return state.current_id.is_some() && state.is_playing
            && player.as_ref().is_some_and(|player| !player.is_playing() && !player.is_paused());
    }
    false
}

/// Get the current playback progress as a float from 0.0 to 1.0.
///
/// Returns 0.0 if nothing is playing or duration is unknown.
pub fn get_progress() -> f32 {
    let player_arc = get_audio_player();
    let state_arc = get_audio_state();

    if let (Ok(player_guard), Ok(state_guard)) = (player_arc.lock(), state_arc.lock()) {
        if !state_guard.is_playing && state_guard.current_id.is_none() {
            return 0.0;
        }

        if let Some(ref player) = *player_guard {
            if let Some(duration) = player.duration() {
                if duration.as_secs_f32() > 0.0 {
                    let position = player.position();
                    return (position.as_secs_f32() / duration.as_secs_f32()).clamp(0.0, 1.0);
                }
            }
        }
    }

    0.0
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

/// Get the total duration of the currently playing audio.
pub fn get_current_duration() -> Option<Duration> {
    if let Ok(state) = get_audio_state().lock() {
        if state.current_id.is_some() && state.total_duration > Duration::ZERO {
            return Some(state.total_duration);
        }
    }
    None
}

/// Update position tracking after a seek.
///
/// This is a no-op since AudioPlayer.seek() internally handles position reset.
/// The function exists for semantic clarity at call sites.
pub fn set_seek_position(_position: Duration) {
    // AudioPlayer.seek() resets pause_offset and play_start internally,
    // so we don't need to track position separately here.
    // This function exists for clarity at call sites.
}

/// Format duration as "M:SS" string.
pub fn format_duration(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    let minutes = total_secs / 60;
    let seconds = total_secs % 60;
    format!("{}:{:02}", minutes, seconds)
}
