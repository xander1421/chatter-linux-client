// Copyright 2025 LiveKit, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{collections::HashMap, sync::Arc};

use libwebrtc::{
    native::{
        frame_cryptor::{
            DataPacketCryptor, EncryptedPacket, EncryptionAlgorithm, EncryptionState, FrameCryptor,
        },
        packet_trailer,
    },
    rtp_receiver::RtpReceiver,
    rtp_sender::RtpSender,
};
use livekit_protocol::PacketTrailerFeature;
use parking_lot::Mutex;

use super::{key_provider::KeyProvider, EncryptionType};
use crate::{
    e2ee::E2eeOptions,
    id::{ParticipantIdentity, TrackSid},
    participant::{LocalParticipant, RemoteParticipant},
    prelude::{LocalTrack, LocalTrackPublication, RemoteTrack, RemoteTrackPublication},
    rtc_engine::lk_runtime::LkRuntime,
};
use std::fmt::Debug;

type StateChangedHandler = Box<dyn Fn(ParticipantIdentity, EncryptionState) + Send>;

fn has_packet_trailer_features(features: &[i32]) -> bool {
    features.iter().any(|f| {
        *f == PacketTrailerFeature::PtfUserTimestamp as i32
            || *f == PacketTrailerFeature::PtfFrameId as i32
            || *f == PacketTrailerFeature::PtfUserData as i32
    })
}

fn needs_video_receiver_packet_trailer_handler(features: &[i32]) -> bool {
    has_packet_trailer_features(features)
}

struct ManagerInner {
    options: Option<E2eeOptions>, // If Some, it means the e2ee was initialized
    enabled: bool,                // Used to enable/disable e2ee
    dc_encryption_enabled: bool,
    frame_cryptors: HashMap<(ParticipantIdentity, TrackSid), FrameCryptor>,
    data_packet_cryptor: Option<DataPacketCryptor>,
    // Only local RTP senders: receivers choose the index carried by each frame.
    sender_cryptors: HashMap<(ParticipantIdentity, TrackSid), FrameCryptor>,
    sender_key_index: i32,
}

#[derive(Clone)]
pub struct E2eeManager {
    inner: Arc<Mutex<ManagerInner>>,
    state_changed: Arc<Mutex<Option<StateChangedHandler>>>,
}

impl E2eeManager {
    /// E2eeOptions is an optional parameter. We may support to reconfigure e2ee after connect in
    /// the future.
    pub(crate) fn new(options: Option<E2eeOptions>, with_dc_encryption: bool) -> Self {
        // Create DataPacketCryptor whenever E2EE options are available
        // This allows for decryption even if we're not encrypting our own data
        let data_packet_cryptor = options.as_ref().map(|opts| {
            DataPacketCryptor::new(
                EncryptionAlgorithm::AesGcm,
                opts.key_provider.handle.clone(),
            )
        });

        let sender_key_index = options
            .as_ref()
            .map_or(0, |opts| opts.key_provider.get_latest_key_index());
        Self {
            inner: Arc::new(Mutex::new(ManagerInner {
                enabled: options.is_some(), // Enabled by default if options is provided
                dc_encryption_enabled: options.is_some() && with_dc_encryption,
                options,
                frame_cryptors: HashMap::new(),
                data_packet_cryptor,
                sender_cryptors: HashMap::new(),
                sender_key_index,
            })),
            state_changed: Default::default(),
        }
    }

    pub(crate) fn cleanup(&self) {
        let mut inner = self.inner.lock();
        for cryptor in inner.frame_cryptors.values() {
            cryptor.set_enabled(false);
        }
        inner.frame_cryptors.clear();
        inner.sender_cryptors.clear();
    }

    /// Register to e2ee state changes
    /// Used by the room to dispatch the event to the room dispatcher
    pub(crate) fn on_state_changed(
        &self,
        handler: impl Fn(ParticipantIdentity, EncryptionState) + Send + 'static,
    ) {
        *self.state_changed.lock() = Some(Box::new(handler));
    }

    pub(crate) fn initialized(&self) -> bool {
        self.inner.lock().options.is_some()
    }

    pub(crate) fn on_track_subscribed(
        &self,
        track: RemoteTrack,
        publication: RemoteTrackPublication,
        participant: RemoteParticipant,
    ) {
        let identity = participant.identity();
        let receiver = track.transceiver().unwrap().receiver();
        let mut packet_trailer_handler = None;

        let needs_packet_trailer_handler = needs_video_receiver_packet_trailer_handler(
            &publication.proto_info().packet_trailer_features,
        );

        if let RemoteTrack::Video(video_track) = &track {
            if needs_packet_trailer_handler {
                let handler = packet_trailer::create_receiver_handler(
                    LkRuntime::instance().pc_factory(),
                    &receiver,
                );
                video_track.set_packet_trailer_handler(handler.clone());
                packet_trailer_handler = Some(handler);
                log::info!(
                    "attached packet_trailer handler for subscribed track {} from {}",
                    publication.sid(),
                    identity,
                );
            } else {
                log::debug!(
                    "skipping packet_trailer handler for subscribed track {} from {} without advertised packet trailer support",
                    publication.sid(),
                    identity,
                );
            }
        }

        if !self.initialized() || publication.encryption_type() == EncryptionType::None {
            return;
        }

        let frame_cryptor = self.setup_rtp_receiver(&identity, receiver);
        if let Some(handler) = packet_trailer_handler.as_ref() {
            frame_cryptor.set_packet_trailer_handler(handler);
        }
        self.setup_cryptor(&frame_cryptor);

        let mut inner = self.inner.lock();
        inner
            .frame_cryptors
            .insert((identity, publication.sid()), frame_cryptor.clone());
    }

    pub(crate) fn on_local_track_published(
        &self,
        track: LocalTrack,
        publication: LocalTrackPublication,
        participant: LocalParticipant,
    ) {
        let identity = participant.identity();
        let sender = track.transceiver().unwrap().sender();

        let packet_trailer_handler = if let LocalTrack::Video(video_track) = &track {
            let handler = video_track.packet_trailer_handler();
            if handler.is_some() {
                log::info!(
                    "packet_trailer enabled for published track {} from {}",
                    publication.sid(),
                    identity,
                );
            } else {
                log::info!(
                    "packet_trailer not enabled for published track {} from {}",
                    publication.sid(),
                    identity,
                );
            }
            handler
        } else {
            None
        };

        if !self.initialized() || publication.encryption_type() == EncryptionType::None {
            return;
        }

        self.register_rtp_sender(identity, publication.sid(), sender, |cryptor| {
            if let Some(handler) = packet_trailer_handler.as_ref() {
                cryptor.set_packet_trailer_handler(handler);
            }
            self.setup_cryptor(cryptor);
        });
    }

    // The production publication boundary, also exercised with real native RTP
    // senders in the tests below. The lock covers creation through enablement.
    fn register_rtp_sender(
        &self,
        identity: ParticipantIdentity,
        sid: TrackSid,
        sender: RtpSender,
        configure: impl FnOnce(&FrameCryptor),
    ) -> FrameCryptor {
        let mut inner = self.inner.lock();
        let cryptor = self.setup_rtp_sender(&inner, &identity, sender);
        configure(&cryptor);
        inner
            .sender_cryptors
            .insert((identity.clone(), sid.clone()), cryptor.clone());
        inner
            .frame_cryptors
            .insert((identity, sid), cryptor.clone());
        cryptor.set_enabled(inner.enabled);
        log::info!(
            "Media sender key selected: sender_index={}",
            cryptor.key_index()
        );
        cryptor
    }

    fn setup_cryptor(&self, frame_cryptor: &FrameCryptor) {
        let state_changed = self.state_changed.clone();
        frame_cryptor.on_state_change(Some(Box::new(move |participant_identity, state| {
            if let Some(state_changed) = state_changed.lock().as_ref() {
                state_changed(participant_identity.into(), state);
            }
        })));
    }

    /// Called by the room
    pub(crate) fn on_local_track_unpublished(
        &self,
        publication: LocalTrackPublication,
        participant: LocalParticipant,
    ) {
        self.remove_frame_cryptor(participant.identity(), publication.sid());
    }

    /// Called by the room
    pub(crate) fn on_track_unsubscribed(
        &self,
        _: RemoteTrack,
        publication: RemoteTrackPublication,
        participant: RemoteParticipant,
    ) {
        self.remove_frame_cryptor(participant.identity(), publication.sid());
    }

    pub fn frame_cryptors(&self) -> HashMap<(ParticipantIdentity, TrackSid), FrameCryptor> {
        self.inner.lock().frame_cryptors.clone()
    }

    pub fn enabled(&self) -> bool {
        self.inner.lock().enabled && self.initialized()
    }

    pub fn is_dc_encryption_enabled(&self) -> bool {
        self.inner.lock().dc_encryption_enabled
    }

    pub fn set_enabled(&self, enabled: bool) {
        let mut inner = self.inner.lock();
        if inner.enabled == enabled {
            return;
        }

        inner.enabled = enabled;
        for (_, cryptor) in inner.frame_cryptors.iter() {
            cryptor.set_enabled(enabled);
        }
    }

    /// Install an externally managed shared media epoch atomically with sender
    /// selection. `next` is receive-only preinstallation; it never selects the
    /// sending epoch. Existing receiver indices and older ring slots are untouched.
    /// Returns false for absent providers or indices outside the configured ring.
    pub fn install_shared_media_key(
        &self,
        key_index: i32,
        key: Vec<u8>,
        next: Option<(i32, Vec<u8>)>,
    ) -> bool {
        let mut inner = self.inner.lock();
        let Some(options) = inner.options.as_ref() else {
            return false;
        };
        let ring_size = options.key_provider.key_ring_size();
        if key_index < 0
            || key_index >= ring_size
            || next
                .as_ref()
                .is_some_and(|(index, _)| *index < 0 || *index >= ring_size || *index == key_index)
        {
            return false;
        }
        let provider = options.key_provider.clone();
        if !provider.select_shared_media_key(key, key_index) {
            return false;
        }
        inner.sender_key_index = key_index;
        for cryptor in inner.sender_cryptors.values() {
            cryptor.set_key_index(key_index);
        }
        // Select all senders first: on a large adoption jump the future slot
        // may be the OLD sending slot. Preinstallation must not make a sender
        // temporarily encrypt with that future receive key.
        if let Some((index, key)) = next {
            if !provider.set_shared_receive_key(key, index) {
                return false;
            }
        }
        if let Some(cryptor) = inner.sender_cryptors.values().next() {
            log::info!(
                "Media sender key selected: sender_index={}",
                cryptor.key_index()
            );
        }
        true
    }

    pub fn key_provider(&self) -> Option<KeyProvider> {
        let inner = self.inner.lock();
        inner.options.as_ref().map(|opts| opts.key_provider.clone())
    }

    pub fn encryption_type(&self) -> EncryptionType {
        let inner = self.inner.lock();
        inner
            .options
            .as_ref()
            .map(|opts| opts.encryption_type)
            .unwrap_or(EncryptionType::None)
    }

    fn setup_rtp_sender(
        &self,
        inner: &ManagerInner,
        participant_identity: &ParticipantIdentity,
        sender: RtpSender,
    ) -> FrameCryptor {
        let options = inner.options.as_ref().unwrap();

        let frame_cryptor = FrameCryptor::new_for_rtp_sender(
            LkRuntime::instance().pc_factory(),
            participant_identity.to_string(),
            EncryptionAlgorithm::AesGcm,
            options.key_provider.handle.clone(),
            sender,
        );
        // Native cryptors start disabled. Select the current epoch before
        // enabling; provider latest_key_index alone only selects data keys.
        frame_cryptor.set_key_index(inner.sender_key_index);
        frame_cryptor
    }

    fn setup_rtp_receiver(
        &self,
        participant_identity: &ParticipantIdentity,
        receiver: RtpReceiver,
    ) -> FrameCryptor {
        let inner = self.inner.lock();
        let options = inner.options.as_ref().unwrap();

        let frame_cryptor = FrameCryptor::new_for_rtp_receiver(
            LkRuntime::instance().pc_factory(),
            participant_identity.to_string(),
            EncryptionAlgorithm::AesGcm,
            options.key_provider.handle.clone(),
            receiver,
        );
        frame_cryptor.set_enabled(inner.enabled);
        frame_cryptor
    }

    fn remove_frame_cryptor(&self, participant_identity: ParticipantIdentity, track_sid: TrackSid) {
        log::debug!("removing frame cryptor for {}", participant_identity);

        let mut inner = self.inner.lock();
        inner
            .sender_cryptors
            .remove(&(participant_identity.clone(), track_sid.clone()));
        inner
            .frame_cryptors
            .remove(&(participant_identity, track_sid));
    }

    /// Decrypt data received from a data channel
    pub(crate) fn decrypt_data(
        &self,
        data: Vec<u8>,
        iv: Vec<u8>,
        key_index: u32,
        participant_identity: &str,
    ) -> Option<Vec<u8>> {
        let inner = self.inner.lock();

        let data_packet_cryptor = inner.data_packet_cryptor.as_ref()?;

        let encrypted_packet = EncryptedPacket {
            data,
            iv,
            key_index,
        };
        match data_packet_cryptor.decrypt(participant_identity, &encrypted_packet) {
            Ok(decrypted_data) => Some(decrypted_data),
            Err(e) => {
                log::warn!("handle_encrypted_data error: {}", e);
                None
            }
        }
    }

    /// Encrypt data for transmission over a data channel
    pub(crate) fn encrypt_data(
        &self,
        data: Vec<u8>,
        participant_identity: &ParticipantIdentity,
        key_index: u32,
    ) -> Result<EncryptedPacket, Box<dyn std::error::Error>> {
        let inner = self.inner.lock();

        let data_packet_cryptor = inner
            .data_packet_cryptor
            .as_ref()
            .ok_or("DataPacketCryptor is not initialized")?;

        data_packet_cryptor.encrypt(participant_identity.as_str(), key_index, &data)
    }
}

impl Debug for E2eeManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("E2eeManager")
            .field("enabled", &self.inner.lock().enabled)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receiver_packet_trailer_handler_is_skipped_without_features() {
        assert!(!needs_video_receiver_packet_trailer_handler(&[]));
    }

    #[test]
    fn receiver_packet_trailer_handler_is_needed_for_user_timestamp() {
        let features = [PacketTrailerFeature::PtfUserTimestamp as i32];

        assert!(needs_video_receiver_packet_trailer_handler(&features));
    }

    #[test]
    fn receiver_packet_trailer_handler_is_needed_for_frame_id() {
        let features = [PacketTrailerFeature::PtfFrameId as i32];

        assert!(needs_video_receiver_packet_trailer_handler(&features));
    }

    #[test]
    fn receiver_packet_trailer_handler_is_needed_for_user_data() {
        let features = [PacketTrailerFeature::PtfUserData as i32];

        assert!(needs_video_receiver_packet_trailer_handler(&features));
    }
}

// No SFU, network negotiation, microphone or output device is needed: these
// tests exercise the production registration/install path and native cryptor
// SetKeyIndex/key_index APIs on real RTP audio senders.
#[cfg(all(test, not(target_arch = "wasm32")))]
mod media_key_index_tests {
    use super::*;
    use crate::e2ee::key_provider::KeyProviderOptions;
    use libwebrtc::{
        audio_source::{native::NativeAudioSource, AudioSourceOptions},
        media_stream_track::MediaStreamTrack,
        peer_connection::PeerConnection,
        peer_connection_factory::{native::PeerConnectionFactoryExt, RtcConfiguration},
        rtp_transceiver::{RtpTransceiverDirection, RtpTransceiverInit},
    };

    // SDK runtime tasks must outlive all native peer fixtures, as in the app.
    fn test_runtime() -> &'static tokio::runtime::Runtime {
        static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
        RUNTIME.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
        })
    }

    fn manager() -> E2eeManager {
        E2eeManager::new(
            Some(E2eeOptions {
                encryption_type: EncryptionType::Gcm,
                key_provider: KeyProvider::with_shared_key(
                    KeyProviderOptions {
                        ratchet_window_size: 0,
                        key_ring_size: 16,
                        ..Default::default()
                    },
                    vec![1; 32],
                ),
            }),
            false,
        )
    }

    fn sender(runtime: &LkRuntime) -> (PeerConnection, RtpSender) {
        let factory = runtime.pc_factory();
        let pc = factory
            .create_peer_connection(RtcConfiguration::default())
            .unwrap();
        let source = NativeAudioSource::new(AudioSourceOptions::default(), 48000, 1, 50);
        let track = factory.create_audio_track("test", source);
        let transceiver = pc
            .add_transceiver(
                MediaStreamTrack::Audio(track),
                RtpTransceiverInit {
                    direction: RtpTransceiverDirection::SendOnly,
                    stream_ids: vec![],
                    send_encodings: vec![],
                },
            )
            .unwrap();
        (pc, transceiver.sender())
    }

    fn register(
        manager: &E2eeManager,
        sender: RtpSender,
        sid: &str,
        expected: i32,
    ) -> FrameCryptor {
        manager.register_rtp_sender(
            "local".into(),
            sid.to_owned().try_into().unwrap(),
            sender,
            |cryptor| {
                // Assert native selection at the production boundary BEFORE enable.
                assert!(!cryptor.enabled());
                assert_eq!(cryptor.key_index(), expected);
            },
        )
    }

    #[test]
    fn media_epoch_updates_existing_and_later_senders_preserving_receive_ring() {
        let _runtime = test_runtime().enter();
        let runtime = LkRuntime::instance();
        let manager = manager();
        let provider = manager.key_provider().unwrap();
        let (pc, first) = sender(&runtime);
        let first = register(&manager, first, "TR_first", 0);
        for epoch in [1, 7, 15, 16, 17, 32] {
            let index = epoch % 16;
            let next = (epoch + 1) % 16;
            let prior_index = (epoch - 1) % 16;
            let prior = provider.get_shared_key(prior_index);
            assert!(manager.install_shared_media_key(
                index,
                vec![epoch as u8; 32],
                Some((next, vec![(epoch + 1) as u8; 32]))
            ));
            assert_eq!(first.key_index(), index);
            assert_eq!(provider.get_latest_key_index(), index);
            assert_eq!(provider.get_shared_key(index), Some(vec![epoch as u8; 32]));
            assert_eq!(
                provider.get_shared_key(next),
                Some(vec![(epoch + 1) as u8; 32])
            );
            assert_eq!(provider.get_shared_key(prior_index), prior);
            let (later_pc, later) = sender(&runtime);
            let later = register(&manager, later, "TR_later", index);
            assert!(later.enabled());
            assert_eq!(later.key_index(), index);
            manager.remove_frame_cryptor("local".into(), "TR_later".to_owned().try_into().unwrap());
            later_pc.close();
        }
        // A later video/screen sender uses the same production creation path.
        let video_pc = runtime
            .pc_factory()
            .create_peer_connection(RtcConfiguration::default())
            .unwrap();
        let video = runtime.pc_factory().create_video_track(
            "video-test",
            libwebrtc::video_source::native::NativeVideoSource::new(
                libwebrtc::video_source::VideoResolution {
                    width: 16,
                    height: 16,
                },
                false,
            ),
        );
        let video_sender = video_pc
            .add_transceiver(
                MediaStreamTrack::Video(video),
                RtpTransceiverInit {
                    direction: RtpTransceiverDirection::SendOnly,
                    stream_ids: vec![],
                    send_encodings: vec![],
                },
            )
            .unwrap()
            .sender();
        let video = register(&manager, video_sender, "TR_video", 0);
        assert!(manager.install_shared_media_key(4, vec![4; 32], Some((5, vec![5; 32]))));
        assert_eq!(video.key_index(), 4);
        assert_eq!(first.key_index(), 4);
        // Unpublication removes sender enrollment; later rotations do not touch it.
        manager.remove_frame_cryptor("local".into(), "TR_video".to_owned().try_into().unwrap());
        assert!(manager.install_shared_media_key(6, vec![6; 32], Some((7, vec![7; 32]))));
        assert_eq!(video.key_index(), 4);
        video_pc.close();
        // DM re-root resets actual existing native sender selection too.
        assert!(manager.install_shared_media_key(0, vec![99; 32], None));
        assert_eq!(first.key_index(), 0);
        let (later_pc, later) = sender(&runtime);
        register(&manager, later, "TR_reset", 0);
        assert!(!manager.install_shared_media_key(16, vec![2; 32], None));
        assert!(!manager.install_shared_media_key(0, vec![2; 32], Some((0, vec![3; 32]))));
        assert_eq!(provider.get_shared_key(0), Some(vec![99; 32]));
        assert!(manager.install_shared_media_key(7, vec![7; 32], Some((8, vec![8; 32]))));
        manager.cleanup();
        assert!(!first.enabled());
        assert!(manager.inner.lock().sender_cryptors.is_empty());
        // SDK reconnect cleanup retains the selected epoch for republication.
        let (republish_pc, republish) = sender(&runtime);
        register(&manager, republish, "TR_republish", 7);
        manager.cleanup();
        later_pc.close();
        republish_pc.close();
        pc.close();
    }

    #[test]
    fn native_provider_rejection_does_not_select_a_sending_epoch() {
        let _runtime = test_runtime().enter();
        let provider = KeyProvider::new(KeyProviderOptions::default());
        let manager = E2eeManager::new(
            Some(E2eeOptions {
                encryption_type: EncryptionType::Gcm,
                key_provider: provider.clone(),
            }),
            false,
        );
        assert!(!manager.install_shared_media_key(7, vec![7; 32], Some((8, vec![8; 32]))));
        assert_eq!(manager.inner.lock().sender_key_index, 0);
        assert_eq!(provider.get_latest_key_index(), 0);
    }

    #[test]
    fn rotation_cannot_miss_sender_between_creation_and_registration() {
        let _runtime = test_runtime().enter();
        let runtime = LkRuntime::instance();
        let manager = manager();
        assert!(manager.install_shared_media_key(7, vec![7; 32], Some((8, vec![8; 32]))));
        let (pc, rtp_sender) = sender(&runtime);
        let entering = Arc::new(std::sync::Barrier::new(2));
        let rotating = manager.clone();
        let barrier = entering.clone();
        let worker = std::thread::spawn(move || {
            barrier.wait();
            assert!(rotating.install_shared_media_key(8, vec![8; 32], Some((9, vec![9; 32]))));
        });
        let cryptor = manager.register_rtp_sender(
            "local".into(),
            "TR_race".to_owned().try_into().unwrap(),
            rtp_sender,
            |cryptor| {
                assert!(!cryptor.enabled());
                assert_eq!(cryptor.key_index(), 7);
                entering.wait(); // Installer contends with the registration lock.
            },
        );
        worker.join().unwrap();
        assert!(cryptor.enabled());
        assert_eq!(cryptor.key_index(), 8);
        // A newly added receiver is not enrolled as a sender or reindexed.
        let receiver = pc
            .add_transceiver_for_media(
                libwebrtc::MediaType::Audio,
                RtpTransceiverInit {
                    direction: RtpTransceiverDirection::RecvOnly,
                    stream_ids: vec![],
                    send_encodings: vec![],
                },
            )
            .unwrap()
            .receiver();
        let receiver = manager.setup_rtp_receiver(&"remote".into(), receiver);
        let before = receiver.key_index();
        manager.inner.lock().frame_cryptors.insert(
            ("remote".into(), "TR_remote".to_owned().try_into().unwrap()),
            receiver.clone(),
        );
        assert!(manager.install_shared_media_key(9, vec![9; 32], Some((10, vec![10; 32]))));
        assert_eq!(receiver.key_index(), before);
        assert_eq!(cryptor.key_index(), 9);
        manager.cleanup();
        pc.close();
    }
}
