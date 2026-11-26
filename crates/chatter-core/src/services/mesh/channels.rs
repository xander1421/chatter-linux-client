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
        let identity = self.private_group_identity()?;
        let state = PrivateChannel::create(name, &identity, &self.config.nickname())?;
        self.private_channels.save(&state)?;
        self.timeline.open_private_channel(&state.id, &state.name);
        Ok(state.id)
    }

    /// Groups this device can invite the selected peer into.
    pub fn private_channels_invitable_for(
        &self,
        peer: &PeerId,
        active_scope: &str,
    ) -> Result<Vec<(String, String)>, MeshError> {
        let Ok(identity) = self.private_group_identity() else {
            return Ok(Vec::new());
        };
        let Some(peer) = self.state.group_peer_for_wire(peer) else {
            return Ok(Vec::new());
        };
        let creator = identity.peer_id();
        let mut channels: Vec<_> = self
            .private_channels
            .list()?
            .into_iter()
            .filter(|channel| {
                // An invitee who has not answered is already in the roster; a
                // fresh direct invite is how the creator re-arms it.
                channel.creator == creator.as_str()
                    && (channel
                        .unanswered_direct_invites
                        .contains_key(peer.as_str())
                        || !channel
                            .members
                            .iter()
                            .any(|member| member.peer == peer.as_str()))
            })
            .map(|channel| (channel.id, channel.name))
            .collect();
        channels.sort_by(|a, b| {
            (a.0.as_str() != active_scope, a.1.to_lowercase(), &a.0).cmp(&(
                b.0.as_str() != active_scope,
                b.1.to_lowercase(),
                &b.0,
            ))
        });
        Ok(channels)
    }

    /// Produce a short-lived QR for the selected nearby peer. It pins the
    /// creator device's full Noise key, and can be redeemed in any live Noise
    /// session to that device during its validity window.
    pub fn private_channel_invite(&self, id: &str, peer: &PeerId) -> Result<String, MeshError> {
        let identity = self.private_group_identity()?;
        let wire = self.state.route(peer);
        if !self.state.with(|s| {
            s.sessions.has_session(&wire)
                && s.chatter_hello
                    .get(&wire)
                    .is_some_and(|hello| hello.capabilities & chatter_hello::QR_CHANNELS != 0)
        }) {
            return Err(self.needs_session(&wire));
        }
        let Some(member) = self.state.pinned_member(&wire, true) else {
            return Err(self.needs_signing_binding(&wire));
        };
        let mut state = self
            .private_channels
            .load(id)?
            .ok_or_else(|| MeshError::Binding("unknown private channel".into()))?;
        let text = state.invite(
            &identity,
            &PeerId::from_str(&member.peer),
            identity.static_public(),
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

    fn needs_signing_binding(&self, peer: &PeerId) -> MeshError {
        let _ = self
            .ble
            .send(BleCommand::EnsureSigningBinding { peer: peer.clone() });
        self.needs_session(peer)
    }

    /// Queue bitchat's native 0x06 group invitation for a verified nearby peer.
    pub fn invite_private_channel_direct(&self, id: &str, peer: &PeerId) -> Result<(), MeshError> {
        let identity = self.private_group_identity()?;
        if self.events.peer_blocked(peer) {
            return Err(self.needs_session(peer));
        }
        let mut state = self
            .private_channels
            .load(id)?
            .ok_or_else(|| MeshError::Binding("unknown private channel".into()))?;
        if state.creator != identity.peer_id().as_str() {
            return Err(MeshError::Binding("only the creator can invite".into()));
        }
        let Some(member) = self.state.pinned_member(peer, false) else {
            return Err(self.needs_signing_binding(peer));
        };
        let group_peer = PeerId::from_str(&member.peer);
        if let Some(existing) = state.members.iter().find(|m| m.peer == group_peer.as_str()) {
            if existing.fingerprint != member.fingerprint
                || existing.signing_key != member.signing_key
            {
                return Err(MeshError::Binding(
                    "peer identity differs from the group roster".into(),
                ));
            }
            if state
                .unanswered_direct_invites
                .contains_key(group_peer.as_str())
            {
                state
                    .unanswered_direct_invites
                    .insert(group_peer.to_string(), now_secs());
                state
                    .unanswered_expired
                    .retain(|old| old != group_peer.as_str());
                self.private_channels.save(&state)?;
                self.events.send(MeshEvent::PrivateChannelChanged);
            }
            return self
                .ble
                .send(BleCommand::PrivateInvite {
                    peer: group_peer.clone(),
                    bytes: state.update_bytes()?,
                })
                .map_err(|_| MeshError::NotRunning);
        }
        state.rotate(&identity, Some(member), None)?;
        state
            .unanswered_direct_invites
            .insert(group_peer.to_string(), now_secs());
        self.private_channels.save(&state)?;
        let bytes = state.update_bytes()?;
        self.ble
            .send(BleCommand::PrivateInvite {
                peer: group_peer.clone(),
                bytes: bytes.clone(),
            })
            .map_err(|_| MeshError::NotRunning)?;
        for member in &state.members {
            if member.peer != identity.peer_id().as_str() && member.peer != group_peer.as_str() {
                self.ble
                    .send(BleCommand::PrivateEpoch {
                        peer: PeerId::from_str(&member.peer),
                        bytes: bytes.clone(),
                    })
                    .map_err(|_| MeshError::NotRunning)?;
            }
        }
        self.events.send(MeshEvent::PrivateChannelChanged);
        Ok(())
    }

    pub fn pending_private_invitations(&self) -> Vec<(String, String, String)> {
        let now = now_secs();
        let nearby = self.state.peers();
        let invitations: Vec<_> = self.state.with(|state| {
            state.pending_group_invites.retain(|_, pending| {
                now.saturating_sub(pending.received_at) < DIRECT_INVITE_LIFETIME_SECS
            });
            state
                .pending_group_invites
                .values()
                .map(|pending| {
                    let group = &pending.state;
                    (
                        group.id.clone(),
                        group.name.clone(),
                        PeerId::from_str(&group.creator),
                        group.members[0].nickname.clone(),
                    )
                })
                .collect()
        });
        invitations
            .into_iter()
            .filter_map(|(id, name, creator, nickname)| {
                let wire = self.state.wire_for_group_peer(&creator)?;
                let contact = self.state.contact(&wire);
                if self.events.peer_blocked(&contact) {
                    return None;
                }
                let nickname = nearby
                    .iter()
                    .find(|peer| peer.peer == contact)
                    .map_or(nickname, |peer| peer.nickname.clone());
                Some((id, name, nickname))
            })
            .collect()
    }

    pub fn decide_private_invitation(&self, id: &str, join: bool) -> Result<(), MeshError> {
        let identity = self.private_group_identity()?;
        let pending = self.state.with(|s| s.pending_group_invites.remove(id));
        let Some(pending) = pending else {
            return Err(MeshError::Binding("invitation expired".into()));
        };
        if !join {
            let state = &pending.state;
            self.state.with(|s| {
                s.declined_group_invites
                    .insert(id.to_owned(), (state.epoch, now_secs()));
            });
            let Some(creator) = self
                .state
                .wire_for_group_peer(&PeerId::from_str(&state.creator))
            else {
                return Ok(());
            };
            if now_secs().saturating_sub(pending.received_at) < DIRECT_INVITE_LIFETIME_SECS
                && state.verify()
                && state.member_key(&identity.peer_id()) == Some(identity.signing_public())
                && self.private_creator_is_live(&creator, &state.creator_static, &state.creator_key)
            {
                // The leave notice already signs group ID, epoch, and roster peer.
                // A separate Chatter control prefix keeps the decline off the iOS wire format.
                let notice = state.leave_notice(&identity)?;
                let mut bytes = b"CHTRPCD1".to_vec();
                bytes.extend(
                    serde_json::to_vec(&notice)
                        .map_err(|_| MeshError::Binding("invalid decline notice".into()))?,
                );
                self.ble
                    .send(BleCommand::PrivateDecline {
                        peer: creator,
                        notice: bytes,
                    })
                    .map_err(|_| MeshError::NotRunning)?;
            }
            return Ok(());
        }
        let state = pending.state;
        let creator = self
            .state
            .wire_for_group_peer(&PeerId::from_str(&state.creator))
            .ok_or_else(|| MeshError::Binding("creator pairing unavailable".into()))?;
        if now_secs().saturating_sub(pending.received_at) >= DIRECT_INVITE_LIFETIME_SECS
            || self.events.peer_blocked(&self.state.contact(&creator))
            || !state.verify()
            || state.member_key(&identity.peer_id()) != Some(identity.signing_public())
            || !state.members.iter().any(|m| {
                m.peer == identity.peer_id().as_str() && m.fingerprint == identity.fingerprint()
            })
            || !self.private_creator_is_live(&creator, &state.creator_static, &state.creator_key)
        {
            return Err(MeshError::Binding("invitation is no longer valid".into()));
        }
        self.private_channels.save(&state)?;
        self.timeline.open_private_channel(&state.id, &state.name);
        self.events.send(MeshEvent::PrivateChannelChanged);
        let _ = self.ble.send(BleCommand::PrivateEpochAck {
            peer: creator,
            id: state.id,
        });
        Ok(())
    }

    fn private_creator_is_live(
        &self,
        wire: &PeerId,
        static_key: &[u8],
        signing_key: &[u8],
    ) -> bool {
        self.state.pinned_member(wire, true).is_some()
            && self
                .state
                .private_group_keys(wire)
                .is_some_and(|(noise, signing)| noise == static_key && signing == signing_key)
    }

    pub fn join_private_channel(&self, qr: &str) -> Result<(), MeshError> {
        let invite = private_channel::Invite::from_text(qr, now_secs())
            .ok_or_else(|| MeshError::Binding("invalid or expired private channel QR".into()))?;
        if invite.target != self.private_group_identity()?.peer_id().as_str() {
            return Err(MeshError::Binding(
                "this QR was issued to another device".into(),
            ));
        }
        let creator = self
            .state
            .wire_for_group_peer(&PeerId::from_str(&invite.creator))
            .ok_or_else(|| MeshError::Binding("creator pairing unavailable".into()))?;
        if !self.private_creator_is_live(&creator, &invite.creator_static, &invite.creator_key) {
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
        let identity = self.private_group_identity()?;
        let mut state = self
            .private_channels
            .load(id)?
            .ok_or_else(|| MeshError::Binding("unknown private channel".into()))?;
        let peer = self
            .state
            .group_peer_for_wire(peer)
            .unwrap_or_else(|| peer.clone());
        state.rotate(&identity, None, Some(&peer))?;
        self.private_channels.save(&state)?;
        let removed_state = state.removed_update_bytes(&identity)?;
        self.ble
            .send(BleCommand::PrivateEpoch {
                peer: peer.clone(),
                bytes: removed_state,
            })
            .map_err(|_| MeshError::NotRunning)?;
        let bytes = state.update_bytes()?;
        for member in &state.members {
            if member.peer != identity.peer_id().as_str() {
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
        let identity = self.private_group_identity()?;
        let state = self
            .private_channels
            .load(id)?
            .ok_or_else(|| MeshError::Binding("unknown private channel".into()))?;
        let notice = state.leave_notice(&identity)?;
        let peers: Vec<PeerId> = state
            .members
            .iter()
            .filter(|member| member.peer != identity.peer_id().as_str())
            .map(|member| PeerId::from_str(&member.peer))
            .collect();
        self.private_channels.save_pending_leave(&PendingLeave {
            notice: notice.clone(),
            creator: state.creator.clone(),
            recipients: peers.iter().map(ToString::to_string).collect(),
            acknowledged: vec![],
        })?;
        let encoded = notice.encode()?;
        let timestamp_ms = now_millis();
        let (_, broadcast) =
            state.encrypt(&identity, &encoded, &self.config.nickname(), timestamp_ms)?;
        let notice = serde_json::to_vec(&notice)
            .map_err(|_| MeshError::Binding("invalid leave notice".into()))?;
        let mut control = b"CHTRPCL2".to_vec();
        control.extend(notice);
        self.ble
            .send(BleCommand::PrivateLeave {
                peers,
                notice: control,
                broadcast,
                timestamp_ms,
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
        if !self
            .private_group_identity()
            .is_ok_and(|identity| state.creator == identity.peer_id().as_str())
        {
            return Ok(Vec::new());
        }
        // Nearby names carry the `#xxxx` suffix that tells same-named peers apart.
        let nearby = self.state.peers();
        Ok(state
            .members
            .iter()
            .filter(|member| member.peer != state.creator)
            .map(|member| {
                let peer = PeerId::from_str(&member.peer);
                let display_name = self
                    .state
                    .wire_for_group_peer(&peer)
                    .and_then(|wire| {
                        let contact = self.state.contact(&wire);
                        nearby
                            .iter()
                            .find(|nearby| nearby.peer == contact)
                            .map(|nearby| nearby.nickname.clone())
                    })
                    .or_else(|| {
                        self.timeline
                            .remembered_sender_name(&mapping::peer_user_id(&peer))
                    })
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| member.nickname.clone());
                PrivateChannelMember {
                    peer_id: member.peer.clone(),
                    display_name,
                    status_label: if state.unanswered_expired.contains(&member.peer) {
                        "Invite not answered"
                    } else if state.awaiting_rekey.contains(&member.peer) {
                        "Waiting for the new key"
                    } else {
                        "Has the current key"
                    },
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

    /// Nearby peers and their membership in the selected private group.
    pub fn private_channel_nearby_peers(
        &self,
        id: &str,
    ) -> Result<Vec<(MeshPeer, bool)>, MeshError> {
        let channel = self
            .private_channels
            .load(id)?
            .ok_or_else(|| MeshError::Binding("unknown private channel".into()))?;
        Ok(self
            .nearby_peers()
            .into_iter()
            .map(|peer| {
                let member = self
                    .state
                    .group_peer_for_wire(&peer.peer)
                    .is_some_and(|group| {
                        channel.member_key(&group).is_some()
                            && !channel
                                .unanswered_direct_invites
                                .contains_key(group.as_str())
                    });
                (peer, member)
            })
            .collect())
    }

    pub fn private_channel_is_creator(&self, id: &str) -> Result<bool, MeshError> {
        let state = self
            .private_channels
            .load(id)?
            .ok_or_else(|| MeshError::Binding("unknown private channel".into()))?;
        Ok(self
            .private_group_identity()
            .is_ok_and(|identity| state.creator == identity.peer_id().as_str()))
    }

    pub fn private_channel_rekey_pending(&self, id: &str) -> Result<bool, MeshError> {
        let state = self
            .private_channels
            .load(id)?
            .ok_or_else(|| MeshError::Binding("unknown private channel".into()))?;
        Ok(state.stale)
    }

    pub fn send_private_channel(&self, id: &str, text: &str) -> Result<String, MeshError> {
        let identity = self.private_group_identity()?;
        let state = self
            .private_channels
            .load(id)?
            .ok_or_else(|| MeshError::Binding("unknown private channel".into()))?;
        if state.stale {
            return Err(MeshError::Binding(
                "Waiting for key update from creator".into(),
            ));
        }
        let timestamp_ms = now_millis();
        let (message_id, bytes) =
            state.encrypt(&identity, text, &self.config.nickname(), timestamp_ms)?;
        self.ble
            .send(BleCommand::PrivateChannelMessage {
                channel_id: id.into(),
                bytes,
                timestamp_ms,
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
