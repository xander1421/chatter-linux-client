//! Global audio playback state.
//!
//! Tracks which audio attachment is currently playing so the UI can reflect this.

use std::sync::{Arc, Mutex, OnceLock};

use chatter_linux::media::AudioPlayer;

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

/// Update the audio state when playback starts.
pub fn set_playing(attachment_id: &str) {
    if let Ok(mut state) = get_audio_state().lock() {
        state.current_id = Some(attachment_id.to_string());
        state.is_playing = true;
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
    }
}
