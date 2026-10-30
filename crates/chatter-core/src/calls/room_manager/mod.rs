//! LiveKit room connection management.

// LiveKit types don't implement Send+Sync
#![allow(clippy::arc_with_non_send_sync)]

use livekit::e2ee::{
    E2eeOptions, EncryptionType,
    key_provider::{KeyProvider, KeyProviderOptions},
};
use livekit::options::TrackPublishOptions;
use livekit::prelude::*;
// Desktop screen-share encoder tuning.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use livekit::options::{DegradationPreference, VideoCodec, VideoEncoderBackend, VideoEncoding};
use livekit::webrtc::audio_source::RtcAudioSource;
use livekit::webrtc::video_source::RtcVideoSource;
#[cfg(not(target_os = "ios"))]
use livekit::webrtc::video_source::{VideoResolution, native::NativeVideoSource};
use parking_lot::RwLock;
use std::sync::Arc;
use tokio::sync::mpsc;

use super::audio_capture::{AudioCapture, AudioDevice};
use super::audio_playback::AudioPlayback;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use super::screen_capture::{ScreenCapture, ScreenCaptureConfig, ScreenSource};
use super::shared_frames::SharedVideoFrames;
use super::timing::CallTiming;
use super::video_capture::{VideoCapture, VideoCaptureConfig, VideoDevice};
use super::video_playback::VideoPlayback;
use crate::error::CallError;

/// Room-wide SDK evidence, emitted once per connection. LiveKit omits the
/// track from this event: this cannot attribute success to audio, a particular
/// speaker, every packet, or physical playback. Local encryption and key
/// ratcheting alone must never be reported as receiver success.
#[derive(Default)]
struct ReceiverCryptorEvidence {
    reported_ok: bool,
}

impl ReceiverCryptorEvidence {
    fn observe(
        &mut self,
        remote: bool,
        state: livekit::webrtc::native::frame_cryptor::EncryptionState,
    ) -> bool {
        if remote
            && state == livekit::webrtc::native::frame_cryptor::EncryptionState::Ok
            && !self.reported_ok
        {
            self.reported_ok = true;
            return true;
        }
        false
    }
}

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
    /// The SDK is recovering the session on its own (resume or full
    /// restart, up to its own attempt budget). Keys stay installed; nothing
    /// application-side may join again while this lasts.
    Reconnecting {
        room_name: String,
        participant_count: usize,
    },
    /// Connection failed, or an established session was lost for good: the
    /// SDK gave up (or the server removed us). Terminal for this connection;
    /// only an application reconnect with a fresh token can follow.
    Failed { reason: String },
}

/// Whether a server-reported disconnect is worth one application-level
/// reconnect (fresh token, new session) or ends the call outright.
fn loss_is_retryable(reason: livekit::DisconnectReason) -> bool {
    use livekit::DisconnectReason as R;
    matches!(
        reason,
        R::UnknownReason
            | R::SignalClose
            | R::ConnectionTimeout
            | R::StateMismatch
            | R::JoinFailure
            | R::MediaFailure
    )
}

/// Room event for UI updates.
#[derive(Clone, Debug)]
pub enum RoomEvent {
    /// Connection state changed.
    StateChanged(RoomState),
    /// An established session is gone and the SDK will not bring it back
    /// (`StateChanged(Failed)` precedes this). `generation` identifies the
    /// connection that died, so a late notice cannot act on a newer one.
    ConnectionLost {
        generation: u64,
        reason: String,
        retryable: bool,
    },
    /// Reconsider canonical membership for this connection after departure/recovery.
    MembershipChanged { generation: u64 },
    /// Recovery notice scoped to a connection; never emitted for initial connect.
    RecoveryPhase { generation: u64, recovered: bool },
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
    /// The set of currently-speaking participants changed. Carries every
    /// speaking identity (local included); the UI marks matching tiles and
    /// clears the rest so the "who's talking" ring follows the audio.
    ActiveSpeakersChanged { speaker_identities: Vec<String> },
}

/// LiveKit E2EE key-ring slots: a peer up to 15 epochs behind still decrypts.
const KEY_RING_SIZE: i32 = 16;

/// The key-ring slot for chain `epoch`. LiveKit reduces the index modulo the
/// ring size itself; doing it here keeps every index a valid non-negative
/// `i32` for any `u32` epoch. 2^32 is a multiple of the ring size, so
/// `wrapping_add(1)` still lands on the next slot at `u32::MAX`.
fn key_ring_index(epoch: u32) -> i32 {
    (epoch % KEY_RING_SIZE as u32) as i32
}

/// E2EE key state (key provider + epoch under one lock).
struct E2eeState {
    provider: Option<KeyProvider>,
    epoch: u32,
}

// Keep native ownership of the receive ring: get_shared_key exports derived
// native material, which must never be passed back through SetSharedKey.
pub(crate) struct ReconnectKeys {
    provider: KeyProvider,
    epoch: u32,
    current: zeroize::Zeroizing<Vec<u8>>,
    next: zeroize::Zeroizing<Vec<u8>>,
}

/// Synchronous provisional-room transaction: never expose a room or commit
/// chain authority until every bounded catch-up install succeeds. The callback
/// is the real room E2EE manager in production and permits precise fault points
/// in tests using the retained native provider.
fn initialize_provisional_keys(
    snapshot: Option<&ReconnectKeys>,
    catch_up: Option<(&mut crate::crypto::call_key_provider::CallKeyState, u32)>,
    mut install: impl FnMut(u32, Vec<u8>, Vec<u8>) -> bool,
) -> Result<Option<u32>, CallError> {
    let result = (|| {
        if let Some(snapshot) = snapshot {
            if !install(
                snapshot.epoch,
                snapshot.current.to_vec(),
                snapshot.next.to_vec(),
            ) {
                return Err(CallError::CryptoError(
                    "Reconnect media key installation failed".into(),
                ));
            }
        }
        if let Some((chain, target)) = catch_up {
            crate::services::call_worker::install_reconnect_catch_up(chain, target, &mut install)?;
            return Ok(Some(chain.epoch()));
        }
        Ok(None)
    })();
    if result.is_err() {
        if let Some(snapshot) = snapshot {
            if !install(
                snapshot.epoch,
                snapshot.current.to_vec(),
                snapshot.next.to_vec(),
            ) {
                // Restore retry authority metadata even under persistent native
                // rejection. This cannot guarantee restoration of native bytes.
                snapshot
                    .provider
                    .set_shared_key(snapshot.current.to_vec(), key_ring_index(snapshot.epoch));
            }
        }
    }
    result
}

/// Manages LiveKit room connections with E2EE.
pub struct RoomManager {
    timing: RwLock<Option<CallTiming>>,
    /// Current room (if connected).
    room: Arc<RwLock<Option<Room>>>,
    /// Current state.
    state: Arc<RwLock<RoomState>>,
    /// Event sender for UI updates (bounded for backpressure).
    event_tx: mpsc::Sender<RoomEvent>,
    /// Audio capture manager.
    audio_capture: Arc<RwLock<AudioCapture>>,
    /// Published microphone track, retained for an atomic device handoff.
    microphone_track_sid: Arc<RwLock<Option<TrackSid>>>,
    /// A muted microphone remains requested even though capture is paused.
    microphone_requested: std::sync::atomic::AtomicBool,
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
    /// Capture explicitly retained during a room transport replacement.
    camera_capture_retained: std::sync::atomic::AtomicBool,
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
    #[cfg(not(target_os = "ios"))]
    screen_share_track_sid: Arc<RwLock<Option<TrackSid>>>,
    /// E2EE key state (provider + epoch).
    e2ee: Arc<RwLock<E2eeState>>,
    /// Handle to the spawned event handler task so disconnect() can await it.
    event_task: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    /// Bumped per connect attempt. The event handler of a connection carries
    /// its own value, so a detached old handler (disconnect join timed out)
    /// can neither mark a newer connection disconnected nor report its loss.
    connection_generation: Arc<std::sync::atomic::AtomicU64>,
    /// Invalidates last-peer grace across SDK recovery without retiring the room.
    membership_revision: Arc<std::sync::atomic::AtomicU64>,
    /// Shared frame store the UI reads. Kept here (not just inside
    /// `video_playback`) so the capture path can tee the LOCAL camera/screen
    /// feed into it for the self-preview tile.
    shared_frames: Option<SharedVideoFrames>,
}

impl RoomManager {
    /// Create a new RoomManager.
    pub fn new() -> (Self, mpsc::Receiver<RoomEvent>) {
        Self::with_shared_frames(None)
    }

    /// Create a new RoomManager with shared video frame storage.
    pub fn with_shared_frames(
        shared_frames: Option<SharedVideoFrames>,
    ) -> (Self, mpsc::Receiver<RoomEvent>) {
        let (event_tx, event_rx) = mpsc::channel(256);

        // Initialize audio playback (may fail if no output device)
        let audio_playback = match AudioPlayback::new() {
            Ok(playback) => Some(playback),
            Err(e) => {
                tracing::warn!(
                    "Failed to initialize audio playback: {}. Remote audio will not be played.",
                    e
                );
                None
            }
        };

        // Initialize video playback with shared frames if provided
        let video_playback = match &shared_frames {
            Some(shared) => VideoPlayback::with_shared_frames(shared.clone()),
            None => VideoPlayback::new(),
        };

        let manager = Self {
            timing: RwLock::new(None),
            room: Arc::new(RwLock::new(None)),
            state: Arc::new(RwLock::new(RoomState::Disconnected)),
            event_tx,
            audio_capture: Arc::new(RwLock::new(AudioCapture::new())),
            microphone_track_sid: Arc::new(RwLock::new(None)),
            microphone_requested: std::sync::atomic::AtomicBool::new(false),
            audio_playback: Arc::new(RwLock::new(audio_playback)),
            video_capture: Arc::new(RwLock::new(VideoCapture::new())),
            camera_published: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            camera_capture_retained: std::sync::atomic::AtomicBool::new(false),
            camera_track_sid: Arc::new(RwLock::new(None)),
            video_playback: Arc::new(RwLock::new(video_playback)),
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            screen_capture: Arc::new(RwLock::new(ScreenCapture::new())),
            #[cfg(not(target_os = "ios"))]
            screen_share_track_sid: Arc::new(RwLock::new(None)),
            e2ee: Arc::new(RwLock::new(E2eeState {
                provider: None,
                epoch: 0,
            })),
            event_task: Arc::new(RwLock::new(None)),
            connection_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            membership_revision: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            shared_frames,
        };
        (manager, event_rx)
    }
}

mod connection;
mod events;
mod keys;
mod media;

#[cfg(test)]
mod tests;

fn replace_timing_slot(slot: &RwLock<Option<CallTiming>>, next: CallTiming) {
    let mut current = slot.write();
    if let Some(previous) = current.take() {
        previous.retire();
    }
    *current = Some(next);
}

// Keep selection and retirement under the slot lock, so a failed old connect
// cannot clear or retire the replacement connection's observer.
fn retire_timing_slot(slot: &RwLock<Option<CallTiming>>, expected: Option<&CallTiming>) {
    let mut current = slot.write();
    if expected.is_none_or(|expected| {
        current
            .as_ref()
            .is_some_and(|timing| timing.same_attempt(expected))
    }) {
        if let Some(timing) = current.take() {
            timing.retire();
        }
    }
    if let Some(expected) = expected {
        expected.retire();
    }
}
