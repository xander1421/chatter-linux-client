use super::*;
use chatter_mesh::tlv::courier_envelope::courier_receipt_id;
use chatter_mesh_noise::sign::{sign_bytes, verify_bytes};

use super::super::account_identity::{PAIRING_RECEIPT_MAGIC, store_paired_pin};
use super::super::chatter_hello::COURIER_RECEIPTS;

pub(super) const COURIER_RECEIPT_MAGIC: &[u8; 8] = b"CHTRCRP1";
pub(super) const PAIRED_COURIER_MAGIC: &[u8; 8] = b"CHTRPCU1";
const SIGNING_BINDING_MAGIC: &[u8; 8] = b"CHTRNSB1";
pub(super) const SIGNING_BINDING_REQUEST: &[u8; 8] = b"CHTRNSR1";
pub(super) const SIGNING_PIN_INTENT_MS: u64 = 300_000;

fn signing_binding_bytes(noise: &[u8; 32], transcript: &[u8; 32], signing: &[u8; 32]) -> Vec<u8> {
    [SIGNING_BINDING_MAGIC.as_slice(), noise, transcript, signing].concat()
}

impl BleRuntime {
    pub(super) fn allows_chatter_hello(&self, peer: &PeerId) -> bool {
        self.state.paired_session(peer)
            || self.config.is_pairing_peer(peer, std::time::Instant::now())
    }

    pub(super) fn allows_pairing_control(&self, peer: &PeerId, bytes: &[u8]) -> bool {
        self.config.is_pairing_peer(peer, Instant::now())
            && (ChatterHello::decode(bytes).is_some()
                || BindingV1::decode(bytes).is_some()
                || bytes.starts_with(SIGNING_BINDING_MAGIC)
                || bytes.starts_with(SIGNING_BINDING_REQUEST))
    }

    fn on_pairing_receipt(&mut self, peer: &PeerId, receipt: &[u8]) {
        let Ok(receipt) = <&[u8; 128]>::try_from(receipt) else {
            return;
        };
        let key: &[u8; 32] = receipt[32..64].try_into().expect("pairing receipt layout");
        let signature: &[u8; 64] = receipt[64..].try_into().expect("pairing receipt layout");
        let paired = self.state.with(|state| {
            let Some(offer) = state.pairing_offers.get(peer) else {
                return false;
            };
            let Some(static_key) = state.sessions.remote_static(peer) else {
                return false;
            };
            if offer.expires_at <= now_secs()
                || offer.token != receipt[..32]
                || state.sessions.handshake_hash(peer) != Some(offer.transcript)
                || !verify_bytes(
                    signature,
                    &[
                        PAIRING_RECEIPT_MAGIC.as_slice(),
                        &offer.token,
                        &offer.transcript,
                        key,
                    ]
                    .concat(),
                    key,
                )
            {
                return false;
            }
            let (Some(local), Some(remote)) = (
                state.chatter_hello.local_nonce(peer),
                state.chatter_hello.get(peer).map(|hello| hello.nonce),
            ) else {
                return false;
            };
            let pairing = super::super::pairing::establish_key(
                &offer.token,
                &offer.transcript,
                &local,
                &remote,
                true,
            );
            if !store_paired_pin(
                state,
                self.storage.as_ref(),
                self.sealer.as_ref(),
                peer,
                &static_key,
                key,
                pairing,
            ) {
                return false;
            }
            state.pairing_offers.remove(peer);
            true
        });
        if paired {
            self.refresh_pairing_beacons();
            self.gossip.retain_signing_key(peer, key);
            self.emit_peers();
        }
    }

    fn observe_account_session(&self, owner: &str, peer: &PeerId, noise: &[u8; 32]) -> bool {
        let contact = self.state.contact(peer);
        if self.state.route(&contact) != *peer {
            return false;
        }
        if let Some((key, _)) = self.state.rotated_keys(peer) {
            return key == *noise;
        }
        self.binding_store
            .observe_live_key(owner, &contact, noise)
            .is_ok()
    }

    pub(super) fn account_mesh_keys(
        &self,
        owner: &str,
        peer: &PeerId,
    ) -> Option<([u8; 32], [u8; 32])> {
        let contact = self.state.contact(peer);
        if self.state.route(&contact) != *peer {
            return None;
        }
        let noise = self
            .binding_store
            .verified_noise_key(owner, &contact)
            .ok()??;
        let signing = self
            .binding_store
            .verified_signing_key(owner, &contact)
            .ok()??;
        Some(self.state.rotated_keys(peer).unwrap_or((noise, signing)))
    }

    async fn send_mesh_signing_binding(&mut self, peer: &PeerId) {
        let Some(transcript) = self.state.with(|state| state.sessions.handshake_hash(peer)) else {
            return;
        };
        let key = self.identity.signing_public();
        let proof = sign_bytes(
            &self.identity,
            &signing_binding_bytes(&self.identity.static_public(), &transcript, &key),
        );
        let frame = [SIGNING_BINDING_MAGIC.as_slice(), &key, &proof].concat();
        self.send_private_control(peer, &frame).await;
    }

    pub(super) async fn on_handshake(
        &mut self,
        link: &LinkId,
        packet: &BitchatPacket,
        claimed: &PeerId,
    ) {
        let local = self.identity.peer_id();
        let identity = Arc::clone(&self.identity);
        let step = self.state.with(|state| {
            if !state
                .sessions
                .admit_inbound(claimed, &local, &link.to_string(), &packet.payload)
            {
                return None;
            }
            let step = state
                .sessions
                .on_handshake(claimed, &local, &identity, &packet.payload);
            if step
                .as_ref()
                .map_or(true, |step| step.established.is_some())
            {
                state.sessions.release_inbound(claimed);
            }
            Some(step)
        });

        let step = match step {
            Some(Ok(step)) => step,
            Some(Err(error)) => {
                // Warn: no session means no direct message with this peer will
                // ever open, and nothing retries on a schedule. Silence here reads
                // as "they never wrote", which is the opposite of what happened.
                tracing::warn!(target: "mesh::ble", peer = %claimed, %error, "handshake failed");
                return;
            }
            None => return,
        };

        if let Some(reply) = step.reply {
            let packet = self.build_packet(
                MessageType::NoiseHandshake,
                Some(claimed),
                reply,
                chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
            );
            if self.state.with(|state| {
                state.links.peer_for(link) == Some(claimed)
                    || state.links.discovery_peer_for(link) == Some(claimed)
            }) {
                self.transmit_on_link(&packet, claimed, link).await;
            } else {
                self.transmit(&packet, Some(claimed), None).await;
            }
        }

        if let Some(recipient_key) = step.established {
            self.courier_claims.remove(claimed);
            self.state.with(|state| {
                state.dm_handshake_at_ms.remove(claimed);
                state.known_peer_reopen_attempts.remove(claimed);
                state.chatter_hello.forget_live(claimed);
                state.pending_bindings.remove(claimed);
                state.pairing_offers.remove(claimed);
            });
            // Only a direct handshake can authenticate this physical link.
            let direct = packet.ttl == chatter_mesh::relay::MESSAGE_TTL_DEFAULT
                && self
                    .state
                    .with(|state| state.links.mark_noise_authenticated(link, claimed));
            if direct
                && let Some(record) = self.link_admission.get_mut(link)
                && record.candidate.as_ref() == Some(claimed)
            {
                record.candidate_authenticated = true;
            }
            let paired = self.state.is_paired_link_to(link, claimed);
            let application_data_allowed = paired || self.config.public_chat_enabled();
            if direct {
                self.prune_duplicate_links(claimed, now_millis()).await;
                if !self.config.public_chat_enabled() && paired {
                    self.sync_new_peer(link, claimed, now_secs()).await;
                }
                let announced_key_matches = self.state.with(|state| {
                    state.peers.info(claimed).is_some_and(|peer| {
                        peer.noise_public_key.as_deref() == Some(recipient_key.as_slice())
                    })
                });
                if application_data_allowed && announced_key_matches {
                    self.hand_over_courier_mail(
                        claimed,
                        &recipient_key,
                        now_secs(),
                        AnnounceIngress::verified_direct_on_authenticated_link(),
                    )
                    .await;
                }
            }
            // The mesh runtime exists only after the user enabled discovery.
            // Keep capability negotiation inside this authenticated Noise link.
            if direct {
                self.send_pairing_proof(link, claimed).await;
            }
            self.send_chatter_hello(claimed).await;
            if !application_data_allowed {
                return;
            }
            // A session is the only signal strong enough for the router to send
            // without retaining, so this is the moment retained mail can go.
            let _ = self.router.send(RouterCommand::PeerAvailable {
                peer: self.state.contact(claimed),
            });
            self.flush_pending_dms(claimed).await;
            self.emit_peers();
        }
    }

    pub(super) async fn sync_private_groups(&mut self, claimed: &PeerId) {
        if self.verified_pairing_link(claimed).is_none() {
            return;
        }
        let Some(group) = self.state.group_peer_for_wire(claimed) else {
            return;
        };
        if let Ok(leaves) = self.private_channels.pending_leaves_for(&group) {
            for leave in leaves {
                if let Ok(bytes) = serde_json::to_vec(&leave.notice) {
                    let mut control = b"CHTRPCL2".to_vec();
                    control.extend(bytes);
                    self.send_group_control(claimed, &control).await;
                }
            }
        }
        if let Ok(states) = self.private_channels.list() {
            for state in states {
                if state.creator == self.private_group_identity.peer_id().as_str()
                    && state
                        .removed_members
                        .iter()
                        .any(|peer| peer == group.as_str())
                    && let Ok(bytes) = state.removed_update_bytes(&self.private_group_identity)
                {
                    self.send_group_state(&group, &bytes, false).await;
                }
                if state.creator == self.private_group_identity.peer_id().as_str()
                    && state.member_key(&group).is_some()
                {
                    for notice in &state.departed {
                        if notice.peer == group.as_str() {
                            continue;
                        }
                        if let Ok(bytes) = serde_json::to_vec(notice) {
                            let mut control = b"CHTRPCL2".to_vec();
                            control.extend(bytes);
                            self.send_group_control(claimed, &control).await;
                        }
                    }
                    if let Some(hint) = state.epoch_proof().encode() {
                        self.send_group_control(claimed, &hint).await;
                    }
                    if !state.stale
                        && !state
                            .unanswered_direct_invites
                            .get(group.as_str())
                            .is_some_and(|at| {
                                now_secs().saturating_sub(*at)
                                    >= super::super::DIRECT_INVITE_LIFETIME_SECS
                            })
                        && let Ok(bytes) = state.update_bytes()
                    {
                        let invite = state.unanswered_direct_invites.contains_key(group.as_str());
                        self.send_group_state(&group, &bytes, invite).await;
                    }
                }
            }
        }
    }

    pub(super) async fn on_encrypted(
        &mut self,
        link: &LinkId,
        packet: &BitchatPacket,
        claimed: &PeerId,
    ) {
        if self.events.peer_blocked(&self.state.contact(claimed)) {
            return;
        }
        // Only accept encrypted traffic addressed to us; a directed packet for
        // somebody else is relayed, not opened.
        if !self.addressed_to_us(packet) {
            return;
        }
        let opened = self
            .state
            .with(|state| state.sessions.decrypt(claimed, &packet.payload));
        let plaintext = match opened {
            Ok(plaintext) => zeroize::Zeroizing::new(plaintext),
            Err(NoiseError::WrongState(_)) => {
                // No session. Start one rather than dropping the peer silently:
                // this is how a first message from a peer we have never spoken
                // to gets answered.
                self.start_handshake(claimed).await;
                return;
            }
            Err(error) => {
                // Warn: this was a message, and over the mesh there is no server
                // copy to fetch again. The plaintext is gone.
                tracing::warn!(target: "mesh::ble", peer = %claimed, %error, "decrypt failed");
                // The peer restarted or re-keyed and our session no longer
                // matches theirs; until one side re-opens Noise every later
                // message is lost the same way. Open a new handshake, at most
                // once per retry window. The current session stays until the
                // new one completes, so forged ciphertext cannot tear it down.
                let due = self.state.with(|state| {
                    !state.sessions.is_handshaking(claimed)
                        && now_millis()
                            .saturating_sub(*state.dm_handshake_at_ms.get(claimed).unwrap_or(&0))
                            >= DM_HANDSHAKE_RETRY_MS
                });
                if due {
                    self.start_handshake(claimed).await;
                }
                return;
            }
        };

        // The link carried traffic only this peer's session key could produce.
        self.state.with(|state| {
            if packet.ttl == chatter_mesh::relay::MESSAGE_TTL_DEFAULT
                && state.links.peer_for(link) == Some(claimed)
            {
                state.links.mark_noise_authenticated(link, claimed);
            }
        });

        if let Some(receipt) = plaintext.strip_prefix(PAIRING_RECEIPT_MAGIC) {
            self.on_pairing_receipt(claimed, receipt);
            self.send_pairing_proof(link, claimed).await;
            return;
        }
        if self.on_pairing_control(link, claimed, &plaintext).await {
            return;
        }
        if !self.config.public_chat_enabled()
            && !self.state.is_paired_link_to(link, claimed)
            && !self.allows_pairing_control(claimed, &plaintext)
        {
            return;
        }
        if let Some(payload) = plaintext.strip_prefix(PAIRED_COURIER_MAGIC) {
            if self.state.is_paired_link_to(link, claimed) {
                self.receive_courier(payload, packet.timestamp, claimed)
                    .await;
            }
            return;
        }
        if let Some(receipt) = plaintext.strip_prefix(COURIER_RECEIPT_MAGIC) {
            let Ok(receipt) = <&[u8; 32]>::try_from(receipt) else {
                return;
            };
            let now = now_secs();
            let mut keys = self
                .courier_claims
                .get(claimed)
                .into_iter()
                .flatten()
                .copied()
                .collect::<Vec<_>>();
            keys.extend(
                self.state
                    .with(|state| state.sessions.remote_static(claimed)),
            );
            for key in keys {
                match self.couriers.acknowledge_delivery(&key, receipt, now) {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(error) => {
                        tracing::warn!(target: "mesh::ble", %error, "courier receipt could not retire mail");
                        break;
                    }
                }
            }
            return;
        }

        if let Some(hello) = ChatterHello::decode(&plaintext) {
            if !self.allows_chatter_hello(claimed) {
                return;
            }
            let can_bind = hello.compatible()
                && hello.capabilities & super::super::chatter_hello::NOISE_SIGNING_BINDING != 0;
            let signal_capable = hello.capabilities & super::super::chatter_hello::SIGNAL_MESH != 0
                && hello.compatible()
                && hello.max_carrier_bytes >= frames::MAX_CARRIER as u32;
            match self
                .binding_store
                .note_hello(claimed, &hello.nonce, now_secs())
            {
                Ok(true) => {}
                Ok(false) => return,
                Err(error) => {
                    tracing::warn!(target: "mesh::ble", peer = %claimed, %error,
                        "cannot persist Chatter Hello nonce");
                    return;
                }
            }
            let supports_receipts = hello.capabilities & COURIER_RECEIPTS != 0;
            let recipient_key = self.state.with(|state| {
                if state.chatter_hello.accept(claimed, hello) {
                    state.sessions.remote_static(claimed)
                } else {
                    None
                }
            });
            // If the remote Hello arrived before we selected this peer,
            // repeat our existing nonce after accepting their retry.
            self.send_chatter_hello(claimed).await;
            if let Some(key) = recipient_key
                && let Err(error) =
                    self.couriers
                        .resolve_receipt_support(&key, supports_receipts, now_secs())
            {
                tracing::warn!(target: "mesh::ble", %error, "courier receipt support could not be recorded");
            }
            tracing::info!(target: "mesh::ble", peer = %claimed, signal_capable, "Chatter Hello accepted");
            if can_bind {
                self.send_mesh_signing_binding(claimed).await;
                if self
                    .signing_pin_intent
                    .get(claimed)
                    .is_some_and(|(at, _)| now_millis().saturating_sub(*at) < SIGNING_PIN_INTENT_MS)
                {
                    self.request_signing_pin(claimed).await;
                }
            }
            if signal_capable
                && let Ok(Some((owner, device))) = self.key_store.mesh_local_account_device()
                && let Some(static_key) = self
                    .state
                    .with(|state| state.sessions.remote_static(claimed))
                && self.observe_account_session(&owner, claimed, &static_key)
            {
                let contact = self.state.contact(claimed);
                if matches!(
                    self.binding_store.state(&owner, &contact),
                    Ok(PeerBinding::Verified { .. })
                ) {
                    match signal_mesh::bundle_for_peer(
                        &self.key_store,
                        &self.binding_store,
                        &owner,
                        &contact,
                        device,
                        now_secs(),
                    ) {
                        Ok(bytes) => self.send_signal_bundle(claimed, &bytes).await,
                        Err(error) => {
                            tracing::warn!(target: "mesh::ble", peer = %claimed, %error,
                                "mesh Signal bundle generation failed");
                        }
                    }
                    if self.binding_store.signal_opted_in(&owner, &contact).ok() == Some(true)
                        && let Ok(pending) =
                            self.binding_store
                                .pending_mesh_carriers(&owner, &contact, now_secs())
                    {
                        for bytes in pending {
                            self.send_signal_carrier(claimed, bytes).await;
                        }
                    }
                }
            }
            self.emit_peers();
            return;
        }
        if plaintext.as_slice() == SIGNING_BINDING_REQUEST {
            self.send_mesh_signing_binding(claimed).await;
            return;
        }
        if let Some(binding) = plaintext.strip_prefix(SIGNING_BINDING_MAGIC) {
            let Ok(binding) = <&[u8; 96]>::try_from(binding) else {
                return;
            };
            let (key, proof) = binding.split_at(32);
            let (Ok(key), Ok(proof)) = (<&[u8; 32]>::try_from(key), <&[u8; 64]>::try_from(proof))
            else {
                return;
            };
            let now = now_millis();
            // An unsolicited handshake proves possession but does not give
            // a radio stranger a slot in the durable contact table.
            if !self
                .signing_pin_intent
                .get(claimed)
                .is_some_and(|(at, _)| now.saturating_sub(*at) < SIGNING_PIN_INTENT_MS)
            {
                return;
            }
            let saved = self.state.with(|state| {
                let Some(noise) = state.sessions.remote_static(claimed) else {
                    return false;
                };
                let Some(transcript) = state.sessions.handshake_hash(claimed) else {
                    return false;
                };
                if !verify_bytes(proof, &signing_binding_bytes(&noise, &transcript, key), key) {
                    return false;
                }
                let mut candidate = state.peers.clone();
                if !candidate.pin_noise_identity(claimed, &noise, key, now) {
                    return false;
                }
                if state.peers.pinned(claimed).is_none() {
                    let Ok(sealed) = self.sealer.seal(&candidate.pins_to_bytes()) else {
                        return false;
                    };
                    if self.storage.save(StorageKey::PeerPins, &sealed).is_err() {
                        return false;
                    }
                }
                state.peers = candidate;
                true
            });
            if saved {
                self.gossip.retain_signing_key(claimed, key);
                self.signing_pin_intent.remove(claimed);
                self.emit_peers();
            }
            return;
        }
        if let Some(proof) = BindingV1::decode(&plaintext) {
            let valid = self.state.with(|state| {
                let Some(static_key) = state.sessions.remote_static(claimed) else {
                    return false;
                };
                let Some(transcript) = state.sessions.handshake_hash(claimed) else {
                    return false;
                };
                let Some(local) = state.chatter_hello.local_nonce(claimed) else {
                    return false;
                };
                let Some(remote) = state.chatter_hello.get(claimed).map(|h| h.nonce) else {
                    return false;
                };
                proof.verify_live(&static_key, &transcript, &local, &remote, now_secs())
            });
            if valid {
                self.state.with(|state| {
                    state.pending_bindings.insert(claimed.clone(), proof);
                });
                self.emit_peers();
            }
            return;
        }
        if let Some(frame) = BundleFrame::decode(&plaintext, now_secs()) {
            let Ok(Some((owner, device))) = self.key_store.mesh_local_account_device() else {
                tracing::warn!(target: "mesh::ble", peer = %claimed, "mesh Signal bundle dropped: no local account");
                return;
            };
            let Some(static_key) = self
                .state
                .with(|state| state.sessions.remote_static(claimed))
            else {
                return;
            };
            let contact = self.state.contact(claimed);
            if !self.observe_account_session(&owner, claimed, &static_key) {
                return;
            }
            let received =
                self.binding_store
                    .receive_mesh_bundle(&owner, &contact, &frame, now_secs());
            tracing::info!(target: "mesh::ble", peer = %claimed, ok = received.is_ok(),
                new = matches!(received, Ok(true)), "mesh Signal bundle received");
            match received {
                Ok(true) if frame.reply_requested => {
                    if let Ok(bytes) = signal_mesh::bundle_for_peer(
                        &self.key_store,
                        &self.binding_store,
                        &owner,
                        &contact,
                        device,
                        now_secs(),
                    ) {
                        self.send_signal_bundle(claimed, &bytes).await;
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(target: "mesh::ble", peer=%claimed, %error, "rejected mesh Signal bundle")
                }
            }
            return;
        }

        if let Some(invite) = Invite::decode_request(&plaintext, now_secs()) {
            self.on_private_join_request(claimed, invite).await;
            return;
        }
        if let Some(proof) = EpochProof::decode(&plaintext) {
            self.on_private_epoch_hint(claimed, proof).await;
            return;
        }
        if let Some(bytes) = plaintext.strip_prefix(b"CHTRPCD1") {
            if let Ok(notice) = serde_json::from_slice::<LeaveNotice>(bytes) {
                self.on_private_decline(claimed, notice);
            }
            return;
        }
        if let Some(bytes) = plaintext.strip_prefix(b"CHTRPCL2") {
            if let Ok(notice) = serde_json::from_slice::<LeaveNotice>(bytes) {
                let id = notice.id.clone();
                let epoch = notice.epoch;
                if self
                    .state
                    .group_peer_for_wire(claimed)
                    .is_some_and(|group| self.on_private_leave(&group, notice))
                {
                    let mut ack = b"CHTRPLA1".to_vec();
                    if let Ok(uuid) = uuid::Uuid::parse_str(&id) {
                        ack.extend_from_slice(uuid.as_bytes());
                        ack.extend_from_slice(&epoch.to_be_bytes());
                        self.send_group_control(claimed, &ack).await;
                    }
                }
            }
            return;
        }
        if plaintext.len() == 28 && plaintext.starts_with(b"CHTRPLA1") {
            if let Ok(id) = uuid::Uuid::from_slice(&plaintext[8..24]) {
                let epoch = u32::from_be_bytes(plaintext[24..28].try_into().expect("leave epoch"));
                if let Some(group) = self.state.group_peer_for_wire(claimed) {
                    let _ =
                        self.private_channels
                            .clear_pending_leave(&id.to_string(), epoch, &group);
                }
            }
            return;
        }
        if plaintext.starts_with(b"CHTRPCA1") {
            self.on_private_epoch_ack(claimed, &plaintext);
            return;
        }

        let Some((payload_type, body)) = decode_noise_payload(&plaintext) else {
            return;
        };
        match payload_type {
            NoisePayloadType::GroupInvite | NoisePayloadType::GroupKeyUpdate => {
                if let Some(update) = PrivateChannel::decode_update(body) {
                    let invite = payload_type == NoisePayloadType::GroupInvite;
                    self.on_private_epoch(claimed, update, invite).await;
                }
            }
            NoisePayloadType::PrivateMessage => {
                let Ok(message) = PrivateMessagePacket::decode(body) else {
                    return;
                };
                if self.emit_dm(
                    claimed,
                    &message.message_id,
                    &message.content,
                    packet.timestamp,
                ) {
                    self.send_ack(claimed, &message.message_id, NoisePayloadType::Delivered)
                        .await;
                }
            }
            NoisePayloadType::Delivered => self.forward_ack(claimed, body, AckKind::Delivered),
            NoisePayloadType::ReadReceipt => self.forward_ack(claimed, body, AckKind::Read),
            // Groups, voice, and verification are deferred; dropping the payload
            // is the correct handling for a type this build does not implement.
            _ => {}
        }
    }

    /// Only the bound mesh signing key may submit a Signal carrier for local
    /// decryption. Relays see a bounded opaque packet and never handle account
    /// identity or Signal plaintext.
    pub(super) async fn on_signal_carrier(
        &mut self,
        packet: &BitchatPacket,
        claimed: &PeerId,
    ) -> bool {
        if packet.recipient_id.is_none()
            || frames::CarrierFrame::decode(&packet.payload, now_secs()).is_none()
        {
            return false;
        }
        if !self.addressed_to_us(packet) {
            return true;
        }
        let Ok(Some((owner, _))) = self.key_store.mesh_local_account_device() else {
            return false;
        };
        let Some((_, signing)) = self.account_mesh_keys(&owner, claimed) else {
            return false;
        };
        if !verify_packet(packet, &signing) {
            return false;
        }
        self.process_signal_carrier(&packet.payload, claimed, packet.timestamp, false)
            .await
    }

    /// The outer courier seal authenticates the sender's Noise static key;
    /// direct E4 packets are signature-checked before entering this function.
    pub(super) async fn process_signal_carrier(
        &mut self,
        bytes: &[u8],
        claimed: &PeerId,
        timestamp: u64,
        replay: bool,
    ) -> bool {
        use libsignal_protocol::{DeviceId, ProtocolAddress};
        let wire_peer = self.state.route(claimed);
        let contact = self.state.contact(&wire_peer);
        let claimed = &wire_peer;
        let minute = now_secs() / 60;
        if self.carrier_ingress.len() >= 1024 {
            self.carrier_ingress
                .retain(|_, (seen, _)| seen.saturating_add(1) >= minute);
            if self.carrier_ingress.len() >= 1024 {
                return false;
            }
        }
        let entry = self
            .carrier_ingress
            .entry(claimed.as_str().to_owned())
            .or_insert((minute, 0));
        if entry.0 != minute {
            *entry = (minute, 0);
        }
        if entry.1 >= CARRIER_INGRESS_PER_MINUTE {
            return false;
        }
        entry.1 += 1;
        let Some(frame) = frames::CarrierFrame::decode(bytes, now_secs()) else {
            return false;
        };
        if self.events.peer_blocked(&self.state.contact(claimed)) {
            return false;
        }
        let Ok(Some((owner, local_device))) = self.key_store.mesh_local_account_device() else {
            return false;
        };
        let Some(_) = self.account_mesh_keys(&owner, claimed) else {
            return false;
        };
        if self.binding_store.signal_opted_in(&owner, &contact).ok() != Some(true) {
            return false;
        }
        let Ok(PeerBinding::Verified {
            account_id,
            device_id,
            ..
        }) = self.binding_store.state(&owner, &contact)
        else {
            return false;
        };
        let Ok(local_device) = DeviceId::try_from(local_device) else {
            return false;
        };
        let Ok(remote_device) = DeviceId::try_from(device_id) else {
            return false;
        };
        let remote = ProtocolAddress::new(
            uuid::Uuid::from_bytes(account_id).to_string(),
            remote_device,
        );
        let local = ProtocolAddress::new(owner.clone(), local_device);
        let _guard = self.signal_lock.lock();
        if replay
            && self
                .binding_store
                .abort_mesh_receive(&owner, &contact, &frame.message_id)
                .is_err()
        {
            return false;
        }
        match self
            .binding_store
            .mesh_received(&owner, &contact, &frame.message_id)
        {
            Ok(None) => {}
            Ok(Some(1)) => {
                // A resent ciphertext was already ratcheted. Generate a fresh
                // encrypted receipt without asking libsignal to decrypt it twice.
                let mut store = crate::crypto::signal::SignalStore::for_mesh(
                    self.key_store.clone(),
                    self.binding_store.clone(),
                    owner.clone(),
                    contact.clone(),
                );
                let receipt_id = uuid::Uuid::new_v4();
                let receipt = signal_mesh::SignalPlaintext {
                    kind: signal_mesh::PlainKind::Delivered,
                    conversation_id: signal_mesh::conversation_id(&local, &remote),
                    message_id: frame.message_id,
                    sender_device: u32::from(local_device),
                    recipient_device: device_id,
                    expires_at: now_secs().saturating_add(24 * 3600),
                    text: String::new(),
                };
                let reply = futures::executor::block_on(signal_mesh::encrypt(
                    &mut store,
                    &remote,
                    &local,
                    &receipt,
                    *receipt_id.as_bytes(),
                ))
                .and_then(|frame| {
                    frame
                        .encode()
                        .map_err(crate::services::mesh::MeshError::Binding)
                })
                .ok();
                drop(store);
                drop(_guard);
                if let Some(reply) = reply {
                    self.send_signal_carrier(claimed, reply).await;
                }
                return true;
            }
            Ok(Some(_)) => return true,
            Err(_) => return false,
        }
        let session_address = format!("mesh:{owner}:{}:{}", remote.name(), remote.device_id());
        if self
            .binding_store
            .begin_mesh_receive(
                &owner,
                &contact,
                &frame.message_id,
                bytes,
                timestamp,
                &session_address,
                frame.ciphertext_type == 2,
            )
            .is_err()
        {
            return false;
        }
        let mut store = crate::crypto::signal::SignalStore::for_mesh(
            self.key_store.clone(),
            self.binding_store.clone(),
            owner.clone(),
            contact.clone(),
        );
        let plaintext = match futures::executor::block_on(signal_mesh::decrypt(
            &mut store, &remote, &local, &frame,
        )) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(target: "mesh::ble", peer=%claimed, %error, "mesh Signal decrypt rejected");
                let _ = self
                    .binding_store
                    .abort_mesh_receive(&owner, &contact, &frame.message_id);
                return false;
            }
        };
        let Some(plain) = signal_mesh::SignalPlaintext::decode(&plaintext, now_secs()) else {
            let _ = self
                .binding_store
                .abort_mesh_receive(&owner, &contact, &frame.message_id);
            return false;
        };
        if plain.conversation_id != signal_mesh::conversation_id(&local, &remote)
            || plain.sender_device != device_id
            || plain.recipient_device != u32::from(local_device)
            || plain.expires_at != frame.expires_at
        {
            let _ = self
                .binding_store
                .abort_mesh_receive(&owner, &contact, &frame.message_id);
            return false;
        }
        if matches!(plain.kind, signal_mesh::PlainKind::Text)
            && (plain.message_id != frame.message_id || plain.text.is_empty())
        {
            let _ = self
                .binding_store
                .abort_mesh_receive(&owner, &contact, &frame.message_id);
            return false;
        }
        let kind = match plain.kind {
            signal_mesh::PlainKind::Text => 1,
            signal_mesh::PlainKind::Delivered => 2,
        };
        let wire_id = uuid::Uuid::from_bytes(plain.message_id)
            .to_string()
            .to_uppercase();
        let signal_message = matches!(plain.kind, signal_mesh::PlainKind::Text)
            .then(|| self.signal_dm_message(claimed, &wire_id, &plain.text, timestamp));
        if signal_message
            .as_ref()
            .is_some_and(|message| self.events.persist_received_dm(message).is_err())
        {
            let _ = self
                .binding_store
                .abort_mesh_receive(&owner, &contact, &frame.message_id);
            return false;
        }
        let delivery_id = mapping::wire_message_id(&wire_id).to_string();
        let receipt_for = matches!(plain.kind, signal_mesh::PlainKind::Delivered)
            .then_some((&plain.message_id, delivery_id.as_str()));
        let Ok((true, acknowledged)) = self.binding_store.finish_mesh_receive(
            &owner,
            &contact,
            &frame.message_id,
            kind,
            frame.expires_at,
            receipt_for,
        ) else {
            return false;
        };
        if let Some(message) = signal_message {
            self.events.publish_received_dm(message);
        }
        let reply = match plain.kind {
            signal_mesh::PlainKind::Text => {
                let receipt_id = uuid::Uuid::new_v4();
                let receipt = signal_mesh::SignalPlaintext {
                    kind: signal_mesh::PlainKind::Delivered,
                    conversation_id: plain.conversation_id,
                    message_id: plain.message_id,
                    sender_device: u32::from(local_device),
                    recipient_device: device_id,
                    expires_at: now_secs().saturating_add(24 * 3600),
                    text: String::new(),
                };
                futures::executor::block_on(signal_mesh::encrypt(
                    &mut store,
                    &remote,
                    &local,
                    &receipt,
                    *receipt_id.as_bytes(),
                ))
                .and_then(|frame| {
                    frame
                        .encode()
                        .map_err(crate::services::mesh::MeshError::Binding)
                })
                .ok()
            }
            signal_mesh::PlainKind::Delivered => {
                if acknowledged {
                    self.events.send(MeshEvent::Delivery {
                        message_id: wire_id,
                        peer: Some(contact.clone()),
                        state: super::super::events::MeshDelivery::Delivered,
                    });
                }
                None
            }
        };
        drop(store);
        drop(_guard);
        if let Some(reply) = reply {
            self.send_signal_carrier(claimed, reply).await;
        }
        true
    }

    fn forward_ack(&self, peer: &PeerId, body: &[u8], kind: AckKind) {
        // The ack body is the message ID as raw UTF-8, not a TLV.
        let Ok(message_id) = String::from_utf8(body.to_vec()) else {
            return;
        };
        let _ = self.router.send(RouterCommand::Acked {
            message_id,
            peer: self.state.contact(peer),
            kind,
        });
    }

    pub(super) async fn send_courier_frame(
        &self,
        peer: &PeerId,
        envelope: &CourierEnvelope,
    ) -> bool {
        let Ok(payload) = envelope.encode() else {
            return false;
        };
        let peer = self.state.route(peer);
        if self.state.has_paired_pin(&peer) {
            // Stable recipient tags and repeated envelopes stay inside Noise.
            if !self.state.paired_session(&peer) {
                return false;
            }
            let frame = [PAIRED_COURIER_MAGIC.as_slice(), &payload].concat();
            return self.send_private_control(&peer, &frame).await;
        }
        let packet = self.build_packet(
            MessageType::CourierEnvelope,
            Some(&peer),
            payload,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        self.transmit(&packet, Some(&peer), None).await
    }

    pub(super) async fn on_courier_envelope(&mut self, packet: &BitchatPacket, claimed: &PeerId) {
        self.receive_courier(&packet.payload, packet.timestamp, claimed)
            .await;
    }

    async fn receive_courier(&mut self, payload: &[u8], timestamp: u64, claimed: &PeerId) {
        let Ok(envelope) = CourierEnvelope::decode(payload) else {
            return;
        };
        if envelope.is_expired(now_millis()) {
            return;
        }

        // Is it ours? Only the seal can answer that — the recipient tag rotates
        // and is a hint, not proof.
        let paired = self.state.open_paired_mail(&envelope, now_secs());
        let authenticated_pair = paired.is_some();
        let opened = paired
            .map(|(plaintext, contact)| (plaintext, contact, None))
            .or_else(|| {
                let (plaintext, sender_key) = super::super::prekeys::open_envelope(
                    &self.state,
                    &envelope,
                    &self.identity,
                    now_millis(),
                )?;
                let sender = PeerId::from_noise_key(&sender_key);
                Some((plaintext, sender, Some(sender_key)))
            });
        if let Some((plaintext, sender, sender_key)) = opened {
            let delivered = if let Some(carrier) = plaintext.strip_prefix(b"CSM1") {
                if let Ok(Some((owner, _))) = self.key_store.mesh_local_account_device()
                    && self
                        .account_mesh_keys(&owner, &self.state.route(&sender))
                        .is_some_and(|(noise, _)| authenticated_pair || Some(noise) == sender_key)
                {
                    self.process_signal_carrier(carrier, &sender, timestamp, false)
                        .await
                } else {
                    false
                }
            } else if let Some((NoisePayloadType::PrivateMessage, body)) =
                decode_noise_payload(&plaintext)
                && let Ok(message) = PrivateMessagePacket::decode(body)
            {
                self.emit_dm(&sender, &message.message_id, &message.content, timestamp)
            } else {
                false
            };
            if delivered {
                let id = courier_receipt_id(&envelope.ciphertext);
                let receipt = [COURIER_RECEIPT_MAGIC.as_slice(), &id].concat();
                self.send_private_control(claimed, &receipt).await;
            }
            return;
        }

        // The Noise pin survives expiry of the transient peer record.
        let depositor_key = self.state.with(|state| {
            state
                .peers
                .pinned(claimed)
                .map(|pin| pin.noise_public_key.clone())
        });
        let Some(depositor_key) = depositor_key else {
            tracing::debug!(
                target: "mesh::ble",
                peer = %claimed,
                "courier deposit from an unverified peer refused"
            );
            return;
        };
        if let Err(error) = self.couriers.deposit(&envelope, &depositor_key, now_secs()) {
            // Warn: the depositor believes we are carrying this for them, and we
            // are not. Nothing tells them otherwise.
            tracing::warn!(target: "mesh::ble", %error, "courier deposit refused");
        }
    }
}
