use super::*;

impl BleRuntime {
    pub(super) async fn on_handshake(
        &mut self,
        link: &LinkId,
        packet: &BitchatPacket,
        claimed: &PeerId,
    ) {
        let local = self.identity.peer_id();
        let identity = Arc::clone(&self.identity);
        let step = self.state.with(|state| {
            state
                .sessions
                .on_handshake(claimed, &local, &identity, &packet.payload)
        });

        let step = match step {
            Ok(step) => step,
            Err(error) => {
                // Warn: no session means no direct message with this peer will
                // ever open, and nothing retries on a schedule. Silence here reads
                // as "they never wrote", which is the opposite of what happened.
                tracing::warn!(target: "mesh::ble", peer = %claimed, %error, "handshake failed");
                return;
            }
        };

        if let Some(reply) = step.reply {
            let packet = self.build_packet(
                MessageType::NoiseHandshake,
                Some(claimed),
                reply,
                chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
            );
            self.transmit(&packet, Some(claimed), None).await;
        }

        if step.established.is_some() {
            self.state.with(|state| {
                state.dm_handshake_at_ms.remove(claimed);
                state.chatter_hello.forget_live(claimed);
                state.pending_bindings.remove(claimed);
            });
            // The handshake authenticated `claimed`'s static key (the session
            // manager refuses one that does not derive the ID), and it finished
            // on this link: that is the per-link proof courier handover needs.
            self.state
                .with(|state| state.links.mark_noise_authenticated(link, claimed));
            // The mesh runtime exists only after the user enabled discovery.
            // Keep capability negotiation inside this authenticated Noise link.
            self.send_chatter_hello(claimed).await;
            // A session is the only signal strong enough for the router to send
            // without retaining, so this is the moment retained mail can go.
            let _ = self.router.send(RouterCommand::PeerAvailable {
                peer: claimed.clone(),
            });
            self.flush_pending_dms(claimed).await;
            if let Ok(leaves) = self.private_channels.pending_leaves_for(claimed) {
                for leave in leaves {
                    if let Ok(bytes) = serde_json::to_vec(&leave.notice) {
                        let mut control = b"CHTRPCL2".to_vec();
                        control.extend(bytes);
                        self.send_private_control(claimed, &control).await;
                    }
                }
            }
            if let Ok(states) = self.private_channels.list() {
                for state in states {
                    if state.creator == self.identity.peer_id().as_str()
                        && state
                            .removed_members
                            .iter()
                            .any(|peer| peer == claimed.as_str())
                        && let Ok(bytes) = state.removed_update_bytes(&self.identity)
                    {
                        self.send_group_state(claimed, &bytes, false).await;
                    }
                    if state.creator == self.identity.peer_id().as_str()
                        && state.member_key(claimed).is_some()
                    {
                        for notice in &state.departed {
                            if notice.peer == claimed.as_str() {
                                continue;
                            }
                            if let Ok(bytes) = serde_json::to_vec(notice) {
                                let mut control = b"CHTRPCL2".to_vec();
                                control.extend(bytes);
                                self.send_private_control(claimed, &control).await;
                            }
                        }
                        if let Some(hint) = state.epoch_proof().encode() {
                            self.send_private_control(claimed, &hint).await;
                        }
                        if !state.stale
                            && !state.unanswered_direct_invites.get(claimed.as_str())
                                .is_some_and(|at| now_secs().saturating_sub(*at) >= super::super::DIRECT_INVITE_LIFETIME_SECS)
                            && let Ok(bytes) = state.update_bytes()
                        {
                            self.send_group_state(claimed, &bytes, false).await;
                        }
                    }
                }
            }
            self.emit_peers();
        }
    }

    pub(super) async fn on_encrypted(
        &mut self,
        link: &LinkId,
        packet: &BitchatPacket,
        claimed: &PeerId,
    ) {
        if self.events.peer_blocked(claimed) {
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
                return;
            }
        };

        // The link carried traffic only this peer's session key could produce.
        self.state
            .with(|state| state.links.mark_noise_authenticated(link, claimed));

        if let Some(hello) = ChatterHello::decode(&plaintext) {
            let signal_capable = hello.capabilities & super::super::chatter_hello::SIGNAL_MESH != 0
                && hello.compatible()
                && hello.max_carrier_bytes >= signal_mesh::MAX_CARRIER as u32;
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
            self.state.with(|state| {
                state.chatter_hello.accept(claimed, hello);
            });
            tracing::info!(target: "mesh::ble", peer = %claimed, signal_capable, "Chatter Hello accepted");
            if signal_capable {
                if let Ok(Some((owner, device))) = self.key_store.mesh_local_account_device() {
                    if let Some(static_key) = self
                        .state
                        .with(|state| state.sessions.remote_static(claimed))
                    {
                        let _ = self
                            .binding_store
                            .observe_live_key(&owner, claimed, &static_key);
                    }
                    if matches!(
                        self.binding_store.state(&owner, claimed),
                        Ok(PeerBinding::Verified { .. })
                    ) {
                        match signal_mesh::bundle_for_peer(
                            &self.key_store,
                            &self.binding_store,
                            &owner,
                            claimed,
                            device,
                            now_secs(),
                        ) {
                            Ok(bytes) => self.send_signal_bundle(claimed, &bytes).await,
                            Err(error) => {
                                tracing::warn!(target: "mesh::ble", peer = %claimed, %error,
                                "mesh Signal bundle generation failed")
                            }
                        }
                        if self.binding_store.signal_opted_in(&owner, claimed).ok() == Some(true)
                            && let Ok(pending) = self.binding_store.pending_mesh_carriers(
                                &owner,
                                claimed,
                                now_secs(),
                            )
                        {
                            for bytes in pending {
                                self.send_signal_carrier(claimed, bytes).await;
                            }
                        }
                    }
                }
            }
            self.emit_peers();
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
            if self
                .binding_store
                .observe_live_key(&owner, claimed, &static_key)
                .is_err()
            {
                return;
            }
            let received =
                self.binding_store
                    .receive_mesh_bundle(&owner, claimed, &frame, now_secs());
            tracing::info!(target: "mesh::ble", peer = %claimed, ok = received.is_ok(),
                new = matches!(received, Ok(true)), "mesh Signal bundle received");
            match received {
                Ok(true) if frame.reply_requested => {
                    if let Ok(bytes) = signal_mesh::bundle_for_peer(
                        &self.key_store,
                        &self.binding_store,
                        &owner,
                        claimed,
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
                if self.on_private_leave(claimed, notice) {
                    let mut ack = b"CHTRPLA1".to_vec();
                    if let Ok(uuid) = uuid::Uuid::parse_str(&id) {
                        ack.extend_from_slice(uuid.as_bytes());
                        ack.extend_from_slice(&epoch.to_be_bytes());
                        self.send_private_control(claimed, &ack).await;
                    }
                }
            }
            return;
        }
        if plaintext.len() == 28 && plaintext.starts_with(b"CHTRPLA1") {
            if let Ok(id) = uuid::Uuid::from_slice(&plaintext[8..24]) {
                let epoch = u32::from_be_bytes(plaintext[24..28].try_into().expect("leave epoch"));
                let _ = self
                    .private_channels
                    .clear_pending_leave(&id.to_string(), epoch, claimed);
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
                self.emit_dm(
                    claimed,
                    &message.message_id,
                    &message.content,
                    packet.timestamp,
                );
                self.send_ack(claimed, &message.message_id, NoisePayloadType::Delivered)
                    .await;
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
            || signal_mesh::CarrierFrame::decode(&packet.payload, now_secs()).is_none()
        {
            return false;
        }
        if !self.addressed_to_us(packet) {
            return true;
        }
        let Ok(Some((owner, _))) = self.key_store.mesh_local_account_device() else {
            return false;
        };
        let Ok(Some(signing)) = self.binding_store.verified_signing_key(&owner, claimed) else {
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
        let Some(frame) = signal_mesh::CarrierFrame::decode(bytes, now_secs()) else {
            return false;
        };
        if self.events.peer_blocked(claimed) {
            return false;
        }
        let Ok(Some((owner, local_device))) = self.key_store.mesh_local_account_device() else {
            return false;
        };
        let Ok(Some(_signing)) = self.binding_store.verified_signing_key(&owner, claimed) else {
            return false;
        };
        if self.binding_store.signal_opted_in(&owner, claimed).ok() != Some(true) {
            return false;
        }
        let Ok(PeerBinding::Verified {
            account_id,
            device_id,
            ..
        }) = self.binding_store.state(&owner, claimed)
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
                .abort_mesh_receive(&owner, claimed, &frame.message_id)
                .is_err()
        {
            return false;
        }
        match self
            .binding_store
            .mesh_received(&owner, claimed, &frame.message_id)
        {
            Ok(None) => {}
            Ok(Some(1)) => {
                // A resent ciphertext was already ratcheted. Generate a fresh
                // encrypted receipt without asking libsignal to decrypt it twice.
                let mut store = crate::crypto::signal::SignalStore::for_mesh(
                    self.key_store.clone(),
                    self.binding_store.clone(),
                    owner.clone(),
                    claimed.clone(),
                );
                let receipt_id = uuid::Uuid::new_v4();
                let receipt = signal_mesh::SignalPlaintext {
                    kind: signal_mesh::PlainKind::Delivered,
                    conversation_id: signal_mesh::conversation_id(
                        &self.identity.peer_id(),
                        claimed,
                    ),
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
                .and_then(|frame| frame.encode())
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
                claimed,
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
            claimed.clone(),
        );
        let plaintext = match futures::executor::block_on(signal_mesh::decrypt(
            &mut store, &remote, &local, &frame,
        )) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(target: "mesh::ble", peer=%claimed, %error, "mesh Signal decrypt rejected");
                let _ = self
                    .binding_store
                    .abort_mesh_receive(&owner, claimed, &frame.message_id);
                return false;
            }
        };
        let Some(plain) = signal_mesh::SignalPlaintext::decode(&plaintext, now_secs()) else {
            let _ = self
                .binding_store
                .abort_mesh_receive(&owner, claimed, &frame.message_id);
            return false;
        };
        if plain.conversation_id != signal_mesh::conversation_id(&self.identity.peer_id(), claimed)
            || plain.sender_device != device_id
            || plain.recipient_device != u32::from(local_device)
            || plain.expires_at != frame.expires_at
        {
            let _ = self
                .binding_store
                .abort_mesh_receive(&owner, claimed, &frame.message_id);
            return false;
        }
        if matches!(plain.kind, signal_mesh::PlainKind::Text)
            && (plain.message_id != frame.message_id || plain.text.is_empty())
        {
            let _ = self
                .binding_store
                .abort_mesh_receive(&owner, claimed, &frame.message_id);
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
            .is_some_and(|message| self.events.persist_signal_received(message).is_err())
        {
            let _ = self
                .binding_store
                .abort_mesh_receive(&owner, claimed, &frame.message_id);
            return false;
        }
        let delivery_id = mapping::wire_message_id(&wire_id).to_string();
        let receipt_for = matches!(plain.kind, signal_mesh::PlainKind::Delivered)
            .then_some((&plain.message_id, delivery_id.as_str()));
        let Ok((true, acknowledged)) = self.binding_store.finish_mesh_receive(
            &owner,
            claimed,
            &frame.message_id,
            kind,
            frame.expires_at,
            receipt_for,
        ) else {
            return false;
        };
        if let Some(message) = signal_message {
            self.events.publish_signal_received(message);
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
                .and_then(|frame| frame.encode())
                .ok()
            }
            signal_mesh::PlainKind::Delivered => {
                if acknowledged {
                    self.events.send(MeshEvent::Delivery {
                        message_id: wire_id,
                        peer: Some(claimed.clone()),
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
            peer: peer.clone(),
            kind,
        });
    }

    pub(super) async fn on_courier_envelope(&mut self, packet: &BitchatPacket, claimed: &PeerId) {
        let Ok(envelope) = CourierEnvelope::decode(&packet.payload) else {
            return;
        };

        // Is it ours? Only the seal can answer that — the recipient tag rotates
        // and is a hint, not proof.
        if let Some((plaintext, sender_key)) = super::super::prekeys::open_envelope(
            &self.state,
            &envelope,
            &self.identity,
            now_millis(),
        ) {
            if let Some(carrier) = plaintext.strip_prefix(b"CSM1") {
                let sender = PeerId::from_noise_key(&sender_key);
                if let Ok(Some((owner, _))) = self.key_store.mesh_local_account_device()
                    && self.binding_store.verified_noise_key(&owner, &sender).ok()
                        == Some(Some(sender_key))
                {
                    self.process_signal_carrier(carrier, &sender, packet.timestamp, false)
                        .await;
                }
                return;
            }
            if let Some((NoisePayloadType::PrivateMessage, body)) = decode_noise_payload(&plaintext)
                && let Ok(message) = PrivateMessagePacket::decode(body)
            {
                let sender = PeerId::from_noise_key(&sender_key);
                self.emit_dm(
                    &sender,
                    &message.message_id,
                    &message.content,
                    packet.timestamp,
                );
            }
            return;
        }

        // Not ours: carry it, if the depositor is signature-verified.
        let snapshot = self.state.with(|state| {
            state
                .peers
                .info(claimed)
                .map(|record| (record.noise_public_key.clone(), record.verified_nickname))
        });
        let Some((Some(depositor_key), verified)) = snapshot else {
            return;
        };
        if !verified {
            // An unverified peer may not deposit mail: without a checked
            // signature the pool is a free amplifier. Said at debug rather than
            // warn only because an attacker controls how often it happens — but
            // said, because a silent return here is indistinguishable from mail
            // that was never sent.
            tracing::debug!(
                target: "mesh::ble",
                peer = %claimed,
                "courier deposit from an unverified peer refused"
            );
            return;
        }
        if let Err(error) = self.couriers.deposit(&envelope, &depositor_key, now_secs()) {
            // Warn: the depositor believes we are carrying this for them, and we
            // are not. Nothing tells them otherwise.
            tracing::warn!(target: "mesh::ble", %error, "courier deposit refused");
        }
    }
}
