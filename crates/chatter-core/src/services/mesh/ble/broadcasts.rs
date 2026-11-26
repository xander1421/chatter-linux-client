use super::*;

fn public_chat_type(kind: Option<MessageType>) -> bool {
    matches!(
        kind,
        Some(
            MessageType::Message
                | MessageType::ChannelMessage
                | MessageType::FileTransfer
                | MessageType::FileManifest
                | MessageType::FileChunk
                | MessageType::FileChunkRequest
        )
    )
}

pub(super) fn packet_kind(packet: &BitchatPacket) -> Option<MessageType> {
    match packet.message_type() {
        Some(MessageType::Fragment) => {
            let (header, _) = fragment::FragmentHeader::parse(packet).ok()?;
            MessageType::from_u8(header.original_type)
        }
        kind => kind,
    }
}

pub(in crate::services::mesh) fn frame_kind(bytes: &[u8]) -> Option<MessageType> {
    match bytes.get(1).copied().and_then(MessageType::from_u8) {
        Some(MessageType::Fragment) => packet_kind(&codec::decode(bytes).ok()?),
        kind => kind,
    }
}

pub(super) fn allows_transmission(config: &MeshConfig, packet: &BitchatPacket) -> bool {
    if !allows_public_packet(config, packet) {
        return false;
    }
    if config.bridge_mode_enabled() {
        return true;
    }
    match (packet.message_type(), packet_kind(packet)) {
        (Some(MessageType::Announce), _) => {
            matches!(
                AnnouncementPacket::carries_bridge_metadata(&packet.payload),
                Ok(false)
            )
        }
        // A fragment alone cannot establish that the announce has no metadata.
        (Some(MessageType::Fragment), Some(MessageType::Announce)) => false,
        _ => true,
    }
}

pub(super) fn allows_forwarding(config: &MeshConfig, packet: &BitchatPacket) -> bool {
    allows_transmission(config, packet)
        && (packet_kind(packet) != Some(MessageType::Announce)
            || (packet.message_type() == Some(MessageType::Announce)
                && matches!(
                    AnnouncementPacket::carries_pairing_beacon(&packet.payload),
                    Ok(false)
                )))
}

fn allows_public_packet(config: &MeshConfig, packet: &BitchatPacket) -> bool {
    config.public_chat_enabled() || !public_chat_type(packet_kind(packet))
}

impl BleRuntime {
    pub(super) fn allows_link_frame(&self, link: &LinkId, kind: Option<MessageType>) -> bool {
        self.config.public_chat_enabled()
            || self.state.is_paired_link(link)
            || (matches!(
                kind,
                Some(
                    MessageType::Announce
                        | MessageType::NoiseHandshake
                        | MessageType::NoiseEncrypted
                )
            ) && self
                .link_admission
                .get(link)
                .is_some_and(|record| record.permits_proof(&self.config, Instant::now())))
    }

    pub(super) fn allows_public_packet(&self, packet: &BitchatPacket) -> bool {
        allows_public_packet(&self.config, packet)
    }

    pub(super) fn allows_queued_frame(&self, bytes: &[u8]) -> bool {
        self.config.public_chat_enabled() || !public_chat_type(frame_kind(bytes))
    }

    /// Broadcast a public message into the mesh room.
    pub async fn broadcast_public(&mut self, message_id: &str, content: &str) {
        if !self.config.public_chat_enabled() {
            return;
        }
        let now_ms = now_millis();
        let nickname = self.config.nickname();
        // The payload of a public message is the message text and nothing else
        // (`BLEService.swift:504`: `payload: Data(content.utf8)`). There is no
        // room on the wire for a message ID, a timestamp or a sender name:
        // receivers take the sender from the packet header, the time from
        // `packet.timestamp`, and the name from the announce registry. bitchat's
        // `BitchatMessage.toBinaryPayload()` encodes all of those, and is dead
        // code outside its own tests.
        let packet = self.build_packet(
            MessageType::Message,
            None,
            content.as_bytes().to_vec(),
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        // Signed, because the receiver requires it: an unsigned public message
        // is dropped before its payload is even decoded
        // (`BLEPublicMessageHandler.swift:91`).
        let packet = self.maybe_sign(packet, true);
        // Remember it so the copy that floods back to us is recognised as our
        // own rather than displayed twice.
        self.state
            .with(|state| state.self_broadcasts.record(message_id, &packet, now_ms));
        self.originate_broadcast(&packet).await;

        let local = self.identity.peer_id();
        let message =
            outbound_message(&MeshConversation::Room, message_id, &local, content, now_ms);
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message,
            conversation: MeshConversation::Room,
            sender: local,
            nickname,
            is_local: true,
            sender_verified: true,
        })));
    }

    pub(super) async fn broadcast_channel(&mut self, message_id: &str, name: &str, content: &str) {
        if !self.config.public_chat_enabled() {
            return;
        }
        let Some(payload) = chatter_mesh::channel::encode(name, content) else {
            return;
        };
        let now_ms = now_millis();
        let packet = self.build_packet(
            MessageType::ChannelMessage,
            None,
            payload,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        let packet = self.maybe_sign(packet, true);
        self.state
            .with(|state| state.self_broadcasts.record(message_id, &packet, now_ms));
        self.originate_broadcast(&packet).await;
        let conversation = MeshConversation::Channel(name.to_owned());
        let sender = self.identity.peer_id();
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message: outbound_message(&conversation, message_id, &sender, content, now_ms),
            conversation,
            sender,
            nickname: self.config.nickname(),
            is_local: true,
            sender_verified: false,
        })));
    }

    pub(super) async fn broadcast_channel_presence(&mut self, name: &str) {
        if !self.config.public_chat_enabled() {
            return;
        }
        let Some(payload) = chatter_mesh::channel::encode_presence(name) else {
            return;
        };
        let mut packet = self.build_packet(
            MessageType::ChannelMessage,
            None,
            payload,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        packet.timestamp = super::transmission::presence_timestamp(packet.timestamp);
        let packet = self.maybe_sign(packet, true);
        self.originate_broadcast(&packet).await;
    }

    pub(super) async fn send_private_control(&self, peer: &PeerId, bytes: &[u8]) -> bool {
        if !self.config.public_chat_enabled()
            && !self.state.paired_session(peer)
            && !self.allows_pairing_control(peer, bytes)
            && !bytes.starts_with(super::super::account_identity::PAIRING_RECEIPT_MAGIC)
        {
            return false;
        }
        let Ok(ciphertext) = self.state.with(|s| s.sessions.encrypt(peer, bytes)) else {
            // A current member retries when this peer establishes Noise again.
            return false;
        };
        let packet = self.build_packet(
            MessageType::NoiseEncrypted,
            Some(peer),
            ciphertext,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        self.transmit(&packet, Some(peer), None).await
    }

    pub(super) async fn send_group_control(&self, peer: &PeerId, bytes: &[u8]) -> bool {
        self.pairing_verified(peer) && self.send_private_control(peer, bytes).await
    }

    pub(super) async fn send_group_state(&mut self, peer: &PeerId, bytes: &[u8], invite: bool) {
        let Some(wire) = self.state.wire_for_group_peer(peer) else {
            return;
        };
        let peer = &wire;
        if !self.config.public_chat_enabled() && !self.state.paired_session(peer) {
            return;
        }
        let kind = if invite {
            NoisePayloadType::GroupInvite
        } else {
            NoisePayloadType::GroupKeyUpdate
        };
        let payload = chatter_mesh::packet::encode_noise_payload(kind, bytes);
        let sealed = self.pairing_verified(peer).then(|| {
            self.state
                .with(|state| state.sessions.encrypt(peer, &payload))
        });
        let Some(Ok(ciphertext)) = sealed else {
            let now = now_millis();
            let due = self.state.with(|state| {
                state
                    .links
                    .links_to(peer)
                    .iter()
                    .any(|link| state.links.is_writable(link))
                    && !state.sessions.has_session(peer)
                    && now.saturating_sub(*state.dm_handshake_at_ms.get(peer).unwrap_or(&0))
                        >= DM_HANDSHAKE_RETRY_MS
            });
            if due {
                self.start_handshake(peer).await;
            }
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

    pub(super) async fn broadcast_private_channel(
        &mut self,
        id: &str,
        bytes: Vec<u8>,
        timestamp_ms: u64,
    ) {
        let Ok(Some(state)) = self.private_channels.load(id) else {
            return;
        };
        if state.stale {
            return;
        }
        let local_group = self.private_group_identity.peer_id();
        let Some(opened) = state.decrypt(&bytes, timestamp_ms) else {
            return;
        };
        let message_id = opened.id;
        let content = opened.content;
        let sender = opened.sender;
        let mut packet = self.build_packet(MessageType::GroupMessage, None, bytes, 7);
        packet.timestamp = timestamp_ms;
        self.state
            .with(|s| s.self_broadcasts.record(&message_id, &packet, now_millis()));
        self.originate_broadcast(&packet).await;
        let conversation = MeshConversation::PrivateChannel(id.to_owned());
        let nickname = if sender == local_group {
            self.config.nickname()
        } else {
            self.state
                .with(|s| s.peers.info(&sender).map(|p| p.nickname.clone()))
                .unwrap_or_else(|| sender.as_str().to_owned())
        };
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message: outbound_message(&conversation, &message_id, &sender, &content, timestamp_ms),
            conversation,
            is_local: sender == local_group,
            sender,
            nickname,
            sender_verified: true,
        })));
    }
}
