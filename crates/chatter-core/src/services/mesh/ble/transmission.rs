use super::*;

impl BleRuntime {
    pub(super) async fn announce(&mut self) {
        let now_ms = now_millis();
        let passed = self.state.with(|state| {
            let passed = duty::announce_floor_passed(state.last_announce_ms, now_ms);
            // Inside the floor the request is deferred, not dropped: a peer that
            // connected a moment after another must still hear us, and
            // `flush_deferred_announce` sends one at expiry. One flag, so any
            // number of requests inside a floor coalesce into that one.
            state.announce_deferred = !passed;
            passed
        });
        if !passed {
            return;
        }
        let nickname = self.config.nickname();
        let mut capabilities = PeerCapabilities::from_bits(0);
        if self.config.bridge_geohash.is_some() {
            capabilities.insert(PeerCapabilities::BRIDGE);
        }
        // We serve one-time prekeys only while our store is loaded.
        if self.state.with(|state| state.prekeys.local.is_some()) {
            capabilities.insert(PeerCapabilities::PREKEYS);
        }
        let announcement = AnnouncementPacket::new(
            nickname.clone(),
            self.identity.static_public().to_vec(),
            self.identity.signing_public().to_vec(),
            None,
            Some(capabilities),
            self.config.bridge_geohash.clone(),
        );
        let payload = match announcement.encode() {
            Ok(payload) => payload,
            Err(error) => {
                // Silent before. An announce that cannot be encoded means this
                // device is invisible to every peer forever, which is the single
                // worst failure the mesh has and deserves to be loud.
                tracing::error!(
                    target: "mesh::ble",
                    %error,
                    nickname = %nickname,
                    "the announce could not be encoded; this device is invisible"
                );
                return;
            }
        };

        let mut packet = self.build_packet(
            MessageType::Announce,
            None,
            payload,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        packet.timestamp = now_ms;
        // Signed over the packet preimage, not over `canonicalAnnounceBytes`.
        // bitchat has both, but `buildAnnounceSignature` has no callers outside
        // its own tests — the wire uses `noiseService.signPacket(packet)`
        // (`BLEService.swift:1685`), and an announce signed the other way is
        // dropped by every real peer before it reaches their registry.
        let packet = self.maybe_sign(packet, true);

        self.state.with(|state| state.last_announce_ms = now_ms);
        self.originate_broadcast(&packet).await;
        if now_ms.saturating_sub(self.last_channel_presence_ms) >= 60_000 {
            self.last_channel_presence_ms = now_ms;
            for name in self.events.active_channels() {
                self.broadcast_channel_presence(&name).await;
            }
        }
    }

    /// Every *other* link bound to the peer a packet arrived on.
    /// Send the announce that was requested inside the floor, once it expires.
    pub(super) async fn flush_deferred_announce(&mut self, now_ms: u64) {
        let due = self.state.with(|state| {
            state.announce_deferred && duty::announce_floor_passed(state.last_announce_ms, now_ms)
        });
        if due {
            self.announce().await;
        }
    }

    fn sibling_links_of(&self, ingress_link: Option<&LinkId>) -> Vec<LinkId> {
        let Some(ingress) = ingress_link else {
            return Vec::new();
        };
        self.state
            .with(|state| sibling_links_locked(state, ingress))
    }

    /// Sign a packet over its own preimage, when the caller asked for it.
    pub(super) fn maybe_sign(&self, mut packet: BitchatPacket, sign: bool) -> BitchatPacket {
        if sign && let Ok(signature) = sign_packet(&self.identity, &packet) {
            packet.signature = Some(signature);
        }
        packet
    }

    pub(super) fn build_packet(
        &self,
        msg_type: MessageType,
        recipient: Option<&PeerId>,
        payload: Vec<u8>,
        ttl: u8,
    ) -> BitchatPacket {
        let sender_id = self
            .identity
            .peer_id()
            .to_routing_bytes()
            .unwrap_or([0u8; SENDER_ID_LEN]);
        // Broadcast is encoded by *omitting* the recipient, which is what
        // bitchat sends (`recipientID: nil`). The explicit all-`0xFF` form is
        // also accepted by them, but the omitted form is what their own packets
        // carry, so sending it keeps our traffic byte-comparable with theirs.
        let recipient_id = recipient.and_then(PeerId::to_routing_bytes);
        BitchatPacket::new(
            msg_type,
            sender_id,
            recipient_id,
            now_millis(),
            payload,
            ttl,
        )
    }

    /// Put one of our own broadcasts on the air, filed in the gossip store first.
    ///
    /// Every packet we originate for the room goes through here. Filing it is
    /// what puts it in our sync filter: without that, each sync round tells
    /// every peer we lack our own messages, and they send them back — forever,
    /// because bitchat-android's answers carry no RSR flag and the ingress guard
    /// drops them as our own traffic before anything could store them. bitchat
    /// files its own sends the same way (`BluetoothMeshService.kt`,
    /// `onPublicPacketSeen(signedPacket)`). Types the store does not carry
    /// (manifests, chunks, chunk requests) are ignored by it.
    ///
    /// Our own announce is the one exception, a deliberate departure from
    /// bitchat: the store's stale-peer sweep purges every carried packet of a
    /// peer whose announce has aged past `STALE_PEER_TIMEOUT_SECS`, and filing
    /// ours would aim that sweep at our own messages between announces.
    pub(super) async fn originate_broadcast(&mut self, packet: &BitchatPacket) {
        if packet.message_type() != Some(MessageType::Announce) {
            self.gossip.on_public_packet_seen(packet, now_secs());
        }
        self.transmit(packet, None, None).await;
    }

    /// Plan and write one packet.
    pub(super) async fn transmit(
        &self,
        packet: &BitchatPacket,
        directed_peer: Option<&PeerId>,
        ingress_link: Option<&LinkId>,
    ) -> bool {
        let padding = fragment::pads_ble_frame(packet.msg_type);
        let Ok(encoded) = codec::encode(packet, padding) else {
            return false;
        };

        // Split horizon has to be by *peer*, not only by link. Two devices can hold
        // two links to each other at once — each having connected outward to the
        // other — and excluding just the ingress link then leaves the sibling link
        // to the same peer selected, so a relay sends the packet straight back to
        // where it came from.
        //
        // That is not theoretical. On a two-node mesh it turned every broadcast
        // into an echo: the sender logged 250 expired reassemblies of *its own*
        // chunks against 14 real ones, roughly seventeen thousand fragments of its
        // own traffic coming back and occupying the air the transfer needed.
        //
        // `excluded_links` exists for exactly this — its own documentation says
        // "every other link to the ingress peer" — and nothing had ever populated
        // it.
        let siblings = self.sibling_links_of(ingress_link);

        let (plan, frames) = {
            let links = self.state.link_candidates();
            let request = OutboundPlanRequest::new(packet, encoded.len(), &links)
                .with_ingress_link(ingress_link)
                .with_excluded_links(&siblings)
                .with_directed_only_peer(directed_peer)
                .with_direct_announce_ttl(DIRECT_ANNOUNCE_TTL);
            let plan = planner::plan(&request);

            let frames = match plan.fragment_chunk_size {
                Some(chunk) => {
                    let mut id = [0u8; 8];
                    id.copy_from_slice(&packet.packet_id()[..8]);
                    match fragment::plan_fragments(packet, chunk, id) {
                        Ok(fragments) => fragments
                            .iter()
                            .filter_map(|f| {
                                codec::encode(f, fragment::pads_ble_frame(f.msg_type)).ok()
                            })
                            .collect(),
                        Err(_) => Vec::new(),
                    }
                }
                None => vec![encoded],
            };
            (plan, frames)
        };

        if plan.should_spool_directed
            && let Some(peer) = plan.directed_peer.clone()
        {
            // Directed, with no link to go down. Holding it lets an ordinary
            // link gap heal instead of dropping the packet.
            self.state.with(|state| {
                state.spool.enqueue(
                    packet.clone(),
                    peer,
                    planner::message_id(packet),
                    now_secs(),
                );
            });
            return false;
        }

        if plan.selected.is_empty() {
            // Not an error — a broadcast with no links yet is normal on a quiet
            // mesh. Logged because the alternative is indistinguishable from a
            // packet that went out and was ignored, and telling those two apart
            // is most of what bringing up a second device consists of.
            tracing::debug!(
                target: "mesh::ble",
                msg_type = packet.msg_type,
                links = self.state.link_candidates().len(),
                "nothing to write to; packet not sent"
            );
            return false;
        }
        tracing::debug!(
            target: "mesh::ble",
            msg_type = packet.msg_type,
            links = plan.selected.len(),
            frames = frames.len(),
            "writing"
        );

        let priority = planner::priority(packet);
        let write_priority = chatter_mesh_link::backpressure::WritePriority {
            level: priority.level,
            suborder: priority.suborder,
        };

        // Queued, never written straight out — and this is the whole of the
        // pacing now. `flush_buffered_writes` takes one frame per link per
        // `WRITE_PACING_DELAY`, which is the rate the radio actually carries.
        //
        // Writing them back to back here is what broke large transfers. BlueZ
        // accepts frames far faster than the link can carry them and discards the
        // excess below the socket, silently, so nothing on either side reported a
        // loss: the receiver simply saw a fraction of each chunk. Android's bridge
        // does push back, which is why the same code worked in one direction and
        // not the other. Spacing every frame on this side is what makes both
        // directions behave, and it needs no estimate of the link's speed.
        //
        // bitchat reaches the same place from the other end — `interFragmentDelayMs
        // = 20` between fragments in `FragmentingPacketSender`. It can sleep inside
        // its send loop because that loop is a coroutine per transfer; this one is
        // the single event loop that also has to service link events, so the wait
        // happens in the drain instead.
        //
        // The cost is that a one-frame packet waits up to one interval too. That is
        // the price of not having to decide per packet which ones are large.
        let mut admitted_all = true;
        for link in plan.selected.iter() {
            for frame in &frames {
                let admitted = self.state.with(|state| {
                    state.writes.enqueue(
                        link,
                        frame.clone(),
                        write_priority,
                        PENDING_WRITE_BUFFER_CAP_BYTES,
                    )
                });
                // Byte-capped, so this can refuse. A refusal is a dropped packet
                // and has to be said out loud — which is what `#[must_use]` on
                // `WriteEnqueue` is for.
                if !matches!(admitted, WriteEnqueue::Enqueued { .. }) {
                    admitted_all = false;
                    tracing::warn!(
                        target: "mesh::ble",
                        link = %link,
                        msg_type = packet.msg_type,
                        "the outbound queue is full; a frame was dropped"
                    );
                }
            }
        }
        admitted_all
    }

    pub(super) async fn relay(&self, ingress: &LinkId, packet: &BitchatPacket, local: &PeerId) {
        let degree = self.state.with(|state| state.peers.connected_count());
        let sender_is_self = PeerId::from_bytes(&packet.sender_id) == *local;
        let recipient_is_self = self.addressed_to_us(packet);
        let input = RelayInput::from_packet(
            packet,
            sender_is_self,
            recipient_is_self,
            degree,
            chatter_mesh::relay::HIGH_DEGREE_THRESHOLD,
        );
        let mut jitter = |low: u32, high: u32| -> u32 {
            if high <= low {
                return low;
            }
            low + (now_millis() as u32) % (high - low)
        };
        let decision = relay::decide(&input, &mut jitter);
        if !decision.should_relay {
            return;
        }

        let mut forwarded = packet.clone();
        forwarded.ttl = decision.new_ttl;
        // The delay is collision avoidance on a shared channel, so it has to be
        // a real wait rather than a hint. Spawned so the loop keeps servicing the
        // radio while it elapses.
        let state = Arc::clone(&self.state);
        let ingress = ingress.clone();
        let delay = u64::from(decision.delay_ms);
        tokio::spawn(async move {
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            transmit_forwarded(&state, &forwarded, &ingress).await;
        });
    }
}
