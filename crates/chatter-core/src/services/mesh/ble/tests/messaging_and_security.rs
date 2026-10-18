
#[tokio::test]
async fn announces_from_any_cause_are_floored_at_one_second() {
    let (mut runtime, radio, state) = runtime();
    let link = LinkId::new("/mock/one");
    state.with(|inner| {
        inner.links.link_up(link.clone(), LinkRole::Central);
        inner.links.set_writable(&link, true);
        inner.max_write.insert(link.clone(), 512);
    });

    // A connect storm or a repeated "announce now" inside the floor: one goes out.
    runtime.announce().await;
    runtime.announce().await;
    runtime.announce().await;
    runtime.drain_writes().await;
    assert_eq!(
        radio.writes().len(),
        1,
        "announces inside the floor are held"
    );

    // Still inside the floor: the deferred one waits.
    runtime.flush_deferred_announce(now_millis()).await;
    runtime.drain_writes().await;
    assert_eq!(radio.writes().len(), 1);

    // At expiry the held requests go out as exactly one announce, so a peer
    // that connected inside the floor still hears us.
    let rewind = |state: &MeshState| {
        state.with(|inner| {
            inner.last_announce_ms = inner
                .last_announce_ms
                .saturating_sub(chatter_mesh_link::duty::ANNOUNCE_MIN_INTERVAL_MS);
        });
    };
    rewind(&state);
    runtime.flush_deferred_announce(now_millis()).await;
    runtime.flush_deferred_announce(now_millis()).await;
    runtime.drain_writes().await;
    assert_eq!(radio.writes().len(), 2, "one deferred announce, coalesced");

    // Nothing deferred: expiry alone sends nothing.
    rewind(&state);
    runtime.flush_deferred_announce(now_millis()).await;
    runtime.drain_writes().await;
    assert_eq!(radio.writes().len(), 2);

    // Once the floor has passed, a direct request goes out.
    runtime.announce().await;
    runtime.drain_writes().await;
    assert_eq!(radio.writes().len(), 3);
}

#[tokio::test]
async fn nothing_is_written_when_no_link_is_writable() {
    let (mut runtime, radio, state) = runtime();
    let link = LinkId::new("/mock/one");
    // Bound and up, but not writable.
    state.with(|inner| {
        inner.links.link_up(link.clone(), LinkRole::Central);
        inner.max_write.insert(link.clone(), 512);
    });
    runtime.announce().await;
    runtime.drain_writes().await;
    assert_eq!(radio.write_count(), 0);
}

#[tokio::test]
async fn a_public_message_is_mirrored_locally_and_recorded_as_our_own() {
    let (mut runtime, radio, state) = runtime();
    let link = LinkId::new("/mock/one");
    state.with(|inner| {
        inner.links.link_up(link.clone(), LinkRole::Central);
        inner.links.set_writable(&link, true);
        inner.max_write.insert(link.clone(), 512);
    });
    let mut events = runtime.events.subscribe();

    runtime.broadcast_public("MSG-1", "hello room").await;

    runtime.drain_writes().await;

    let writes = radio.writes();
    assert_eq!(writes.len(), 1);
    let packet = codec::decode(&writes[0].1).expect("decodable");
    assert_eq!(packet.message_type(), Some(MessageType::Message));
    assert!(packet.is_broadcast());
    // The three things a real bitchat checks before it will show this, and
    // the three the port originally got wrong: the payload is the message
    // text and nothing else, the packet is signed, and "everyone" is encoded
    // by omitting the recipient.
    assert_eq!(packet.payload, b"hello room");
    assert!(packet.recipient_id.is_none());
    let signing_key = runtime.identity.signing_public();
    assert!(
        verify_packet(&packet, &signing_key),
        "an unsigned public message is dropped before its payload is read"
    );

    // The echo of our own broadcast must be recognisable.
    assert!(state.with(|inner| inner.self_broadcasts.contains(&packet)));
    assert_eq!(
        state.with(|inner| inner
            .self_broadcasts
            .message_id(&packet)
            .map(str::to_string)),
        Some("MSG-1".to_string())
    );

    match events.try_recv() {
        Ok(MeshEvent::Message(message)) => {
            assert!(message.is_local);
            assert_eq!(message.conversation, MeshConversation::Room);
            assert_eq!(
                message.message.conversation_type,
                crate::domain::message::ConversationType::Mesh
            );
        }
        other => panic!("expected a mirrored message, got {other:?}"),
    }
}

#[tokio::test]
async fn a_write_rejection_buffers_rather_than_dropping_the_packet() {
    let (mut runtime, radio, state) = runtime();
    let link = LinkId::new("/mock/one");
    state.with(|inner| {
        inner.links.link_up(link.clone(), LinkRole::Central);
        inner.links.set_writable(&link, true);
        inner.max_write.insert(link.clone(), 512);
    });
    radio.set_reject_writes(true);

    runtime.broadcast_public("MSG-1", "hello room").await;

    runtime.drain_writes().await;

    assert_eq!(radio.write_count(), 0, "the controller refused the write");
    assert_eq!(
        state.with(|inner| inner.writes.queued_count(&link)),
        1,
        "a rejected write is retryable and must not be discarded"
    );

    // Once the controller drains, the buffered write goes out.
    radio.set_reject_writes(false);
    runtime.flush_buffered_writes().await;
    assert_eq!(radio.write_count(), 1);
    assert_eq!(state.with(|inner| inner.writes.queued_count(&link)), 0);
}

#[tokio::test]
async fn a_connected_link_is_treated_as_writable_immediately() {
    let (mut runtime, _radio, state) = runtime();
    let link = LinkId::new("/mock/one");
    runtime
        .on_link_event(LinkEvent::Connected {
            link: link.clone(),
            role: LinkRole::Peripheral,
            max_write: 244,
        })
        .await;
    assert!(state.with(|inner| inner.links.is_writable(&link)));
    assert_eq!(
        state.with(|inner| inner.max_write.get(&link).copied()),
        Some(244)
    );
}

#[tokio::test]
async fn a_disconnect_drops_every_piece_of_link_state() {
    let (mut runtime, _radio, state) = runtime();
    let link = LinkId::new("/mock/one");
    runtime
        .on_link_event(LinkEvent::Connected {
            link: link.clone(),
            role: LinkRole::Central,
            max_write: 244,
        })
        .await;
    runtime
        .on_link_event(LinkEvent::Disconnected { link: link.clone() })
        .await;
    assert!(!state.with(|inner| inner.links.contains(&link)));
    assert!(state.with(|inner| inner.max_write.get(&link).is_none()));
    assert!(state.link_candidates().is_empty());
}

#[tokio::test]
async fn losing_the_adapter_clears_every_link() {
    let (mut runtime, _radio, state) = runtime();
    let link = LinkId::new("/mock/one");
    runtime
        .on_link_event(LinkEvent::Connected {
            link,
            role: LinkRole::Central,
            max_write: 244,
        })
        .await;
    runtime
        .on_link_event(LinkEvent::AdapterPowerChanged { powered: false })
        .await;
    assert!(state.with(|inner| inner.links.is_empty()));
    assert!(!state.with(|inner| inner.adapter_powered));
    assert!(state.link_candidates().is_empty());
}

#[tokio::test]
async fn a_private_message_with_no_session_is_held_and_a_handshake_starts() {
    let (mut runtime, radio, state) = runtime();
    let link = LinkId::new("/mock/one");
    let peer = PeerId::from_str("bbbbbbbbbbbbbbbb");
    state.with(|inner| {
        inner.links.link_up(link.clone(), LinkRole::Central);
        inner.links.bind_if_unbound(&link, &peer);
        inner.links.set_writable(&link, true);
        inner.max_write.insert(link.clone(), 512);
    });

    runtime.send_private(&peer, "MSG-1", "secret").await;

    runtime.drain_writes().await;

    // The message is held, not sent in the clear.
    let held = state.with(|inner| inner.pending_dms.get(&peer).map(Vec::len));
    assert_eq!(held, Some(1));
    let writes = radio.writes();
    assert_eq!(writes.len(), 1, "only the handshake went out");
    let packet = codec::decode(&writes[0].1).expect("decodable");
    assert_eq!(packet.message_type(), Some(MessageType::NoiseHandshake));
    assert!(
        !writes[0].1.windows(6).any(|w| w == b"secret"),
        "plaintext must never reach the wire"
    );
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn a_send_past_the_rekey_threshold_opens_one_renegotiation() {
    use chatter_mesh_noise::session::REKEY_MESSAGE_THRESHOLD;

    let (mut runtime, radio, state) = runtime();
    let local_peer = runtime.identity.peer_id();
    let peer_identity = MeshIdentity::generate(&mut os_rng());
    let peer = peer_identity.peer_id();
    let link = LinkId::new("/mock/rekey");
    state.with(|inner| {
        inner.links.link_up(link.clone(), LinkRole::Central);
        inner.links.bind_if_unbound(&link, &peer);
        inner.links.set_writable(&link, true);
        inner.max_write.insert(link.clone(), 512);
    });

    // Establish a session: the peer initiates, we answer.
    let mut remote = super::super::session::SessionManager::new();
    let m1 = remote.initiate(&local_peer, &peer_identity).expect("m1");
    let identity = Arc::clone(&runtime.identity);
    let m2 = state
        .with(|inner| {
            inner
                .sessions
                .on_handshake(&peer, &local_peer, &identity, &m1)
        })
        .expect("we read m1")
        .reply
        .expect("m2");
    let m3 = remote
        .on_handshake(&local_peer, &peer, &peer_identity, &m2)
        .expect("peer reads m2")
        .reply
        .expect("m3");
    state
        .with(|inner| {
            inner
                .sessions
                .on_handshake(&peer, &local_peer, &identity, &m3)
        })
        .expect("we complete");
    assert!(state.has_session(&peer));

    // Below the threshold a send is just a send.
    runtime.send_private(&peer, "MSG-1", "hello").await;
    runtime.drain_writes().await;
    let types = |radio: &MockLink| -> Vec<Option<MessageType>> {
        radio
            .writes()
            .iter()
            .map(|(_, bytes)| codec::decode(bytes).expect("decodable").message_type())
            .collect()
    };
    assert_eq!(types(&radio), [Some(MessageType::NoiseEncrypted)]);

    // At the threshold the message still goes out on the old session, and
    // one handshake follows it; a second send does not restart it.
    state.with(|inner| {
        inner
            .sessions
            .set_messages_sent_for_testing(&peer, REKEY_MESSAGE_THRESHOLD)
    });
    runtime.send_private(&peer, "MSG-2", "again").await;
    runtime.send_private(&peer, "MSG-3", "and again").await;
    runtime.drain_writes().await;
    assert_eq!(
        types(&radio),
        [
            Some(MessageType::NoiseEncrypted),
            Some(MessageType::NoiseEncrypted),
            Some(MessageType::NoiseHandshake),
            Some(MessageType::NoiseEncrypted),
        ]
    );
    assert!(
        state.has_session(&peer),
        "the old session stays until replaced"
    );
    assert!(state.with(|inner| inner.sessions.is_handshaking(&peer)));
}

/// Courier mail is released only on a link that proved the recipient's key
/// itself. A peer-level session used to suffice, which is exactly the state a
/// replayed announce rebinding an attacker's link produces.
#[tokio::test]
async fn courier_mail_waits_for_a_handshake_on_the_announcing_link() {
    use chatter_mesh::tlv::courier_envelope::{epoch_day, recipient_tag};

    let (mut runtime, radio, state) = runtime();
    let local_peer = runtime.identity.peer_id();
    let peer_identity = MeshIdentity::generate(&mut os_rng());
    let peer = peer_identity.peer_id();
    let link = LinkId::new("/mock/courier");
    state.with(|inner| {
        inner.links.link_up(link.clone(), LinkRole::Central);
        inner.links.set_writable(&link, true);
        inner.max_write.insert(link.clone(), 512);
    });

    // We carry one envelope for the peer.
    let now_s = now_millis() / 1000;
    let recipient = peer_identity.static_public();
    let envelope = CourierEnvelope::new(
        recipient_tag(&recipient, epoch_day(now_s)),
        (now_s + 3600) * 1000,
        vec![0x5A; 96],
        1,
        None,
    );
    runtime
        .couriers
        .deposit(&envelope, &[0xA1; 32], now_s)
        .expect("deposit");

    // A peer-level session exists, but it was not made on this link.
    let mut remote = super::super::session::SessionManager::new();
    let identity = Arc::clone(&runtime.identity);
    let m1 = remote.initiate(&local_peer, &peer_identity).expect("m1");
    let m2 = state
        .with(|inner| {
            inner
                .sessions
                .on_handshake(&peer, &local_peer, &identity, &m1)
        })
        .expect("m1 read")
        .reply
        .expect("m2");
    let m3 = remote
        .on_handshake(&local_peer, &peer, &peer_identity, &m2)
        .expect("m2 read")
        .reply
        .expect("m3");
    state
        .with(|inner| {
            inner
                .sessions
                .on_handshake(&peer, &local_peer, &identity, &m3)
        })
        .expect("established");

    let announce = |timestamp: u64| {
        let announcement = AnnouncementPacket::new(
            "recipient".to_string(),
            peer_identity.static_public().to_vec(),
            peer_identity.signing_public().to_vec(),
            None,
            None,
            None,
        );
        let mut packet = BitchatPacket::new(
            MessageType::Announce,
            peer.to_routing_bytes().expect("routing bytes"),
            None,
            timestamp,
            announcement.encode().expect("encodes"),
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        packet.signature = Some(sign_packet(&peer_identity, &packet).expect("signs"));
        packet
    };
    let sent = |radio: &MockLink, kind: MessageType| -> Vec<BitchatPacket> {
        radio
            .writes()
            .iter()
            .map(|(_, bytes)| codec::decode(bytes).expect("decodable"))
            .filter(|packet| packet.message_type() == Some(kind))
            .collect()
    };

    // A direct, verified announce: no mail yet, but a handshake to prove it.
    assert!(
        runtime
            .on_announce(&link, &announce(now_millis()), &peer)
            .await
    );
    runtime.drain_writes().await;
    assert!(sent(&radio, MessageType::CourierEnvelope).is_empty());
    assert!(runtime.couriers.carries_for(&recipient, now_s));
    let opening = sent(&radio, MessageType::NoiseHandshake);
    assert_eq!(opening.len(), 1, "one handshake opened to prove the link");

    // The peer answers on this link; completing the handshake marks it.
    let mut remote = super::super::session::SessionManager::new();
    let m2 = remote
        .on_handshake(&local_peer, &peer, &peer_identity, &opening[0].payload)
        .expect("peer reads m1")
        .reply
        .expect("m2");
    let reply = BitchatPacket::new(
        MessageType::NoiseHandshake,
        peer.to_routing_bytes().expect("routing bytes"),
        local_peer.to_routing_bytes(),
        now_millis(),
        m2,
        chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
    );
    runtime.on_handshake(&link, &reply, &peer).await;
    assert!(state.with(|inner| inner.links.is_noise_authenticated(&link, &peer)));

    // The next announce on the proved link releases the mail.
    assert!(
        runtime
            .on_announce(&link, &announce(now_millis() + 1), &peer)
            .await
    );
    runtime.drain_writes().await;
    assert_eq!(sent(&radio, MessageType::CourierEnvelope).len(), 1);
    assert!(!runtime.couriers.carries_for(&recipient, now_s));
}

#[tokio::test]
async fn our_prekey_bundle_is_saved_before_it_is_published() {
    let (mut runtime, radio, state) = runtime();
    let link = LinkId::new("/mock/prekeys");
    state.with(|inner| {
        inner.links.link_up(link.clone(), LinkRole::Central);
        inner.links.set_writable(&link, true);
        inner.max_write.insert(link.clone(), 512);
    });

    // Not restored: nothing is generated, nothing published.
    runtime.maintain_prekeys(now_millis()).await;
    assert!(
        runtime
            .storage
            .load(StorageKey::LocalPrekeys)
            .unwrap()
            .is_none()
    );

    state.with(|inner| inner.prekeys.local = Some(super::super::prekeys::LocalPrekeys::default()));
    runtime.maintain_prekeys(now_millis()).await;
    runtime.drain_writes().await;

    let saved = runtime
        .storage
        .load(StorageKey::LocalPrekeys)
        .unwrap()
        .expect("saved");
    assert!(super::super::prekeys::LocalPrekeys::from_bytes(&saved).is_some());
    let published: Vec<BitchatPacket> = radio
        .writes()
        .iter()
        .map(|(_, bytes)| codec::decode(bytes).expect("decodable"))
        .filter(|packet| packet.message_type() == Some(MessageType::PrekeyBundle))
        .collect();
    assert_eq!(published.len(), 1);
    let bundle = PrekeyBundle::decode(&published[0].payload).expect("bundle");
    assert_eq!(bundle.owner_static, runtime.identity.static_public());
    assert!(bundle.verify(&runtime.identity.signing_public()));
    assert!(
        runtime
            .gossip
            .has_prekey_bundle(&runtime.identity.peer_id())
    );

    // Unchanged on the next tick: no second publish.
    runtime.maintain_prekeys(now_millis()).await;
    runtime.drain_writes().await;
    assert_eq!(radio.writes().len(), 1);
}

#[tokio::test]
async fn a_peer_bundle_is_kept_only_when_its_pinned_key_signed_it() {
    let (mut runtime, _radio, state) = runtime();
    let owner = MeshIdentity::generate(&mut os_rng());
    let announcement = AnnouncementPacket::new(
        "owner".to_string(),
        owner.static_public().to_vec(),
        owner.signing_public().to_vec(),
        None,
        None,
        None,
    );
    state.with(|inner| {
        inner
            .peers
            .upsert_verified_announce(&owner.peer_id(), &announcement, true, now_millis());
    });

    let mut theirs = super::super::prekeys::LocalPrekeys::default();
    theirs.refresh(now_millis(), &mut os_rng());
    let genuine = theirs.bundle(&owner).expect("bundle");
    let bundle_packet = |bundle: &PrekeyBundle| {
        BitchatPacket::new(
            MessageType::PrekeyBundle,
            owner.peer_id().to_routing_bytes().expect("routing bytes"),
            None,
            now_millis(),
            bundle.encode().expect("encodes"),
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        )
    };

    // Signed by someone else over the owner's static key: refused.
    let forger = MeshIdentity::generate(&mut os_rng());
    let forged = PrekeyBundle::sign(
        owner.static_public(),
        genuine.prekeys.clone(),
        genuine.generated_at,
        &forger,
    );
    assert!(!runtime.on_prekey_bundle(&bundle_packet(&forged)));
    assert!(
        state
            .with(|inner| inner
                .prekeys
                .peers
                .take(&owner.static_public(), now_millis()))
            .is_none()
    );

    // The genuine one is cached, so offline mail to the owner seals to it.
    assert!(runtime.on_prekey_bundle(&bundle_packet(&genuine)));
    let (_, prekey_id) = super::super::prekeys::seal_offline(
        &state,
        b"payload",
        &owner.static_public(),
        &runtime.identity,
        now_millis(),
    )
    .expect("sealed");
    assert!(prekey_id.is_some_and(|id| genuine.prekeys.iter().any(|p| p.id == id)));
}

#[tokio::test]
async fn a_courier_envelope_sealed_to_our_prekey_opens_and_consumes_it() {
    let (mut runtime, _radio, state) = runtime();
    let mut events = runtime.events.subscribe();
    state.with(|inner| {
        let mut local = super::super::prekeys::LocalPrekeys::default();
        local.refresh(now_millis(), &mut os_rng());
        inner.prekeys.local = Some(local);
    });
    let bundle = state
        .with(|inner| {
            inner
                .prekeys
                .local
                .as_ref()
                .unwrap()
                .bundle(&runtime.identity)
        })
        .expect("bundle");

    let sender = MeshIdentity::generate(&mut os_rng());
    let peer_state = MeshState::new(sender.peer_id());
    peer_state.with(|inner| inner.prekeys.peers.ingest(bundle.clone(), now_millis()));
    let inner = PrivateMessagePacket::new("PK-1".to_string(), "forward secret".to_string())
        .encode()
        .expect("encodes");
    let payload = encode_noise_payload(NoisePayloadType::PrivateMessage, &inner);
    let (ciphertext, prekey_id) = super::super::prekeys::seal_offline(
        &peer_state,
        &payload,
        &runtime.identity.static_public(),
        &sender,
        now_millis(),
    )
    .expect("sealed");
    assert!(prekey_id.is_some());
    let now_s = now_secs();
    let envelope = CourierEnvelope::new(
        chatter_mesh::tlv::courier_envelope::recipient_tag(
            &runtime.identity.static_public(),
            chatter_mesh::tlv::courier_envelope::epoch_day(now_s),
        ),
        (now_s + 3600) * 1000,
        ciphertext,
        1,
        prekey_id,
    );
    let packet = BitchatPacket::new(
        MessageType::CourierEnvelope,
        sender.peer_id().to_routing_bytes().expect("routing bytes"),
        runtime.identity.peer_id().to_routing_bytes(),
        now_millis(),
        envelope.encode().expect("encodes"),
        chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
    );
    runtime.on_courier_envelope(&packet, &sender.peer_id());

    match events.try_recv() {
        Ok(MeshEvent::Message(message)) => {
            assert_eq!(message.sender, sender.peer_id());
            assert_eq!(mapping::text_of(&message.message), Some("forward secret"));
        }
        other => panic!("expected the DM, got {other:?}"),
    }
    // Consumed: the next refresh changes the set (so it is saved and
    // republished under a newer stamp) and the bundle no longer offers it.
    assert!(state.with(|inner| {
        inner
            .prekeys
            .local
            .as_mut()
            .unwrap()
            .refresh(now_millis() + 1, &mut os_rng())
    }));
    let refreshed = state
        .with(|inner| {
            inner
                .prekeys
                .local
                .as_ref()
                .unwrap()
                .bundle(&runtime.identity)
        })
        .expect("bundle");
    assert!(refreshed.prekeys.iter().all(|p| Some(p.id) != prekey_id));
    assert!(refreshed.generated_at > bundle.generated_at);
}

#[tokio::test]
async fn a_file_from_a_verified_peer_is_stored_and_reachable_from_its_row() {
    // A file's row is only a pointer: unlike a text message, "it arrived"
    // and "its bytes are somewhere the user can open" are two claims, and
    // the second is the one that makes the feature exist.
    let (mut runtime, _radio, state, timeline) = runtime_parts();
    let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
    let identity = register_verified_peer(&state, &sender, "wanderer");
    let mut events = runtime.events.subscribe();

    let file = sample_file();
    let packet = file_packet(&sender, &identity, &file, None, true);
    runtime.on_file_transfer(&packet, &sender);

    let attachment = match events.try_recv() {
        Ok(MeshEvent::Message(message)) => {
            assert!(!message.is_local, "someone else's file is not ours");
            assert_eq!(message.conversation, MeshConversation::Room);
            assert!(
                message.sender_verified,
                "the signature verified, so the sender is verified"
            );
            assert_eq!(
                mapping::text_of(&message.message),
                None,
                "a file has no text body; `Mixed` with no text is the point"
            );
            let attachments = mapping::attachments_of(&message.message);
            assert_eq!(attachments.len(), 1, "one file, one attachment");
            attachments[0]
        }
        other => panic!("expected the file, got {other:?}"),
    };

    // The row survived the store, still naming the same attachment — the
    // event and the durable copy must not disagree about which bytes the
    // message is.
    let history = timeline
        .history(&MeshConversation::Room, None, 10)
        .expect("history");
    assert_eq!(history.len(), 1);
    assert_eq!(mapping::attachments_of(&history[0]), [attachment]);

    // And the bytes are really on disk, where the existing attachment read
    // path looks for them.
    let path = crate::services::attachment::attachment_file_path(&attachment);
    assert!(path.exists(), "the content has to be at {}", path.display());
    assert_ne!(
        std::fs::read(&path).expect("readable"),
        file.content,
        "a mesh file lands encrypted, like every other attachment — the \
             bytes sit outside SQLCipher, so storing them in the clear would \
             leave mesh files the one thing in this app lying open on disk"
    );
}

#[tokio::test]
async fn the_same_file_arriving_again_does_not_store_a_second_copy() {
    // The dedup set in `on_packet` catches the flood, but the gossip archive
    // exists to hand public packets back after that window closes. A text
    // message re-arriving is free — `record` is idempotent on the message ID.
    // A file is not: its bytes go to disk *before* the row, so storing first
    // and letting the row be ignored leaves an encrypted megabyte nothing
    // references, and shows the user an attachment the stored row never had.
    let (mut runtime, _radio, state, timeline) = runtime_parts();
    let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
    let identity = register_verified_peer(&state, &sender, "wanderer");
    let packet = file_packet(&sender, &identity, &sample_file(), None, true);

    runtime.on_file_transfer(&packet, &sender);
    let first = timeline
        .history(&MeshConversation::Room, None, 10)
        .expect("history");
    assert_eq!(first.len(), 1);
    let stored = mapping::attachments_of(&first[0]).to_vec();

    // The same packet, arriving by another path with the dedup entry gone.
    let mut events = runtime.events.subscribe();
    runtime.on_file_transfer(&packet, &sender);

    assert!(
        events.try_recv().is_err(),
        "the second arrival must produce no row and no event"
    );
    let after = timeline
        .history(&MeshConversation::Room, None, 10)
        .expect("history");
    assert_eq!(after.len(), 1);
    assert_eq!(
        mapping::attachments_of(&after[0]),
        stored.as_slice(),
        "still pointing at the copy that was stored the first time"
    );
}

#[tokio::test]
async fn an_unsigned_file_transfer_is_dropped() {
    // The security gate, and the reason this handler exists rather than a
    // `_ => {}`. A broadcast's `sender_id` is a header field the sender
    // picked, so without a signature that verifies against the key their
    // announce established, anybody in range publishes files under anybody's
    // name. bitchat names the concrete attack: a peer that overheard a public
    // voice burst spoofing a broadcast note under the talker's ID to
    // overwrite their signature-verified bubble with attacker audio.
    let (mut runtime, _radio, state, timeline) = runtime_parts();
    let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
    let identity = register_verified_peer(&state, &sender, "wanderer");
    let mut events = runtime.events.subscribe();

    let packet = file_packet(&sender, &identity, &sample_file(), None, false);
    runtime.on_file_transfer(&packet, &sender);

    assert!(
        events.try_recv().is_err(),
        "an unsigned file must not reach the room"
    );
    assert!(
        timeline
            .history(&MeshConversation::Room, None, 10)
            .expect("history")
            .is_empty(),
        "and nothing about it may reach disk"
    );
}

#[tokio::test]
async fn a_file_from_a_peer_that_never_announced_is_dropped() {
    // A signature is only worth something against a key we obtained
    // independently. With no announce there is nothing to check against, so
    // a self-signed packet would otherwise authenticate itself.
    let (mut runtime, _radio, _state, timeline) = runtime_parts();
    let stranger = PeerId::from_str("cccccccccccccccc");
    let identity = MeshIdentity::generate(&mut os_rng());

    let packet = file_packet(&stranger, &identity, &sample_file(), None, true);
    let mut events = runtime.events.subscribe();
    runtime.on_file_transfer(&packet, &stranger);

    assert!(
        events.try_recv().is_err(),
        "a validly-signed file from an unknown key is still unattributable"
    );
    assert!(
        timeline
            .history(&MeshConversation::Room, None, 10)
            .expect("history")
            .is_empty()
    );
}

#[tokio::test]
async fn our_own_file_coming_straight_back_is_not_stored_twice() {
    // Worse than the duplicate row a returning text message produces: a
    // second pass through `store_file` writes a *second encrypted copy of
    // the bytes* under a new attachment id, so every echo of a 1 MiB file
    // would cost another megabyte of disk.
    let (mut runtime, radio, state, timeline) = runtime_parts();
    let local = runtime.identity.peer_id();
    let link = LinkId::new("/mock/out");
    state.with(|inner| {
        inner.links.link_up(link.clone(), LinkRole::Central);
        inner.links.set_writable(&link, true);
        inner.max_write.insert(link.clone(), 512);
    });
    let mut events = runtime.events.subscribe();

    runtime.broadcast_file("MSG-F1", &sample_file()).await;

    runtime.drain_writes().await;
    let packet = codec::decode(&radio.writes()[0].1).expect("decodable");
    assert_eq!(packet.message_type(), Some(MessageType::FileTransfer));
    // The local mirror: the row the sender is already looking at.
    let sent = match events.try_recv() {
        Ok(MeshEvent::Message(message)) => {
            assert!(message.is_local);
            assert_eq!(
                message.message.delivery_status,
                MeshDelivery::Broadcast.as_str(),
                "nobody acknowledges a room file, so \"sent\" would be a \
                     one-to-one word for something with no recipient"
            );
            mapping::attachments_of(&message.message).to_vec()
        }
        other => panic!("expected the local mirror, got {other:?}"),
    };
    assert_eq!(sent.len(), 1);

    runtime.on_file_transfer(&packet, &local);

    assert!(
        events.try_recv().is_err(),
        "our own file returning must not produce a second row"
    );
    let history = timeline
        .history(&MeshConversation::Room, None, 10)
        .expect("history");
    assert_eq!(history.len(), 1, "one row");
    assert_eq!(
        mapping::attachments_of(&history[0]),
        sent.as_slice(),
        "and one copy of the bytes, still the first one"
    );
}

#[tokio::test]
async fn a_file_addressed_to_us_alone_is_not_accepted_in_the_clear() {
    // bitchat delivers a directed `FileTransfer` as a private message
    // (`BLEFileTransferPolicy.deliveryPlan`). This client will not: the
    // packet is unencrypted, and every mesh DM here is Noise-sealed, so
    // showing it in a conversation would teach the UI that private traffic
    // can arrive in the clear. `on_public_message` refuses the legacy
    // directed `Message` for the same reason.
    let (mut runtime, _radio, state, timeline) = runtime_parts();
    let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
    let identity = register_verified_peer(&state, &sender, "wanderer");
    let local = runtime.identity.peer_id();
    let mut events = runtime.events.subscribe();

    let packet = file_packet(&sender, &identity, &sample_file(), Some(&local), true);
    runtime.on_file_transfer(&packet, &sender);

    assert!(
        events.try_recv().is_err(),
        "a cleartext file addressed to one person is not a mesh DM"
    );
    assert!(
        timeline
            .history(&MeshConversation::Room, None, 10)
            .expect("history")
            .is_empty(),
        "and it is certainly not a room message"
    );
}

#[tokio::test]
async fn a_sent_file_is_signed_and_broadcast_with_the_payload_intact() {
    let (mut runtime, radio, state, _timeline) = runtime_parts();
    let link = LinkId::new("/mock/one");
    state.with(|inner| {
        inner.links.link_up(link.clone(), LinkRole::Central);
        inner.links.set_writable(&link, true);
        inner.max_write.insert(link.clone(), 512);
    });

    let file = sample_file();
    runtime.broadcast_file("MSG-F1", &file).await;
    runtime.drain_writes().await;

    let writes = radio.writes();
    assert_eq!(writes.len(), 1);
    let packet = codec::decode(&writes[0].1).expect("decodable");
    assert_eq!(packet.message_type(), Some(MessageType::FileTransfer));
    assert!(
        packet.recipient_id.is_none(),
        "\"everyone\" is encoded by omitting the recipient"
    );
    assert!(
        verify_packet(&packet, &runtime.identity.signing_public()),
        "an unsigned file transfer is dropped by the receiver before its \
             payload is read"
    );
    let decoded = BitchatFilePacket::decode(&packet.payload).expect("payload decodes");
    assert_eq!(decoded.content, file.content);
    assert_eq!(decoded.file_name.as_deref(), Some("note.txt"));
    assert_eq!(
        decoded.file_size,
        Some(file.content.len() as u64),
        "the declared size has to match the bytes, or the receiver rejects it"
    );

    // The echo of our own file must be recognisable, or it comes back and is
    // stored a second time.
    assert!(state.with(|inner| inner.self_broadcasts.contains(&packet)));
}
