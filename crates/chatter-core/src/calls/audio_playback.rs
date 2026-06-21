//! Audio playback for remote participant streams.
//!
//! Receives audio frames from remote participants via LiveKit and plays
//! them through the local audio output device using CPAL.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleRate, StreamConfig};
use futures::StreamExt;
use livekit::track::RemoteAudioTrack;
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use parking_lot::RwLock;

use crate::error::CallError;

/// Audio output device info.
#[derive(Debug, Clone)]
pub struct AudioOutputDevice {
    /// Device name for display.
    pub name: String,
    /// Internal device identifier.
    pub id: String,
    /// Whether this is the default device.
    pub is_default: bool,
}

/// Manages audio playback for remote participants.
///
/// Uses lazy initialization - the CPAL output stream is only created when
/// the first audio track is added, avoiding unnecessary PipeWire/ALSA streams
/// when no calls are active.
pub struct AudioPlayback {
    /// Active audio streams from remote participants.
    streams: Arc<RwLock<HashMap<String, ActiveAudioStream>>>,
    /// CPAL output stream handle (lazily initialized).
    output_stream: Arc<RwLock<Option<cpal::Stream>>>,
    /// Audio sample buffer for mixing.
    sample_buffer: Arc<RwLock<Vec<f32>>>,
    /// Sample rate for output (populated on init).
    sample_rate: Arc<RwLock<u32>>,
    /// Number of output channels (populated on init).
    channels: Arc<RwLock<u16>>,
    /// Whether playback is active.
    is_active: Arc<AtomicBool>,
    /// Volume (0.0 to 1.0).
    volume: Arc<RwLock<f32>>,
    /// Participants whose audio is locally muted — frames decoded but
    /// discarded before mixing so the wire still flows but nothing plays.
    muted_peers: Arc<RwLock<HashSet<String>>>,
}

struct ActiveAudioStream {
    stop_signal: Arc<AtomicBool>,
}

impl AudioPlayback {
    /// Create a new audio playback manager.
    ///
    /// This does NOT create the CPAL output stream immediately. The stream
    /// is created lazily when `add_track()` is called for the first time.
    /// This prevents idle PipeWire/ALSA streams when no calls are active.
    pub fn new() -> Result<Self, CallError> {
        tracing::info!("Audio playback manager created (stream will be initialized on first use)");

        Ok(Self {
            streams: Arc::new(RwLock::new(HashMap::new())),
            output_stream: Arc::new(RwLock::new(None)),
            sample_buffer: Arc::new(RwLock::new(Vec::with_capacity(4096))),
            sample_rate: Arc::new(RwLock::new(48000)), // Default, updated on init
            channels: Arc::new(RwLock::new(2)),        // Default, updated on init
            is_active: Arc::new(AtomicBool::new(true)),
            volume: Arc::new(RwLock::new(1.0f32)),
            muted_peers: Arc::new(RwLock::new(HashSet::new())),
        })
    }

    /// Initialize the CPAL output stream (called lazily on first track).
    fn ensure_stream_initialized(&self) -> Result<(), CallError> {
        // Check if already initialized
        {
            let stream_guard = self.output_stream.read();
            if stream_guard.is_some() {
                return Ok(());
            }
        }

        let host = cpal::default_host();
        let output_device = host.default_output_device().ok_or_else(|| {
            CallError::DeviceError("No audio output device found".to_string())
        })?;

        let supported_config = output_device
            .default_output_config()
            .map_err(|e| CallError::DeviceError(format!("Failed to get output config: {}", e)))?;

        let sample_rate = supported_config.sample_rate().0;
        let channels = supported_config.channels();

        // Store actual sample rate and channels
        *self.sample_rate.write() = sample_rate;
        *self.channels.write() = channels;

        tracing::info!(
            "Audio playback: using device '{}' at {}Hz, {} channels",
            output_device.name().unwrap_or_else(|_| "unknown".to_string()),
            sample_rate,
            channels
        );

        let sample_buffer_clone = self.sample_buffer.clone();
        let is_active_clone = self.is_active.clone();
        let volume_clone = self.volume.clone();

        // Build output stream
        let config = StreamConfig {
            channels,
            sample_rate: SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let output_stream = output_device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    if !is_active_clone.load(Ordering::Relaxed) {
                        data.fill(0.0);
                        return;
                    }

                    let vol = *volume_clone.read();
                    let mut buffer = sample_buffer_clone.write();

                    // Take samples from buffer
                    let samples_to_take = data.len().min(buffer.len());
                    for (i, sample) in data.iter_mut().enumerate() {
                        if i < samples_to_take {
                            *sample = buffer[i] * vol;
                        } else {
                            *sample = 0.0;
                        }
                    }

                    // Remove consumed samples
                    if samples_to_take > 0 {
                        buffer.drain(0..samples_to_take);
                    }
                },
                |err| {
                    tracing::error!("Audio output error: {}", err);
                },
                None,
            )
            .map_err(|e| CallError::DeviceError(format!("Failed to build output stream: {}", e)))?;

        output_stream.play().map_err(|e| {
            CallError::DeviceError(format!("Failed to start audio playback: {}", e))
        })?;

        tracing::info!("Audio playback stream started");

        *self.output_stream.write() = Some(output_stream);
        Ok(())
    }

    /// Add a remote audio track for playback.
    pub fn add_track(&self, participant_id: String, track: RemoteAudioTrack) {
        tracing::info!("Adding audio track from participant: {}", participant_id);

        // Re-activate playback (may have been stopped by a previous call)
        self.is_active.store(true, Ordering::Relaxed);

        // Lazily initialize the CPAL stream on first track
        if let Err(e) = self.ensure_stream_initialized() {
            tracing::error!("Failed to initialize audio output: {}. Remote audio will not be played.", e);
            return;
        }

        let stop_signal = Arc::new(AtomicBool::new(false));
        let stop_signal_clone = stop_signal.clone();
        let sample_buffer = self.sample_buffer.clone();
        let muted_peers = self.muted_peers.clone();
        let target_sample_rate = *self.sample_rate.read() as i32;
        let target_channels = *self.channels.read() as usize;

        // Create native audio stream from the track
        let rtc_track = track.rtc_track();
        let mut audio_stream = NativeAudioStream::new(rtc_track, target_sample_rate, target_channels as i32);

        // Spawn task to receive and buffer audio frames
        let participant_id_clone = participant_id.clone();
        tokio::spawn(async move {
            tracing::debug!("Audio stream receiver started for {}", participant_id_clone);

            while !stop_signal_clone.load(Ordering::Relaxed) {
                match audio_stream.next().await {
                    Some(frame) => {
                        // Drain the wire even when locally muted — drop the
                        // frame after decode so the LiveKit ring buffer
                        // doesn't back up.
                        if muted_peers.read().contains(&participant_id_clone) {
                            continue;
                        }

                        // Convert audio frame to f32 samples
                        let samples: Vec<f32> = frame
                            .data
                            .iter()
                            .map(|&s| s as f32 / i16::MAX as f32)
                            .collect();

                        // Add to buffer
                        let mut buffer = sample_buffer.write();
                        buffer.extend(samples);

                        // Prevent buffer from growing too large (keep ~500ms of audio)
                        let max_samples = (target_sample_rate as usize) * target_channels / 2;
                        if buffer.len() > max_samples {
                            let excess = buffer.len() - max_samples;
                            buffer.drain(0..excess);
                        }
                    }
                    None => {
                        tracing::debug!("Audio stream ended for {}", participant_id_clone);
                        break;
                    }
                }
            }

            tracing::debug!("Audio stream receiver stopped for {}", participant_id_clone);
        });

        // Store the active stream
        let mut streams = self.streams.write();
        streams.insert(
            participant_id,
            ActiveAudioStream { stop_signal },
        );
    }

    /// Remove a participant's audio track.
    pub fn remove_track(&self, participant_id: &str) {
        tracing::info!("Removing audio track from participant: {}", participant_id);

        let mut streams = self.streams.write();
        if let Some(stream) = streams.remove(participant_id) {
            stream.stop_signal.store(true, Ordering::Relaxed);
        }
    }

    /// Set playback volume (0.0 to 1.0).
    pub fn set_volume(&self, vol: f32) {
        *self.volume.write() = vol.clamp(0.0, 1.0);
    }

    /// Locally mute/unmute a remote participant's audio. Frames keep
    /// decoding (so the connection stays healthy and unmute is instant);
    /// muted frames are simply discarded before mixing.
    pub fn set_peer_muted(&self, participant_id: &str, muted: bool) {
        let mut set = self.muted_peers.write();
        if muted {
            set.insert(participant_id.to_string());
        } else {
            set.remove(participant_id);
        }
    }

    pub fn is_peer_muted(&self, participant_id: &str) -> bool {
        self.muted_peers.read().contains(participant_id)
    }

    /// Get current volume.
    pub fn volume(&self) -> f32 {
        *self.volume.read()
    }

    /// Stop all playback.
    pub fn stop(&self) {
        self.is_active.store(false, Ordering::Relaxed);

        // Stop all active streams
        let mut streams = self.streams.write();
        for (_, stream) in streams.drain() {
            stream.stop_signal.store(true, Ordering::Relaxed);
        }

        // Clear the output stream so it's recreated on next call
        *self.output_stream.write() = None;

        // Drain any leftover samples
        self.sample_buffer.write().clear();

        tracing::info!("Audio playback stopped");
    }

    /// List available audio output devices.
    pub fn list_devices() -> Result<Vec<AudioOutputDevice>, CallError> {
        let host = cpal::default_host();
        let default_device = host.default_output_device();
        let default_name = default_device
            .as_ref()
            .and_then(|d| d.name().ok());

        let devices: Vec<AudioOutputDevice> = host
            .output_devices()
            .map_err(|e| CallError::DeviceError(format!("Failed to list output devices: {}", e)))?
            .filter_map(|device: Device| {
                let name = device.name().ok()?;
                let is_default = default_name.as_ref() == Some(&name);
                // Use device name as ID (CPAL doesn't provide a separate stable ID)
                let id = name.clone();
                Some(AudioOutputDevice { name, id, is_default })
            })
            .collect();

        Ok(devices)
    }
}

impl Drop for AudioPlayback {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_output_devices() {
        let devices = AudioPlayback::list_devices();
        assert!(devices.is_ok());
        let devices = devices.unwrap();
        println!("Found {} output devices", devices.len());
        for device in &devices {
            println!("  - {} (default: {})", device.name, device.is_default);
        }
    }
}
