use super::*;

#[test]
fn holds_matches_a_stored_message_by_id_whatever_its_ttl_or_rsr() {
    let mut sync = manager();
    let original = message(b"hello", NOW_MS);
    assert!(!sync.holds(&original));
    sync.on_public_packet_seen(&original, NOW);

    let mut response = original.clone();
    response.ttl = 0;
    response.is_rsr = true;
    assert!(sync.holds(&response), "a sync copy is the same packet");
    assert!(!sync.holds(&message(b"other", NOW_MS)));
}

// -- solicitation ------------------------------------------------------

#[test]
fn a_solicited_rsr_is_accepted_and_an_unsolicited_one_is_not() {
    let mut tracker = RequestSyncTracker::default();
    // Nothing was asked of this peer: the flag alone buys nothing, which is the
    // whole point — it is attacker-controlled.
    assert!(!tracker.is_valid_response(&them(), true, NOW));

    tracker.register_request(&them(), NOW);
    assert!(tracker.is_valid_response(&them(), true, NOW));

    // Asking one peer does not open a window for another.
    assert!(!tracker.is_valid_response(&peer("1122334455667788"), true, NOW));
}

#[test]
fn a_packet_without_the_rsr_flag_is_never_a_response() {
    let mut tracker = RequestSyncTracker::default();
    tracker.register_request(&them(), NOW);
    // Not a rejection: it simply takes the ordinary freshness path.
    assert!(!tracker.is_valid_response(&them(), false, NOW));
}

#[test]
fn the_response_window_closes_exactly_at_thirty_seconds() {
    let mut tracker = RequestSyncTracker::default();
    tracker.register_request(&them(), NOW);

    assert!(tracker.is_valid_response(&them(), true, NOW + RESPONSE_WINDOW_SECS));
    assert!(!tracker.is_valid_response(&them(), true, NOW + RESPONSE_WINDOW_SECS + 1));
}

#[test]
fn a_re_request_slides_the_window_and_cleanup_drops_closed_ones() {
    let mut tracker = RequestSyncTracker::default();
    tracker.register_request(&them(), NOW);
    tracker.register_request(&them(), NOW + 20);
    // The second request refreshes the timestamp rather than stacking.
    assert_eq!(tracker.pending_count(), 1);
    assert!(tracker.is_valid_response(&them(), true, NOW + 50));

    tracker.cleanup(NOW + 50);
    assert_eq!(tracker.pending_count(), 1);
    tracker.cleanup(NOW + 51);
    assert_eq!(tracker.pending_count(), 0);
    assert!(!tracker.is_valid_response(&them(), true, NOW + 51));
}

#[test]
fn a_backwards_clock_does_not_invalidate_an_open_request() {
    let mut tracker = RequestSyncTracker::default();
    tracker.register_request(&them(), NOW);
    // Saturating: the age is zero, not a wrapped eternity.
    assert!(tracker.is_valid_response(&them(), true, NOW - 5));
}

#[test]
fn the_manager_only_opens_a_window_for_unicast_requests() {
    let mut manager = manager();

    // No connected peers: the discovery fallback broadcasts, and a broadcast
    // cannot attribute its answers, so no window opens.
    let sends = manager.maintenance(&[], NOW).expect("maintenance");
    assert!(!sends.is_empty());
    assert!(sends.iter().all(|send| send.to.is_none()));
    assert_eq!(manager.pending_request_count(), 0);
    assert!(!manager.is_valid_response(&them(), true, NOW));

    // Unicast to a connected peer does.
    let sends = manager
        .maintenance(&[them()], NOW + 60)
        .expect("maintenance");
    assert!(sends.iter().all(|send| send.to == Some(them())));
    assert_eq!(manager.pending_request_count(), 1);
    assert!(manager.is_valid_response(&them(), true, NOW + 60));
    assert!(!manager.is_valid_response(&them(), true, NOW + 60 + RESPONSE_WINDOW_SECS + 1));
}

#[test]
fn an_answer_without_rsr_is_awaited_only_from_the_peer_we_asked_in_the_window() {
    let mut manager = manager();
    assert!(!manager.awaits_answer_from(&them(), NOW), "never asked");

    manager.initial_sync_to(&them(), NOW).expect("request");
    assert!(manager.awaits_answer_from(&them(), NOW));
    assert!(
        !manager.is_valid_response(&them(), false, NOW),
        "RSR path unchanged"
    );
    let stranger = PeerId::from_bytes(&[0x42; 8]);
    assert!(!manager.awaits_answer_from(&stranger, NOW));
    assert!(!manager.awaits_answer_from(&them(), NOW + RESPONSE_WINDOW_SECS + 1));
}

#[test]
fn retention_is_the_stores_own_window_for_the_type() {
    let manager = manager();
    let window_ms = manager.config().public_message_max_age_secs * 1000;
    assert!(manager.within_retention(&message(b"edge", NOW_MS - window_ms), NOW));
    assert!(!manager.within_retention(&message(b"old", NOW_MS - window_ms - 1), NOW));
    // Announces keep the short window, not the public-message one.
    let announce = packet_of(MessageType::Announce, b"", NOW_MS - 16 * 60 * 1000);
    assert!(!manager.within_retention(&announce, NOW));
}

// -- rate limiting -----------------------------------------------------

#[test]
fn the_rate_limiter_allows_exactly_eight_per_window() {
    let mut limiter = SyncResponseRateLimiter::default();
    for attempt in 1..=MAX_RESPONSES_PER_WINDOW {
        assert!(
            limiter.should_respond(&them(), NOW),
            "response {attempt} of {MAX_RESPONSES_PER_WINDOW} must be allowed"
        );
    }
    // The ninth inside the window is refused.
    assert!(!limiter.should_respond(&them(), NOW));
    assert_eq!(limiter.recorded(&them()), MAX_RESPONSES_PER_WINDOW);
}

#[test]
fn the_rate_limit_window_is_inclusive_and_slides() {
    let mut limiter = SyncResponseRateLimiter::default();
    for _ in 0..MAX_RESPONSES_PER_WINDOW {
        assert!(limiter.should_respond(&them(), NOW));
    }
    // Exactly one window later the first entries are still inside it.
    assert!(!limiter.should_respond(&them(), NOW + RESPONSE_RATE_LIMIT_WINDOW_SECS));
    // One second past, they fall out and the budget is free again.
    assert!(limiter.should_respond(&them(), NOW + RESPONSE_RATE_LIMIT_WINDOW_SECS + 1));
}

#[test]
fn the_rate_limit_is_per_peer() {
    let mut limiter = SyncResponseRateLimiter::default();
    for _ in 0..MAX_RESPONSES_PER_WINDOW {
        assert!(limiter.should_respond(&them(), NOW));
    }
    assert!(!limiter.should_respond(&them(), NOW));
    // A flooding peer must not be able to mute everyone else.
    assert!(limiter.should_respond(&peer("1122334455667788"), NOW));
}

#[test]
fn pruning_forgets_departed_peers() {
    let mut limiter = SyncResponseRateLimiter::default();
    assert!(limiter.should_respond(&them(), NOW));
    limiter.prune(NOW + RESPONSE_RATE_LIMIT_WINDOW_SECS);
    assert_eq!(limiter.recorded(&them()), 1);
    limiter.prune(NOW + RESPONSE_RATE_LIMIT_WINDOW_SECS + 1);
    assert_eq!(limiter.recorded(&them()), 0);
}

#[test]
fn a_flooding_peer_stops_getting_answers_after_eight_rounds() {
    let mut manager = manager();
    manager.on_public_packet_seen(&message(b"hello", NOW_MS), NOW);

    for attempt in 1..=MAX_RESPONSES_PER_WINDOW {
        let sends =
            manager.handle_request_sync(&them(), &empty_request(SyncTypeFlags::MESSAGE), NOW);
        assert_eq!(sends.len(), 1, "round {attempt} must be answered");
    }
    assert!(
        manager
            .handle_request_sync(&them(), &empty_request(SyncTypeFlags::MESSAGE), NOW)
            .is_empty(),
        "the ninth request inside the window must be dropped"
    );
}

// -- link-local requests, RSR responses --------------------------------

#[test]
fn every_request_we_originate_is_link_local() {
    let mut manager = manager();
    manager.set_board_packets(Vec::new());
    let mut sends = manager.maintenance(&[them()], NOW).expect("maintenance");
    sends.push(manager.initial_sync_to(&them(), NOW).expect("initial"));
    sends.extend(
        manager
            .request_missing_fragments(&[[0x01; 8]], &[them()], NOW)
            .expect("fragments"),
    );

    assert!(!sends.is_empty());
    for send in &sends {
        assert_eq!(send.packet.msg_type, MessageType::RequestSync.as_u8());
        assert_eq!(send.packet.ttl, 0, "REQUEST_SYNC must never be relayable");
        assert!(!send.packet.is_rsr, "a request is not a response");
        assert!(send.sign_before_send, "our own packets go out signed");
    }
}

#[test]
fn responses_are_rsr_marked_ttl_zero_and_never_re_signed() {
    let mut manager = manager();
    manager.on_public_packet_seen(&message(b"carried", NOW_MS), NOW);

    let sends = manager.handle_request_sync(&them(), &empty_request(SyncTypeFlags::MESSAGE), NOW);
    assert_eq!(sends.len(), 1);
    let send = &sends[0];
    assert_eq!(send.to, Some(them()));
    assert_eq!(send.packet.ttl, 0);
    assert!(send.packet.is_rsr);
    assert!(
        !send.sign_before_send,
        "re-signing would replace the author's signature with ours"
    );
    assert_eq!(send.packet.payload, b"carried".to_vec());
}

#[test]
fn a_response_keeps_the_original_signature_valid() {
    // ttl and is_rsr are both excluded from the signing preimage, which is what
    // makes it safe to mutate them on a stored packet.
    let mut manager = manager();
    let mut original = message(b"signed", NOW_MS);
    original.signature = Some([0x5A; 64]);
    manager.on_public_packet_seen(&original, NOW);

    let sends = manager.handle_request_sync(&them(), &empty_request(SyncTypeFlags::MESSAGE), NOW);
    let sent = &sends[0].packet;
    assert_eq!(sent.signature, original.signature);
    assert_eq!(
        sent.signing_preimage().expect("preimage"),
        original.signing_preimage().expect("preimage")
    );
}

// -- the diff ----------------------------------------------------------

#[test]
fn a_peer_holding_a_subset_gets_exactly_the_difference() {
    let held: Vec<BitchatPacket> = (0..5u8)
        .map(|i| message(&[0x10 + i], NOW_MS + u64::from(i)))
        .collect();

    // The responder carries all five.
    let mut responder = manager();
    for packet in &held {
        responder.on_public_packet_seen(packet, NOW);
    }

    // The requester carries the first three, and builds a real filter over them.
    let mut requester = manager();
    for packet in held.iter().take(3) {
        requester.on_public_packet_seen(packet, NOW);
    }
    let request_send = requester.initial_sync_to(&them(), NOW).expect("request");
    let request = decode_request(&request_send);

    let sends = responder.handle_request_sync(&them(), &request, NOW);
    let payloads: Vec<Vec<u8>> = sends
        .iter()
        .map(|send| send.packet.payload.clone())
        .collect();
    assert_eq!(
        payloads,
        vec![vec![0x13], vec![0x14]],
        "only the two the requester lacks, in store order"
    );
}

#[test]
fn a_peer_holding_everything_gets_nothing_back() {
    let held: Vec<BitchatPacket> = (0..5u8)
        .map(|i| message(&[0x20 + i], NOW_MS + u64::from(i)))
        .collect();

    let mut responder = manager();
    let mut requester = manager();
    for packet in &held {
        responder.on_public_packet_seen(packet, NOW);
        requester.on_public_packet_seen(packet, NOW);
    }

    let request = decode_request(&requester.initial_sync_to(&them(), NOW).expect("request"));
    assert!(
        responder
            .handle_request_sync(&them(), &request, NOW)
            .is_empty(),
        "a filter covering the whole store leaves nothing to send"
    );
}

#[test]
fn the_diff_is_computed_over_recomputed_packet_ids() {
    // The filter is built from `packet_id()`, and the responder must recompute
    // it the same way — diffing anything else (a stored hex string, the raw
    // bytes) silently stops the filter from ever matching.
    let mut responder = manager();
    let carried = message(b"already-held", NOW_MS);
    responder.on_public_packet_seen(&carried, NOW);

    let params = gcs::build_filter(&[carried.packet_id()], 256, 0.01);
    let request = RequestSyncPacket {
        p: u8::try_from(params.p).expect("p fits"),
        m: params.m,
        data: params.data,
        types: Some(SyncTypeFlags::MESSAGE.raw()),
        since_timestamp_ms: None,
        fragment_id_filter: None,
    };
    assert!(
        responder
            .handle_request_sync(&them(), &request, NOW)
            .is_empty()
    );

    // A relayed copy of the same packet has a different ttl and still matches:
    // packet IDs deliberately exclude ttl.
    let relayed = BitchatPacket {
        ttl: 3,
        ..carried.clone()
    };
    assert_eq!(relayed.packet_id(), carried.packet_id());
}

// -- the since cursor --------------------------------------------------

#[test]
fn the_since_cursor_excludes_older_packets_but_never_announces() {
    let mut manager = manager();
    let announce = packet_of(MessageType::Announce, b"", NOW_MS - 50_000);
    let old = message(b"old", NOW_MS - 60_000);
    let new = message(b"new", NOW_MS);
    manager.on_public_packet_seen(&announce, NOW);
    manager.on_public_packet_seen(&old, NOW);
    manager.on_public_packet_seen(&new, NOW);

    let mut request = empty_request(SyncTypeFlags::PUBLIC_MESSAGES);
    request.since_timestamp_ms = Some(NOW_MS - 30_000);

    let sends = manager.handle_request_sync(&them(), &request, NOW);
    assert_eq!(sends.len(), 2);
    assert!(
        sends
            .iter()
            .any(|send| send.packet.msg_type == MessageType::Announce.as_u8()),
        "announces carry the keys needed to verify everything else"
    );
    let messages: Vec<&SyncSend> = sends
        .iter()
        .filter(|send| send.packet.msg_type == MessageType::Message.as_u8())
        .collect();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].packet.payload, b"new".to_vec());
}

#[test]
fn the_cursor_boundary_is_inclusive() {
    let mut manager = manager();
    let exactly_at = message(b"at", NOW_MS - 10_000);
    manager.on_public_packet_seen(&exactly_at, NOW);

    let mut request = empty_request(SyncTypeFlags::MESSAGE);
    request.since_timestamp_ms = Some(NOW_MS - 10_000);
    assert_eq!(manager.handle_request_sync(&them(), &request, NOW).len(), 1);

    let mut request = empty_request(SyncTypeFlags::MESSAGE);
    request.since_timestamp_ms = Some(NOW_MS - 9_999);
    assert!(
        manager
            .handle_request_sync(&them(), &request, NOW)
            .is_empty()
    );
}

#[test]
fn a_truncated_filter_carries_a_cursor_and_a_complete_one_does_not() {
    // 32 bytes of filter covers roughly 28 IDs at P=7, well under the 40 held.
    let mut manager = manager_with(SyncConfig {
        gcs_max_bytes: 32,
        ..SyncConfig::default()
    });
    for i in 0..40u64 {
        manager.on_public_packet_seen(&message(&[u8::try_from(i).unwrap_or(0)], NOW_MS + i), NOW);
    }

    let request = decode_request(&manager.initial_sync_to(&them(), NOW).expect("request"));
    let since = request
        .since_timestamp_ms
        .expect("a truncated filter needs a cursor");
    // The filter covers a newest-first prefix, so the cursor sits inside the
    // store's range and below its newest entry.
    assert!(since >= NOW_MS);
    assert!(since < NOW_MS + 40);

    // One message fits any budget, so no cursor is needed. Built through
    // `manager_with` because the local binding above shadows the `manager()`
    // helper for the rest of this function.
    let mut manager = manager_with(SyncConfig::default());
    manager.on_public_packet_seen(&message(b"only", NOW_MS), NOW);
    let request = decode_request(&manager.initial_sync_to(&them(), NOW).expect("request"));
    assert_eq!(request.since_timestamp_ms, None);
}

// -- expiry ------------------------------------------------------------

#[test]
fn public_messages_expire_at_exactly_six_hours() {
    let config = SyncConfig::default();
    assert_eq!(config.public_message_max_age_secs, 6 * 60 * 60);
    let window_ms = config.public_message_max_age_secs * 1000;

    let mut manager = manager();
    let at_boundary = message(b"boundary", NOW_MS - window_ms);
    let past_boundary = message(b"past", NOW_MS - window_ms - 1);

    manager.on_public_packet_seen(&at_boundary, NOW);
    assert_eq!(manager.message_count(), 1, "exactly at the window is fresh");

    manager.on_public_packet_seen(&past_boundary, NOW);
    assert_eq!(manager.message_count(), 1, "one millisecond past is stale");

    // And the boundary packet drops out a millisecond later.
    manager.maintenance(&[], NOW).expect("maintenance");
    assert_eq!(manager.message_count(), 1);
    manager.maintenance(&[], NOW + 1).expect("maintenance");
    assert_eq!(manager.message_count(), 0);
}

#[test]
fn fragments_and_file_transfers_keep_the_short_window() {
    let config = SyncConfig::default();
    assert_eq!(config.max_message_age_secs, 15 * 60);
    let window_ms = config.max_message_age_secs * 1000;

    let mut manager = manager();
    manager.on_public_packet_seen(&fragment([0x01; 8], 0, NOW_MS - window_ms), NOW);
    assert_eq!(manager.fragment_count(), 1);
    manager.on_public_packet_seen(&fragment([0x02; 8], 0, NOW_MS - window_ms - 1), NOW);
    assert_eq!(manager.fragment_count(), 1, "past the window is refused");

    // File transfers share that short window.
    manager.on_public_packet_seen(
        &packet_of(MessageType::FileTransfer, b"at", NOW_MS - window_ms),
        NOW,
    );
    manager.on_public_packet_seen(
        &packet_of(MessageType::FileTransfer, b"past", NOW_MS - window_ms - 1),
        NOW,
    );
    assert_eq!(
        manager
            .handle_request_sync(&them(), &empty_request(SyncTypeFlags::FILE_TRANSFER), NOW)
            .len(),
        1
    );

    // A message of the same age is still carried: the windows differ per type,
    // and that difference is the whole town-crier property.
    manager.on_public_packet_seen(&message(b"old but public", NOW_MS - window_ms - 1), NOW);
    assert_eq!(manager.message_count(), 1);
}

#[test]
fn prekey_bundles_keep_a_day_and_the_boundary_is_exact() {
    // The longest window of the three: a bundle's whole job is being reachable
    // while its owner is off the mesh.
    let window_ms = SyncConfig::default().prekey_bundle_max_age_secs * 1000;
    assert_eq!(window_ms, 24 * 60 * 60 * 1000);

    let mut manager = manager();
    manager.on_public_packet_seen(&bundle_packet([0xE1; 32], NOW_MS - window_ms), NOW);
    manager.on_public_packet_seen(&bundle_packet([0xE2; 32], NOW_MS - window_ms - 1), NOW);

    assert!(manager.has_prekey_bundle(&PeerId::from_noise_key(&[0xE1; 32])));
    assert!(
        !manager.has_prekey_bundle(&PeerId::from_noise_key(&[0xE2; 32])),
        "one millisecond past the window is stale"
    );
}

#[test]
fn an_expired_packet_is_never_served() {
    let mut manager = manager();
    let window_ms = manager.config().public_message_max_age_secs * 1000;
    manager.on_public_packet_seen(&message(b"fresh now", NOW_MS - window_ms), NOW);
    assert_eq!(manager.message_count(), 1);

    // A second later it is outside the window; the store still holds it until
    // maintenance runs, but it must not be handed out.
    let sends =
        manager.handle_request_sync(&them(), &empty_request(SyncTypeFlags::MESSAGE), NOW + 1);
    assert!(sends.is_empty(), "a stale packet is not a sync candidate");
}

// -- fragment-ID filter ------------------------------------------------

#[test]
fn a_fragment_filter_restricts_the_diff_and_bypasses_the_cursor() {
    let mut manager = manager_with(SyncConfig {
        fragment_capacity: 10,
        ..SyncConfig::default()
    });

    let wanted = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
    let other = [0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18];
    manager.on_public_packet_seen(&fragment(wanted, 1, NOW_MS - 60_000), NOW);
    manager.on_public_packet_seen(&fragment(other, 2, NOW_MS), NOW);

    let mut request = empty_request(SyncTypeFlags::FRAGMENT);
    // A cursor past everything: without the filter this yields nothing.
    request.since_timestamp_ms = Some(NOW_MS + 1);
    request.fragment_id_filter = encode_fragment_id_filter(&[wanted]);

    let sends = manager.handle_request_sync(&them(), &request, NOW);
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].packet.msg_type, MessageType::Fragment.as_u8());
    assert_eq!(sends[0].packet.payload.get(..8), Some(&wanted[..]));
    assert_eq!(sends[0].packet.ttl, 0);
    assert!(sends[0].packet.is_rsr);
}

#[test]
fn a_request_naming_no_stream_we_hold_gets_nothing() {
    let mut manager = manager();
    manager.on_public_packet_seen(&fragment([0x01; 8], 0, NOW_MS), NOW);

    let mut request = empty_request(SyncTypeFlags::FRAGMENT);
    request.fragment_id_filter = encode_fragment_id_filter(&[[0x99; 8]]);
    assert!(
        manager
            .handle_request_sync(&them(), &request, NOW)
            .is_empty()
    );
}

#[test]
fn a_fragment_shorter_than_a_stream_id_belongs_to_no_stream() {
    let mut manager = manager();
    // Malformed, but it arrived over the radio and must not panic anything.
    manager.on_public_packet_seen(
        &packet_of(MessageType::Fragment, &[0x01, 0x02], NOW_MS),
        NOW,
    );
    assert_eq!(manager.fragment_count(), 1);

    let mut request = empty_request(SyncTypeFlags::FRAGMENT);
    request.fragment_id_filter = encode_fragment_id_filter(&[[0x01; 8]]);
    assert!(
        manager
            .handle_request_sync(&them(), &request, NOW)
            .is_empty()
    );

    // Untargeted, it is served like any other fragment.
    assert_eq!(
        manager
            .handle_request_sync(&them(), &empty_request(SyncTypeFlags::FRAGMENT), NOW)
            .len(),
        1
    );
}

#[test]
fn a_targeted_request_names_at_most_sixty_streams_and_fits_the_cap() {
    let mut manager = manager();
    let ids: Vec<[u8; 8]> = (0..100u8).map(|i| [i; 8]).collect();
    let sends = manager
        .request_missing_fragments(&ids, &[them()], NOW)
        .expect("request");
    assert_eq!(sends.len(), 1);

    let request = decode_request(&sends[0]);
    let filter = request
        .fragment_id_filter
        .as_deref()
        .expect("filter present");
    assert_eq!(filter.split(',').count(), MAX_FRAGMENT_ID_FILTER_COUNT);
    // 60 * 17 - 1 = 1019, inside the decoder's 1024-byte acceptance cap. One
    // more ID and the peer would ignore the value entirely.
    assert_eq!(filter.len(), 1019);
    assert!(filter.len() <= 1024);
    assert_eq!(
        request.fragment_ids().map(|ids| ids.len()),
        Some(MAX_FRAGMENT_ID_FILTER_COUNT)
    );
    assert_eq!(
        request.types.map(SyncTypeFlags::from_raw),
        Some(SyncTypeFlags::FRAGMENT)
    );
}

#[test]
fn an_ignored_oversized_filter_still_leaves_a_working_sync() {
    // The decoder drops an oversized 0x06 value and keeps the rest of the
    // request. That must degrade to an untargeted fragment round, not to
    // silence — which is exactly what a `None` filter does here.
    let mut manager = manager();
    manager.on_public_packet_seen(&fragment([0x07; 8], 0, NOW_MS), NOW);

    let mut request = empty_request(SyncTypeFlags::FRAGMENT);
    request.fragment_id_filter = Some("z".repeat(2000));
    let encoded = request.encode().expect("encode");
    let decoded = RequestSyncPacket::decode(&encoded).expect("decode");
    assert_eq!(decoded.fragment_id_filter, None);

    assert_eq!(manager.handle_request_sync(&them(), &decoded, NOW).len(), 1);
}

#[test]
fn targeted_requests_need_both_a_stream_and_a_peer() {
    let mut manager = manager();
    assert!(
        manager
            .request_missing_fragments(&[], &[them()], NOW)
            .expect("no streams")
            .is_empty()
    );
    assert!(
        manager
            .request_missing_fragments(&[[0x01; 8]], &[], NOW)
            .expect("no peers")
            .is_empty(),
        "targeted resync has no broadcast fallback by design"
    );
}

// -- intake rules ------------------------------------------------------

#[test]
fn directed_traffic_is_never_gossiped() {
    let mut manager = manager();
    let mut directed = message(b"private-ish", NOW_MS);
    directed.recipient_id = Some([0x22; SENDER_ID_LEN]);
    manager.on_public_packet_seen(&directed, NOW);
    assert_eq!(manager.message_count(), 0);

    // An explicit broadcast recipient, and an absent one, are both public.
    let mut broadcast = message(b"everyone", NOW_MS);
    broadcast.recipient_id = Some(BROADCAST_RECIPIENT);
    manager.on_public_packet_seen(&broadcast, NOW);
    manager.on_public_packet_seen(&message(b"no recipient", NOW_MS), NOW);
    assert_eq!(manager.message_count(), 2);
}

#[test]
fn unknown_and_non_syncable_types_are_ignored() {
    let mut manager = manager();
    let mut unknown = message(b"future", NOW_MS);
    unknown.msg_type = 0x7F;
    manager.on_public_packet_seen(&unknown, NOW);
    manager.on_public_packet_seen(&packet_of(MessageType::VoiceFrame, b"audio", NOW_MS), NOW);
    manager.on_public_packet_seen(
        &packet_of(MessageType::CourierEnvelope, b"sealed", NOW_MS),
        NOW,
    );
    assert_eq!(manager.message_count(), 0);
    assert!(
        manager
            .handle_request_sync(&them(), &empty_request(SyncTypeFlags::PUBLIC_MESSAGES), NOW)
            .is_empty()
    );
}
