//! Audio playback for remote participant streams.
//!
//! Receives audio frames from remote participants via LiveKit and plays
//! them through the local audio output device using CPAL.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

#[cfg(not(target_os = "android"))]
use cpal::Device;
#[cfg(not(target_os = "android"))]
use cpal::StreamConfig;
#[cfg(not(target_os = "android"))]
use cpal::traits::StreamTrait;
#[cfg(not(target_os = "android"))]
use cpal::traits::{DeviceTrait, HostTrait};
use futures::StreamExt;
use livekit::track::RemoteAudioTrack;
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use parking_lot::{Mutex, RwLock};
use tokio::sync::watch;

use super::timing::CallTiming;
use crate::error::CallError;

/// Manages audio playback for remote participants.
///
/// Uses lazy initialization - the CPAL output stream is only created when
/// the first audio track is added, avoiding unnecessary PipeWire/ALSA streams
/// when no calls are active.
pub struct AudioPlayback {
    /// Active audio streams from remote participants.
    streams: Arc<RwLock<TrackMixer>>,
    /// CPAL output stream handle (lazily initialized).
    output_stream: Arc<RwLock<Option<OutputStream>>>,
    /// Sample rate for output (populated on init).
    sample_rate: Arc<RwLock<u32>>,
    /// Number of output channels (populated on init).
    channels: Arc<RwLock<u16>>,
    /// Whether playback is active.
    is_active: Arc<AtomicBool>,
    /// Volume (0.0 to 1.0).
    volume: Arc<AtomicU32>,
    /// 0 = no nonzero callback output, 1 = observed, 2 = reported off-thread.
    rendered_evidence: Arc<AtomicU8>,
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

/// A bounded queue in the output format, separate for each remote track.
struct SampleQueue {
    samples: VecDeque<f32>,
    capacity: usize,
    channels: usize,
    muted: bool,
}

impl SampleQueue {
    fn new(rate: usize, channels: usize, muted: bool) -> Self {
        let capacity = (rate / 2).max(1) * channels;
        Self {
            samples: VecDeque::with_capacity(capacity),
            capacity,
            channels,
            muted,
        }
    }

    fn push_pcm(&mut self, pcm: &[i16]) {
        if self.muted {
            return;
        }
        // NativeAudioStream converts to our output rate/channel count. Retain
        // whole interleaved frames so overflow never shifts channel alignment.
        for frame in pcm.chunks_exact(self.channels) {
            while self.samples.len() + self.channels > self.capacity {
                for _ in 0..self.channels {
                    self.samples.pop_front();
                }
            }
            self.samples
                .extend(frame.iter().map(|s| *s as f32 / 32768.0));
        }
    }

    fn set_muted(&mut self, muted: bool) {
        if self.muted != muted {
            self.muted = muted;
            self.samples.clear(); // Never play pre-mute speech when unmuting.
        }
    }
}

struct ActiveAudioStream {
    participant: String,
    queue: Arc<Mutex<SampleQueue>>,
    // Dropping the sender cancels even an idle receiver waiting for a frame.
    _stop: watch::Sender<()>,
}

#[derive(Default)]
struct TrackMixer {
    tracks: HashMap<String, ActiveAudioStream>,
    muted_peers: HashSet<String>,
}

impl TrackMixer {
    fn replace(
        &mut self,
        participant: String,
        sid: String,
        rate: usize,
        channels: usize,
    ) -> (Arc<Mutex<SampleQueue>>, watch::Receiver<()>) {
        let queue = Arc::new(Mutex::new(SampleQueue::new(
            rate,
            channels,
            self.muted_peers.contains(&participant),
        )));
        let (stop, receiver) = watch::channel(());
        // The old sender drops, its task stops, and its queued speech is no
        // longer in the mix. A late old frame can only reach its detached queue.
        self.tracks.insert(
            sid,
            ActiveAudioStream {
                participant,
                queue: queue.clone(),
                _stop: stop,
            },
        );
        (queue, receiver)
    }

    fn remove(&mut self, participant: &str, sid: Option<&str>) {
        self.tracks.retain(|track_sid, track| {
            !(track.participant == participant && sid.is_none_or(|sid| track_sid == sid))
        });
    }

    fn set_muted(&mut self, participant: &str, muted: bool) {
        if muted {
            self.muted_peers.insert(participant.to_owned());
        } else {
            self.muted_peers.remove(participant);
        }
        for track in self
            .tracks
            .values()
            .filter(|track| track.participant == participant)
        {
            track.queue.lock().set_muted(muted);
        }
    }
}

/// Sum the same output positions from every track. Starved tracks contribute
/// silence; they cannot consume another speaker's time. The realtime callback
/// never allocates, logs or waits for a lock. Contended input contributes silence
/// for this callback and retains its bounded queue for the next one.
fn drain_mix(
    data: &mut [f32],
    mixer: &RwLock<TrackMixer>,
    is_active: &AtomicBool,
    volume: &AtomicU32,
) {
    data.fill(0.0);
    if !is_active.load(Ordering::Relaxed) {
        return;
    }
    let Some(mixer) = mixer.try_read() else {
        return;
    };
    for track in mixer.tracks.values() {
        let Some(mut queue) = track.queue.try_lock() else {
            continue;
        };
        if queue.muted {
            continue;
        }
        for sample in data.iter_mut() {
            let Some(next) = queue.samples.pop_front() else {
                break;
            };
            *sample += next;
        }
    }
    let volume = f32::from_bits(volume.load(Ordering::Relaxed));
    for sample in data {
        *sample = (*sample * volume).clamp(-1.0, 1.0);
    }
}

// The output callback has already applied mute, mixing and volume. This marks
// consumption by the device API, not authenticated decryption or audibility.
fn record_rendered_samples(data: &[f32], evidence: &AtomicU8) {
    if evidence.load(Ordering::Relaxed) == 0
        && data
            .iter()
            .any(|sample| sample.is_finite() && *sample != 0.0)
    {
        let _ = evidence.compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed);
    }
}

/// oboe data callback for the Android call stream: mono f32 from the mix.
#[cfg(target_os = "android")]
struct VoiceOutput {
    mixer: Arc<RwLock<TrackMixer>>,
    is_active: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
    rendered_evidence: Arc<AtomicU8>,
}

#[cfg(target_os = "android")]
impl oboe::AudioOutputCallback for VoiceOutput {
    type FrameType = (f32, oboe::Mono);

    fn on_audio_ready(
        &mut self,
        _stream: &mut dyn oboe::AudioOutputStreamSafe,
        data: &mut [f32],
    ) -> oboe::DataCallbackResult {
        drain_mix(data, &self.mixer, &self.is_active, &self.volume);
        record_rendered_samples(data, &self.rendered_evidence);
        oboe::DataCallbackResult::Continue
    }

    fn on_error_after_close(
        &mut self,
        _stream: &mut dyn oboe::AudioOutputStreamSafe,
        error: oboe::Error,
    ) {
        tracing::error!("Audio output stream closed: {:?}", error);
    }
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
            streams: Arc::new(RwLock::new(TrackMixer::default())),
            output_stream: Arc::new(RwLock::new(None)),
            sample_rate: Arc::new(RwLock::new(48000)), // Default, updated on init
            channels: Arc::new(RwLock::new(2)),        // Default, updated on init
            is_active: Arc::new(AtomicBool::new(true)),
            volume: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            rendered_evidence: Arc::new(AtomicU8::new(0)),
            output_device: RwLock::new(None),
        })
    }

    /// The output device called `name`, or the host default for `None`.
    #[cfg(not(target_os = "android"))]
    fn find_output_device(name: Option<&str>) -> Result<Device, CallError> {
        if crate::system::null_audio() {
            return Err(CallError::DeviceError(
                "audio devices disabled (CHATTER_NULL_AUDIO)".to_string(),
            ));
        }
        let host = cpal::default_host();
        match name {
            None => host
                .default_output_device()
                .ok_or_else(|| CallError::DeviceError("No audio output device found".to_string())),
            Some(name) => host
                .output_devices()
                .map_err(|e| {
                    CallError::DeviceError(format!("Failed to list output devices: {}", e))
                })?
                .find(|d| {
                    d.description()
                        .map(|description| description.name().to_owned())
                        .is_ok_and(|n| n == name)
                })
                .ok_or_else(|| {
                    CallError::DeviceError(format!("Output device '{}' not found", name))
                }),
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
            let stream =
                self.build_stream(&device, *self.sample_rate.read(), *self.channels.read())?;
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
                mixer: self.streams.clone(),
                is_active: self.is_active.clone(),
                volume: self.volume.clone(),
                rendered_evidence: self.rendered_evidence.clone(),
            })
            .open_stream()
            .map_err(|e| {
                CallError::DeviceError(format!("Failed to open voice output stream: {:?}", e))
            })?;
        *self.sample_rate.write() = stream.get_sample_rate() as u32;
        *self.channels.write() = 1;
        stream.start().map_err(|e| {
            CallError::DeviceError(format!("Failed to start audio playback: {:?}", e))
        })?;
        tracing::info!(
            "Audio playback: voice-communication stream at {}Hz mono",
            stream.get_sample_rate()
        );
        *self.output_stream.write() = Some(stream);
        Ok(())
    }

    /// Initialize the CPAL output stream (called lazily on first track).
    #[cfg(not(target_os = "android"))]
    fn ensure_stream_initialized(&self) -> Result<(), CallError> {
        // Tracks are mixed but never rendered to a speaker.
        if crate::system::null_audio() {
            return Ok(());
        }
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

        let sample_rate = supported_config.sample_rate();
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
    fn build_stream(
        &self,
        device: &Device,
        sample_rate: u32,
        channels: u16,
    ) -> Result<OutputStream, CallError> {
        tracing::info!(
            "Audio playback: using device '{}' at {}Hz, {} channels",
            device
                .description()
                .map(|description| description.name().to_owned())
                .unwrap_or_else(|_| "unknown".to_string()),
            sample_rate,
            channels
        );

        let mixer = self.streams.clone();
        let is_active_clone = self.is_active.clone();
        let volume_clone = self.volume.clone();
        let rendered_evidence = self.rendered_evidence.clone();

        // Build output stream
        let config = StreamConfig {
            channels,
            sample_rate,
            buffer_size: cpal::BufferSize::Default,
        };

        let output_stream = device
            .build_output_stream(
                config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    drain_mix(data, &mixer, &is_active_clone, &volume_clone);
                    record_rendered_samples(data, &rendered_evidence);
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
        self.add_track_timed(participant_id, track, None);
    }

    pub(crate) fn add_track_timed(
        &self,
        participant_id: String,
        track: RemoteAudioTrack,
        timing: Option<CallTiming>,
    ) {
        if let Some(timing) = &timing {
            timing.mark("remote_audio_track_subscribed");
        }
        tracing::info!("Adding audio track from participant: {}", participant_id);

        // Re-activate playback (may have been stopped by a previous call)
        self.is_active.store(true, Ordering::Relaxed);

        // Lazily initialize the CPAL stream on first track
        if let Err(e) = self.ensure_stream_initialized() {
            tracing::error!(
                "Failed to initialize audio output: {}. Remote audio will not be played.",
                e
            );
            return;
        }

        let target_sample_rate = *self.sample_rate.read() as i32;
        let target_channels = *self.channels.read() as usize;
        let (queue, mut stopped) = self.streams.write().replace(
            participant_id,
            track.sid().to_string(),
            target_sample_rate as usize,
            target_channels,
        );
        // libwebrtc 0.3.41's typed RTP stats use serde(default) on plain
        // u64 counters: absent received/concealed fields become zero. Their
        // availability cannot be established from this API, so do not report
        // fabricated zero deltas. A presence-preserving SDK API is required.
        tracing::debug!(
            sample_counters = "unavailable",
            "Remote audio evidence availability"
        );
        let rtc_track = track.rtc_track();
        let mut audio_stream =
            NativeAudioStream::new(rtc_track, target_sample_rate, target_channels as i32);
        let rendered_evidence = self.rendered_evidence.clone();

        tokio::spawn(async move {
            let mut first_nonzero = true;
            let mut first_frame = true;
            let mut last_output_probe = std::time::Instant::now();
            let mut evidence_tick = tokio::time::interval(std::time::Duration::from_millis(100));
            evidence_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = stopped.changed() => break,
                    _ = evidence_tick.tick() => {
                        if rendered_evidence.compare_exchange(1, 2, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
                            tracing::info!(stage = "remote_pcm_rendered", "Nonzero mixed PCM consumed by audio output callback (not decryption or audibility proof)");
                        }
                        // Sample again off the real-time callback so a later
                        // reconnect can establish fresh output evidence. This
                        // shared flag describes the mixed output, never a peer.
                        if last_output_probe.elapsed() >= std::time::Duration::from_secs(5)
                            && rendered_evidence.compare_exchange(2, 0, Ordering::Relaxed, Ordering::Relaxed).is_ok()
                        {
                            last_output_probe = std::time::Instant::now();
                        }
                    },
                    frame = audio_stream.next() => {
                        let Some(frame) = frame else { break; };
                        if first_frame && !frame.data.is_empty() {
                            if let Some(timing) = &timing { timing.first_decoded_frame(); }
                            first_frame = false;
                        }
                        let mut queue = queue.lock();
                        if queue.muted { continue; }
                        let nonzero = first_nonzero && frame.data.iter().any(|&s| s != 0);
                        queue.push_pcm(&frame.data);
                        drop(queue);
                        if nonzero {
                            // Post-decoder PCM can include concealment. Neither
                            // queued PCM nor RTP counters prove cryptor success
                            // or audible output. Do not attach peer identifiers.
                            tracing::info!("First nonzero remote PCM queued (not decryption or audible-output proof)");
                            first_nonzero = false;
                        }
                    }
                }
            }
        });
    }

    /// Remove all audio from a departed participant.
    pub fn remove_track(&self, participant_id: &str) {
        self.streams.write().remove(participant_id, None);
    }

    /// An unsubscribe for an old track must not remove its replacement.
    pub fn remove_track_if_sid(&self, participant_id: &str, sid: &str) {
        self.streams.write().remove(participant_id, Some(sid));
    }

    /// Set playback volume (0.0 to 1.0).
    pub fn set_volume(&self, vol: f32) {
        self.volume.store(
            if vol.is_finite() {
                vol.clamp(0.0, 1.0)
            } else {
                0.0
            }
            .to_bits(),
            Ordering::Relaxed,
        );
    }

    /// Locally mute/unmute a remote participant's audio. Frames keep
    /// decoding (so the connection stays healthy and unmute is instant);
    /// muted frames are simply discarded before mixing.
    pub fn set_peer_muted(&self, participant_id: &str, muted: bool) {
        self.streams.write().set_muted(participant_id, muted);
    }

    pub fn is_peer_muted(&self, participant_id: &str) -> bool {
        self.streams.read().muted_peers.contains(participant_id)
    }

    /// Get current volume.
    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }

    /// Stop all playback.
    pub fn stop(&self) {
        self.is_active.store(false, Ordering::Relaxed);

        // Drop cancellation senders and detach all queued speech before
        // shutting down output. Do not hold the registry lock while CPAL stops.
        self.streams.write().tracks.clear();
        *self.output_stream.write() = None;
        self.rendered_evidence.store(0, Ordering::Relaxed);

        tracing::info!("Audio playback stopped");
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
    fn output_evidence_requires_nonzero_finite_rendered_samples_and_reports_once() {
        let evidence = AtomicU8::new(0);
        record_rendered_samples(&[0.0, -0.0, f32::NAN], &evidence);
        assert_eq!(evidence.load(Ordering::Relaxed), 0);
        record_rendered_samples(&[0.25], &evidence);
        assert_eq!(evidence.load(Ordering::Relaxed), 1);
        assert!(
            evidence
                .compare_exchange(1, 2, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        );
        record_rendered_samples(&[0.5], &evidence);
        assert_eq!(evidence.load(Ordering::Relaxed), 2);
        assert!(
            evidence
                .compare_exchange(1, 2, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
        );
    }

    fn render(mixer: &RwLock<TrackMixer>, length: usize, volume: f32) -> Vec<f32> {
        let mut output = vec![9.0; length];
        drain_mix(
            &mut output,
            mixer,
            &AtomicBool::new(true),
            &AtomicU32::new(volume.to_bits()),
        );
        output
    }

    #[test]
    fn simultaneous_speakers_share_output_time_and_clip_after_volume() {
        let mixer = RwLock::new(TrackMixer::default());
        let (alice, _alice_stop) = mixer.write().replace("alice".into(), "a".into(), 48000, 1);
        let (bob, _bob_stop) = mixer.write().replace("bob".into(), "b".into(), 48000, 1);
        alice.lock().push_pcm(&[16384, -16384, 24576, -24576]);
        bob.lock().push_pcm(&[8192, 8192, 24576, -24576]);
        assert_eq!(render(&mixer, 4, 1.0), [0.75, -0.25, 1.0, -1.0]);
        assert_eq!(render(&mixer, 4, 1.0), [0.0; 4]);
        alice.lock().push_pcm(&[24576]);
        bob.lock().push_pcm(&[24576]);
        assert_eq!(render(&mixer, 1, 0.5), [0.75]);
    }

    #[test]
    fn starved_speaker_and_inactive_output_are_silent() {
        let mixer = RwLock::new(TrackMixer::default());
        let (alice, _) = mixer.write().replace("alice".into(), "a".into(), 48000, 1);
        let (_bob, _) = mixer.write().replace("bob".into(), "b".into(), 48000, 1);
        alice.lock().push_pcm(&[8192, 16384]);
        assert_eq!(render(&mixer, 4, 1.0), [0.25, 0.5, 0.0, 0.0]);
        alice.lock().push_pcm(&[8192]);
        let mut output = [1.0; 2];
        drain_mix(
            &mut output,
            &mixer,
            &AtomicBool::new(false),
            &AtomicU32::new(1.0f32.to_bits()),
        );
        assert_eq!(output, [0.0; 2]);
    }

    #[test]
    fn mute_discards_backlog_and_input_but_other_speaker_keeps_playing() {
        let mixer = RwLock::new(TrackMixer::default());
        let (alice, _) = mixer.write().replace("alice".into(), "a".into(), 48000, 1);
        let (bob, _) = mixer.write().replace("bob".into(), "b".into(), 48000, 1);
        alice.lock().push_pcm(&[16384; 4]);
        mixer.write().set_muted("alice", true);
        alice.lock().push_pcm(&[16384; 4]);
        bob.lock().push_pcm(&[8192; 4]);
        assert_eq!(render(&mixer, 4, 1.0), [0.25; 4]);
        mixer.write().set_muted("alice", false);
        assert_eq!(render(&mixer, 4, 1.0), [0.0; 4]);
        alice.lock().push_pcm(&[16384]);
        assert_eq!(render(&mixer, 1, 1.0), [0.5]);
    }

    #[test]
    fn removal_replacement_and_late_unsubscribe_cannot_replay_old_speech() {
        let mixer = RwLock::new(TrackMixer::default());
        let (old, old_stop) = mixer
            .write()
            .replace("alice".into(), "new".into(), 48000, 1);
        old.lock().push_pcm(&[16384; 4]);
        let (new, _new_stop) = mixer
            .write()
            .replace("alice".into(), "new".into(), 48000, 1);
        assert!(
            old_stop.has_changed().is_err(),
            "replacement drops cancellation sender"
        );
        old.lock().push_pcm(&[16384; 4]); // Frame racing cancellation is detached.
        mixer.write().remove("alice", Some("old"));
        new.lock().push_pcm(&[8192; 4]);
        assert_eq!(render(&mixer, 4, 1.0), [0.25; 4]);
        new.lock().push_pcm(&[8192; 4]);
        mixer.write().remove("alice", Some("new"));
        assert_eq!(render(&mixer, 4, 1.0), [0.0; 4]);
        // Clear/stop detaches every participant, including frames arriving late.
        let (late, _) = mixer.write().replace("bob".into(), "b".into(), 48000, 1);
        mixer.write().tracks.clear();
        late.lock().push_pcm(&[16384]);
        assert_eq!(render(&mixer, 1, 1.0), [0.0]);
    }

    #[test]
    fn replacement_retains_participant_mute_and_queues_are_independently_bounded() {
        let mut mixer = TrackMixer::default();
        mixer.set_muted("alice", true);
        let (alice, _) = mixer.replace("alice".into(), "a".into(), 8, 2);
        alice.lock().push_pcm(&[16384; 20]);
        assert!(alice.lock().samples.is_empty());
        let (bob, _) = mixer.replace("bob".into(), "b".into(), 8, 2);
        bob.lock()
            .push_pcm(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        assert_eq!(
            bob.lock().samples.iter().copied().collect::<Vec<_>>(),
            [5, 6, 7, 8, 9, 10, 11, 12].map(|s| s as f32 / 32768.0)
        );
        assert_eq!(
            render(&RwLock::new(mixer), 8, 1.0),
            [5, 6, 7, 8, 9, 10, 11, 12].map(|s| s as f32 / 32768.0)
        );
    }

    #[test]
    fn one_participants_multiple_tracks_mix_and_remove_independently() {
        let mixer = RwLock::new(TrackMixer::default());
        let (mic, _) = mixer
            .write()
            .replace("alice".into(), "mic".into(), 48000, 2);
        let (screen, _) = mixer
            .write()
            .replace("alice".into(), "screen".into(), 48000, 2);
        mic.lock().push_pcm(&[16384, -16384]);
        screen.lock().push_pcm(&[8192, 8192]);
        assert_eq!(render(&mixer, 2, 1.0), [0.75, -0.25]);
        mixer.write().set_muted("alice", true);
        mic.lock().push_pcm(&[16384, -16384]);
        screen.lock().push_pcm(&[8192, 8192]);
        assert_eq!(render(&mixer, 2, 1.0), [0.0; 2]);
        mixer.write().set_muted("alice", false);
        mixer.write().remove("alice", Some("mic"));
        mic.lock().push_pcm(&[16384, -16384]);
        screen.lock().push_pcm(&[8192, 8192]);
        assert_eq!(render(&mixer, 2, 1.0), [0.25; 2]);
        mixer.write().remove("alice", None);
        assert!(mixer.read().tracks.is_empty());
    }

    #[test]
    fn repeated_unmuted_update_does_not_discard_live_audio() {
        let mixer = RwLock::new(TrackMixer::default());
        let (queue, _) = mixer.write().replace("alice".into(), "a".into(), 48000, 1);
        queue.lock().push_pcm(&[8192]);
        mixer.write().set_muted("alice", false);
        assert_eq!(render(&mixer, 1, 1.0), [0.25]);
    }

    #[test]
    fn volume_rejects_nonfinite_values_and_stays_in_range() {
        let playback = AudioPlayback::new().unwrap();
        for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1.0] {
            playback.set_volume(invalid);
            assert_eq!(playback.volume(), 0.0);
        }
        playback.set_volume(2.0);
        assert_eq!(playback.volume(), 1.0);
        playback.set_volume(0.25);
        assert_eq!(playback.volume(), 0.25);
    }

    #[test]
    fn callback_does_not_wait_for_registry_or_producer_locks() {
        let mixer = RwLock::new(TrackMixer::default());
        let (alice, _) = mixer.write().replace("alice".into(), "a".into(), 48000, 1);
        alice.lock().push_pcm(&[16384]);
        let held = mixer.write();
        assert_eq!(render(&mixer, 1, 1.0), [0.0]);
        drop(held);
        let held = alice.lock();
        assert_eq!(render(&mixer, 1, 1.0), [0.0]);
        drop(held);
        assert_eq!(render(&mixer, 1, 1.0), [0.5]);
    }
}
