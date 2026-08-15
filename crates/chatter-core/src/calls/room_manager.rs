//! LiveKit room connection management.

// LiveKit types don't implement Send+Sync
#![allow(clippy::arc_with_non_send_sync)]

use livekit::prelude::*;
use livekit::e2ee::{E2eeOptions, EncryptionType, key_provider::{KeyProvider, KeyProviderOptions}};
use livekit::options::TrackPublishOptions;
// Screen-share only (desktop): the publish path is cfg-gated off on mobile.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use livekit::options::{VideoCodec, VideoEncoding};
use livekit::webrtc::audio_source::RtcAudioSource;
use livekit::webrtc::video_source::RtcVideoSource;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use livekit::webrtc::video_source::{native::NativeVideoSource, VideoResolution};
use tokio::sync::mpsc;
use std::sync::Arc;
use parking_lot::RwLock;

use crate::error::CallError;
use super::audio_capture::{AudioCapture, AudioDevice};
use super::audio_playback::AudioPlayback;
use super::video_capture::{VideoCapture, VideoDevice, VideoCaptureConfig};
use super::video_playback::VideoPlayback;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use super::screen_capture::{ScreenCapture, ScreenSource, ScreenCaptureConfig};
use super::shared_frames::SharedVideoFrames;

/// Current state of the room connection.
#[derive(Clone, Debug, PartialEq)]
pub enum RoomState {
    /// Not connected to any room.
    Disconnected,
    /// Connecting to room.
    Connecting,
    /// Connected and active.
    Connected {
        room_name: String,
        participant_count: usize,
    },
    /// Connection failed.
    Failed { reason: String },
}

/// Room event for UI updates.
#[derive(Clone, Debug)]
pub enum RoomEvent {
    /// Connection state changed.
    StateChanged(RoomState),
    /// Participant joined.
    ParticipantJoined {
        identity: String,
        display_name: Option<String>,
    },
    /// Participant left.
    ParticipantLeft { identity: String },
    /// Track subscribed (remote video/audio available).
    TrackSubscribed {
        participant_identity: String,
        track_sid: String,
        is_video: bool,
        is_screen_share: bool,
    },
    /// Track unsubscribed.
    TrackUnsubscribed {
        participant_identity: String,
        track_sid: String,
        is_video: bool,
        is_screen_share: bool,
    },
    /// E2EE state changed.
    EncryptionStateChanged {
        participant_identity: String,
        is_encrypted: bool,
    },
    /// LiveKit reported a new connection-quality score for a
    /// participant. Score matches LiveKit's ConnectionQuality ordering:
    /// 0 Unknown, 1 Poor, 2 Good, 3 Excellent. We collapse Lost→Unknown.
    ConnectionQualityChanged {
        participant_identity: String,
        quality: u8,
    },
}

/// E2EE key state (key provider + epoch under one lock).
struct E2eeState {
    provider: Option<KeyProvider>,
    epoch: u32,
}

/// Manages LiveKit room connections with E2EE.
pub struct RoomManager {
    /// Current room (if connected).
    room: Arc<RwLock<Option<Room>>>,
    /// Current state.
    state: Arc<RwLock<RoomState>>,
    /// Event sender for UI updates (bounded for backpressure).
    event_tx: mpsc::Sender<RoomEvent>,
    /// Audio capture manager.
    audio_capture: Arc<RwLock<AudioCapture>>,
    /// Audio playback manager for remote participants.
    audio_playback: Arc<RwLock<Option<AudioPlayback>>>,
    /// Video capture manager.
    video_capture: Arc<RwLock<VideoCapture>>,
    /// Whether the camera track is currently published to the room. Turning
    /// video off fully stops capture and unpublishes the track (like screen
    /// share); turning it on publishes a fresh track. Guards against double
    /// publish on rapid toggles, and doubles as the "is camera on" flag. Reset
    /// on disconnect. Enables voice→video escalation (start as a voice call,
    /// then turn the camera on with the video button).
    camera_published: Arc<std::sync::atomic::AtomicBool>,
    /// SID of the currently-published camera track. Cleared on stop so we can
    /// `unpublish_track(sid)` — otherwise the track stays published with no
    /// frames and remote viewers freeze on the last frame instead of showing
    /// the avatar (mirrors `screen_share_track_sid`).
    camera_track_sid: Arc<RwLock<Option<TrackSid>>>,
    /// Video playback manager for remote participants.
    video_playback: Arc<RwLock<VideoPlayback>>,
    /// Screen capture manager.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    screen_capture: Arc<RwLock<ScreenCapture>>,
    /// SID of the currently-published screen-share track. Cleared on stop
    /// so we can call `local_participant.unpublish_track(sid)` and free
    /// the slot — otherwise restart creates a second track while the
    /// first stays published with no frames, and remote viewers stay
    /// subscribed to the dead one.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    screen_share_track_sid: Arc<RwLock<Option<TrackSid>>>,
    /// E2EE key state (provider + epoch).
    e2ee: Arc<RwLock<E2eeState>>,
    /// Handle to the spawned event handler task so disconnect() can await it.
    event_task: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
}

impl RoomManager {
    /// Create a new RoomManager.
    pub fn new() -> (Self, mpsc::Receiver<RoomEvent>) {
        Self::with_shared_frames(None)
    }

    /// Create a new RoomManager with shared video frame storage.
    pub fn with_shared_frames(shared_frames: Option<SharedVideoFrames>) -> (Self, mpsc::Receiver<RoomEvent>) {
        let (event_tx, event_rx) = mpsc::channel(256);

        // Initialize audio playback (may fail if no output device)
        let audio_playback = match AudioPlayback::new() {
            Ok(playback) => Some(playback),
            Err(e) => {
                tracing::warn!("Failed to initialize audio playback: {}. Remote audio will not be played.", e);
                None
            }
        };

        // Initialize video playback with shared frames if provided
        let video_playback = match shared_frames {
            Some(shared) => VideoPlayback::with_shared_frames(shared),
            None => VideoPlayback::new(),
        };

        let manager = Self {
            room: Arc::new(RwLock::new(None)),
            state: Arc::new(RwLock::new(RoomState::Disconnected)),
            event_tx,
            audio_capture: Arc::new(RwLock::new(AudioCapture::new())),
            audio_playback: Arc::new(RwLock::new(audio_playback)),
            video_capture: Arc::new(RwLock::new(VideoCapture::new())),
            camera_published: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            camera_track_sid: Arc::new(RwLock::new(None)),
            video_playback: Arc::new(RwLock::new(video_playback)),
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            screen_capture: Arc::new(RwLock::new(ScreenCapture::new())),
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            screen_share_track_sid: Arc::new(RwLock::new(None)),
            e2ee: Arc::new(RwLock::new(E2eeState { provider: None, epoch: 0 })),
            event_task: Arc::new(RwLock::new(None)),
        };
        (manager, event_rx)
    }

    // =========================================================================
    // Device Enumeration
    // =========================================================================

    /// List available audio input devices.
    pub fn list_audio_devices() -> Result<Vec<AudioDevice>, CallError> {
        AudioCapture::list_devices()
    }

    /// List available video input devices.
    pub fn list_video_devices() -> Result<Vec<VideoDevice>, CallError> {
        VideoCapture::list_devices()
    }

    /// List available screen sources for sharing.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn list_screen_sources() -> Result<Vec<ScreenSource>, CallError> {
        ScreenCapture::list_sources()
    }

    // =========================================================================
    // Track Publishing
    // =========================================================================

    /// Publish microphone audio to the room.
    ///
    /// # Arguments
    /// * `device_id` - Optional device ID (uses default if None)
    pub async fn publish_microphone(&self, device_id: Option<&str>) -> Result<(), CallError> {
        let local_participant = {
            let room = self.room.read();
            room.as_ref().ok_or(CallError::NotInCall)?.local_participant()
        };

        // Start audio capture
        let audio_source = {
            let mut capture = self.audio_capture.write();
            capture.start(device_id)?
        };

        // Create and publish track
        let audio_track = LocalAudioTrack::create_audio_track(
            "microphone",
            RtcAudioSource::Native(audio_source),
        );

        let publish_options = TrackPublishOptions {
            source: TrackSource::Microphone,
            ..Default::default()
        };

        local_participant
            .publish_track(LocalTrack::Audio(audio_track), publish_options)
            .await
            .map_err(|e| CallError::TrackPublishFailed(e.to_string()))?;

        tracing::info!("Published microphone track");
        Ok(())
    }

    /// Publish camera video to the room.
    ///
    /// # Arguments
    /// * `device_path` - Optional device path (uses default if None)
    /// * `config` - Optional capture configuration
    pub async fn publish_camera(
        &self,
        device_path: Option<&str>,
        config: Option<VideoCaptureConfig>,
    ) -> Result<(), CallError> {
        use std::sync::atomic::Ordering;

        // Already on — guard against double-publish from a rapid re-toggle.
        if self.camera_published.load(Ordering::SeqCst) {
            tracing::info!("Camera already published — ignoring publish");
            return Ok(());
        }

        let local_participant = {
            let room = self.room.read();
            room.as_ref().ok_or(CallError::NotInCall)?.local_participant()
        };

        // Start video capture
        let video_source = {
            let mut capture = self.video_capture.write();
            capture.start(device_path, config)?
        };

        // Create and publish track
        let video_track = LocalVideoTrack::create_video_track(
            "camera",
            RtcVideoSource::Native(video_source),
        );

        let publish_options = TrackPublishOptions {
            source: TrackSource::Camera,
            ..Default::default()
        };

        let publication = local_participant
            .publish_track(LocalTrack::Video(video_track), publish_options)
            .await
            .map_err(|e| CallError::TrackPublishFailed(e.to_string()))?;

        *self.camera_track_sid.write() = Some(publication.sid());
        self.camera_published.store(true, Ordering::SeqCst);
        tracing::info!("Published camera track");
        Ok(())
    }

    /// Publish screen share to the room.
    ///
    /// # Arguments
    /// * `source` - Screen source to capture
    /// * `config` - Optional capture configuration
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub async fn publish_screen_share(
        &self,
        source: ScreenSource,
        config: Option<ScreenCaptureConfig>,
    ) -> Result<(), CallError> {
        let local_participant = {
            let room = self.room.read();
            room.as_ref().ok_or(CallError::NotInCall)?.local_participant()
        };

        // Two-phase startup so the `NativeVideoSource` is sized to the
        // real capture dims (e.g. 5120×1440). Previously we seeded at a
        // fixed 7680×4320 — libwebrtc then upscaled every capture frame
        // to that size before encode, and receivers decoded 33 MP frames
        // (~150 ms libyuv convert per frame, starving the tokio worker
        // and overflowing the audio relay queue).
        // Capture the negotiated frame rate before `config` is moved into
        // `begin`, so the LiveKit publish cap matches what the capturer
        // actually produces (otherwise raising one without the other is a
        // no-op — see the three coupled "60 fps" sites).
        let target_fps = config
            .as_ref()
            .map(|c| c.fps)
            .unwrap_or_else(|| ScreenCaptureConfig::default().fps);

        let handshake = {
            let mut capture = self.screen_capture.write();
            capture.begin(source, config)?
        };

        // Wait for the capture thread's probe frame to come back with
        // real dims. Portal selection can take a while; the timeout is
        // generous but bounded so a cancelled portal doesn't hang the
        // call worker forever.
        let dims_result = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            handshake.dims_rx,
        )
        .await;

        let (width, height) = match dims_result {
            Ok(Ok(d)) => d,
            Ok(Err(_)) => {
                self.screen_capture.write().stop();
                return Err(CallError::TrackPublishFailed(
                    "Screen capture thread exited before sending probe dims".to_string(),
                ));
            }
            Err(_) => {
                self.screen_capture.write().stop();
                return Err(CallError::TrackPublishFailed(
                    "Timed out waiting for screen capture probe frame (portal dialog cancelled?)".to_string(),
                ));
            }
        };

        let video_source = NativeVideoSource::new(
            VideoResolution { width, height },
            true, // is_screencast
        );

        // Hand the source to the capture thread so it can start pushing
        // frames (including the buffered probe frame). If the channel is
        // dropped the thread has already given up.
        if handshake.source_tx.send(video_source.clone()).is_err() {
            self.screen_capture.write().stop();
            return Err(CallError::TrackPublishFailed(
                "Screen capture thread exited before source handoff".to_string(),
            ));
        }

        self.screen_capture.read().complete();

        let video_track = LocalVideoTrack::create_video_track(
            "screen",
            RtcVideoSource::Native(video_source),
        );

        // Screen-share track tuned for high-res content (e.g. 5120×1440
        // ultrawides). Defaults (VP8, simulcast=true, 1080p30 preset cap)
        // throttle the encoder to 3 Mbps and run THREE parallel VP8
        // encoders.
        //
        // Why H.265 specifically:
        //   - H.264 NVENC advertises Baseline Level 3.1 in webrtc-sys
        //     (`profile-level-id: 42e01f`), which maxes out at ~720p.
        //     At 5120×1440 `nvEncInitializeEncoder` returns error 8
        //     (invalid param) and the encoder is unusable.
        //   - Even H.264 Level 5.2 caps at 4096×2304 — a 5120-wide frame
        //     is physically out of spec.
        //   - HEVC/H.265 supports up to 8K (8192×4320 at level 6.0),
        //     which fits 5120×1440 cleanly, and the NVENC factory
        //     already advertises it via the same encoder path.
        //
        // simulcast=false and an explicit `video_encoding` keep us off
        // the SDK's hardcoded 1080p/30/3 Mbps screenshare preset.
        //
        // Optimise for fat links (1 Gbps+ LAN/fibre): 60 Mbps is the *ceiling*,
        // not a floor. At 5120x1440 this is ~0.13 bpp @144 fps — visually
        // lossless for desktop/text content (the 25 Mbps prior cap softened
        // small text under motion; 8 Mbps blocked badly). 60 Mbps is ~7.5 MB/s,
        // trivial on 1 Gbps (125 MB/s) yet leaves headroom for audio + the
        // peer's own upstream. WebRTC's congestion controller adapts the
        // *actual* send rate down on constrained links, so capping high costs
        // nothing on slow networks while letting fast ones run near-lossless.
        // Per-link capability negotiation (advertise downlink, pick the tier)
        // is the follow-up milestone; this is the high-net default.
        let publish_options = TrackPublishOptions {
            source: TrackSource::Screenshare,
            video_codec: VideoCodec::H265,
            simulcast: false,
            video_encoding: Some(VideoEncoding {
                max_bitrate: 60_000_000,
                max_framerate: target_fps as f64,
            }),
            ..Default::default()
        };

        let publication = local_participant
            .publish_track(LocalTrack::Video(video_track), publish_options)
            .await
            .map_err(|e| CallError::TrackPublishFailed(e.to_string()))?;

        let sid = publication.sid();
        *self.screen_share_track_sid.write() = Some(sid.clone());

        tracing::info!(
            "Published screen share track (H.265 NVENC, simulcast off, {} fps cap, 60 Mbps, {}x{}, sid={})",
            target_fps, width, height, sid
        );
        Ok(())
    }

    /// Stop microphone capture and unpublish track.
    pub fn stop_microphone(&self) {
        let mut capture = self.audio_capture.write();
        capture.stop();
        // Note: Track will be auto-unpublished when room disconnects
        // For explicit unpublish, we'd need to track the publication
    }

    /// Stop camera capture AND unpublish the LiveKit track.
    ///
    /// Without the unpublish step the SFU keeps the track alive after the local
    /// capture loop ends, so remote viewers freeze on the last frame instead of
    /// showing the avatar. Mirrors `stop_screen_share`.
    pub async fn stop_camera(&self) {
        {
            let mut capture = self.video_capture.write();
            capture.stop();
        }
        self.camera_published
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let sid = self.camera_track_sid.write().take();
        let Some(sid) = sid else { return };
        let local_participant = {
            let room = self.room.read();
            match room.as_ref() {
                Some(r) => r.local_participant(),
                None => return,
            }
        };
        match local_participant.unpublish_track(&sid).await {
            Ok(_) => tracing::info!("Unpublished camera track sid={}", sid),
            Err(e) => tracing::warn!("Failed to unpublish camera track {}: {}", sid, e),
        }
    }

    /// Stop screen share capture AND unpublish the LiveKit track.
    ///
    /// Without the unpublish step the SFU keeps the track alive after the
    /// local capture loop ends. Remote viewers stay subscribed to a
    /// frozen stream; restarting screen share creates a second track on
    /// top of the dead one and the receiver tile picks whichever
    /// subscription it had first.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub async fn stop_screen_share(&self) {
        {
            let mut capture = self.screen_capture.write();
            capture.stop();
        }
        let sid = self.screen_share_track_sid.write().take();
        let Some(sid) = sid else { return };
        let local_participant = {
            let room = self.room.read();
            match room.as_ref() {
                Some(r) => r.local_participant(),
                None => return,
            }
        };
        match local_participant.unpublish_track(&sid).await {
            Ok(_) => tracing::info!("Unpublished screen share track sid={}", sid),
            Err(e) => tracing::warn!("Failed to unpublish screen share track {}: {}", sid, e),
        }
    }

    /// Toggle microphone mute.
    pub fn set_microphone_muted(&self, muted: bool) {
        let mut capture = self.audio_capture.write();
        capture.set_muted(muted);
    }

    /// Check if microphone is muted.
    pub fn is_microphone_muted(&self) -> bool {
        self.audio_capture.read().is_muted()
    }

    /// Check if the camera is currently on (track published).
    pub fn is_camera_enabled(&self) -> bool {
        self.camera_published
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Check if screen sharing is active.
    pub fn is_screen_sharing(&self) -> bool {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        { self.screen_capture.read().is_running() }
        #[cfg(any(target_os = "android", target_os = "ios"))]
        { false }
    }

    /// Set playback volume for remote audio (0.0 to 1.0).
    pub fn set_playback_volume(&self, volume: f32) {
        if let Some(playback) = self.audio_playback.read().as_ref() {
            playback.set_volume(volume);
        }
    }

    /// Get current playback volume.
    pub fn playback_volume(&self) -> f32 {
        self.audio_playback
            .read()
            .as_ref()
            .map(|p| p.volume())
            .unwrap_or(1.0)
    }

    /// Locally mute/unmute a specific remote participant's audio. Frames
    /// keep decoding so the LiveKit stream stays healthy; muted frames are
    /// dropped before mixing.
    pub fn set_peer_audio_muted(&self, participant_id: &str, muted: bool) {
        if let Some(playback) = self.audio_playback.read().as_ref() {
            playback.set_peer_muted(participant_id, muted);
        }
    }

    /// Whether a remote participant is locally audio-muted.
    pub fn is_peer_audio_muted(&self, participant_id: &str) -> bool {
        self.audio_playback
            .read()
            .as_ref()
            .map(|p| p.is_peer_muted(participant_id))
            .unwrap_or(false)
    }

    /// Locally pause/resume rendering of a remote participant's video. The
    /// LiveKit subscription stays up; frames decode but are dropped before
    /// storage. The UI tile falls back to the avatar while paused.
    pub fn set_peer_video_paused(&self, participant_id: &str, paused: bool) {
        self.video_playback.read().set_peer_paused(participant_id, paused);
    }

    /// Whether a remote participant's video is locally paused.
    pub fn is_peer_video_paused(&self, participant_id: &str) -> bool {
        self.video_playback.read().is_peer_paused(participant_id)
    }

    /// Get the latest video frame for a participant as raw data.
    ///
    /// Returns None if no frame is available for the participant.
    pub fn get_video_frame(&self, participant_id: &str) -> Option<super::shared_frames::VideoFrameData> {
        self.video_playback.read().get_frame(participant_id)
    }

    /// Get all remote video frames as raw data.
    pub fn get_all_video_frames(&self) -> Vec<super::shared_frames::VideoFrameData> {
        self.video_playback.read().get_all_frames()
    }

    /// Check if we have a video frame available for a participant.
    pub fn has_video_frame(&self, participant_id: &str) -> bool {
        self.video_playback.read().has_frame(participant_id)
    }

    /// Get the number of active video streams.
    pub fn video_stream_count(&self) -> usize {
        self.video_playback.read().stream_count()
    }

    /// Connect to a LiveKit room with E2EE.
    ///
    /// # Arguments
    /// * `url` - LiveKit server URL
    /// * `token` - LiveKit access token (from server)
    /// * `encryption_key` - 32-byte key for E2EE (from Signal derivation)
    pub async fn connect(
        &self,
        url: &str,
        token: &str,
        encryption_key: Vec<u8>,
    ) -> Result<(), CallError> {
        self.set_state(RoomState::Connecting);

        // Create key provider with Signal-derived key.
        //
        // `ratchet_window_size: 0` disables LiveKit's OWN key ratchet. We drive
        // the key chain externally (CallKeyState, pushed via `set_shared_key`).
        // With a non-zero window, a single transient decrypt failure makes
        // LiveKit auto-ratchet its shared key using its PBKDF2 derivation —
        // which diverges from our HKDF chain and never re-syncs, permanently
        // killing media. We install the keys ourselves, so LiveKit must not.
        let key_provider = KeyProvider::with_shared_key(
            KeyProviderOptions { ratchet_window_size: 0, ..KeyProviderOptions::default() },
            encryption_key,
        );

        // Store key provider for later ratcheting
        {
            let mut e2ee = self.e2ee.write();
            e2ee.provider = Some(key_provider.clone());
            e2ee.epoch = 0;
        }

        // Configure room with E2EE
        let mut room_options = RoomOptions::default();
        room_options.auto_subscribe = true;
        room_options.encryption = Some(E2eeOptions {
            encryption_type: EncryptionType::Gcm,
            key_provider,
        });

        // Connect to room (timeout prevents indefinite hang on slow/dead servers)
        let (room, mut events) = tokio::time::timeout(
            tokio::time::Duration::from_secs(10),
            Room::connect(url, token, room_options),
        )
        .await
        .map_err(|_| CallError::ConnectionFailed("LiveKit connection timed out (10s)".to_string()))?
        .map_err(|e| CallError::ConnectionFailed(e.to_string()))?;

        // Enable E2EE
        room.e2ee_manager().set_enabled(true);

        let room_name = room.name().to_string();
        let remote_participants: Vec<_> = room.remote_participants().values().cloned().collect();
        let participant_count = remote_participants.len() + 1;

        // Emit ParticipantJoined events for participants already in the room
        // (LiveKit only sends ParticipantConnected for NEW participants after we join)
        // Also subscribe to their existing tracks since TrackSubscribed events may have
        // already fired before we set up our event handler
        for participant in &remote_participants {
            let identity = participant.identity().to_string();
            let name = participant.name();
            tracing::info!("RoomManager: Existing participant in room - identity={}, name={}", identity, name);
            let _ = self.event_tx.try_send(RoomEvent::ParticipantJoined {
                identity: identity.clone(),
                display_name: if name.is_empty() { None } else { Some(name.to_string()) },
            });

            // Process existing tracks from this participant
            for (track_sid, track_publication) in participant.track_publications() {
                if let Some(track) = track_publication.track() {
                    match track {
                        RemoteTrack::Audio(audio_track) => {
                            tracing::info!(
                                "RoomManager: Existing audio track from {} (sid: {})",
                                identity,
                                track_sid
                            );
                            if let Some(playback) = self.audio_playback.read().as_ref() {
                                playback.add_track(identity.clone(), audio_track);
                                tracing::info!("RoomManager: Added existing audio track to playback");
                            }
                            let _ = self.event_tx.try_send(RoomEvent::TrackSubscribed {
                                participant_identity: identity.clone(),
                                track_sid: track_sid.to_string(),
                                is_video: false,
                                is_screen_share: false,
                            });
                        }
                        RemoteTrack::Video(video_track) => {
                            let source = video_track.source();
                            let is_screen_share = matches!(source, TrackSource::Screenshare | TrackSource::ScreenshareAudio);
                            tracing::info!(
                                "RoomManager: Existing video track from {} (sid: {}, source: {:?})",
                                identity,
                                track_sid,
                                source
                            );
                            self.video_playback.read().add_track(
                                identity.clone(),
                                video_track,
                                is_screen_share,
                            );
                            tracing::info!("RoomManager: Added existing video track to playback");
                            let _ = self.event_tx.try_send(RoomEvent::TrackSubscribed {
                                participant_identity: identity.clone(),
                                track_sid: track_sid.to_string(),
                                is_video: true,
                                is_screen_share,
                            });
                        }
                    }
                }
            }
        }

        // Store room
        {
            let mut room_guard = self.room.write();
            *room_guard = Some(room);
        }

        self.set_state(RoomState::Connected {
            room_name,
            participant_count,
        });

        // Spawn event handler on LocalSet (LiveKit types are !Send)
        let event_tx = self.event_tx.clone();
        let state = self.state.clone();
        let room_arc = self.room.clone();
        let audio_playback = self.audio_playback.clone();
        let video_playback = self.video_playback.clone();

        // Must use spawn_local because:
        // 1. Call worker runs on a LocalSet with new_current_thread() runtime
        // 2. LiveKit Room/events are !Send and must stay on the same thread
        let event_task_slot = self.event_task.clone();
        let handle = tokio::task::spawn_local(async move {
            tracing::info!("Room event handler started");
            while let Some(event) = events.recv().await {
                tracing::info!("LiveKit room event received: {:?}", event);
                Self::handle_room_event(event, &event_tx, &state, &room_arc, &audio_playback, &video_playback);
                // Yield to allow other tasks (like forwarder) to run
                tokio::task::yield_now().await;
            }

            // Room disconnected - stop audio/video playback
            tracing::info!("Room event stream ended - marking disconnected");
            if let Some(playback) = audio_playback.read().as_ref() {
                playback.stop();
            }
            video_playback.read().stop();
            *state.write() = RoomState::Disconnected;
            let _ = event_tx.try_send(RoomEvent::StateChanged(RoomState::Disconnected));
        });
        *event_task_slot.write() = Some(handle);

        Ok(())
    }

    /// Disconnect from the current room.
    ///
    /// This method is safe to call from the UI thread. Capture threads are
    /// signaled to stop (non-blocking) and will clean themselves up.
    /// Thread joins happen in Drop to ensure proper cleanup.
    pub async fn disconnect(&self) {
        // Update state immediately so UI can respond
        self.set_state(RoomState::Disconnected);

        // Stop audio playback
        if let Some(playback) = self.audio_playback.read().as_ref() {
            playback.stop();
            tracing::info!("Audio playback stopped");
        }

        // Stop video playback
        self.video_playback.read().stop();
        tracing::info!("Video playback stopped");

        // Signal all capture threads to stop (non-blocking, just sets atomic flags)
        // The threads will exit on their own, joins happen in Drop
        {
            let audio = self.audio_capture.read();
            audio.signal_stop();
        }
        {
            let video = self.video_capture.read();
            video.signal_stop();
            // Track auto-unpublishes when the room closes; clear the flag + sid
            // so the next call re-publishes instead of inheriting a dead handle.
            self.camera_published
                .store(false, std::sync::atomic::Ordering::SeqCst);
            self.camera_track_sid.write().take();
        }
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            let screen = self.screen_capture.read();
            screen.signal_stop();
            // Track auto-unpublishes when the room closes; clear the cached
            // sid so the next call doesn't inherit a stale handle.
            self.screen_share_track_sid.write().take();
        }

        // Take the room handle
        let room = {
            let mut room_guard = self.room.write();
            room_guard.take()
        };

        // Close the LiveKit room with timeout
        if let Some(room) = room {
            match tokio::time::timeout(
                tokio::time::Duration::from_secs(3),
                room.close()
            ).await {
                Ok(_) => tracing::info!("Room closed successfully"),
                Err(_) => tracing::warn!("Room close timed out, forcing disconnect"),
            }
        }

        // Wait for event handler task to finish draining
        let event_handle = self.event_task.write().take();
        if let Some(handle) = event_handle {
            match tokio::time::timeout(
                tokio::time::Duration::from_secs(2),
                handle,
            ).await {
                Ok(Ok(())) => tracing::debug!("Event handler task joined cleanly"),
                Ok(Err(e)) => tracing::warn!("Event handler task panicked: {}", e),
                Err(_) => tracing::warn!("Event handler join timed out (2s), proceeding"),
            }
        }

        tracing::info!("Disconnect completed (capture cleanup deferred)");
    }

    /// Get current state.
    pub fn state(&self) -> RoomState {
        self.state.read().clone()
    }

    /// Check if connected.
    pub fn is_connected(&self) -> bool {
        matches!(self.state(), RoomState::Connected { .. })
    }

    /// Get current participant count.
    pub fn participant_count(&self) -> usize {
        if let RoomState::Connected { participant_count, .. } = self.state() {
            participant_count
        } else {
            0
        }
    }

    /// Ratchet the encryption key for forward secrecy.
    ///
    /// This advances the key to a new epoch, providing forward secrecy
    /// so that compromise of a later key doesn't reveal earlier messages.
    /// Should be called periodically during long calls.
    ///
    /// Returns the new key epoch, or None if not connected.
    pub fn ratchet_key(&self) -> Option<u32> {
        let mut e2ee = self.e2ee.write();
        if let Some(ref key_provider) = e2ee.provider {
            if let Some(_new_key) = key_provider.ratchet_shared_key(0) {
                e2ee.epoch += 1;
                tracing::info!(epoch = e2ee.epoch, "E2EE key ratcheted");
                Some(e2ee.epoch)
            } else {
                tracing::warn!("Key ratchet failed");
                None
            }
        } else {
            tracing::warn!("Cannot ratchet key: not connected");
            None
        }
    }

    /// Get the current key epoch.
    pub fn key_epoch(&self) -> u32 {
        self.e2ee.read().epoch
    }

    /// Set a new encryption key (for external key management).
    ///
    /// This replaces the current key with a new one derived externally.
    /// In shared key mode, this sets the key at the next epoch index.
    ///
    /// Note: For normal operation, prefer `ratchet_key()` which uses
    /// LiveKit's internal key derivation. Use this only when you need
    /// to sync keys from an external source (e.g., CallKeyState).
    pub fn set_encryption_key(&self, key: Vec<u8>) -> bool {
        let mut e2ee = self.e2ee.write();
        let provider = match e2ee.provider.clone() {
            Some(p) => p,
            None => {
                tracing::warn!("Cannot set key: no key provider");
                return false;
            }
        };

        // The provider is in SHARED-key mode, so the frame cryptor reads the
        // shared key ring — `set_shared_key`, NOT the per-participant
        // `set_key` (which only feeds the non-shared path and never reached
        // the cryptor, the root cause of media dying on the first ratchet).
        e2ee.epoch += 1;
        let new_epoch = e2ee.epoch;
        provider.set_shared_key(key, new_epoch as i32);
        tracing::info!(epoch = new_epoch, "E2EE key updated externally");
        true
    }

    /// Install an externally-ratcheted shared key for `epoch`, pre-installing
    /// `epoch + 1` so a peer whose 30 s ratchet timer fires slightly ahead can
    /// still decrypt immediately.
    ///
    /// Both sides advance the same deterministic [`CallKeyState`] chain at the
    /// same 30 s cadence, so they reach the same epoch (= key index). The two
    /// directions of small timing skew are absorbed without LiveKit's own
    /// ratchet (which we disabled): a peer slightly *ahead* is covered by the
    /// pre-installed `epoch + 1`; a peer slightly *behind* by the 16-slot key
    /// ring still holding older epochs. `epoch` is installed LAST so it becomes
    /// the `latest_key_index` we encrypt with.
    pub fn set_ratcheted_shared_key(&self, epoch: u32, key_cur: Vec<u8>, key_next: Vec<u8>) -> bool {
        let mut e2ee = self.e2ee.write();
        let provider = match e2ee.provider.clone() {
            Some(p) => p,
            None => {
                tracing::warn!("Cannot ratchet key: no key provider");
                return false;
            }
        };
        provider.set_shared_key(key_next, (epoch + 1) as i32);
        provider.set_shared_key(key_cur, epoch as i32);
        e2ee.epoch = epoch;
        true
    }

    /// Install `key` as the shared key at index 0 and reset the epoch counter.
    ///
    /// Used on the DM caller side when the real ECDH-derived key arrives after
    /// a placeholder connect: the callee connected *with* this same key at
    /// index 0, so re-installing it at index 0 (and resetting the epoch) puts
    /// both peers on the same key and the same epoch baseline, so the periodic
    /// ratchet then advances them in lockstep.
    pub fn reset_shared_key(&self, key: Vec<u8>) -> bool {
        let mut e2ee = self.e2ee.write();
        let provider = match e2ee.provider.clone() {
            Some(p) => p,
            None => {
                tracing::warn!("Cannot reset key: no key provider");
                return false;
            }
        };
        provider.set_shared_key(key, 0);
        e2ee.epoch = 0;
        tracing::info!("E2EE key re-rooted at index 0 (real DM key)");
        true
    }

    fn set_state(&self, state: RoomState) {
        *self.state.write() = state.clone();
        let _ = self.event_tx.try_send(RoomEvent::StateChanged(state));
    }

    fn handle_room_event(
        event: livekit::RoomEvent,
        event_tx: &mpsc::Sender<RoomEvent>,
        state: &Arc<RwLock<RoomState>>,
        room: &Arc<RwLock<Option<Room>>>,
        audio_playback: &Arc<RwLock<Option<AudioPlayback>>>,
        video_playback: &Arc<RwLock<VideoPlayback>>,
    ) {
        match event {
            livekit::RoomEvent::ParticipantConnected(participant) => {
                let identity = participant.identity().to_string();
                let name = participant.name();
                tracing::info!("RoomManager: ParticipantConnected - identity={}, name={}", identity, name);
                let event = RoomEvent::ParticipantJoined {
                    identity: identity.clone(),
                    display_name: if name.is_empty() { None } else { Some(name.to_string()) },
                };
                tracing::info!("RoomManager: Sending event to channel: {:?}", event);
                match event_tx.try_send(event) {
                    Ok(()) => tracing::info!("RoomManager: Event sent successfully"),
                    Err(e) => tracing::error!("RoomManager: Failed to send ParticipantJoined event: {:?}", e),
                }

                // Update participant count (avoid holding read lock while getting write lock)
                let new_state = {
                    let room_guard = room.read();
                    if let Some(r) = room_guard.as_ref() {
                        let count = r.remote_participants().len() + 1;
                        let state_guard = state.read();
                        if let RoomState::Connected { room_name, .. } = &*state_guard {
                            Some(RoomState::Connected {
                                room_name: room_name.clone(),
                                participant_count: count,
                            })
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }; // All read locks dropped here

                if let Some(new_state) = new_state {
                    *state.write() = new_state;
                }
            }
            livekit::RoomEvent::ParticipantDisconnected(participant) => {
                let participant_id = participant.identity().to_string();

                // Remove audio playback for this participant
                if let Some(playback) = audio_playback.read().as_ref() {
                    playback.remove_track(&participant_id);
                }

                // Remove video playback for this participant — both their camera
                // and screen-share tracks (the peer is gone entirely).
                {
                    let vp = video_playback.read();
                    vp.remove_track(&participant_id, false);
                    vp.remove_track(&participant_id, true);
                }

                let _ = event_tx.try_send(RoomEvent::ParticipantLeft {
                    identity: participant_id,
                });
            }
            livekit::RoomEvent::TrackSubscribed { track, participant, .. } => {
                let participant_id = participant.identity().to_string();
                let is_video = matches!(track, RemoteTrack::Video(_));

                // Determine if this is a screen share track
                let is_screen_share = match &track {
                    RemoteTrack::Video(video_track) => {
                        matches!(video_track.source(), TrackSource::Screenshare | TrackSource::ScreenshareAudio)
                    }
                    _ => false,
                };

                // Handle audio/video track playback
                match &track {
                    RemoteTrack::Audio(audio_track) => {
                        tracing::info!(
                            "RoomManager: Audio track subscribed from {} (sid: {})",
                            participant_id,
                            track.sid()
                        );
                        if let Some(playback) = audio_playback.read().as_ref() {
                            playback.add_track(participant_id.clone(), audio_track.clone());
                            tracing::info!("RoomManager: Added audio track to playback");
                        } else {
                            tracing::warn!("RoomManager: No audio playback available, cannot play remote audio");
                        }
                    }
                    RemoteTrack::Video(video_track) => {
                        let source = video_track.source();
                        tracing::info!(
                            "RoomManager: Video track subscribed from {} (sid: {}, source: {:?}, screen_share: {})",
                            participant_id,
                            track.sid(),
                            source,
                            is_screen_share
                        );
                        video_playback.read().add_track(
                            participant_id.clone(),
                            video_track.clone(),
                            is_screen_share,
                        );
                        tracing::info!("RoomManager: Added video track to playback");
                    }
                }

                let _ = event_tx.try_send(RoomEvent::TrackSubscribed {
                    participant_identity: participant_id,
                    track_sid: track.sid().to_string(),
                    is_video,
                    is_screen_share,
                });
            }
            livekit::RoomEvent::TrackUnsubscribed { track, participant, .. } => {
                let participant_id = participant.identity().to_string();
                let is_video = matches!(track, RemoteTrack::Video(_));
                let mut is_screen_share = false;

                // Remove audio playback if it's an audio track
                if matches!(track, RemoteTrack::Audio(_)) {
                    if let Some(playback) = audio_playback.read().as_ref() {
                        playback.remove_track(&participant_id);
                        tracing::info!("RoomManager: Removed audio track from playback for {}", participant_id);
                    }
                }

                // Remove video playback if it's a video track
                if let RemoteTrack::Video(ref v) = track {
                    is_screen_share = matches!(
                        v.source(),
                        TrackSource::Screenshare | TrackSource::ScreenshareAudio
                    );
                    video_playback.read().remove_track(&participant_id, is_screen_share);
                    tracing::info!(
                        "RoomManager: Removed video track from playback for {} (screen_share={})",
                        participant_id, is_screen_share
                    );
                }

                let _ = event_tx.try_send(RoomEvent::TrackUnsubscribed {
                    participant_identity: participant_id,
                    track_sid: track.sid().to_string(),
                    is_video,
                    is_screen_share,
                });
            }
            livekit::RoomEvent::ConnectionQualityChanged { participant, quality } => {
                use livekit::participant::ConnectionQuality as CQ;
                let score: u8 = match quality {
                    CQ::Excellent => 4,
                    CQ::Good      => 3,
                    CQ::Poor      => 1,
                    _             => 0,
                };
                let _ = event_tx.try_send(RoomEvent::ConnectionQualityChanged {
                    participant_identity: participant.identity().to_string(),
                    quality: score,
                });
            }
            livekit::RoomEvent::E2eeStateChanged { participant, state: enc_state } => {
                use livekit::webrtc::native::frame_cryptor::EncryptionState;
                let is_encrypted = matches!(enc_state, EncryptionState::Ok | EncryptionState::KeyRatcheted);
                let _ = event_tx.try_send(RoomEvent::EncryptionStateChanged {
                    participant_identity: participant.identity().to_string(),
                    is_encrypted,
                });
            }
            _ => {}
        }
    }
}
