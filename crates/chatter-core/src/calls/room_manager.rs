//! LiveKit room connection management.

// LiveKit types don't implement Send+Sync
#![allow(clippy::arc_with_non_send_sync)]

use livekit::e2ee::{
    E2eeOptions, EncryptionType,
    key_provider::{KeyProvider, KeyProviderOptions},
};
use livekit::options::TrackPublishOptions;
use livekit::prelude::*;
// Screen-share only (desktop): the publish path is cfg-gated off on mobile.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use livekit::options::{DegradationPreference, VideoCodec, VideoEncoderBackend, VideoEncoding};
use livekit::webrtc::audio_source::RtcAudioSource;
use livekit::webrtc::video_source::RtcVideoSource;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
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
            microphone_requested: std::sync::atomic::AtomicBool::new(false),
            audio_playback: Arc::new(RwLock::new(audio_playback)),
            video_capture: Arc::new(RwLock::new(VideoCapture::new())),
            camera_published: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            camera_track_sid: Arc::new(RwLock::new(None)),
            video_playback: Arc::new(RwLock::new(video_playback)),
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            screen_capture: Arc::new(RwLock::new(ScreenCapture::new())),
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
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
        self.publish_microphone_restored(device_id, false).await
    }

    pub(crate) async fn publish_microphone_restored(
        &self,
        device_id: Option<&str>,
        muted: bool,
    ) -> Result<(), CallError> {
        let local_participant = {
            let room = self.room.read();
            room.as_ref()
                .ok_or(CallError::NotInCall)?
                .local_participant()
        };

        // Start audio capture
        let audio_source = {
            let mut capture = self.audio_capture.write();
            let source = capture.start(device_id)?;
            // Pause a restored muted capture before publishing the new track.
            if muted {
                capture.set_muted(true);
            }
            source
        };

        // Create and publish track
        let audio_track =
            LocalAudioTrack::create_audio_track("microphone", RtcAudioSource::Native(audio_source));

        let publish_options = TrackPublishOptions {
            source: TrackSource::Microphone,
            ..Default::default()
        };

        local_participant
            .publish_track(LocalTrack::Audio(audio_track), publish_options)
            .await
            .map_err(|e| CallError::TrackPublishFailed(e.to_string()))?;

        self.microphone_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
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
            room.as_ref()
                .ok_or(CallError::NotInCall)?
                .local_participant()
        };

        // Tee the local camera to the self-preview ("You") tile, keyed by the
        // local participant identity (== the "You" tile's user_id).
        let local_preview =
            self.shared_frames
                .as_ref()
                .map(|shared| super::video_capture::LocalPreview {
                    shared: shared.clone(),
                    local_id: local_participant.identity().to_string(),
                    screen_share: false,
                });

        // Start video capture
        let video_source = {
            let mut capture = self.video_capture.write();
            capture.start(device_path, config, local_preview)?
        };

        // Create and publish track
        let video_track =
            LocalVideoTrack::create_video_track("camera", RtcVideoSource::Native(video_source));

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
            room.as_ref()
                .ok_or(CallError::NotInCall)?
                .local_participant()
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

        // Tee the local screen feed to its OWN tile (throttled inside the
        // capture loop), keyed by `screen_share_frame_id` — NOT the bare local
        // id — so it doesn't overwrite the camera in the shared "You" slot.
        let local_preview =
            self.shared_frames
                .as_ref()
                .map(|shared| super::video_capture::LocalPreview {
                    shared: shared.clone(),
                    local_id: super::screen_share_frame_id(
                        &local_participant.identity().to_string(),
                        true,
                    ),
                    screen_share: true,
                });

        let handshake = {
            let mut capture = self.screen_capture.write();
            capture.begin(source, config, local_preview)?
        };

        // Wait for the capture thread's probe frame to come back with
        // real dims. Portal selection can take a while; the timeout is
        // generous but bounded so a cancelled portal doesn't hang the
        // call worker forever.
        let dims_result =
            tokio::time::timeout(std::time::Duration::from_secs(120), handshake.dims_rx).await;

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
                    "Timed out waiting for screen capture probe frame (portal dialog cancelled?)"
                        .to_string(),
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

        let video_track =
            LocalVideoTrack::create_video_track("screen", RtcVideoSource::Native(video_source));

        // Screen-share track tuned for high-res content (e.g. 5120×1440
        // ultrawides). Settings validated by a live end-to-end wire sweep
        // (examples/screenshare_wire_bench.rs, since removed; see
        // `git log -- crates/chatter-core/examples`). The headline result at 5120×1440:
        //   default (MaintainResolution)      →  30 fps, 149 ms latency
        //   THIS (Disabled + H265 + NVENC)    → 104 fps,  30-45 ms, PSNR ~44
        // i.e. 3.5× the framerate at FULL native resolution, visually lossless.
        //
        // degradation_preference = MaintainFramerateAndResolution (== the old
        // "Disabled") is the single most important knob. The SDK defaults a
        // Screenshare source to MaintainResolution, which tells libwebrtc's
        // CPU-overuse detector to hold resolution and DROP FRAMERATE under load —
        // that was our hard ~30-35 fps wall, codec- and bitrate-independent
        // (measured flat from 60→800 Mbps). Disabled throttles neither axis.
        //
        // video_encoder = Nvenc forces the HW HEVC path (confirmed engaged via
        // NVENC session-init logs). H.265 is the winner ONLY once the throttle is
        // off: at the wall all codecs tied ~33 fps, but with Disabled+NVENC H.265
        // holds 104 fps steady while SW VP8/VP9 manage 45-75 and thrash. Our
        // earlier "H.265 = 36 fps regression" was MaintainResolution strangling
        // it, not the codec. On non-NVIDIA hosts the SDK falls back (VAAPI/SW);
        // per-GPU encoder selection is a follow-up.
        //
        // simulcast=false runs a single encoder and, with an explicit
        // `video_encoding`, keeps us off the hardcoded 1080p/30/3 Mbps
        // screenshare preset (which otherwise caps ~27 fps at any resolution).
        //
        // Bitrate CEILING scales with pixel count (~16 bits/px), clamped
        // [10, 150] Mbps. The sweep showed H.265 is near-lossless (PSNR 44) even
        // at 60 Mbps here, and fps is flat vs bitrate — this is just headroom, a
        // ceiling the congestion controller stays under on constrained links.
        let pixels = (width as u64) * (height as u64);
        let max_bitrate = (pixels * 16).clamp(10_000_000, 150_000_000);
        let publish_options = TrackPublishOptions {
            source: TrackSource::Screenshare,
            video_codec: VideoCodec::H265,
            video_encoder: VideoEncoderBackend::Nvenc,
            degradation_preference: Some(DegradationPreference::MaintainFramerateAndResolution),
            simulcast: false,
            video_encoding: Some(VideoEncoding {
                max_bitrate,
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
            "Published screen share track (H.265/NVENC, degradation=disabled, simulcast off, {} fps cap, {} Mbps, {}x{}, sid={})",
            target_fps,
            max_bitrate / 1_000_000,
            width,
            height,
            sid
        );
        Ok(())
    }

    /// Stop microphone capture and unpublish track.
    pub fn stop_microphone(&self) {
        self.microphone_requested
            .store(false, std::sync::atomic::Ordering::SeqCst);
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
        // Drop the self-preview frame so the "You" tile reverts to the avatar.
        if let Some(ref shared) = self.shared_frames {
            shared.remove_frame(&local_participant.identity().to_string());
        }
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
        // Drop the local screen frame (its own tile key) so the screen tile
        // clears. The camera tile (bare local id) is untouched.
        if let Some(ref shared) = self.shared_frames {
            shared.remove_frame(&super::screen_share_frame_id(
                &local_participant.identity().to_string(),
                true,
            ));
        }
    }

    /// Toggle microphone mute.
    pub fn set_microphone_muted(&self, muted: bool) {
        let mut capture = self.audio_capture.write();
        capture.set_muted(muted);
    }

    pub(crate) fn reconnect_microphone_device(&self) -> Option<String> {
        self.microphone_requested
            .load(std::sync::atomic::Ordering::SeqCst)
            .then(|| self.audio_capture.read().device_name().to_string())
    }

    /// Check if microphone is muted.
    pub fn is_microphone_muted(&self) -> bool {
        self.audio_capture.read().is_muted()
    }

    /// Snapshot the effective selection before disconnect clears publication.
    pub(crate) fn reconnect_camera_selection(&self) -> Option<(String, VideoCaptureConfig)> {
        self.is_camera_enabled()
            .then(|| self.video_capture.read().reconnect_selection())
    }

    /// Check if the camera is currently on (track published).
    pub fn is_camera_enabled(&self) -> bool {
        self.camera_published
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Check if screen sharing is active.
    pub fn is_screen_sharing(&self) -> bool {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            self.screen_capture.read().is_running()
        }
        #[cfg(any(target_os = "android", target_os = "ios"))]
        {
            false
        }
    }

    /// Set playback volume for remote audio (0.0 to 1.0).
    pub fn set_playback_volume(&self, volume: f32) {
        if let Some(playback) = self.audio_playback.read().as_ref() {
            playback.set_volume(volume);
        }
    }

    /// Route remote audio to the named output device (see
    /// [`AudioPlayback::set_output_device`]).
    pub fn set_audio_output(&self, device: &str) -> Result<(), CallError> {
        match self.audio_playback.read().as_ref() {
            Some(playback) => playback.set_output_device(device),
            None => Err(CallError::DeviceError(
                "Audio playback unavailable".to_string(),
            )),
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
        self.video_playback
            .read()
            .set_peer_paused(participant_id, paused);
    }

    /// Whether a remote participant's video is locally paused.
    pub fn is_peer_video_paused(&self, participant_id: &str) -> bool {
        self.video_playback.read().is_peer_paused(participant_id)
    }

    /// Get the latest video frame for a participant as raw data.
    ///
    /// Returns None if no frame is available for the participant.
    pub fn get_video_frame(
        &self,
        participant_id: &str,
    ) -> Option<super::shared_frames::VideoFrameData> {
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
        self.connect_timed(
            url,
            token,
            encryption_key,
            CallTiming::new("room_connect_requested"),
        )
        .await
    }

    pub(crate) fn timing_mark(&self, stage: &'static str) {
        if let Some(timing) = self.timing.read().as_ref() {
            timing.mark(stage);
        }
    }

    fn retire_timing(&self, expected: Option<&CallTiming>) {
        retire_timing_slot(&self.timing, expected);
    }

    pub(crate) async fn connect_timed(
        &self,
        url: &str,
        token: &str,
        encryption_key: Vec<u8>,
        timing: CallTiming,
    ) -> Result<(), CallError> {
        self.microphone_requested
            .store(false, std::sync::atomic::Ordering::SeqCst);
        // Disable the SDK ratchet: the application owns the HKDF epoch chain.
        let key_provider = KeyProvider::with_shared_key(
            KeyProviderOptions {
                ratchet_window_size: 0,
                key_ring_size: KEY_RING_SIZE,
                ..KeyProviderOptions::default()
            },
            encryption_key,
        );
        self.connect_with_provider(url, token, key_provider, 0, timing, None, None)
            .await
    }

    pub(crate) fn snapshot_reconnect_keys(
        &self,
        chain: &crate::crypto::call_key_provider::CallKeyState,
    ) -> Result<ReconnectKeys, CallError> {
        let e2ee = self.e2ee.read();
        let provider = e2ee.provider.clone().ok_or(CallError::NotInCall)?;
        if e2ee.epoch != chain.epoch()
            || provider.get_latest_key_index() != key_ring_index(chain.epoch())
        {
            return Err(CallError::CryptoError(
                "Reconnect key epoch mismatch".into(),
            ));
        }
        Ok(ReconnectKeys {
            provider,
            epoch: chain.epoch(),
            current: zeroize::Zeroizing::new(chain.current_key_vec()),
            next: zeroize::Zeroizing::new(
                chain
                    .peek_next_key()
                    .map_err(|e| CallError::CryptoError(e.to_string()))?
                    .to_vec(),
            ),
        })
    }

    pub(crate) async fn connect_reusing_keys(
        &self,
        url: &str,
        token: &str,
        snapshot: ReconnectKeys,
    ) -> Result<(), CallError> {
        self.connect_with_provider(
            url,
            token,
            snapshot.provider.clone(),
            snapshot.epoch,
            CallTiming::new("room_reconnect_requested"),
            Some(snapshot),
            None,
        )
        .await
    }

    /// Catch up on the provisional room, before its handle or Connected event
    /// can permit publication. The caller commits this staged chain on success.
    pub(crate) async fn connect_reusing_keys_aligned(
        &self,
        url: &str,
        token: &str,
        snapshot: ReconnectKeys,
        chain: &mut crate::crypto::call_key_provider::CallKeyState,
        started_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<(), CallError> {
        self.connect_with_provider(
            url,
            token,
            snapshot.provider.clone(),
            snapshot.epoch,
            CallTiming::new("room_reconnect_requested"),
            Some(snapshot),
            Some((chain, started_at)),
        )
        .await
    }

    async fn connect_with_provider(
        &self,
        url: &str,
        token: &str,
        key_provider: KeyProvider,
        epoch: u32,
        timing: CallTiming,
        snapshot: Option<ReconnectKeys>,
        catch_up: Option<(
            &mut crate::crypto::call_key_provider::CallKeyState,
            Option<chrono::DateTime<chrono::Utc>>,
        )>,
    ) -> Result<(), CallError> {
        // Signaling uses the same private CA as gRPC/storage, not the SDK's
        // default public root store. Validate before changing room state.
        let parsed = url::Url::parse(url)
            .map_err(|_| CallError::ConnectionFailed("invalid LiveKit endpoint".into()))?;
        let tls_root_ca_pem = match parsed.scheme() {
            "wss" | "https" => Some(
                crate::config::TLS_ROOT_CA_PEM
                    .ok_or_else(|| CallError::ConnectionFailed("missing pinned LiveKit TLS CA".into()))?
                    .to_owned(),
            ),
            "ws" | "http" if cfg!(any(test, feature = "dev-trust-root")) => None,
            _ => return Err(CallError::ConnectionFailed("LiveKit signaling requires WSS".into())),
        };
        replace_timing_slot(&self.timing, timing.clone());
        timing.mark("room_connect_begin");
        let generation = self
            .connection_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        self.set_state(RoomState::Connecting);

        // Store key provider for later ratcheting
        {
            let mut e2ee = self.e2ee.write();
            e2ee.provider = Some(key_provider.clone());
            e2ee.epoch = epoch;
        }

        // Configure room with E2EE
        let mut room_options = RoomOptions::default();
        room_options.auto_subscribe = true;
        room_options.tls_root_ca_pem = tls_root_ca_pem;
        room_options.encryption = Some(E2eeOptions {
            encryption_type: EncryptionType::Gcm,
            key_provider,
        });

        // Connect to room (timeout prevents indefinite hang on slow/dead servers)
        let connect_started = std::time::Instant::now();
        tracing::info!(
            stage = "room_connect_begin",
            elapsed_us = 0u64,
            "Call stage"
        );
        let connected = tokio::time::timeout(
            tokio::time::Duration::from_secs(10),
            Room::connect(url, token, room_options),
        )
        .await;
        let stage = match &connected {
            Ok(Ok(_)) => "room_connect_succeeded",
            Ok(Err(_)) => "room_connect_failed",
            Err(_) => "room_connect_timed_out",
        };
        tracing::info!(
            stage,
            elapsed_us = connect_started.elapsed().as_micros() as u64,
            "Call stage"
        );
        timing.mark(stage);
        if !matches!(&connected, Ok(Ok(_))) {
            self.retire_timing(Some(&timing));
        }
        let (room, mut events) = connected
            .map_err(|_| {
                CallError::ConnectionFailed("LiveKit connection timed out (10s)".to_string())
            })?
            .map_err(|e| CallError::ConnectionFailed(e.to_string()))?;

        // Complete key initialization before publishing the room handle, emitting
        // Connected, or permitting any new local media publication.
        let catch_up = catch_up.map(|(chain, started_at)| {
            let target = started_at
                .map(crate::services::call_worker::reconnect_wall_epoch)
                .unwrap_or(chain.epoch());
            (chain, target)
        });
        match initialize_provisional_keys(snapshot.as_ref(), catch_up, |epoch, current, next| {
            room.e2ee_manager().install_shared_media_key(
                key_ring_index(epoch),
                current,
                Some((key_ring_index(epoch.wrapping_add(1)), next)),
            )
        }) {
            Ok(Some(epoch)) => self.e2ee.write().epoch = epoch,
            Ok(None) => {}
            Err(error) => {
                let _ = room.close().await;
                self.set_state(RoomState::Disconnected);
                self.retire_timing(Some(&timing));
                return Err(error);
            }
        }

        // Enable E2EE
        room.e2ee_manager().set_enabled(true);

        let room_name = room.name().to_string();
        let remote_participants: Vec<_> = room.remote_participants().values().cloned().collect();
        let participant_count = remote_participants.len() + 1;
        timing.mark(if remote_participants.is_empty() {
            "remote_participant_waiting"
        } else {
            "remote_participant_present"
        });

        // Emit ParticipantJoined events for participants already in the room
        // (LiveKit only sends ParticipantConnected for NEW participants after we join)
        // Also subscribe to their existing tracks since TrackSubscribed events may have
        // already fired before we set up our event handler
        for participant in &remote_participants {
            let identity = participant.identity().to_string();
            let name = participant.name();
            tracing::info!(
                "RoomManager: Existing participant in room - identity={}, name={}",
                identity,
                name
            );
            let _ = self.event_tx.try_send(RoomEvent::ParticipantJoined {
                identity: identity.clone(),
                display_name: if name.is_empty() {
                    None
                } else {
                    Some(name.to_string())
                },
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
                                playback.add_track_timed(
                                    identity.clone(),
                                    audio_track,
                                    Some(timing.clone()),
                                );
                                tracing::info!(
                                    "RoomManager: Added existing audio track to playback"
                                );
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
                            let is_screen_share = matches!(
                                source,
                                TrackSource::Screenshare | TrackSource::ScreenshareAudio
                            );
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
        let generation_slot = self.connection_generation.clone();
        let membership_revision = self.membership_revision.clone();
        let handle = tokio::task::spawn_local(async move {
            tracing::info!("Room event handler started");
            let mut cryptor_evidence = ReceiverCryptorEvidence::default();
            while let Some(event) = events.recv().await {
                // All effects, including track/playback and membership, belong to this connection.
                if generation_slot.load(std::sync::atomic::Ordering::SeqCst) != generation {
                    break;
                }
                // Invalidate grace before any awaited control delivery at either
                // SDK recovery boundary. Connection generation stays unchanged.
                if matches!(&event, livekit::RoomEvent::Reconnecting | livekit::RoomEvent::Reconnected) {
                    membership_revision.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                // Avoid dumping arbitrary SDK event payloads and identities.
                if let livekit::RoomEvent::E2eeStateChanged { participant, state } = &event {
                    if cryptor_evidence.observe(
                        matches!(participant, livekit::participant::Participant::Remote(_)),
                        *state,
                    ) {
                        timing.mark("remote_frame_cryptor_ok_room_wide_track_unknown");
                        tracing::info!(
                            stage = "remote_frame_cryptor_ok",
                            "Remote SDK frame cryptor reported Ok (room-wide, track unavailable)"
                        );
                    }
                }
                // A detached handler (disconnect join timed out) must not
                // mirror its dead connection's lifecycle onto a newer one.
                let live = generation_slot.load(std::sync::atomic::Ordering::SeqCst) == generation;
                Self::handle_room_event(
                    event,
                    (generation, live),
                    &generation_slot,
                    &event_tx,
                    &state,
                    &room_arc,
                    &audio_playback,
                    &video_playback,
                    &timing,
                )
                .await;
                // Yield to allow other tasks (like forwarder) to run
                tokio::task::yield_now().await;
            }

            // Retire only this handler's observer: a detached old handler must
            // never retire a newer connection's timing.
            timing.retire();
            if generation_slot.load(std::sync::atomic::Ordering::SeqCst) != generation {
                return;
            }
            // Room disconnected - stop audio/video playback
            tracing::info!("Room event stream ended - marking disconnected");
            if let Some(playback) = audio_playback.read().as_ref() {
                playback.stop();
            }
            video_playback.read().stop();
            // Only this connection's handler may close its state, and a
            // terminal loss already recorded stays recorded (the worker
            // fences its cleanup on `Failed`).
            if generation_slot.load(std::sync::atomic::Ordering::SeqCst) == generation {
                let mut guard = state.write();
                if !matches!(*guard, RoomState::Failed { .. }) {
                    *guard = RoomState::Disconnected;
                    drop(guard);
                    let _ = event_tx.try_send(RoomEvent::StateChanged(RoomState::Disconnected));
                }
            }
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
        // Retire handler ownership before any await or reuse of shared playback.
        self.connection_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.retire_timing(None);
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
            match tokio::time::timeout(tokio::time::Duration::from_secs(3), room.close()).await {
                Ok(_) => tracing::info!("Room closed successfully"),
                Err(_) => tracing::warn!("Room close timed out, forcing disconnect"),
            }
        }

        // Wait for event handler task to finish draining
        let event_handle = self.event_task.write().take();
        if let Some(handle) = event_handle {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), handle).await {
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

    /// The session is alive: connected, or being recovered by the SDK with
    /// its keys still installed. Media publish gates that need a working
    /// transport check for `Connected` specifically.
    pub fn is_connected(&self) -> bool {
        matches!(
            self.state(),
            RoomState::Connected { .. } | RoomState::Reconnecting { .. }
        )
    }

    /// Identity of the current connect attempt ([`RoomEvent::ConnectionLost`]).
    pub fn connection_generation(&self) -> u64 {
        self.connection_generation
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Grace checks must belong to one uninterrupted SDK membership phase.
    pub fn membership_revision(&self) -> u64 {
        self.membership_revision.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Get current participant count.
    pub fn participant_count(&self) -> usize {
        match self.state() {
            RoomState::Connected {
                participant_count, ..
            }
            | RoomState::Reconnecting {
                participant_count, ..
            } => participant_count,
            _ => 0,
        }
    }

    /// Identities of the remote participants currently in the room.
    ///
    /// A LiveKit identity is the peer's `user_id` — the server mints call
    /// tokens with `SetIdentity(userID)` — so this is the live roster of who
    /// can decrypt media right now. Key rotation must target this, not channel
    /// membership: a peer who left the room is still a channel member.
    ///
    /// Empty when not connected.
    pub fn remote_participant_identities(&self) -> Vec<String> {
        match self.room.read().as_ref() {
            Some(room) => room
                .remote_participants()
                .keys()
                .map(|identity| identity.to_string())
                .collect(),
            None => Vec::new(),
        }
    }

    /// Our own LiveKit `joined_at_ms` and each remote participant's
    /// `(identity, joined_at_ms)` — the seniority the group-call key
    /// authority is chosen by. `None` when not connected.
    ///
    /// `joined_at_ms` is assigned by the LiveKit server, so every participant
    /// reads the same values; like the roster itself it is server-asserted.
    pub fn room_seniority(&self) -> Option<(i64, Vec<(String, i64)>)> {
        let room = self.room.read();
        let room = room.as_ref()?;
        let remote = room
            .remote_participants()
            .iter()
            .map(|(identity, participant)| (identity.to_string(), participant.joined_at()))
            .collect();
        Some((room.local_participant().joined_at(), remote))
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

    /// Test-only: the current epoch and every slot of the shared-key ring, as
    /// LiveKit's `KeyProvider` returns them. `None` without a key provider.
    #[cfg(feature = "testing")]
    pub fn installed_key_ring(&self) -> Option<(u32, Vec<Option<Vec<u8>>>)> {
        let e2ee = self.e2ee.read();
        let provider = e2ee.provider.as_ref()?;
        Some((
            e2ee.epoch,
            (0..KEY_RING_SIZE)
                .map(|i| provider.get_shared_key(i))
                .collect(),
        ))
    }

    /// Test-only: how many tracks we publish to the room. 0 when not connected.
    #[cfg(feature = "testing")]
    pub fn local_track_count(&self) -> usize {
        self.room
            .read()
            .as_ref()
            .map_or(0, |r| r.local_participant().track_publications().len())
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
    /// ring still holding older epochs. The SDK manager installs and selects
    /// the current sending epoch before preinstalling the future receive key,
    /// serialized with new sender creation. Receive preinstallation does not
    /// change the selected media or data sending key.
    pub fn set_ratcheted_shared_key(
        &self,
        epoch: u32,
        key_cur: Vec<u8>,
        key_next: Vec<u8>,
    ) -> bool {
        let mut e2ee = self.e2ee.write();
        match e2ee.provider.as_ref() {
            Some(_) => {}
            None => {
                tracing::warn!("Cannot ratchet key: no key provider");
                return false;
            }
        };
        let room = self.room.read();
        let Some(room) = room.as_ref() else {
            return false;
        };
        if !room.e2ee_manager().install_shared_media_key(
            key_ring_index(epoch),
            key_cur,
            Some((key_ring_index(epoch.wrapping_add(1)), key_next)),
        ) {
            return false;
        }
        e2ee.epoch = epoch;
        tracing::info!(
            epoch,
            sender_index = key_ring_index(epoch),
            "Media encryption epoch selected"
        );
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
        let key = zeroize::Zeroizing::new(key);
        let mut e2ee = self.e2ee.write();
        match e2ee.provider.as_ref() {
            Some(_) => {}
            None => {
                tracing::warn!("Cannot reset key: no key provider");
                return false;
            }
        };
        let room = self.room.read();
        let Some(room) = room.as_ref() else {
            return false;
        };
        if !room
            .e2ee_manager()
            .install_shared_media_key(0, key.to_vec(), None)
        {
            return false;
        }
        e2ee.epoch = 0;
        tracing::info!(
            epoch = 0u32,
            sender_index = 0i32,
            "Media encryption epoch selected"
        );
        tracing::info!("E2EE key re-rooted at index 0 (real DM key)");
        true
    }

    fn set_state(&self, state: RoomState) {
        *self.state.write() = state.clone();
        let _ = self.event_tx.try_send(RoomEvent::StateChanged(state));
    }

    // Reserve capacity asynchronously: terminal control must survive a full data queue.
    // Recheck ownership after waiting, before placing the event in the queue.
    async fn send_control(
        tx: &mpsc::Sender<RoomEvent>,
        owner: &std::sync::atomic::AtomicU64,
        generation: u64,
        event: RoomEvent,
    ) {
        if let Ok(permit) = tx.reserve().await {
            if owner.load(std::sync::atomic::Ordering::SeqCst) == generation {
                permit.send(event);
            }
        }
    }

    /// Canonical transport and membership are usable for peer-left policy.
    pub fn membership_is_current(&self) -> bool {
        matches!(self.state(), RoomState::Connected { .. })
            && self
                .room
                .read()
                .as_ref()
                .is_some_and(|r| r.connection_state() == livekit::ConnectionState::Connected)
    }

    async fn handle_room_event(
        event: livekit::RoomEvent,
        (generation, live): (u64, bool),
        generation_slot: &std::sync::atomic::AtomicU64,
        event_tx: &mpsc::Sender<RoomEvent>,
        state: &Arc<RwLock<RoomState>>,
        room: &Arc<RwLock<Option<Room>>>,
        audio_playback: &Arc<RwLock<Option<AudioPlayback>>>,
        video_playback: &Arc<RwLock<VideoPlayback>>,
        timing: &CallTiming,
    ) {
        if !live {
            return;
        }
        match event {
            livekit::RoomEvent::ParticipantConnected(participant) => {
                timing.mark("remote_participant_present");
                let identity = participant.identity().to_string();
                let name = participant.name();
                tracing::info!(
                    "RoomManager: ParticipantConnected - identity={}, name={}",
                    identity,
                    name
                );
                let event = RoomEvent::ParticipantJoined {
                    identity: identity.clone(),
                    display_name: if name.is_empty() {
                        None
                    } else {
                        Some(name.to_string())
                    },
                };
                tracing::info!("RoomManager: Sending event to channel: {:?}", event);
                match event_tx.try_send(event) {
                    Ok(()) => tracing::info!("RoomManager: Event sent successfully"),
                    Err(e) => tracing::error!(
                        "RoomManager: Failed to send ParticipantJoined event: {:?}",
                        e
                    ),
                }

                // Update participant count (avoid holding read lock while getting write lock)
                let new_state = {
                    let room_guard = room.read();
                    if let Some(r) = room_guard.as_ref() {
                        let count = r.remote_participants().len() + 1;
                        let state_guard = state.read();
                        match &*state_guard {
                            RoomState::Connected { room_name, .. } => Some(RoomState::Connected {
                                room_name: room_name.clone(),
                                participant_count: count,
                            }),
                            RoomState::Reconnecting { room_name, .. } => {
                                Some(RoomState::Reconnecting {
                                    room_name: room_name.clone(),
                                    participant_count: count,
                                })
                            }
                            _ => None,
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

                // Full restart evicts participants before delivering Reconnecting.
                // Consult the SDK state as well as our mirrored phase.
                let membership_current = matches!(*state.read(), RoomState::Connected { .. })
                    && room.read().as_ref().is_some_and(|r| {
                        r.connection_state() == livekit::ConnectionState::Connected
                    });
                if membership_current {
                    let _ = event_tx.try_send(RoomEvent::ParticipantLeft {
                        identity: participant_id,
                    });
                    Self::send_control(
                        event_tx,
                        generation_slot,
                        generation,
                        RoomEvent::MembershipChanged { generation },
                    )
                    .await;
                }
            }
            livekit::RoomEvent::TrackSubscribed {
                track, participant, ..
            } => {
                let participant_id = participant.identity().to_string();
                let is_video = matches!(track, RemoteTrack::Video(_));

                // Determine if this is a screen share track
                let is_screen_share = match &track {
                    RemoteTrack::Video(video_track) => {
                        matches!(
                            video_track.source(),
                            TrackSource::Screenshare | TrackSource::ScreenshareAudio
                        )
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
                            playback.add_track_timed(
                                participant_id.clone(),
                                audio_track.clone(),
                                Some(timing.clone()),
                            );
                            tracing::info!("RoomManager: Added audio track to playback");
                        } else {
                            tracing::warn!(
                                "RoomManager: No audio playback available, cannot play remote audio"
                            );
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
            livekit::RoomEvent::TrackUnsubscribed {
                track, participant, ..
            } => {
                let participant_id = participant.identity().to_string();
                let is_video = matches!(track, RemoteTrack::Video(_));
                let mut is_screen_share = false;

                // Remove audio playback if it's an audio track
                if matches!(track, RemoteTrack::Audio(_)) {
                    if let Some(playback) = audio_playback.read().as_ref() {
                        playback.remove_track_if_sid(&participant_id, &track.sid().to_string());
                        tracing::info!(
                            "RoomManager: Removed audio track from playback for {}",
                            participant_id
                        );
                    }
                }

                // Remove video playback if it's a video track
                if let RemoteTrack::Video(ref v) = track {
                    is_screen_share = matches!(
                        v.source(),
                        TrackSource::Screenshare | TrackSource::ScreenshareAudio
                    );
                    video_playback
                        .read()
                        .remove_track(&participant_id, is_screen_share);
                    tracing::info!(
                        "RoomManager: Removed video track from playback for {} (screen_share={})",
                        participant_id,
                        is_screen_share
                    );
                }

                let _ = event_tx.try_send(RoomEvent::TrackUnsubscribed {
                    participant_identity: participant_id,
                    track_sid: track.sid().to_string(),
                    is_video,
                    is_screen_share,
                });
            }
            livekit::RoomEvent::ConnectionQualityChanged {
                participant,
                quality,
            } => {
                use livekit::participant::ConnectionQuality as CQ;
                let score: u8 = match quality {
                    CQ::Excellent => 4,
                    CQ::Good => 3,
                    CQ::Poor => 1,
                    _ => 0,
                };
                let _ = event_tx.try_send(RoomEvent::ConnectionQualityChanged {
                    participant_identity: participant.identity().to_string(),
                    quality: score,
                });
            }
            livekit::RoomEvent::E2eeStateChanged {
                participant,
                state: enc_state,
            } => {
                use livekit::webrtc::native::frame_cryptor::EncryptionState;
                let is_encrypted = matches!(
                    enc_state,
                    EncryptionState::Ok | EncryptionState::KeyRatcheted
                );
                let _ = event_tx.try_send(RoomEvent::EncryptionStateChanged {
                    participant_identity: participant.identity().to_string(),
                    is_encrypted,
                });
            }
            livekit::RoomEvent::ActiveSpeakersChanged { speakers } => {
                let speaker_identities =
                    speakers.iter().map(|p| p.identity().to_string()).collect();
                let _ = event_tx.try_send(RoomEvent::ActiveSpeakersChanged { speaker_identities });
            }
            // SDK-owned recovery: the session, its tracks and its installed
            // keys survive; we only mirror the phase. Never join again here.
            livekit::RoomEvent::Reconnecting => {
                if !live {
                    return;
                }
                let next = match &*state.read() {
                    RoomState::Connected {
                        room_name,
                        participant_count,
                    } => Some(RoomState::Reconnecting {
                        room_name: room_name.clone(),
                        participant_count: *participant_count,
                    }),
                    _ => None,
                };
                if let Some(next) = next {
                    tracing::warn!("Room connection interrupted; SDK reconnecting");
                    *state.write() = next.clone();
                    let _ = event_tx.try_send(RoomEvent::StateChanged(next));
                    Self::send_control(
                        event_tx,
                        generation_slot,
                        generation,
                        RoomEvent::RecoveryPhase {
                            generation,
                            recovered: false,
                        },
                    )
                    .await;
                }
            }
            livekit::RoomEvent::Reconnected => {
                if !live {
                    return;
                }
                let count = room
                    .read()
                    .as_ref()
                    .map(|r| r.remote_participants().len() + 1)
                    .unwrap_or(1);
                let next = match &*state.read() {
                    RoomState::Reconnecting { room_name, .. } => Some(RoomState::Connected {
                        room_name: room_name.clone(),
                        participant_count: count,
                    }),
                    _ => None,
                };
                if let Some(next) = next {
                    tracing::info!("Room connection recovered by SDK");
                    *state.write() = next.clone();
                    Self::send_control(
                        event_tx,
                        generation_slot,
                        generation,
                        RoomEvent::RecoveryPhase {
                            generation,
                            recovered: true,
                        },
                    )
                    .await;
                    Self::send_control(
                        event_tx,
                        generation_slot,
                        generation,
                        RoomEvent::StateChanged(next),
                    )
                    .await;
                    Self::send_control(
                        event_tx,
                        generation_slot,
                        generation,
                        RoomEvent::MembershipChanged { generation },
                    )
                    .await;
                }
            }
            // Terminal for this connection: the SDK closes the room next and
            // the stream-end branch below stops playback. `ClientInitiated` is
            // our own `disconnect()`, already recorded there.
            livekit::RoomEvent::Disconnected { reason } => {
                if !live || reason == livekit::DisconnectReason::ClientInitiated {
                    return;
                }
                let lost = matches!(
                    &*state.read(),
                    RoomState::Connected { .. } | RoomState::Reconnecting { .. }
                );
                if !lost {
                    return;
                }
                let reason_text = format!("{reason:?}");
                tracing::warn!(reason = %reason_text, "Room connection lost");
                let failed = RoomState::Failed {
                    reason: reason_text.clone(),
                };
                *state.write() = failed.clone();
                Self::send_control(
                    event_tx,
                    generation_slot,
                    generation,
                    RoomEvent::StateChanged(failed),
                )
                .await;
                Self::send_control(
                    event_tx,
                    generation_slot,
                    generation,
                    RoomEvent::ConnectionLost {
                        generation,
                        reason: reason_text,
                        retryable: loss_is_retryable(reason),
                    },
                )
                .await;
            }
            // Redundant with Reconnecting/Reconnected/Disconnected above.
            livekit::RoomEvent::ConnectionStateChanged(_) => {}
            _ => {}
        }
    }
}

#[cfg(test)]
mod loss_policy_tests {
    use super::loss_is_retryable;
    use livekit::DisconnectReason as R;

    #[test]
    fn only_transport_losses_earn_an_application_reconnect() {
        for reason in [R::SignalClose, R::ConnectionTimeout, R::UnknownReason] {
            assert!(loss_is_retryable(reason), "{reason:?}");
        }
        for reason in [
            R::ClientInitiated,
            R::DuplicateIdentity,
            R::ParticipantRemoved,
            R::RoomDeleted,
            R::RoomClosed,
            R::ServerShutdown,
            R::UserRejected,
        ] {
            assert!(!loss_is_retryable(reason), "{reason:?}");
        }
    }
}

#[cfg(test)]
mod receiver_cryptor_evidence_tests {
    use super::ReceiverCryptorEvidence;
    use livekit::webrtc::native::frame_cryptor::EncryptionState as State;

    #[test]
    fn local_encryption_and_remote_key_changes_are_not_receiver_success() {
        let mut evidence = ReceiverCryptorEvidence::default();
        assert!(!evidence.observe(false, State::Ok));
        for state in [
            State::New,
            State::KeyRatcheted,
            State::MissingKey,
            State::DecryptionFailed,
            State::EncryptionFailed,
            State::InternalError,
        ] {
            assert!(!evidence.observe(true, state));
        }
        assert!(evidence.observe(true, State::Ok));
        assert!(!evidence.observe(true, State::Ok));
        assert!(!evidence.observe(true, State::DecryptionFailed));
        assert!(!evidence.observe(true, State::Ok));
        // A fresh connection gets its own evidence; no cross-call inheritance.
        assert!(ReceiverCryptorEvidence::default().observe(true, State::Ok));
    }
}

#[cfg(test)]
mod key_ring_tests {
    use super::key_ring_index;

    #[test]
    fn key_ring_index_never_overflows() {
        assert_eq!(key_ring_index(0), 0);
        assert_eq!(key_ring_index(17), 1);
        assert_eq!(key_ring_index(u32::MAX), 15);
        assert_eq!(
            key_ring_index(u32::MAX.wrapping_add(1)),
            0,
            "next slot after the last epoch"
        );
    }
}

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

#[cfg(test)]
mod timing_lifecycle_tests {
    use super::{CallTiming, RwLock, replace_timing_slot, retire_timing_slot};

    #[test]
    fn disconnect_then_failed_reconnect_rejects_queued_key_milestone() {
        let old = CallTiming::new("join_requested_worker");
        let slot = RwLock::new(Some(old.clone()));
        retire_timing_slot(&slot, None);
        // Token refresh fails before connect_timed can install a new observer.
        assert!(slot.read().is_none());
        assert!(!old.mark("call_key_accepted_publish_gate_open"));
        assert!(!old.first_decoded_frame());
    }

    #[test]
    fn failed_old_connection_does_not_retire_replacement() {
        let old = CallTiming::new("test");
        let next = CallTiming::new("test");
        let slot = RwLock::new(Some(old.clone()));
        replace_timing_slot(&slot, next.clone());
        assert!(!old.first_decoded_frame());
        retire_timing_slot(&slot, Some(&old));
        assert!(!old.mark("remote_participant_present"));
        assert!(slot.read().as_ref().unwrap().same_attempt(&next));
        assert!(next.first_decoded_frame());
        retire_timing_slot(&slot, Some(&next));
        assert!(slot.read().is_none());
        assert!(!next.mark("call_key_accepted_publish_gate_open"));
    }
}

#[cfg(test)]
mod reconnect_initialization_tests {
    use super::*;
    use crate::crypto::call_key_provider::CallKeyState;

    #[test]
    fn provisional_partial_rejection_rolls_back_native_provider_and_retries() {
        // No SDK Room or sender cryptor fixture is constructed here. Real
        // KeyProvider setters mutate native slots; callback rejection supplies
        // the bounded fault after current selection or before any mutation.
        for failure_epoch in [0, 2] {
            for reject_next in [false, true] {
                for reject_rollback in [false, true] {
                    let original = CallKeyState::new(&[9; 32], "provisional-native").unwrap();
                    let provider = KeyProvider::with_shared_key(
                        KeyProviderOptions {
                            key_ring_size: KEY_RING_SIZE,
                            ratchet_window_size: 0,
                            ..KeyProviderOptions::default()
                        },
                        original.current_key_vec(),
                    );
                    let snapshot = ReconnectKeys {
                        provider: provider.clone(),
                        epoch: original.epoch(),
                        current: zeroize::Zeroizing::new(original.current_key_vec()),
                        next: zeroize::Zeroizing::new(original.peek_next_key().unwrap().to_vec()),
                    };
                    let reference = KeyProvider::with_shared_key(
                        KeyProviderOptions {
                            key_ring_size: KEY_RING_SIZE,
                            ratchet_window_size: 0,
                            ..KeyProviderOptions::default()
                        },
                        original.current_key_vec(),
                    );
                    reference.set_shared_key(snapshot.next.to_vec(), 1);
                    reference.set_shared_key(snapshot.current.to_vec(), 0);
                    let mut candidate = original.clone();
                    let mut failed = false;
                    let mut rollback_seen = false;
                    let result = initialize_provisional_keys(
                        Some(&snapshot),
                        Some((&mut candidate, 3)),
                        |epoch, current, next| {
                            if failed {
                                rollback_seen = true;
                                assert_eq!(epoch, snapshot.epoch);
                                if reject_rollback {
                                    return false;
                                }
                            }
                            if epoch == failure_epoch && !failed {
                                failed = true;
                                if reject_next {
                                    provider.set_shared_key(current, key_ring_index(epoch));
                                    assert_eq!(
                                        provider.get_latest_key_index(),
                                        key_ring_index(epoch)
                                    );
                                }
                                return false;
                            }
                            // set_shared_key selects data metadata as well, so restore
                            // current selection after installing future receive material.
                            provider.set_shared_key(next, key_ring_index(epoch + 1));
                            provider.set_shared_key(current, key_ring_index(epoch));
                            true
                        },
                    );
                    assert!(result.is_err());
                    assert!(rollback_seen);
                    assert_eq!(candidate.epoch(), original.epoch());
                    assert_eq!(candidate.current_key(), original.current_key());
                    assert_eq!(candidate.anchor(), original.anchor());
                    assert_eq!(
                        provider.get_latest_key_index(),
                        key_ring_index(original.epoch())
                    );
                    let current_material = provider
                        .get_shared_key(key_ring_index(original.epoch()))
                        .unwrap();
                    assert_eq!(Some(current_material), reference.get_shared_key(0));
                    if !reject_rollback {
                        assert_eq!(provider.get_shared_key(1), reference.get_shared_key(1));
                    }
                    // Retrying the same production helper reconstructs all elapsed
                    // slots and commits only after every callback succeeds.
                    assert_eq!(
                        initialize_provisional_keys(
                            Some(&snapshot),
                            Some((&mut candidate, 3)),
                            |epoch, current, next| {
                                provider.set_shared_key(next, key_ring_index(epoch + 1));
                                provider.set_shared_key(current, key_ring_index(epoch));
                                true
                            }
                        )
                        .unwrap(),
                        Some(3)
                    );
                    assert_eq!(candidate.epoch(), 3);
                    assert_eq!(provider.get_latest_key_index(), 3);
                    for epoch in 0..=4 {
                        assert!(
                            !provider
                                .get_shared_key(key_ring_index(epoch))
                                .unwrap()
                                .is_empty()
                        );
                    }
                    // Compare derived native material against an independently
                    // reconstructed provider, never reimport native exports.
                    let mut expected = original.clone();
                    for epoch in 0..=4 {
                        reference.set_shared_key(expected.current_key_vec(), key_ring_index(epoch));
                        assert_eq!(
                            provider.get_shared_key(key_ring_index(epoch)),
                            reference.get_shared_key(key_ring_index(epoch))
                        );
                        if epoch < 4 {
                            expected.ratchet().unwrap();
                        }
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn muted_microphone_request_survives_teardown_until_explicit_stop() {
        let (manager, _) = RoomManager::new();
        manager
            .microphone_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
        manager.set_microphone_muted(true);
        assert!(manager.reconnect_microphone_device().is_some());
        assert!(manager.is_microphone_muted());
        manager.disconnect().await;
        assert!(manager.reconnect_microphone_device().is_some());
        manager.stop_microphone();
        assert!(manager.reconnect_microphone_device().is_none());
    }

    #[tokio::test]
    async fn fresh_room_failed_connect_preserves_absolute_epoch_and_native_ring() {
        for epoch in [7, 15, 16, 17] {
            let (manager, _events) = RoomManager::new();
            let mut chain = CallKeyState::new(&[3; 32], "reconnect-boundary").unwrap();
            let provider = KeyProvider::with_shared_key(
                KeyProviderOptions {
                    ratchet_window_size: 0,
                    key_ring_size: KEY_RING_SIZE,
                    ..KeyProviderOptions::default()
                },
                chain.current_key_vec(),
            );
            for _ in 0..epoch {
                chain.ratchet().unwrap();
                provider.set_shared_key(chain.current_key_vec(), key_ring_index(chain.epoch()));
            }
            let before = zeroize::Zeroizing::new(
                (0..KEY_RING_SIZE)
                    .map(|i| provider.get_shared_key(i))
                    .collect::<Vec<_>>(),
            );
            {
                let mut state = manager.e2ee.write();
                state.provider = Some(provider.clone());
                state.epoch = epoch;
            }
            let snapshot = manager.snapshot_reconnect_keys(&chain).unwrap();
            manager.disconnect().await;
            // Exercise the production fresh-room initialization path without
            // a server. A malformed URL fails before any network connection.
            assert!(
                manager
                    .connect_reusing_keys("invalid://", "", snapshot)
                    .await
                    .is_err()
            );
            let before_chain = chain.clone();
            for _ in 0..2 {
                let snapshot = manager.snapshot_reconnect_keys(&chain).unwrap();
                let mut candidate = chain.clone();
                assert!(
                    manager
                        .connect_reusing_keys_aligned(
                            "invalid://",
                            "",
                            snapshot,
                            &mut candidate,
                            Some(chrono::Utc::now() - chrono::Duration::seconds(900)),
                        )
                        .await
                        .is_err()
                );
                assert_eq!(candidate.epoch(), before_chain.epoch());
                assert_eq!(candidate.anchor(), before_chain.anchor());
                assert_eq!(candidate.current_key(), before_chain.current_key());
                // Provider is retained, but a boundary while the room is absent
                // must fail without changing manager authority.
                assert!(!manager.set_ratcheted_shared_key(
                    epoch + 1,
                    chain.peek_next_key().unwrap().to_vec(),
                    vec![0; 32],
                ));
            }
            assert_eq!(manager.key_epoch(), epoch);
            assert_eq!(
                manager
                    .e2ee
                    .read()
                    .provider
                    .as_ref()
                    .unwrap()
                    .get_latest_key_index(),
                key_ring_index(epoch)
            );
            for i in 0..KEY_RING_SIZE {
                let held = zeroize::Zeroizing::new(provider.get_shared_key(i));
                assert!(*held == before[i as usize]);
            }
            // Snapshot staging must reject mismatched authority before teardown.
            chain.ratchet().unwrap();
            assert!(manager.snapshot_reconnect_keys(&chain).is_err());
            assert_eq!(manager.key_epoch(), epoch);
        }
    }
}
