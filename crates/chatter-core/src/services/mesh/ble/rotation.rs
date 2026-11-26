use std::sync::atomic::{AtomicBool, Ordering};

use chatter_mesh_noise::seal::{SEAL_OVERHEAD, open_courier, seal_courier};
use chatter_mesh_noise::sign::{sign_bytes, verify_bytes};

use super::*;
use crate::storage::mesh_identity_store::{MeshIdentityStore, OVERLAP_SECS, ROTATION_SECS};

pub(in crate::services::mesh) struct RadioRotation {
    pub store: Arc<MeshIdentityStore>,
    pub active: Arc<parking_lot::RwLock<Arc<MeshIdentity>>>,
    pub build_radio: Arc<dyn Fn() -> Option<Arc<dyn MeshLink>> + Send + Sync>,
    pub wiped: Arc<AtomicBool>,
    pub running: Arc<AtomicBool>,
    pub started_at: u64,
}

const ROTATION_MAGIC: &[u8; 8] = b"CHTRROT1";
const RECOVERY_MAGIC: &[u8; 8] = b"CHTRRCR1";
const PREKEY_MAGIC: &[u8; 8] = b"CHTRRPK1";
const HEADER_LEN: usize = 112;
const PROOF_LEN: usize = HEADER_LEN + 128;

pub(super) struct RotationProof {
    header: [u8; HEADER_LEN],
    previous_signature: [u8; 64],
    next_signature: [u8; 64],
}

impl RotationProof {
    fn new(
        previous: &MeshIdentity,
        next: &MeshIdentity,
        expires_at: u64,
        transcript: &[u8; 32],
    ) -> Self {
        let mut header = [0; HEADER_LEN];
        header[..8].copy_from_slice(ROTATION_MAGIC);
        header[8..40].copy_from_slice(&previous.static_public());
        header[40..72].copy_from_slice(&next.static_public());
        header[72..104].copy_from_slice(&next.signing_public());
        header[104..].copy_from_slice(&expires_at.to_be_bytes());
        let preimage = [header.as_slice(), transcript].concat();
        Self {
            header,
            previous_signature: sign_bytes(previous, &preimage),
            next_signature: sign_bytes(next, &preimage),
        }
    }

    fn encode(&self) -> Vec<u8> {
        [
            self.header.as_slice(),
            &self.previous_signature,
            &self.next_signature,
        ]
        .concat()
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let bytes = <&[u8; PROOF_LEN]>::try_from(bytes).ok()?;
        Some(Self {
            header: bytes[..HEADER_LEN].try_into().ok()?,
            previous_signature: bytes[HEADER_LEN..HEADER_LEN + 64].try_into().ok()?,
            next_signature: bytes[HEADER_LEN + 64..].try_into().ok()?,
        })
    }

    fn prior_static(&self) -> &[u8; 32] {
        self.header[8..40]
            .try_into()
            .expect("rotation proof layout")
    }

    fn next_static(&self) -> &[u8; 32] {
        self.header[40..72]
            .try_into()
            .expect("rotation proof layout")
    }

    fn next_signing(&self) -> &[u8; 32] {
        self.header[72..104]
            .try_into()
            .expect("rotation proof layout")
    }

    fn expires_at(&self) -> u64 {
        u64::from_be_bytes(
            self.header[104..]
                .try_into()
                .expect("rotation proof layout"),
        )
    }

    fn valid_at(&self, now: u64) -> bool {
        now < self.expires_at()
            && self.expires_at()
                <= now.saturating_add(
                    OVERLAP_SECS + crate::domain::mesh::account_binding::CLOCK_SKEW_SECS,
                )
    }

    fn verify(&self, transcript: &[u8; 32], previous_signing: &[u8; 32]) -> bool {
        let preimage = [self.header.as_slice(), transcript].concat();
        self.prior_static() != self.next_static()
            && previous_signing != self.next_signing()
            && verify_bytes(&self.previous_signature, &preimage, previous_signing)
            && verify_bytes(&self.next_signature, &preimage, self.next_signing())
    }
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

    pub(super) fn restore_retired_identities(&mut self, now: u64) -> bool {
        let Some(rotation) = self.rotation.as_ref() else {
            return true;
        };
        match rotation.store.retired(now) {
            Ok(retired) => {
                self.retired_identities = retired;
                true
            }
            Err(error) => {
                self.degraded(format!("Mesh identity overlap could not be read: {error}"));
                false
            }
        }
    }

    pub(super) fn expire_retired_identities(&mut self, now: u64) -> bool {
        self.pending_rotations
            .retain(|_, proof| proof.valid_at(now));
        self.retired_identities
            .iter()
            .all(|epoch| epoch.expires_at > now)
            || self.restore_retired_identities(now)
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

        rotation.store.schedule_cleanup(next.started_at);
        let contacts = self.state.with(|state| {
            state
                .peers
                .pinned_identities()
                .filter_map(|(peer, pin)| {
                    let noise = state.sessions.remote_static(peer)?;
                    if pin.paired && pin.noise_public_key.as_slice() == noise {
                        Some((peer.clone(), state.sessions.handshake_hash(peer)?))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        });
        for (peer, transcript) in contacts {
            let notice = RotationProof::new(
                &self.identity,
                &next.identity,
                now.saturating_add(OVERLAP_SECS),
                &transcript,
            );
            self.send_private_control(&peer, &notice.encode()).await;
        }
        // Rotation proofs can span several ATT writes. Give the old session
        // time to send those fragments before closing its links.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        while tokio::time::Instant::now() < deadline
            && !self.rotation_stopped()
            && !self.state.with(|state| state.writes.is_empty())
        {
            self.flush_buffered_writes().await;
            tokio::time::sleep(WRITE_PACING_DELAY).await;
        }
        if !self.stop_epoch().await || self.rotation_stopped() {
            return false;
        }

        let Some(rotation) = self.rotation.as_mut() else {
            return false;
        };
        let Some(radio) = (rotation.build_radio)() else {
            self.degraded("No Bluetooth radio was available after mesh identity rotation".into());
            return false;
        };
        self.radio = radio;
        self.identity = next.identity;
        *rotation.active.write() = self.identity.clone();
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

    /// A link may come up after both sides changed addresses. Only in a
    /// local overlap do we open an internal session to test for a contact.
    pub(super) fn can_recover_rotation(&self, peer: &PeerId, now: u64) -> bool {
        self.identity.peer_id() < *peer
            && self
                .retired_identities
                .iter()
                .any(|epoch| epoch.expires_at > now)
            && self
                .state
                .with(|state| state.peers.pinned_identities().any(|(_, pin)| pin.paired))
    }

    /// A previously pinned static key authenticates the contact immediately.
    /// When both ends rotated, seal to each locally paired recipient's last
    /// known static key inside the fresh session, so an unpaired endpoint
    /// learns neither the previous identity nor who can open these proofs.
    pub(super) async fn send_rotation_recovery(&mut self, peer: &PeerId, direct: bool) {
        let now = now_secs();
        if !self
            .retired_identities
            .iter()
            .any(|epoch| epoch.expires_at > now)
        {
            return;
        }
        let session = self.state.with(|state| {
            let remote = state.sessions.remote_static(peer)?;
            let hash = state.sessions.handshake_hash(peer)?;
            let paired = state
                .peers
                .pinned(peer)
                .is_some_and(|pin| pin.paired && pin.noise_public_key.as_slice() == remote);
            Some((hash, paired))
        });
        let Some((transcript, paired)) = session else {
            return;
        };
        let notices: Vec<_> = self
            .retired_identities
            .iter()
            .filter(|epoch| epoch.expires_at > now)
            .map(|epoch| {
                RotationProof::new(
                    &epoch.identity,
                    &self.identity,
                    epoch.expires_at,
                    &transcript,
                )
                .encode()
            })
            .collect();
        if paired {
            for notice in notices {
                self.send_private_control(peer, &notice).await;
            }
        } else if direct {
            let recipients: Vec<[u8; 32]> = self.state.with(|state| {
                state
                    .peers
                    .pinned_identities()
                    .filter(|(_, pin)| pin.paired)
                    .filter_map(|(_, pin)| pin.noise_public_key.as_slice().try_into().ok())
                    .collect()
            });
            if recipients.is_empty() {
                return;
            }
            for notice in notices {
                for recipient in &recipients {
                    if let Ok(sealed) = seal_courier(&notice, recipient, &self.identity) {
                        let frame = [RECOVERY_MAGIC.as_slice(), &sealed].concat();
                        self.send_private_control(peer, &frame).await;
                    }
                }
            }
        } else {
            return;
        }
        // The broadcast bundle may have arrived before the receiver moved its
        // pin. Send another copy after the proof so that sealing can use fresh
        // prekeys as soon as the new route is installed.
        let bundle = self
            .state
            .with(|state| state.prekeys.local.as_ref()?.bundle(&self.identity));
        if let Some(Ok(bytes)) = bundle.map(|bundle| bundle.encode()) {
            let frame = [PREKEY_MAGIC.as_slice(), &bytes].concat();
            self.send_private_control(peer, &frame).await;
        }
    }

    pub(super) fn on_rotation_control(&mut self, claimed: &PeerId, plaintext: &[u8]) -> bool {
        if let Some(bytes) = plaintext.strip_prefix(PREKEY_MAGIC) {
            if let Ok(bundle) = PrekeyBundle::decode(bytes) {
                self.state.with(|state| {
                    let Some(pin) = state.peers.pinned(claimed).filter(|pin| pin.paired) else {
                        return;
                    };
                    let Ok(key) = <&[u8; 32]>::try_from(pin.signing_public_key.as_slice()) else {
                        return;
                    };
                    if state.sessions.remote_static(claimed) == Some(bundle.owner_static)
                        && pin.noise_public_key.as_slice() == bundle.owner_static
                        && bundle.verify(key)
                    {
                        state.prekeys.peers.ingest(bundle, now_millis());
                    }
                });
            }
            return true;
        }
        let recovery;
        let plaintext = if let Some(sealed) = plaintext.strip_prefix(RECOVERY_MAGIC) {
            if sealed.len() != PROOF_LEN + SEAL_OVERHEAD {
                return true;
            }
            let now = now_secs();
            let remote = self
                .state
                .with(|state| state.sessions.remote_static(claimed));
            recovery = std::iter::once(self.identity.as_ref())
                .chain(
                    self.retired_identities
                        .iter()
                        .filter(|epoch| epoch.expires_at > now)
                        .map(|epoch| epoch.identity.as_ref()),
                )
                .find_map(|identity| {
                    let (proof, sender) = open_courier(sealed, identity).ok()?;
                    (Some(sender) == remote).then_some(proof)
                });
            let Some(proof) = recovery.as_deref() else {
                return true;
            };
            proof
        } else {
            plaintext
        };
        if !plaintext.starts_with(ROTATION_MAGIC) {
            return false;
        }
        let Some(proof) = RotationProof::decode(plaintext) else {
            return true;
        };
        let now = now_secs();
        if !proof.valid_at(now) {
            return true;
        }
        let prior = PeerId::from_noise_key(proof.prior_static());
        let next = PeerId::from_noise_key(proof.next_static());
        let valid = self.state.with(|state| {
            let Some(pin) = state.peers.pinned(&prior) else {
                return false;
            };
            let Ok(key) = <&[u8; 32]>::try_from(pin.signing_public_key.as_slice()) else {
                return false;
            };
            let Some(transcript) = state.sessions.handshake_hash(claimed) else {
                return false;
            };
            let Some(remote) = state.sessions.remote_static(claimed) else {
                return false;
            };
            (*claimed == prior && remote == *proof.prior_static()
                || *claimed == next && remote == *proof.next_static())
                && pin.paired
                && pin.noise_public_key.as_slice() == proof.prior_static()
                && proof.verify(&transcript, key)
        });
        if valid {
            if *claimed == next {
                self.commit_rotation(&proof);
            } else if self.pending_rotations.len()
                < chatter_mesh_link::registry::MAX_PINNED_IDENTITIES
            {
                self.pending_rotations.insert(next, proof);
            }
        }
        true
    }

    pub(super) fn finish_pending_rotation(&mut self, peer: &PeerId, remote_static: &[u8; 32]) {
        if let Some(proof) = self.pending_rotations.remove(peer)
            && proof.next_static() == remote_static
            && proof.valid_at(now_secs())
        {
            self.commit_rotation(&proof);
        }
    }

    pub(super) fn pending_rotation_for(&self, peer: &PeerId) -> bool {
        self.pending_rotations
            .get(peer)
            .is_some_and(|proof| proof.valid_at(now_secs()))
    }

    fn commit_rotation(&mut self, proof: &RotationProof) {
        let prior = PeerId::from_noise_key(proof.prior_static());
        let next = PeerId::from_noise_key(proof.next_static());
        let saved = self.state.with(|state| {
            let mut candidate = state.peers.clone();
            if !candidate.advance_pin(
                &prior,
                proof.next_static(),
                proof.next_signing(),
                now_millis(),
            ) {
                return false;
            }
            let Ok(sealed) = self.sealer.seal(&candidate.pins_to_bytes()) else {
                return false;
            };
            if self.storage.save(StorageKey::PeerPins, &sealed).is_err() {
                return false;
            }
            state.peers = candidate;
            state.prekeys.peers.forget(proof.prior_static());
            state.sessions.forget_peer(&prior);
            state.chatter_hello.forget_live(&prior);
            state.dm_handshake_at_ms.remove(&prior);
            state.known_peer_reopen_attempts.remove(&prior);
            if let Some(mut pending) = state.pending_dms.remove(&prior) {
                state
                    .pending_dms
                    .entry(next.clone())
                    .or_default()
                    .append(&mut pending);
            }
            true
        });
        if saved {
            self.gossip.retain_signing_key(&next, proof.next_signing());
            let contact = self.state.with(|state| state.peers.contact(&next).clone());
            let _ = self
                .router
                .send(RouterCommand::PeerAvailable { peer: contact });
            self.emit_peers();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::os_rng;
    use crate::services::mesh::{session::SessionManager, transport::MeshDirectory};
    use chatter_mesh_store::router::CourierDirectory;

    fn establish(runtime: &BleRuntime, remote_identity: &MeshIdentity) -> SessionManager {
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
    async fn a_paired_contact_keeps_its_pin_and_courier_route_across_rotation() {
        let (mut runtime, _, state) = super::super::tests::runtime();
        let (old, next, stranger) = (
            MeshIdentity::generate(&mut os_rng()),
            MeshIdentity::generate(&mut os_rng()),
            MeshIdentity::generate(&mut os_rng()),
        );
        let old_peer = old.peer_id();
        let next_peer = next.peer_id();
        let local = runtime.identity.peer_id();
        state.with(|state| {
            assert!(state.peers.pair_noise_identity(
                &old_peer,
                &old.static_public(),
                &old.signing_public(),
                now_millis(),
            ));
        });
        let directory = MeshDirectory::new(state.clone());
        assert_eq!(directory.noise_key(&old_peer), Some(old.static_public()));

        let mut old_session = establish(&runtime, &old);
        let notice = RotationProof::new(
            &old,
            &next,
            now_secs() + OVERLAP_SECS,
            &old_session.handshake_hash(&local).unwrap(),
        )
        .encode();
        let mut stranger_session = establish(&runtime, &stranger);
        let packet = |sender: &PeerId, ciphertext| {
            BitchatPacket::new(
                MessageType::NoiseEncrypted,
                sender.to_routing_bytes().unwrap(),
                local.to_routing_bytes(),
                now_millis(),
                ciphertext,
                chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
            )
        };
        let link = LinkId::new("paired");
        let forged = packet(
            &stranger.peer_id(),
            stranger_session.encrypt(&local, &notice).unwrap(),
        );
        runtime
            .on_encrypted(&link, &forged, &stranger.peer_id())
            .await;
        assert!(!runtime.pending_rotation_for(&next_peer));
        let update = packet(&old_peer, old_session.encrypt(&local, &notice).unwrap());
        runtime.on_encrypted(&link, &update, &old_peer).await;
        assert!(runtime.pending_rotation_for(&next_peer));
        assert_eq!(directory.noise_key(&old_peer), Some(old.static_public()));

        establish(&runtime, &next);
        runtime.finish_pending_rotation(&next_peer, &next.static_public());
        assert_eq!(state.route(&old_peer), next_peer);
        assert_eq!(state.contact(&next_peer), old_peer);
        let key = directory.noise_key(&old_peer).unwrap();
        assert_eq!(key, next.static_public());
        let sealed = seal_courier(b"courier", &key, &runtime.identity).unwrap();
        assert!(open_courier(&sealed, &old).is_err());
        assert_eq!(open_courier(&sealed, &next).unwrap().0, b"courier");

        state.with(|state| state.peers.clear_pins());
        runtime.restore_pins().unwrap();
        assert_eq!(state.route(&old_peer), next_peer);
        assert_eq!(directory.noise_key(&old_peer), Some(next.static_public()));
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
        assert!(state.with(|state| state.peers.pinned(&chosen.peer_id()).unwrap().paired));
        assert!(state.with(|state| state.pairing_offers.is_empty()));
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

    #[test]
    fn recovery_is_bound_to_the_fresh_session_and_the_paired_recipient() {
        let (mut runtime, _, state) = super::super::tests::runtime();
        let old = MeshIdentity::generate(&mut os_rng());
        let next = MeshIdentity::generate(&mut os_rng());
        let receiver_old = MeshIdentity::generate(&mut os_rng());
        let stranger = MeshIdentity::generate(&mut os_rng());
        state.with(|state| {
            assert!(state.peers.pair_noise_identity(
                &old.peer_id(),
                &old.static_public(),
                &old.signing_public(),
                now_millis(),
            ));
        });
        runtime
            .retired_identities
            .push(crate::storage::mesh_identity_store::RetiredMeshEpoch {
                identity: Arc::new(receiver_old.clone()),
                expires_at: now_secs() + OVERLAP_SECS,
            });
        let session = establish(&runtime, &next);
        let transcript = session.handshake_hash(&runtime.identity.peer_id()).unwrap();
        let notice =
            RotationProof::new(&old, &next, now_secs() + OVERLAP_SECS, &transcript).encode();
        let encrypted = seal_courier(&notice, &receiver_old.static_public(), &next).unwrap();
        assert!(open_courier(&encrypted, &stranger).is_err());
        let packet = [RECOVERY_MAGIC.as_slice(), &encrypted].concat();
        assert!(runtime.on_rotation_control(&next.peer_id(), &packet));
        assert_eq!(state.route(&old.peer_id()), next.peer_id());
    }
}
