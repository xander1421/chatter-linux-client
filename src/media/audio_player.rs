//! Audio player.
//!
//! Audio playback using rodio for voice messages and audio files.

use rodio::{Decoder, OutputStream, OutputStreamBuilder, Sink, Source};
use std::fs::File;
use std::io::{BufReader, Cursor};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// Source of audio data.
#[derive(Clone)]
enum AudioSource {
    /// Audio loaded from a file path.
    File(PathBuf),
    /// Audio loaded from memory (secure in-memory playback).
    Memory(Arc<Vec<u8>>),
}

/// Audio player using rodio.
///
/// Supports MP3, WAV, OGG, and FLAC formats.
/// Can load audio from files or from memory for secure playback.
pub struct AudioPlayer {
    /// Output stream (must be kept alive for playback).
    _stream: OutputStream,
    /// Current playback sink.
    sink: Option<Sink>,
    /// Currently loaded audio source.
    current_source: Option<AudioSource>,
    /// Duration of the current audio file.
    current_duration: Option<Duration>,
    /// Whether playback is paused.
    is_paused: bool,
    /// Timestamp when playback started (for position tracking).
    play_start: Option<Instant>,
    /// Accumulated offset when paused (for position tracking).
    pause_offset: Duration,
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
            current_source: None,
            current_duration: None,
            is_paused: false,
            play_start: None,
            pause_offset: Duration::ZERO,
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

        self.current_source = Some(AudioSource::File(path.to_path_buf()));
        self.current_duration = Some(duration);
        self.is_paused = false;
        self.play_start = None;
        self.pause_offset = Duration::ZERO;

        Ok(duration)
    }

    /// Loads audio from memory for secure playback.
    ///
    /// Used for playing attachments that are decrypted in memory.
    /// The audio data never touches the filesystem.
    ///
    /// # Arguments
    ///
    /// * `data` - Audio data bytes (MP3, WAV, OGG, FLAC)
    ///
    /// # Returns
    ///
    /// Duration of the audio.
    ///
    /// # Errors
    ///
    /// Returns an error if the audio cannot be decoded.
    pub fn load_from_memory(&mut self, data: Vec<u8>) -> Result<Duration, MediaError> {
        // Stop any current playback
        self.stop();

        // Wrap in Arc for sharing
        let data = Arc::new(data);

        // Decode to get duration
        let cursor = Cursor::new(data.as_ref().clone());
        let source = Decoder::new(cursor)
            .map_err(|e| MediaError::InvalidFormat(format!("failed to decode audio from memory: {}", e)))?;

        let duration = source.total_duration().unwrap_or(Duration::ZERO);

        self.current_source = Some(AudioSource::Memory(data));
        self.current_duration = Some(duration);
        self.is_paused = false;
        self.play_start = None;
        self.pause_offset = Duration::ZERO;

        Ok(duration)
    }

    /// Get duration from in-memory audio data.
    ///
    /// Static method for extracting duration without creating a full player.
    pub fn get_duration_from_memory(data: &[u8]) -> Option<Duration> {
        let cursor = Cursor::new(data.to_vec());
        let source = Decoder::new(cursor).ok()?;
        source.total_duration()
    }

    /// Starts or resumes playback.
    ///
    /// # Errors
    ///
    /// Returns an error if no audio is loaded or playback fails.
    pub fn play(&mut self) -> Result<(), MediaError> {
        let source = self.current_source.as_ref().ok_or_else(|| {
            MediaError::PlaybackError("no audio loaded".to_string())
        })?.clone();

        // If paused, just resume
        if self.is_paused {
            if let Some(ref sink) = self.sink {
                sink.play();
                self.is_paused = false;
                // Resume position tracking from where we paused
                self.play_start = Some(Instant::now());
                return Ok(());
            }
        }

        // Create new sink and start playback
        let sink = Sink::connect_new(self._stream.mixer());

        match source {
            AudioSource::File(ref path) => {
                let file = File::open(path)?;
                let reader = BufReader::new(file);
                let decoder = Decoder::new(reader)
                    .map_err(|e| MediaError::InvalidFormat(format!("failed to decode audio: {}", e)))?;
                sink.append(decoder);
            }
            AudioSource::Memory(ref data) => {
                let cursor = Cursor::new(data.as_ref().clone());
                let decoder = Decoder::new(cursor)
                    .map_err(|e| MediaError::InvalidFormat(format!("failed to decode audio from memory: {}", e)))?;
                sink.append(decoder);
            }
        }

        self.sink = Some(sink);
        self.is_paused = false;
        // Start position tracking
        self.play_start = Some(Instant::now());
        self.pause_offset = Duration::ZERO;

        Ok(())
    }

    /// Pauses playback.
    pub fn pause(&mut self) {
        if let Some(ref sink) = self.sink {
            sink.pause();
            self.is_paused = true;
            // Accumulate elapsed time into pause_offset
            if let Some(start) = self.play_start.take() {
                self.pause_offset += start.elapsed();
            }
        }
    }

    /// Stops playback and resets to the beginning.
    pub fn stop(&mut self) {
        if let Some(sink) = self.sink.take() {
            sink.stop();
        }
        self.is_paused = false;
        self.play_start = None;
        self.pause_offset = Duration::ZERO;
    }

    /// Seeks to a specific position.
    ///
    /// Note: Due to rodio limitations, this stops and restarts playback from the new position.
    ///
    /// # Errors
    ///
    /// Returns an error if no audio is loaded or seek fails.
    pub fn seek(&mut self, position: Duration) -> Result<(), MediaError> {
        let source = self.current_source.as_ref().ok_or_else(|| {
            MediaError::PlaybackError("no audio loaded".to_string())
        })?.clone();

        let was_playing = self.is_playing();

        // Stop current playback
        self.stop();

        // Create new sink
        let sink = Sink::connect_new(self._stream.mixer());

        match source {
            AudioSource::File(ref path) => {
                let file = File::open(path)?;
                let reader = BufReader::new(file);
                let decoder = Decoder::new(reader)
                    .map_err(|e| MediaError::InvalidFormat(format!("failed to decode audio: {}", e)))?;
                let skipped = decoder.skip_duration(position);
                sink.append(skipped);
            }
            AudioSource::Memory(ref data) => {
                let cursor = Cursor::new(data.as_ref().clone());
                let decoder = Decoder::new(cursor)
                    .map_err(|e| MediaError::InvalidFormat(format!("failed to decode audio from memory: {}", e)))?;
                let skipped = decoder.skip_duration(position);
                sink.append(skipped);
            }
        }

        if !was_playing {
            sink.pause();
            self.is_paused = true;
            // Track position from seek point when paused
            self.pause_offset = position;
            self.play_start = None;
        } else {
            self.is_paused = false;
            // Track position from seek point when playing
            self.pause_offset = position;
            self.play_start = Some(Instant::now());
        }

        self.sink = Some(sink);
        // Restore source since stop() cleared it
        self.current_source = Some(source);

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

    /// Returns the current playback position.
    ///
    /// This tracks position using timestamps since rodio doesn't expose it.
    pub fn position(&self) -> Duration {
        let current = if let Some(start) = self.play_start {
            self.pause_offset + start.elapsed()
        } else {
            self.pause_offset
        };

        // Clamp to duration if available
        if let Some(duration) = self.current_duration {
            current.min(duration)
        } else {
            current
        }
    }

    /// Returns the total duration of the loaded audio.
    pub fn duration(&self) -> Option<Duration> {
        self.current_duration
    }

    /// Returns true if audio is currently playing.
    pub fn is_playing(&self) -> bool {
        matches!(self.state(), PlaybackState::Playing { .. })
    }

    /// Returns true if audio is paused.
    pub fn is_paused(&self) -> bool {
        matches!(self.state(), PlaybackState::Paused { .. })
    }

    /// Get the duration of an audio file without creating a full player.
    ///
    /// This is a static method for extracting duration during upload.
    pub fn get_duration(path: &Path) -> Option<Duration> {
        let file = File::open(path).ok()?;
        let reader = BufReader::new(file);
        let source = Decoder::new(reader).ok()?;
        source.total_duration()
    }
}
