//! Audio playback for remote participant streams.
//!
//! Receives audio frames from remote participants via LiveKit and plays
//! them through the local audio output device using CPAL.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[cfg(not(target_os = "android"))]
use cpal::traits::StreamTrait;
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::Device;
#[cfg(not(target_os = "android"))]
use cpal::{SampleRate, StreamConfig};
use futures::StreamExt;
use livekit::track::RemoteAudioTrack;
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use parking_lot::RwLock;

use crate::error::CallError;

/// Manages audio playback for remote participants.
///
/// Uses lazy initialization - the CPAL output stream is only created when
/// the first audio track is added, avoiding unnecessary PipeWire/ALSA streams
/// when no calls are active.
pub struct AudioPlayback {
    /// Active audio streams from remote participants.
    streams: Arc<RwLock<HashMap<String, ActiveAudioStream>>>,
    /// CPAL output stream handle (lazily initialized).
    output_stream: Arc<RwLock<Option<OutputStream>>>,
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
    /// Output device chosen by the user (CPAL device name); `None` = default.
    /// Unused on Android, where AudioManager owns the route.
    #[cfg_attr(target_os = "android", allow(dead_code))]
    output_device: RwLock<Option<String>>,
}

/// The live output stream. Desktop plays through CPAL on the chosen device;
/// Android opens a voice-communication stream through oboe (see
/// `ensure_stream_initialized`).
#[cfg(not(target_os = "android"))]
type OutputStream = cpal::Stream;
#[cfg(target_os = "android")]
type OutputStream = oboe::AudioStreamAsync<oboe::Output, VoiceOutput>;

/// Fill `data` from the mix buffer (silence when inactive or starved) and drop
/// what was consumed. Shared by the CPAL and oboe callbacks.
fn drain_mix(data: &mut [f32], buffer: &RwLock<Vec<f32>>, is_active: &AtomicBool, volume: &RwLock<f32>) {
    if !is_active.load(Ordering::Relaxed) {
        data.fill(0.0);
        return;
    }
    let vol = *volume.read();
    let mut buffer = buffer.write();
    let samples_to_take = data.len().min(buffer.len());
    for (i, sample) in data.iter_mut().enumerate() {
        *sample = if i < samples_to_take { buffer[i] * vol } else { 0.0 };
    }
    if samples_to_take > 0 {
        buffer.drain(0..samples_to_take);
    }
}

/// oboe data callback for the Android call stream: mono f32 from the mix.
#[cfg(target_os = "android")]
struct VoiceOutput {
    buffer: Arc<RwLock<Vec<f32>>>,
    is_active: Arc<AtomicBool>,
    volume: Arc<RwLock<f32>>,
}

#[cfg(target_os = "android")]
impl oboe::AudioOutputCallback for VoiceOutput {
    type FrameType = (f32, oboe::Mono);

    fn on_audio_ready(
        &mut self,
        _stream: &mut dyn oboe::AudioOutputStreamSafe,
        data: &mut [f32],
    ) -> oboe::DataCallbackResult {
        drain_mix(data, &self.buffer, &self.is_active, &self.volume);
        oboe::DataCallbackResult::Continue
    }

    fn on_error_after_close(&mut self, _stream: &mut dyn oboe::AudioOutputStreamSafe, error: oboe::Error) {
        tracing::error!("Audio output stream closed: {:?}", error);
    }
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
            output_device: RwLock::new(None),
        })
    }

    /// The output device called `name`, or the host default for `None`.
    #[cfg(not(target_os = "android"))]
    fn find_output_device(name: Option<&str>) -> Result<Device, CallError> {
        let host = cpal::default_host();
        match name {
            None => host.default_output_device().ok_or_else(|| {
                CallError::DeviceError("No audio output device found".to_string())
            }),
            Some(name) => host
                .output_devices()
                .map_err(|e| CallError::DeviceError(format!("Failed to list output devices: {}", e)))?
                .find(|d| d.name().is_ok_and(|n| n == name))
                .ok_or_else(|| CallError::DeviceError(format!("Output device '{}' not found", name))),
        }
    }

    /// Route remote audio to the output device called `name` (an id from
    /// [`Self::list_devices`]).
    ///
    /// Before the first track this only records the choice. Mid-call the
    /// stream is rebuilt on the new device at the CURRENT rate and channel
    /// count, because every running track was opened resampling to those; if
    /// the device refuses that format, the old stream is left playing and the
    /// error is returned.
    #[cfg(not(target_os = "android"))]
    pub fn set_output_device(&self, name: &str) -> Result<(), CallError> {
        let device = Self::find_output_device(Some(name))?;
        if self.output_stream.read().is_some() {
            let stream = self.build_stream(&device, *self.sample_rate.read(), *self.channels.read())?;
            *self.output_stream.write() = Some(stream);
        }
        *self.output_device.write() = Some(name.to_string());
        tracing::info!("Audio playback: output device set to '{}'", name);
        Ok(())
    }

    /// Android has no output-device list: the call route (earpiece, speaker,
    /// headset) is AudioManager's, chosen by the platform layer.
    #[cfg(target_os = "android")]
    pub fn set_output_device(&self, name: &str) -> Result<(), CallError> {
        Err(CallError::DeviceError(format!(
            "Output device '{name}' not selectable on Android; the call route is set by AudioManager"
        )))
    }

    /// Open the Android call stream (called lazily on first track).
    ///
    /// `USAGE_VOICE_COMMUNICATION` is what makes the stream follow the call
    /// route AudioManager selects (`setCommunicationDevice` /
    /// `setSpeakerphoneOn`); CPAL opens every stream as media, which plays on
    /// the loudspeaker whatever the route says. OpenSL ES rather than AAudio:
    /// an AAudio stream is DISCONNECTED whenever its routed device changes (every
    /// speaker toggle, every headset plug), while an OpenSL ES player is moved
    /// to the new device by the framework and keeps playing.
    #[cfg(target_os = "android")]
    fn ensure_stream_initialized(&self) -> Result<(), CallError> {
        use oboe::{AudioStream as _, AudioStreamBase as _};

        if self.output_stream.read().is_some() {
            return Ok(());
        }
        let mut stream = oboe::AudioStreamBuilder::default()
            .set_output()
            .set_f32()
            .set_mono()
            .set_audio_api(oboe::AudioApi::OpenSLES)
            .set_usage(oboe::Usage::VoiceCommunication)
            .set_content_type(oboe::ContentType::Speech)
            .set_sample_rate(48_000)
            .set_callback(VoiceOutput {
                buffer: self.sample_buffer.clone(),
                is_active: self.is_active.clone(),
                volume: self.volume.clone(),
            })
            .open_stream()
            .map_err(|e| CallError::DeviceError(format!("Failed to open voice output stream: {:?}", e)))?;
        *self.sample_rate.write() = stream.get_sample_rate() as u32;
        *self.channels.write() = 1;
        stream
            .start()
            .map_err(|e| CallError::DeviceError(format!("Failed to start audio playback: {:?}", e)))?;
        tracing::info!("Audio playback: voice-communication stream at {}Hz mono", stream.get_sample_rate());
        *self.output_stream.write() = Some(stream);
        Ok(())
    }

    /// Initialize the CPAL output stream (called lazily on first track).
    #[cfg(not(target_os = "android"))]
    fn ensure_stream_initialized(&self) -> Result<(), CallError> {
        // Check if already initialized
        {
            let stream_guard = self.output_stream.read();
            if stream_guard.is_some() {
                return Ok(());
            }
        }

        // A chosen device that has since disappeared falls back to the default
        // rather than leaving the call silent.
        let chosen = self.output_device.read().clone();
        let output_device = match Self::find_output_device(chosen.as_deref()) {
            Ok(device) => device,
            Err(e) if chosen.is_some() => {
                tracing::warn!("{}; using the default output", e);
                Self::find_output_device(None)?
            }
            Err(e) => return Err(e),
        };

        let supported_config = output_device
            .default_output_config()
            .map_err(|e| CallError::DeviceError(format!("Failed to get output config: {}", e)))?;

        let sample_rate = supported_config.sample_rate().0;
        let channels = supported_config.channels();

        // Store actual sample rate and channels
        *self.sample_rate.write() = sample_rate;
        *self.channels.write() = channels;

        let output_stream = self.build_stream(&output_device, sample_rate, channels)?;
        *self.output_stream.write() = Some(output_stream);
        Ok(())
    }

    /// Build and start a CPAL output stream on `device` that drains the mix
    /// buffer.
    #[cfg(not(target_os = "android"))]
    fn build_stream(&self, device: &Device, sample_rate: u32, channels: u16) -> Result<OutputStream, CallError> {
        tracing::info!(
            "Audio playback: using device '{}' at {}Hz, {} channels",
            device.name().unwrap_or_else(|_| "unknown".to_string()),
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

        let output_stream = device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    drain_mix(data, &sample_buffer_clone, &is_active_clone, &volume_clone);
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

        Ok(output_stream)
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

}

impl Drop for AudioPlayback {
    fn drop(&mut self) {
        self.stop();
    }
}
