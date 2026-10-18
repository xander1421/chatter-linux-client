use super::*;

impl GossipSyncManager {
    // -- requesting --------------------------------------------------------

    /// The first request to a freshly connected peer, covering every enabled type
    /// at once so the link is not spent on five separate rounds.
    ///
    /// The runtime should delay this by [`INITIAL_SYNC_DELAY_SECS`].
    pub fn initial_sync_to(
        &mut self,
        peer: &PeerId,
        now_unix_secs: u64,
    ) -> Result<SyncSend, StoreError> {
        let types = self.initial_sync_types();
        self.build_request(Some(peer), types, None, now_unix_secs)
    }

    /// Targeted fragment recovery for reassemblies that have stalled.
    ///
    /// Asks each connected peer for exactly the named streams instead of waiting on
    /// the next periodic fragment round to happen to cover them. At most
    /// `chatter_mesh::tlv::request_sync::MAX_FRAGMENT_ID_FILTER_COUNT` streams ride
    /// in one request; the encoder truncates past that, which keeps the filter value
    /// inside the decoder's 1024-byte acceptance cap on the far side. An oversized
    /// value would be *ignored* by the peer while the rest of the request still
    /// decoded, silently degrading a targeted resync into an untargeted one.
    ///
    /// Returns nothing when no stream is named or no peer is connected — there is
    /// no broadcast fallback here, because an untargeted broadcast is precisely what
    /// this call exists to avoid.
    pub fn request_missing_fragments(
        &mut self,
        fragment_ids: &[[u8; FRAGMENT_ID_LEN]],
        connected_peers: &[PeerId],
        now_unix_secs: u64,
    ) -> Result<Vec<SyncSend>, StoreError> {
        let Some(filter) = encode_fragment_id_filter(fragment_ids) else {
            return Ok(Vec::new());
        };
        if connected_peers.is_empty() {
            return Ok(Vec::new());
        }

        let mut sends = Vec::with_capacity(connected_peers.len());
        for peer in connected_peers {
            sends.push(self.build_request(
                Some(peer),
                SyncTypeFlags::FRAGMENT,
                Some(filter.clone()),
                now_unix_secs,
            )?);
        }
        Ok(sends)
    }

    /// One maintenance tick: expire, prune, and emit the sync rounds now due.
    ///
    /// Does **not** persist the archive — [`persist_if_dirty`](Self::persist_if_dirty)
    /// is separate so a storage failure cannot swallow the packets this round wanted
    /// to send. The runtime calls both, in either order.
    pub fn maintenance(
        &mut self,
        connected_peers: &[PeerId],
        now_unix_secs: u64,
    ) -> Result<Vec<SyncSend>, StoreError> {
        self.cleanup_expired(now_unix_secs);
        self.cleanup_stale_announcements_if_needed(now_unix_secs);
        self.requests.cleanup(now_unix_secs);
        self.rate_limiter.prune(now_unix_secs);

        let has_board = self.board_packets.is_some();
        let mut due: Vec<SyncTypeFlags> = Vec::new();
        for schedule in &mut self.schedules {
            if schedule.interval_secs == 0 {
                continue;
            }
            // No board source wired up means nothing to offer or store; skip the
            // round entirely rather than asking for posts we would discard.
            if schedule.types == SyncTypeFlags::BOARD && !has_board {
                continue;
            }
            if schedule.is_due(now_unix_secs) {
                schedule.last_sent_secs = Some(now_unix_secs);
                due.push(schedule.types);
            }
        }

        let mut sends = Vec::new();
        for types in due {
            sends.extend(self.periodic_sync(types, connected_peers, now_unix_secs)?);
        }
        Ok(sends)
    }

    /// Unicast to every connected peer, or broadcast when there are none.
    ///
    /// Unicast is what makes the answers attributable: a response to a request we
    /// sent *this peer* can be checked against our own record of having sent it. The
    /// broadcast form exists only for the discovery phase, where there is no peer to
    /// address and therefore nothing to attribute.
    fn periodic_sync(
        &mut self,
        types: SyncTypeFlags,
        connected_peers: &[PeerId],
        now_unix_secs: u64,
    ) -> Result<Vec<SyncSend>, StoreError> {
        if connected_peers.is_empty() {
            return Ok(vec![self.build_request(
                None,
                types,
                None,
                now_unix_secs,
            )?]);
        }
        let mut sends = Vec::with_capacity(connected_peers.len());
        for peer in connected_peers {
            sends.push(self.build_request(Some(peer), types, None, now_unix_secs)?);
        }
        Ok(sends)
    }

    /// Builds one `REQUEST_SYNC` packet.
    ///
    /// `ttl` is 0 and nothing may change that: the request is link-local, and a
    /// non-zero TTL would ask every node the packet reached to answer.
    fn build_request(
        &mut self,
        to: Option<&PeerId>,
        types: SyncTypeFlags,
        fragment_id_filter: Option<String>,
        now_unix_secs: u64,
    ) -> Result<SyncSend, StoreError> {
        // Only a unicast request can be answered attributably, so only a unicast
        // request opens a response window.
        if let Some(peer) = to {
            self.requests.register_request(peer, now_unix_secs);
        }

        let payload = self
            .build_gcs_payload(types, fragment_id_filter, now_unix_secs)
            .encode()?;

        let packet = BitchatPacket::new(
            MessageType::RequestSync,
            self.my_sender_id,
            to.and_then(PeerId::to_routing_bytes),
            now_unix_secs.saturating_mul(1000),
            payload,
            0,
        );
        Ok(SyncSend::request(to, packet))
    }

    /// Builds the filter describing what we already hold, so the peer can send only
    /// the difference.
    fn build_gcs_payload(
        &self,
        types: SyncTypeFlags,
        fragment_id_filter: Option<String>,
        now_unix_secs: u64,
    ) -> RequestSyncPacket {
        let p = gcs::derive_p(self.config.gcs_target_fpr);
        // `derive_p` clamps to 1..=32, so the fallback is unreachable; it exists
        // because a panic on radio-adjacent code is never the right answer.
        let p_byte = u8::try_from(p).unwrap_or(MAX_P);

        let empty_filter =
            |packet_types: SyncTypeFlags, filter: Option<String>| RequestSyncPacket {
                p: p_byte,
                // `m = 1` decodes to the empty set on the far side, i.e. "I hold
                // nothing, send me everything".
                m: 1,
                data: Vec::new(),
                types: Some(packet_types.raw()),
                since_timestamp_ms: None,
                fragment_id_filter: filter,
            };

        let mut candidates = self.collect_candidates(types, now_unix_secs);
        if candidates.is_empty() {
            return empty_filter(types, fragment_id_filter);
        }

        // Newest first. Everything downstream depends on this order: the byte-budget
        // trim drops from the tail, so the covered set is always a contiguous
        // newest-prefix and the since-cursor is exact.
        candidates.sort_by_key(|c| core::cmp::Reverse(c.timestamp));

        let n_max = gcs::estimate_max_elements(self.config.gcs_max_bytes, p);
        let cap = self.capacity_for(types);
        let take = candidates.len().min(n_max.min(cap));
        if take == 0 {
            return empty_filter(types, fragment_id_filter);
        }

        let ids: Vec<[u8; 16]> = candidates
            .iter()
            .take(take)
            .map(|packet| packet.packet_id())
            .collect();
        let params = gcs::build_filter(&ids, self.config.gcs_max_bytes, self.config.gcs_target_fpr);

        // When the filter cannot cover every candidate — the store exceeds `take`,
        // or the encoder trimmed the tail to fit the byte budget — tell the
        // responder how far back the filter actually reaches. Packets older than
        // the cursor are outside the filter but not missing; without it the
        // responder would re-send that entire tail every round.
        let covered = params.included;
        let since_timestamp_ms = if covered > 0 && covered < candidates.len() {
            candidates.get(covered - 1).map(|packet| packet.timestamp)
        } else {
            None
        };

        RequestSyncPacket {
            p: u8::try_from(params.p).unwrap_or(MAX_P),
            m: params.m,
            data: params.data,
            types: Some(types.raw()),
            since_timestamp_ms,
            fragment_id_filter,
        }
    }
}
