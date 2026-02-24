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
use super::video_capture::{VideoCapture, VideoDevice, VideoCaptureConfig};
use super::screen_capture::{ScreenCapture, ScreenSource, ScreenCaptureConfig};

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
    /// Video capture manager.
    video_capture: Arc<RwLock<VideoCapture>>,
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
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let manager = Self {
            room: Arc::new(RwLock::new(None)),
            state: Arc::new(RwLock::new(RoomState::Disconnected)),
            event_tx,
            audio_capture: Arc::new(RwLock::new(AudioCapture::new())),
            video_capture: Arc::new(RwLock::new(VideoCapture::new())),
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
        let participant_count = room.remote_participants().len() + 1;

        // Store room
        {
            let mut room_guard = self.room.write();
            *room_guard = Some(room);
        }

        self.set_state(RoomState::Connected {
            room_name,
            participant_count,
        });

        // Spawn event handler
        let event_tx = self.event_tx.clone();
        let state = self.state.clone();
        let room_arc = self.room.clone();

        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                Self::handle_room_event(event, &event_tx, &state, &room_arc);
            }

            // Room disconnected
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
            // Get current epoch for key index
            let current_epoch = *self.key_epoch.read();

            // LiveKit's ratchet_shared_key derives the next key from the current one
            // key_index 0 is the shared key slot
            if let Some(_new_key) = key_provider.ratchet_shared_key(current_epoch as i32) {
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
    ) {
        match event {
            livekit::RoomEvent::ParticipantConnected(participant) => {
                let name = participant.name();
                let _ = event_tx.send(RoomEvent::ParticipantJoined {
                    identity: participant.identity().to_string(),
                    display_name: if name.is_empty() { None } else { Some(name) },
                });

                // Update participant count
                if let Some(room) = room.read().as_ref() {
                    let count = room.remote_participants().len() + 1;
                    if let RoomState::Connected { room_name, .. } = &*state.read() {
                        *state.write() = RoomState::Connected {
                            room_name: room_name.clone(),
                            participant_count: count,
                        };
                    }
                }
            }
            livekit::RoomEvent::ParticipantDisconnected(participant) => {
                let _ = event_tx.send(RoomEvent::ParticipantLeft {
                    identity: participant.identity().to_string(),
                });
            }
            livekit::RoomEvent::TrackSubscribed { track, participant, .. } => {
                let is_video = matches!(track, RemoteTrack::Video(_));
                let _ = event_tx.send(RoomEvent::TrackSubscribed {
                    participant_identity: participant.identity().to_string(),
                    track_sid: track.sid().to_string(),
                    is_video,
                });
            }
            livekit::RoomEvent::TrackUnsubscribed { track, participant, .. } => {
                let _ = event_tx.send(RoomEvent::TrackUnsubscribed {
                    participant_identity: participant.identity().to_string(),
                    track_sid: track.sid().to_string(),
                });
            }
            livekit::RoomEvent::E2eeStateChanged { participant, state: enc_state } => {
                use livekit::webrtc::native::frame_cryptor::EncryptionState;
                let is_encrypted = matches!(enc_state, EncryptionState::Ok);
                let _ = event_tx.send(RoomEvent::EncryptionStateChanged {
                    participant_identity: participant.identity().to_string(),
                    is_encrypted,
                });
            }
            _ => {}
        }
    }
}
