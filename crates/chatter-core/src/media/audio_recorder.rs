//! Audio recorder.
//!
//! Voice message recording using cpal for audio input and hound for WAV encoding.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, Stream, StreamConfig};
use hound::{SampleFormat as HoundSampleFormat, WavSpec, WavWriter};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::MediaError;

/// Recording state.
#[derive(Debug, Clone)]
pub enum RecordingState {
    /// Not recording.
    Idle,
    /// Currently recording.
    Recording {
        /// When recording started.
        started_at: Instant,
    },
    /// Recording stopped, ready to save.
    Stopped {
        /// Recorded samples.
        sample_count: usize,
        /// Duration of the recording.
        duration: Duration,
    },
}

/// Audio recorder for voice messages.
///
/// Uses cpal for microphone input and hound for WAV encoding.
pub struct AudioRecorder {
    /// Input device.
    device: Device,
    /// Stream config (sample rate, channels).
    config: StreamConfig,
    /// Active recording stream (must be kept alive during recording).
    stream: Option<Stream>,
    /// Current recording state.
    state: RecordingState,
    /// Sample buffer shared with recording callback.
    samples: Arc<Mutex<Vec<f32>>>,
    /// Sample rate of the recording.
    sample_rate: u32,
    /// Number of channels.
    channels: u16,
    /// When recording started (for duration tracking).
    start_time: Option<Instant>,
}

impl AudioRecorder {
    /// Creates a new audio recorder.
    ///
    /// # Errors
    ///
    /// Returns an error if no input device is available.
    pub fn new() -> Result<Self, MediaError> {
        if crate::system::null_audio() {
            return Err(MediaError::RecordingError(
                "audio devices disabled (CHATTER_NULL_AUDIO)".to_string(),
            ));
        }
        let host = cpal::default_host();

        let device = host.default_input_device().ok_or_else(|| {
            MediaError::RecordingError("no audio input device available".to_string())
        })?;

        let supported_config = device.default_input_config().map_err(|e| {
            MediaError::RecordingError(format!("failed to get input config: {}", e))
        })?;

        let config: StreamConfig = supported_config.config();
        let sample_rate = config.sample_rate;
        let channels = config.channels;

        Ok(Self {
            device,
            config,
            stream: None,
            state: RecordingState::Idle,
            samples: Arc::new(Mutex::new(Vec::new())),
            sample_rate,
            channels,
            start_time: None,
        })
    }

    /// Starts recording.
    ///
    /// # Errors
    ///
    /// Returns an error if recording is already in progress or stream creation fails.
    pub fn start(&mut self) -> Result<(), MediaError> {
        if matches!(self.state, RecordingState::Recording { .. }) {
            return Err(MediaError::RecordingError(
                "recording already in progress".to_string(),
            ));
        }

        // Clear any previous samples
        {
            let mut samples = self.samples.lock().unwrap();
            samples.clear();
        }

        let samples_clone = Arc::clone(&self.samples);

        // Get supported sample format
        let supported_config = self.device.default_input_config().map_err(|e| {
            MediaError::RecordingError(format!("failed to get input config: {}", e))
        })?;

        let err_fn = |err| {
            tracing::error!("Recording stream error: {}", err);
        };

        let stream = match supported_config.sample_format() {
            SampleFormat::F32 => self.build_input_stream::<f32>(samples_clone, err_fn)?,
            SampleFormat::I16 => self.build_input_stream::<i16>(samples_clone, err_fn)?,
            SampleFormat::I32 => self.build_input_stream::<i32>(samples_clone, err_fn)?,
            SampleFormat::U8 => self.build_input_stream::<u8>(samples_clone, err_fn)?,
            SampleFormat::U16 => self.build_input_stream::<u16>(samples_clone, err_fn)?,
            SampleFormat::U32 => self.build_input_stream::<u32>(samples_clone, err_fn)?,
            SampleFormat::I8 => self.build_input_stream::<i8>(samples_clone, err_fn)?,
            SampleFormat::U64 => self.build_input_stream::<u64>(samples_clone, err_fn)?,
            SampleFormat::I64 => self.build_input_stream::<i64>(samples_clone, err_fn)?,
            SampleFormat::F64 => self.build_input_stream::<f64>(samples_clone, err_fn)?,
            _ => {
                return Err(MediaError::RecordingError(
                    "unsupported sample format".to_string(),
                ));
            }
        };

        stream.play().map_err(|e| {
            MediaError::RecordingError(format!("failed to start recording stream: {}", e))
        })?;

        let now = Instant::now();
        self.stream = Some(stream);
        self.start_time = Some(now);
        self.state = RecordingState::Recording { started_at: now };

        Ok(())
    }

    /// Builds an input stream for a specific sample type.
    fn build_input_stream<T>(
        &self,
        samples: Arc<Mutex<Vec<f32>>>,
        err_fn: impl Fn(cpal::Error) + Send + 'static,
    ) -> Result<Stream, MediaError>
    where
        T: cpal::SizedSample,
        f32: cpal::FromSample<T>,
    {
        // Bound memory even if the recording worker is temporarily delayed.
        let max_samples = self.sample_rate as usize
            * self.channels as usize
            * crate::services::voice_recording::MAX_RECORDING_SECONDS as usize;
        self.device
            .build_input_stream(
                self.config,
                move |data: &[T], _: &cpal::InputCallbackInfo| {
                    let mut samples = samples.lock().unwrap();
                    let remaining = max_samples.saturating_sub(samples.len());
                    for &sample in data.iter().take(remaining) {
                        // Convert to f32 for storage using cpal's FromSample trait
                        let f32_sample: f32 = cpal::Sample::from_sample(sample);
                        samples.push(f32_sample);
                    }
                },
                err_fn,
                None,
            )
            .map_err(|e| MediaError::RecordingError(format!("failed to build input stream: {}", e)))
    }

    /// Stops recording and returns the duration.
    ///
    /// # Errors
    ///
    /// Returns an error if not currently recording.
    pub fn stop(&mut self) -> Result<Duration, MediaError> {
        if !matches!(self.state, RecordingState::Recording { .. }) {
            return Err(MediaError::RecordingError("not recording".to_string()));
        }

        // Stop and drop the stream
        self.stream = None;

        let duration = self
            .start_time
            .map(|t| t.elapsed())
            .unwrap_or(Duration::ZERO);
        self.start_time = None;

        let sample_count = self.samples.lock().unwrap().len();

        self.state = RecordingState::Stopped {
            sample_count,
            duration,
        };

        Ok(duration)
    }

    /// Saves the recording to a WAV file.
    ///
    /// # Arguments
    ///
    /// * `path` - Output file path (should end in .wav).
    ///
    /// # Errors
    ///
    /// Returns an error if there's no recording to save or file writing fails.
    pub fn save(&self, path: &Path) -> Result<(), MediaError> {
        match &self.state {
            RecordingState::Stopped { .. } => {}
            RecordingState::Idle => {
                return Err(MediaError::RecordingError(
                    "no recording to save".to_string(),
                ));
            }
            RecordingState::Recording { .. } => {
                return Err(MediaError::RecordingError(
                    "stop recording before saving".to_string(),
                ));
            }
        }

        let samples = self.samples.lock().unwrap();
        if samples.is_empty() {
            return Err(MediaError::RecordingError("no audio recorded".to_string()));
        }

        let spec = WavSpec {
            channels: self.channels,
            sample_rate: self.sample_rate,
            bits_per_sample: 16,
            sample_format: HoundSampleFormat::Int,
        };

        let mut writer = WavWriter::create(path, spec)
            .map_err(|e| MediaError::RecordingError(format!("failed to create WAV file: {}", e)))?;

        // Convert f32 samples to i16 for WAV encoding
        for &sample in samples.iter() {
            let clamped = sample.clamp(-1.0, 1.0);
            let i16_sample = (clamped * i16::MAX as f32) as i16;
            writer.write_sample(i16_sample).map_err(|e| {
                MediaError::RecordingError(format!("failed to write sample: {}", e))
            })?;
        }

        writer
            .finalize()
            .map_err(|e| MediaError::RecordingError(format!("failed to finalize WAV: {}", e)))?;

        Ok(())
    }

    /// Cancels the current recording and discards samples.
    pub fn cancel(&mut self) {
        self.stream = None;
        self.start_time = None;
        self.samples.lock().unwrap().clear();
        self.state = RecordingState::Idle;
    }

    /// Gets the current recording state.
    pub fn state(&self) -> &RecordingState {
        &self.state
    }

    /// Gets the elapsed recording duration.
    ///
    /// Returns `None` if not currently recording.
    pub fn duration(&self) -> Option<Duration> {
        match &self.state {
            RecordingState::Recording { started_at } => Some(started_at.elapsed()),
            RecordingState::Stopped { duration, .. } => Some(*duration),
            RecordingState::Idle => None,
        }
    }

    /// Gets the sample rate of the recording.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Gets the number of channels.
    pub fn channels(&self) -> u16 {
        self.channels
    }
}
