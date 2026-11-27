use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::*;
use crate::storage::mesh_identity_store::{MeshIdentityStore, ROTATION_SECS};
use rand::Rng;

fn radio_gap() -> Duration {
    Duration::from_secs(rand::rng().random_range(45..=120))
}

async fn wait_out_radio_gap(duration: Duration, stopped: impl Fn() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + duration;
    while !stopped() {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return true;
        }
        tokio::time::sleep_until(deadline.min(now + Duration::from_millis(250))).await;
    }
    false
}

pub(in crate::services::mesh) struct RadioRotation {
    pub store: Arc<MeshIdentityStore>,
    pub active: super::super::identity::ActiveMeshIdentity,
    pub build_radio: Arc<dyn Fn() -> Option<Arc<dyn MeshLink>> + Send + Sync>,
    pub wiped: Arc<AtomicBool>,
    pub running: Arc<AtomicBool>,
    pub started_at: u64,
    pub nostr: Arc<super::super::CommandBus<super::super::NostrCommand>>,
}

impl BleRuntime {
    pub(in crate::services::mesh) fn with_rotation(mut self, rotation: RadioRotation) -> Self {
        self.rotation = Some(rotation);
        self
    }

    pub(super) fn identity_wiped(&self) -> bool {
        self.rotation
            .as_ref()
            .is_some_and(|rotation| rotation.wiped.load(Ordering::SeqCst))
    }

    pub(super) fn rotation_stopped(&self) -> bool {
        self.identity_wiped()
            || self
                .rotation
                .as_ref()
                .is_some_and(|rotation| !rotation.running.load(Ordering::SeqCst))
    }

    pub(super) fn rotation_due(&self, now: u64) -> bool {
        self.rotation
            .as_ref()
            .is_some_and(|rotation| now.saturating_sub(rotation.started_at) >= ROTATION_SECS)
    }

    pub(super) async fn start_next_epoch(&mut self) -> bool {
        let now = now_secs();
        let Some(rotation) = self.rotation.as_ref() else {
            return false;
        };
        let next = {
            let _signal_guard = self.signal_lock.lock();
            if self.identity_wiped() {
                Err(crate::services::mesh::MeshError::IdentityWiped)
            } else {
                rotation.store.advance(now, false).map_err(Into::into)
            }
        };
        let next = match next {
            Ok(next) => next,
            Err(error) => {
                self.stop_epoch().await;
                self.degraded(format!("Mesh identity could not rotate: {error}"));
                return false;
            }
        };

        if !self.stop_epoch().await
            || !wait_out_radio_gap(radio_gap(), || self.rotation_stopped()).await
        {
            return false;
        }

        let Some(rotation) = self.rotation.as_mut() else {
            return false;
        };
        let Some(radio) = (rotation.build_radio)() else {
            self.degraded("No Bluetooth radio was available after mesh identity rotation".into());
            return false;
        };
        let _signal_guard = self.signal_lock.lock();
        if rotation.wiped.load(Ordering::SeqCst) || !rotation.running.load(Ordering::SeqCst) {
            return false;
        }
        self.radio = radio;
        self.identity = next.identity;
        super::super::identity::activate_broadcast_epoch(&rotation.active, self.identity.clone());
        self.state
            .begin_beacon_epoch(&self.identity.peer_id(), next.started_at);
        let _ = rotation
            .nostr
            .send(super::super::NostrCommand::RotateIdentity(
                self.identity.clone(),
            ));
        // A manifest or chunk under the next sender would link both epochs
        // through the transfer ID. Transfers may retry within their own epoch.
        self.files_out.clear();
        self.pairing_candidates.clear();
        self.pairing_exchanges.clear();
        self.courier_claims.clear();
        self.beacon_cursor = 0;
        self.published_beacons.clear();
        rotation.started_at = next.started_at;
        let (dial_tx, dial_rx) = tokio::sync::mpsc::unbounded_channel();
        self.dial_tx = dial_tx;
        self.dial_rx = Some(dial_rx);
        self.dial_in_flight = false;
        self.link_last_inbound_ms.clear();
        self.gossip = GossipSyncManager::new(
            &self.identity.peer_id(),
            SyncConfig::default(),
            Some(GossipMessageArchive::new(Arc::new(self.storage.clone()))),
            now,
        );
        true
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::crypto::os_rng;
    use crate::services::mesh::session::SessionManager;

    #[tokio::test(start_paused = true)]
    async fn rotation_leaves_a_random_silent_gap_and_shutdown_interrupts_it() {
        let samples: std::collections::BTreeSet<_> = (0..24).map(|_| radio_gap()).collect();
        assert!(samples.len() > 1);
        let delay = *samples.first().unwrap();
        assert!(delay >= Duration::from_secs(45));
        assert!(*samples.last().unwrap() <= Duration::from_secs(120));
        let started = tokio::time::Instant::now();
        assert!(wait_out_radio_gap(delay, || false).await);
        assert_eq!(started.elapsed(), delay);
        let started = tokio::time::Instant::now();
        assert!(
            !wait_out_radio_gap(delay, || started.elapsed() >= Duration::from_millis(250)).await
        );
        assert_eq!(started.elapsed(), Duration::from_millis(250));
    }

    pub(in crate::services::mesh::ble) fn establish(
        runtime: &BleRuntime,
        remote_identity: &MeshIdentity,
    ) -> SessionManager {
        let peer = remote_identity.peer_id();
        let local = runtime.identity.peer_id();
        let mut remote = SessionManager::new();
        let first = remote.initiate(&local, remote_identity).unwrap();
        let second = runtime
            .state
            .with(|state| {
                state
                    .sessions
                    .on_handshake(&peer, &local, &runtime.identity, &first)
            })
            .unwrap()
            .reply
            .unwrap();
        let third = remote
            .on_handshake(&local, &peer, remote_identity, &second)
            .unwrap()
            .reply
            .unwrap();
        runtime.state.with(|state| {
            let done = state
                .sessions
                .on_handshake(&peer, &local, &runtime.identity, &third)
                .unwrap();
            assert_eq!(done.established, Some(remote_identity.static_public()));
        });
        remote
    }

    #[tokio::test]
    async fn only_the_selected_noise_peer_can_confirm_an_in_person_pairing() {
        use crate::domain::mesh::account_binding::BindingQr;
        use crate::services::mesh::account_identity::{
            PairingOffer, pairing_receipt, pairing_token,
        };

        let (mut runtime, _, state) = super::super::tests::runtime();
        let chosen = MeshIdentity::generate(&mut os_rng());
        let stranger = MeshIdentity::generate(&mut os_rng());
        let local = runtime.identity.peer_id();
        let mut selected_session = establish(&runtime, &chosen);
        let mut stranger_session = establish(&runtime, &stranger);
        let transcript = selected_session.handshake_hash(&local).unwrap();
        let qr = BindingQr {
            account_id: [1; 16],
            device_id: 1,
            signal_identity: [0; 33],
            mesh_static: runtime.identity.static_public(),
            mesh_signing: runtime.identity.signing_public(),
            nonce: [2; 16],
            expires_at: now_secs() + 60,
            signature: [3; 64],
        };
        state.with(|state| {
            state
                .chatter_hello
                .set_local_nonce(&chosen.peer_id(), qr.nonce);
            state
                .chatter_hello
                .accept(&chosen.peer_id(), ChatterHello::new([5; 16]));
            state.pairing_offers.insert(
                chosen.peer_id(),
                PairingOffer {
                    token: pairing_token(&qr),
                    transcript,
                    expires_at: qr.expires_at,
                },
            );
        });
        let packet = |peer: &PeerId, ciphertext| {
            BitchatPacket::new(
                MessageType::NoiseEncrypted,
                peer.to_routing_bytes().unwrap(),
                local.to_routing_bytes(),
                now_millis(),
                ciphertext,
                chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
            )
        };
        let link = LinkId::new("scanner");
        let invalid = pairing_receipt(&stranger, &qr, &transcript);
        let invalid = packet(
            &stranger.peer_id(),
            stranger_session.encrypt(&local, &invalid).unwrap(),
        );
        runtime
            .on_encrypted(&link, &invalid, &stranger.peer_id())
            .await;
        assert!(state.with(|state| state.peers.pinned(&stranger.peer_id()).is_none()));

        let valid = pairing_receipt(&chosen, &qr, &transcript);
        let valid = packet(
            &chosen.peer_id(),
            selected_session.encrypt(&local, &valid).unwrap(),
        );
        runtime.on_encrypted(&link, &valid, &chosen.peer_id()).await;
        assert!(state.with(|state| {
            state
                .peers
                .pinned(&chosen.peer_id())
                .unwrap()
                .pairing
                .is_some()
        }));
        assert!(state.with(|state| state.pairing_offers.is_empty()));
    }

    #[test]
    fn fragment_stream_ids_are_shared_within_a_stream_and_fresh_on_retry() {
        let packet = BitchatPacket::new(
            MessageType::NoiseEncrypted,
            [1; 8],
            Some([2; 8]),
            1,
            vec![0xff; 850],
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        let stream = || {
            let frames = encode_fragment_stream(&packet, 96).unwrap();
            assert!(frames.len() > 1);
            let ids = frames
                .iter()
                .map(|bytes| {
                    let packet = codec::decode(bytes).unwrap();
                    fragment::FragmentHeader::parse(&packet)
                        .unwrap()
                        .0
                        .fragment_id
                })
                .collect::<Vec<_>>();
            assert!(ids.iter().all(|id| *id == ids[0]));
            ids[0]
        };
        assert_ne!(stream(), stream());
    }

    #[tokio::test]
    async fn rotation_has_no_stable_public_identity_fields() {
        let (mut runtime, radio, state) = super::super::tests::runtime();
        let link = LinkId::new("observer");
        state.with(|state| {
            state.links.link_up(link.clone(), LinkRole::Peripheral);
            state.links.set_writable(&link, true);
            state.max_write.insert(link.clone(), 512);
        });
        runtime.announce().await;
        runtime.drain_writes().await;
        runtime.identity = Arc::new(MeshIdentity::generate(&mut os_rng()));
        state.with(|state| state.last_announce_ms = 0);
        runtime.announce().await;
        runtime.drain_writes().await;
        let announces: Vec<_> = radio
            .writes()
            .iter()
            .filter_map(|(_, bytes)| codec::decode(bytes).ok())
            .filter(|packet| packet.message_type() == Some(MessageType::Announce))
            .collect();
        assert_eq!(announces.len(), 2);
        let first = AnnouncementPacket::decode(&announces[0].payload).unwrap();
        let second = AnnouncementPacket::decode(&announces[1].payload).unwrap();
        assert_ne!(announces[0].sender_id, announces[1].sender_id);
        assert_ne!(announces[0].signature, announces[1].signature);
        assert_ne!(first.noise_public_key, second.noise_public_key);
        assert_ne!(first.signing_public_key, second.signing_public_key);
        assert!(
            !announces[1]
                .payload
                .windows(32)
                .any(|field| field == first.noise_public_key || field == first.signing_public_key)
        );
    }
}
