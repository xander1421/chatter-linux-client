#[tokio::test]
async fn losing_the_peripheral_role_is_reported_rather_than_silent() {
    // The half-deaf state: this device still advertises, still scans, still
    // dials out — and accepts nobody. Seen as a desktop registering zero
    // inbound links all session while a phone in the same room reported
    // twenty-one successful outbound connections to it, each having subscribed
    // from its own side. Neither log could explain the other, because the side
    // that stopped working said nothing.
    let (mut runtime, _radio, _state, _timeline) = runtime_parts();
    let mut events = runtime.events.subscribe();

    runtime.on_link_event(LinkEvent::PeripheralRoleEnded).await;

    match events.try_recv() {
        Ok(MeshEvent::Degraded(reason)) => assert!(
            reason.to_lowercase().contains("connect"),
            "the reason has to name what stopped working, got {reason:?}"
        ),
        other => panic!("losing inbound links must be reported, not swallowed; got {other:?}"),
    }
}

#[tokio::test]
async fn a_connect_timeout_earns_a_longer_ignore_than_an_ordinary_refusal() {
    // The runtime tells the two apart on purpose — a timeout means the peer
    // held a scarce central slot for twenty seconds and gave nothing back, an
    // outright refusal means it said no cheaply — and until the mock could fail
    // a connect, neither branch had ever run in a test. The comment in `dial`
    // records that `note_connect_timeout` "existed and was never reachable
    // from here", which is the same class of gap: code written for a case
    // nothing could produce.
    use chatter_mesh_link::LinkError;

    const PEER: &str = "AA:BB:CC:DD:EE:FF";
    const STRONG: i16 = -40;

    // A timeout: ignored for the full timeout window.
    let (mut runtime, radio, state, _timeline) = runtime_parts();
    radio.set_connect_error(Some(LinkError::ConnectTimedOut {
        address: PEER.to_string(),
        stage: "subscribing for notifications".to_string(),
    }));
    runtime.dial(PEER).await;
    runtime.settle_dial().await;
    let after = now_millis() + chatter_mesh_link::scheduler::TIMEOUT_DISCOVERY_IGNORE_MS / 2;
    assert!(
        !state.with(|inner| inner.scheduler.should_connect(PEER, STRONG, 0, after)),
        "a peer that timed out is left alone for the whole timeout window"
    );

    // A single ordinary refusal: not enough to ignore it. One failed connect is
    // a peer that walked behind a wall, not a peer to give up on.
    let (mut runtime, radio, state, _timeline) = runtime_parts();
    radio.set_connect_error(Some(LinkError::ConnectFailed {
        address: PEER.to_string(),
        reason: "refused".to_string(),
    }));
    runtime.dial(PEER).await;
    runtime.settle_dial().await;
    let soon = now_millis() + chatter_mesh_link::scheduler::TIMEOUT_DISCOVERY_IGNORE_MS / 2;
    assert!(
        state.with(|inner| inner.scheduler.should_connect(PEER, STRONG, 0, soon)),
        "one refusal is not a timeout, and must not buy the timeout's silence"
    );
}

#[tokio::test]
async fn the_mock_refuses_an_oversized_frame_like_a_real_radio_would() {
    // Guards the guard. Both real link layers return `PayloadTooLarge` past the
    // negotiated MTU and the mock used to accept anything, so a fragment sized
    // wrongly would pass every test and be discarded on a radio — the failure
    // mode fragmentation bugs actually have.
    //
    // The whole mesh suite now writes against radios that enforce the same
    // number the planner fragments against, and passes. That is only worth
    // something if the enforcement can fail, which is what this checks.
    let (_runtime, radio, state, _timeline) = runtime_parts();
    let link = writable_link_on(&state, &radio, "/mock/tight", 64);

    assert!(
        matches!(
            radio.send(&link, &[0u8; 65]).await,
            Err(chatter_mesh_link::LinkError::PayloadTooLarge {
                actual: 65,
                max: 64
            })
        ),
        "one byte over the ceiling is refused"
    );
    assert!(
        radio.send(&link, &[0u8; 64]).await.is_ok(),
        "and exactly the ceiling is not"
    );
}

#[tokio::test]
async fn a_dead_link_takes_its_whole_queue_with_it() {
    // Observed on hardware once the drop was logged: the error is "link is no
    // longer connected", and every remaining frame for that link will fail the
    // same way. Draining them one per pacing interval spends 20 ms each to
    // learn that — twenty seconds for one queued chunk — while live links wait
    // their turn behind it.
    let (mut runtime, radio, state, _timeline) = runtime_parts();
    // 512 so the payload below fragments into a queue worth discarding.
    let link = writable_link_on(&state, &radio, "/mock/doomed", 512);
    let dir = tempfile::tempdir().expect("temp dir");
    let source = dir.path().join("doomed.bin");
    let bytes = body(4_000);
    std::fs::write(&source, &bytes).expect("write");

    runtime
        .start_streamed_file("MSG-DEAD", test_manifest([0x3D; 8], &bytes), source)
        .await;
    runtime.pump_streamed_files(now_secs()).await;
    let queued = state.with(|inner| inner.writes.queued_count(&link));
    assert!(queued > 2, "a chunk is many frames, all of them queued");

    // The radio now fails terminally, as a disconnected link does.
    radio.set_links_gone(true);
    runtime.flush_buffered_writes().await;

    assert_eq!(
        state.with(|inner| inner.writes.queued_count(&link)),
        0,
        "one failure is enough to know the rest will fail too"
    );
    assert_eq!(
        radio.write_count(),
        0,
        "and nothing reached the air, since the link is gone"
    );
}

#[tokio::test]
async fn a_relay_never_sends_a_packet_back_to_the_peer_it_came_from() {
    // Split horizon by link is not enough. Two devices can hold two links to
    // each other at once — each having connected outward to the other — and
    // excluding only the ingress link leaves the sibling link to the same peer
    // selected, so the relay sends the packet straight back.
    //
    // Measured on a two-node mesh before this: the sender logged 250 expired
    // reassemblies of its *own* chunks against 14 real ones, roughly seventeen
    // thousand fragments of its own traffic returning to occupy the air its
    // transfer needed.
    let (runtime, radio, state, _timeline) = runtime_parts();
    let neighbour = PeerId::from_str("bbbbbbbbbbbbbbbb");
    let identity = register_verified_peer(&state, &neighbour, "twolinks");

    // Both links to the same peer, as two devices that each dialled the other.
    let inbound = writable_link_on(&state, &radio, "/mock/they-dialled-us", 512);
    let sibling = writable_link_on(&state, &radio, "/mock/we-dialled-them", 512);
    state.with(|inner| {
        inner.links.bind_if_unbound(&inbound, &neighbour);
        inner.links.bind_if_unbound(&sibling, &neighbour);
    });

    // A broadcast from that peer, arriving on one of the two links.
    let packet = signed(
        &neighbour,
        &identity,
        MessageType::Message,
        b"worth relaying".to_vec(),
        true,
    );
    runtime
        .relay(&inbound, &packet, &runtime.identity.peer_id())
        .await;
    // `relay` spawns the write behind a collision-avoidance delay, so the
    // frames are not queued the instant it returns.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    runtime.drain_writes().await;

    let sent_back: Vec<_> = radio
        .writes()
        .into_iter()
        .filter(|(link, _)| *link == inbound || *link == sibling)
        .collect();
    assert!(
        sent_back.is_empty(),
        "the only links are to the peer that sent it, so the relay has nowhere \
             to go — it wrote {} frame(s) back at the sender",
        sent_back.len()
    );
}

#[tokio::test]
async fn frames_go_out_one_at_a_time_rather_than_all_at_once() {
    // The pacing that makes a large transfer possible, and where it now lives.
    //
    // It used to be a prediction: hold the next *chunk* back by how long its
    // 70 fragments ought to take, from a calibrated rate. That needed a rate
    // to calibrate, and one figure could not fit two radios — the same sender
    // measured ~10.5 kB/s to one phone and ~2.5 kB/s to another.
    //
    // So the spacing moved to the frames themselves, which is where the limit
    // actually is and needs no estimate. `transmit` queues; the drain takes one
    // frame per link per interval. bitchat spaces fragments the same way
    // (`interFragmentDelayMs = 20`).
    let (mut runtime, radio, state, _timeline) = runtime_parts();
    // 512, the real negotiated write size on this stack, so the chunk below
    // actually fragments. A text payload is no good here: `broadcast_public`
    // DEFLATEs, and any string long enough to fragment is compressible enough
    // not to.
    writable_link_on(&state, &radio, "/mock/paced", 512);
    let dir = tempfile::tempdir().expect("temp dir");
    let source = dir.path().join("paced.bin");
    let bytes = body(4_000);
    std::fs::write(&source, &bytes).expect("write");

    let manifest = test_manifest([0x2C; 8], &bytes);
    runtime
        .start_streamed_file("MSG-PACE", manifest.clone(), source)
        .await;
    // The manifest is one frame; the chunk is many. Clear the manifest out so
    // what is counted below is the fragment stream and nothing else.
    runtime.drain_writes().await;
    runtime.pump_streamed_files(now_secs()).await;

    let link = LinkId::new("/mock/paced");
    let queued_frames = state.with(|inner| inner.writes.queued_count(&link));
    assert!(
        queued_frames > 1,
        "a chunk is many frames, and all of them are queued rather than written"
    );
    assert_eq!(
        radio.write_count(),
        1,
        "only the manifest, drained above; the chunk's frames are all still queued"
    );

    // Each turn of the drain puts exactly one frame on the air per link.
    let baseline = radio.write_count();
    for step in 1..=queued_frames {
        runtime.flush_buffered_writes().await;
        assert_eq!(
            radio.write_count(),
            baseline + step,
            "one frame per interval, so the rate is the interval and not a guess"
        );
    }

    // And the queue empties rather than the tail being lost.
    assert!(state.with(|inner| inner.writes.is_empty()));
}

#[tokio::test]
async fn a_streamed_send_puts_a_manifest_out_first_then_paces_chunks_by_tick() {
    // The send side never reads the file whole, and never emits it in one
    // pass: the loop that emits chunks is the same loop that services link
    // events, so a 16 MiB transfer done in one go would leave the mesh deaf
    // for minutes.
    let (mut runtime, radio, state, timeline) = runtime_parts();
    let sender_dir = tempfile::tempdir().expect("temp dir");
    let source = sender_dir.path().join("outgoing.bin");
    let bytes = body(3_000);
    std::fs::write(&source, &bytes).expect("write");
    writable_link_on(&state, &radio, "/mock/send", 4096);
    let mut events = runtime.events.subscribe();

    let manifest = test_manifest([0x17; 8], &bytes);
    runtime
        .start_streamed_file("MSG-S1", manifest.clone(), source.clone())
        .await;
    runtime.drain_writes().await;

    // The manifest goes first, signed, as a broadcast.
    let writes = radio.writes();
    assert_eq!(writes.len(), 1, "one packet, and it is not a chunk");
    let packet = codec::decode(&writes[0].1).expect("decodable");
    assert_eq!(packet.message_type(), Some(MessageType::FileManifest));
    assert!(packet.recipient_id.is_none());
    assert!(verify_packet(&packet, &runtime.identity.signing_public()));
    assert_eq!(
        FileManifest::decode(&packet.payload).expect("decodes"),
        manifest
    );
    assert!(
        state.with(|inner| inner.self_broadcasts.contains(&packet)),
        "the echo of our own manifest has to be recognisable"
    );

    // The sender's own copy exists immediately: without it the row names an
    // attachment that does not exist and the sender is the one person on the
    // mesh who cannot open what they just sent.
    match events.try_recv() {
        Ok(MeshEvent::Message(message)) => {
            assert!(message.is_local);
            assert_eq!(mapping::attachments_of(&message.message).len(), 1);
        }
        other => panic!("expected the local mirror, got {other:?}"),
    }
    assert_eq!(
        timeline
            .history(&MeshConversation::Room, None, 10)
            .expect("history")
            .len(),
        1
    );

    // Then one chunk at a time, in order, each signed in its own right —
    // and each only once the previous one has had time on the air.
    let mut seen = Vec::new();
    let mut rebuilt = Vec::new();
    for _ in 0..manifest.chunk_count {
        let before = radio.write_count();
        runtime.pump_streamed_files(now_secs()).await;
        runtime.drain_writes().await;
        let writes = radio.writes();
        assert_eq!(
            writes.len(),
            before + 1,
            "one chunk per pacing interval, so the loop keeps servicing the radio"
        );
        let packet = codec::decode(&writes[before].1).expect("decodable");
        assert_eq!(packet.message_type(), Some(MessageType::FileChunk));
        assert!(
            verify_packet(&packet, &runtime.identity.signing_public()),
            "the receiver refuses an unsigned chunk"
        );
        let chunk = FileChunk::decode(&packet.payload).expect("decodes");
        assert_eq!(chunk.transfer_id, manifest.transfer_id);
        seen.push(chunk.index);
        rebuilt.extend_from_slice(&chunk.bytes);
    }
    assert_eq!(seen, (0..manifest.chunk_count).collect::<Vec<_>>());
    assert_eq!(rebuilt, bytes, "the chunks tile the file exactly");

    // Drained: nothing more goes out unprompted, because there is no
    // acknowledgement to wait for.
    let before = radio.write_count();
    runtime.pump_streamed_files(now_secs()).await;
    runtime.drain_writes().await;
    assert_eq!(radio.write_count(), before);
}

#[tokio::test]
async fn a_chunk_request_from_a_receiver_is_served_before_the_first_pass() {
    // The other half of receiver-driven retransmission. The peer that asked is
    // *blocked* on that chunk; the first pass is merely unfinished.
    let (mut runtime, radio, state, _timeline) = runtime_parts();
    let dir = tempfile::tempdir().expect("temp dir");
    let source = dir.path().join("outgoing2.bin");
    let bytes = body(4_000);
    std::fs::write(&source, &bytes).expect("write");
    writable_link_on(&state, &radio, "/mock/serve", 4096);
    let asker = PeerId::from_str("bbbbbbbbbbbbbbbb");
    let asker_identity = register_verified_peer(&state, &asker, "wanderer");

    let manifest = test_manifest([0x18; 8], &bytes);
    runtime
        .start_streamed_file("MSG-S2", manifest.clone(), source)
        .await;
    runtime.drain_writes().await;
    // Two passes, with the clock moved on between them so the second is not
    // held back by the first chunk's pacing interval.
    runtime.pump_streamed_files(now_secs()).await;
    runtime.drain_writes().await;
    runtime.pump_streamed_files(now_secs()).await;

    // A signed request for chunk 0, which has already been sent once.
    let request = FileChunkRequest::new(manifest.transfer_id, vec![0]).expect("valid");
    let packet = signed(
        &asker,
        &asker_identity,
        MessageType::FileChunkRequest,
        request.encode().expect("encodes"),
        true,
    );
    runtime.on_file_chunk_request(&packet, &asker);

    // Everything the first pass queued has to be on the air before the
    // baseline is taken, or `before` indexes into frames that were merely
    // waiting rather than the one the re-request produces.
    runtime.drain_writes().await;
    let before = radio.write_count();
    // Named rather than indexed. Tests install no tracing subscriber, so a
    // transfer that never started logs nothing anywhere, and indexing
    // straight into the write log turns that into an out-of-bounds panic
    // that says only "len is 0" — which is every possible cause at once.
    assert!(
        before > 0,
        "precondition: the manifest and the first pass should already be on \
             the air after two pumps, but nothing was written (transfer still \
             live: {})",
        !runtime.files_out.is_empty()
    );
    runtime.pump_streamed_files(now_secs()).await;
    runtime.drain_writes().await;
    let writes = radio.writes();
    let served = writes
        .get(before)
        .unwrap_or_else(|| panic!("the re-requested chunk should have been written"));
    let sent =
        FileChunk::decode(&codec::decode(&served.1).expect("decodable").payload).expect("decodes");
    assert_eq!(
        sent.index, 0,
        "the re-request jumps the queue ahead of the unfinished first pass"
    );

    // An unsigned request is ignored, so a stranger cannot make us spend
    // airtime re-sending a file.
    let unsigned = signed(
        &asker,
        &asker_identity,
        MessageType::FileChunkRequest,
        request.encode().expect("encodes"),
        false,
    );
    runtime.on_file_chunk_request(&unsigned, &asker);
    let before = radio.write_count();
    // Named rather than indexed. Tests install no tracing subscriber, so a
    // transfer that never started logs nothing anywhere, and indexing
    // straight into the write log turns that into an out-of-bounds panic
    // that says only "len is 0" — which is every possible cause at once.
    assert!(
        before > 0,
        "precondition: the manifest and the first pass should already be on \
             the air after two pumps, but nothing was written (transfer still \
             live: {})",
        !runtime.files_out.is_empty()
    );
    runtime.pump_streamed_files(now_secs()).await;
    runtime.drain_writes().await;
    let writes = radio.writes();
    let served = writes
        .get(before)
        .unwrap_or_else(|| panic!("the re-requested chunk should have been written"));
    let sent =
        FileChunk::decode(&codec::decode(&served.1).expect("decodable").payload).expect("decodes");
    assert_eq!(
        sent.index, 2,
        "the pass carried on; the unsigned request bought nothing"
    );
}

#[tokio::test]
async fn our_own_manifest_and_chunks_coming_back_are_ignored() {
    // These types have no gossip-sync bit, so a self-authored copy can only be
    // our own flood echoing back — there is nothing to recover and the local
    // copy was stored when the transfer started. Accepting one would spool our
    // own file into a second attachment.
    let (mut runtime, radio, state, timeline) = runtime_parts();
    isolate_spool(&mut runtime);
    writable_link_on(&state, &radio, "/mock/echo", 4096);
    let dir = tempfile::tempdir().expect("temp dir");
    let source = dir.path().join("mine.bin");
    let bytes = body(2_048);
    std::fs::write(&source, &bytes).expect("write");

    let manifest = test_manifest([0x19; 8], &bytes);
    runtime
        .start_streamed_file("MSG-S3", manifest.clone(), source)
        .await;
    runtime.drain_writes().await;
    let rows_before = timeline
        .history(&MeshConversation::Room, None, 10)
        .expect("history")
        .len();

    let local = runtime.identity.peer_id();
    let own_manifest = manifest_packet(&local, &runtime.identity.clone(), &manifest, true);
    runtime.on_file_manifest(&own_manifest, &local);
    assert!(
        runtime.files_in.is_empty(),
        "our own manifest must not open a receive spool for a file we are sending"
    );

    let own_chunk = chunk_packet(
        &local,
        &runtime.identity.clone(),
        &manifest,
        &bytes,
        0,
        true,
    );
    runtime.on_file_chunk(&own_chunk, &local);
    assert!(runtime.files_in.is_empty());
    assert_eq!(
        timeline
            .history(&MeshConversation::Room, None, 10)
            .expect("history")
            .len(),
        rows_before,
        "and no second row for a file we already have"
    );
}

#[tokio::test]
async fn a_chunked_transfer_is_refused_when_it_is_addressed_to_us_alone() {
    // Every gate P8 established still applies. A directed cleartext file has
    // no business in a client whose every DM is Noise-sealed, and accepting
    // one would teach the UI that private traffic can arrive in the clear.
    let (mut runtime, _radio, state, _timeline) = runtime_parts();
    isolate_spool(&mut runtime);
    let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
    let identity = register_verified_peer(&state, &sender, "wanderer");
    let local = runtime.identity.peer_id();

    let bytes = body(2_048);
    let manifest = test_manifest([0x1A; 8], &bytes);
    let mut packet = BitchatPacket::new(
        MessageType::FileManifest,
        sender.to_routing_bytes().expect("routing bytes"),
        local.to_routing_bytes(),
        now_millis(),
        manifest.encode().expect("encodes"),
        chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
    );
    packet.signature = Some(sign_packet(&identity, &packet).expect("signs"));
    runtime.on_file_manifest(&packet, &sender);
    assert!(runtime.files_in.is_empty());
}

#[tokio::test]
async fn a_small_file_still_rides_one_packet_and_a_large_one_streams() {
    // The interop guarantee. Real bitchat speaks `FileTransfer` and knows
    // nothing about our 0xE0–0xE2 extension, so everything small must keep going out the way
    // P8 sent it. Chunking is an extension for the sizes that path cannot
    // carry, selected by size — not a replacement.
    let (mut runtime, radio, state, _timeline) = runtime_parts();
    writable_link_on(&state, &radio, "/mock/fork", 4096);

    runtime.broadcast_file("MSG-SMALL", &sample_file()).await;

    runtime.drain_writes().await;
    let writes = radio.writes();
    assert_eq!(
        codec::decode(&writes[0].1)
            .expect("decodable")
            .message_type(),
        Some(MessageType::FileTransfer),
        "a small file is still a single bitchat file packet"
    );

    let dir = tempfile::tempdir().expect("temp dir");
    let source = dir.path().join("big.bin");
    let bytes = body(MAX_SINGLE_PACKET_FILE_BYTES + 1);
    std::fs::write(&source, &bytes).expect("write");
    let before = radio.write_count();
    runtime
        .start_streamed_file("MSG-BIG", test_manifest([0x1B; 8], &bytes), source)
        .await;
    runtime.drain_writes().await;
    let writes = radio.writes();
    assert_eq!(
        codec::decode(&writes[before].1)
            .expect("decodable")
            .message_type(),
        Some(MessageType::FileManifest),
        "and anything above one packet's worth announces itself instead"
    );
}
