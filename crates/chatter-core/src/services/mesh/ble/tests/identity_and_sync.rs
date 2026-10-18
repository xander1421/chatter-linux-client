
#[tokio::test]
async fn a_peer_heard_only_by_relay_is_not_marked_connected() {
    // Found with three devices in one room: the second phone's announces
    // arrived over the *first* phone's link, we marked it connected anyway,
    // and `reconcile_connectivity` demoted it 8s later for having no link of
    // its own — so it flapped `reconnected=true` on every announce forever.
    //
    // "We heard from it" is not "it is reachable on a link of its own".
    let (mut runtime, _radio, state) = runtime();
    let relay = PeerId::from_str("aaaaaaaaaaaaaaaa");

    // One link, owned by the relay.
    let link = LinkId::new("/mock/relay");
    state.with(|inner| {
        inner.links.link_up(link.clone(), LinkRole::Central);
        inner.links.set_writable(&link, true);
        inner.max_write.insert(link.clone(), 512);
        inner.links.bind_if_unbound(&link, &relay);
    });

    // A signed announce authored by the distant peer, arriving on that link.
    // Its ID has to be the one its own Noise key derives, as a real device's
    // is — an announce that claims any other ID is now refused outright.
    let identity = MeshIdentity::generate(&mut os_rng());
    let distant = identity.peer_id();
    let announcement = AnnouncementPacket::new(
        "distant".to_string(),
        identity.static_public().to_vec(),
        identity.signing_public().to_vec(),
        None,
        None,
        None,
    );
    let mut packet = BitchatPacket::new(
        MessageType::Announce,
        distant.to_routing_bytes().expect("routing bytes"),
        None,
        now_millis(),
        announcement.encode().expect("encodes"),
        chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
    );
    packet.signature =
        Some(chatter_mesh_noise::sign::sign_packet(&identity, &packet).expect("signs"));

    runtime.on_announce(&link, &packet, &distant).await;

    assert!(
        state.with(|inner| inner.peers.info(&distant).is_some()),
        "a relayed peer is still a peer we know about"
    );
    assert!(
        !state.with(|inner| inner.peers.info(&distant).expect("known").connected),
        "but not one we can reach on a link of its own"
    );
}

/// §98 — the other half of the same attack, once the derivation check closed
/// the first half.
///
/// A peer's Noise public key travels in the clear in every announce it sends,
/// and the peer ID derives from it, so an attacker who has simply HEARD a
/// victim announce can replay that Noise key and the derived ID will match.
/// Everything then hinges on the signing key — which was pinned by nothing.
/// The attacker signs with their own key, the signature verifies against the
/// key the packet supplies, and the victim's identity is taken over in our
/// registry.
///
/// The registry had recorded the victim's signing key since first contact.
/// Nothing compared it.
#[tokio::test]
async fn a_replayed_noise_key_under_a_new_signing_key_cannot_take_over_a_known_peer() {
    let (mut runtime, _radio, state) = runtime();

    let victim_identity = MeshIdentity::generate(&mut os_rng());
    let victim = victim_identity.peer_id();

    // First contact: the victim announces itself, honestly.
    let honest = AnnouncementPacket::new(
        "victim".to_string(),
        victim_identity.static_public().to_vec(),
        victim_identity.signing_public().to_vec(),
        None,
        None,
        None,
    );
    let mut honest_packet = BitchatPacket::new(
        MessageType::Announce,
        victim.to_routing_bytes().expect("routing bytes"),
        None,
        now_millis(),
        honest.encode().expect("encodes"),
        chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
    );
    honest_packet.signature = Some(sign_packet(&victim_identity, &honest_packet).expect("signs"));

    let victim_link = LinkId::new("/mock/victim");
    state.with(|inner| {
        inner.links.link_up(victim_link.clone(), LinkRole::Central);
        inner.links.set_writable(&victim_link, true);
        inner.max_write.insert(victim_link.clone(), 512);
    });
    assert!(
        runtime
            .on_announce(&victim_link, &honest_packet, &victim)
            .await,
        "the honest announce establishes the pin"
    );
    let pinned_signing_key = state
        .with(|inner| {
            inner
                .peers
                .info(&victim)
                .and_then(|r| r.signing_public_key.clone())
        })
        .expect("the registry records the signing key on first contact");

    // The attacker replays the victim's Noise key — so the peer ID still
    // derives correctly — under its own signing key, and self-signs.
    let attacker = MeshIdentity::generate(&mut os_rng());
    let forged = AnnouncementPacket::new(
        "victim".to_string(),
        victim_identity.static_public().to_vec(), // heard off the air
        attacker.signing_public().to_vec(),       // the attacker's own
        None,
        None,
        None,
    );
    let mut forged_packet = BitchatPacket::new(
        MessageType::Announce,
        victim.to_routing_bytes().expect("routing bytes"),
        None,
        now_millis(),
        forged.encode().expect("encodes"),
        chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
    );
    forged_packet.signature = Some(sign_packet(&attacker, &forged_packet).expect("signs"));

    let attacker_signing_key =
        <[u8; 32]>::try_from(attacker.signing_public().as_slice()).expect("32 bytes");
    assert!(
        verify_packet(&forged_packet, &attacker_signing_key),
        "the fixture must be a self-signature that really verifies — that is the attack"
    );

    let attacker_link = LinkId::new("/mock/attacker");
    state.with(|inner| {
        inner
            .links
            .link_up(attacker_link.clone(), LinkRole::Central);
        inner.links.set_writable(&attacker_link, true);
        inner.max_write.insert(attacker_link.clone(), 512);
    });

    runtime.save_pins().expect("save pins");
    state.with(|inner| {
        inner.peers.clear_pins();
        inner.peers.remove(&victim);
    });
    runtime.restore_pins().expect("restore pins");
    assert!(state.with(|inner| inner.peers.info(&victim).is_none()));
    let mut events = runtime.events.subscribe();
    let accepted = runtime
        .on_announce(&attacker_link, &forged_packet, &victim)
        .await;
    assert!(
        matches!(events.try_recv(), Ok(MeshEvent::IdentityKeyChanged { peer }) if peer == victim)
    );

    assert!(!accepted, "the forged announce must be refused");
    assert_eq!(
        state.with(|inner| inner
            .peers
            .pinned(&victim)
            .map(|r| r.signing_public_key.clone())),
        Some(pinned_signing_key),
        "the victim's pinned signing key must survive the attempt"
    );
    assert!(
        state.with(|inner| inner.links.peer_for(&attacker_link).is_none()),
        "and the attacker's link must not be bound to the victim's identity"
    );
}

#[tokio::test]
async fn an_announce_claiming_a_peer_id_it_cannot_derive_is_rejected() {
    // The mesh ID *is* SHA256(noise static key)[..8] — that is how our own
    // is built (`MeshIdentity::peer_id`). An announce is therefore checkable
    // without any crypto at all: derive the ID from the key it carries and
    // compare it to the sender ID on the packet.
    //
    // Verifying only the packet signature against the signing key the
    // announcement itself supplies proves nothing about *who* sent it — it
    // proves whoever wrote the announce holds the key they put in it, which
    // every attacker does. Anyone in radio range could announce any peer ID
    // with their own keys and take over that identity in our registry.
    let (mut runtime, _radio, state) = runtime();

    // A real peer, whose ID genuinely derives from its own Noise key.
    let victim_identity = MeshIdentity::generate(&mut os_rng());
    let victim = victim_identity.peer_id();

    // The attacker claims the victim's ID and supplies its own two keys,
    // then signs with the matching signing secret.
    let attacker = MeshIdentity::generate(&mut os_rng());
    let announcement = AnnouncementPacket::new(
        "victim".to_string(),
        attacker.static_public().to_vec(),
        attacker.signing_public().to_vec(),
        None,
        None,
        None,
    );
    let mut packet = BitchatPacket::new(
        MessageType::Announce,
        victim.to_routing_bytes().expect("routing bytes"),
        None,
        now_millis(),
        announcement.encode().expect("encodes"),
        chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
    );
    packet.signature = Some(sign_packet(&attacker, &packet).expect("signs"));

    // The forgery is *signature valid*. That is the point: the signature is
    // the attacker's own, over their own key, and it verifies.
    let signing_key =
        <[u8; 32]>::try_from(announcement.signing_public_key.as_slice()).expect("32 bytes");
    assert!(
        verify_packet(&packet, &signing_key),
        "the fixture must be a self-signature that really verifies"
    );

    let link = LinkId::new("/mock/attacker");
    state.with(|inner| {
        inner.links.link_up(link.clone(), LinkRole::Central);
        inner.links.set_writable(&link, true);
        inner.max_write.insert(link.clone(), 512);
    });

    let accepted = runtime.on_announce(&link, &packet, &victim).await;

    assert!(!accepted, "the announce must be refused");
    assert!(
        state.with(|inner| inner.peers.info(&victim).is_none()),
        "an announce whose sender ID is not derivable from the Noise key it \
             carries must not enter the peer registry"
    );
    assert!(
        state.with(|inner| inner.links.peer_for(&link).is_none()),
        "and must not bind the attacker's link to the victim's identity"
    );
}

#[tokio::test]
async fn a_sync_request_from_a_peer_that_never_announced_is_not_answered() {
    // A REQUEST_SYNC carrying an empty or undecodable GCS filter matches
    // nothing, so `handle_request_sync` reads it as "this peer holds none of
    // it" and replies with the whole store. The requester is whatever sender
    // ID the packet claimed, and answering it needs no session, no announce
    // and no binding of any kind — so anyone in radio range could ask once
    // and be handed every announce and prekey bundle we hold, each keyed by
    // its owner's identity. That is a social graph of everyone we have met.
    let (mut runtime, radio, state) = runtime();

    // Something identity-bearing in the store: one real peer's announce.
    let subject = MeshIdentity::generate(&mut os_rng());
    let subject_peer = subject.peer_id();
    let announcement = AnnouncementPacket::new(
        "subject".to_string(),
        subject.static_public().to_vec(),
        subject.signing_public().to_vec(),
        None,
        None,
        None,
    );
    let mut announce = BitchatPacket::new(
        MessageType::Announce,
        subject_peer.to_routing_bytes().expect("routing bytes"),
        None,
        now_millis(),
        announcement.encode().expect("encodes"),
        chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
    );
    announce.signature = Some(sign_packet(&subject, &announce).expect("signs"));
    let subject_link = LinkId::new("/mock/subject");
    state.with(|inner| {
        inner.links.link_up(subject_link.clone(), LinkRole::Central);
        inner.links.set_writable(&subject_link, true);
        inner.max_write.insert(subject_link.clone(), 512);
    });
    runtime.on_packet(&subject_link, announce).await;
    // Flush whatever the announce itself set in motion, so the only writes
    // the assertion can see are the answer to the stranger's request.
    runtime.drain_writes().await;
    assert!(
        runtime.gossip.has_announcement(&subject_peer),
        "the fixture must really have something worth leaking"
    );

    // A stranger we have never heard an announce from asks for everything.
    let stranger = PeerId::from_str("cccccccccccccccc");
    let link = LinkId::new("/mock/stranger");
    state.with(|inner| {
        inner.links.link_up(link.clone(), LinkRole::Central);
        inner.links.set_writable(&link, true);
        inner.max_write.insert(link.clone(), 512);
        inner.links.bind_if_unbound(&link, &stranger);
    });
    let mut request = RequestSyncPacket::new(19, 4096, Vec::new());
    request.types = Some(u64::MAX);
    let packet = BitchatPacket::new(
        MessageType::RequestSync,
        stranger.to_routing_bytes().expect("routing bytes"),
        None,
        now_millis(),
        request.encode().expect("encodes"),
        chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
    );

    let before = radio.write_count();
    runtime.on_request_sync(&packet, &stranger).await;
    runtime.drain_writes().await;

    assert_eq!(
        radio.write_count(),
        before,
        "a peer that has never proved an identity to us gets no sync response"
    );
}

#[tokio::test]
async fn a_sync_request_from_a_peer_that_did_announce_is_still_answered() {
    // The gate must not break sync itself: an honest peer announces on
    // connect and then asks, and that request has to be served or two
    // devices in a room never exchange history at all.
    let (mut runtime, radio, state) = runtime();

    let subject = MeshIdentity::generate(&mut os_rng());
    let subject_peer = subject.peer_id();
    let announcement = AnnouncementPacket::new(
        "subject".to_string(),
        subject.static_public().to_vec(),
        subject.signing_public().to_vec(),
        None,
        None,
        None,
    );
    let mut announce = BitchatPacket::new(
        MessageType::Announce,
        subject_peer.to_routing_bytes().expect("routing bytes"),
        None,
        now_millis(),
        announcement.encode().expect("encodes"),
        chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
    );
    announce.signature = Some(sign_packet(&subject, &announce).expect("signs"));
    let link = LinkId::new("/mock/subject");
    state.with(|inner| {
        inner.links.link_up(link.clone(), LinkRole::Central);
        inner.links.set_writable(&link, true);
        inner.max_write.insert(link.clone(), 512);
    });
    runtime.on_packet(&link, announce).await;
    runtime.drain_writes().await;

    let mut request = RequestSyncPacket::new(19, 4096, Vec::new());
    request.types = Some(u64::MAX);
    let packet = BitchatPacket::new(
        MessageType::RequestSync,
        subject_peer.to_routing_bytes().expect("routing bytes"),
        None,
        now_millis(),
        request.encode().expect("encodes"),
        chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
    );

    let before = radio.write_count();
    runtime.on_request_sync(&packet, &subject_peer).await;
    runtime.drain_writes().await;

    assert!(
        radio.write_count() > before,
        "an announced peer must still get its diff"
    );
}

#[tokio::test]
async fn a_refused_announce_is_not_kept_or_re_served_by_the_gossip_store() {
    // Refusing a forgery from our own registry is only half the job. The
    // gossip store keeps the latest announce per peer and serves it to
    // everyone who syncs with us, so retaining one we just refused turns this
    // device into a relay for the forgery. And `on_public_packet_seen` reads
    // an announce older than the stale-peer timeout as "this peer left" and
    // purges the store's state for the claimed sender — which would make an
    // old forged announce a remote wipe of a real peer's gossip state.
    let (mut runtime, _radio, state) = runtime();

    let victim_identity = MeshIdentity::generate(&mut os_rng());
    let victim = victim_identity.peer_id();
    let attacker = MeshIdentity::generate(&mut os_rng());
    let announcement = AnnouncementPacket::new(
        "victim".to_string(),
        attacker.static_public().to_vec(),
        attacker.signing_public().to_vec(),
        None,
        None,
        None,
    );
    let mut packet = BitchatPacket::new(
        MessageType::Announce,
        victim.to_routing_bytes().expect("routing bytes"),
        None,
        now_millis(),
        announcement.encode().expect("encodes"),
        chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
    );
    packet.signature = Some(sign_packet(&attacker, &packet).expect("signs"));

    let link = LinkId::new("/mock/attacker");
    state.with(|inner| {
        inner.links.link_up(link.clone(), LinkRole::Central);
        inner.links.set_writable(&link, true);
        inner.max_write.insert(link.clone(), 512);
    });

    runtime.on_packet(&link, packet).await;

    assert!(
        !runtime.gossip.has_announcement(&victim),
        "a refused announce must not be retained for re-gossip"
    );
}

#[tokio::test]
async fn an_announce_may_not_swap_the_noise_key_recorded_for_a_peer() {
    // Defence in depth behind the derivation check: the sender ID is only
    // eight bytes of the digest, so a second key hashing into the same ID is
    // 2^64 work rather than impossible. Once we have recorded a Noise key
    // for a peer, an announce carrying a *different* one is a key
    // substitution attempt and is refused before the signature is even
    // checked — a valid signature over a swapped key is exactly what the
    // attack looks like (`BLEAnnounceTrustPolicy`, `MessageHandler.kt:252`).
    let (mut runtime, _radio, state) = runtime();

    let honest = MeshIdentity::generate(&mut os_rng());
    let peer = honest.peer_id();
    let first = AnnouncementPacket::new(
        "honest".to_string(),
        honest.static_public().to_vec(),
        honest.signing_public().to_vec(),
        None,
        None,
        None,
    );
    state.with(|inner| {
        inner
            .peers
            .upsert_verified_announce(&peer, &first, true, now_millis());
    });

    // Same peer ID, attacker's keys, signed by the attacker.
    let attacker = MeshIdentity::generate(&mut os_rng());
    let swapped = AnnouncementPacket::new(
        "honest".to_string(),
        attacker.static_public().to_vec(),
        attacker.signing_public().to_vec(),
        None,
        None,
        None,
    );
    let mut packet = BitchatPacket::new(
        MessageType::Announce,
        peer.to_routing_bytes().expect("routing bytes"),
        None,
        now_millis(),
        swapped.encode().expect("encodes"),
        chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
    );
    packet.signature = Some(sign_packet(&attacker, &packet).expect("signs"));

    let link = LinkId::new("/mock/swap");
    state.with(|inner| {
        inner.links.link_up(link.clone(), LinkRole::Central);
        inner.links.set_writable(&link, true);
        inner.max_write.insert(link.clone(), 512);
    });

    runtime.on_announce(&link, &packet, &peer).await;

    assert_eq!(
        state.with(|inner| inner
            .peers
            .info(&peer)
            .expect("still known")
            .noise_public_key
            .clone()),
        Some(honest.static_public().to_vec()),
        "the recorded Noise key must survive an announce that tries to \
             replace it — that key is what courier mail is sealed to"
    );
}

#[tokio::test]
async fn an_empty_public_message_is_not_stored_as_a_blank_row() {
    // An empty payload is valid UTF-8 and decodes without complaint, so it used
    // to become an ordinary row holding `Text("")`. The room then showed a
    // bubble with a sender and a timestamp and no body — indistinguishable from
    // a message whose text failed to load, and impossible to tell apart later
    // because the row is all any surface has.
    //
    // Neither side composes one: `send_room` refuses via `require_content`, and
    // bitchat refuses on its send path too (`MeshCore.kt:415`). Neither checks
    // on receive, which is the gap.
    let (mut runtime, _radio, state) = runtime();
    let sender = PeerId::from_str("eeeeeeeeeeeeeeee");
    let identity = register_verified_peer(&state, &sender, "quiet");
    let mut events = runtime.events.subscribe();

    for payload in [b"".to_vec(), b"   ".to_vec(), b"\n\t".to_vec()] {
        let packet = signed(&sender, &identity, MessageType::Message, payload, true);
        runtime.on_public_message(&packet, &sender);
    }

    assert!(
        events.try_recv().is_err(),
        "an empty broadcast is not a message and must not reach the timeline"
    );

    // The guard is on emptiness alone, not on the sender: a real message from
    // the same peer still arrives.
    let packet = signed(
        &sender,
        &identity,
        MessageType::Message,
        b"still here".to_vec(),
        true,
    );
    runtime.on_public_message(&packet, &sender);
    match events.try_recv() {
        Ok(MeshEvent::Message(message)) => assert_eq!(
            super::super::mapping::text_of(&message.message),
            Some("still here")
        ),
        other => panic!("expected the real message, got {other:?}"),
    }
}

#[tokio::test]
async fn our_own_broadcast_coming_straight_back_is_not_shown_twice() {
    // Reported from the phone: every send appeared twice, once labelled
    // "broadcast" and once "sent". The second row is our own packet returning
    // — flooded or synced back — carrying a different derived wire ID, so
    // nothing collapsed the two, and with no delivery state it fell through to
    // the one-to-one word "sent" beside its own correct copy.
    let (mut runtime, radio, state) = runtime();
    let local = runtime.identity.peer_id();
    let link_out = LinkId::new("/mock/out");
    state.with(|inner| {
        inner.links.link_up(link_out.clone(), LinkRole::Central);
        inner.links.set_writable(&link_out, true);
        inner.max_write.insert(link_out.clone(), 512);
    });
    let mut events = runtime.events.subscribe();

    runtime.broadcast_public("MSG-1", "said once").await;

    runtime.drain_writes().await;
    let packet = codec::decode(&radio.writes()[0].1).expect("decodable");
    // The local mirror: the copy the user is already looking at.
    assert!(matches!(events.try_recv(), Ok(MeshEvent::Message(_))));

    runtime.on_public_message(&packet, &local);

    assert!(
        events.try_recv().is_err(),
        "our own broadcast returning must not produce a second row"
    );
}

#[tokio::test]
async fn our_own_announce_coming_back_does_not_register_us_as_a_peer() {
    // The ingress guard admits a self-authored TTL-0 sync response on purpose,
    // so our own announce really does reach `on_announce`. If it registers, we
    // become our own peer: "N nearby" counts us, and a link binds to our own
    // identity. bitchat refuses it as `.reject(.selfAnnounce)`.
    let (mut runtime, radio, state) = runtime();
    let local = runtime.identity.peer_id();
    let link_out = LinkId::new("/mock/out");
    state.with(|inner| {
        inner.links.link_up(link_out.clone(), LinkRole::Central);
        inner.links.set_writable(&link_out, true);
        inner.max_write.insert(link_out.clone(), 512);
    });

    // Build the announce exactly as we would send it, then feed it back.
    runtime.announce().await;
    runtime.drain_writes().await;
    let sent = radio.writes();
    let packet = codec::decode(&sent[0].1).expect("decodable");
    assert_eq!(
        PeerId::from_bytes(&packet.sender_id),
        local,
        "the fixture must really be our own announce"
    );

    let link = LinkId::new("/mock/self");
    runtime.on_announce(&link, &packet, &local).await;

    assert!(
        state.with(|inner| inner.peers.info(&local).is_none()),
        "we must never appear in our own peer registry"
    );
    assert!(
        state.with(|inner| inner.links.peer_for(&link).is_none()),
        "and no link may be bound to our own identity"
    );
}

#[tokio::test]
async fn a_message_of_ours_recovered_from_a_peer_is_attributed_to_us() {
    // Gossip sync hands back what we wrote — that is how a device with no
    // database gets its own history again. It must land as ours: verified
    // against our own key (we are deliberately not in our own registry) and
    // marked local, or our own words appear on screen as a stranger's.
    let (mut runtime, _radio, _state) = runtime();
    let local = runtime.identity.peer_id();
    let link_out = LinkId::new("/mock/out");
    _state.with(|inner| {
        inner.links.link_up(link_out.clone(), LinkRole::Central);
        inner.links.set_writable(&link_out, true);
        inner.max_write.insert(link_out.clone(), 512);
    });
    let mut events = runtime.events.subscribe();

    // Built directly rather than through `broadcast_public`, because that
    // records the packet in the self-broadcast tracker — which is precisely
    // what marks a message as "already shown" and suppresses it. Recovery is
    // the opposite case: a packet we authored in some *earlier* session, whose
    // tracker entry died with the process, arriving from a peer's archive.
    let packet = runtime.maybe_sign(
        runtime.build_packet(
            MessageType::Message,
            None,
            b"our own words".to_vec(),
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        ),
        true,
    );
    assert!(
        !_state.with(|inner| inner.self_broadcasts.contains(&packet)),
        "the fixture must model a packet this session never sent"
    );

    runtime.on_public_message(&packet, &local);

    match events.try_recv() {
        Ok(MeshEvent::Message(message)) => {
            assert!(message.is_local, "a recovered self message is ours");
            assert_eq!(message.sender, local);
            assert_eq!(
                super::super::mapping::text_of(&message.message),
                Some("our own words")
            );
        }
        other => panic!("expected the recovered message, got {other:?}"),
    }
}

#[tokio::test]
async fn our_own_broadcasts_are_in_our_sync_filter_so_a_peer_does_not_echo_them() {
    // Seen on devices: bitchat-android answered every one of our sync
    // rounds with our own public messages. A sent broadcast never reached
    // the gossip store, so our filter claimed we lacked it; and bitchat's
    // answer carries no RSR flag, so the ingress guard drops it as our own
    // traffic before anything can store it. Nothing ever closed the loop.
    // bitchat files its own sends at send time
    // (`BluetoothMeshService.kt`, `onPublicPacketSeen(signedPacket)`).
    let (mut runtime, radio, state) = runtime();
    let local = runtime.identity.peer_id();
    let link = LinkId::new("/mock/out");
    state.with(|inner| {
        inner.links.link_up(link.clone(), LinkRole::Central);
        inner.links.set_writable(&link, true);
        inner.max_write.insert(link.clone(), 512);
    });

    runtime.broadcast_public("MSG-1", "said once").await;
    runtime.broadcast_file("MSG-F1", &sample_file()).await;
    runtime.drain_writes().await;
    let sent: Vec<BitchatPacket> = radio
        .writes()
        .iter()
        .map(|(_, bytes)| codec::decode(bytes).expect("decodable"))
        .collect();
    assert_eq!(sent.len(), 2, "one message and one single-packet file");

    // The peer heard both live, as a real neighbour does.
    let now = now_secs();
    let peer = MeshIdentity::generate(&mut os_rng()).peer_id();
    let mut theirs = GossipSyncManager::new(&peer, SyncConfig::default(), None, now);
    for packet in &sent {
        theirs.on_public_packet_seen(packet, now);
    }

    // One full sync round from us to them.
    let request = runtime.gossip.initial_sync_to(&peer, now).expect("request");
    let request = RequestSyncPacket::decode(&request.packet.payload).expect("decodes");
    let answers = theirs.handle_request_sync(&local, &request, now);

    let echoed: Vec<_> = answers
        .iter()
        .filter(|send| {
            sent.iter()
                .any(|p| p.packet_id() == send.packet.packet_id())
        })
        .collect();
    assert!(
        echoed.is_empty(),
        "a peer must not send our own broadcasts back to us: {echoed:?}"
    );
}

#[tokio::test]
async fn a_recovered_message_of_ours_synced_twice_is_shown_once() {
    // A self-authored TTL-0 sync response skips both the ingress self
    // checks and the dedup cache — that is what lets a device that lost its
    // database recover its own history. It must still be recovered *once*:
    // two peers answering the same round, or one peer answering two rounds,
    // must not put the same words on screen twice.
    let (mut runtime, _radio, state) = runtime();
    let mut events = runtime.events.subscribe();

    // A peer that announced on its own link, so the link is bound to it and
    // a response arriving there can be attributed to our request.
    let subject = MeshIdentity::generate(&mut os_rng());
    let subject_peer = subject.peer_id();
    let announcement = AnnouncementPacket::new(
        "subject".to_string(),
        subject.static_public().to_vec(),
        subject.signing_public().to_vec(),
        None,
        None,
        None,
    );
    let mut announce = BitchatPacket::new(
        MessageType::Announce,
        subject_peer.to_routing_bytes().expect("routing bytes"),
        None,
        now_millis(),
        announcement.encode().expect("encodes"),
        chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
    );
    announce.signature = Some(sign_packet(&subject, &announce).expect("signs"));
    let link = LinkId::new("/mock/subject");
    state.with(|inner| {
        inner.links.link_up(link.clone(), LinkRole::Central);
        inner.links.set_writable(&link, true);
        inner.max_write.insert(link.clone(), 512);
    });
    runtime.on_packet(&link, announce).await;
    runtime.drain_writes().await;
    while events.try_recv().is_ok() {}

    // Ours, from an earlier session: signed by us, unknown to this
    // session's self-broadcast tracker.
    let mut packet = runtime.maybe_sign(
        runtime.build_packet(
            MessageType::Message,
            None,
            b"our own words".to_vec(),
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        ),
        true,
    );
    packet.ttl = 0;
    packet.is_rsr = true;

    runtime
        .gossip
        .initial_sync_to(&subject_peer, now_secs())
        .expect("request");
    runtime.on_packet(&link, packet.clone()).await;
    runtime.on_packet(&link, packet).await;

    let mut shown = 0;
    while let Ok(event) = events.try_recv() {
        if matches!(event, MeshEvent::Message(_)) {
            shown += 1;
        }
    }
    assert_eq!(shown, 1, "recovered once, shown once");
}

/// A bitchat-android peer that announced on its own link. Its announce opens
/// our initial sync request to it, so its answers are awaited.
async fn android_peer_on_its_link(
    runtime: &mut BleRuntime,
    state: &MeshState,
) -> (MeshIdentity, LinkId) {
    let peer = MeshIdentity::generate(&mut os_rng());
    let announcement = AnnouncementPacket::new(
        "android".to_string(),
        peer.static_public().to_vec(),
        peer.signing_public().to_vec(),
        None,
        None,
        None,
    );
    let mut announce = BitchatPacket::new(
        MessageType::Announce,
        peer.peer_id().to_routing_bytes().expect("routing bytes"),
        None,
        now_millis(),
        announcement.encode().expect("encodes"),
        chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
    );
    announce.signature = Some(sign_packet(&peer, &announce).expect("signs"));
    let link = writable_link(state, "/mock/android", 512);
    runtime.on_packet(&link, announce).await;
    runtime.drain_writes().await;
    assert!(
        runtime
            .gossip
            .awaits_answer_from(&peer.peer_id(), now_secs())
    );
    (peer, link)
}

/// A public message the way bitchat-android answers a sync round with it:
/// signed by its author, TTL 0, no RSR flag.
fn android_sync_answer(author: &MeshIdentity, text: &[u8], timestamp: u64) -> BitchatPacket {
    let mut packet = BitchatPacket::new(
        MessageType::Message,
        author.peer_id().to_routing_bytes().expect("routing bytes"),
        None,
        timestamp,
        text.to_vec(),
        0,
    );
    packet.signature = Some(sign_packet(author, &packet).expect("signs"));
    packet
}

fn messages_shown(events: &mut tokio::sync::broadcast::Receiver<MeshEvent>) -> usize {
    let mut shown = 0;
    while let Ok(event) = events.try_recv() {
        if matches!(event, MeshEvent::Message(_)) {
            shown += 1;
        }
    }
    shown
}

/// Whether any frame on the radio carries this packet, after the relay
/// jitter (220 ms at most) has passed.
async fn put_on_air(runtime: &BleRuntime, radio: &MockLink, packet: &BitchatPacket) -> bool {
    tokio::time::sleep(Duration::from_millis(400)).await;
    runtime.drain_writes().await;
    radio.writes().iter().any(|(_, bytes)| {
        codec::decode(bytes).is_ok_and(|sent| sent.packet_id() == packet.packet_id())
    })
}

#[tokio::test]
async fn a_stale_android_sync_answer_is_accepted_once_and_never_relayed() {
    // bitchat-android answers sync with TTL 0 and no RSR flag. A message
    // older than the two-minute skew window failed the freshness check, was
    // never filed, and came back every round.
    let (mut runtime, radio, state) = runtime();
    let (android, link) = android_peer_on_its_link(&mut runtime, &state).await;
    // Somewhere a relay could go.
    writable_link(&state, "/mock/bystander", 512);
    let mut events = runtime.events.subscribe();

    let stale = android_sync_answer(&android, b"from ten minutes ago", now_millis() - 600_000);
    runtime.on_packet(&link, stale.clone()).await;
    assert_eq!(messages_shown(&mut events), 1, "accepted");
    assert!(
        runtime.gossip.holds(&stale),
        "filed, so our filter has it next round"
    );

    // Replays: the dedup cache, and past it the store.
    runtime.on_packet(&link, stale.clone()).await;
    state.with(|inner| inner.dedup.reset());
    runtime.on_packet(&link, stale.clone()).await;
    assert_eq!(messages_shown(&mut events), 0, "a replay is dropped");

    assert!(!put_on_air(&runtime, &radio, &stale).await, "never relayed");

    // Control: the same author's live message is relayed, so the check
    // above can see a relay.
    let mut live = android_sync_answer(&android, b"live", now_millis());
    live.ttl = chatter_mesh::relay::MESSAGE_TTL_DEFAULT;
    live.signature = Some(sign_packet(&android, &live).expect("signs"));
    runtime.on_packet(&link, live.clone()).await;
    assert!(
        put_on_air(&runtime, &radio, &live).await,
        "the control relays"
    );
}

#[tokio::test]
async fn a_stale_android_sync_answer_past_our_retention_is_dropped() {
    let (mut runtime, _radio, state) = runtime();
    let (android, link) = android_peer_on_its_link(&mut runtime, &state).await;
    let mut events = runtime.events.subscribe();

    let retention_ms = SyncConfig::default().public_message_max_age_secs * 1000;
    let ancient = android_sync_answer(&android, b"too old", now_millis() - retention_ms - 60_000);
    runtime.on_packet(&link, ancient.clone()).await;

    assert_eq!(messages_shown(&mut events), 0);
    assert!(!runtime.gossip.holds(&ancient));
}

#[tokio::test]
async fn a_stale_sync_answer_on_an_unverified_link_is_dropped() {
    let (mut runtime, _radio, state) = runtime();
    let (android, _link) = android_peer_on_its_link(&mut runtime, &state).await;
    let mut events = runtime.events.subscribe();
    let stale = android_sync_answer(&android, b"old", now_millis() - 600_000);

    // A link no announce has bound.
    let unbound = writable_link(&state, "/mock/unbound", 512);
    runtime.on_packet(&unbound, stale.clone()).await;

    // A link bound to a peer with no pinned announce, even with a request
    // of ours out to it.
    let unpinned = MeshIdentity::generate(&mut os_rng()).peer_id();
    let bound = writable_link(&state, "/mock/unpinned", 512);
    state.with(|inner| inner.links.bind_if_unbound(&bound, &unpinned));
    runtime
        .gossip
        .initial_sync_to(&unpinned, now_secs())
        .expect("request");
    runtime.on_packet(&bound, stale.clone()).await;

    assert_eq!(messages_shown(&mut events), 0);
    assert!(!runtime.gossip.holds(&stale));
}

#[tokio::test]
async fn a_stale_sync_answer_with_a_bad_signature_is_dropped() {
    let (mut runtime, _radio, state) = runtime();
    let (android, link) = android_peer_on_its_link(&mut runtime, &state).await;
    let mut events = runtime.events.subscribe();

    let mut forged = android_sync_answer(&android, b"old", now_millis() - 600_000);
    let other = MeshIdentity::generate(&mut os_rng());
    forged.signature = Some(sign_packet(&other, &forged).expect("signs"));
    runtime.on_packet(&link, forged.clone()).await;

    assert_eq!(messages_shown(&mut events), 0);
    assert!(
        !runtime.gossip.holds(&forged),
        "not filed, so not served onward"
    );
}

#[tokio::test]
async fn an_announce_is_signed_and_carries_both_public_keys() {
    let (mut runtime, radio, state) = runtime();
    // One writable link, so the announce has somewhere to go.
    let link = LinkId::new("/mock/one");
    state.with(|inner| {
        inner.links.link_up(link.clone(), LinkRole::Central);
        inner.links.set_writable(&link, true);
        inner.max_write.insert(link.clone(), 512);
    });

    runtime.announce().await;

    runtime.drain_writes().await;

    let writes = radio.writes();
    assert_eq!(writes.len(), 1, "the announce went out on the one link");
    let packet = codec::decode(&writes[0].1).expect("decodable");
    assert_eq!(packet.message_type(), Some(MessageType::Announce));

    let announcement =
        AnnouncementPacket::decode(&packet.payload).expect("announce payload decodes");
    assert_eq!(
        announcement.noise_public_key,
        runtime.identity.static_public().to_vec()
    );
    assert_eq!(
        announcement.signing_public_key,
        runtime.identity.signing_public().to_vec()
    );

    assert!(packet.signature.is_some(), "announces are signed");
    let signing_key =
        <[u8; 32]>::try_from(announcement.signing_public_key.as_slice()).expect("32 bytes");
    assert!(
        verify_packet(&packet, &signing_key),
        "the signature must verify against the advertised key, over the \
             packet preimage — a peer verifying `canonicalAnnounceBytes` \
             instead never enters anyone's registry"
    );
    assert!(
        packet.recipient_id.is_none(),
        "an announce is addressed to everyone by omitting the recipient"
    );
}
