//! LiveKit room connection management.

// LiveKit types don't implement Send+Sync, and MutexGuard across await is intentional
// since we need to hold state during room operations
#![allow(clippy::arc_with_non_send_sync)]
#![allow(clippy::await_holding_lock)]

use livekit::prelude::*;
use livekit::e2ee::{E2eeOptions, EncryptionType, key_provider::{KeyProvider, KeyProviderOptions}};
use livekit::options::TrackPublishOptions;
use livekit::webrtc::audio_source::RtcAudioSource;
use livekit::webrtc::video_source::RtcVideoSource;
use tokio::sync::mpsc;
use std::sync::Arc;
use parking_lot::RwLock;

use crate::error::CallError;
use super::audio_capture::{AudioCapture, AudioDevice};
use super::audio_playback::AudioPlayback;
use super::video_capture::{VideoCapture, VideoDevice, VideoCaptureConfig};
use super::video_playback::VideoPlayback;
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
    },
    /// E2EE state changed.
    EncryptionStateChanged {
        participant_identity: String,
        is_encrypted: bool,
    },
}

/// Manages LiveKit room connections with E2EE.
pub struct RoomManager {
    /// Current room (if connected).
    room: Arc<RwLock<Option<Room>>>,
    /// Current state.
    state: Arc<RwLock<RoomState>>,
    /// Event sender for UI updates.
    event_tx: mpsc::UnboundedSender<RoomEvent>,
    /// Audio capture manager.
    audio_capture: Arc<RwLock<AudioCapture>>,
    /// Audio playback manager for remote participants.
    audio_playback: Arc<RwLock<Option<AudioPlayback>>>,
    /// Video capture manager.
    video_capture: Arc<RwLock<VideoCapture>>,
    /// Video playback manager for remote participants.
    video_playback: Arc<RwLock<VideoPlayback>>,
    /// Screen capture manager.
    screen_capture: Arc<RwLock<ScreenCapture>>,
    /// Key provider for E2EE key updates (stored for ratcheting).
    key_provider: Arc<RwLock<Option<KeyProvider>>>,
    /// Current encryption key epoch (for coordinated ratcheting).
    key_epoch: Arc<RwLock<u32>>,
}

impl RoomManager {
    /// Create a new RoomManager.
    pub fn new() -> (Self, mpsc::UnboundedReceiver<RoomEvent>) {
        Self::with_shared_frames(None)
    }

    /// Create a new RoomManager with shared video frame storage.
    pub fn with_shared_frames(shared_frames: Option<SharedVideoFrames>) -> (Self, mpsc::UnboundedReceiver<RoomEvent>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();

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
            video_playback: Arc::new(RwLock::new(video_playback)),
            screen_capture: Arc::new(RwLock::new(ScreenCapture::new())),
            key_provider: Arc::new(RwLock::new(None)),
            key_epoch: Arc::new(RwLock::new(0)),
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
        let room = self.room.read();
        let room = room.as_ref().ok_or(CallError::NotInCall)?;

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

        room.local_participant()
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
        let room = self.room.read();
        let room = room.as_ref().ok_or(CallError::NotInCall)?;

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

        room.local_participant()
            .publish_track(LocalTrack::Video(video_track), publish_options)
            .await
            .map_err(|e| CallError::TrackPublishFailed(e.to_string()))?;

        tracing::info!("Published camera track");
        Ok(())
    }

    /// Publish screen share to the room.
    ///
    /// # Arguments
    /// * `source` - Screen source to capture
    /// * `config` - Optional capture configuration
    pub async fn publish_screen_share(
        &self,
        source: ScreenSource,
        config: Option<ScreenCaptureConfig>,
    ) -> Result<(), CallError> {
        let room = self.room.read();
        let room = room.as_ref().ok_or(CallError::NotInCall)?;

        // Start screen capture
        let video_source = {
            let mut capture = self.screen_capture.write();
            capture.start(source, config)?
        };

        // Create and publish track
        let video_track = LocalVideoTrack::create_video_track(
            "screen",
            RtcVideoSource::Native(video_source),
        );

        let publish_options = TrackPublishOptions {
            source: TrackSource::Screenshare,
            ..Default::default()
        };

        room.local_participant()
            .publish_track(LocalTrack::Video(video_track), publish_options)
            .await
            .map_err(|e| CallError::TrackPublishFailed(e.to_string()))?;

        tracing::info!("Published screen share track");
        Ok(())
    }

    /// Stop microphone capture and unpublish track.
    pub fn stop_microphone(&self) {
        let mut capture = self.audio_capture.write();
        capture.stop();
        // Note: Track will be auto-unpublished when room disconnects
        // For explicit unpublish, we'd need to track the publication
    }

    /// Stop camera capture and unpublish track.
    pub fn stop_camera(&self) {
        let mut capture = self.video_capture.write();
        capture.stop();
    }

    /// Stop screen share capture and unpublish track.
    pub fn stop_screen_share(&self) {
        let mut capture = self.screen_capture.write();
        capture.stop();
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

    /// Toggle camera enabled.
    pub fn set_camera_enabled(&self, enabled: bool) {
        let mut capture = self.video_capture.write();
        capture.set_enabled(enabled);
    }

    /// Check if camera is enabled.
    pub fn is_camera_enabled(&self) -> bool {
        self.video_capture.read().is_enabled()
    }

    /// Check if screen sharing is active.
    pub fn is_screen_sharing(&self) -> bool {
        self.screen_capture.read().is_running()
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

    /// Get the latest video frame for a participant as a Slint Image.
    ///
    /// Returns None if no frame is available for the participant.
    pub fn get_video_frame(&self, participant_id: &str) -> Option<slint::Image> {
        self.video_playback.read().get_frame(participant_id)
    }

    /// Get all remote video frames as a Vec of Slint Images.
    pub fn get_all_video_frames(&self) -> Vec<slint::Image> {
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

        // Create key provider with Signal-derived key
        let key_provider = KeyProvider::with_shared_key(
            KeyProviderOptions::default(),
            encryption_key,
        );

        // Store key provider for later ratcheting
        {
            let mut kp = self.key_provider.write();
            *kp = Some(key_provider.clone());
        }
        *self.key_epoch.write() = 0;

        // Configure room with E2EE
        let mut room_options = RoomOptions::default();
        room_options.auto_subscribe = true;
        room_options.encryption = Some(E2eeOptions {
            encryption_type: EncryptionType::Gcm,
            key_provider,
        });

        // Connect to room
        let (room, mut events) = Room::connect(url, token, room_options)
            .await
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
            let _ = self.event_tx.send(RoomEvent::ParticipantJoined {
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
                            let _ = self.event_tx.send(RoomEvent::TrackSubscribed {
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
                            let _ = self.event_tx.send(RoomEvent::TrackSubscribed {
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
        tokio::task::spawn_local(async move {
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
            let _ = event_tx.send(RoomEvent::StateChanged(RoomState::Disconnected));
        });

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
        }
        {
            let screen = self.screen_capture.read();
            screen.signal_stop();
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
        let kp = self.key_provider.read();
        if let Some(ref key_provider) = *kp {
            // LiveKit's ratchet_shared_key derives the next key from the current one
            // key_index 0 is the shared key slot - always ratchet at index 0
            if let Some(_new_key) = key_provider.ratchet_shared_key(0) {
                // Track our epoch
                let mut epoch = self.key_epoch.write();
                *epoch += 1;
                let new_epoch = *epoch;

                tracing::info!(epoch = new_epoch, "E2EE key ratcheted");
                Some(new_epoch)
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
        *self.key_epoch.read()
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
        let kp = self.key_provider.read();
        let room = self.room.read();

        if let (Some(key_provider), Some(room)) = (&*kp, &*room) {
            // Increment epoch first
            let mut epoch = self.key_epoch.write();
            *epoch += 1;
            let new_epoch = *epoch;

            // Set key using local participant's identity at the new epoch index
            let local_identity = room.local_participant().identity();
            let success = key_provider.set_key(&local_identity, new_epoch as i32, key);

            if success {
                tracing::info!(epoch = new_epoch, "E2EE key updated externally");
            } else {
                tracing::warn!(epoch = new_epoch, "Failed to set E2EE key");
            }
            success
        } else {
            tracing::warn!("Cannot set key: not connected");
            false
        }
    }

    fn set_state(&self, state: RoomState) {
        *self.state.write() = state.clone();
        let _ = self.event_tx.send(RoomEvent::StateChanged(state));
    }

    fn handle_room_event(
        event: livekit::RoomEvent,
        event_tx: &mpsc::UnboundedSender<RoomEvent>,
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
                match event_tx.send(event) {
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

                // Remove video playback for this participant
                video_playback.read().remove_track(&participant_id);

                let _ = event_tx.send(RoomEvent::ParticipantLeft {
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

                let _ = event_tx.send(RoomEvent::TrackSubscribed {
                    participant_identity: participant_id,
                    track_sid: track.sid().to_string(),
                    is_video,
                    is_screen_share,
                });
            }
            livekit::RoomEvent::TrackUnsubscribed { track, participant, .. } => {
                let participant_id = participant.identity().to_string();

                // Remove audio playback if it's an audio track
                if matches!(track, RemoteTrack::Audio(_)) {
                    if let Some(playback) = audio_playback.read().as_ref() {
                        playback.remove_track(&participant_id);
                        tracing::info!("RoomManager: Removed audio track from playback for {}", participant_id);
                    }
                }

                // Remove video playback if it's a video track
                if matches!(track, RemoteTrack::Video(_)) {
                    video_playback.read().remove_track(&participant_id);
                    tracing::info!("RoomManager: Removed video track from playback for {}", participant_id);
                }

                let _ = event_tx.send(RoomEvent::TrackUnsubscribed {
                    participant_identity: participant_id,
                    track_sid: track.sid().to_string(),
                });
            }
            livekit::RoomEvent::E2eeStateChanged { participant, state: enc_state } => {
                use livekit::webrtc::native::frame_cryptor::EncryptionState;
                let is_encrypted = matches!(enc_state, EncryptionState::Ok | EncryptionState::KeyRatcheted);
                let _ = event_tx.send(RoomEvent::EncryptionStateChanged {
                    participant_identity: participant.identity().to_string(),
                    is_encrypted,
                });
            }
            _ => {}
        }
    }
}
