use super::*;

impl BleRuntime {
    /// Whether a self-authored packet is one we have already shown.
    ///
    /// The self-broadcast tracker covers this session's sends for five
    /// minutes; the gossip store covers everything we sent or already accepted
    /// for its whole retention window. The second check is what makes recovery
    /// happen once: a self-authored TTL-0 sync response bypasses the dedup
    /// cache by design, so without it every peer that answered a round with the
    /// same lost message of ours added another copy to the screen.
    pub(super) fn is_own_echo(&self, packet: &BitchatPacket) -> bool {
        self.gossip.holds(packet)
            || self
                .state
                .with(|state| state.self_broadcasts.contains(packet))
    }

    pub(super) fn on_public_message(&mut self, packet: &BitchatPacket, claimed: &PeerId) {
        if self.events.peer_blocked(claimed) {
            return;
        }
        // A `Message` addressed to one recipient is a legacy private message. We
        // do not carry those — private traffic is Noise-encrypted — and showing
        // it as public chat would put a message meant for one person in the room.
        if !packet.is_broadcast() {
            return;
        }
        let Ok(content) = core::str::from_utf8(&packet.payload) else {
            return;
        };
        // An empty payload decodes cleanly and is not a message. `send_room` and
        // `send_dm` both refuse to compose one (`require_content`), and bitchat
        // refuses on its send side too (`MeshCore.kt:415`, `content.isEmpty()`) —
        // but neither of us checks on receive, so an empty broadcast from anywhere
        // became a stored row holding `Text("")`. There is nothing to render in
        // it: the room showed a bubble with a sender and a timestamp and no body,
        // which reads as a message that failed to load rather than one that was
        // never said.
        //
        // Whitespace counts as empty for the same reason. This is the one place
        // the check belongs, because a row, once stored, is what every surface
        // reads from and no later layer can tell it apart from a message whose
        // text genuinely went missing.
        if content.trim().is_empty() {
            tracing::debug!(
                target: "mesh::ble",
                peer = %claimed,
                bytes = packet.payload.len(),
                "an empty public message was dropped"
            );
            return;
        }

        // The sender is whoever the packet header says, authenticated against
        // the signing key their announce established — there is no self-declared
        // sender field to prefer, and preferring one would let anybody publish
        // under anybody's name. An unsigned or unverifiable public message is
        // dropped, matching `BLEPublicMessageHandler.swift:86-95`.
        let sender = claimed.clone();
        // A message we wrote ourselves, recovered from a peer's gossip archive.
        // The ingress guard admits a self-authored TTL-0 sync response precisely
        // so this can happen — it is how a device that lost its database gets its
        // own history back — so it has to be handled rather than dropped.
        //
        // It is authenticated against our own signing key, because we are not in
        // our own peer registry (and must not be: see `on_announce`). It is
        // attributed to us, because it *is* ours; treating it as inbound would put
        // our own words on screen as though a stranger had said them.
        let is_self = sender == self.identity.peer_id();
        // One of our own recent broadcasts, flooded or synced back to us. We
        // displayed it the moment it was composed, so showing it again is a
        // duplicate — and a confusing one, because the two rows disagree: the
        // local copy is labelled `broadcast` while this one has no delivery state
        // and falls through to "sent".
        //
        // `SelfBroadcastTracker` is exactly the "we have already shown this"
        // record. The ingress guard deliberately skips it for a TTL-0 sync
        // response — that is how a device with no database recovers its own
        // history — so this is the layer that has to consult it. A self-authored
        // packet the tracker does *not* know is genuine recovery and still lands.
        if is_self && self.is_own_echo(packet) {
            tracing::debug!(
                target: "mesh::ble",
                "our own broadcast came back; already shown"
            );
            return;
        }
        let (signing_key, nickname, verified) = if is_self {
            (self.identity.signing_public(), self.config.nickname(), true)
        } else {
            let known = self.state.with(|state| {
                state.peers.info(&sender).map(|peer| {
                    (
                        peer.signing_public_key.clone(),
                        peer.nickname.clone(),
                        peer.verified_nickname,
                    )
                })
            });
            let Some((Some(key), nickname, verified)) = known else {
                return;
            };
            let Ok(key) = <[u8; 32]>::try_from(key.as_slice()) else {
                return;
            };
            (key, nickname, verified)
        };
        if !verify_packet(packet, &signing_key) {
            tracing::debug!(
                target: "mesh::ble",
                peer = %sender,
                "public message with missing or invalid signature dropped",
            );
            return;
        }

        tracing::info!(
            target: "mesh::ble",
            peer = %sender,
            nickname = %nickname,
            bytes = content.len(),
            is_self,
            "public message received"
        );
        let inbound = InboundMessage {
            // No message ID on the wire, so one is derived from the packet. The
            // dedup key is a function of sender, timestamp, type and payload
            // hash, so every copy that reaches us over a different path — and
            // the same message seen twice — resolves to one ID.
            wire_id: MessageDeduplicator::dedup_key(packet),
            conversation: MeshConversation::Room,
            sender: sender.clone(),
            nickname: nickname.clone(),
            content: content.to_string(),
            timestamp_ms: packet.timestamp,
        };
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message: inbound.to_domain(),
            conversation: inbound.conversation,
            sender,
            nickname,
            // `is_local` decides which side of the timeline it lands on and
            // whether it counts as unread. A recovered self-authored message is
            // ours on both counts.
            is_local: is_self,
            sender_verified: verified,
        })));
    }

    pub(super) fn on_channel_message(&mut self, packet: &BitchatPacket, claimed: &PeerId) -> bool {
        if !packet.is_broadcast() {
            return false;
        }
        let presence = chatter_mesh::channel::decode_presence(&packet.payload);
        let message = chatter_mesh::channel::decode(&packet.payload);
        let Some(name) = presence
            .clone()
            .or_else(|| message.as_ref().map(|(name, _)| name.clone()))
        else {
            return false;
        };
        let is_self = *claimed == self.identity.peer_id();
        let (key, nickname) = if is_self {
            (self.identity.signing_public(), self.config.nickname())
        } else {
            let Some((key, nickname)) = self.state.with(|state| {
                state.peers.info(claimed).and_then(|peer| {
                    let key = peer.signing_public_key.as_ref()?;
                    Some((
                        <[u8; 32]>::try_from(key.as_slice()).ok()?,
                        peer.nickname.clone(),
                    ))
                })
            }) else {
                return false;
            };
            (key, nickname)
        };
        if !verify_packet(packet, &key) {
            return false;
        }
        self.events.send(MeshEvent::ChannelObserved(name.clone()));
        let Some((_, content)) = message else {
            return true;
        };
        if !self.events.channel_joined(&name)
            || self.events.channel_muted(&name)
            || self.events.peer_blocked(claimed)
            || (is_self && self.is_own_echo(packet))
        {
            return true;
        }
        let inbound = InboundMessage {
            wire_id: MessageDeduplicator::dedup_key(packet),
            conversation: MeshConversation::Channel(name),
            sender: claimed.clone(),
            nickname: nickname.clone(),
            content,
            timestamp_ms: packet.timestamp,
        };
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message: inbound.to_domain(),
            conversation: inbound.conversation,
            sender: claimed.clone(),
            nickname,
            is_local: is_self,
            sender_verified: false,
        })));
        true
    }

    pub(super) fn on_private_channel_message(
        &mut self,
        packet: &BitchatPacket,
        _claimed: &PeerId,
    ) -> bool {
        if !packet.is_broadcast() || packet.payload.is_empty() || packet.payload.len() > 4096 {
            return false;
        }
        let Some(group_id) = PrivateChannel::envelope_group_id(&packet.payload) else {
            return true;
        };
        let id = uuid::Uuid::from_bytes(group_id).to_string();
        let Ok(Some(mut state)) = self.private_channels.load(&id) else {
            // Nonmembers still relay sealed group packets.
            return true;
        };
        if self.is_own_echo(packet) {
            return true;
        }
        let Some(opened) = state.decrypt(&packet.payload) else {
            return true;
        };
        let author = opened.sender.clone();
        state.awaiting_rekey.retain(|peer| peer != author.as_str());
        if !state.record_message(&opened.id) || self.private_channels.save(&state).is_err() {
            return true;
        }
        if let Some(notice) = LeaveNotice::decode(&opened.content) {
            if notice.peer != author.as_str() || !state.verify_leave(&notice) {
                return true;
            }
            self.on_private_leave(&author, notice);
            return true;
        }
        if self.events.peer_blocked(&author) {
            return true;
        }
        let now_ms = now_millis();
        self.private_authored_ingress
            .retain(|_, (start, _)| now_ms.saturating_sub(*start) < 60_000);
        if self.private_authored_ingress.len() >= 128
            && !self.private_authored_ingress.contains_key(&author)
        {
            return true;
        }
        let budget = self
            .private_authored_ingress
            .entry(author.clone())
            .or_insert((now_ms, 0));
        if budget.1 >= CHANNEL_INGRESS_PER_MINUTE {
            return true;
        }
        budget.1 += 1;
        let conversation = MeshConversation::PrivateChannel(state.id.clone());
        let inbound = InboundMessage {
            wire_id: opened.id,
            conversation: conversation.clone(),
            sender: author.clone(),
            nickname: opened.nickname.clone(),
            content: opened.content,
            timestamp_ms: opened.timestamp_ms,
        };
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message: inbound.to_domain(),
            conversation,
            sender: author.clone(),
            nickname: opened.nickname,
            is_local: author == self.identity.peer_id(),
            sender_verified: true,
        })));
        true
    }

    pub(super) async fn on_private_join_request(&mut self, claimed: &PeerId, invite: Invite) {
        if invite.creator != self.identity.peer_id().as_str() || invite.target != claimed.as_str() {
            return;
        }
        let Some(signing_key) = self.state.with(|s| {
            s.peers
                .info(claimed)
                .and_then(|p| p.signing_public_key.clone())
        }) else {
            return;
        };
        let Some(static_key) = self.state.with(|s| s.sessions.remote_static(claimed)) else {
            return;
        };
        let nickname = self
            .state
            .with(|s| s.peers.info(claimed).map(|p| p.nickname.clone()))
            .unwrap_or_else(|| claimed.to_string());
        let member = Member {
            peer: claimed.to_string(),
            fingerprint: chatter_mesh_noise::fingerprint_of(&static_key),
            signing_key,
            nickname,
        };
        let Ok(Some(state)) = self
            .private_channels
            .redeem(&invite, &self.identity, member)
        else {
            return;
        };
        let Ok(bytes) = state.update_bytes() else {
            return;
        };
        for member in &state.members {
            if member.peer != self.identity.peer_id().as_str() {
                self.send_group_state(
                    &PeerId::from_str(&member.peer),
                    &bytes,
                    member.peer == claimed.as_str(),
                )
                .await;
            }
        }
        self.events.send(MeshEvent::PrivateChannelChanged);
    }

    pub(super) async fn on_private_epoch(&mut self, claimed: &PeerId, mut update: PrivateChannel) {
        let Some(static_key) = self.state.with(|s| s.sessions.remote_static(claimed)) else {
            return;
        };
        if claimed.as_str() != update.creator
            || chatter_mesh_noise::fingerprint_of(&static_key) != update.members[0].fingerprint
            || !update.verify()
        {
            return;
        }
        update.creator_static = static_key.to_vec();
        let existing = self.private_channels.load(&update.id).ok().flatten();
        if let Some(ref old) = existing {
            if old.creator_key != update.creator_key
                || old.creator_static != update.creator_static
                || update.epoch < old.epoch
            {
                return;
            }
            if update.epoch == old.epoch {
                if update.signature != old.signature {
                    return;
                }
                if old.stale && old.stale_epoch <= old.epoch {
                    let mut recovered = old.clone();
                    recovered.stale = false;
                    recovered.stale_epoch = 0;
                    if self.private_channels.save(&recovered).is_ok() {
                        self.events.send(MeshEvent::PrivateChannelChanged);
                    }
                }
                self.send_private_epoch_ack(claimed, old).await;
                return;
            }
        } else {
            let accepted = self
                .private_channels
                .pending(&update.id)
                .ok()
                .flatten()
                .is_some_and(|invite| {
                    invite.creator == update.creator
                        && invite.creator_key == update.creator_key
                        && invite.creator_static == update.creator_static
                        && invite.target == self.identity.peer_id().as_str()
                });
            if !accepted {
                return;
            }
        }
        if update.member_key(&self.identity.peer_id()).is_none() {
            if existing.is_some() {
                let _ = self.private_channels.remove(&update.id);
                self.events.close_private_channel(&update.id);
                self.events.send(MeshEvent::PrivateChannelChanged);
            }
            return;
        }
        if let Some(old) = existing {
            update.replay = old.replay;
            if old.stale_epoch > update.epoch {
                update.stale = true;
                update.stale_epoch = old.stale_epoch;
            }
        }
        if self.private_channels.save(&update).is_err() {
            return;
        }
        let _ = self.private_channels.clear_pending(&update.id);
        self.events.open_private_channel(&update.id, &update.name);
        self.events.send(MeshEvent::PrivateChannelChanged);
        self.send_private_epoch_ack(claimed, &update).await;
    }

    pub(super) async fn on_private_epoch_hint(&mut self, claimed: &PeerId, proof: EpochProof) {
        let Ok(Some(mut state)) = self.private_channels.load(proof.channel_id()) else {
            return;
        };
        if claimed.as_str() != state.creator || !proof.verified_for(&state) {
            return;
        }
        let epoch = proof.epoch();
        if epoch > state.epoch {
            if !state.stale || epoch > state.stale_epoch {
                state.stale = true;
                state.stale_epoch = epoch;
                if self.private_channels.save(&state).is_ok() {
                    self.events.send(MeshEvent::PrivateChannelChanged);
                }
            }
        } else if state.stale && state.stale_epoch <= state.epoch && proof.matches(&state) {
            state.stale = false;
            state.stale_epoch = 0;
            if self.private_channels.save(&state).is_ok() {
                self.events.send(MeshEvent::PrivateChannelChanged);
            }
        }
    }

    pub(super) fn on_private_leave(&mut self, claimed: &PeerId, notice: LeaveNotice) -> bool {
        let Ok(Some(mut state)) = self.private_channels.load(&notice.id) else {
            return false;
        };
        if notice.peer != claimed.as_str() && state.member_key(claimed).is_none() {
            return false;
        }
        if state.epoch > notice.epoch && state.member_key(&PeerId::from_str(&notice.peer)).is_none()
        {
            // A newer creator-signed roster already excludes this peer.
            return true;
        }
        if !state.verify_leave(&notice) {
            return false;
        }
        if state.departed.iter().any(|old| old.peer == notice.peer) {
            return true;
        }
        state.departed.push(notice);
        if self.private_channels.save(&state).is_ok() {
            self.events.send(MeshEvent::PrivateChannelChanged);
            return true;
        }
        false
    }

    async fn send_private_epoch_ack(&mut self, claimed: &PeerId, update: &PrivateChannel) {
        use sha2_10::Digest;
        let mut ack = b"CHTRPCA1".to_vec();
        ack.extend_from_slice(
            uuid::Uuid::parse_str(&update.id)
                .expect("verified channel id")
                .as_bytes(),
        );
        ack.extend_from_slice(&update.epoch.to_be_bytes());
        ack.extend_from_slice(&sha2_10::Sha256::digest(&update.signature));
        self.send_private_control(claimed, &ack).await;
    }

    pub(super) fn on_private_epoch_ack(&mut self, claimed: &PeerId, bytes: &[u8]) {
        use sha2_10::Digest;
        if bytes.len() != 60 || !bytes.starts_with(b"CHTRPCA1") {
            return;
        }
        let Ok(id) = uuid::Uuid::from_slice(&bytes[8..24]) else {
            return;
        };
        let Ok(Some(mut state)) = self.private_channels.load(&id.to_string()) else {
            return;
        };
        if state.member_key(claimed).is_none()
            || !state
                .awaiting_rekey
                .iter()
                .any(|peer| peer == claimed.as_str())
            || bytes[24..28] != state.epoch.to_be_bytes()
            || bytes[28..60] != sha2_10::Sha256::digest(&state.signature)[..]
        {
            return;
        }
        state.awaiting_rekey.retain(|peer| peer != claimed.as_str());
        if self.private_channels.save(&state).is_ok() {
            self.events.send(MeshEvent::PrivateChannelChanged);
        }
    }
}
