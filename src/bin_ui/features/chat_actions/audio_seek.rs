//! Audio seek callback.
//!
//! Handles seeking within audio playback.

use std::time::Duration;

use slint::SharedString;

use crate::bin_ui::audio_state;
use crate::MainWindow;

/// Wire audio seek callback.
pub fn wire_audio_seek(window: &MainWindow) {
    window.on_audio_seek(move |attachment_id: SharedString, position: f32| {
        let attachment_id_str = attachment_id.to_string();

        tracing::debug!("Audio seek: {} to position {:.2}", attachment_id_str, position);

        // Check if this attachment is currently playing
        let current_playing = audio_state::get_current_playing_id();
        if current_playing.as_deref() != Some(attachment_id.as_str()) {
            tracing::debug!("Seek ignored: {} is not currently playing", attachment_id_str);
            return;
        }

        // Get duration from audio state
        let duration = match audio_state::get_current_duration() {
            Some(d) => d,
            None => {
                tracing::warn!("No duration available for seeking");
                return;
            }
        };

        // Calculate seek position
        let seek_ms = (position as f64 * duration.as_millis() as f64) as u64;
        let seek_duration = Duration::from_millis(seek_ms);

        tracing::debug!("Seeking to {:?} (position {})", seek_duration, position);

        // Run seek in background thread to avoid blocking UI
        // rodio's skip_duration decodes linearly which is CPU-intensive
        tokio::task::spawn_blocking(move || {
            let player_arc = audio_state::get_audio_player();
            match player_arc.lock() {
                Ok(mut player_guard) => {
                    if let Some(ref mut player) = *player_guard {
                        // We need to reload and seek because rodio doesn't support native seek
                        // The AudioPlayer.seek() method handles this internally
                        if let Err(e) = player.seek(seek_duration) {
                            tracing::error!("Failed to seek audio: {}", e);
                        } else {
                            // Update position tracking in audio_state
                            audio_state::set_seek_position(seek_duration);
                            tracing::info!("Audio seek successful to {:?}", seek_duration);
                        }
                    } else {
                        tracing::error!("Audio player not initialized");
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to lock audio player: {}", e);
                }
            }
        });
    });
}
