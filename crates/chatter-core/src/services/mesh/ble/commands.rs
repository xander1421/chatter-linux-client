use super::*;

impl BleRuntime {
    pub(super) async fn on_request_sync(&mut self, packet: &BitchatPacket, claimed: &PeerId) {
        let Ok(request) = RequestSyncPacket::decode(&packet.payload) else {
            return;
        };

        // A sync response can replay the entire store, and the filter does not
        // constrain that: a GCS filter that is empty or undecodable matches
        // nothing, so `handle_request_sync` reads it as "the requester holds none
        // of it" and answers with everything. Content is not the exposure —
        // public messages are already public and private ones are sealed — but
        // announces and prekey bundles are *keyed by their owner's identity*, and
        // handing the set of them to a stranger hands over the list of everyone
        // this device has met, plus the material to start a session with each.
        //
        // So a request is only answered for a peer that has proved an identity to
        // us: a verified announce, which since it must derive the sender ID from
        // the Noise key it carries is a real binding rather than a claim. Honest
        // peers announce on connect before they ask, so this costs a real device
        // nothing.
        let known = self.state.with(|state| {
            state
                .peers
                .info(claimed)
                .is_some_and(|record| record.verified_nickname)
        });
        if !known {
            tracing::debug!(
                target: "mesh::ble",
                peer = %claimed,
                "sync request from a peer with no verified announce ignored"
            );
            return;
        }

        let sends = self
            .gossip
            .handle_request_sync(claimed, &request, now_secs());
        for send in sends {
            if send.packet.message_type() == Some(MessageType::GroupMessage) {
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
                    || channel.departed.iter().any(|member| member.peer == claimed.as_str())
                {
                    continue;
                }
            }
            self.dispatch_sync(send).await;
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
                if self.state.can_write_to(&peer)
                    && !self.state.with(|state| state.sessions.has_session(&peer))
                {
                    self.start_handshake(&peer).await;
                }
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
