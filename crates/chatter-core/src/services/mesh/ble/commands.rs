use super::*;

impl BleRuntime {
    pub(super) async fn on_request_sync(
        &mut self,
        link: &LinkId,
        packet: &BitchatPacket,
        claimed: &PeerId,
    ) {
        let (announced, authenticated) = self.state.with(|state| {
            let announced = (state.links.peer_for(link) == Some(claimed)
                || state.links.discovery_peer_for(link) == Some(claimed))
                && state
                    .peers
                    .info(claimed)
                    .is_some_and(|peer| peer.signing_public_key.is_some());
            (announced, state.links.is_noise_authenticated(link, claimed))
        });
        if !self.config.public_chat_enabled()
            && (!authenticated || !self.state.paired_session(claimed))
        {
            return;
        }
        if !announced && !authenticated {
            return;
        }
        let Ok(request) = RequestSyncPacket::decode(&packet.payload) else {
            return;
        };

        let sends = self
            .gossip
            .handle_request_sync(claimed, &request, now_secs());
        let mut public_messages = std::collections::HashMap::<_, Vec<_>>::new();
        for send in &sends {
            if !authenticated
                && self.config.public_chat_enabled()
                && send.packet.message_type() == Some(MessageType::Message)
            {
                public_messages
                    .entry(send.packet.sender_id)
                    .or_default()
                    .push(&send.packet);
            }
        }
        let requester = claimed.to_routing_bytes();
        for send in &sends {
            let kind = send.packet.message_type();
            if kind == Some(MessageType::Announce)
                && !matches!(
                    AnnouncementPacket::carries_pairing_beacon(&send.packet.payload),
                    Ok(false)
                )
            {
                continue;
            }
            if !authenticated && kind != Some(MessageType::Message) {
                // An unproven requester gets a contact-free author key only
                // when it verifies some history included in this response.
                if kind != Some(MessageType::Announce)
                    || requester == Some(send.packet.sender_id)
                    || !matches!(
                        AnnouncementPacket::carries_contact_metadata(&send.packet.payload),
                        Ok(false)
                    )
                {
                    continue;
                }
                let Some(messages) = public_messages.get(&send.packet.sender_id) else {
                    continue;
                };
                let Ok(announce) = AnnouncementPacket::decode(&send.packet.payload) else {
                    continue;
                };
                let Ok(key) = <[u8; 32]>::try_from(announce.signing_public_key) else {
                    continue;
                };
                if !messages.iter().any(|packet| verify_packet(packet, &key)) {
                    continue;
                }
            }
            if kind == Some(MessageType::GroupMessage) {
                // The store carries opaque ciphertext from any group. Only a
                // current member of this exact epoch may receive a sync copy.
                if !self.group_traffic_allowed(link) {
                    continue;
                }
                let Some(member) = self.state.group_peer_for_wire(claimed) else {
                    continue;
                };
                let Ok(Some(channel)) = self
                    .private_channels
                    .find_group(&send.packet.payload, send.packet.timestamp)
                else {
                    continue;
                };
                if channel.stale
                    || channel.member_key(&member).is_none()
                    || channel
                        .departed
                        .iter()
                        .any(|notice| notice.peer == member.as_str())
                {
                    continue;
                }
            }
            self.transmit_on_link(&send.packet, claimed, link).await;
        }
    }

    // ---------------------------------------------------------------- outbound

    pub(super) async fn on_command(&mut self, command: BleCommand) {
        match command {
            BleCommand::BindingProof { peer, bytes } => {
                if bytes.starts_with(super::super::account_identity::PAIRING_RECEIPT_MAGIC) {
                    self.refresh_pairing_beacons();
                }
                self.send_private_control(&peer, &bytes).await;
            }
            BleCommand::OpenSession { peer } => {
                if self.state.can_open_session_to(&peer)
                    && (!self.state.has_session(&peer) || !self.state.can_write_to(&peer))
                {
                    self.start_handshake(&peer).await;
                } else if self.state.has_session(&peer) && self.state.can_write_to(&peer) {
                    self.send_chatter_hello(&peer).await;
                }
            }
            BleCommand::EnsureSigningBinding { peer } => {
                self.request_signing_pin(&peer).await;
            }
            BleCommand::SignalBundle { peer, bytes } => {
                self.send_signal_bundle(&peer, &bytes).await
            }
            BleCommand::SignalCarrier { peer, bytes } => {
                self.send_signal_carrier(&peer, bytes).await
            }
            BleCommand::Stop => {}
            BleCommand::Announce => self.announce().await,
            BleCommand::Broadcast { packet, sign } => {
                let packet = self.maybe_sign(packet, sign);
                self.originate_broadcast(&packet).await;
            }
            BleCommand::Directed { peer, packet, sign } => {
                let packet = self.maybe_sign(packet, sign);
                self.transmit(&packet, Some(&peer), None).await;
            }
            BleCommand::PrivateMessage {
                peer,
                message_id,
                content,
            } => self.send_private(&peer, &message_id, &content).await,
            BleCommand::Courier { peer, envelope } => {
                self.send_courier_frame(&peer, &envelope).await;
            }
            BleCommand::Ack {
                peer,
                message_id,
                payload_type,
            } => self.send_ack(&peer, &message_id, payload_type).await,
            BleCommand::PublicMessage {
                message_id,
                content,
            } => self.broadcast_public(&message_id, &content).await,
            BleCommand::ChannelMessage {
                message_id,
                name,
                content,
            } => self.broadcast_channel(&message_id, &name, &content).await,
            BleCommand::ChannelPresence { name } => self.broadcast_channel_presence(&name).await,
            BleCommand::PrivateJoinRequest { creator, bytes } => {
                self.send_group_control(&creator, &bytes).await;
            }
            BleCommand::PrivateEpoch { peer, bytes } => {
                self.send_group_state(&peer, &bytes, false).await
            }
            BleCommand::PrivateInvite { peer, bytes } => {
                self.send_group_state(&peer, &bytes, true).await
            }
            BleCommand::PrivateDecline { peer, notice } => {
                self.send_group_control(&peer, &notice).await;
            }
            BleCommand::PrivateEpochAck { peer, id } => {
                if let Ok(Some(state)) = self.private_channels.load(&id) {
                    self.send_private_epoch_ack(&peer, &state).await;
                }
            }
            BleCommand::PrivateChannelMessage {
                channel_id,
                bytes,
                timestamp_ms,
            } => {
                self.broadcast_private_channel(&channel_id, bytes, timestamp_ms)
                    .await
            }
            BleCommand::PrivateLeave {
                peers,
                notice,
                broadcast,
                timestamp_ms,
            } => {
                for peer in peers {
                    if let Some(wire) = self.state.wire_for_group_peer(&peer) {
                        self.send_group_control(&wire, &notice).await;
                    }
                }
                let mut packet = self.build_packet(MessageType::GroupMessage, None, broadcast, 7);
                packet.timestamp = timestamp_ms;
                self.originate_broadcast(&packet).await;
            }
            BleCommand::FileTransfer { message_id, file } => {
                self.broadcast_file(&message_id, &file).await
            }
            BleCommand::StreamedFile {
                message_id,
                manifest,
                path,
            } => self.start_streamed_file(&message_id, manifest, path).await,
        }
    }
}
