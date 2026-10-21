use super::*;

impl MeshService {
    // ------------------------------------------------------------- write paths

    /// Broadcast into the public mesh room.
    ///
    /// Returns the wire message ID. The message itself arrives on the event
    /// stream as a [`MeshEvent::Message`] with `is_local` set — one path for
    /// local and remote messages, so a row cannot be built two different ways.
    pub fn send_room(&self, text: &str) -> Result<String, MeshError> {
        let content = Self::require_content(text)?;
        if !self.has_radio {
            return Err(MeshError::NoRadio);
        }
        let message_id = new_message_id();
        self.ble
            .send(BleCommand::PublicMessage {
                message_id: message_id.clone(),
                content,
            })
            .map_err(|_| MeshError::WorkerGone)?;
        // A broadcast has no recipient and therefore no later state to progress
        // to. Recording it now is the whole of its delivery history; leaving it
        // unrecorded would make it render as an ordinary one-to-one "sent".
        self.timeline
            .record_delivery(&message_id, MeshDelivery::Broadcast);
        Ok(message_id)
    }

    pub fn join_channel(&self, name: &str) -> Result<String, MeshError> {
        let name = chatter_mesh::channel::canonical_name(name).ok_or(MeshError::OutOfRange(
            "channel name must be 1-32 ASCII letters, digits or hyphens",
        ))?;
        if !self.timeline.has_channel(&name) && self.timeline.active_channels().len() >= 8 {
            return Err(MeshError::OutOfRange(
                "at most eight mesh channels can be active",
            ));
        }
        self.timeline.join_channel(&name);
        let _ = self
            .ble
            .send(BleCommand::ChannelPresence { name: name.clone() });
        Ok(name)
    }

    /// Create a local, creator-managed channel. No name or roster is broadcast.
    pub fn create_private_channel(&self, name: &str) -> Result<String, MeshError> {
        let state = PrivateChannel::create(name, &self.identity, &self.config.nickname())?;
        self.private_channels.save(&state)?;
        self.timeline.open_private_channel(&state.id, &state.name);
        Ok(state.id)
    }

    /// Produce a short-lived QR for the selected nearby peer. It pins the
    /// creator device's full Noise key, and can be redeemed in any live Noise
    /// session to that device during its validity window.
    pub fn private_channel_invite(&self, id: &str, peer: &PeerId) -> Result<String, MeshError> {
        if !self.state.with(|s| {
            s.sessions.has_session(peer)
                && s.chatter_hello
                    .get(peer)
                    .is_some_and(|hello| hello.capabilities & chatter_hello::QR_CHANNELS != 0)
        }) {
            return Err(self.needs_session(peer));
        }
        let mut state = self
            .private_channels
            .load(id)?
            .ok_or_else(|| MeshError::Binding("unknown private channel".into()))?;
        let text = state.invite(
            &self.identity,
            peer,
            self.identity.static_public(),
            now_secs(),
        )?;
        self.private_channels.save(&state)?;
        Ok(text)
    }

    pub fn private_channel_invite_png(
        &self,
        id: &str,
        peer: &PeerId,
    ) -> Result<Vec<u8>, MeshError> {
        use image::{ImageEncoder, Luma};
        let text = self.private_channel_invite(id, peer)?;
        let qr = qrcode::QrCode::new(text.as_bytes())
            .map_err(|e| MeshError::Binding(format!("could not encode private channel QR: {e}")))?;
        let image = qr.render::<Luma<u8>>().quiet_zone(true).build();
        let scaled =
            image::imageops::resize(&image, 512, 512, image::imageops::FilterType::Nearest);
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(scaled.as_raw(), 512, 512, image::ExtendedColorType::L8)
            .map_err(|e| MeshError::Binding(format!("could not render private channel QR: {e}")))?;
        Ok(bytes)
    }

    pub fn join_private_channel(&self, qr: &str) -> Result<(), MeshError> {
        let invite = private_channel::Invite::from_text(qr, now_secs())
            .ok_or_else(|| MeshError::Binding("invalid or expired private channel QR".into()))?;
        if invite.target != self.identity.peer_id().as_str() {
            return Err(MeshError::Binding(
                "this QR was issued to another device".into(),
            ));
        }
        let creator = PeerId::from_str(&invite.creator);
        if self.state.with(|s| s.sessions.remote_static(&creator))
            != Some(
                invite
                    .creator_static
                    .as_slice()
                    .try_into()
                    .map_err(|_| MeshError::Binding("invalid creator key".into()))?,
            )
        {
            return Err(MeshError::Binding(
                "QR creator does not match the Noise session".into(),
            ));
        }
        if !self.state.with(|s| {
            s.sessions.has_session(&creator)
                && s.chatter_hello
                    .get(&creator)
                    .is_some_and(|hello| hello.capabilities & chatter_hello::QR_CHANNELS != 0)
        }) {
            return Err(MeshError::Binding(
                "connect to the creator as a Chatter mesh peer before scanning".into(),
            ));
        }
        self.private_channels.save_pending(&invite)?;
        self.ble
            .send(BleCommand::PrivateJoinRequest {
                creator,
                bytes: invite
                    .request_bytes()
                    .ok_or_else(|| MeshError::Binding("invalid private channel QR".into()))?,
            })
            .map_err(|_| MeshError::NotRunning)
    }

    pub fn remove_private_channel_member(&self, id: &str, peer: &PeerId) -> Result<(), MeshError> {
        let mut state = self
            .private_channels
            .load(id)?
            .ok_or_else(|| MeshError::Binding("unknown private channel".into()))?;
        state.rotate(&self.identity, None, Some(peer))?;
        self.private_channels.save(&state)?;
        let removed_state = state.removed_update_bytes(&self.identity)?;
        self.ble
            .send(BleCommand::PrivateEpoch {
                peer: peer.clone(),
                bytes: removed_state,
            })
            .map_err(|_| MeshError::NotRunning)?;
        let bytes = state.update_bytes()?;
        for member in &state.members {
            if member.peer != self.identity.peer_id().as_str() {
                self.ble
                    .send(BleCommand::PrivateEpoch {
                        peer: PeerId::from_str(&member.peer),
                        bytes: bytes.clone(),
                    })
                    .map_err(|_| MeshError::NotRunning)?;
            }
        }
        Ok(())
    }

    /// Announce departure through Noise and encrypted broadcast before deleting
    /// the local key. Delivery remains best effort for offline members.
    pub fn leave_private_channel(&self, id: &str) -> Result<(), MeshError> {
        let state = self
            .private_channels
            .load(id)?
            .ok_or_else(|| MeshError::Binding("unknown private channel".into()))?;
        let notice = state.leave_notice(&self.identity)?;
        let peers: Vec<PeerId> = state
            .members
            .iter()
            .filter(|member| member.peer != self.identity.peer_id().as_str())
            .map(|member| PeerId::from_str(&member.peer))
            .collect();
        self.private_channels.save_pending_leave(&PendingLeave {
            notice: notice.clone(),
            creator: state.creator.clone(),
            recipients: peers.iter().map(ToString::to_string).collect(),
            acknowledged: vec![],
        })?;
        let encoded = notice.encode()?;
        let (_, broadcast) = state.encrypt(&self.identity, &encoded, &self.config.nickname())?;
        let notice = serde_json::to_vec(&notice)
            .map_err(|_| MeshError::Binding("invalid leave notice".into()))?;
        let mut control = b"CHTRPCL2".to_vec();
        control.extend(notice);
        self.ble
            .send(BleCommand::PrivateLeave {
                peers,
                notice: control,
                broadcast,
            })
            .map_err(|_| MeshError::NotRunning)?;
        self.private_channels.remove(id)?;
        self.private_channels.clear_pending(id)?;
        self.timeline.close_private_channel(id);
        Ok(())
    }

    pub fn private_channel_removable_members(
        &self,
        id: &str,
    ) -> Result<Vec<PrivateChannelMember>, MeshError> {
        let state = self
            .private_channels
            .load(id)?
            .ok_or_else(|| MeshError::Binding("unknown private channel".into()))?;
        if state.creator != self.identity.peer_id().as_str() {
            return Ok(Vec::new());
        }
        Ok(state
            .members
            .iter()
            .filter(|member| member.peer != state.creator)
            .map(|member| {
                let peer = PeerId::from_str(&member.peer);
                let known_name = self.nickname_for(&peer);
                let display_name = if known_name != member.peer && !known_name.trim().is_empty() {
                    known_name
                } else {
                    self.timeline
                        .remembered_sender_name(&mapping::peer_user_id(&peer))
                        .filter(|name| !name.trim().is_empty())
                        .unwrap_or_else(|| member.peer.chars().take(16).collect())
                };
                PrivateChannelMember {
                    peer_id: member.peer.clone(),
                    display_name,
                    awaiting_rekey: state.awaiting_rekey.contains(&member.peer),
                }
            })
            .collect())
    }

    pub fn private_channel_member_count(&self, id: &str) -> Result<usize, MeshError> {
        let state = self
            .private_channels
            .load(id)?
            .ok_or_else(|| MeshError::Binding("unknown private channel".into()))?;
        Ok(state.members.len())
    }

    pub fn private_channel_is_creator(&self, id: &str) -> Result<bool, MeshError> {
        let state = self
            .private_channels
            .load(id)?
            .ok_or_else(|| MeshError::Binding("unknown private channel".into()))?;
        Ok(state.creator == self.identity.peer_id().as_str())
    }

    pub fn private_channel_rekey_pending(&self, id: &str) -> Result<bool, MeshError> {
        let state = self
            .private_channels
            .load(id)?
            .ok_or_else(|| MeshError::Binding("unknown private channel".into()))?;
        Ok(state.stale)
    }

    pub fn send_private_channel(&self, id: &str, text: &str) -> Result<String, MeshError> {
        let state = self
            .private_channels
            .load(id)?
            .ok_or_else(|| MeshError::Binding("unknown private channel".into()))?;
        if state.stale {
            return Err(MeshError::Binding(
                "Waiting for key update from creator".into(),
            ));
        }
        let (message_id, bytes) = state.encrypt(&self.identity, text, &self.config.nickname())?;
        self.ble
            .send(BleCommand::PrivateChannelMessage {
                channel_id: id.into(),
                bytes,
            })
            .map_err(|_| MeshError::NotRunning)?;
        // The encrypted packet is broadcast over the radio, with no
        // per-member acknowledgement. Keep the durable state in step with
        // the live row emitted by `broadcast_private_channel`.
        self.timeline
            .record_delivery(&message_id, MeshDelivery::Broadcast);
        Ok(message_id)
    }

    pub fn nearby_channels(&self) -> Vec<String> {
        self.timeline.nearby_channels()
    }

    pub fn set_channel_muted(&self, name: &str, muted: bool) -> Result<(), MeshError> {
        let name = chatter_mesh::channel::canonical_name(name)
            .ok_or(MeshError::OutOfRange("invalid channel name"))?;
        self.events.set_channel_muted(&name, muted);
        Ok(())
    }

    pub fn channel_muted(&self, name: &str) -> bool {
        self.events.channel_muted(name)
    }

    pub fn set_peer_muted(&self, peer: &PeerId, muted: bool) {
        self.events.set_peer_muted(peer, muted);
    }

    pub fn peer_muted(&self, peer: &PeerId) -> bool {
        self.events.peer_muted(peer)
    }

    pub fn set_peer_blocked(&self, peer: &PeerId, blocked: bool) {
        self.events.set_peer_blocked(peer, blocked);
    }

    pub fn peer_blocked(&self, peer: &PeerId) -> bool {
        self.events.peer_blocked(peer)
    }

    pub fn leave_channel(&self, name: &str) -> Result<(), MeshError> {
        let name = chatter_mesh::channel::canonical_name(name)
            .ok_or(MeshError::OutOfRange("invalid channel name"))?;
        self.timeline.leave_channel(&name);
        Ok(())
    }

    pub fn send_channel(&self, name: &str, text: &str) -> Result<String, MeshError> {
        let name = chatter_mesh::channel::canonical_name(name)
            .ok_or(MeshError::OutOfRange("invalid channel name"))?;
        if !self.timeline.has_channel(&name) {
            return Err(MeshError::OutOfRange("join the channel before sending"));
        }
        let content = Self::require_content(text)?;
        if chatter_mesh::channel::encode(&name, &content).is_none() {
            return Err(MeshError::OutOfRange("channel text exceeds 256 bytes"));
        }
        if !self.has_radio {
            return Err(MeshError::NoRadio);
        }
        let message_id = new_message_id();
        self.ble
            .send(BleCommand::ChannelMessage {
                message_id: message_id.clone(),
                name,
                content,
            })
            .map_err(|_| MeshError::WorkerGone)?;
        Ok(message_id)
    }
}
