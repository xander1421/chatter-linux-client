use super::*;

impl BleRuntime {
    // ------------------------------------------------------------- link events

    pub(super) async fn on_link_event(&mut self, event: LinkEvent) {
        match event {
            LinkEvent::Discovered {
                address,
                rssi,
                peer,
            } => self.on_discovered(&address, rssi, peer).await,
            LinkEvent::Connected {
                link,
                role,
                max_write,
            } => self.on_connected(link, role, max_write).await,
            LinkEvent::Disconnected { link } => self.on_disconnected(&link).await,
            LinkEvent::Data { link, bytes } => self.on_data(&link, bytes).await,
            LinkEvent::PeripheralRoleEnded => {
                // Not fatal and not recoverable from here: outbound links still
                // work, so the mesh is half alive, and that is precisely the state
                // worth saying out loud. A peer that dials us now gets nowhere and
                // has no way to know why.
                tracing::warn!(
                    target: "mesh::ble",
                    "this device can no longer be connected to; inbound links have \
                     stopped being served, and only outbound ones will form now"
                );
                self.degraded(
                    "Other devices can no longer connect to this one over Bluetooth. \
                     Restarting the app should restore it."
                        .to_string(),
                );
            }
            LinkEvent::AdapterPowerChanged { powered } => {
                self.state.with(|state| {
                    state.adapter_powered = powered;
                    if !powered {
                        // Every link dies with the adapter. Keeping the bindings
                        // would make the router believe it can still write.
                        state.links.clear();
                        state.max_write.clear();
                        state.address_of.clear();
                        state.connecting.clear();
                        state.scheduler.reset();
                    }
                });
                self.emit_status();
                self.emit_peers();
            }
        }
    }

    /// A device advertising the mesh service came into range.
    ///
    /// `peer` is the identity from the advert's service data, when it carried
    /// one. Recognising it here is the whole point: a private address rotates,
    /// and without the id the only way to learn that an unfamiliar address is a
    /// peer we already hold is to connect to it and read its announce. That
    /// connection is not free — it displaces the ACL we already had, and the
    /// disconnect makes the peer's advertiser restart under yet another address.
    /// Measured before this existed: nineteen addresses dialled in four minutes,
    /// every one of them the same phone.
    ///
    /// `None` means the scan response has not been seen yet, not that the device
    /// is a stranger, so it falls through to the ordinary scheduler path.
    ///
    /// Deliberately counts links of *either* role, unlike
    /// [`redundant_central_links_locked`], which prunes only central ones. The two
    /// asymmetries point the same way: a peer already reachable needs no second
    /// path, so an inbound link is reason enough not to dial out — but a link the
    /// peer dialled to us cost us no slot and is not ours to hang up. Measured
    /// consequence: where both devices are running, the phone usually dials first
    /// and the desktop then never dials at all, which is what a 25 minute run on a
    /// single peripheral link with zero warnings looks like.
    ///
    /// This does nothing at a cold start, when there are no links to recognise a
    /// peer by. Over five restarts, two produced a second link before the first
    /// announce arrived; the prune resolved each within seconds and no frame was
    /// lost. That residual is the price of identity arriving with the first
    /// announce rather than before it.
    pub(super) async fn on_discovered(&mut self, address: &str, rssi: i16, peer: Option<[u8; 8]>) {
        let now = now_millis();

        if let Some(peer) = peer {
            let peer = PeerId::from_bytes(&peer);
            let held = self.state.with(|state| {
                state
                    .links
                    .links()
                    .any(|(_, link)| link.peer.as_ref() == Some(&peer))
            });
            if held {
                tracing::trace!(
                    target: "mesh::ble",
                    %peer,
                    %address,
                    "another address for a peer we already hold; not dialling"
                );
                return;
            }
        }

        let decision = self.state.with(|state| {
            let candidate =
                chatter_mesh_link::scheduler::ConnectionCandidate::new(address, rssi, now);
            let connected_or_connecting =
                state.links.count_role(LinkRole::Central) + state.connecting.len();
            let existing = ExistingConnectionState {
                is_connecting: state.connecting.contains(address),
                is_connected: state
                    .address_of
                    .values()
                    .any(|known| known.as_str() == address),
                last_connect_attempt_ms: None,
            };
            let peripheral = if state.links.count_role(LinkRole::Peripheral) > 0 {
                PeripheralConnectionState::Connected
            } else {
                PeripheralConnectionState::Disconnected
            };
            state.scheduler.handle_discovery(
                &candidate,
                connected_or_connecting,
                Some(existing),
                peripheral,
                now,
            )
        });

        match decision {
            DiscoveryDecision::ConnectNow => self.dial(address).await,
            DiscoveryDecision::CancelStaleConnection => {
                self.state.with(|state| {
                    state.connecting.remove(address);
                });
            }
            // Queued and retry are both "drain later", which the tick does.
            DiscoveryDecision::Queued
            | DiscoveryDecision::ScheduleRetry { .. }
            | DiscoveryDecision::Ignore => {}
        }
    }

    /// Start a connect without blocking the loop; [`finish_dial`] applies it.
    pub(super) async fn dial(&mut self, address: &str) {
        // Adverts repeat every few hundred ms, so a skipped dial is retried by
        // the next discovery or queue drain rather than lost.
        if self.dial_in_flight {
            return;
        }
        self.dial_in_flight = true;
        let now = now_millis();
        self.state.with(|state| {
            state.connecting.insert(address.to_string());
            state.scheduler.note_connect_attempt(now);
        });
        let radio = Arc::clone(&self.radio);
        let tx = self.dial_tx.clone();
        let address = address.to_string();
        tokio::spawn(async move {
            let result = radio.connect(&address).await;
            // A closed receiver means the runtime has stopped; nothing to apply.
            let _ = tx.send(DialOutcome {
                address,
                started_ms: now,
                result,
            });
        });
    }

    pub(super) fn finish_dial(&mut self, outcome: DialOutcome) {
        self.dial_in_flight = false;
        let DialOutcome {
            address,
            started_ms: now,
            result,
        } = outcome;
        let address = address.as_str();
        match result {
            Ok(link) => {
                self.state.with(|state| {
                    // `Connected` can now be handled before this result: the
                    // link layer emits it from the connect itself.
                    if state.links.is_writable(&link) {
                        state.connecting.remove(address);
                    }
                    state.address_of.insert(link, address.to_string());
                    state.scheduler.note_connected(address);
                });
            }
            Err(error) => {
                let timed_out =
                    matches!(error, chatter_mesh_link::LinkError::ConnectTimedOut { .. });
                self.state.with(|state| {
                    state.connecting.remove(address);
                    // A timeout earns the long ignore window; an outright failure
                    // earns only the short one. `note_connect_timeout` existed and
                    // was never reachable from here, because the radio layer
                    // reported a timeout as an ordinary failure — so the window it
                    // documents had never once applied.
                    if timed_out {
                        state.scheduler.note_connect_timeout(address, now);
                    } else {
                        state.scheduler.note_connect_failed(address, now);
                    }
                });
                // A timeout is worth saying out loud; an outright failure is not.
                // A peer that walked out of range is ordinary churn and would
                // drown the log, but twenty seconds spent half-connected is the
                // symptom of the unexplained link death, and the error now names
                // the stage that hung. Debug is invisible on Android, which is
                // where the question gets asked.
                if timed_out {
                    tracing::warn!(target: "mesh::ble", %address, %error, "connect timed out");
                } else {
                    tracing::debug!(target: "mesh::ble", %address, %error, "connect failed");
                }
            }
        }
    }

    /// Tests drive the loop by hand: wait for the one dial in flight and apply it.
    #[cfg(test)]
    pub(super) async fn settle_dial(&mut self) {
        let outcome = self
            .dial_rx
            .as_mut()
            .expect("runtime not running")
            .recv()
            .await
            .expect("a dial was started");
        self.finish_dial(outcome);
    }

    async fn on_connected(&mut self, link: LinkId, role: LinkRole, max_write: usize) {
        self.link_last_inbound_ms.insert(link.clone(), now_millis());
        self.state.with(|state| {
            state.links.link_up(link.clone(), role);
            // `Connected` means writable: the trait's contract is that an
            // implementation withholds this event until the link can carry a
            // write, so the flag is set here rather than waited for.
            state.links.set_writable(&link, true);
            state.max_write.insert(link.clone(), max_write);
            if let Some(address) = state.address_of.get(&link).cloned() {
                state.connecting.remove(&address);
            }
        });
        self.emit_status();
        // Announce straight away: a peer that has just connected knows nothing
        // about us until one arrives, and the cadence timer may be seconds out.
        self.announce().await;
        self.flush_buffered_writes().await;
    }

    async fn on_disconnected(&mut self, link: &LinkId) {
        let now = now_millis();
        self.link_last_inbound_ms.remove(link);
        let peer = self.state.with(|state| {
            let (moved, discarded) = rescue_link_writes(state, link, None);
            let peer = state.links.link_down(link);
            state.max_write.remove(link);
            state.assembler.discard(link);
            if moved > 0 || !discarded.is_empty() {
                tracing::info!(target: "mesh::ble", %link, moved, discarded = discarded.len(),
                    "link closed with queued frames");
            }
            if let Some(peer) = &peer {
                state.dm_handshake_at_ms.remove(peer);
            }
            if let Some(address) = state.address_of.remove(link) {
                state.connecting.remove(&address);
                state.scheduler.note_disconnected(&address, now);
            }
            // A session outlives its link on purpose — the peer may come back on
            // another one — but the link's Noise authentication does not.
            state.links.clear_noise_authentication(link);
            peer
        });
        self.reconcile_peers();
        self.emit_status();
        if let Some(peer) = peer {
            tracing::debug!(target: "mesh::ble", peer = %peer, "link down");
            self.retry_pending_dm_handshake(&peer).await;
        }
    }

    pub(super) async fn on_data(&mut self, link: &LinkId, bytes: Vec<u8>) {
        let now = now_millis();
        self.link_last_inbound_ms.insert(link.clone(), now);
        let appended = self.state.with(|state| {
            let offset = state.assembler.buffered_bytes(link);
            state.last_traffic_ms = now;
            state.assembler.append(
                link,
                &[InboundChunk { offset, bytes }],
                InboundLimits::default(),
                now,
            )
        });

        // Both discard paths report the numbers that tell them apart, and both at
        // warn. The metadata was already being assembled and then thrown away with
        // `..`, which is how a peer whose frames never land looks identical to a
        // peer that never sent any. `received=56 expected=70` is what made the
        // fragment-expiry bug findable; the same applies here.
        match appended {
            InboundAppend::Decoded { packet, metadata } => {
                report_stall_discard(link, &metadata);
                self.on_packet(link, *packet).await
            }
            InboundAppend::Waiting { metadata } => report_stall_discard(link, &metadata),
            InboundAppend::Oversized { metadata } => {
                tracing::warn!(
                    target: "mesh::ble",
                    link = %link,
                    needed_bytes = metadata.accumulated_bytes,
                    cap_bytes = InboundLimits::default().cap_bytes,
                    appended_bytes = metadata.appended_bytes,
                    packet_type = ?metadata.packet_type,
                    "inbound frame over the cap; the link's buffer was dropped"
                );
            }
        }
    }

    // ----------------------------------------------------------------- inbound

    pub(super) async fn on_packet(&mut self, link: &LinkId, packet: BitchatPacket) {
        let now_ms = now_millis();
        let channel_fragment = if packet.message_type() == Some(MessageType::Fragment) {
            chatter_mesh::fragment::FragmentHeader::parse(&packet)
                .ok()
                .filter(|(header, _)| header.original_type == MessageType::ChannelMessage.as_u8())
        } else {
            None
        };
        if packet.message_type() == Some(MessageType::ChannelMessage) || channel_fragment.is_some()
        {
            if packet.message_type() == Some(MessageType::ChannelMessage)
                && packet.payload.len() > 290
            {
                return;
            }
            if channel_fragment
                .as_ref()
                .is_some_and(|(header, _)| header.total > 8)
            {
                return;
            }
            if channel_fragment
                .as_ref()
                .is_some_and(|(_, chunk)| chunk.len() > 512)
            {
                return;
            }
            let key = link.to_string();
            self.channel_ingress
                .retain(|_, (start, _)| now_ms.saturating_sub(*start) < 60_000);
            if self.channel_ingress.len() >= 128 && !self.channel_ingress.contains_key(&key) {
                return;
            }
            let budget = self.channel_ingress.entry(key).or_insert((now_ms, 0));
            if budget.1 >= CHANNEL_INGRESS_PER_MINUTE {
                return;
            }
            budget.1 += 1;
        }
        let private_fragment = if packet.message_type() == Some(MessageType::Fragment) {
            chatter_mesh::fragment::FragmentHeader::parse(&packet)
                .ok()
                .filter(|(header, _)| {
                    header.original_type == MessageType::ChatterPrivateChannel.as_u8()
                })
        } else {
            None
        };
        if packet.message_type() == Some(MessageType::ChatterPrivateChannel)
            || private_fragment.is_some()
        {
            if packet.message_type() == Some(MessageType::ChatterPrivateChannel)
                && packet.payload.len() > 1200
            {
                return;
            }
            if private_fragment
                .as_ref()
                .is_some_and(|(header, chunk)| header.total > 8 || chunk.len() > 512)
            {
                return;
            }
            let key = link.to_string();
            self.private_ingress
                .retain(|_, (start, _)| now_ms.saturating_sub(*start) < 60_000);
            if self.private_ingress.len() >= 128 && !self.private_ingress.contains_key(&key) {
                return;
            }
            let budget = self.private_ingress.entry(key).or_insert((now_ms, 0));
            if budget.1 >= CHANNEL_INGRESS_PER_MINUTE {
                return;
            }
            budget.1 += 1;
        }
        let now_s = now_secs();
        let local = self.identity.peer_id();
        let claimed = PeerId::from_bytes(&packet.sender_id);
        let bound = self.state.with(|state| state.links.peer_for(link).cloned());

        // Which peer's outstanding request this could be answering.
        //
        // For a sync response that is the peer on the *link* — the one that
        // forwarded it — not the ID stamped on the packet. A gossip response
        // carries another peer's packet verbatim, so its sender is the original
        // author and can be anybody, including us. Asking "do we have a request
        // out to the author" therefore answered a question nobody had, and every
        // response was rejected as unsolicited: the guard names the link peer in
        // its rejection while this had computed the answer for the author.
        //
        // `IngressGuard::evaluate` picks the same peer for the same reason and
        // passes it to the closure below; the closure cannot use it directly only
        // because `self.gossip` may not be borrowed inside `self.state.with`.
        let validation_peer = if packet.is_rsr {
            bound.clone().unwrap_or_else(|| claimed.clone())
        } else {
            claimed.clone()
        };
        let solicited = self
            .gossip
            .is_valid_response(&validation_peer, packet.is_rsr, now_s);

        // Copied out so the closure below borrows only the state, not the whole
        // runtime.
        let guard = self.guard;
        let accepted = self.state.with(|state| {
            guard.evaluate(
                IngressInput {
                    packet: &packet,
                    claimed_sender: &claimed,
                    bound_peer: bound.as_ref(),
                    local_peer: &local,
                    now_unix_millis: now_ms,
                },
                &state.self_broadcasts,
                |_| solicited,
            )
        });

        let (received_from, stale_sync_answer) = match accepted {
            Ok(accepted) => (accepted.received_from, false),
            Err(rejection) => {
                // Every check before the timestamp passed: the guard decides
                // freshness last. See `stale_sync_answer_from` for the one case
                // let through.
                let admitted = match &rejection {
                    IngressRejection::TimestampSkew { .. } => self.stale_sync_answer_from(
                        bound.as_ref(),
                        &packet,
                        &claimed,
                        now_ms,
                        now_s,
                    ),
                    _ => None,
                };
                let Some(peer) = admitted else {
                    // A packet addressed to us that we refuse is a conversation
                    // that silently stops (a Noise opening from a restarted peer
                    // never answered); debug is invisible on Android.
                    let addressed_here = packet
                        .recipient_id
                        .is_some_and(|id| PeerId::from_bytes(&id) == local);
                    if addressed_here {
                        tracing::info!(
                            target: "mesh::ble",
                            %rejection,
                            msg_type = packet.msg_type,
                            claimed_sender = %claimed,
                            bound_peer = ?bound,
                            "directed packet rejected at ingress"
                        );
                    }
                    // `pending` is the whole of the RSR decision: a response is
                    // exempt from the freshness check only while we have an
                    // outstanding request to that peer. Printing the count next to
                    // the rejection is what distinguishes "we never asked" from
                    // "we asked and the bookkeeping lost it".
                    tracing::debug!(
                        target: "mesh::ble",
                        %rejection,
                        is_rsr = packet.is_rsr,
                        claimed_sender = %claimed,
                        pending_sync_peers = ?self.gossip.pending_request_peers(),
                        "packet rejected at ingress"
                    );
                    return;
                };
                tracing::debug!(
                    target: "mesh::ble",
                    from = %peer,
                    author = %claimed,
                    age_ms = now_ms.saturating_sub(packet.timestamp),
                    "stale sync answer admitted"
                );
                (peer, true)
            }
        };

        // Dedup before anything with a side effect. A flooded mesh delivers the
        // same packet on several links, and processing each one duplicates the
        // message and re-relays it.
        let duplicate = self.state.with(|state| {
            let key = MessageDeduplicator::dedup_key(&packet);
            if !MessageDeduplicator::should_deduplicate(&packet, &local) {
                return false;
            }
            if state.dedup.is_duplicate(&key, now_s) {
                return true;
            }
            state.dedup.mark_processed(&key, now_s);
            false
        });
        if duplicate {
            return;
        }

        self.state.with(|state| {
            state.ingress.record_if_new(
                &packet,
                link.clone(),
                received_from.clone(),
                now_ms,
                chatter_mesh_link::registry::INGRESS_RECORD_LIFETIME_MS,
            );
            state.peers.update_last_seen(&received_from, now_ms);
        });

        let retainable = self.dispatch(link, &packet, &claimed, now_s).await;

        // Refused identity-control packets must not be propagated to peers.
        if !retainable {
            return;
        }
        if packet.is_broadcast() {
            self.gossip.on_public_packet_seen(&packet, now_s);
        }
        // TTL 0 already stops the relay; this does not depend on that.
        if stale_sync_answer {
            return;
        }
        self.relay(link, &packet, &local).await;
    }

    /// The link peer, when a packet the guard refused only as too old is an
    /// answer to our sync request from a bitchat-android peer.
    ///
    /// bitchat-android answers with TTL 0 and no RSR flag
    /// (`GossipSyncManager.kt`, `pkt.copy(ttl = SYNC_TTL_HOPS)`), so any public
    /// message more than two minutes old that we lack failed the skew check,
    /// was never filed, and came back every round. Admitted only when all hold,
    /// and refused otherwise:
    ///
    /// - TTL 0 and no RSR flag: the shape of that answer. RSR keeps its own path.
    /// - A broadcast `Message`, the only type bitchat-android serves that we
    ///   would display. Announces have their own age policy; fragments skip the
    ///   dedup cache, so a replay of one could not be recognised.
    /// - In the past. A future timestamp past the skew stays refused.
    /// - Not our own: the guard has already refused that, checked again here.
    /// - On a link bound to a peer with a pinned, verified announce, and we have
    ///   a sync request outstanding to that peer.
    /// - Inside the retention window our own store keeps for the type.
    /// - Not already held by the store. With the dedup cache in `on_packet`, a
    ///   replay is dropped.
    /// - Signed by its author's announced key, so a forgery is not filed and
    ///   served onward.
    ///
    /// The caller never relays it.
    fn stale_sync_answer_from(
        &self,
        bound: Option<&PeerId>,
        packet: &BitchatPacket,
        claimed: &PeerId,
        now_ms: u64,
        now_s: u64,
    ) -> Option<PeerId> {
        if packet.ttl != 0 || packet.is_rsr {
            return None;
        }
        if packet.message_type() != Some(MessageType::Message) || !packet.is_broadcast() {
            return None;
        }
        if packet.timestamp >= now_ms || *claimed == self.identity.peer_id() {
            return None;
        }
        let peer = bound?;
        let author_key = self.state.with(|state| {
            if state.peers.pinned(peer).is_none() {
                return None;
            }
            state
                .peers
                .info(claimed)
                .and_then(|info| info.signing_public_key.clone())
        })?;
        if !self.gossip.awaits_answer_from(peer, now_s)
            || !self.gossip.within_retention(packet, now_s)
            || self.gossip.holds(packet)
        {
            return None;
        }
        let key = <[u8; 32]>::try_from(author_key.as_slice()).ok()?;
        verify_packet(packet, &key).then(|| peer.clone())
    }

    /// Unwrap a fragment, then handle whatever it turns out to be.
    ///
    /// Deliberately not recursive. The fragmenter never nests fragments, so one
    /// level of unwrapping is the whole truth — and expressing it as one level
    /// means a hostile peer cannot make us descend at all.
    ///
    /// Returns whether the packet may be retained and re-gossiped. Only an
    /// announce or unverified Leave says no — see [`Self::handle_typed`].
    async fn dispatch(
        &mut self,
        link: &LinkId,
        packet: &BitchatPacket,
        claimed: &PeerId,
        now_s: u64,
    ) -> bool {
        if packet.message_type() != Some(MessageType::Fragment) {
            return self.handle_typed(link, packet, claimed).await;
        }

        let reassembled = self
            .state
            .with(|state| state.fragments.accept(packet, now_s));
        let Ok(Some(bytes)) = reassembled else {
            return true;
        };
        let Ok(inner) = codec::decode(&bytes) else {
            return true;
        };
        if inner.message_type() == Some(MessageType::Fragment) {
            // A fragment carrying a fragment is not something the fragmenter
            // produces; it is someone trying to make us loop.
            return true;
        }
        self.handle_typed(link, &inner, claimed).await
    }

    /// Handle one decoded packet, and say whether the gossip store may keep it.
    ///
    /// The flag exists for announces alone. An announce we refused is an identity
    /// forgery, and retaining it would make this device re-serve that forgery to
    /// every peer that syncs with us; worse, `on_public_packet_seen` reads a
    /// *stale* announce as "this peer left" and purges the store's state for the
    /// claimed sender, so an unauthenticated old announce is a remote wipe of a
    /// real peer's gossip state. Genuine staleness is still collected by
    /// `cleanup_stale_announcements_if_needed` on the maintenance tick, which
    /// needs no packet from anyone to do its job.
    pub(super) async fn handle_typed(
        &mut self,
        link: &LinkId,
        packet: &BitchatPacket,
        claimed: &PeerId,
    ) -> bool {
        match packet.message_type() {
            Some(MessageType::Announce) => return self.on_announce(link, packet, claimed).await,
            Some(MessageType::Message) => self.on_public_message(packet, claimed),
            Some(MessageType::ChannelMessage) => return self.on_channel_message(packet, claimed),
            Some(MessageType::ChatterPrivateChannel) => {
                return self.on_private_channel_message(packet, claimed);
            }
            Some(MessageType::Leave) => {
                let verified = self.state.with(|state| {
                    state
                        .peers
                        .pinned(claimed)
                        .and_then(|pin| {
                            <[u8; 32]>::try_from(pin.signing_public_key.as_slice()).ok()
                        })
                        .is_some_and(|key| verify_packet(packet, &key))
                });
                if !verified {
                    return false;
                }
                self.state.with(|state| {
                    state.peers.remove(claimed);
                    state.sessions.forget(claimed);
                });
                self.gossip.remove_announcement_for_peer(claimed);
                self.emit_peers();
            }
            Some(MessageType::NoiseHandshake) => self.on_handshake(link, packet, claimed).await,
            Some(MessageType::NoiseEncrypted) => self.on_encrypted(link, packet, claimed).await,
            Some(MessageType::ChatterCarrier) => {
                return self.on_signal_carrier(packet, claimed).await;
            }
            Some(MessageType::CourierEnvelope) => self.on_courier_envelope(packet, claimed).await,
            Some(MessageType::RequestSync) => self.on_request_sync(packet, claimed).await,
            Some(MessageType::FileTransfer) => self.on_file_transfer(packet, claimed),
            Some(MessageType::FileManifest) => self.on_file_manifest(packet, claimed),
            Some(MessageType::FileChunk) => self.on_file_chunk(packet, claimed),
            Some(MessageType::FileChunkRequest) => self.on_file_chunk_request(packet, claimed),
            Some(MessageType::PrekeyBundle) => return self.on_prekey_bundle(packet),
            // Boards, groups, voice and prekey bundles were deferred from P0.
            // Not handling them is right; not *relaying* them would partition
            // every peer that does implement them, so the relay decision in
            // `on_packet` still runs.
            _ => {}
        }
        true
    }
}
