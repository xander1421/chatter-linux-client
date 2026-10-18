use super::*;

impl BleRuntime {
    /// Broadcast a public message into the mesh room.
    pub async fn broadcast_public(&mut self, message_id: &str, content: &str) {
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
        let Some(payload) = chatter_mesh::channel::encode_presence(name) else {
            return;
        };
        let packet = self.build_packet(
            MessageType::ChannelMessage,
            None,
            payload,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        let packet = self.maybe_sign(packet, true);
        self.originate_broadcast(&packet).await;
    }

    pub(super) async fn send_private_control(&mut self, peer: &PeerId, bytes: &[u8]) {
        let Ok(ciphertext) = self.state.with(|s| s.sessions.encrypt(peer, bytes)) else {
            // A current member retries when this peer establishes Noise again.
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

    pub(super) async fn broadcast_private_channel(&mut self, id: &str, bytes: Vec<u8>) {
        let Ok(Some(state)) = self.private_channels.load(id) else {
            return;
        };
        if state.stale {
            return;
        }
        let packet_sender = self.identity.peer_id();
        let Some((message_id, content)) = state.decrypt(&packet_sender, &bytes) else {
            return;
        };
        let sender = packet_sender.clone();
        let packet = self.build_packet(
            MessageType::ChatterPrivateChannel,
            None,
            bytes,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        let packet = self.maybe_sign(packet, true);
        self.state
            .with(|s| s.self_broadcasts.record(&message_id, &packet, now_millis()));
        self.originate_broadcast(&packet).await;
        let conversation = MeshConversation::PrivateChannel(id.to_owned());
        let nickname = if sender == packet_sender {
            self.config.nickname()
        } else {
            self.state
                .with(|s| s.peers.info(&sender).map(|p| p.nickname.clone()))
                .unwrap_or_else(|| sender.as_str().to_owned())
        };
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message: outbound_message(&conversation, &message_id, &sender, &content, now_millis()),
            conversation,
            is_local: sender == packet_sender,
            sender,
            nickname,
            sender_verified: true,
        })));
    }
}
