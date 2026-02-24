//! Audio capture from microphone using cpal.
//!
//! Provides device enumeration and audio frame capture for LiveKit publishing.

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::audio_source::AudioSourceOptions;

use crate::error::CallError;

/// Information about an available audio input device.
#[derive(Debug, Clone)]
pub struct AudioDevice {
    /// Device name for display.
    pub name: String,
    /// Internal device identifier.
    pub id: String,
    /// Whether this is the default device.
    pub is_default: bool,
}

/// Information about an available audio output device.
#[derive(Debug, Clone)]
pub struct AudioOutputDevice {
    /// Device name for display.
    pub name: String,
    /// Internal device identifier.
    pub id: String,
    /// Whether this is the default device.
    pub is_default: bool,
}

/// Audio capture manager for microphone input.
pub struct AudioCapture {
    /// The cpal audio stream (must be kept alive).
    stream: Option<cpal::Stream>,
    /// Flag to control capture.
    running: Arc<AtomicBool>,
    /// Current device name.
    device_name: String,
    /// LiveKit audio source for publishing.
    audio_source: Option<NativeAudioSource>,
    /// Sample rate being used.
    sample_rate: u32,
    /// Number of channels.
    num_channels: u32,
}

impl AudioCapture {
    /// Create a new audio capture (not yet started).
    pub fn new() -> Self {
        Self {
            stream: None,
            running: Arc::new(AtomicBool::new(false)),
            device_name: String::new(),
            audio_source: None,
            sample_rate: 48000,
            num_channels: 1,
        }
    }

    /// List available audio input devices (microphones).
    pub fn list_devices() -> Result<Vec<AudioDevice>, CallError> {
        let host = cpal::default_host();
        let default_device = host.default_input_device();
        let default_name = default_device.as_ref().and_then(|d| d.name().ok());

        let mut devices = Vec::new();

        for device in host.input_devices().map_err(|e| {
            CallError::TrackPublishFailed(format!("Failed to enumerate audio devices: {}", e))
        })? {
            if let Ok(name) = device.name() {
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
    pub fn list_output_devices() -> Result<Vec<AudioOutputDevice>, CallError> {
        let host = cpal::default_host();
        let default_device = host.default_output_device();
        let default_name = default_device.as_ref().and_then(|d| d.name().ok());

        let mut devices = Vec::new();

        for device in host.output_devices().map_err(|e| {
            CallError::TrackPublishFailed(format!("Failed to enumerate output devices: {}", e))
        })? {
            if let Ok(name) = device.name() {
                let is_default = default_name.as_ref() == Some(&name);
                devices.push(AudioOutputDevice {
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
        let host = cpal::default_host();
        let device = host.default_input_device().ok_or_else(|| {
            CallError::TrackPublishFailed("No default audio input device".to_string())
        })?;

        let name = device.name().map_err(|e| {
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
        if self.running.load(Ordering::SeqCst) {
            return Err(CallError::TrackPublishFailed(
                "Audio capture already running".to_string(),
            ));
        }

        let host = cpal::default_host();

        // Find the requested device or use default
        let device = if let Some(id) = device_id {
            host.input_devices()
                .map_err(|e| {
                    CallError::TrackPublishFailed(format!("Failed to enumerate devices: {}", e))
                })?
                .find(|d| d.name().ok().as_deref() == Some(id))
                .ok_or_else(|| {
                    CallError::TrackPublishFailed(format!("Audio device not found: {}", id))
                })?
        } else {
            host.default_input_device().ok_or_else(|| {
                CallError::TrackPublishFailed("No default audio input device".to_string())
            })?
        };

        self.device_name = device.name().unwrap_or_default();
        tracing::info!("Starting audio capture from device: {}", self.device_name);

        // Get supported config
        let supported_config = device
            .default_input_config()
            .map_err(|e| CallError::TrackPublishFailed(format!("No supported audio config: {}", e)))?;

        self.sample_rate = supported_config.sample_rate().0;
        self.num_channels = supported_config.channels() as u32;

        tracing::info!(
            "Audio config: {}Hz, {} channels, format: {:?}",
            self.sample_rate,
            self.num_channels,
            supported_config.sample_format()
        );

        // Create LiveKit audio source
        let audio_source = NativeAudioSource::new(
            AudioSourceOptions {
                echo_cancellation: true,
                noise_suppression: true,
                auto_gain_control: true,
            },
            self.sample_rate,
            self.num_channels,
            50, // 50ms buffer
        );

        // Clone for the capture callback
        let source_clone = audio_source.clone();
        let running = self.running.clone();
        let sample_rate = self.sample_rate;
        let num_channels = self.num_channels;

        // Get a handle to the Tokio runtime for use in the callback
        // The cpal callback runs on a separate thread without Tokio context
        let rt_handle = tokio::runtime::Handle::current();

        // Build the stream based on sample format
        let stream_config: StreamConfig = supported_config.clone().into();
        let err_fn = |err| tracing::error!("Audio capture error: {}", err);

        let stream = match supported_config.sample_format() {
            SampleFormat::F32 => {
                let rt_handle = rt_handle.clone();
                device.build_input_stream(
                    &stream_config,
                    move |data: &[f32], _: &cpal::InputCallbackInfo| {
                        if running.load(Ordering::SeqCst) {
                            // Convert f32 to i16
                            let samples: Vec<i16> = data
                                .iter()
                                .map(|&s| (s * 32767.0).clamp(-32768.0, 32767.0) as i16)
                                .collect();

                            let frame = AudioFrame {
                                data: Cow::Owned(samples.clone()),
                                sample_rate,
                                num_channels,
                                samples_per_channel: samples.len() as u32 / num_channels,
                            };

                            // Non-blocking capture using runtime handle
                            let source = source_clone.clone();
                            rt_handle.spawn(async move {
                                let _ = source.capture_frame(&frame).await;
                            });
                        }
                    },
                    err_fn,
                    None,
                )
            }
            SampleFormat::I16 => {
                let rt_handle = rt_handle.clone();
                device.build_input_stream(
                    &stream_config,
                    move |data: &[i16], _: &cpal::InputCallbackInfo| {
                        if running.load(Ordering::SeqCst) {
                            // Copy data to owned Vec for async task
                            let samples: Vec<i16> = data.to_vec();
                            let frame = AudioFrame {
                                data: Cow::Owned(samples.clone()),
                                sample_rate,
                                num_channels,
                                samples_per_channel: samples.len() as u32 / num_channels,
                            };

                            let source = source_clone.clone();
                            rt_handle.spawn(async move {
                                let _ = source.capture_frame(&frame).await;
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
        .map_err(|e| CallError::TrackPublishFailed(format!("Failed to build audio stream: {}", e)))?;

        // Start the stream
        stream
            .play()
            .map_err(|e| CallError::TrackPublishFailed(format!("Failed to start audio stream: {}", e)))?;

        self.running.store(true, Ordering::SeqCst);
        self.stream = Some(stream);
        self.audio_source = Some(audio_source.clone());

        tracing::info!("Audio capture started successfully");
        Ok(audio_source)
    }

    /// Stop audio capture.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(stream) = self.stream.take() {
            drop(stream);
        }
        self.audio_source = None;
        tracing::info!("Audio capture stopped");
    }

    /// Check if capture is running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Get current device name.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Toggle mute (stops/starts the capture stream).
    pub fn set_muted(&mut self, muted: bool) {
        if muted {
            self.running.store(false, Ordering::SeqCst);
        } else {
            self.running.store(true, Ordering::SeqCst);
        }
    }

    /// Check if muted.
    pub fn is_muted(&self) -> bool {
        !self.running.load(Ordering::SeqCst)
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
    fn test_audio_output_device_creation() {
        let device = AudioOutputDevice {
            name: "Test Speakers".to_string(),
            id: "test-speakers-456".to_string(),
            is_default: false,
        };

        assert_eq!(device.name, "Test Speakers");
        assert_eq!(device.id, "test-speakers-456");
        assert!(!device.is_default);
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
                eprintln!("list_output_devices failed (expected in headless env): {}", e);
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
    fn test_audio_output_device_clone() {
        let device = AudioOutputDevice {
            name: "Test Output".to_string(),
            id: "output-id".to_string(),
            is_default: false,
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
