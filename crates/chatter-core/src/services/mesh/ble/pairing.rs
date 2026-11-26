use super::*;
use chatter_mesh_noise::seal::{open_courier, seal_courier};
use chatter_mesh_noise::sign::{sign_bytes, verify_bytes};
use subtle::ConstantTimeEq;

use super::super::pairing::route_proof;

const ROUTE_MAGIC: &[u8; 8] = b"CHTRPRT1";
const PULL_MAGIC: &[u8; 8] = b"CHTRPLQ1";
const CANDIDATE_MS: u64 = 60_000;

fn sign_route(
    identity: &MeshIdentity,
    key: &chatter_mesh_link::registry::PairingKey,
    transcript: &[u8; 32],
) -> Vec<u8> {
    let signing = identity.signing_public();
    let proof = route_proof(key, transcript, &identity.static_public(), &signing, true);
    let signed = [ROUTE_MAGIC.as_slice(), transcript, &proof].concat();
    let signature = sign_bytes(identity, &signed);
    [ROUTE_MAGIC.as_slice(), &signing, &proof, &signature].concat()
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
        let Some(transcript) = self.state.with(|state| state.sessions.handshake_hash(peer)) else {
            return;
        };
        if self.pairing_proofs_sent.get(peer) == Some(&transcript) {
            return;
        }
        self.pairing_proofs_sent.insert(peer.clone(), transcript);
        let frame = sign_route(&self.identity, &key, &transcript);
        self.send_private_control(peer, &frame).await;
    }

    pub(super) async fn on_pairing_control(&mut self, peer: &PeerId, plaintext: &[u8]) -> bool {
        if let Some(pull) = plaintext.strip_prefix(PULL_MAGIC) {
            self.on_courier_pull(peer, pull).await;
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
        let Some(transcript) = self.state.with(|state| state.sessions.handshake_hash(peer)) else {
            return true;
        };
        if self
            .pairing_proofs_received
            .insert(peer.clone(), transcript)
            == Some(transcript)
        {
            return true;
        }
        let accepted = self.state.with(|state| {
            let noise = state.sessions.remote_static(peer)?;
            if PeerId::from_noise_key(&noise) != *peer
                || !verify_bytes(
                    signature,
                    &[ROUTE_MAGIC.as_slice(), &transcript, proof].concat(),
                    signing,
                )
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
            self.gossip.retain_signing_key(peer, signing);
            self.pairing_candidates.remove(peer);
            self.send_pairing_proof(peer).await;
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
            || self.state.pairing_key(peer).is_none()
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
    async fn paired_contacts_reconnect_and_open_courier_mail_after_a_week_offline() {
        let (mut a, a_radio, a_state) = super::super::tests::runtime();
        let (mut b, _, b_state) = super::super::tests::runtime();
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
        announce.pairing_beacon = b_state.pairing_beacons(&wire_b).first().copied();
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
        let transcript = a_state
            .with(|state| state.sessions.handshake_hash(&wire_b))
            .unwrap();
        let key_a = a_state.pairing_key(&contact_b).unwrap();
        let key_b = b_state.pairing_key(&contact_a).unwrap();
        let frame_a =
            wrap_private_control(&a, &wire_b, &sign_route(&a.identity, &key_a, &transcript));
        let frame_b =
            wrap_private_control(&b, &wire_a, &sign_route(&b.identity, &key_b, &transcript));
        a.on_encrypted(&link_a, &frame_b, &wire_b).await;
        b.on_encrypted(&link_b, &frame_a, &wire_a).await;
        assert_eq!(a_state.route(&contact_b), wire_b);
        assert_eq!(b_state.route(&contact_a), wire_a);
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
        assert!(a_radio.writes().iter().any(|(_, bytes)| {
            codec::decode(bytes)
                .is_ok_and(|packet| packet.message_type() == Some(MessageType::CourierEnvelope))
        }));
        let id = courier_receipt_id(&envelope.ciphertext);
        let ack = [
            super::super::secure_ingress::COURIER_RECEIPT_MAGIC.as_slice(),
            &id,
        ]
        .concat();
        let ack = wrap_private_control(&b, &wire_a, &ack);
        a.on_encrypted(&link_a, &ack, &wire_b).await;
        assert!(!a.couriers.carries_for(&prior_address, now));
    }

    #[test]
    fn a_stranger_cannot_match_either_epoch_or_reflect_a_beacon() {
        let (a, _, a_state) = super::super::tests::runtime();
        let (b, _, b_state) = super::super::tests::runtime();
        let (stranger, _, stranger_state) = super::super::tests::runtime();
        pair(&a, &b.identity, 1, true);
        pair(&a, &stranger.identity, 9, false);
        pair(&b, &a.identity, 1, false);
        // This stranger is paired with someone else and has a populated index.
        pair(&stranger, &a.identity, 9, true);
        let first = b_state.pairing_beacons(&b.identity.peer_id())[0];
        let next_identity = MeshIdentity::generate(&mut crate::crypto::os_rng());
        let next = b_state.pairing_beacons(&next_identity.peer_id())[0];
        assert_ne!(first, next);
        assert_ne!(&first[5..], &next[5..]);
        assert_eq!(
            a_state.paired_contact_for_beacon(&first),
            Some(b.identity.peer_id())
        );
        assert_eq!(
            a_state.paired_contact_for_beacon(&next),
            Some(b.identity.peer_id())
        );
        assert!(stranger_state.paired_contact_for_beacon(&first).is_none());
        assert!(stranger_state.paired_contact_for_beacon(&next).is_none());
        assert!(b_state.paired_contact_for_beacon(&first).is_none());
    }
}
