//! Audio player.
//!
//! Audio playback using rodio for voice messages and audio files.

use rodio::{Decoder, OutputStream, OutputStreamBuilder, Sink, Source};
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::MediaError;

/// Audio playback state.
#[derive(Debug, Clone, PartialEq)]
pub enum PlaybackState {
    /// Audio is stopped (not loaded or finished).
    Stopped,
    /// Audio is currently playing.
    Playing {
        /// Current playback position.
        position: Duration,
        /// Total duration of the audio.
        duration: Duration,
    },
    /// Audio is paused.
    Paused {
        /// Position where playback was paused.
        position: Duration,
        /// Total duration of the audio.
        duration: Duration,
    },
}

/// Audio player using rodio.
///
/// Supports MP3, WAV, OGG, and FLAC formats.
pub struct AudioPlayer {
    /// Output stream (must be kept alive for playback).
    _stream: OutputStream,
    /// Current playback sink.
    sink: Option<Sink>,
    /// Currently loaded file path.
    current_file: Option<PathBuf>,
    /// Duration of the current audio file.
    current_duration: Option<Duration>,
    /// Whether playback is paused.
    is_paused: bool,
}

impl AudioPlayer {
    /// Creates a new audio player.
    ///
    /// # Errors
    ///
    /// Returns an error if the audio output stream cannot be created.
    pub fn new() -> Result<Self, MediaError> {
        let stream = OutputStreamBuilder::open_default_stream()
            .map_err(|e| MediaError::PlaybackError(format!("failed to create audio output: {}", e)))?;

        Ok(Self {
            _stream: stream,
            sink: None,
            current_file: None,
            current_duration: None,
            is_paused: false,
        })
    }

    /// Loads an audio file for playback.
    ///
    /// Returns the duration of the audio file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or decoded.
    pub fn load(&mut self, path: &Path) -> Result<Duration, MediaError> {
        // Stop any current playback
        self.stop();

        // Open and decode the file to get duration
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let source = Decoder::new(reader)
            .map_err(|e| MediaError::InvalidFormat(format!("failed to decode audio: {}", e)))?;

        // Get duration (may not be available for all formats)
        let duration = source.total_duration().unwrap_or(Duration::ZERO);

        self.current_file = Some(path.to_path_buf());
        self.current_duration = Some(duration);
        self.is_paused = false;

        Ok(duration)
    }

    /// Starts or resumes playback.
    ///
    /// # Errors
    ///
    /// Returns an error if no file is loaded or playback fails.
    pub fn play(&mut self) -> Result<(), MediaError> {
        let path = self.current_file.as_ref().ok_or_else(|| {
            MediaError::PlaybackError("no audio file loaded".to_string())
        })?;

        // If paused, just resume
        if self.is_paused {
            if let Some(ref sink) = self.sink {
                sink.play();
                self.is_paused = false;
                return Ok(());
            }
        }

        // Create new sink and start playback
        let sink = Sink::connect_new(self._stream.mixer());

        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let source = Decoder::new(reader)
            .map_err(|e| MediaError::InvalidFormat(format!("failed to decode audio: {}", e)))?;

        sink.append(source);
        self.sink = Some(sink);
        self.is_paused = false;

        Ok(())
    }

    /// Pauses playback.
    pub fn pause(&mut self) {
        if let Some(ref sink) = self.sink {
            sink.pause();
            self.is_paused = true;
        }
    }

    /// Stops playback and resets to the beginning.
    pub fn stop(&mut self) {
        if let Some(sink) = self.sink.take() {
            sink.stop();
        }
        self.is_paused = false;
    }

    /// Seeks to a specific position.
    ///
    /// Note: Due to rodio limitations, this stops and restarts playback from the new position.
    ///
    /// # Errors
    ///
    /// Returns an error if no file is loaded or seek fails.
    pub fn seek(&mut self, position: Duration) -> Result<(), MediaError> {
        let path = self.current_file.as_ref().ok_or_else(|| {
            MediaError::PlaybackError("no audio file loaded".to_string())
        })?.clone();

        let was_playing = self.is_playing();

        // Stop current playback
        self.stop();

        // Create new sink
        let sink = Sink::connect_new(self._stream.mixer());

        // Open and decode file
        let file = File::open(&path)?;
        let reader = BufReader::new(file);
        let source = Decoder::new(reader)
            .map_err(|e| MediaError::InvalidFormat(format!("failed to decode audio: {}", e)))?;

        // Skip to position
        let skipped = source.skip_duration(position);
        sink.append(skipped);

        if !was_playing {
            sink.pause();
            self.is_paused = true;
        }

        self.sink = Some(sink);

        Ok(())
    }

    /// Sets the playback volume.
    ///
    /// # Arguments
    ///
    /// * `volume` - Volume level from 0.0 (mute) to 1.0 (full volume).
    pub fn set_volume(&mut self, volume: f32) {
        if let Some(ref sink) = self.sink {
            sink.set_volume(volume.clamp(0.0, 1.0));
        }
    }

    /// Gets the current playback state.
    pub fn state(&self) -> PlaybackState {
        let duration = match self.current_duration {
            Some(d) => d,
            None => return PlaybackState::Stopped,
        };

        match &self.sink {
            Some(sink) if sink.empty() => PlaybackState::Stopped,
            Some(_) if self.is_paused => PlaybackState::Paused {
                position: Duration::ZERO, // rodio doesn't expose position
                duration,
            },
            Some(_) => PlaybackState::Playing {
                position: Duration::ZERO, // rodio doesn't expose position
                duration,
            },
            None => PlaybackState::Stopped,
        }
    }

    /// Returns true if audio is currently playing.
    pub fn is_playing(&self) -> bool {
        matches!(self.state(), PlaybackState::Playing { .. })
    }

    /// Returns true if audio is paused.
    pub fn is_paused(&self) -> bool {
        matches!(self.state(), PlaybackState::Paused { .. })
    }
}
