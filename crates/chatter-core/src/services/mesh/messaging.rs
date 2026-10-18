use super::*;

impl MeshService {
    /// Publish into a geohash channel.
    pub fn send_geohash(&self, cell: &str, text: &str) -> Result<String, MeshError> {
        let content = Self::require_content(text)?;
        if !chatter_mesh_nostr::geohash::is_valid(cell) {
            return Err(MeshError::InvalidGeohash(cell.to_string()));
        }
        if self.relay_connector.is_none() {
            return Err(MeshError::NoRelayConnector);
        }
        let message_id = new_message_id();
        self.nostr
            .send(NostrCommand::GeohashMessage {
                cell: cell.to_string(),
                message_id: message_id.clone(),
                content,
                nickname: self.config.nickname(),
            })
            .map_err(|_| MeshError::WorkerGone)?;
        self.timeline
            .record_delivery(&message_id, MeshDelivery::Broadcast);
        Ok(message_id)
    }

    /// Send a private message, letting the router choose the transport.
    ///
    /// The returned [`MeshDelivery`] is the branch the router took, which is the
    /// whole decision the UI needs. Every DM remains in the outbox until the
    /// recipient acknowledges it, including one queued to a secure BLE link.
    pub async fn send_dm(&self, peer: &PeerId, text: &str) -> Result<MeshDelivery, MeshError> {
        if self.events.peer_blocked(peer) {
            return Err(MeshError::OutOfRange("peer is locally blocked"));
        }
        let content = Self::require_content(text)?;
        if !self.can_route_to(peer) {
            return Err(MeshError::Unroutable(peer.as_str().to_string()));
        }
        let message_id = new_message_id();
        let nickname = self.nickname_for(peer);

        // Mirror it into the timeline before routing. The mesh has no server
        // echo, so this is the only copy — and it must exist whichever transport
        // the router picks, which is why it is here and not in a runtime.
        //
        // The send goes through [`MeshEvents`], so this both stores the row and
        // publishes it: the message is on disk before this call returns, and a
        // restart mid-delivery still shows what the user sent.
        let conversation = MeshConversation::Dm(peer.clone());
        let message = mapping::outbound_message(
            &conversation,
            &message_id,
            &self.identity.peer_id(),
            &content,
            now_millis(),
        );
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message,
            conversation,
            sender: self.identity.peer_id(),
            nickname: self.config.nickname(),
            is_local: true,
            sender_verified: true,
        })));

        let outcome = self
            .router
            .send_private(content, peer.clone(), nickname, message_id.clone())
            .await?;

        let delivery = match outcome.link {
            LinkState::Secure => MeshDelivery::Secure,
            LinkState::Insecure => MeshDelivery::Insecure,
            LinkState::Reachable => MeshDelivery::Reachable,
            LinkState::Unreachable => MeshDelivery::Unreachable,
        };
        // Persisted here rather than left to the router's own `Delivery` event.
        // That event is the right vehicle for every *later* transition, but it
        // travels through an actor, so relying on it alone leaves a window in
        // which the message is on disk with no state — and a message with no
        // recorded state renders as an ordinary "sent". The outcome is already
        // in hand at this point; storing it is not a guess.
        self.timeline.record_delivery(&message_id, delivery);
        Ok(delivery)
    }

    /// Send a text-only DM over an independent, verified mesh Signal ratchet.
    /// A bounded carrier is queued locally and retried on the next compatible
    /// BLE link. It never touches Tor-only secret chats or the Noise DM router.
    pub async fn send_signal_dm(
        &self,
        owner: UserId,
        local_device_id: u32,
        peer: &PeerId,
        text: &str,
    ) -> Result<MeshDelivery, MeshError> {
        use crate::crypto::signal::SessionManager;
        use libsignal_protocol::{DeviceId, ProtocolAddress};
        use rand::TryRngCore;

        if self.events.peer_blocked(peer) {
            return Err(MeshError::OutOfRange("peer is locally blocked"));
        }
        let content = Self::require_content(text)?;
        if content.len() > signal_mesh::MAX_TEXT {
            return Err(MeshError::OutOfRange("mesh Signal text exceeds 2048 bytes"));
        }
        if !self.has_radio {
            return Err(MeshError::NoRadio);
        }
        let _guard = self.signal_lock.lock();
        if !self
            .binding_store
            .signal_opted_in(&owner.to_string(), peer)?
        {
            return Err(MeshError::Binding(
                "enable Signal for this mesh conversation first".into(),
            ));
        }
        let (mut store, remote) = self.verified_signal_target(owner, peer)?;
        let local = ProtocolAddress::new(
            owner.to_string(),
            DeviceId::try_from(local_device_id)
                .map_err(|_| MeshError::Binding("invalid local device ID".into()))?,
        );
        if !futures::executor::block_on(SessionManager::has_session(&store, &remote))
            .map_err(|e| MeshError::Binding(format!("mesh Signal session check failed: {e}")))?
        {
            let Some(proto) =
                self.binding_store
                    .take_remote_mesh_bundle(&owner.to_string(), peer, now_secs())?
            else {
                // Offer ours (opening a Noise session if needed); the peer's
                // reply carries the bundle the next send will use.
                if let Ok(bytes) = signal_mesh::bundle_for_peer(
                    &self.key_store,
                    &self.binding_store,
                    &owner.to_string(),
                    peer,
                    local_device_id,
                    now_secs(),
                ) {
                    let _ = self.ble.send(BleCommand::SignalBundle {
                        peer: peer.clone(),
                        bytes,
                    });
                }
                return Err(MeshError::Binding(
                    "waiting for this verified peer's live PQXDH bundle".into(),
                ));
            };
            let bundle = crate::services::signal::SignalService::convert_prekey_bundle(proto)
                .map_err(|e| MeshError::Binding(format!("invalid mesh Signal bundle: {e}")))?;
            let mut identities = store.clone();
            let mut rng = rand::rngs::OsRng.unwrap_err();
            futures::executor::block_on(SessionManager::establish_session(
                &remote,
                &local,
                &mut store,
                &mut identities,
                &bundle,
                &mut rng,
            ))
            .map_err(|e| MeshError::Binding(format!("mesh PQXDH failed: {e}")))?;
        }
        let id = Uuid::new_v4();
        let message_id = id.to_string().to_uppercase();
        let plaintext = signal_mesh::SignalPlaintext {
            kind: signal_mesh::PlainKind::Text,
            conversation_id: signal_mesh::conversation_id(&self.identity.peer_id(), peer),
            message_id: *id.as_bytes(),
            sender_device: local_device_id,
            recipient_device: remote.device_id().into(),
            expires_at: now_secs().saturating_add(24 * 3600),
            text: content.clone(),
        };
        let frame = futures::executor::block_on(signal_mesh::encrypt(
            &mut store,
            &remote,
            &local,
            &plaintext,
            *id.as_bytes(),
        ))?;
        let bytes = self
            .binding_store
            .queue_mesh_carrier(&owner.to_string(), peer, &frame)?;
        let delivery = MeshDelivery::SignalQueued;
        let conversation = MeshConversation::Dm(peer.clone());
        let message = mapping::outbound_message(
            &conversation,
            &message_id,
            &self.identity.peer_id(),
            &content,
            now_millis(),
        );
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message,
            conversation,
            sender: self.identity.peer_id(),
            nickname: self.config.nickname(),
            is_local: true,
            sender_verified: true,
        })));
        self.events.send(MeshEvent::Delivery {
            message_id,
            peer: Some(peer.clone()),
            state: delivery,
        });
        // Submission happens after the durable queued row and its UI event.
        // The runtime reports a courier handoff separately; only the
        // encrypted Signal receipt can advance this row to Delivered.
        let _ = self.ble.send(BleCommand::SignalCarrier {
            peer: peer.clone(),
            bytes,
        });
        Ok(delivery)
    }

    /// Acknowledge that a message has been read.
    ///
    /// Routed over the same first-reachable rule the router uses, because a
    /// receipt that goes out on a transport the peer is not on leaves their
    /// message unread forever.
    pub fn mark_read(&self, peer: &PeerId, message_id: &str) -> Result<(), MeshError> {
        if self.state.is_connected(peer) || self.state.is_reachable(peer) {
            self.ble
                .send(BleCommand::Ack {
                    peer: peer.clone(),
                    message_id: message_id.to_string(),
                    payload_type: NoisePayloadType::ReadReceipt,
                })
                .map_err(|_| MeshError::WorkerGone)
        } else if self.nostr_state.knows_peer(peer) {
            self.nostr
                .send(NostrCommand::Ack {
                    peer: peer.clone(),
                    message_id: message_id.to_string(),
                    payload_type: NoisePayloadType::ReadReceipt,
                })
                .map_err(|_| MeshError::WorkerGone)
        } else {
            Err(MeshError::Unroutable(peer.as_str().to_string()))
        }
    }
}
