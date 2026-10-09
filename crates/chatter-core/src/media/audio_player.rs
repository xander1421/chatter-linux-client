//! Audio player.
//!
//! Audio playback using rodio for voice messages and audio files.
//! Uses streaming playback with native seeking support.

use rodio::{Decoder, OutputStream, OutputStreamBuilder, Sink, Source, source::SeekError};
use std::fs::File;
use std::io::{BufReader, Cursor, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Source of audio data.
#[derive(Clone)]
enum AudioSource {
    /// Audio loaded from a file path.
    File(PathBuf),
    /// Raw audio data for streaming playback (fast start).
    Stream {
        data: Arc<Vec<u8>>,
        mime_type: Option<String>,
    },
    /// Encrypted attachment decrypted on demand (no full plaintext in memory).
    Range {
        dec: Arc<crate::services::attachment::CbcRangeDecryptor>,
        mime_type: Option<String>,
    },
}

/// Audio player using rodio.
///
/// Supports MP3, WAV, OGG, and FLAC formats.
/// Can load audio from files or from memory for secure playback.
pub struct AudioPlayer {
    /// Output stream (must be kept alive for playback).
    stream: Option<OutputStream>,
    /// Output errors are reported on the audio thread; retire it on the caller.
    output_failed: Arc<AtomicBool>,
    /// Current playback sink.
    sink: Option<Sink>,
    /// Currently loaded audio source.
    current_source: Option<AudioSource>,
    /// Duration of the current audio file.
    current_duration: Option<Duration>,
}

// cpal's CoreAudio backend holds a `Box<dyn FnMut()>` property-listener callback
// without a Send bound, so `OutputStream` is !Send on macOS. All access to
// AudioPlayer goes through the global Mutex in audio_state.rs, which already
// serializes it, so moving it across threads is sound.
unsafe impl Send for AudioPlayer {}

impl AudioPlayer {
    /// Creates a new audio player.
    ///
    /// # Errors
    ///
    /// Hardware output is opened lazily by `play`, which reports output errors.
    pub fn new() -> Result<Self, MediaError> {
        Ok(Self {
            stream: None,
            output_failed: Arc::new(AtomicBool::new(false)),
            sink: None,
            current_source: None,
            current_duration: None,
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
        let source = decode_catching_panics(reader)?;

        // Get duration (may not be available for all formats)
        let duration = source.total_duration().unwrap_or(Duration::ZERO);

        self.current_source = Some(AudioSource::File(path.to_path_buf()));
        self.current_duration = Some(duration);

        Ok(duration)
    }

    /// Loads audio from memory for secure playback.
    ///
    /// Used for playing attachments that are decrypted in memory.
    /// Uses streaming playback with native seeking support.
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
        self.load_from_memory_with_mime(data, None)
    }

    /// Loads audio from memory with optional MIME type hint.
    ///
    /// # Arguments
    ///
    /// * `data` - Audio data bytes
    /// * `mime_type` - Optional MIME type hint (e.g., "audio/mpeg")
    pub fn load_from_memory_with_mime(
        &mut self,
        data: Vec<u8>,
        mime_type: Option<String>,
    ) -> Result<Duration, MediaError> {
        // Stop any current playback
        self.stop();

        let data_len = data.len();
        let data_arc = Arc::new(data);

        // Build decoder with seekable hint and byte length for proper seeking
        let cursor = Cursor::new(data_arc.as_ref().clone());
        let decoder = self.build_seekable_decoder(cursor, data_len as u64, mime_type.as_deref())?;

        // Get duration
        let duration = decoder.total_duration().unwrap_or(Duration::ZERO);

        tracing::info!(
            data_size = data_len,
            ?duration,
            ?mime_type,
            "Loaded audio for streaming playback"
        );

        self.current_source = Some(AudioSource::Stream {
            data: data_arc,
            mime_type,
        });
        self.current_duration = Some(duration);

        Ok(duration)
    }

    /// Loads audio from an authenticated range decryptor (streaming).
    ///
    /// Decrypts on demand — no full plaintext buffer is held for the lifetime of
    /// playback (unlike `load_from_memory`). Used for large audio attachments.
    pub fn load_from_range(
        &mut self,
        dec: Arc<crate::services::attachment::CbcRangeDecryptor>,
        mime_type: Option<String>,
    ) -> Result<Duration, MediaError> {
        self.stop();

        let reader = crate::services::attachment::CbcRangeReader::new(dec.clone());
        let byte_len = reader.len();
        let decoder = self.build_seekable_decoder(reader, byte_len, mime_type.as_deref())?;
        let duration = decoder.total_duration().unwrap_or(Duration::ZERO);

        tracing::info!(
            data_size = byte_len,
            ?duration,
            ?mime_type,
            "Loaded audio for streaming playback (range decryptor)"
        );

        self.current_source = Some(AudioSource::Range { dec, mime_type });
        self.current_duration = Some(duration);

        Ok(duration)
    }

    /// Build a decoder with seeking enabled.
    fn build_seekable_decoder<R: Read + Seek + Send + Sync + 'static>(
        &self,
        data: R,
        byte_len: u64,
        mime_type: Option<&str>,
    ) -> Result<Decoder<R>, MediaError> {
        let mut builder = Decoder::builder()
            .with_data(data)
            .with_seekable(true)
            .with_byte_len(byte_len);

        if let Some(mime) = mime_type {
            builder = builder.with_mime_type(mime);
        }

        // Symphonia's decoders can panic on malformed frames (e.g. a subtract-
        // with-overflow in symphonia-bundle-mp3's demuxer on certain MP3s).
        // Catch it here rather than let it unwind through the caller's
        // Mutex<AudioPlayer> guard and poison it for every future playback.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| builder.build())) {
            Ok(Ok(decoder)) => Ok(decoder),
            Ok(Err(e)) => Err(MediaError::InvalidFormat(format!(
                "failed to decode audio: {}",
                e
            ))),
            Err(_) => Err(MediaError::InvalidFormat(
                "failed to decode audio: decoder panicked on malformed data".to_string(),
            )),
        }
    }

    /// Get duration from in-memory audio data.
    ///
    /// Static method for extracting duration without creating a full player.
    pub fn get_duration_from_memory(data: &[u8]) -> Option<Duration> {
        let cursor = Cursor::new(data.to_vec());
        let source = decode_catching_panics(cursor).ok()?;
        source.total_duration()
    }

    /// Starts or resumes playback.
    ///
    /// # Errors
    ///
    /// Returns an error if no audio is loaded or playback fails.
    pub fn play(&mut self) -> Result<(), MediaError> {
        if self.current_source.is_none() {
            return Err(MediaError::PlaybackError("no audio loaded".to_string()));
        }
        if self.output_failed.load(Ordering::Acquire) {
            tracing::warn!("Retiring failed audio output; replay starts at the beginning");
            self.stop();
        }
        // If we have a sink and it's paused, just resume
        if let Some(ref sink) = self.sink {
            if sink.is_paused() {
                sink.play();
                return Ok(());
            }
            // If sink exists but not paused, it might be empty - recreate
            if !sink.empty() {
                return Ok(()); // Already playing
            }
        }

        // Need to create new sink and start playback
        let source = self
            .current_source
            .as_ref()
            .ok_or_else(|| MediaError::PlaybackError("no audio loaded".to_string()))?
            .clone();

        if self.stream.is_none() {
            // Each output owns its flag, so an old callback cannot invalidate
            // a replacement stream. Never close/reopen on the audio thread.
            let failed = Arc::new(AtomicBool::new(false));
            let callback_failed = failed.clone();
            let stream = OutputStreamBuilder::from_default_device()
                .map_err(|e| {
                    MediaError::PlaybackError(format!("failed to select audio output: {e}"))
                })?
                .with_error_callback(move |error| {
                    callback_failed.store(true, Ordering::Release);
                    tracing::error!(%error, "Attachment audio output stream failed");
                })
                .open_stream_or_fallback()
                .map_err(|e| {
                    MediaError::PlaybackError(format!("failed to create audio output: {e}"))
                })?;
            self.output_failed = failed;
            self.stream = Some(stream);
        }
        let sink = Sink::connect_new(
            self.stream
                .as_ref()
                .ok_or_else(|| MediaError::PlaybackError("no audio output".to_string()))?
                .mixer(),
        );

        match source {
            AudioSource::File(ref path) => {
                let file = File::open(path)?;
                let metadata = file.metadata()?;
                let byte_len = metadata.len();
                let reader = BufReader::new(file);
                let decoder = self.build_seekable_decoder(reader, byte_len, None)?;
                sink.append(decoder);
            }
            AudioSource::Stream {
                ref data,
                ref mime_type,
            } => {
                tracing::info!(data_size = data.len(), "Streaming audio from memory");
                let byte_len = data.len() as u64;
                let cursor = Cursor::new(data.as_ref().clone());
                let decoder =
                    self.build_seekable_decoder(cursor, byte_len, mime_type.as_deref())?;
                sink.append(decoder);
            }
            AudioSource::Range {
                ref dec,
                ref mime_type,
            } => {
                let reader = crate::services::attachment::CbcRangeReader::new(dec.clone());
                let byte_len = reader.len();
                tracing::info!(data_size = byte_len, "Streaming audio from range decryptor");
                let decoder =
                    self.build_seekable_decoder(reader, byte_len, mime_type.as_deref())?;
                sink.append(decoder);
            }
        }

        self.sink = Some(sink);
        if self.output_failed.load(Ordering::Acquire) {
            self.stop();
            return Err(MediaError::PlaybackError(
                "audio output stream failed".to_string(),
            ));
        }
        Ok(())
    }

    /// Pauses playback.
    pub fn pause(&mut self) {
        if let Some(ref sink) = self.sink {
            sink.pause();
        }
    }

    /// Stops playback and resets to the beginning.
    pub fn stop(&mut self) {
        if let Some(sink) = self.sink.take() {
            sink.stop();
        }
        // A route change can disconnect Android's retained AAudio output.
        // Loading another source must open fresh hardware instead of queueing
        // it onto a mixer whose device callback no longer runs.
        self.stream.take();
    }

    /// Seeks to a specific position.
    ///
    /// Uses rodio's native `try_seek()` for efficient seeking.
    ///
    /// # Errors
    ///
    /// Returns an error if no audio is loaded or seek fails.
    pub fn seek(&mut self, position: Duration) -> Result<(), MediaError> {
        if self.output_failed.load(Ordering::Acquire) {
            return Err(MediaError::PlaybackError(
                "audio output stream failed".to_string(),
            ));
        }
        if self.current_source.is_none() {
            return Err(MediaError::PlaybackError("no audio loaded".to_string()));
        }

        // Use rodio's native try_seek
        if let Some(ref sink) = self.sink {
            match sink.try_seek(position) {
                Ok(()) => {
                    tracing::debug!(?position, "Seek successful");
                    return Ok(());
                }
                Err(SeekError::NotSupported { .. }) => {
                    tracing::warn!("Native seek not supported for this format");
                    return Err(MediaError::PlaybackError("seek not supported".to_string()));
                }
                Err(e) => {
                    return Err(MediaError::PlaybackError(format!("seek failed: {:?}", e)));
                }
            }
        }

        Err(MediaError::PlaybackError(
            "no active playback to seek".to_string(),
        ))
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
        if self.output_failed.load(Ordering::Acquire) {
            return PlaybackState::Stopped;
        }
        let duration = match self.current_duration {
            Some(d) => d,
            None => return PlaybackState::Stopped,
        };

        match &self.sink {
            Some(sink) if sink.empty() => PlaybackState::Stopped,
            Some(sink) if sink.is_paused() => PlaybackState::Paused {
                position: sink.get_pos(),
                duration,
            },
            Some(sink) => PlaybackState::Playing {
                position: sink.get_pos(),
                duration,
            },
            None => PlaybackState::Stopped,
        }
    }

    /// Returns the current playback position.
    ///
    /// Uses rodio's native `get_pos()` for accurate position tracking.
    pub fn position(&self) -> Duration {
        if self.output_failed.load(Ordering::Acquire) {
            return Duration::ZERO;
        }
        self.sink
            .as_ref()
            .map(|s| s.get_pos())
            .unwrap_or(Duration::ZERO)
    }

    /// Returns the total duration of the loaded audio.
    pub fn duration(&self) -> Option<Duration> {
        self.current_duration
    }

    /// Returns true if audio is currently playing.
    pub fn is_playing(&self) -> bool {
        if self.output_failed.load(Ordering::Acquire) {
            return false;
        }
        self.sink
            .as_ref()
            .map(|s| !s.is_paused() && !s.empty())
            .unwrap_or(false)
    }

    /// Returns true if audio is paused.
    pub fn is_paused(&self) -> bool {
        if self.output_failed.load(Ordering::Acquire) {
            return false;
        }
        self.sink.as_ref().map(|s| s.is_paused()).unwrap_or(false)
    }

    /// Get the duration of an audio file without creating a full player.
    ///
    /// This is a static method for extracting duration during upload.
    pub fn get_duration(path: &Path) -> Option<Duration> {
        let file = File::open(path).ok()?;
        let reader = BufReader::new(file);
        let source = decode_catching_panics(reader).ok()?;
        source.total_duration()
    }
}

/// Build a `Decoder` while catching panics from the underlying format
/// probe/demuxer (e.g. symphonia-bundle-mp3's subtract-with-overflow on
/// certain malformed MP3 frames). A single bad file must return a decode
/// error, not unwind through a caller's `Mutex` guard and poison it for
/// every subsequent playback attempt.
fn decode_catching_panics<R: Read + Seek + Send + Sync + 'static>(
    reader: R,
) -> Result<Decoder<R>, MediaError> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Decoder::new(reader))) {
        Ok(Ok(decoder)) => Ok(decoder),
        Ok(Err(e)) => Err(MediaError::InvalidFormat(format!(
            "failed to decode audio: {}",
            e
        ))),
        Err(_) => Err(MediaError::InvalidFormat(
            "failed to decode audio: decoder panicked on malformed data".to_string(),
        )),
    }
}
