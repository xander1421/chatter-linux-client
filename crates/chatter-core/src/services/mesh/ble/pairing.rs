use super::*;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use chatter_mesh_link::registry::PairingKey;
use chatter_mesh_noise::seal::{open_courier, seal_courier};
use chatter_mesh_noise::sign::{sign_bytes, verify_bytes};
use subtle::ConstantTimeEq;

use super::super::pairing::{group_identity_key, route_proof};

const ROUTE_MAGIC: &[u8; 8] = b"CHTRPRT3";
const GROUP_MAGIC: &[u8; 8] = b"CHTRPGI1";
const PULL_MAGIC: &[u8; 8] = b"CHTRPLQ1";
const CANDIDATE_MS: u64 = 60_000;
const GROUP_NONCE_LEN: usize = 24;
const GROUP_PLAINTEXT_LEN: usize = 128;
type SealedGroupIdentity = [u8; GROUP_NONCE_LEN + GROUP_PLAINTEXT_LEN + 16];

#[derive(Default)]
pub(super) struct PairingExchange {
    transcript: [u8; 32],
    proof_sent: bool,
    proof_seen: bool,
    verified: bool,
    group_sent: bool,
    group_received: bool,
    pending_group: Option<Box<SealedGroupIdentity>>,
}

fn sign_route(identity: &MeshIdentity, key: &PairingKey, transcript: &[u8; 32]) -> Vec<u8> {
    let signing = identity.signing_public();
    let proof = route_proof(key, transcript, &identity.static_public(), &signing, true);
    let signed = [ROUTE_MAGIC.as_slice(), transcript, &proof].concat();
    let signature = sign_bytes(identity, &signed);
    [ROUTE_MAGIC.as_slice(), &signing, &proof, &signature].concat()
}

fn seal_group_identity(
    group: &MeshIdentity,
    key: &PairingKey,
    transcript: &[u8; 32],
) -> Option<Vec<u8>> {
    let noise = group.static_public();
    let signing = group.signing_public();
    let aad = [GROUP_MAGIC.as_slice(), transcript].concat();
    let certificate = sign_bytes(group, &[aad.as_slice(), &noise, &signing].concat());
    let plaintext = [&noise[..], &signing, &certificate].concat();
    let key = group_identity_key(key, transcript, true);
    let cipher = XChaCha20Poly1305::new_from_slice(&key[..]).ok()?;
    let mut nonce = [0; GROUP_NONCE_LEN];
    getrandom::fill(&mut nonce).ok()?;
    let ciphertext = cipher
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: &plaintext,
                aad: &aad,
            },
        )
        .ok()?;
    Some([GROUP_MAGIC.as_slice(), &nonce, &ciphertext].concat())
}

fn open_group_identity(
    sealed: &SealedGroupIdentity,
    key: &PairingKey,
    transcript: &[u8; 32],
) -> Option<([u8; 32], [u8; 32])> {
    let (nonce, ciphertext) = sealed.split_first_chunk::<GROUP_NONCE_LEN>()?;
    let key = group_identity_key(key, transcript, false);
    let cipher = XChaCha20Poly1305::new_from_slice(&key[..]).ok()?;
    let aad = [GROUP_MAGIC.as_slice(), transcript].concat();
    let plaintext = cipher
        .decrypt(
            &XNonce::from(*nonce),
            Payload {
                msg: ciphertext,
                aad: &aad,
            },
        )
        .ok()?;
    let plaintext = <[u8; GROUP_PLAINTEXT_LEN]>::try_from(plaintext).ok()?;
    let noise: [u8; 32] = plaintext[..32].try_into().ok()?;
    let signing: [u8; 32] = plaintext[32..64].try_into().ok()?;
    let signature = plaintext[64..].try_into().ok()?;
    verify_bytes(
        &signature,
        &[aad.as_slice(), &noise, &signing].concat(),
        &signing,
    )
    .then_some((noise, signing))
}

impl BleRuntime {
    pub(super) fn note_pairing_candidate(&mut self, wire: &PeerId, contact: &PeerId, now: u64) {
        self.pairing_candidates
            .retain(|_, (_, at)| now.saturating_sub(*at) < CANDIDATE_MS);
        if self.pairing_candidates.len() < chatter_mesh_link::registry::MAX_PINNED_IDENTITIES {
            self.pairing_candidates
                .insert(wire.clone(), (contact.clone(), now));
        }
    }

    fn exchange(&mut self, peer: &PeerId, transcript: [u8; 32]) -> &mut PairingExchange {
        let exchange = self.pairing_exchanges.entry(peer.clone()).or_default();
        if exchange.transcript != transcript {
            *exchange = PairingExchange {
                transcript,
                ..PairingExchange::default()
            };
        }
        exchange
    }

    fn pairing_transcript(&self, peer: &PeerId) -> Option<[u8; 32]> {
        self.state.with(|state| state.sessions.handshake_hash(peer))
    }

    pub(super) fn pairing_verified(&self, peer: &PeerId) -> bool {
        self.pairing_exchanges.get(peer).is_some_and(|exchange| {
            exchange.verified && self.pairing_transcript(peer) == Some(exchange.transcript)
        })
    }

    pub(super) fn pairing_verified_on_link(&self, link: &LinkId, peer: &PeerId) -> bool {
        self.pairing_verified(peer)
            && self
                .state
                .with(|state| state.links.is_noise_authenticated(link, peer))
    }

    pub(super) fn group_traffic_allowed(&self, link: &LinkId) -> bool {
        let peer = self.state.with(|state| state.links.peer_for(link).cloned());
        peer.is_some_and(|peer| self.pairing_verified_on_link(link, &peer))
    }

    pub(super) async fn send_pairing_proof(&mut self, peer: &PeerId) {
        let contact = self
            .pairing_candidates
            .get(peer)
            .filter(|(_, at)| now_millis().saturating_sub(*at) < CANDIDATE_MS)
            .map(|(contact, _)| contact)
            .unwrap_or(peer);
        let Some(key) = self.state.pairing_key(contact) else {
            return;
        };
        let Some(transcript) = self.pairing_transcript(peer) else {
            return;
        };
        if self.exchange(peer, transcript).proof_sent {
            return;
        }
        let frame = sign_route(&self.identity, &key, &transcript);
        if self.send_private_control(peer, &frame).await {
            self.exchange(peer, transcript).proof_sent = true;
        }
    }

    async fn send_group_identity(&mut self, peer: &PeerId) {
        if !self.pairing_verified(peer) {
            return;
        }
        let exchange = &self.pairing_exchanges[peer];
        if exchange.group_sent {
            return;
        }
        let transcript = exchange.transcript;
        let Some(key) = self.state.pairing_key(peer) else {
            return;
        };
        let Some(frame) = seal_group_identity(&self.private_group_identity, &key, &transcript)
        else {
            return;
        };
        if self.send_private_control(peer, &frame).await {
            self.exchange(peer, transcript).group_sent = true;
        }
    }

    pub(super) async fn on_pairing_control(&mut self, peer: &PeerId, plaintext: &[u8]) -> bool {
        if let Some(pull) = plaintext.strip_prefix(PULL_MAGIC) {
            self.on_courier_pull(peer, pull).await;
            return true;
        }
        if let Some(content) = plaintext.strip_prefix(GROUP_MAGIC) {
            if let Ok(sealed) = <&SealedGroupIdentity>::try_from(content) {
                self.on_group_identity(peer, sealed).await;
            }
            return true;
        }
        let Some(content) = plaintext.strip_prefix(ROUTE_MAGIC) else {
            return false;
        };
        let Ok(content) = <&[u8; 128]>::try_from(content) else {
            return true;
        };
        let signing: &[u8; 32] = content[..32].try_into().expect("route frame layout");
        let proof: &[u8; 32] = content[32..64].try_into().expect("route frame layout");
        let signature: &[u8; 64] = content[64..].try_into().expect("route frame layout");
        let Some(transcript) = self.pairing_transcript(peer) else {
            return true;
        };
        let exchange = self.exchange(peer, transcript);
        if exchange.proof_seen {
            return true;
        }
        exchange.proof_seen = true;
        let pending = exchange.pending_group.take();
        let accepted = self.state.with(|state| {
            let noise = state.sessions.remote_static(peer)?;
            let signed = [ROUTE_MAGIC.as_slice(), &transcript, proof].concat();
            if PeerId::from_noise_key(&noise) != *peer || !verify_bytes(signature, &signed, signing)
            {
                return None;
            }
            let mut matches = state.peers.pinned_identities().filter_map(|(prior, pin)| {
                let key = pin.pairing.as_ref()?;
                let expected = route_proof(key, &transcript, &noise, signing, false);
                bool::from(expected.ct_eq(proof)).then_some((prior.clone(), pin.contact.clone()))
            });
            let (prior, contact) = matches.next()?;
            if matches.next().is_some() {
                return None;
            }
            drop(matches);
            if prior == *peer {
                let pin = state.peers.pinned(peer)?;
                return (pin.noise_public_key.as_slice() == noise
                    && pin.signing_public_key.as_slice() == signing)
                    .then_some((contact, false));
            }
            let mut candidate = state.peers.clone();
            if !candidate.bind_paired_route(&prior, &noise, signing, now_millis()) {
                return None;
            }
            let sealed = self.sealer.seal(&candidate.pins_to_bytes()).ok()?;
            self.storage.save(StorageKey::PeerPins, &sealed).ok()?;
            state.peers = candidate;
            state.sessions.forget_peer(&prior);
            state.chatter_hello.forget_live(&prior);
            state.dm_handshake_at_ms.remove(&prior);
            state.known_peer_reopen_attempts.remove(&prior);
            if let Some(mut pending) = state.pending_dms.remove(&prior) {
                state
                    .pending_dms
                    .entry(peer.clone())
                    .or_default()
                    .append(&mut pending);
            }
            Some((contact, true))
        });
        if let Some((contact, moved)) = accepted {
            self.exchange(peer, transcript).verified = true;
            self.gossip.retain_signing_key(peer, signing);
            self.pairing_candidates.remove(peer);
            self.send_pairing_proof(peer).await;
            self.send_group_identity(peer).await;
            if let Some(sealed) = pending {
                self.on_group_identity(peer, &sealed).await;
            }
            self.request_pairing_mail(peer).await;
            if moved {
                let _ = self
                    .router
                    .send(RouterCommand::PeerAvailable { peer: contact });
                self.flush_pending_dms(peer).await;
                self.emit_peers();
            }
        }
        true
    }

    async fn on_group_identity(&mut self, peer: &PeerId, sealed: &SealedGroupIdentity) {
        let Some(transcript) = self.pairing_transcript(peer) else {
            return;
        };
        let Some(exchange) = self.pairing_exchanges.get_mut(peer) else {
            return;
        };
        if exchange.transcript != transcript || exchange.group_received {
            return;
        }
        if !exchange.verified {
            // Directed Noise packets can be reordered across links. Save at
            // most one opaque frame while the corresponding proof is pending.
            if exchange.proof_sent && !exchange.proof_seen && exchange.pending_group.is_none() {
                exchange.pending_group = Some(Box::new(*sealed));
            }
            return;
        }
        let Some(key) = self.state.pairing_key(peer) else {
            return;
        };
        let Some(keys) = open_group_identity(sealed, &key, &transcript) else {
            return;
        };
        let saved = self.state.with(|state| {
            if state
                .peers
                .pinned(peer)
                .is_some_and(|pin| pin.private_group_keys == Some(keys))
            {
                return Some(());
            }
            let mut candidate = state.peers.clone();
            if !candidate.bind_private_group_keys(peer, keys) {
                return None;
            }
            let sealed = self.sealer.seal(&candidate.pins_to_bytes()).ok()?;
            self.storage.save(StorageKey::PeerPins, &sealed).ok()?;
            state.peers = candidate;
            Some(())
        });
        if saved.is_some() {
            self.exchange(peer, transcript).group_received = true;
            self.send_group_identity(peer).await;
            self.sync_private_groups(peer).await;
        }
    }

    // A courier never has to learn a stable address in a public announce.
    // The nested Noise X seal proves possession of the requested address and
    // binds it to this authenticated link before any offer or receipt is used.
    async fn request_pairing_mail(&mut self, peer: &PeerId) {
        if !self.state.is_connected(peer) {
            return;
        }
        let Some((remote, transcript)) = self.state.with(|state| {
            Some((
                state.sessions.remote_static(peer)?,
                state.sessions.handshake_hash(peer)?,
            ))
        }) else {
            return;
        };
        let payload = [PULL_MAGIC.as_slice(), &transcript].concat();
        for identity in self.state.local_courier_keys() {
            if let Ok(sealed) = seal_courier(&payload, &remote, &identity) {
                let frame = [PULL_MAGIC.as_slice(), &sealed].concat();
                self.send_private_control(peer, &frame).await;
            }
        }
    }

    async fn on_courier_pull(&mut self, peer: &PeerId, pull: &[u8]) {
        if pull.len() != PULL_MAGIC.len() + 32 + chatter_mesh_noise::seal::SEAL_OVERHEAD
            || !self.state.is_connected(peer)
            || !self.pairing_verified(peer)
        {
            return;
        }
        let Some(transcript) = self.state.with(|state| state.sessions.handshake_hash(peer)) else {
            return;
        };
        let Ok((proof, key)) = open_courier(pull, &self.identity) else {
            return;
        };
        if proof != [PULL_MAGIC.as_slice(), &transcript].concat() {
            return;
        }
        let claims = self.courier_claims.entry(peer.clone()).or_default();
        if claims.len() >= chatter_mesh_link::registry::MAX_PINNED_IDENTITIES || !claims.insert(key)
        {
            return;
        }
        let now = now_secs();
        if let Err(error) = self.couriers.resolve_receipt_support(&key, true, now) {
            tracing::warn!(target: "mesh::ble", %error, "could not prepare paired courier handover");
            return;
        }
        self.hand_over_courier_mail(
            peer,
            &key,
            now,
            AnnounceIngress::verified_direct_on_authenticated_link(),
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::mesh::{
        account_identity::store_paired_pin, pairing::establish_key, timeline::temp_identity_store,
        transport::MeshDirectory,
    };
    use chatter_mesh::tlv::courier_envelope::{courier_receipt_id, epoch_day, recipient_tag};
    use chatter_mesh_store::router::CourierDirectory;

    fn pair(receiver: &BleRuntime, sender: &MeshIdentity, seed: u8, issuer: bool) {
        let key = establish_key(&[seed; 32], &[2; 32], &[3; 16], &[4; 16], issuer);
        receiver.state.with(|state| {
            assert!(store_paired_pin(
                state,
                receiver.storage.as_ref(),
                receiver.sealer.as_ref(),
                &sender.peer_id(),
                &sender.static_public(),
                &sender.signing_public(),
                key,
            ));
        });
    }

    fn bind_link(state: &MeshState, link: &LinkId, peer: &PeerId) {
        state.with(|state| {
            state.links.link_up(link.clone(), LinkRole::Central);
            state.links.set_writable(link, true);
            state.links.note_discovery_peer(link, peer);
            assert!(state.links.mark_noise_authenticated(link, peer));
            state.max_write.insert(link.clone(), 512);
        });
    }

    fn wrap_private_control(
        sender: &BleRuntime,
        recipient: &PeerId,
        plaintext: &[u8],
    ) -> BitchatPacket {
        let ciphertext = sender
            .state
            .with(|state| state.sessions.encrypt(recipient, plaintext))
            .unwrap();
        sender.build_packet(
            MessageType::NoiseEncrypted,
            Some(recipient),
            ciphertext,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        )
    }

    #[tokio::test]
    async fn a_noise_endpoint_with_only_admission_receives_no_stable_group_field() {
        let (mut runtime, radio, state) = super::super::tests::runtime();
        runtime.private_group_identity =
            Arc::new(MeshIdentity::generate(&mut crate::crypto::os_rng()));
        let contact = MeshIdentity::generate(&mut crate::crypto::os_rng());
        pair(&runtime, &contact, 1, true);
        let endpoint = MeshIdentity::generate(&mut crate::crypto::os_rng());
        let wire = endpoint.peer_id();
        let local = runtime.identity.peer_id();
        let link = LinkId::new("unpaired-noise");
        // Admission already selected a contact, but the Noise endpoint on the
        // new route has never completed the in-person pairing.
        runtime.note_pairing_candidate(&wire, &contact.peer_id(), now_millis());
        let mut session = super::super::rotation::tests::establish(&runtime, &endpoint);
        bind_link(&state, &link, &wire);
        let transcript = session.handshake_hash(&local).unwrap();
        runtime.send_pairing_proof(&wire).await;
        runtime.send_group_identity(&wire).await;
        runtime.drain_writes().await;

        let writes = radio.writes();
        assert_eq!(writes.len(), 1);
        let packet = codec::decode(&writes[0].1).unwrap();
        assert_eq!(packet.message_type(), Some(MessageType::NoiseEncrypted));
        let opening = session.decrypt(&local, &packet.payload).unwrap();
        let key = state.pairing_key(&contact.peer_id()).unwrap();
        assert_eq!(opening, sign_route(&runtime.identity, &key, &transcript));
        for stable in [
            runtime.private_group_identity.static_public(),
            runtime.private_group_identity.signing_public(),
        ] {
            assert!(!opening.windows(stable.len()).any(|field| field == stable));
        }

        let unrelated = PairingKey::new([9; 32], false);
        let proof = sign_route(&endpoint, &unrelated, &transcript);
        let packet = BitchatPacket::new(
            MessageType::NoiseEncrypted,
            wire.to_routing_bytes().unwrap(),
            Some(local.to_routing_bytes().unwrap()),
            now_millis(),
            session.encrypt(&local, &proof).unwrap(),
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        runtime.on_encrypted(&link, &packet, &wire).await;
        runtime.drain_writes().await;
        assert!(!runtime.pairing_verified(&wire));
        assert_eq!(radio.writes(), writes);
    }

    #[test]
    fn group_credentials_require_the_pairing_key_direction_and_current_transcript() {
        let group = MeshIdentity::generate(&mut crate::crypto::os_rng());
        let sender = PairingKey::new([1; 32], true);
        let receiver = PairingKey::new([1; 32], false);
        let stranger = PairingKey::new([2; 32], false);
        let transcript = [3; 32];
        let wrapped = seal_group_identity(&group, &sender, &transcript).unwrap();
        let sealed = wrapped
            .strip_prefix(GROUP_MAGIC)
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(
            open_group_identity(sealed, &receiver, &transcript),
            Some((group.static_public(), group.signing_public()))
        );
        assert!(open_group_identity(sealed, &stranger, &transcript).is_none());
        assert!(open_group_identity(sealed, &sender, &transcript).is_none());
        assert!(open_group_identity(sealed, &receiver, &[4; 32]).is_none());
    }

    #[tokio::test]
    async fn paired_contacts_reconnect_and_open_courier_mail_after_a_week_offline() {
        let (mut a, a_radio, a_state) = super::super::tests::runtime();
        let (mut b, _, b_state) = super::super::tests::runtime();
        a.config.set_public_chat(false);
        b.config.set_public_chat(false);
        let store_a = temp_identity_store();
        let store_b = temp_identity_store();
        let start = 1_800_000_000;
        a.identity = store_a.advance(start, true).unwrap().identity;
        b.identity = store_b.advance(start, true).unwrap().identity;
        let contact_a = a.identity.peer_id();
        let contact_b = b.identity.peer_id();
        pair(&a, &b.identity, 1, false);
        pair(&b, &a.identity, 1, true);
        let directory = MeshDirectory::new(a_state.clone());
        let prior_address = directory.noise_key(&contact_b).unwrap();

        // Neither device receives an announce or a transition in the meantime.
        for day in 1..=7 {
            a.identity = store_a
                .advance(start + day * 86_400, day == 7)
                .unwrap()
                .identity;
            b.identity = store_b
                .advance(start + day * 86_400, day == 7)
                .unwrap()
                .identity;
        }
        // The only data the restarted recipient needs for continuity is its
        // persisted QR pin, independent of the lifetime of either broadcast key.
        b_state.with(|state| {
            state.peers.clear_pins();
            state.pairing_couriers = Default::default();
        });
        b.restore_pins().unwrap();

        let wire_a = a.identity.peer_id();
        let wire_b = b.identity.peer_id();
        assert_ne!(contact_a, wire_a);
        assert_ne!(contact_b, wire_b);
        let mut announce = AnnouncementPacket::new(
            "new epoch".into(),
            b.identity.static_public().to_vec(),
            b.identity.signing_public().to_vec(),
            None,
            None,
            None,
        );
        announce.pairing_beacon = b_state
            .pairing_beacons(&wire_b)
            .into_iter()
            .find(|slot| a_state.paired_contact_for_beacon(slot) == Some(contact_b.clone()));
        assert_eq!(
            a_state.paired_contact_for_announce(&announce),
            Some(contact_b.clone())
        );

        let b_session = super::super::rotation::tests::establish(&a, &b.identity);
        b_state.with(|state| state.sessions = b_session);
        let link_a = LinkId::new("paired-a");
        let link_b = LinkId::new("paired-b");
        bind_link(&a_state, &link_a, &wire_b);
        bind_link(&b_state, &link_b, &wire_a);
        for (runtime, state, link, contact) in [
            (&mut a, &a_state, &link_a, &contact_b),
            (&mut b, &b_state, &link_b, &contact_a),
        ] {
            runtime.link_admission.insert(
                link.clone(),
                admission::LinkAdmission::new(Some(contact.clone()), Instant::now()),
            );
            state.with(|state| state.awaiting_pairing_proof.insert(link.clone()));
            assert!(!state.is_paired_link(link));
            assert!(!state.can_write_to(contact));
        }
        let transcript = a_state
            .with(|state| state.sessions.handshake_hash(&wire_b))
            .unwrap();
        let key_a = a_state.pairing_key(&contact_b).unwrap();
        let key_b = b_state.pairing_key(&contact_a).unwrap();
        let frame_a =
            wrap_private_control(&a, &wire_b, &sign_route(&a.identity, &key_a, &transcript));
        let frame_b =
            wrap_private_control(&b, &wire_a, &sign_route(&b.identity, &key_b, &transcript));
        b.note_pairing_candidate(&wire_a, &contact_a, now_millis());
        b.send_pairing_proof(&wire_a).await;
        let sealed = seal_group_identity(&a.private_group_identity, &key_a, &transcript).unwrap();
        let group_frame = wrap_private_control(&a, &wire_b, &sealed);
        b.on_encrypted(&link_b, &group_frame, &wire_a).await;
        assert!(b_state.private_group_keys(&contact_a).is_none());
        a.on_encrypted(&link_a, &frame_b, &wire_b).await;
        b.on_encrypted(&link_b, &frame_a, &wire_a).await;
        assert_eq!(a_state.route(&contact_b), wire_b);
        assert_eq!(
            b_state.private_group_keys(&wire_a),
            Some((
                a.private_group_identity.static_public(),
                a.private_group_identity.signing_public()
            ))
        );
        assert_eq!(b_state.route(&contact_a), wire_a);
        assert!(a_state.is_paired_link(&link_a));
        assert!(b_state.is_paired_link(&link_b));
        assert_eq!(directory.noise_key(&contact_b), Some(prior_address));

        let now = now_secs();
        let (ciphertext, prekey) = super::super::super::prekeys::seal_offline(
            &a_state,
            b"for my paired contact",
            &prior_address,
            &a.identity,
            now * 1_000,
        )
        .unwrap();
        let envelope = CourierEnvelope::new(
            recipient_tag(&prior_address, epoch_day(now)),
            (now + 1_000) * 1_000,
            ciphertext,
            1,
            prekey,
        );
        assert_eq!(
            b_state.open_paired_mail(&envelope, now),
            Some((b"for my paired contact".to_vec(), contact_a.clone()))
        );

        a.couriers
            .deposit(&envelope, &a.identity.static_public(), now)
            .unwrap();
        let payload = [PULL_MAGIC.as_slice(), &transcript].concat();
        let proof = seal_courier(
            &payload,
            &a.identity.static_public(),
            &b_state.pairing_courier(&contact_a).unwrap().local,
        )
        .unwrap();
        let pull = [PULL_MAGIC.as_slice(), &proof].concat();
        let pull = wrap_private_control(&b, &wire_a, &pull);
        a.on_encrypted(&link_a, &pull, &wire_b).await;
        a.drain_writes().await;
        let mut delivered = None;
        for (_, bytes) in a_radio.writes() {
            let packet = codec::decode(&bytes).unwrap();
            assert_ne!(packet.message_type(), Some(MessageType::CourierEnvelope));
            if packet.message_type() == Some(MessageType::NoiseEncrypted) {
                let plain = b_state
                    .with(|state| state.sessions.decrypt(&wire_a, &packet.payload))
                    .unwrap();
                if let Some(bytes) =
                    plain.strip_prefix(super::super::secure_ingress::PAIRED_COURIER_MAGIC)
                {
                    delivered = Some(CourierEnvelope::decode(bytes).unwrap());
                }
            }
        }
        assert_eq!(delivered, Some(envelope.clone()));
        let id = courier_receipt_id(&envelope.ciphertext);
        let ack = [
            super::super::secure_ingress::COURIER_RECEIPT_MAGIC.as_slice(),
            &id,
        ]
        .concat();
        let ack = wrap_private_control(&b, &wire_a, &ack);
        a.on_encrypted(&link_a, &ack, &wire_b).await;
        assert!(!a.couriers.carries_for(&prior_address, now));

        // A buddy can receive the opaque packet before the new epoch key. Its
        // original timestamp still routes the packet once the key arrives.
        let mut group =
            PrivateChannel::create("retained history", &a.private_group_identity, "a").unwrap();
        let enrolled = super::super::super::private_channel::Member {
            peer: b.private_group_identity.peer_id().to_string(),
            fingerprint: b.private_group_identity.fingerprint(),
            signing_key: b.private_group_identity.signing_public().to_vec(),
            nickname: "b".into(),
        };
        group
            .rotate(&a.private_group_identity, Some(enrolled), None)
            .unwrap();
        b.private_channels.save(&group).unwrap();
        group.rotate(&a.private_group_identity, None, None).unwrap();
        let timestamp_ms = now_millis().saturating_sub(120_000);
        let (id, payload) = group
            .encrypt(
                &a.private_group_identity,
                "before the epoch update",
                "a",
                timestamp_ms,
            )
            .unwrap();
        let mut packet = a.build_packet(MessageType::GroupMessage, None, payload, 7);
        packet.timestamp = timestamp_ms;
        assert!(b.on_private_channel_message(&packet, &wire_a));
        assert!(
            b.private_channels
                .load(&group.id)
                .unwrap()
                .unwrap()
                .replay
                .is_empty()
        );
        b.on_private_epoch(
            &wire_a,
            PrivateChannel::decode_update(&group.update_bytes().unwrap()).unwrap(),
            false,
        )
        .await;
        assert_eq!(
            b.private_channels.load(&group.id).unwrap().unwrap().replay,
            vec![id]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn private_group_frames_stay_on_proven_links() {
        let (mut runtime, radio, state) = super::super::tests::runtime();
        let paired = MeshIdentity::generate(&mut crate::crypto::os_rng());
        let stranger = MeshIdentity::generate(&mut crate::crypto::os_rng());
        let paired_link = LinkId::new("paired-group-link");
        let stranger_link = LinkId::new("unpaired-group-link");
        for (identity, link) in [(&paired, &paired_link), (&stranger, &stranger_link)] {
            super::super::rotation::tests::establish(&runtime, identity);
            bind_link(&state, link, &identity.peer_id());
        }
        let peer = paired.peer_id();
        pair(&runtime, &paired, 3, true);
        let transcript = state
            .with(|state| state.sessions.handshake_hash(&peer))
            .unwrap();
        let key = state.pairing_key(&peer).unwrap();
        let remote = PairingKey::new(*key.root(), false);
        let proof = sign_route(&paired, &remote, &transcript);
        assert!(runtime.on_pairing_control(&peer, &proof).await);
        runtime.drain_writes().await;
        state.with(|state| {
            state.max_write.insert(paired_link.clone(), 128);
        });

        let group_signer = &runtime.private_group_identity;
        let group = PrivateChannel::create("private", group_signer, "local").unwrap();
        let timestamp_ms = now_millis();
        let (_, payload) = group
            .encrypt(group_signer, &"private".repeat(60), "local", timestamp_ms)
            .unwrap();
        let mut packet = runtime.build_packet(MessageType::GroupMessage, None, payload, 7);
        packet.timestamp = timestamp_ms;
        assert!(
            !runtime
                .transmit_on_link(&packet, &stranger.peer_id(), &stranger_link)
                .await
        );
        assert!(runtime.transmit(&packet, None, None).await);
        runtime.drain_writes().await;
        let group_writes: Vec<_> = radio
            .writes()
            .into_iter()
            .filter(|(_, bytes)| {
                codec::decode(bytes).is_ok_and(|packet| {
                    super::super::transmission::is_private_group_packet(&packet)
                })
            })
            .collect();
        assert!(group_writes.len() > 1, "expected a fragmented group packet");
        assert!(group_writes.iter().all(|(link, _)| *link == paired_link));

        // Once the other link is also paired, it can relay the new format but
        // must never see even the first fragment of a legacy clear envelope.
        let relay = stranger.peer_id();
        pair(&runtime, &stranger, 4, true);
        let transcript = state
            .with(|state| state.sessions.handshake_hash(&relay))
            .unwrap();
        let remote = PairingKey::new(*state.pairing_key(&relay).unwrap().root(), false);
        assert!(
            runtime
                .on_pairing_control(&relay, &sign_route(&stranger, &remote, &transcript))
                .await
        );
        runtime.drain_writes().await;
        let before = radio.writes().len();
        packet.sender_id = peer.to_routing_bytes().unwrap();
        let mut legacy = packet.clone();
        legacy.payload.splice(19..19, [2, 0, 4, 0, 0, 0, 1]);
        for fragment in fragment::plan_fragments(&legacy, 100, [7; 8]).unwrap() {
            runtime.on_packet(&paired_link, fragment).await;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        runtime.drain_writes().await;
        assert_eq!(radio.writes().len(), before);

        for fragment in fragment::plan_fragments(&packet, 100, [8; 8]).unwrap() {
            runtime.on_packet(&paired_link, fragment).await;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        runtime.drain_writes().await;
        let writes = radio.writes();
        assert!(writes.len() > before);
        assert!(
            writes[before..]
                .iter()
                .all(|(link, _)| *link == stranger_link)
        );
    }

    #[tokio::test]
    async fn a_creator_can_revoke_a_member_after_both_radio_identities_rotate() {
        let (mut creator, _, creator_state) = super::super::tests::runtime();
        let (mut member, _, member_state) = super::super::tests::runtime();
        let old_creator_wire = creator.identity.peer_id();
        let old_member_wire = member.identity.peer_id();
        let creator_group = creator.private_group_identity.clone();
        let member_group = member.private_group_identity.clone();
        pair(&creator, &member.identity, 3, true);
        pair(&member, &creator.identity, 3, false);
        for (state, wire, group) in [
            (&creator_state, &old_member_wire, &member_group),
            (&member_state, &old_creator_wire, &creator_group),
        ] {
            state.with(|state| {
                assert!(
                    state.peers.bind_private_group_keys(
                        wire,
                        (group.static_public(), group.signing_public())
                    )
                )
            });
        }
        let mut channel =
            PrivateChannel::create("After rotation", &creator_group, "creator").unwrap();
        channel
            .rotate(
                &creator_group,
                creator_state.pinned_member(&old_member_wire, false),
                None,
            )
            .unwrap();
        member.private_channels.save(&channel).unwrap();
        creator.private_channels.save(&channel).unwrap();
        let old_channel = channel.clone();
        creator.identity = Arc::new(MeshIdentity::generate(&mut crate::crypto::os_rng()));
        member.identity = Arc::new(MeshIdentity::generate(&mut crate::crypto::os_rng()));
        let creator_wire = creator.identity.peer_id();
        let member_wire = member.identity.peer_id();
        let member_session = super::super::rotation::tests::establish(&creator, &member.identity);
        member_state.with(|state| state.sessions = member_session);
        let transcript = creator_state
            .with(|state| state.sessions.handshake_hash(&member_wire))
            .unwrap();
        let frame = sign_route(
            &creator.identity,
            &creator_state.pairing_key(&old_member_wire).unwrap(),
            &transcript,
        );
        assert!(member.on_pairing_control(&creator_wire, &frame).await);
        assert_eq!(
            member_state.wire_for_group_peer(&creator_group.peer_id()),
            Some(creator_wire.clone())
        );
        channel
            .rotate(&creator_group, None, Some(&member_group.peer_id()))
            .unwrap();
        let timestamp_ms = now_millis();
        let new_message = channel
            .encrypt(&creator_group, "after revocation", "creator", timestamp_ms)
            .unwrap()
            .1;
        assert!(old_channel.decrypt(&new_message, timestamp_ms).is_none());
        let revoked = channel.removed_update_bytes(&creator_group).unwrap();
        member
            .on_private_epoch(
                &creator_wire,
                PrivateChannel::decode_update(&revoked).unwrap(),
                false,
            )
            .await;
        assert!(member.private_channels.load(&channel.id).unwrap().is_none());
    }

    #[test]
    fn a_stranger_cannot_match_a_slot_or_reflect_a_first_write() {
        let (a, _, a_state) = super::super::tests::runtime();
        let (b, _, b_state) = super::super::tests::runtime();
        let (stranger, _, stranger_state) = super::super::tests::runtime();
        pair(&a, &b.identity, 1, true);
        pair(&a, &stranger.identity, 9, false);
        pair(&b, &a.identity, 1, false);
        pair(&stranger, &a.identity, 9, true);
        let preface = b_state
            .pairing_beacon_for_contact(&b.identity.peer_id(), &a.identity.peer_id())
            .unwrap();
        let slots = b_state.pairing_beacons(&b.identity.peer_id());
        assert_eq!(
            a_state.paired_contact_for_preface(&preface),
            Some(b.identity.peer_id())
        );
        assert!(b_state.paired_contact_for_preface(&preface).is_none());
        assert!(
            stranger_state
                .paired_contact_for_preface(&preface)
                .is_none()
        );
        assert!(a_state.paired_contact_for_beacon(&preface).is_none());
        assert!(!slots.contains(&preface));
        assert_eq!(
            slots
                .iter()
                .filter(|slot| a_state.paired_contact_for_beacon(slot).is_some())
                .count(),
            1
        );
        assert!(
            slots
                .iter()
                .all(|slot| stranger_state.paired_contact_for_beacon(slot).is_none())
        );
    }
}
