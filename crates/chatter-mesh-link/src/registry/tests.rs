use super::*;

const DIRECT_TTL: u8 = 7;

fn link(id: &str) -> LinkId {
    LinkId::new(id)
}

fn peer(hex_byte: u8) -> PeerId {
    PeerId::from_bytes(&[hex_byte; 8])
}

fn announcement(nickname: &str) -> AnnouncementPacket {
    AnnouncementPacket::new(
        nickname.to_string(),
        vec![0xAB; 32],
        vec![0xCD; 32],
        None,
        None,
        None,
    )
}

fn packet(msg_type: MessageType, sender: u8, ttl: u8, payload: &[u8]) -> BitchatPacket {
    BitchatPacket::new(
        msg_type,
        [sender; 8],
        None,
        1_700_000_000_000,
        payload.to_vec(),
        ttl,
    )
}

fn announce_packet(sender: u8) -> BitchatPacket {
    packet(MessageType::Announce, sender, DIRECT_TTL, b"announce")
}

fn store_with_bound_link(id: &str, role: LinkRole, owner: &PeerId) -> LinkStateStore {
    let mut store = LinkStateStore::new();
    store.link_up(link(id), role);
    assert_eq!(store.bind_if_unbound(&link(id), owner), BindOutcome::Bound);
    store
}

// -- LinkStateStore: binding ------------------------------------------

#[test]
fn a_link_starts_unbound_and_binds_once() {
    let mut store = LinkStateStore::new();
    store.link_up(link("l1"), LinkRole::Central);
    assert!(store.contains(&link("l1")));
    assert_eq!(store.peer_for(&link("l1")), None);

    assert_eq!(
        store.bind_if_unbound(&link("l1"), &peer(0xAA)),
        BindOutcome::Bound
    );
    assert_eq!(store.peer_for(&link("l1")), Some(&peer(0xAA)));
    assert_eq!(
        store.bind_if_unbound(&link("l1"), &peer(0xAA)),
        BindOutcome::AlreadyBound
    );
}

#[test]
fn an_unverified_announce_can_never_take_a_bound_link() {
    // The whole point of bind_if_unbound: the pre-verification path sees only
    // a sender ID in a header, which anyone can write.
    let mut store = store_with_bound_link("l1", LinkRole::Central, &peer(0xAA));
    assert_eq!(
        store.bind_if_unbound(&link("l1"), &peer(0xBB)),
        BindOutcome::RefusedBoundToOtherPeer(peer(0xAA))
    );
    assert_eq!(store.peer_for(&link("l1")), Some(&peer(0xAA)));
}

#[test]
fn binding_an_unknown_link_is_refused() {
    let mut store = LinkStateStore::new();
    assert_eq!(
        store.bind_if_unbound(&link("ghost"), &peer(0xAA)),
        BindOutcome::UnknownLink
    );
}

#[test]
fn links_to_finds_every_link_one_identity_owns() {
    let mut store = LinkStateStore::new();
    store.link_up(link("l1"), LinkRole::Central);
    store.link_up(link("l2"), LinkRole::Peripheral);
    store.link_up(link("l3"), LinkRole::Central);
    store.bind_if_unbound(&link("l1"), &peer(0xAA));
    store.bind_if_unbound(&link("l2"), &peer(0xAA));
    store.bind_if_unbound(&link("l3"), &peer(0xBB));

    assert_eq!(
        store.links_to(&peer(0xAA)),
        BTreeSet::from([link("l1"), link("l2")])
    );
    assert_eq!(store.links_to(&peer(0xCC)), BTreeSet::new());
}

#[test]
fn presence_is_named_for_our_own_role() {
    let mut store = LinkStateStore::new();
    store.link_up(link("out"), LinkRole::Central);
    store.bind_if_unbound(&link("out"), &peer(0xAA));
    let presence = store.presence(&peer(0xAA));
    assert!(presence.as_central);
    assert!(!presence.as_peripheral);
    assert!(presence.any());

    store.link_up(link("in"), LinkRole::Peripheral);
    store.bind_if_unbound(&link("in"), &peer(0xAA));
    assert_eq!(
        store.presence(&peer(0xAA)),
        LinkPresence {
            as_central: true,
            as_peripheral: true
        }
    );
    assert!(!store.presence(&peer(0xBB)).any());
}

#[test]
fn presence_by_peer_covers_only_bound_links() {
    let mut store = LinkStateStore::new();
    store.link_up(link("l1"), LinkRole::Central);
    store.link_up(link("l2"), LinkRole::Peripheral);
    store.bind_if_unbound(&link("l1"), &peer(0xAA));
    let map = store.presence_by_peer();
    assert_eq!(map.len(), 1);
    assert!(map.get(&peer(0xAA)).is_some_and(|p| p.as_central));
}

#[test]
fn preferred_link_tracks_the_latest_outbound_binding_only() {
    let mut store = LinkStateStore::new();
    store.link_up(link("out1"), LinkRole::Central);
    store.link_up(link("out2"), LinkRole::Central);
    store.link_up(link("in"), LinkRole::Peripheral);

    store.bind_if_unbound(&link("out1"), &peer(0xAA));
    assert_eq!(
        store.preferred_links().get(&peer(0xAA)),
        Some(&link("out1"))
    );
    store.bind_if_unbound(&link("out2"), &peer(0xAA));
    assert_eq!(
        store.preferred_links().get(&peer(0xAA)),
        Some(&link("out2"))
    );

    // Inbound links are the peer's to consolidate; they never become the
    // preferred outbound target.
    store.bind_if_unbound(&link("in"), &peer(0xBB));
    assert_eq!(store.preferred_links().get(&peer(0xBB)), None);
}

#[test]
fn dropping_the_preferred_link_repairs_onto_a_writable_survivor() {
    let mut store = LinkStateStore::new();
    for id in ["s1", "s2", "preferred"] {
        store.link_up(link(id), LinkRole::Central);
        store.bind_if_unbound(&link(id), &peer(0xAA));
    }
    store.set_writable(&link("s2"), true);
    assert_eq!(
        store.preferred_links().get(&peer(0xAA)),
        Some(&link("preferred"))
    );

    assert_eq!(store.link_down(&link("preferred")), Some(peer(0xAA)));
    // s2 is writable, s1 is not: repairing onto s1 would strand directed
    // sends until its characteristic came back.
    assert_eq!(store.preferred_links().get(&peer(0xAA)), Some(&link("s2")));
}

#[test]
fn dropping_a_non_preferred_duplicate_leaves_the_preferred_link_alone() {
    let mut store = LinkStateStore::new();
    store.link_up(link("dup"), LinkRole::Central);
    store.bind_if_unbound(&link("dup"), &peer(0xAA));
    store.link_up(link("main"), LinkRole::Central);
    store.bind_if_unbound(&link("main"), &peer(0xAA));
    assert_eq!(
        store.preferred_links().get(&peer(0xAA)),
        Some(&link("main"))
    );

    let _ = store.link_down(&link("dup"));
    assert_eq!(
        store.preferred_links().get(&peer(0xAA)),
        Some(&link("main"))
    );
}

#[test]
fn dropping_the_last_link_clears_the_preference() {
    let mut store = store_with_bound_link("l1", LinkRole::Central, &peer(0xAA));
    let _ = store.link_down(&link("l1"));
    assert_eq!(store.preferred_links().get(&peer(0xAA)), None);
    assert!(store.links_to(&peer(0xAA)).is_empty());
}

#[test]
fn link_up_is_idempotent_and_keeps_the_binding() {
    let mut store = store_with_bound_link("l1", LinkRole::Central, &peer(0xAA));
    store.set_writable(&link("l1"), true);
    store.link_up(link("l1"), LinkRole::Central);
    assert_eq!(store.peer_for(&link("l1")), Some(&peer(0xAA)));
    assert!(store.is_writable(&link("l1")));
    assert_eq!(store.len(), 1);
}

#[test]
fn role_counts_and_clear() {
    let mut store = LinkStateStore::new();
    store.link_up(link("a"), LinkRole::Central);
    store.link_up(link("b"), LinkRole::Central);
    store.link_up(link("c"), LinkRole::Peripheral);
    store.bind_if_unbound(&link("a"), &peer(0xAA));
    assert_eq!(store.count_role(LinkRole::Central), 2);
    assert_eq!(store.count_role(LinkRole::Peripheral), 1);

    assert_eq!(store.clear(), BTreeSet::from([peer(0xAA)]));
    assert!(store.is_empty());
    assert_eq!(store.preferred_links().get(&peer(0xAA)), None);
}

// -- LinkStateStore: the rebind guards --------------------------------

#[test]
fn an_unverified_announce_yields_no_rebind_witness_at_all() {
    // There is no way to call the rebind path without one: the guard is
    // structural, not a bool a caller can pass wrong.
    assert!(VerifiedDirectAnnounce::new(peer(0xBB), false, true).is_none());
    assert!(VerifiedDirectAnnounce::new(peer(0xBB), true, false).is_none());
    assert!(VerifiedDirectAnnounce::new(peer(0xBB), false, false).is_none());
    assert!(VerifiedDirectAnnounce::new(peer(0xBB), true, true).is_some());
}

#[test]
fn witness_from_packet_requires_the_origination_ttl() {
    let direct = announce_packet(0xBB);
    let mut relayed = announce_packet(0xBB);
    relayed.ttl = DIRECT_TTL - 1;
    let not_announce = packet(MessageType::Message, 0xBB, DIRECT_TTL, b"hi");

    assert!(VerifiedDirectAnnounce::from_packet(peer(0xBB), &direct, true, DIRECT_TTL).is_some());
    assert!(VerifiedDirectAnnounce::from_packet(peer(0xBB), &relayed, true, DIRECT_TTL).is_none());
    assert!(
        VerifiedDirectAnnounce::from_packet(peer(0xBB), &not_announce, true, DIRECT_TTL).is_none()
    );
    assert!(VerifiedDirectAnnounce::from_packet(peer(0xBB), &direct, false, DIRECT_TTL).is_none());
}

#[test]
fn a_verified_direct_announce_rebinds_a_rotated_peer_id() {
    // The legitimate case: the peer relaunched, rotated its ID, and the open
    // connection still points at the retired one.
    let mut store = store_with_bound_link("l1", LinkRole::Central, &peer(0xAA));
    let witness = VerifiedDirectAnnounce::new(peer(0xBB), true, true).unwrap();

    let outcome = store.rebind_after_verified_direct(&link("l1"), &witness, 1_000);
    let rebind = outcome.rebound().expect("rebind allowed");
    assert_eq!(rebind.previous_peer, peer(0xAA));
    assert_eq!(rebind.new_peer, peer(0xBB));
    assert!(rebind.retired_links.is_empty());
    assert!(rebind.retire_previous_peer);

    assert_eq!(store.peer_for(&link("l1")), Some(&peer(0xBB)));
    assert!(store.links_to(&peer(0xAA)).is_empty());
    assert_eq!(store.preferred_links().get(&peer(0xBB)), Some(&link("l1")));
    assert_eq!(store.preferred_links().get(&peer(0xAA)), None);
}

#[test]
fn rebind_is_refused_when_the_identity_already_owns_a_live_link() {
    // The containment that matters most: a replayed announce cannot displace
    // an identity that is genuinely connected elsewhere.
    let mut store = store_with_bound_link("victim", LinkRole::Central, &peer(0xBB));
    store.link_up(link("attacker"), LinkRole::Peripheral);
    store.bind_if_unbound(&link("attacker"), &peer(0xAA));

    let witness = VerifiedDirectAnnounce::new(peer(0xBB), true, true).unwrap();
    let outcome = store.rebind_after_verified_direct(&link("attacker"), &witness, 1_000);
    assert_eq!(
        outcome.refusal(),
        Some(&RebindRefusal::IdentityOwnsAnotherLink)
    );

    // Nothing moved, and no cooldown was spent on the refusal.
    assert_eq!(store.peer_for(&link("attacker")), Some(&peer(0xAA)));
    assert_eq!(store.peer_for(&link("victim")), Some(&peer(0xBB)));
    assert_eq!(
        store.links_to(&peer(0xBB)),
        BTreeSet::from([link("victim")])
    );
}

#[test]
fn rebind_is_refused_inside_the_cooldown_and_allowed_after_it() {
    let mut store = store_with_bound_link("l1", LinkRole::Central, &peer(0xAA));

    let first = VerifiedDirectAnnounce::new(peer(0xBB), true, true).unwrap();
    assert!(store
        .rebind_after_verified_direct(&link("l1"), &first, 1_000)
        .rebound()
        .is_some());

    // A third identity replays immediately: refused, with the remaining
    // window reported.
    let second = VerifiedDirectAnnounce::new(peer(0xCC), true, true).unwrap();
    assert_eq!(
        store.rebind_after_verified_direct(&link("l1"), &second, 1_500),
        RebindOutcome::Refused(RebindRefusal::CooldownActive {
            remaining_ms: LINK_REBIND_COOLDOWN_MS - 500
        })
    );
    assert_eq!(store.peer_for(&link("l1")), Some(&peer(0xBB)));

    // One millisecond before the window closes: still refused.
    assert_eq!(
        store.rebind_after_verified_direct(
            &link("l1"),
            &second,
            1_000 + LINK_REBIND_COOLDOWN_MS - 1
        ),
        RebindOutcome::Refused(RebindRefusal::CooldownActive { remaining_ms: 1 })
    );

    // Exactly at the window: the entry is forgotten, so the rebind is allowed.
    // (bitchat keeps entries with age strictly < the window.)
    let outcome =
        store.rebind_after_verified_direct(&link("l1"), &second, 1_000 + LINK_REBIND_COOLDOWN_MS);
    assert!(outcome.rebound().is_some());
    assert_eq!(store.peer_for(&link("l1")), Some(&peer(0xCC)));
}

#[test]
fn the_cooldown_is_per_link_not_global() {
    let mut store = LinkStateStore::new();
    for id in ["l1", "l2"] {
        store.link_up(link(id), LinkRole::Central);
    }
    store.bind_if_unbound(&link("l1"), &peer(0xAA));
    store.bind_if_unbound(&link("l2"), &peer(0xBB));

    let to_cc = VerifiedDirectAnnounce::new(peer(0xCC), true, true).unwrap();
    assert!(store
        .rebind_after_verified_direct(&link("l1"), &to_cc, 1_000)
        .rebound()
        .is_some());
    let to_dd = VerifiedDirectAnnounce::new(peer(0xDD), true, true).unwrap();
    assert!(store
        .rebind_after_verified_direct(&link("l2"), &to_dd, 1_100)
        .rebound()
        .is_some());
}

#[test]
fn a_clock_that_steps_backwards_keeps_the_cooldown_closed() {
    let mut store = store_with_bound_link("l1", LinkRole::Central, &peer(0xAA));
    let first = VerifiedDirectAnnounce::new(peer(0xBB), true, true).unwrap();
    store.rebind_after_verified_direct(&link("l1"), &first, 10_000);

    let second = VerifiedDirectAnnounce::new(peer(0xCC), true, true).unwrap();
    assert_eq!(
        store.rebind_after_verified_direct(&link("l1"), &second, 5_000),
        RebindOutcome::Refused(RebindRefusal::CooldownActive {
            remaining_ms: LINK_REBIND_COOLDOWN_MS
        })
    );
}

#[test]
fn rebinding_an_unbound_or_unknown_link_is_not_a_rebind() {
    let mut store = LinkStateStore::new();
    store.link_up(link("l1"), LinkRole::Central);
    let witness = VerifiedDirectAnnounce::new(peer(0xBB), true, true).unwrap();

    assert_eq!(
        store.rebind_after_verified_direct(&link("l1"), &witness, 1_000),
        RebindOutcome::Refused(RebindRefusal::LinkUnbound)
    );
    assert_eq!(
        store.rebind_after_verified_direct(&link("ghost"), &witness, 1_000),
        RebindOutcome::Refused(RebindRefusal::UnknownLink)
    );
    // A refused rebind must not have spent the link's cooldown.
    store.bind_if_unbound(&link("l1"), &peer(0xAA));
    assert!(store
        .rebind_after_verified_direct(&link("l1"), &witness, 1_001)
        .rebound()
        .is_some());
}

#[test]
fn reclaiming_your_own_link_is_a_no_op_that_does_not_spend_the_cooldown() {
    let mut store = store_with_bound_link("l1", LinkRole::Central, &peer(0xAA));
    let same = VerifiedDirectAnnounce::new(peer(0xAA), true, true).unwrap();
    assert_eq!(
        store.rebind_after_verified_direct(&link("l1"), &same, 1_000),
        RebindOutcome::Refused(RebindRefusal::SamePeer)
    );

    let other = VerifiedDirectAnnounce::new(peer(0xBB), true, true).unwrap();
    assert!(store
        .rebind_after_verified_direct(&link("l1"), &other, 1_000)
        .rebound()
        .is_some());
}

#[test]
fn rebind_retires_the_rotated_ids_other_outbound_links() {
    // A relaunch leaves the same device holding several outbound links under
    // the retired ID; they duplicate every packet until they are cancelled.
    let mut store = LinkStateStore::new();
    for id in ["stale1", "stale2", "ingress"] {
        store.link_up(link(id), LinkRole::Central);
        store.bind_if_unbound(&link(id), &peer(0xAA));
    }
    let witness = VerifiedDirectAnnounce::new(peer(0xBB), true, true).unwrap();
    let outcome = store.rebind_after_verified_direct(&link("ingress"), &witness, 1_000);
    let rebind = outcome.rebound().expect("rebind allowed");

    assert_eq!(rebind.retired_links, vec![link("stale1"), link("stale2")]);
    assert!(rebind.retire_previous_peer);
    assert!(!store.contains(&link("stale1")));
    assert!(!store.contains(&link("stale2")));
    assert!(store.contains(&link("ingress")));
}

#[test]
fn rebind_leaves_inbound_links_of_the_rotated_id_alone() {
    // We cannot cancel a connection the peer opened to us; it retires its own
    // side by running this same policy. So the old identity survives.
    let mut store = LinkStateStore::new();
    store.link_up(link("ingress"), LinkRole::Central);
    store.link_up(link("theirs"), LinkRole::Peripheral);
    store.bind_if_unbound(&link("ingress"), &peer(0xAA));
    store.bind_if_unbound(&link("theirs"), &peer(0xAA));

    let witness = VerifiedDirectAnnounce::new(peer(0xBB), true, true).unwrap();
    let outcome = store.rebind_after_verified_direct(&link("ingress"), &witness, 1_000);
    let rebind = outcome.rebound().expect("rebind allowed");
    assert!(rebind.retired_links.is_empty());
    assert!(!rebind.retire_previous_peer);
    assert_eq!(
        store.links_to(&peer(0xAA)),
        BTreeSet::from([link("theirs")])
    );
}

#[test]
fn rebind_drops_the_links_noise_proof() {
    // The proof belongs to the previous physical binding. Carrying it across
    // would hand a replayer an authentication it never performed.
    let mut store = store_with_bound_link("l1", LinkRole::Central, &peer(0xAA));
    assert!(store.mark_noise_authenticated(&link("l1"), &peer(0xAA)));
    assert!(store.is_noise_authenticated(&link("l1"), &peer(0xAA)));

    let witness = VerifiedDirectAnnounce::new(peer(0xBB), true, true).unwrap();
    assert!(store
        .rebind_after_verified_direct(&link("l1"), &witness, 1_000)
        .rebound()
        .is_some());
    assert!(!store.is_noise_authenticated(&link("l1"), &peer(0xAA)));
    assert!(!store.is_noise_authenticated(&link("l1"), &peer(0xBB)));
}

#[test]
fn noise_proof_requires_the_binding_to_agree() {
    let mut store = store_with_bound_link("l1", LinkRole::Central, &peer(0xAA));
    // A proof from a peer the link is not bound to (a relayed handshake)
    // authenticates nothing on it.
    store.mark_noise_authenticated(&link("l1"), &peer(0xBB));
    assert!(!store.is_noise_authenticated(&link("l1"), &peer(0xBB)));
    store.mark_noise_authenticated(&link("l1"), &peer(0xAA));
    assert!(store.is_noise_authenticated(&link("l1"), &peer(0xAA)));

    let _ = store.link_down(&link("l1"));
    assert!(!store.is_noise_authenticated(&link("l1"), &peer(0xAA)));
    assert!(!store.mark_noise_authenticated(&link("ghost"), &peer(0xAA)));
}

#[test]
fn the_cooldown_survives_a_reconnect_of_the_same_link() {
    // Link IDs are stable per device, so forgetting the cooldown on
    // disconnect would let a replayer reset the guard by forcing a bounce.
    let mut store = store_with_bound_link("l1", LinkRole::Central, &peer(0xAA));
    let first = VerifiedDirectAnnounce::new(peer(0xBB), true, true).unwrap();
    store.rebind_after_verified_direct(&link("l1"), &first, 1_000);

    let _ = store.link_down(&link("l1"));
    store.link_up(link("l1"), LinkRole::Central);
    store.bind_if_unbound(&link("l1"), &peer(0xAA));

    let second = VerifiedDirectAnnounce::new(peer(0xCC), true, true).unwrap();
    assert!(matches!(
        store.rebind_after_verified_direct(&link("l1"), &second, 2_000),
        RebindOutcome::Refused(RebindRefusal::CooldownActive { .. })
    ));
}

// -- PeerRegistry -----------------------------------------------------

fn registry_with(peer_id: PeerId, verified: bool, connected: bool, last_seen: u64) -> PeerRegistry {
    let mut registry = PeerRegistry::new();
    registry.upsert(PeerRecord {
        peer: peer_id,
        nickname: "nick".into(),
        connected,
        noise_public_key: None,
        signing_public_key: None,
        verified_nickname: verified,
        last_seen_ms: last_seen,
        capabilities: PeerCapabilities::default(),
        bridge_geohash: None,
    });
    registry
}

#[test]
fn a_verified_announce_records_the_peer_and_reports_what_changed() {
    let mut registry = PeerRegistry::new();
    let first = registry.upsert_verified_announce(&peer(0xAA), &announcement("alice"), false, 10);
    assert!(first.is_new_peer);
    assert!(!first.was_disconnected);
    assert_eq!(first.previous_nickname, None);

    let second = registry.upsert_verified_announce(&peer(0xAA), &announcement("alice2"), true, 20);
    assert!(!second.is_new_peer);
    assert!(second.was_disconnected);
    assert_eq!(second.previous_nickname, Some("alice".to_string()));

    let record = registry.info(&peer(0xAA)).unwrap();
    assert_eq!(record.nickname, "alice2");
    assert!(record.verified_nickname);
    assert_eq!(record.last_seen_ms, 20);
    assert_eq!(record.noise_public_key.as_deref(), Some(&[0xAB; 32][..]));
    assert_eq!(registry.connected_count(), 1);
}

#[test]
fn a_connected_peer_is_reachable_regardless_of_age() {
    let registry = registry_with(peer(0xAA), false, true, 0);
    assert!(registry.is_reachable(&peer(0xAA), 10_000_000));
}

#[test]
fn a_remembered_peer_is_unreachable_when_we_have_no_mesh_at_all() {
    // Nothing is connected, so there is no path a memory could be used over.
    let registry = registry_with(peer(0xAA), true, false, 0);
    assert!(!registry.is_reachable(&peer(0xAA), 1_000));
}

#[test]
fn reachability_retention_differs_for_verified_and_unverified_at_the_boundary() {
    let mut registry = registry_with(peer(0xAA), true, false, 0);
    registry.upsert(PeerRecord {
        peer: peer(0xBB),
        nickname: "unverified".into(),
        connected: false,
        noise_public_key: None,
        signing_public_key: None,
        verified_nickname: false,
        last_seen_ms: 0,
        capabilities: PeerCapabilities::default(),
        bridge_geohash: None,
    });
    // Something must be attached for remembered peers to count at all.
    registry.upsert(PeerRecord {
        peer: peer(0xCC),
        nickname: "anchor".into(),
        connected: true,
        noise_public_key: None,
        signing_public_key: None,
        verified_nickname: true,
        last_seen_ms: 0,
        capabilities: PeerCapabilities::default(),
        bridge_geohash: None,
    });

    // Unverified: 45 s exactly is still reachable, 45 s + 1 ms is not.
    assert!(registry.is_reachable(&peer(0xBB), REACHABILITY_RETENTION_UNVERIFIED_MS));
    assert!(!registry.is_reachable(&peer(0xBB), REACHABILITY_RETENTION_UNVERIFIED_MS + 1));
    // Verified is still reachable at that same instant — the whole point of
    // the split — and out to 60 s exactly.
    assert!(registry.is_reachable(&peer(0xAA), REACHABILITY_RETENTION_UNVERIFIED_MS + 1));
    assert!(registry.is_reachable(&peer(0xAA), REACHABILITY_RETENTION_VERIFIED_MS));
    assert!(!registry.is_reachable(&peer(0xAA), REACHABILITY_RETENTION_VERIFIED_MS + 1));
}

#[test]
fn reachability_lookups_reduce_a_full_noise_key_to_its_short_form() {
    let key = vec![0xAB; 32];
    let short = PeerId::from_noise_key(&key);
    let registry = registry_with(short.clone(), true, true, 0);
    let long = PeerId::from_str(&"ab".repeat(32));
    assert_eq!(long.to_short(), short);
    assert!(registry.is_connected(&long));
    assert!(registry.is_reachable(&long, 0));
}

#[test]
fn an_idle_connected_peer_with_a_live_link_is_left_connected() {
    let mut registry = registry_with(peer(0xAA), true, true, 0);
    let mut presence = BTreeMap::new();
    presence.insert(
        peer(0xAA),
        LinkPresence {
            as_central: true,
            as_peripheral: false,
        },
    );
    let changes = registry.reconcile_connectivity(PEER_INACTIVITY_TIMEOUT_MS + 1_000, &presence);
    assert!(changes.is_empty());
    assert!(registry.info(&peer(0xAA)).unwrap().connected);
}

#[test]
fn an_idle_connected_peer_with_no_link_is_demoted_at_the_boundary() {
    let presence = BTreeMap::new();
    let mut registry = registry_with(peer(0xAA), true, true, 0);
    // Exactly the timeout is not yet over it (Swift uses a strict >).
    assert!(registry
        .reconcile_connectivity(PEER_INACTIVITY_TIMEOUT_MS, &presence)
        .is_empty());
    let changes = registry.reconcile_connectivity(PEER_INACTIVITY_TIMEOUT_MS + 1, &presence);
    assert_eq!(changes.disconnected, vec![peer(0xAA)]);
    assert!(changes.removed.is_empty());
    assert!(!registry.info(&peer(0xAA)).unwrap().connected);
}

#[test]
fn a_peer_demoted_this_pass_is_not_also_removed_in_it() {
    // It gets a full retention window as a remembered peer instead.
    let presence = BTreeMap::new();
    let mut registry = registry_with(peer(0xAA), false, true, 0);
    let changes = registry.reconcile_connectivity(1_000_000, &presence);
    assert_eq!(changes.disconnected, vec![peer(0xAA)]);
    assert!(changes.removed.is_empty());
    assert!(registry.info(&peer(0xAA)).is_some());

    let changes = registry.reconcile_connectivity(1_000_000, &presence);
    assert_eq!(
        changes.removed,
        vec![RemovedPeer {
            peer: peer(0xAA),
            nickname: "nick".into()
        }]
    );
    assert!(registry.info(&peer(0xAA)).is_none());
}

#[test]
fn disconnected_peers_are_evicted_on_their_own_retention_window() {
    let presence = BTreeMap::new();
    let mut verified = registry_with(peer(0xAA), true, false, 0);
    let mut unverified = registry_with(peer(0xAA), false, false, 0);

    assert!(verified
        .reconcile_connectivity(REACHABILITY_RETENTION_VERIFIED_MS, &presence)
        .is_empty());
    assert!(!verified.is_empty());
    assert_eq!(
        verified
            .reconcile_connectivity(REACHABILITY_RETENTION_VERIFIED_MS + 1, &presence)
            .removed
            .len(),
        1
    );

    // The unverified peer is gone while the verified one is still held.
    assert_eq!(
        unverified
            .reconcile_connectivity(REACHABILITY_RETENTION_UNVERIFIED_MS + 1, &presence)
            .removed
            .len(),
        1
    );
}

#[test]
fn capabilities_and_bridge_geohash_come_from_the_announce() {
    let mut registry = PeerRegistry::new();
    let mut announcement = announcement("bridge");
    announcement.capabilities = Some(PeerCapabilities::from_bits(
        PeerCapabilities::BRIDGE | PeerCapabilities::PREKEYS,
    ));
    announcement.bridge_geohash = Some("u4pruy".into());
    registry.upsert_verified_announce(&peer(0xAA), &announcement, true, 0);

    let record = registry.info(&peer(0xAA)).unwrap();
    assert!(record.capabilities.contains(PeerCapabilities::BRIDGE));
    assert_eq!(record.bridge_geohash.as_deref(), Some("u4pruy"));
}

/// An announce whose peer ID really is derived from its Noise key, which
/// is what a persisted pin has to be.
fn derived(noise_byte: u8, signing_byte: u8, nickname: &str) -> (PeerId, AnnouncementPacket) {
    let noise = vec![noise_byte; 32];
    let announcement = AnnouncementPacket::new(
        nickname.to_string(),
        noise.clone(),
        vec![signing_byte; 32],
        None,
        None,
        None,
    );
    (PeerId::from_noise_key(&noise), announcement)
}

#[test]
fn the_first_announce_pins_and_a_later_one_does_not_move_the_pin() {
    let mut registry = PeerRegistry::new();
    let (alice, first) = derived(1, 2, "alice");

    assert!(
        registry
            .upsert_verified_announce(&alice, &first, true, 10)
            .pinned_now
    );

    let (_, renamed) = derived(1, 9, "alice-renamed");
    let update = registry.upsert_verified_announce(&alice, &renamed, true, 20);

    assert!(!update.pinned_now);
    let pin = registry.pinned(&alice).expect("pinned");
    assert_eq!(pin.signing_public_key, vec![2; 32]);
    assert_eq!(pin.nickname, "alice");
    assert_eq!(pin.pinned_at_ms, 10);
}

#[test]
fn forgetting_a_peer_does_not_forget_its_pin() {
    let mut registry = PeerRegistry::new();
    let (alice, announcement) = derived(1, 2, "alice");
    registry.upsert_verified_announce(&alice, &announcement, false, 0);

    // Aged out as a disconnected peer, then removed outright, then the
    // whole registry cleared: every way a record can go.
    let removed =
        registry.reconcile_connectivity(REACHABILITY_RETENTION_VERIFIED_MS + 1, &BTreeMap::new());
    assert_eq!(removed.removed.len(), 1);
    registry.remove(&alice);
    registry.remove_all();

    assert!(registry.info(&alice).is_none());
    assert_eq!(
        registry
            .pinned(&alice)
            .map(|pin| pin.signing_public_key.clone()),
        Some(vec![2; 32])
    );
}

#[test]
fn pins_survive_a_round_trip_through_storage() {
    let mut registry = PeerRegistry::new();
    let (alice, a) = derived(1, 2, "alice");
    let (bob, b) = derived(3, 4, "bob");
    registry.upsert_verified_announce(&alice, &a, true, 10);
    registry.upsert_verified_announce(&bob, &b, true, 20);

    let mut restarted = PeerRegistry::new();
    restarted
        .restore_pins(&registry.pins_to_bytes())
        .expect("restore");

    assert_eq!(restarted.pinned(&alice), registry.pinned(&alice));
    assert_eq!(restarted.pinned(&bob), registry.pinned(&bob));
    assert!(restarted.is_empty(), "a pin is not presence");
}

#[test]
fn a_restored_pin_does_not_replace_one_already_held() {
    let (alice, stored) = derived(1, 2, "alice");
    let mut old = PeerRegistry::new();
    old.upsert_verified_announce(&alice, &stored, true, 10);

    let mut registry = PeerRegistry::new();
    let (_, live) = derived(1, 7, "alice");
    registry.upsert_verified_announce(&alice, &live, true, 99);
    registry
        .restore_pins(&old.pins_to_bytes())
        .expect("restore");

    assert_eq!(
        registry.pinned(&alice).map(|pin| pin.pinned_at_ms),
        Some(99)
    );
}

#[test]
fn a_damaged_pin_blob_is_refused_whole() {
    let mut registry = PeerRegistry::new();
    let (alice, a) = derived(1, 2, "alice");
    let (bob, b) = derived(3, 4, "bob");
    registry.upsert_verified_announce(&alice, &a, true, 10);
    registry.upsert_verified_announce(&bob, &b, true, 20);
    let good = registry.pins_to_bytes();

    let mut wrong_version = good.clone();
    wrong_version[0] = PIN_BLOB_VERSION + 1;

    // Same length, but the first pin's Noise key no longer derives to the
    // ID it is filed under.
    let mut swapped_key = good.clone();
    let key_start = 1 + 1 + alice.as_str().len().max(bob.as_str().len()) + 1;
    swapped_key[key_start] ^= 0xFF;

    for blob in [
        &good[..good.len() - 1],
        &[][..],
        &wrong_version,
        &swapped_key,
    ] {
        let mut target = PeerRegistry::new();
        assert_eq!(target.restore_pins(blob), Err(MalformedPins));
        assert!(target.pinned(&alice).is_none() && target.pinned(&bob).is_none());
    }
}

#[test]
fn a_full_pin_set_refuses_newcomers_and_keeps_what_it_has() {
    let mut registry = PeerRegistry::new();
    let (alice, announcement) = derived(1, 2, "alice");
    registry.upsert_verified_announce(&alice, &announcement, true, 0);
    for n in 0..MAX_PINNED_IDENTITIES as u64 {
        let noise = n.to_be_bytes().repeat(4);
        let flood = AnnouncementPacket::new(
            "flood".to_string(),
            noise.clone(),
            vec![0; 32],
            None,
            None,
            None,
        );
        registry.upsert_verified_announce(&PeerId::from_noise_key(&noise), &flood, true, 1);
    }

    let (late, announcement) = derived(0xEE, 0xEF, "late");
    let update = registry.upsert_verified_announce(&late, &announcement, true, 2);

    assert!(!update.pinned_now);
    assert!(registry.pinned(&late).is_none());
    assert!(
        registry.info(&late).is_some(),
        "still a peer for this session"
    );
    assert!(registry.pinned(&alice).is_some());
}

#[test]
fn nickname_can_be_restricted_to_connected_peers() {
    let registry = registry_with(peer(0xAA), true, false, 0);
    assert_eq!(registry.nickname(&peer(0xAA), false), Some("nick"));
    assert_eq!(registry.nickname(&peer(0xAA), true), None);
    let connected = registry_with(peer(0xAA), true, true, 0);
    assert_eq!(connected.nickname(&peer(0xAA), true), Some("nick"));
    assert_eq!(registry.nickname(&peer(0xBB), false), None);
}

#[test]
fn update_last_seen_and_remove() {
    let mut registry = registry_with(peer(0xAA), true, true, 0);
    registry.update_last_seen(&peer(0xAA), 5_000);
    assert_eq!(registry.info(&peer(0xAA)).unwrap().last_seen_ms, 5_000);
    registry.update_last_seen(&peer(0xBB), 5_000); // unknown: no-op
    assert!(registry.remove(&peer(0xAA)).is_some());
    assert!(registry.remove(&peer(0xAA)).is_none());
    assert!(registry.is_empty());
}

// -- IngressRegistry --------------------------------------------------

#[test]
fn an_ingress_record_remembers_the_link_and_last_hop() {
    let mut ingress = IngressRegistry::new();
    let pkt = packet(MessageType::Message, 0xAA, 5, b"hello");
    assert!(ingress.record_if_new(
        &pkt,
        link("l1"),
        peer(0xBB),
        1_000,
        INGRESS_RECORD_LIFETIME_MS
    ));
    assert_eq!(ingress.link_for(&pkt), Some(&link("l1")));
    // The last hop is not the claimed sender: 0xAA wrote the packet, 0xBB
    // handed it to us.
    assert_eq!(ingress.peer_for(&pkt), Some(&peer(0xBB)));
    assert_eq!(ingress.len(), 1);
}

#[test]
fn a_second_copy_inside_the_lifetime_is_reported_as_not_new() {
    let mut ingress = IngressRegistry::new();
    let pkt = packet(MessageType::Message, 0xAA, 5, b"hello");
    assert!(ingress.record_if_new(
        &pkt,
        link("l1"),
        peer(0xAA),
        1_000,
        INGRESS_RECORD_LIFETIME_MS
    ));
    // Same packet arriving by another path within the window: suppressed, and
    // the original link record is kept.
    assert!(!ingress.record_if_new(
        &pkt,
        link("l2"),
        peer(0xBB),
        1_000 + INGRESS_RECORD_LIFETIME_MS,
        INGRESS_RECORD_LIFETIME_MS
    ));
    assert_eq!(ingress.link_for(&pkt), Some(&link("l1")));
}

#[test]
fn a_record_expires_after_its_lifetime_and_is_replaced() {
    let mut ingress = IngressRegistry::new();
    let pkt = packet(MessageType::Message, 0xAA, 5, b"hello");
    ingress.record_if_new(
        &pkt,
        link("l1"),
        peer(0xAA),
        1_000,
        INGRESS_RECORD_LIFETIME_MS,
    );
    assert!(ingress.record_if_new(
        &pkt,
        link("l2"),
        peer(0xBB),
        1_000 + INGRESS_RECORD_LIFETIME_MS + 1,
        INGRESS_RECORD_LIFETIME_MS
    ));
    assert_eq!(ingress.link_for(&pkt), Some(&link("l2")));
}

#[test]
fn prune_drops_records_past_the_lifetime_only() {
    let mut ingress = IngressRegistry::new();
    let old = packet(MessageType::Message, 0xAA, 5, b"old");
    let fresh = packet(MessageType::Message, 0xAA, 5, b"fresh");
    ingress.record_if_new(&old, link("l1"), peer(0xAA), 0, INGRESS_RECORD_LIFETIME_MS);
    ingress.record_if_new(
        &fresh,
        link("l1"),
        peer(0xAA),
        2_000,
        INGRESS_RECORD_LIFETIME_MS,
    );

    ingress.prune(INGRESS_RECORD_LIFETIME_MS, INGRESS_RECORD_LIFETIME_MS);
    assert_eq!(ingress.len(), 2); // the older one is exactly at the boundary

    ingress.prune(INGRESS_RECORD_LIFETIME_MS + 1, INGRESS_RECORD_LIFETIME_MS);
    assert_eq!(ingress.len(), 1);
    assert!(ingress.record(&old).is_none());
    assert!(ingress.record(&fresh).is_some());

    ingress.prune(10_000, INGRESS_RECORD_LIFETIME_MS);
    assert!(ingress.is_empty());
}

#[test]
fn distinct_packets_get_distinct_records() {
    let mut ingress = IngressRegistry::new();
    let a = packet(MessageType::Message, 0xAA, 5, b"a");
    let b = packet(MessageType::Message, 0xAA, 5, b"b");
    assert!(ingress.record_if_new(&a, link("l1"), peer(0xAA), 0, INGRESS_RECORD_LIFETIME_MS));
    assert!(ingress.record_if_new(&b, link("l2"), peer(0xBB), 0, INGRESS_RECORD_LIFETIME_MS));
    assert_eq!(ingress.link_for(&a), Some(&link("l1")));
    assert_eq!(ingress.link_for(&b), Some(&link("l2")));

    // The relay decrements the TTL; the key must not follow it, or a relayed
    // copy would look like a different packet.
    let mut relayed = a.clone();
    relayed.ttl = 4;
    assert_eq!(ingress.link_for(&relayed), Some(&link("l1")));
}

#[test]
fn clear_empties_the_ingress_memory() {
    let mut ingress = IngressRegistry::new();
    let pkt = packet(MessageType::Message, 0xAA, 5, b"hello");
    ingress.record_if_new(&pkt, link("l1"), peer(0xAA), 0, INGRESS_RECORD_LIFETIME_MS);
    ingress.clear();
    assert!(ingress.is_empty());
    assert!(ingress.record(&pkt).is_none());
}

#[test]
fn refusal_and_rejection_render_for_logs() {
    assert!(RebindRefusal::IdentityOwnsAnotherLink
        .to_string()
        .contains("already owns"));
    assert!(RebindRefusal::CooldownActive { remaining_ms: 5 }
        .to_string()
        .contains('5'));
}

#[test]
fn constants_match_transport_config() {
    assert_eq!(REACHABILITY_RETENTION_VERIFIED_MS, 60_000);
    assert_eq!(REACHABILITY_RETENTION_UNVERIFIED_MS, 45_000);
    assert_eq!(PEER_INACTIVITY_TIMEOUT_MS, 8_000);
    assert_eq!(INGRESS_RECORD_LIFETIME_MS, 3_000);
    assert_eq!(LINK_REBIND_COOLDOWN_MS, 60_000);
    assert_eq!(reachability_retention_ms(true), 60_000);
    assert_eq!(reachability_retention_ms(false), 45_000);
}
