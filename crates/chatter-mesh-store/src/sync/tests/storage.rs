use super::*;

#[test]
fn the_store_is_bounded_and_evicts_oldest_first() {
    let mut manager = manager_with(SyncConfig {
        seen_capacity: 3,
        ..SyncConfig::default()
    });
    for i in 0..5u8 {
        manager.on_public_packet_seen(&message(&[i], NOW_MS + u64::from(i)), NOW);
    }
    assert_eq!(manager.message_count(), 3);

    let payloads: Vec<Vec<u8>> = manager
        .public_message_packets(NOW)
        .into_iter()
        .map(|packet| packet.payload)
        .collect();
    assert_eq!(payloads, vec![vec![2], vec![3], vec![4]]);
}

#[test]
fn re_seeing_a_packet_does_not_move_it_up_the_queue() {
    let mut manager = manager_with(SyncConfig {
        seen_capacity: 2,
        ..SyncConfig::default()
    });
    let first = message(b"first", NOW_MS);
    manager.on_public_packet_seen(&first, NOW);
    manager.on_public_packet_seen(&message(b"second", NOW_MS + 1), NOW);
    // A relayed copy of the first, with a decremented ttl.
    manager.on_public_packet_seen(
        &BitchatPacket {
            ttl: 3,
            ..first.clone()
        },
        NOW,
    );
    manager.on_public_packet_seen(&message(b"third", NOW_MS + 2), NOW);

    let payloads: Vec<Vec<u8>> = manager
        .public_message_packets(NOW)
        .into_iter()
        .map(|packet| packet.payload)
        .collect();
    assert_eq!(
        payloads,
        vec![b"second".to_vec(), b"third".to_vec()],
        "an endlessly relayed packet must not pin itself in the store"
    );
}

// -- announces and peer removal ---------------------------------------

#[test]
fn a_stale_announce_evicts_the_peer_rather_than_being_stored() {
    let mut manager = manager_with(SyncConfig {
        stale_peer_timeout_secs: 60,
        ..SyncConfig::default()
    });

    manager.on_public_packet_seen(&message(b"theirs", NOW_MS), NOW);
    manager.on_public_packet_seen(&packet_of(MessageType::Announce, b"", NOW_MS), NOW);
    assert!(manager.has_announcement(&PeerId::from_bytes(&sender())));

    // An announce older than the stale timeout says the peer left.
    manager.on_public_packet_seen(&packet_of(MessageType::Announce, b"", NOW_MS - 61_000), NOW);
    assert!(!manager.has_announcement(&PeerId::from_bytes(&sender())));
    assert_eq!(manager.message_count(), 0, "their carried traffic goes too");
}

#[test]
fn stale_cleanup_runs_on_its_own_interval() {
    let mut manager = manager_with(SyncConfig {
        stale_peer_cleanup_interval_secs: 60,
        stale_peer_timeout_secs: 30,
        ..SyncConfig::default()
    });

    manager.on_public_packet_seen(&packet_of(MessageType::Announce, b"", NOW_MS), NOW);
    // First tick records the cleanup time.
    manager.maintenance(&[], NOW).expect("maintenance");
    assert!(manager.has_announcement(&PeerId::from_bytes(&sender())));

    // 40 s later the announce is stale, but the cleanup interval has not
    // elapsed, so it survives.
    manager.maintenance(&[], NOW + 40).expect("maintenance");
    assert!(manager.has_announcement(&PeerId::from_bytes(&sender())));

    manager.maintenance(&[], NOW + 60).expect("maintenance");
    assert!(!manager.has_announcement(&PeerId::from_bytes(&sender())));
}

#[test]
fn a_present_android_peer_keeps_its_messages_between_announces() {
    // bitchat-android announces every 60 s in the background and every 120 s
    // on low battery. With Swift's 60 s timeout the sweep purged such a peer's
    // messages between two of its announces.
    let mut manager = manager();
    let peer = PeerId::from_bytes(&sender());
    manager.on_public_packet_seen(&packet_of(MessageType::Announce, b"", NOW_MS), NOW);
    manager.on_public_packet_seen(&message(b"theirs", NOW_MS), NOW);
    manager.maintenance(&[], NOW).expect("maintenance");

    // Next announce still in flight 120 s on: nothing is purged.
    manager.maintenance(&[], NOW + 120).expect("maintenance");
    assert!(manager.has_announcement(&peer));
    assert_eq!(manager.message_count(), 1);

    // A relayed announce 90 s old is presence, not departure.
    manager.on_public_packet_seen(&packet_of(MessageType::Announce, b"", NOW_MS - 90_000), NOW);
    assert_eq!(manager.message_count(), 1);

    // Silent past bitchat-android's 180 s: gone, with its traffic.
    manager
        .maintenance(&[], NOW + STALE_PEER_TIMEOUT_SECS + 1)
        .expect("maintenance");
    assert!(!manager.has_announcement(&peer));
    assert_eq!(manager.message_count(), 0);
}

#[test]
fn removing_public_messages_spares_everything_else() {
    let mut manager = manager();
    manager.on_public_packet_seen(&packet_of(MessageType::Announce, b"", NOW_MS), NOW);
    manager.on_public_packet_seen(&message(b"blocked", NOW_MS), NOW);
    manager.on_public_packet_seen(&fragment([0x03; 8], 0, NOW_MS), NOW);

    let blocked = PeerId::from_bytes(&sender());
    manager
        .remove_public_messages_from(&blocked, NOW)
        .expect("remove");
    assert_eq!(manager.message_count(), 0);
    assert_eq!(manager.fragment_count(), 1);
    assert!(manager.has_announcement(&blocked));
}

#[test]
fn removing_a_peer_keeps_its_prekey_bundle() {
    let mut manager = manager();
    let owner_key = [0xAB; 32];
    manager.on_public_packet_seen(&bundle_packet(owner_key, NOW_MS), NOW);
    manager.on_public_packet_seen(&packet_of(MessageType::Announce, b"", NOW_MS), NOW);

    let owner = PeerId::from_noise_key(&owner_key);
    manager.remove_announcement_for_peer(&PeerId::from_bytes(&sender()));
    assert!(
        manager.has_prekey_bundle(&owner),
        "bundles exist to reach owners who left"
    );
}

// -- prekey bundles ----------------------------------------------------

#[test]
fn bundles_are_keyed_by_owner_not_by_sender_id() {
    // One valid bundle sprayed under many fabricated sender IDs must collapse to
    // a single entry, or the per-owner cap becomes a way to starve real bundles.
    let mut manager = manager();
    let owner_key = [0xCD; 32];
    let payload = bundle_packet(owner_key, NOW_MS).payload;

    for i in 0..5u64 {
        let mut packet = BitchatPacket::new(
            MessageType::PrekeyBundle,
            [u8::try_from(i).unwrap_or(0); SENDER_ID_LEN],
            None,
            NOW_MS + i,
            payload.clone(),
            7,
        );
        packet.recipient_id = Some(BROADCAST_RECIPIENT);
        manager.on_public_packet_seen(&packet, NOW);
        assert!(
            !manager.has_prekey_bundle(&PeerId::from_bytes(
                &[u8::try_from(i).unwrap_or(0); SENDER_ID_LEN]
            )),
            "no fabricated sender ID gets its own entry"
        );
    }
    assert!(manager.has_prekey_bundle(&PeerId::from_noise_key(&owner_key)));
}

#[test]
fn a_newer_bundle_replaces_an_older_one_and_a_replay_does_not() {
    let mut manager = manager();
    let owner_key = [0xCE; 32];
    manager.on_public_packet_seen(&bundle_packet(owner_key, NOW_MS), NOW);
    manager.on_public_packet_seen(&bundle_packet(owner_key, NOW_MS - 1000), NOW);

    let sends =
        manager.handle_request_sync(&them(), &empty_request(SyncTypeFlags::PREKEY_BUNDLE), NOW);
    assert_eq!(sends.len(), 1);
    assert_eq!(
        sends[0].packet.timestamp, NOW_MS,
        "the older copy is refused"
    );

    manager.on_public_packet_seen(&bundle_packet(owner_key, NOW_MS + 1000), NOW);
    let sends =
        manager.handle_request_sync(&them(), &empty_request(SyncTypeFlags::PREKEY_BUNDLE), NOW);
    assert_eq!(sends[0].packet.timestamp, NOW_MS + 1000);
}

#[test]
fn an_undecodable_bundle_is_dropped() {
    let mut manager = manager();
    manager.on_public_packet_seen(
        &packet_of(MessageType::PrekeyBundle, b"not a bundle", NOW_MS),
        NOW,
    );
    assert!(
        manager
            .handle_request_sync(&them(), &empty_request(SyncTypeFlags::PREKEY_BUNDLE), NOW)
            .is_empty()
    );
}

#[test]
fn the_owner_cap_bounds_the_bundle_store() {
    let mut manager = manager_with(SyncConfig {
        prekey_bundle_capacity: 2,
        ..SyncConfig::default()
    });
    for i in 0..4u8 {
        manager.on_public_packet_seen(&bundle_packet([0xD0 + i; 32], NOW_MS), NOW);
    }
    let sends =
        manager.handle_request_sync(&them(), &empty_request(SyncTypeFlags::PREKEY_BUNDLE), NOW);
    assert_eq!(sends.len(), 2);

    // A refresh from a known owner still lands: the cap must never block one.
    manager.on_public_packet_seen(&bundle_packet([0xD0; 32], NOW_MS + 5), NOW);
    let sends =
        manager.handle_request_sync(&them(), &empty_request(SyncTypeFlags::PREKEY_BUNDLE), NOW);
    assert_eq!(sends.len(), 2);
    assert!(sends.iter().any(|send| send.packet.timestamp == NOW_MS + 5));
}

// -- type routing ------------------------------------------------------

#[test]
fn a_round_answers_only_the_types_it_asked_for() {
    let mut manager = manager();
    manager.on_public_packet_seen(&message(b"m", NOW_MS), NOW);
    manager.on_public_packet_seen(&fragment([0x04; 8], 0, NOW_MS), NOW);
    manager.on_public_packet_seen(&packet_of(MessageType::FileTransfer, b"f", NOW_MS), NOW);
    manager.on_public_packet_seen(&packet_of(MessageType::GroupMessage, b"g", NOW_MS), NOW);

    let only_fragments =
        manager.handle_request_sync(&them(), &empty_request(SyncTypeFlags::FRAGMENT), NOW);
    assert_eq!(only_fragments.len(), 1);
    assert_eq!(
        only_fragments[0].packet.msg_type,
        MessageType::Fragment.as_u8()
    );

    let messages_and_groups = manager.handle_request_sync(
        &them(),
        &empty_request(SyncTypeFlags::PUBLIC_MESSAGES.union(SyncTypeFlags::GROUP_MESSAGE)),
        NOW,
    );
    assert_eq!(messages_and_groups.len(), 2);
}

#[test]
fn a_request_with_no_type_field_means_the_default_round() {
    let mut manager = manager();
    manager.on_public_packet_seen(&message(b"m", NOW_MS), NOW);
    manager.on_public_packet_seen(&packet_of(MessageType::Announce, b"", NOW_MS), NOW);
    manager.on_public_packet_seen(&fragment([0x05; 8], 0, NOW_MS), NOW);

    let mut request = empty_request(SyncTypeFlags::MESSAGE);
    request.types = None;
    let sends = manager.handle_request_sync(&them(), &request, NOW);
    // announce + message, and no fragment.
    assert_eq!(sends.len(), 2);
    assert!(
        sends
            .iter()
            .all(|send| send.packet.msg_type != MessageType::Fragment.as_u8())
    );
}

#[test]
fn maintenance_emits_one_request_per_due_schedule() {
    let mut manager = manager_with(SyncConfig {
        message_sync_interval_secs: 1,
        fragment_sync_interval_secs: 1,
        file_transfer_sync_interval_secs: 1,
        prekey_bundle_sync_interval_secs: 1,
        board_sync_interval_secs: 1,
        ..SyncConfig::default()
    });

    // No board source wired: that round is skipped entirely.
    let sends = manager.maintenance(&[them()], NOW).expect("maintenance");
    assert_eq!(sends.len(), 4);

    let types: Vec<SyncTypeFlags> = sends
        .iter()
        .map(|send| SyncTypeFlags::from_raw(decode_request(send).types.unwrap_or_default()))
        .collect();
    assert!(types.contains(&SyncTypeFlags::PUBLIC_MESSAGES.union(SyncTypeFlags::GROUP_MESSAGE)));
    assert!(types.contains(&SyncTypeFlags::FRAGMENT));
    assert!(types.contains(&SyncTypeFlags::FILE_TRANSFER));
    assert!(types.contains(&SyncTypeFlags::PREKEY_BUNDLE));

    // Wiring a board source adds its round.
    manager.set_board_packets(Vec::new());
    let sends = manager
        .maintenance(&[them()], NOW + 10)
        .expect("maintenance");
    assert_eq!(sends.len(), 5);
}

#[test]
fn a_schedule_only_fires_once_per_interval() {
    let mut manager = manager_with(SyncConfig {
        message_sync_interval_secs: 15,
        fragment_sync_interval_secs: 0,
        file_transfer_sync_interval_secs: 0,
        prekey_bundle_sync_interval_secs: 0,
        board_sync_interval_secs: 0,
        ..SyncConfig::default()
    });

    assert_eq!(manager.maintenance(&[them()], NOW).expect("tick").len(), 1);
    assert_eq!(
        manager
            .maintenance(&[them()], NOW + 14)
            .expect("tick")
            .len(),
        0
    );
    assert_eq!(
        manager
            .maintenance(&[them()], NOW + 15)
            .expect("tick")
            .len(),
        1
    );
}

#[test]
fn every_connected_peer_gets_its_own_unicast_request() {
    let mut manager = manager_with(SyncConfig {
        fragment_sync_interval_secs: 0,
        file_transfer_sync_interval_secs: 0,
        prekey_bundle_sync_interval_secs: 0,
        board_sync_interval_secs: 0,
        ..SyncConfig::default()
    });

    let peers = [them(), peer("1122334455667788")];
    let sends = manager.maintenance(&peers, NOW).expect("maintenance");
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[0].to, Some(peers[0].clone()));
    assert_eq!(sends[1].to, Some(peers[1].clone()));
    // Both windows are open, which is what makes both sets of answers usable.
    assert_eq!(manager.pending_request_count(), 2);
    assert!(manager.is_valid_response(&peers[0], true, NOW));
    assert!(manager.is_valid_response(&peers[1], true, NOW));
}

#[test]
fn the_initial_request_coalesces_every_enabled_type() {
    let mut manager = manager();
    manager.set_board_packets(Vec::new());
    let request = decode_request(&manager.initial_sync_to(&them(), NOW).expect("request"));
    let types = SyncTypeFlags::from_raw(request.types.expect("types present"));
    for expected in [
        MessageType::Announce,
        MessageType::Message,
        MessageType::GroupMessage,
        MessageType::Fragment,
        MessageType::FileTransfer,
        MessageType::PrekeyBundle,
        MessageType::BoardPost,
    ] {
        assert!(types.contains(expected), "{expected:?} must be asked for");
    }
}

#[test]
fn board_posts_are_served_from_the_injected_source() {
    let mut manager = manager();
    let post = packet_of(
        MessageType::BoardPost,
        b"post",
        NOW_MS - 10 * 24 * 3600 * 1000,
    );
    manager.set_board_packets(vec![post.clone()]);

    // No freshness window applies: the board store owns retention, so a post
    // older than any sync window is still served until its own expiry.
    let sends = manager.handle_request_sync(&them(), &empty_request(SyncTypeFlags::BOARD), NOW);
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].packet.payload, post.payload);
    assert!(sends[0].packet.is_rsr);

    // The cursor still applies to them.
    let mut request = empty_request(SyncTypeFlags::BOARD);
    request.since_timestamp_ms = Some(NOW_MS);
    assert!(
        manager
            .handle_request_sync(&them(), &request, NOW)
            .is_empty()
    );
}

// -- hostile input -----------------------------------------------------

#[test]
fn a_degenerate_filter_is_read_as_holding_nothing() {
    let mut manager = manager();
    manager.on_public_packet_seen(&message(b"m", NOW_MS), NOW);

    // m = 1 decodes to the empty set; garbage data decodes to whatever it
    // decodes to. Neither may panic, and both must leave us willing to serve.
    for (p, m, data) in [
        (1u8, 1u32, vec![]),
        (32, 1, vec![0xFF; 64]),
        (7, u32::MAX, vec![0xFF; 400]),
        (7, 2, vec![0x00; 8]),
    ] {
        let request = RequestSyncPacket {
            p,
            m,
            data,
            types: Some(SyncTypeFlags::MESSAGE.raw()),
            since_timestamp_ms: None,
            fragment_id_filter: None,
        };
        let _ = manager.handle_request_sync(&them(), &request, NOW);
    }
}

#[test]
fn an_empty_store_still_produces_a_well_formed_request() {
    let mut manager = manager();
    let send = manager.initial_sync_to(&them(), NOW).expect("request");
    let request = decode_request(&send);
    assert_eq!(request.m, 1, "m = 1 means: I hold nothing");
    assert!(request.data.is_empty());
    assert_eq!(request.since_timestamp_ms, None);
    assert_eq!(send.packet.ttl, 0);
}

#[test]
fn a_far_future_timestamp_is_carried_rather_than_purged() {
    // Freshness only has a lower bound, matching Swift. A packet claiming a
    // timestamp from next year is not evidence of anything we can act on here;
    // the receive pipeline's ±2-minute check is where that is caught.
    let mut manager = manager();
    manager.on_public_packet_seen(&message(b"future", NOW_MS + 86_400_000), NOW);
    assert_eq!(manager.message_count(), 1);
}

#[test]
fn a_zero_clock_accepts_everything_rather_than_purging_everything() {
    let mut manager = GossipSyncManager::new(&us(), SyncConfig::default(), None, 0);
    manager.on_public_packet_seen(&message(b"epoch", 0), 0);
    assert_eq!(manager.message_count(), 1);
}

// -- archive integration ----------------------------------------------

#[test]
fn public_messages_survive_a_restart() {
    let storage = Arc::new(MemoryStorage::new());

    let mut first = GossipSyncManager::new(
        &us(),
        SyncConfig::default(),
        Some(GossipMessageArchive::new(storage.clone())),
        NOW,
    );
    first.on_public_packet_seen(&message(b"carried across", NOW_MS), NOW);
    first.persist_if_dirty(NOW).expect("persist");

    // A new process, an hour later: the message is still inside its six-hour
    // window, so the device can still hand it to a partition that missed it.
    let second = GossipSyncManager::new(
        &us(),
        SyncConfig::default(),
        Some(GossipMessageArchive::new(storage.clone())),
        NOW + 3600,
    );
    assert_eq!(second.message_count(), 1);
    assert_eq!(
        second.public_message_packets(NOW + 3600)[0].payload,
        b"carried across".to_vec()
    );
}

#[test]
fn the_archive_drops_what_aged_out_while_the_process_was_dead() {
    let storage = Arc::new(MemoryStorage::new());
    let mut first = GossipSyncManager::new(
        &us(),
        SyncConfig::default(),
        Some(GossipMessageArchive::new(storage.clone())),
        NOW,
    );
    first.on_public_packet_seen(&message(b"old", NOW_MS), NOW);
    first.persist_if_dirty(NOW).expect("persist");

    let window = SyncConfig::default().public_message_max_age_secs;
    let second = GossipSyncManager::new(
        &us(),
        SyncConfig::default(),
        Some(GossipMessageArchive::new(storage.clone())),
        NOW + window + 1,
    );
    assert_eq!(second.message_count(), 0);
}

#[test]
fn only_whole_public_messages_are_archived() {
    let storage = Arc::new(MemoryStorage::new());
    let mut manager = GossipSyncManager::new(
        &us(),
        SyncConfig::default(),
        Some(GossipMessageArchive::new(storage.clone())),
        NOW,
    );
    manager.on_public_packet_seen(&fragment([0x06; 8], 0, NOW_MS), NOW);
    manager.on_public_packet_seen(&packet_of(MessageType::FileTransfer, b"f", NOW_MS), NOW);
    manager.on_public_packet_seen(&message(b"m", NOW_MS), NOW);
    manager.persist_if_dirty(NOW).expect("persist");

    let archive = GossipMessageArchive::new(storage.clone());
    let stored = archive.load_packets();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].msg_type, MessageType::Message.as_u8());
}

#[test]
fn a_blocked_sender_is_purged_from_disk_immediately() {
    // Not at the next tick: a relaunch in that gap would restore them.
    let storage = Arc::new(MemoryStorage::new());
    let mut manager = GossipSyncManager::new(
        &us(),
        SyncConfig::default(),
        Some(GossipMessageArchive::new(storage.clone())),
        NOW,
    );
    manager.on_public_packet_seen(&message(b"blocked", NOW_MS), NOW);
    manager.persist_if_dirty(NOW).expect("persist");
    assert_eq!(GossipMessageArchive::new(storage.clone()).load_packets().len(), 1);

    manager
        .remove_public_messages_from(&PeerId::from_bytes(&sender()), NOW)
        .expect("remove");
    assert!(
        GossipMessageArchive::new(storage.clone()).load_packets().is_empty(),
        "the purge must reach disk before anything can restore it"
    );
}

#[test]
fn persisting_is_idempotent_when_nothing_changed() {
    let storage = Arc::new(MemoryStorage::new());
    let mut manager = GossipSyncManager::new(
        &us(),
        SyncConfig::default(),
        Some(GossipMessageArchive::new(storage.clone())),
        NOW,
    );
    manager.on_public_packet_seen(&message(b"m", NOW_MS), NOW);
    manager.persist_if_dirty(NOW).expect("persist");
    let first = storage.load_archive_packets().expect("load");

    manager.persist_if_dirty(NOW).expect("persist again");
    let second = storage.load_archive_packets().expect("load");
    assert_eq!(first, second);
}

#[test]
fn a_driver_without_an_archive_still_works() {
    let mut manager = manager();
    manager.on_public_packet_seen(&message(b"m", NOW_MS), NOW);
    manager
        .persist_if_dirty(NOW)
        .expect("no archive is not an error");
    assert_eq!(manager.message_count(), 1);
}

#[test]
fn archive_updates_only_changed_rows_and_deletes_evicted_packets() {
    let storage = Arc::new(MemoryStorage::new());
    let mut config = SyncConfig::default();
    config.seen_capacity = 2;
    let mut manager = GossipSyncManager::new(
        &us(), config.clone(), Some(GossipMessageArchive::new(storage.clone())), NOW,
    );
    let first = message(b"first", NOW_MS);
    let second = message(b"second", NOW_MS + 1);
    let third = message(b"third", NOW_MS + 2);
    manager.on_public_packet_seen(&first, NOW);
    manager.on_public_packet_seen(&second, NOW);
    manager.persist_if_dirty(NOW).expect("persist first two");
    assert_eq!(storage.load_archive_packets().expect("load").len(), 2);

    manager.on_public_packet_seen(&third, NOW);
    manager.persist_if_dirty(NOW).expect("persist eviction");
    let rows = storage.load_archive_packets().expect("load");
    assert_eq!(rows.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![second.packet_id(), third.packet_id()]);

    let restarted = GossipSyncManager::new(
        &us(), config, Some(GossipMessageArchive::new(storage)), NOW,
    );
    assert_eq!(restarted.public_message_packets(NOW), vec![second, third]);
}
