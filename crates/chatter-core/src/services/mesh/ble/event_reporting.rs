use super::*;

impl BleRuntime {
    // ----------------------------------------------------------------- helpers

    pub(super) async fn hand_over_courier_mail(
        &mut self,
        peer: &PeerId,
        recipient_key: &[u8; 32],
        now_s: u64,
        ingress: AnnounceIngress,
    ) {
        let envelopes = self
            .couriers
            .take_envelopes(recipient_key, now_s, ingress)
            .unwrap_or_default();
        for envelope in envelopes {
            let Ok(payload) = envelope.encode() else {
                continue;
            };
            let packet = self.build_packet(
                MessageType::CourierEnvelope,
                Some(peer),
                payload,
                chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
            );
            self.transmit(&packet, Some(peer), None).await;
        }
    }

    pub(super) async fn dispatch_sync(&self, send: chatter_mesh_store::sync::SyncSend) {
        let packet = self.maybe_sign(send.packet, send.sign_before_send);
        self.transmit(&packet, send.to.as_ref(), None).await;
    }

    pub(super) fn emit_dm(
        &self,
        peer: &PeerId,
        message_id: &str,
        content: &str,
        timestamp_ms: u64,
    ) {
        let nickname = self
            .state
            .with(|state| state.peers.nickname(peer, false).map(str::to_string))
            .unwrap_or_else(|| peer.as_str().to_string());
        let verified = self
            .state
            .with(|state| state.peers.info(peer).is_some_and(|p| p.verified_nickname));
        let inbound = InboundMessage {
            wire_id: message_id.to_string(),
            conversation: MeshConversation::Dm(peer.clone()),
            sender: peer.clone(),
            nickname: nickname.clone(),
            content: content.to_string(),
            timestamp_ms,
        };
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message: inbound.to_domain(),
            conversation: inbound.conversation,
            sender: peer.clone(),
            nickname,
            is_local: false,
            sender_verified: verified,
        })));
    }

    pub(super) fn signal_dm_message(
        &self,
        peer: &PeerId,
        message_id: &str,
        content: &str,
        timestamp_ms: u64,
    ) -> MeshMessage {
        let nickname = self
            .state
            .with(|state| state.peers.nickname(peer, false).map(str::to_string))
            .unwrap_or_else(|| peer.as_str().to_string());
        let inbound = InboundMessage {
            wire_id: message_id.to_string(),
            conversation: MeshConversation::Dm(peer.clone()),
            sender: peer.clone(),
            nickname: nickname.clone(),
            content: content.to_string(),
            timestamp_ms,
        };
        MeshMessage {
            message: inbound.to_domain(),
            conversation: inbound.conversation,
            sender: peer.clone(),
            nickname,
            is_local: false,
            sender_verified: true,
        }
    }

    pub(super) fn addressed_to_us(&self, packet: &BitchatPacket) -> bool {
        let local = self.identity.peer_id().to_routing_bytes();
        match (packet.recipient_id, local) {
            (Some(recipient), Some(local)) => recipient == local,
            _ => false,
        }
    }

    pub(super) fn reconcile_peers(&mut self) {
        let now = now_millis();
        let removed = self.state.with(|state| {
            let presence = state.links.presence_by_peer();
            let changes = state.peers.reconcile_connectivity(now, &presence);
            for peer in &changes.disconnected {
                state.sessions.forget(peer);
            }
            changes
        });
        for removed in removed.removed {
            self.gossip.remove_announcement_for_peer(&removed.peer);
        }
        self.emit_peers();
    }

    pub(super) fn emit_peers(&self) {
        self.events.send(MeshEvent::Peers(self.state.peers()));
    }

    pub(super) fn emit_status(&self) {
        let status = self.state.status(self.nostr_state.connected_relay_count());
        self.events.send(MeshEvent::Status(status));
    }

    /// Report something that went wrong without stopping the mesh.
    ///
    /// Warn, not debug. Every message that reaches here is an abnormal condition
    /// already judged worth telling the user about, so it is worth telling a log
    /// reader too — and the Android build emits info and above, which meant the
    /// most detailed explanation of a failure was invisible on the device where
    /// the failure was being reported. "the radio would not start: …" was written
    /// at debug and read by nobody.
    pub(super) fn degraded(&self, message: String) {
        tracing::warn!(target: "mesh::ble", "{message}");
        self.events.send(MeshEvent::Degraded(message));
    }
}
