use super::*;

impl BleRuntime {
    pub(super) async fn send_private(&mut self, peer: &PeerId, message_id: &str, content: &str) {
        let Ok(tlv) =
            PrivateMessagePacket::new(message_id.to_string(), content.to_string()).encode()
        else {
            return;
        };
        let payload = encode_noise_payload(NoisePayloadType::PrivateMessage, &tlv);

        let sealed = self
            .state
            .with(|state| state.sessions.encrypt(peer, &payload));
        match sealed {
            Ok(ciphertext) => {
                let packet = self.build_packet(
                    MessageType::NoiseEncrypted,
                    Some(peer),
                    ciphertext,
                    chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
                );
                self.transmit(&packet, Some(peer), None).await;
                self.rekey_if_due(peer).await;
            }
            Err(_) => {
                // No session yet. Hold the message and start a handshake: the
                // router has already retained a copy, so nothing is lost if the
                // handshake never completes.
                let first = self.state.with(|state| {
                    let pending = state.pending_dms.entry(peer.clone()).or_default();
                    let first = pending.is_empty();
                    if !pending
                        .iter()
                        .any(|message| message.message_id == message_id)
                    {
                        pending.push(super::super::state::PendingDm {
                            message_id: message_id.to_string(),
                            content: content.to_string(),
                            queued_at_ms: now_millis(),
                        });
                    }
                    first
                });
                if first {
                    self.start_handshake(peer).await;
                }
            }
        }
    }

    /// Renegotiate a session that has spent 90% of its nonce budget (before
    /// encryption hard-fails at the limit) or was just used after more than
    /// 24 h idle — both halves of bitchat's `needsRenegotiation()`. The old
    /// session keeps carrying traffic until the new handshake completes and
    /// replaces it; the peer treats our opening message as a restart and does
    /// the same.
    async fn rekey_if_due(&mut self, peer: &PeerId) {
        if self.state.with(|state| state.sessions.needs_rekey(peer)) {
            tracing::info!(target: "mesh::ble", %peer, "session due for renegotiation; rekeying");
            self.start_handshake(peer).await;
        }
    }

    pub(super) async fn start_handshake(&mut self, peer: &PeerId) {
        let identity = Arc::clone(&self.identity);
        let opening = self
            .state
            .with(|state| state.sessions.initiate(peer, &identity));
        let Ok(opening) = opening else {
            return;
        };
        self.state.with(|state| {
            state.dm_handshake_at_ms.insert(peer.clone(), now_millis());
        });
        let packet = self.build_packet(
            MessageType::NoiseHandshake,
            Some(peer),
            opening,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        self.transmit(&packet, Some(peer), None).await;
    }

    pub(super) async fn flush_pending_dms(&mut self, peer: &PeerId) {
        let pending = self
            .state
            .with(|state| state.pending_dms.remove(peer).unwrap_or_default());
        for message in pending {
            self.send_private(peer, &message.message_id, &message.content)
                .await;
        }
    }

    pub(super) async fn retry_pending_dm_handshake(&mut self, peer: &PeerId) {
        let now = now_millis();
        let due = self.state.with(|state| {
            !state.sessions.has_session(peer)
                && state
                    .pending_dms
                    .get(peer)
                    .is_some_and(|pending| !pending.is_empty())
                && state
                    .links
                    .links_to(peer)
                    .iter()
                    .any(|link| state.links.is_writable(link))
                && now.saturating_sub(*state.dm_handshake_at_ms.get(peer).unwrap_or(&0))
                    >= DM_HANDSHAKE_RETRY_MS
        });
        if due {
            tracing::info!(target: "mesh::ble", %peer, "retrying DM Noise handshake");
            self.start_handshake(peer).await;
        }
    }

    pub(super) async fn send_ack(
        &mut self,
        peer: &PeerId,
        message_id: &str,
        payload_type: NoisePayloadType,
    ) {
        let payload = encode_noise_payload(payload_type, message_id.as_bytes());
        let sealed = self
            .state
            .with(|state| state.sessions.encrypt(peer, &payload));
        let Ok(ciphertext) = sealed else {
            return;
        };
        let packet = self.build_packet(
            MessageType::NoiseEncrypted,
            Some(peer),
            ciphertext,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        self.transmit(&packet, Some(peer), None).await;
        self.rekey_if_due(peer).await;
    }

    pub(super) async fn send_signal_bundle(&mut self, peer: &PeerId, bytes: &[u8]) {
        let sealed = self.state.with(|state| state.sessions.encrypt(peer, bytes));
        let Ok(ciphertext) = sealed else {
            // Sessions open lazily. The handshake's Chatter Hello re-offers
            // the bundle for a verified peer, so opening one is enough.
            tracing::info!(target: "mesh::ble", %peer, "mesh Signal bundle waits for a Noise session");
            self.start_handshake(peer).await;
            return;
        };
        tracing::info!(target: "mesh::ble", %peer, bytes = bytes.len(), "mesh Signal bundle sent");
        let packet = self.build_packet(
            MessageType::NoiseEncrypted,
            Some(peer),
            ciphertext,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        self.transmit(&packet, Some(peer), None).await;
    }

    pub(super) async fn send_signal_carrier(&mut self, peer: &PeerId, bytes: Vec<u8>) {
        let Some(frame) = signal_mesh::CarrierFrame::decode(&bytes, now_secs()) else {
            return;
        };
        let Ok(Some((owner, _))) = self.key_store.mesh_local_account_device() else {
            return;
        };
        if !matches!(
            self.binding_store.state(&owner, peer),
            Ok(PeerBinding::Verified { .. })
        ) || self.binding_store.signal_opted_in(&owner, peer).ok() != Some(true)
        {
            return;
        }
        // Every offline copy contains the complete E4 carrier. Couriers only
        // learn the rotating recipient tag and a bounded sealed ciphertext.
        let compatible = self.state.with(|state| {
            state.chatter_hello.get(peer).is_some_and(|hello| {
                hello.compatible()
                    && hello.capabilities & super::super::chatter_hello::SIGNAL_MESH != 0
                    && hello.max_carrier_bytes >= signal_mesh::MAX_CARRIER as u32
            })
        });
        if !self.state.can_write_to(peer) || !compatible {
            if self
                .binding_store
                .queue_mesh_carrier(&owner, peer, &frame)
                .is_err()
            {
                return;
            }
            self.courier_signal_carrier(peer, &bytes, frame.message_id, frame.expires_at)
                .await;
            // In range but no Chatter Hello yet (sessions open lazily, e.g.
            // after a restart): open one. The hello flushes queued carriers.
            if self.state.can_write_to(peer)
                && !self.state.with(|state| state.sessions.has_session(peer))
            {
                self.start_handshake(peer).await;
            }
            return;
        }
        let mut packet = self.build_packet(
            MessageType::ChatterCarrier,
            Some(peer),
            bytes,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        let Ok(signature) = sign_packet(&self.identity, &packet) else {
            return;
        };
        packet.signature = Some(signature);
        if self.transmit(&packet, Some(peer), None).await {
            self.events.send(MeshEvent::Delivery {
                message_id: uuid::Uuid::from_bytes(frame.message_id)
                    .to_string()
                    .to_uppercase(),
                peer: Some(peer.clone()),
                state: super::super::events::MeshDelivery::Secure,
            });
        }
    }

    async fn courier_signal_carrier(
        &mut self,
        recipient: &PeerId,
        bytes: &[u8],
        id: [u8; 16],
        expiry: u64,
    ) {
        use chatter_mesh::tlv::courier_envelope::{epoch_day, recipient_tag};
        use chatter_mesh_store::courier::INITIAL_COPIES;
        let Ok(Some((owner, _))) = self.key_store.mesh_local_account_device() else {
            return;
        };
        let Ok(Some(recipient_key)) = self.binding_store.verified_noise_key(&owner, recipient)
        else {
            return;
        };
        let mut payload = Vec::with_capacity(4 + bytes.len());
        payload.extend_from_slice(b"CSM1");
        payload.extend_from_slice(bytes);
        let Some((sealed, prekey_id)) = super::super::prekeys::seal_offline(
            &self.state,
            &payload,
            &recipient_key,
            &self.identity,
            now_millis(),
        ) else {
            return;
        };
        let envelope = CourierEnvelope::new(
            recipient_tag(&recipient_key, epoch_day(now_secs())),
            expiry.saturating_mul(1_000),
            sealed,
            1,
            prekey_id,
        );
        if envelope.encode().is_err() {
            return;
        }
        let candidates: Vec<_> = self
            .state
            .peer_snapshots()
            .into_iter()
            .filter(|p| {
                p.peer != *recipient
                    && p.is_verified
                    && p.is_connected
                    && self.state.can_write_to(&p.peer)
            })
            .take(usize::from(INITIAL_COPIES))
            .map(|p| p.peer)
            .collect();
        let mut handed = false;
        for courier in candidates {
            if self
                .binding_store
                .reserve_mesh_courier(&owner, recipient, &id, &courier)
                .ok()
                != Some(true)
            {
                continue;
            }
            let Ok(payload) = envelope.encode() else {
                break;
            };
            let packet = self.build_packet(
                MessageType::CourierEnvelope,
                Some(&courier),
                payload,
                chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
            );
            if self.transmit(&packet, Some(&courier), None).await {
                handed = true;
            } else {
                let _ = self
                    .binding_store
                    .release_mesh_courier(&owner, recipient, &id, &courier);
            }
        }
        if handed {
            self.events.send(MeshEvent::Delivery {
                message_id: uuid::Uuid::from_bytes(id).to_string().to_uppercase(),
                peer: Some(recipient.clone()),
                state: super::super::events::MeshDelivery::SignalRelayed,
            });
        }
    }

    pub(super) async fn send_chatter_hello(&mut self, peer: &PeerId) {
        let mut nonce = [0u8; 16];
        if getrandom::fill(&mut nonce).is_err() {
            return;
        }
        self.state
            .with(|state| state.chatter_hello.set_local_nonce(peer, nonce));
        let Some(payload) = ChatterHello::new(nonce).encode() else {
            return;
        };
        let sealed = self
            .state
            .with(|state| state.sessions.encrypt(peer, &payload));
        let Ok(ciphertext) = sealed else {
            return;
        };
        let packet = self.build_packet(
            MessageType::NoiseEncrypted,
            Some(peer),
            ciphertext,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        self.transmit(&packet, Some(peer), None).await;
    }
}
