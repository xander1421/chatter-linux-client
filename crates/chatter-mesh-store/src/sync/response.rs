use super::*;

impl GossipSyncManager {
    // -- responding --------------------------------------------------------

    /// Answers a peer's `REQUEST_SYNC` with the packets its filter says it lacks.
    ///
    /// Every returned packet is marked RSR with `ttl = 0`: solicited, link-local,
    /// and exempt from the freshness check on the requester's side — but only
    /// because *they* asked, which their own [`RequestSyncTracker`] is what
    /// verifies.
    ///
    /// Returns nothing when the peer is over its response budget. That is the only
    /// input-dependent reason to stay silent; a request whose filter covers
    /// everything we hold simply produces an empty diff.
    pub fn handle_request_sync(
        &mut self,
        from: &PeerId,
        request: &RequestSyncPacket,
        now_unix_secs: u64,
    ) -> Vec<SyncSend> {
        // A response can replay the whole store, so bound how often one peer can
        // trigger a diff pass regardless of how fast it asks.
        if !self.rate_limiter.should_respond(from, now_unix_secs) {
            return Vec::new();
        }

        // An older client that never sent the field means the default round.
        let requested = match request.types {
            Some(raw) => SyncTypeFlags::from_raw(raw),
            None => SyncTypeFlags::PUBLIC_MESSAGES,
        };

        // The requester's filter only covers packets at or after this cursor. Older
        // packets are outside the filter but not *missing*, and without the cursor
        // we would re-send that entire tail every single round.
        let since = request.since_timestamp_ms;
        let modulus = request.m;
        let sorted = gcs::decode_to_sorted(u32::from(request.p), modulus, &request.data);
        let might_contain = |packet: &BitchatPacket| -> bool {
            gcs::contains(&sorted, gcs::bucket(&packet.packet_id(), modulus))
        };
        let newer_than_cursor = |packet: &BitchatPacket| -> bool {
            match since {
                Some(cursor) => packet.timestamp >= cursor,
                None => true,
            }
        };

        let config = &self.config;
        let fresh = |packet: &BitchatPacket| packet_is_fresh(config, packet, now_unix_secs);
        let mut sends: Vec<SyncSend> = Vec::new();

        // Announces are exempt from the since-cursor: they carry the signing keys
        // needed to verify everything else, and there is at most one per peer, so
        // the resend cost is negligible next to being unable to verify anything.
        if requested.contains(MessageType::Announce) {
            for packet in self.latest_announce_by_peer.values() {
                if fresh(packet) && !might_contain(packet) {
                    sends.push(SyncSend::response(from, packet));
                }
            }
        }

        if requested.contains(MessageType::Message) {
            for packet in self.messages.all_packets(fresh) {
                if newer_than_cursor(packet) && !might_contain(packet) {
                    sends.push(SyncSend::response(from, packet));
                }
            }
        }

        if requested.contains(MessageType::Fragment) {
            // A fragment-ID filter narrows the diff to exactly the named streams —
            // targeted resync for a stalled reassembly — and bypasses the
            // since-cursor for them, since the whole reason the reassembly stalled
            // is that the missing pieces are older than what the cursor covers. The
            // GCS filter still excludes the pieces the requester already holds.
            let wanted_streams = request.fragment_ids();
            for packet in self.fragments.all_packets(fresh) {
                let included = match &wanted_streams {
                    Some(streams) => {
                        fragment_stream_id(packet).is_some_and(|stream| streams.contains(&stream))
                    }
                    None => newer_than_cursor(packet),
                };
                if included && !might_contain(packet) {
                    sends.push(SyncSend::response(from, packet));
                }
            }
        }

        if requested.contains(MessageType::FileTransfer) {
            for packet in self.file_transfers.all_packets(fresh) {
                if newer_than_cursor(packet) && !might_contain(packet) {
                    sends.push(SyncSend::response(from, packet));
                }
            }
        }

        if requested.contains(MessageType::GroupMessage) {
            for packet in self.group_messages.all_packets(fresh) {
                if newer_than_cursor(packet) && !might_contain(packet) {
                    sends.push(SyncSend::response(from, packet));
                }
            }
        }

        // Like announces, bundles are exempt from the since-cursor: there is at
        // most one per owner (newer replaces older), so the resend cost is bounded,
        // and a joining peer must be able to learn bundles generated long before it
        // arrived.
        if requested.contains(MessageType::PrekeyBundle) {
            for packet in self.latest_prekey_bundle_by_owner.values() {
                if fresh(packet) && !might_contain(packet) {
                    sends.push(SyncSend::response(from, packet));
                }
            }
        }

        if requested.contains(MessageType::BoardPost) {
            // No freshness window applies: the board store already filtered to live
            // posts and tombstones, and posts sync until their own expiry.
            for packet in self.board_packets.iter().flatten() {
                if newer_than_cursor(packet) && !might_contain(packet) {
                    sends.push(SyncSend::response(from, packet));
                }
            }
        }

        sends
    }
}
