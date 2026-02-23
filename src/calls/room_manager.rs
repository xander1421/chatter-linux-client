//! LiveKit room connection management.

use livekit::prelude::*;
use livekit::e2ee::{E2eeOptions, EncryptionType, key_provider::{KeyProvider, KeyProviderOptions}};
use tokio::sync::mpsc;
use std::sync::Arc;
use parking_lot::RwLock;

use crate::error::CallError;

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
}

impl RoomManager {
    /// Create a new RoomManager.
    pub fn new() -> (Self, mpsc::UnboundedReceiver<RoomEvent>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let manager = Self {
            room: Arc::new(RwLock::new(None)),
            state: Arc::new(RwLock::new(RoomState::Disconnected)),
            event_tx,
        };
        (manager, event_rx)
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
    pub async fn disconnect(&self) {
        let room = {
            let mut room_guard = self.room.write();
            room_guard.take()
        };

        if let Some(room) = room {
            let _ = room.close().await;
        }

        self.set_state(RoomState::Disconnected);
    }

    /// Get current state.
    pub fn state(&self) -> RoomState {
        self.state.read().clone()
    }

    /// Check if connected.
    pub fn is_connected(&self) -> bool {
        matches!(self.state(), RoomState::Connected { .. })
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
