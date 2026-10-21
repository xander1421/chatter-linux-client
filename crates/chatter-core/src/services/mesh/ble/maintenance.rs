use super::*;

impl BleRuntime {
    // -------------------------------------------------------------------- tick

    pub(super) async fn on_tick(&mut self) {
        let now_ms = now_millis();
        let now_s = now_secs();
        if now_ms.saturating_sub(self.last_private_rekey_ms) >= 10_000 {
            self.last_private_rekey_ms = now_ms;
            if let Ok(channels) = self.private_channels.list() {
                for mut channel in channels {
                    if channel.creator == self.identity.peer_id().as_str()
                        && !channel.departed.is_empty()
                    {
                        let leaving: Vec<_> = channel
                            .departed
                            .iter()
                            .map(|notice| PeerId::from_str(&notice.peer))
                            .collect();
                        for peer in leaving {
                            if peer.as_str() == channel.creator {
                                continue;
                            }
                            if channel.rotate(&self.identity, None, Some(&peer)).is_err() {
                                break;
                            }
                        }
                        if channel.rotate_sender(&self.identity).is_err()
                            || self.private_channels.save(&channel).is_err()
                        {
                            continue;
                        }
                        self.events.send(MeshEvent::PrivateChannelChanged);
                    }
                    if !channel.stale
                        && channel.member_key(&self.identity.peer_id()).is_some()
                        && !channel.awaiting_rekey.is_empty()
                        && let Ok(bytes) = channel.update_bytes()
                    {
                        for peer in &channel.awaiting_rekey {
                            self.send_private_control(&PeerId::from_str(peer), &bytes)
                                .await;
                        }
                    }
                    if let Some(distribution) = channel
                        .distribution(&self.identity)
                        .and_then(|d| d.encode())
                    {
                        let peers = channel.pending_distributions.clone();
                        for peer in peers {
                            if !channel.mark_distribution_attempt(&peer)
                                || self.private_channels.save(&channel).is_err()
                            {
                                continue;
                            }
                            self.send_private_control(&PeerId::from_str(&peer), &distribution)
                                .await;
                        }
                    }
                }
            }
        }
        let pending_peers = self
            .state
            .with(|state| state.pending_dms.keys().cloned().collect::<Vec<_>>());
        for peer in pending_peers {
            self.retry_pending_dm_handshake(&peer).await;
        }

        self.flush_deferred_announce(now_ms).await;

        self.report_throughput(now_ms);

        // Every tick, not on the cleanup cadence: the stall window is 250 ms and
        // the tick 500 ms, so this is the coarsest sweep that still enforces it.
        self.reap_stalled_partials(now_ms);

        // Scan duty cycle. Scanning is the expensive half of BLE, so it runs
        // intermittently while advertising stays up continuously.
        let configured_duty = self.config.scan_duty_cycle;
        // Changing discovery while an LE connect is pending makes BlueZ abort
        // the connect (`le-connection-abort-by-local`). Defer the duty cycle,
        // state included, until the dial reports back; the next tick resumes it.
        let scan = if self.dial_in_flight {
            ScanCommand::Unchanged
        } else {
            self.state.with(|state| {
                let connected = state.peers.connected_count();
                let recent_traffic = now_ms.saturating_sub(state.last_traffic_ms)
                    < duty::RECENT_TRAFFIC_FORCE_SCAN_MS;
                let duty_enabled = state.duty_enabled && configured_duty;
                let command = state.duty.update(
                    duty_enabled,
                    state.app_is_active,
                    connected,
                    recent_traffic,
                    now_ms,
                );
                match command {
                    ScanCommand::Unchanged => state.duty.poll(now_ms),
                    other => other,
                }
            })
        };
        // A refused scan toggle was discarded, and the two directions fail in
        // opposite ways: unable to *start* means we stop discovering peers
        // altogether — the mesh goes blind while looking perfectly healthy —
        // and unable to stop means the duty cycle silently stops saving power.
        // Neither is inferable from any other line.
        match scan {
            ScanCommand::StartScanning => {
                if let Err(error) = self.radio.set_scanning(true).await {
                    tracing::warn!(
                        target: "mesh::ble",
                        %error,
                        "could not start scanning; no new peer will be discovered"
                    );
                }
            }
            ScanCommand::StopScanning => {
                if let Err(error) = self.radio.set_scanning(false).await {
                    tracing::warn!(
                        target: "mesh::ble",
                        %error,
                        "could not stop scanning; the duty cycle is not saving power"
                    );
                }
            }
            ScanCommand::Unchanged => {}
        }

        // Drain the connection queue: discoveries that were queued rather than
        // dialled immediately.
        let next = self.state.with(|state| {
            let connected_or_connecting =
                state.links.count_role(LinkRole::Central) + state.connecting.len();
            if connected_or_connecting >= MAX_CENTRAL_LINKS {
                return QueueDecision::Idle;
            }
            let connecting = state.connecting.clone();
            state.scheduler.next_candidate(
                connected_or_connecting,
                |address| connecting.contains(address),
                now_ms,
            )
        });
        if let QueueDecision::Connect(candidate) = next {
            self.dial(&candidate.address.clone()).await;
        }

        // Announce cadence, cleanup, and the directed spool.
        let plan = self.state.with(|state| {
            let connected = state.peers.connected_count();
            let elapsed = now_ms.saturating_sub(state.last_announce_ms);
            let recent_traffic =
                now_ms.saturating_sub(state.last_traffic_ms) < duty::MAINTENANCE_RECENT_TRAFFIC_MS;
            let cycle = state.maintenance_cycle;
            let plan = duty::maintenance_plan(
                cycle,
                connected,
                state.peers.is_empty(),
                elapsed,
                recent_traffic,
                duty::no_jitter,
            );
            state.maintenance_cycle = if plan.should_reset_counter {
                0
            } else {
                cycle.saturating_add(1)
            };
            plan
        });

        if plan.should_send_announce {
            self.announce().await;
        }
        if plan.should_ensure_advertising {
            // Nobody can see us. Advertising is the first suspect, and a restart
            // is the only lever the trait offers.
            //
            // Reported through `degraded` rather than swallowed, because this is
            // the *recovery* path: if it fails, the peripheral role stays
            // invisible and the one mechanism meant to fix that has just given up
            // in silence. A user watching an empty peer list has no other way to
            // learn the difference between an empty room and a radio that will
            // not advertise.
            if let Err(error) = self.radio.start().await {
                self.degraded(format!(
                    "nobody can see us and the radio would not restart to fix it: {error}"
                ));
            }
        }
        if plan.should_run_cleanup {
            self.cleanup(now_ms, now_s);
        }
        if plan.should_flush_directed_spool {
            self.flush_directed_spool(now_s).await;
        }

        self.flush_buffered_writes().await;

        // Chunked file transfers, which nothing else can schedule either: the
        // send side is paced by this tick rather than by a loop over the file, and
        // the receive side has no other way to notice that a transfer went quiet.
        self.pump_streamed_files(now_s).await;
        self.request_missing_chunks(now_s).await;

        self.maintain_prekeys(now_ms).await;

        // Gossip sync, which nothing else can schedule.
        let connected: Vec<PeerId> = self.state.with(|state| state.peers.connected_peer_ids());
        match self.gossip.maintenance(&connected, now_s) {
            Ok(sends) => {
                for send in sends {
                    self.dispatch_sync(send).await;
                }
            }
            Err(error) => self.degraded(format!("gossip sync maintenance failed: {error}")),
        }
        if let Err(error) = self.gossip.persist_if_dirty(now_s) {
            self.degraded(format!("gossip archive could not be persisted: {error}"));
        }

        let _ = self.router.send(RouterCommand::Maintenance);
    }

    /// Drop partial inbound frames that have sat past the stall window.
    ///
    /// `append` only discards a stalled frame when the same link writes again, so
    /// a peer that sends one byte and goes silent — still connected, never
    /// writing — would pin its partial until disconnect. This is the sweep that
    /// reclaims it.
    fn reap_stalled_partials(&mut self, now_ms: u64) {
        let reaped = self.state.with(|state| {
            state
                .assembler
                .reap_stalled(InboundLimits::default(), now_ms)
        });
        for stalled in reaped {
            tracing::warn!(
                target: "mesh::ble",
                link = %stalled.link,
                discarded_bytes = stalled.discarded_bytes,
                idle_ms = stalled.idle_ms,
                "a partial inbound frame stalled past its window and was reclaimed"
            );
        }
    }

    pub(super) fn cleanup(&mut self, now_ms: u64, now_s: u64) {
        self.state.with(|state| {
            state.dedup.cleanup(now_s);
            for expired in state.fragments.evict_expired(now_s) {
                tracing::warn!(
                    target: "mesh::ble",
                    peer = %hex_encode(&expired.sender),
                    received = expired.received,
                    expected = expired.expected,
                    original_type = expired.original_type,
                    is_broadcast = expired.is_broadcast,
                    lifetime_secs = chatter_mesh::fragment::FRAGMENT_LIFETIME_SECS,
                    "an incomplete reassembly expired; a packet was sent but never \
                     finished arriving"
                );
            }
            state.ingress.prune(
                now_ms,
                chatter_mesh_link::registry::INGRESS_RECORD_LIFETIME_MS,
            );
            state.self_broadcasts.prune(
                now_ms.saturating_sub(chatter_mesh_link::guard::SELF_BROADCAST_RETENTION_MILLIS),
            );
            state.scheduler.prune_backoff(now_ms);
            state.spool.prune_expired(now_s, DIRECTED_SPOOL_WINDOW_SECS);
        });
        if let Err(error) = self.couriers.prune_expired(now_s) {
            tracing::debug!(target: "mesh::ble", %error, "courier pool prune failed");
        }
        // Transfers that have every chunk but were never stored: the process ended
        // between the last chunk and the store, or the store itself failed. Every
        // byte is already on disk and nothing retransmits, so this periodic retry
        // is the only recovery. On the cleanup cadence rather than the tick, so a
        // persistent failure retries occasionally instead of twice a second.
        for id in self.files_in.completed() {
            let nickname = self
                .files_in
                .get(&id)
                .and_then(|transfer| {
                    self.state.with(|state| {
                        state
                            .peers
                            .nickname(&transfer.sender, false)
                            .map(str::to_string)
                    })
                })
                .unwrap_or_else(|| "a peer".to_string());
            self.complete_transfer(&id, &nickname);
        }
        // A sender that walked out of range mid-transfer would otherwise leave its
        // partial file in the spool forever. The window is deliberately long — the
        // point of persisting the bitmap is surviving outages that last minutes —
        // so this bounds disk, not patience.
        self.files_in.abandon_stale(now_s, TRANSFER_ABANDON_SECS);
        self.reconcile_peers();
    }

    async fn flush_directed_spool(&mut self, now_s: u64) {
        let spooled = self.state.with(|state| {
            state
                .spool
                .drain_unexpired(now_s, DIRECTED_SPOOL_WINDOW_SECS)
        });
        for packet in spooled {
            self.transmit(&packet.packet, Some(&packet.recipient), None)
                .await;
        }
    }

    /// Say how fast frames are actually reaching the air, while that is in doubt.
    ///
    /// Pacing allows one frame per [`WRITE_PACING_DELAY`] — fifty a second — but
    /// the drain runs from a `select!` arm competing with radio events and the
    /// tick, so the achieved rate is a separate question from the permitted one.
    /// A 2 MB transfer worked out at roughly 13 frames a second when derived from
    /// the receiver, and nothing in either log could say whether that was the
    /// link's ceiling or the loop's.
    ///
    /// Only while something is queued or in flight, so a quiet mesh stays quiet,
    /// and at info because the question usually comes up on a phone, where debug
    /// is not emitted.
    fn report_throughput(&mut self, now_ms: u64) {
        let queued = self.state.with(|state| state.writes.total_queued_bytes());
        let busy = queued > 0 || !self.files_out.is_empty();
        if !busy {
            // Reset the window so the first report after a quiet spell measures
            // the transfer rather than the silence before it.
            self.frames_written
                .store(0, std::sync::atomic::Ordering::Relaxed);
            self.last_throughput_report_ms = now_ms;
            return;
        }
        let elapsed = now_ms.saturating_sub(self.last_throughput_report_ms);
        if elapsed < THROUGHPUT_REPORT_INTERVAL_MS {
            return;
        }
        let frames = self
            .frames_written
            .swap(0, std::sync::atomic::Ordering::Relaxed);
        self.last_throughput_report_ms = now_ms;
        tracing::info!(
            target: "mesh::ble",
            frames,
            per_second = frames * 1_000 / elapsed.max(1),
            ceiling_per_second = 1_000 / WRITE_PACING_DELAY.as_millis().max(1) as u64,
            queued_bytes = queued,
            "frames on the air"
        );
    }

    /// Put everything queued on the air at once, ignoring the pacing interval.
    ///
    /// Tests only. Sending now queues, and the queue drains one frame per link per
    /// [`WRITE_PACING_DELAY`] — so a test that sends and then reads the mock
    /// radio's log sees nothing without this. Draining explicitly is better than
    /// sleeping through the real interval: the tests assert *what* went on the air,
    /// and the pacing has its own test for *when*.
    #[cfg(test)]
    pub(super) async fn drain_writes(&self) {
        // Bounded rather than `while !is_empty()`: a radio that refuses every
        // write would otherwise spin here forever instead of failing the test.
        for _ in 0..10_000 {
            if self.state.with(|state| state.writes.is_empty()) {
                return;
            }
            self.flush_buffered_writes().await;
        }
    }

    /// Put **one** frame per link on the air.
    ///
    /// Called every [`WRITE_PACING_DELAY`] while anything is queued, so one frame
    /// per link per interval is the transmit rate — deliberately, and this is the
    /// only thing that limits it. It used to drain each link until the radio
    /// refused, which is no limit at all where the radio never refuses: BlueZ
    /// accepted every frame and dropped what the link could not carry.
    ///
    /// One per call rather than a batch because the interval is calibrated per
    /// frame. Draining two would double the rate and put the guesswork back.
    pub(super) async fn flush_buffered_writes(&self) {
        let next = self.state.with(|state| {
            let links: Vec<LinkId> = state.writes.link_ids().cloned().collect();
            links
                .into_iter()
                .filter_map(|link| {
                    let mut writes = state.writes.take_all(&link);
                    if writes.is_empty() {
                        return None;
                    }
                    let head = writes.remove(0);
                    if !writes.is_empty() {
                        // The tail goes back in order. A reordered fragment
                        // stream is worse than a delayed one.
                        state.writes.prepend(&link, writes);
                    }
                    Some((link, head))
                })
                .collect::<Vec<_>>()
        });

        for (link, write) in next {
            match self.radio.send(&link, &write.bytes).await {
                Ok(()) => {
                    self.frames_written
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Err(error) if should_buffer(&error) => {
                    // The radio does push back on some platforms. Put it back at
                    // the head and try again on the next interval.
                    self.state
                        .with(|state| state.writes.prepend(&link, vec![write]));
                    tracing::trace!(target: "mesh::ble", %link, %error, "write deferred");
                }
                Err(error) => {
                    // The whole queue for this link goes, not just this frame.
                    //
                    // The error that actually shows up here, now that it is
                    // logged, is "link is no longer connected": the peer is gone
                    // and every remaining frame will fail the same way. Draining
                    // them one per pacing interval would spend 20 ms each to
                    // discover that — twenty seconds for a single queued chunk —
                    // while the interval that matters is the one live links are
                    // waiting for.
                    //
                    // A disconnect event already clears the queue via
                    // `discard_all`; this covers the window where frames were
                    // queued before the event arrived, which is exactly when a
                    // transfer is in flight.
                    let (moved, discarded_types) = self.state.with(|state| {
                        let result = rescue_link_writes(state, &link, Some(write.clone()));
                        if matches!(&error, chatter_mesh_link::LinkError::LinkGone(_)) {
                            let peer = state.links.link_down(&link);
                            state.max_write.remove(&link);
                            state.assembler.discard(&link);
                            state.links.clear_noise_authentication(&link);
                            if let Some(address) = state.address_of.remove(&link) {
                                state.connecting.remove(&address);
                                state.scheduler.note_disconnected(&address, now_millis());
                            }
                            if let Some(peer) = peer {
                                state.dm_handshake_at_ms.remove(&peer);
                            }
                        }
                        result
                    });
                    if matches!(&error, chatter_mesh_link::LinkError::LinkGone(_)) {
                        let _ = self.radio.disconnect(&link).await;
                    }
                    // Counted per type rather than listed: a stalled transfer
                    // discards dozens of fragments at once and the shape is the
                    // information, not the sequence.
                    let mut by_type: std::collections::BTreeMap<u8, usize> =
                        std::collections::BTreeMap::new();
                    for msg_type in &discarded_types {
                        *by_type.entry(*msg_type).or_default() += 1;
                    }
                    // The type distinguishes a loss that matters from one that does
                    // not, and without it the line cannot: an announce or a sync
                    // request is re-sent on the next interval, while a message or a
                    // file chunk is gone. Byte 1 of the frame is the packet type
                    // (`codec.rs`: version:1 type:1 ttl:1 ...); on a fragment it
                    // reads `Fragment`, which is itself the useful answer, since a
                    // fragmented frame is never periodic chatter.
                    tracing::warn!(
                        target: "mesh::ble",
                        %link,
                        %error,
                        bytes = write.bytes.len(),
                        msg_type = ?write.bytes.get(1),
                        moved_to_sibling = moved,
                        also_discarded = discarded_types.len(),
                        also_discarded_types = ?by_type,
                        "radio refused a frame; queued writes moved to a sibling or left for outbox retry"
                    );
                }
            }
        }
    }
}
