//! Audio capture from microphone using cpal.
//!
//! Provides device enumeration and audio frame capture for LiveKit publishing.

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::AudioSourceOptions;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use tokio::sync::mpsc;

use crate::error::CallError;

/// Information about an available audio device (input or output).
#[derive(Debug, Clone)]
pub struct AudioDevice {
    /// Device name for display.
    pub name: String,
    /// Internal device identifier.
    pub id: String,
    /// Whether this is the default device.
    pub is_default: bool,
}

/// Audio capture manager for microphone input.
/// Audio frame payload sent through the capture channel.
struct CapturedFrame {
    samples: Vec<i16>,
    sample_rate: u32,
    num_channels: u32,
}

pub struct AudioCapture {
    /// The cpal audio stream (must be kept alive).
    stream: Option<cpal::Stream>,
    /// Flag to control capture.
    running: Arc<AtomicBool>,
    /// Current device name.
    device_name: String,
    /// Requested selection: None follows the system default, independently of its name.
    selected_device: Option<String>,
    /// LiveKit audio source for publishing.
    audio_source: Option<NativeAudioSource>,
    /// Sample rate being used.
    sample_rate: u32,
    /// Number of channels.
    num_channels: u32,
    /// Sender for the capture worker (dropped on stop to shut it down).
    frame_tx: Option<mpsc::Sender<CapturedFrame>>,
    stream_failed: Arc<AtomicBool>,
    recovery_muted: Option<bool>,
}

impl AudioCapture {
    /// Create a new audio capture (not yet started).
    pub fn new() -> Self {
        Self {
            stream: None,
            running: Arc::new(AtomicBool::new(false)),
            device_name: String::new(),
            selected_device: None,
            audio_source: None,
            sample_rate: 48000,
            num_channels: 1,
            frame_tx: None,
            stream_failed: Arc::new(AtomicBool::new(false)),
            recovery_muted: None,
        }
    }

    /// List available audio input devices (microphones).
    pub fn list_devices() -> Result<Vec<AudioDevice>, CallError> {
        if crate::system::null_audio() {
            return Ok(Vec::new());
        }
        let host = cpal::default_host();
        let default_device = host.default_input_device();
        let default_name = default_device.as_ref().and_then(|d| {
            d.description()
                .map(|description| description.name().to_owned())
                .ok()
        });

        let mut devices = Vec::new();

        for device in host.input_devices().map_err(|e| {
            CallError::TrackPublishFailed(format!("Failed to enumerate audio devices: {}", e))
        })? {
            if let Ok(name) = device
                .description()
                .map(|description| description.name().to_owned())
            {
                let is_default = default_name.as_ref() == Some(&name);
                devices.push(AudioDevice {
                    id: name.clone(),
                    name: name.clone(),
                    is_default,
                });
            }
        }

        Ok(devices)
    }

    /// List available audio output devices (speakers/headphones).
    pub fn list_output_devices() -> Result<Vec<AudioDevice>, CallError> {
        if crate::system::null_audio() {
            return Ok(Vec::new());
        }
        let host = cpal::default_host();
        let default_device = host.default_output_device();
        let default_name = default_device.as_ref().and_then(|d| {
            d.description()
                .map(|description| description.name().to_owned())
                .ok()
        });

        let mut devices = Vec::new();

        for device in host.output_devices().map_err(|e| {
            CallError::TrackPublishFailed(format!("Failed to enumerate output devices: {}", e))
        })? {
            if let Ok(name) = device
                .description()
                .map(|description| description.name().to_owned())
            {
                let is_default = default_name.as_ref() == Some(&name);
                devices.push(AudioDevice {
                    id: name.clone(),
                    name: name.clone(),
                    is_default,
                });
            }
        }

        Ok(devices)
    }

    /// Get the default audio input device.
    pub fn default_device() -> Result<AudioDevice, CallError> {
        if crate::system::null_audio() {
            return Err(CallError::TrackPublishFailed(
                "audio devices disabled (CHATTER_NULL_AUDIO)".to_string(),
            ));
        }
        let host = cpal::default_host();
        let device = host.default_input_device().ok_or_else(|| {
            CallError::TrackPublishFailed("No default audio input device".to_string())
        })?;

        let name = device
            .description()
            .map(|description| description.name().to_owned())
            .map_err(|e| {
                CallError::TrackPublishFailed(format!("Failed to get device name: {}", e))
            })?;

        Ok(AudioDevice {
            id: name.clone(),
            name,
            is_default: true,
        })
    }

    /// Start capturing audio from the specified device.
    ///
    /// Returns the NativeAudioSource that can be used to create a LiveKit track.
    pub fn start(&mut self, device_id: Option<&str>) -> Result<NativeAudioSource, CallError> {
        self.start_with_source(device_id, None, false)
    }

    fn start_with_source(
        &mut self,
        device_id: Option<&str>,
        existing_source: Option<NativeAudioSource>,
        muted: bool,
    ) -> Result<NativeAudioSource, CallError> {
        if self.running.load(Ordering::SeqCst) {
            return Err(CallError::TrackPublishFailed(
                "Audio capture already running".to_string(),
            ));
        }

        // A disconnected room may have signalled stop without dropping CPAL.
        // Release that device before opening another Android recording stream.
        self.stop();
        self.selected_device = device_id.map(str::to_owned);

        if crate::system::null_audio() {
            // A source LiveKit can publish, never fed by a microphone.
            self.sample_rate = 48_000;
            self.num_channels = 1;
            self.device_name = "null".to_string();
            let source = existing_source.unwrap_or_else(|| {
                NativeAudioSource::new(
                    AudioSourceOptions {
                        echo_cancellation: false,
                        noise_suppression: false,
                        auto_gain_control: false,
                    },
                    self.sample_rate,
                    self.num_channels,
                    50,
                )
            });
            self.audio_source = Some(source.clone());
            self.running.store(!muted, Ordering::SeqCst);
            return Ok(source);
        }

        let host = cpal::default_host();

        // Find the requested device or use default
        let device = if let Some(id) = device_id {
            host.input_devices()
                .map_err(|e| {
                    CallError::TrackPublishFailed(format!("Failed to enumerate devices: {}", e))
                })?
                .find(|d| {
                    d.description()
                        .map(|description| description.name().to_owned())
                        .ok()
                        .as_deref()
                        == Some(id)
                })
                .ok_or_else(|| {
                    CallError::TrackPublishFailed(format!("Audio device not found: {}", id))
                })?
        } else {
            host.default_input_device().ok_or_else(|| {
                CallError::TrackPublishFailed("No default audio input device".to_string())
            })?
        };

        self.device_name = device
            .description()
            .map(|description| description.name().to_owned())
            .unwrap_or_default();
        tracing::info!("Starting audio capture from device: {}", self.device_name);

        // Get supported config
        let supported_config = device.default_input_config().map_err(|e| {
            CallError::TrackPublishFailed(format!("No supported audio config: {}", e))
        })?;

        self.sample_rate = supported_config.sample_rate();
        self.num_channels = supported_config.channels() as u32;

        tracing::info!(
            "Audio config: {}Hz, {} channels, format: {:?}",
            self.sample_rate,
            self.num_channels,
            supported_config.sample_format()
        );

        // Create LiveKit audio source
        let audio_source = existing_source.unwrap_or_else(|| {
            NativeAudioSource::new(
                AudioSourceOptions {
                    echo_cancellation: true,
                    noise_suppression: true,
                    auto_gain_control: true,
                },
                self.sample_rate,
                self.num_channels,
                50, // 50ms buffer
            )
        });

        // Set up bounded channel + single consumer task instead of spawning per frame
        let (frame_tx, mut frame_rx) = mpsc::channel::<CapturedFrame>(64);
        let source_for_worker = audio_source.clone();
        tokio::spawn(async move {
            let mut first_frame = true;
            let mut first_non_silent = true;
            let mut first_error = true;
            while let Some(captured) = frame_rx.recv().await {
                let frame = AudioFrame {
                    data: Cow::Owned(captured.samples),
                    sample_rate: captured.sample_rate,
                    num_channels: captured.num_channels,
                    samples_per_channel: 0, // recalculated below
                };
                let frame = AudioFrame {
                    samples_per_channel: frame.data.len() as u32 / frame.num_channels,
                    ..frame
                };
                let non_silent = first_non_silent && frame.data.iter().any(|&s| s != 0);
                match source_for_worker.capture_frame(&frame).await {
                    Ok(()) => {
                        if first_frame {
                            tracing::info!("First microphone frame delivered to LiveKit");
                            first_frame = false;
                        }
                        if non_silent {
                            tracing::info!(
                                "First non-silent microphone frame delivered to LiveKit"
                            );
                            first_non_silent = false;
                        }
                    }
                    Err(error) if first_error => {
                        tracing::warn!(%error, "Microphone frame delivery failed");
                        first_error = false;
                    }
                    Err(_) => {}
                }
            }
        });

        let running = self.running.clone();
        let sample_rate = self.sample_rate;
        let num_channels = self.num_channels;

        // Build the stream based on sample format
        let stream_config: StreamConfig = supported_config.clone().into();
        let stream_failed = self.stream_failed.clone();
        let err_fn = move |err: cpal::Error| {
            tracing::error!("Audio capture error: {}", err);
            if matches!(
                err.kind(),
                cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::StreamInvalidated
            ) {
                stream_failed.store(true, Ordering::SeqCst);
            }
        };

        let stream = match supported_config.sample_format() {
            SampleFormat::F32 => {
                let tx = frame_tx.clone();
                device.build_input_stream(
                    stream_config,
                    move |data: &[f32], _: &cpal::InputCallbackInfo| {
                        if running.load(Ordering::SeqCst) {
                            let samples: Vec<i16> = data
                                .iter()
                                .map(|&s| (s * 32767.0).clamp(-32768.0, 32767.0) as i16)
                                .collect();
                            let _ = tx.try_send(CapturedFrame {
                                samples,
                                sample_rate,
                                num_channels,
                            });
                        }
                    },
                    err_fn,
                    None,
                )
            }
            SampleFormat::I16 => {
                let tx = frame_tx.clone();
                device.build_input_stream(
                    stream_config,
                    move |data: &[i16], _: &cpal::InputCallbackInfo| {
                        if running.load(Ordering::SeqCst) {
                            let samples = data.to_vec();
                            let _ = tx.try_send(CapturedFrame {
                                samples,
                                sample_rate,
                                num_channels,
                            });
                        }
                    },
                    err_fn,
                    None,
                )
            }
            format => {
                return Err(CallError::TrackPublishFailed(format!(
                    "Unsupported audio format: {:?}",
                    format
                )));
            }
        }
        .map_err(|e| {
            CallError::TrackPublishFailed(format!("Failed to build audio stream: {}", e))
        })?;

        // Start the stream
        stream.play().map_err(|e| {
            CallError::TrackPublishFailed(format!("Failed to start audio stream: {}", e))
        })?;

        self.running.store(!muted, Ordering::SeqCst);
        self.stream = Some(stream);
        self.audio_source = Some(audio_source.clone());
        self.frame_tx = Some(frame_tx);

        tracing::info!("Audio capture started successfully");
        Ok(audio_source)
    }

    /// Android can invalidate an input stream when the communication route
    /// changes. Reopen the device without replacing the source or publication.
    pub fn recover_stream(&mut self) -> Result<(), CallError> {
        if !self.stream_failed.swap(false, Ordering::SeqCst) || self.audio_source.is_none() {
            return Ok(());
        }
        let muted = self.recovery_muted.unwrap_or_else(|| self.is_muted());
        let source = self.audio_source.clone();
        let device = self.selected_device.clone();
        self.stop();
        match self.start_with_source(device.as_deref(), source.clone(), muted) {
            Ok(_) => {
                self.recovery_muted = None;
                self.set_muted(muted);
                Ok(())
            }
            Err(error) => {
                self.audio_source = source;
                self.recovery_muted = Some(muted);
                self.stream_failed.store(true, Ordering::SeqCst);
                Err(error)
            }
        }
    }

    /// Signal audio capture to stop (non-blocking).
    ///
    /// This only sets the running flag. Use `stop()` for full cleanup.
    pub fn signal_stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// Stop audio capture.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        // Drop frame_tx to shut down the consumer task
        self.frame_tx = None;
        if let Some(stream) = self.stream.take() {
            drop(stream);
        }
        self.audio_source = None;
        self.recovery_muted = None;
        self.stream_failed.store(false, Ordering::SeqCst);
        tracing::info!("Audio capture stopped");
    }

    /// Check if capture is running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Requested device for recovery and reconnect; None follows the system default.
    pub fn selected_device(&self) -> Option<&str> {
        self.selected_device.as_deref()
    }

    /// Get current device name.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Toggle mute (stops/starts the capture stream).
    pub fn set_muted(&mut self, muted: bool) {
        if self.recovery_muted.is_some() {
            self.recovery_muted = Some(muted);
            return;
        }
        self.running.store(!muted, Ordering::SeqCst);
    }

    /// Check if muted.
    pub fn is_muted(&self) -> bool {
        self.recovery_muted
            .unwrap_or_else(|| !self.running.load(Ordering::SeqCst))
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Default for AudioCapture {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn recovery_preserves_requested_selection_independently_of_display_name() {
        assert!(crate::system::null_audio(), "run with CHATTER_NULL_AUDIO=1");
        for selection in [None, Some("chosen microphone"), Some("Default Device")] {
            let mut capture = AudioCapture::new();
            capture.start(selection).unwrap();
            // The backend's display name is not a device selection.
            capture.device_name = "backend display label".into();
            capture.set_muted(true);
            for _ in 0..2 {
                capture.stream_failed.store(true, Ordering::SeqCst);
                capture.recover_stream().unwrap();
                assert_eq!(capture.selected_device(), selection);
                assert!(capture.audio_source.is_some());
                assert!(capture.is_muted());
            }
            capture.stop();
            assert_eq!(capture.selected_device(), selection);
        }
    }

    #[test]
    fn test_audio_device_creation() {
        let device = AudioDevice {
            name: "Test Microphone".to_string(),
            id: "test-mic-123".to_string(),
            is_default: true,
        };

        assert_eq!(device.name, "Test Microphone");
        assert_eq!(device.id, "test-mic-123");
        assert!(device.is_default);
    }

    #[test]
    fn test_audio_capture_new() {
        let capture = AudioCapture::new();
        assert!(!capture.is_running());
        assert!(capture.device_name().is_empty());
    }

    #[test]
    fn test_audio_capture_default() {
        let capture = AudioCapture::default();
        assert!(!capture.is_running());
    }

    #[test]
    fn test_audio_capture_mute_toggle() {
        let mut capture = AudioCapture::new();

        // Initially not running, so is_muted returns true
        assert!(capture.is_muted());

        // Set muted should work even when not running
        capture.set_muted(true);
        assert!(capture.is_muted());

        capture.set_muted(false);
        assert!(!capture.is_muted());
    }

    #[test]
    fn test_list_devices_returns_result() {
        // This test requires audio hardware, so we just check it doesn't panic
        let result = AudioCapture::list_devices();

        match result {
            Ok(devices) => {
                // May be empty if no audio devices
                tracing::info!("Found {} input devices", devices.len());

                // If we have devices, check they have names
                for device in &devices {
                    assert!(!device.name.is_empty(), "Device name should not be empty");
                    assert!(!device.id.is_empty(), "Device id should not be empty");
                }

                // Check that at most one device is marked as default
                let default_count = devices.iter().filter(|d| d.is_default).count();
                assert!(default_count <= 1, "Should have at most one default device");
            }
            Err(e) => {
                // May fail in CI/headless environments
                eprintln!("list_devices failed (expected in headless env): {}", e);
            }
        }
    }

    #[test]
    fn test_list_output_devices_returns_result() {
        // This test requires audio hardware, so we just check it doesn't panic
        let result = AudioCapture::list_output_devices();

        match result {
            Ok(devices) => {
                tracing::info!("Found {} output devices", devices.len());

                // If we have devices, check they have names
                for device in &devices {
                    assert!(!device.name.is_empty(), "Device name should not be empty");
                    assert!(!device.id.is_empty(), "Device id should not be empty");
                }

                // Check that at most one device is marked as default
                let default_count = devices.iter().filter(|d| d.is_default).count();
                assert!(default_count <= 1, "Should have at most one default device");
            }
            Err(e) => {
                // May fail in CI/headless environments
                eprintln!(
                    "list_output_devices failed (expected in headless env): {}",
                    e
                );
            }
        }
    }

    #[test]
    fn test_audio_device_clone() {
        let device = AudioDevice {
            name: "Test".to_string(),
            id: "id".to_string(),
            is_default: true,
        };

        let cloned = device.clone();
        assert_eq!(device.name, cloned.name);
        assert_eq!(device.id, cloned.id);
        assert_eq!(device.is_default, cloned.is_default);
    }

    #[test]
    fn test_audio_device_debug() {
        let device = AudioDevice {
            name: "Mic".to_string(),
            id: "mic-1".to_string(),
            is_default: true,
        };

        let debug_str = format!("{:?}", device);
        assert!(debug_str.contains("Mic"));
        assert!(debug_str.contains("mic-1"));
        assert!(debug_str.contains("true"));
    }
}
