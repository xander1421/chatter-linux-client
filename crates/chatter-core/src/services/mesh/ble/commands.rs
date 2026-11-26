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
                let Some(group_id) = PrivateChannel::envelope_group_id(&send.packet.payload) else {
                    continue;
                };
                let Some(epoch) = PrivateChannel::envelope_epoch(&send.packet.payload) else {
                    continue;
                };
                let id = uuid::Uuid::from_bytes(group_id).to_string();
                let Ok(Some(channel)) = self.private_channels.load(&id) else {
                    continue;
                };
                if channel.stale
                    || channel.epoch != epoch
                    || channel.member_key(claimed).is_none()
                    || channel
                        .departed
                        .iter()
                        .any(|member| member.peer == claimed.as_str())
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
                let sealed = self
                    .state
                    .with(|state| state.sessions.encrypt(&peer, &bytes));
                if let Ok(ciphertext) = sealed {
                    let packet = self.build_packet(
                        MessageType::NoiseEncrypted,
                        Some(&peer),
                        ciphertext,
                        chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
                    );
                    self.transmit(&packet, Some(&peer), None).await;
                }
            }
            BleCommand::OpenSession { peer } => {
                if self.state.can_open_session_to(&peer)
                    && (!self.state.has_session(&peer) || !self.state.can_write_to(&peer))
                {
                    self.start_handshake(&peer).await;
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
                let Ok(payload) = envelope.encode() else {
                    return;
                };
                let packet = self.build_packet(
                    MessageType::CourierEnvelope,
                    Some(&peer),
                    payload,
                    chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
                );
                self.transmit(&packet, Some(&peer), None).await;
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
                self.send_private_control(&creator, &bytes).await
            }
            BleCommand::PrivateEpoch { peer, bytes } => {
                self.send_group_state(&peer, &bytes, false).await
            }
            BleCommand::PrivateInvite { peer, bytes } => {
                self.send_group_state(&peer, &bytes, true).await
            }
            BleCommand::PrivateDecline { peer, notice } => {
                self.send_private_control(&peer, &notice).await
            }
            BleCommand::PrivateEpochAck { peer, id } => {
                if let Ok(Some(state)) = self.private_channels.load(&id) {
                    self.send_private_epoch_ack(&peer, &state).await;
                }
            }
            BleCommand::PrivateChannelMessage { channel_id, bytes } => {
                self.broadcast_private_channel(&channel_id, bytes).await
            }
            BleCommand::PrivateLeave {
                peers,
                notice,
                broadcast,
            } => {
                for peer in peers {
                    self.send_private_control(&peer, &notice).await;
                }
                let packet = self.build_packet(MessageType::GroupMessage, None, broadcast, 7);
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
