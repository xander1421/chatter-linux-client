// ---------------------------------------------- chunked file transfer (P9)

use chatter_mesh::tlv::file_stream::MAX_SINGLE_PACKET_FILE_BYTES;

/// Chunk size for the runtime tests: the smallest a manifest may declare, so
/// a handful of kilobytes exercises several chunks without a megabyte of IO.
const TEST_CHUNK: u32 = 1024;

/// Point one runtime's receive spool at a directory of its own.
///
/// The attachments base is a process-wide `OnceLock`, so every test in the
/// process would otherwise share one spool directory and one test's partial
/// transfer could be restored into another's runtime.
fn isolate_spool(runtime: &mut BleRuntime) -> std::path::PathBuf {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().to_path_buf();
    crate::test_tempdir::hold(dir);
    runtime.files_in = FileStreamSpool::new(path.clone());
    path
}

/// Position-dependent filler: a chunk written at the wrong offset changes the
/// digest, which a run of one byte would hide.
fn body(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn test_manifest(id: [u8; 8], bytes: &[u8]) -> FileManifest {
    use sha2::{Digest, Sha256};
    FileManifest::new(
        id,
        Some("clip.bin".into()),
        Some("application/octet-stream".into()),
        bytes.len() as u64,
        TEST_CHUNK,
        Sha256::digest(bytes).into(),
    )
    .expect("valid manifest")
}

fn signed(
    sender: &PeerId,
    identity: &MeshIdentity,
    msg_type: MessageType,
    payload: Vec<u8>,
    sign: bool,
) -> BitchatPacket {
    let mut packet = BitchatPacket::new(
        msg_type,
        sender.to_routing_bytes().expect("routing bytes"),
        None,
        now_millis(),
        payload,
        chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
    );
    if sign {
        packet.signature = Some(sign_packet(identity, &packet).expect("signs"));
    }
    packet
}

fn manifest_packet(
    sender: &PeerId,
    identity: &MeshIdentity,
    manifest: &FileManifest,
    sign: bool,
) -> BitchatPacket {
    signed(
        sender,
        identity,
        MessageType::FileManifest,
        manifest.encode().expect("encodes"),
        sign,
    )
}

fn chunk_packet(
    sender: &PeerId,
    identity: &MeshIdentity,
    manifest: &FileManifest,
    bytes: &[u8],
    index: u32,
    sign: bool,
) -> BitchatPacket {
    let (offset, len) = manifest.chunk_span(index).expect("in range");
    let slice = bytes[offset as usize..offset as usize + len].to_vec();
    signed(
        sender,
        identity,
        MessageType::FileChunk,
        FileChunk::new(manifest.transfer_id, index, slice)
            .expect("valid chunk")
            .encode()
            .expect("encodes"),
        sign,
    )
}

/// A writable link, so `transmit` produces writes instead of logging that
/// there was nowhere to send.
///
/// `max_write` is a parameter because it decides whether one packet becomes
/// one write. Real links negotiate ~514, so a real 32 KiB chunk is ~70
/// fragments — that split is the fragmenter's business and is tested there.
/// The send tests below pass a value above the chunk size so that "one chunk
/// per tick" can be counted in writes, which is the property they are
/// actually about.
fn writable_link(state: &MeshState, name: &str, max_write: usize) -> LinkId {
    let link = LinkId::new(name);
    state.with(|inner| {
        inner.links.link_up(link.clone(), LinkRole::Central);
        inner.links.set_writable(&link, true);
        inner.max_write.insert(link.clone(), max_write);
    });
    link
}

/// `writable_link`, with the radio told the same MTU the planner will use.
///
/// Worth the extra argument: it is what makes an oversized write fail in a
/// test rather than on a radio. The two numbers being the same is the point —
/// a fragmenter that sizes against one and writes against the other is exactly
/// the bug this catches.
fn writable_link_on(state: &MeshState, radio: &MockLink, name: &str, max_write: usize) -> LinkId {
    let link = writable_link(state, name, max_write);
    radio.set_max_write(&link, max_write);
    link
}

#[tokio::test]
async fn the_tick_reclaims_a_partial_frame_from_a_peer_that_goes_silent() {
    // One byte in through the real inbound path, then silence on a link that
    // stays up. `append` would only reclaim it on the next write, which never
    // comes; the tick has to.
    let (mut runtime, _radio, state, _timeline) = runtime_parts();
    let link = writable_link(&state, "silent", 512);

    runtime.on_data(&link, vec![0x01]).await;
    assert_eq!(state.with(|s| s.assembler.buffered_bytes(&link)), 1);

    tokio::time::sleep(Duration::from_millis(
        InboundLimits::default().stall_reset_ms + 50,
    ))
    .await;
    runtime.on_tick().await;

    assert_eq!(
        state.with(|s| s.assembler.total_buffered_bytes()),
        0,
        "a stalled partial survived the tick"
    );
    assert!(state.with(|s| s.assembler.is_empty()));
}

#[tokio::test]
async fn the_tick_leaves_a_partial_frame_that_is_still_inside_its_window() {
    // Stamped in the future so idle time is zero however slow the machine
    // is: the sweep must not eat frames that are still arriving.
    let (mut runtime, _radio, state, _timeline) = runtime_parts();
    let link = writable_link(&state, "busy", 512);
    let _ = state.with(|s| {
        s.assembler.append(
            &link,
            &[InboundChunk {
                offset: 0,
                bytes: vec![0x01, 0x02],
            }],
            InboundLimits::default(),
            now_millis() + 60_000,
        )
    });

    runtime.on_tick().await;

    assert_eq!(state.with(|s| s.assembler.buffered_bytes(&link)), 2);
}

#[tokio::test]
async fn a_radio_that_will_not_restart_when_nobody_can_see_us_is_reported() {
    // `should_ensure_advertising` fires whenever the peer registry is empty,
    // and restarting the radio is the only lever the trait offers for "nobody
    // can see us". The result was discarded, so a restart that failed left the
    // peripheral role invisible and said nothing — the recovery path giving up
    // in silence, which is indistinguishable from an empty room.
    let (mut runtime, radio, _state, _timeline) = runtime_parts();
    let mut events = runtime.events.subscribe();

    // A stopped MockLink refuses to start again, exactly as the Android link
    // does: its handle is single-use.
    let _ = radio.stop().await;

    runtime.on_tick().await;

    let mut degraded = None;
    while let Ok(event) = events.try_recv() {
        if let MeshEvent::Degraded(message) = event {
            degraded = Some(message);
            break;
        }
    }
    let message = degraded
        .expect("the radio refused to restart while nobody could see us and nothing was reported");
    assert!(
        message.contains("restart"),
        "the notice does not say the restart failed: {message}"
    );
}

#[tokio::test]
async fn a_rotated_address_of_a_held_peer_is_not_dialled() {
    // The fix that stops the churn at its source. Every rotation used to cost
    // a connection just to learn who was behind it, and that connection
    // displaced the ACL already held — whose loss restarted the peer's
    // advertiser under yet another address. Nineteen addresses in four
    // minutes, all one phone.
    let (mut runtime, radio, state, _timeline) = runtime_parts();
    let peer = PeerId::from_str("4b51a6a51aa7eb3b");
    let held = writable_link(&state, "central:dev_54_CB_34#1", 514);
    state.with(|inner| {
        inner.links.bind_if_unbound(&held, &peer);
    });

    let bytes = peer.to_routing_bytes().expect("an eight byte peer id");
    runtime
        .on_discovered("70:E0:8F:D7:6B:84", -50, Some(bytes))
        .await;
    assert!(
        radio.connect_attempts().is_empty(),
        "dialled another address for a peer already held"
    );

    // An advert with no id still gets dialled. The scan response often lands
    // after the advertisement, so `None` has to mean "unknown", never "not a
    // peer" — reading it the other way would make the mesh undiscoverable.
    runtime.on_discovered("70:E0:8F:D7:6B:84", -50, None).await;
    assert_eq!(
        radio.connect_attempts(),
        vec!["70:E0:8F:D7:6B:84".to_string()],
        "an advert with no peer id must still be dialled"
    );
}

#[test]
fn one_peer_under_several_rotated_addresses_keeps_exactly_one_central_link() {
    // The measured failure: a Galaxy S9+ rotating its private address gave one
    // desktop eight central links to one peer inside two minutes, against six
    // central slots. Every extra ACL is a slot the rest of the mesh cannot
    // have and a GATT server the phone has to keep serving.
    let (_runtime, _radio, state, _timeline) = runtime_parts();
    let peer = PeerId::from_str("4b51a6a51aa7eb3b");

    // Three links to the same peer, in the order the rotations arrived. The
    // first is stale — its socket is gone, so it is not writable.
    let stale = writable_link(&state, "central:dev_54_CB_34#1", 514);
    let second = writable_link(&state, "central:dev_70_E0_8F#2", 514);
    let third = writable_link(&state, "central:dev_5B_4F_1D#3", 514);
    // ...and one *peripheral* link to the same peer, which the peer dialled to
    // us. Not ours to hang up.
    let inbound = LinkId::new("peripheral:dev_5B_4F_1D#4");
    state.with(|inner| {
        inner.links.link_up(inbound.clone(), LinkRole::Peripheral);
        inner.links.set_writable(&inbound, true);
        inner.links.set_writable(&stale, false);
        for link in [&stale, &second, &third, &inbound] {
            inner.links.bind_if_unbound(link, &peer);
        }
    });

    let doomed = state.with(|inner| redundant_central_links_locked(inner, &peer));

    // Asserted as properties rather than a fixed list: `links()` is ordered by
    // `LinkId`, which is the D-Bus object path, and a rotated address sorts
    // wherever its hex happens to fall — arrival order is not recoverable from
    // it and the test should not pretend otherwise.
    assert_eq!(
        doomed.len(),
        2,
        "expected exactly one central link to survive"
    );
    assert!(doomed.contains(&stale), "kept a link whose socket is gone");
    assert!(
        !doomed.contains(&inbound),
        "hung up a link the peer dialled to us"
    );
    let keeper = [&second, &third]
        .into_iter()
        .find(|link| !doomed.contains(link))
        .expect("a writable central link survives");
    // The newest writable link, specifically. A duplicate exists because the
    // peer rotated, so the older link sits on an address it has moved off —
    // and keeping that one cost both links in the measured cold-start race.
    assert_eq!(keeper, &third, "kept an older link over a fresher one");

    // Deterministic across ingresses: every link carrying an announce computes
    // this, and two announces landing together must agree on the survivor or
    // they close each other. Same input, same answer.
    state.with(|inner| {
        assert_eq!(
            redundant_central_links_locked(inner, &peer),
            doomed,
            "two announces would disagree on the survivor"
        );
        for link in &doomed {
            inner.links.link_down(link);
        }
        assert!(
            redundant_central_links_locked(inner, &peer).is_empty(),
            "kept pruning once {keeper} was the only one left"
        );
    });
}

#[tokio::test]
async fn a_chunked_transfer_round_trips_into_a_row_with_its_bytes_on_disk() {
    // The whole of P9 in one test. A payload this size cannot arrive as one
    // packet at all — `FRAGMENT_LIFETIME_SECS` is measured from the first
    // fragment, so a single-packet send of it is evicted at 30 s however far
    // it got — and it arrives here in chunks, each verified in its own right,
    // written straight to disk, digest-checked, and encrypted into place.
    let (mut runtime, _radio, state, timeline) = runtime_parts();
    let spool = isolate_spool(&mut runtime);
    let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
    let identity = register_verified_peer(&state, &sender, "wanderer");
    let mut events = runtime.events.subscribe();

    let bytes = body(2_500);
    let manifest = test_manifest([0x10; 8], &bytes);
    assert_eq!(manifest.chunk_count, 3);

    runtime.on_file_manifest(
        &manifest_packet(&sender, &identity, &manifest, true),
        &sender,
    );
    assert!(
        events.try_recv().is_err(),
        "a manifest is not a message; nothing may surface until the bytes are \
             here and verified"
    );
    for index in 0..3 {
        runtime.on_file_chunk(
            &chunk_packet(&sender, &identity, &manifest, &bytes, index, true),
            &sender,
        );
    }

    let attachment = match events.try_recv() {
        Ok(MeshEvent::Message(message)) => {
            assert!(!message.is_local);
            assert_eq!(message.conversation, MeshConversation::Room);
            assert!(message.sender_verified);
            let attachments = mapping::attachments_of(&message.message);
            assert_eq!(attachments.len(), 1);
            attachments[0]
        }
        other => panic!("expected the completed file, got {other:?}"),
    };

    let history = timeline
        .history(&MeshConversation::Room, None, 10)
        .expect("history");
    assert_eq!(history.len(), 1);
    assert_eq!(mapping::attachments_of(&history[0]), [attachment]);

    let path = crate::services::attachment::attachment_file_path(&attachment);
    assert!(path.exists(), "the bytes have to be at {}", path.display());
    let stored = std::fs::read(&path).expect("readable");
    assert_ne!(
        stored, bytes,
        "a mesh file lands encrypted, streamed through the same call the CDN \
             upload path uses"
    );

    // The spool is empty and its plaintext is gone: a partial file is
    // unencrypted by construction, so leaving one behind after the transfer
    // finished would be the one thing in this app lying open on disk.
    assert!(runtime.files_in.is_empty());
    assert_eq!(
        std::fs::read_dir(&spool).expect("spool").count(),
        0,
        "no .part or .state may survive a completed transfer"
    );
}

#[tokio::test]
async fn a_missing_chunk_is_re_requested_and_the_transfer_then_completes() {
    // Retransmission is *receiver*-requested, and it has to be: a broadcast
    // is not acknowledged, so the sender cannot know what landed. Without
    // this, one dropped chunk out of thousands means a file that never
    // arrives and nobody can say why.
    let (mut runtime, radio, state, timeline) = runtime_parts();
    isolate_spool(&mut runtime);
    writable_link_on(&state, &radio, "/mock/req", 512);
    let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
    let identity = register_verified_peer(&state, &sender, "wanderer");

    let bytes = body(3_000);
    let manifest = test_manifest([0x11; 8], &bytes);
    runtime.on_file_manifest(
        &manifest_packet(&sender, &identity, &manifest, true),
        &sender,
    );
    for index in [0u32, 2] {
        runtime.on_file_chunk(
            &chunk_packet(&sender, &identity, &manifest, &bytes, index, true),
            &sender,
        );
    }

    // Nothing is asked for while chunks are still plausibly in flight.
    let before = radio.write_count();
    runtime.request_missing_chunks(now_secs()).await;
    assert_eq!(
        radio.write_count(),
        before,
        "a transfer that just made progress is not stalled"
    );

    // Once it has gone quiet, the receiver names exactly what it is missing.
    runtime
        .request_missing_chunks(now_secs() + CHUNK_REQUEST_QUIET_SECS)
        .await;
    runtime.drain_writes().await;
    let writes = radio.writes();
    let packet = codec::decode(&writes[before].1).expect("decodable");
    assert_eq!(packet.message_type(), Some(MessageType::FileChunkRequest));
    assert!(
        verify_packet(&packet, &runtime.identity.signing_public()),
        "an unsigned request is refused by the sender, because serving one \
             costs airtime and would otherwise be an amplification lever"
    );
    let request = FileChunkRequest::decode(&packet.payload).expect("decodes");
    assert_eq!(request.transfer_id, manifest.transfer_id);
    assert_eq!(request.indices, vec![1], "only the chunk that never came");

    // And the answer completes the file rather than restarting it.
    runtime.on_file_chunk(
        &chunk_packet(&sender, &identity, &manifest, &bytes, 1, true),
        &sender,
    );
    assert_eq!(
        timeline
            .history(&MeshConversation::Room, None, 10)
            .expect("history")
            .len(),
        1,
        "the recovered chunk finished the transfer"
    );
}

#[tokio::test]
async fn an_unsigned_chunk_is_refused() {
    // The gate the design is most emphatic about. A manifest signed once says
    // nothing about the chunks that follow it, so accepting an unsigned chunk
    // would turn a transfer into an unauthenticated write channel into a file
    // that is about to be stored and shown under the manifest sender's name.
    let (mut runtime, _radio, state, timeline) = runtime_parts();
    isolate_spool(&mut runtime);
    let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
    let identity = register_verified_peer(&state, &sender, "wanderer");
    let mut events = runtime.events.subscribe();

    let bytes = body(2_048);
    let manifest = test_manifest([0x12; 8], &bytes);
    runtime.on_file_manifest(
        &manifest_packet(&sender, &identity, &manifest, true),
        &sender,
    );

    // Chunk 0 unsigned, chunk 1 properly signed.
    runtime.on_file_chunk(
        &chunk_packet(&sender, &identity, &manifest, &bytes, 0, false),
        &sender,
    );
    assert_eq!(
        runtime
            .files_in
            .get(&manifest.transfer_id)
            .expect("still open")
            .received
            .present(),
        0,
        "an unsigned chunk contributes nothing"
    );

    runtime.on_file_chunk(
        &chunk_packet(&sender, &identity, &manifest, &bytes, 1, true),
        &sender,
    );
    assert_eq!(
        runtime
            .files_in
            .get(&manifest.transfer_id)
            .expect("still open")
            .received
            .present(),
        1,
        "and a signed one does, so the refusal is the signature and not the shape"
    );

    // The transfer is incomplete, so nothing surfaced and nothing was stored.
    assert!(events.try_recv().is_err());
    assert!(
        timeline
            .history(&MeshConversation::Room, None, 10)
            .expect("history")
            .is_empty()
    );
}

#[tokio::test]
async fn an_unsigned_manifest_never_opens_a_transfer() {
    // The same reasoning one packet earlier: a manifest's sender is a header
    // field the sender chose, so without a signature verifying against the
    // key their announce established, anyone in range opens a file under
    // anyone's name — and every chunk after it inherits that attribution.
    let (mut runtime, _radio, state, _timeline) = runtime_parts();
    isolate_spool(&mut runtime);
    let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
    let identity = register_verified_peer(&state, &sender, "wanderer");

    let bytes = body(2_048);
    let manifest = test_manifest([0x13; 8], &bytes);
    runtime.on_file_manifest(
        &manifest_packet(&sender, &identity, &manifest, false),
        &sender,
    );
    assert!(runtime.files_in.is_empty());

    // Nor does a validly signed manifest from a peer that never announced:
    // there is no key to check it against, so it authenticates itself.
    let stranger = PeerId::from_str("cccccccccccccccc");
    let own = MeshIdentity::generate(&mut os_rng());
    runtime.on_file_manifest(
        &manifest_packet(&stranger, &own, &manifest, true),
        &stranger,
    );
    assert!(runtime.files_in.is_empty());
}

#[tokio::test]
async fn a_chunk_from_someone_other_than_the_manifests_sender_is_refused() {
    // Both peers are verified and both signatures are valid — the packet-level
    // gates all pass. What must not pass is a *different* verified peer
    // contributing bytes to a file somebody else opened.
    let (mut runtime, _radio, state, _timeline) = runtime_parts();
    isolate_spool(&mut runtime);
    let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
    let identity = register_verified_peer(&state, &sender, "wanderer");
    let other = PeerId::from_str("dddddddddddddddd");
    let other_identity = register_verified_peer(&state, &other, "someone else");

    let bytes = body(2_048);
    let manifest = test_manifest([0x14; 8], &bytes);
    runtime.on_file_manifest(
        &manifest_packet(&sender, &identity, &manifest, true),
        &sender,
    );
    runtime.on_file_chunk(
        &chunk_packet(&other, &other_identity, &manifest, &bytes, 0, true),
        &other,
    );
    assert_eq!(
        runtime
            .files_in
            .get(&manifest.transfer_id)
            .expect("still open")
            .received
            .present(),
        0
    );
}

#[tokio::test]
async fn a_digest_mismatch_discards_the_transfer_and_asks_for_nothing_more() {
    // A mismatch means some chunk that passed its packet signature still
    // produced the wrong file. There is no chunk to blame, so re-requesting
    // would fetch the same bytes and fail identically — forever, and
    // indistinguishably from a slow transfer.
    let (mut runtime, radio, state, timeline) = runtime_parts();
    let spool = isolate_spool(&mut runtime);
    writable_link_on(&state, &radio, "/mock/bad", 512);
    let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
    let identity = register_verified_peer(&state, &sender, "wanderer");
    let mut events = runtime.events.subscribe();

    let bytes = body(2_500);
    let mut manifest = test_manifest([0x15; 8], &bytes);
    // A manifest whose digest does not describe the bytes that follow it. The
    // manifest is still correctly signed, so nothing before the digest check
    // can catch this.
    manifest.sha256 = [0xAB; 32];
    runtime.on_file_manifest(
        &manifest_packet(&sender, &identity, &manifest, true),
        &sender,
    );
    for index in 0..3 {
        runtime.on_file_chunk(
            &chunk_packet(&sender, &identity, &manifest, &bytes, index, true),
            &sender,
        );
    }

    // Discarded: no row, no attachment, no bytes.
    assert!(
        timeline
            .history(&MeshConversation::Room, None, 10)
            .expect("history")
            .is_empty(),
        "a file that failed its digest must not reach the timeline"
    );
    assert!(runtime.files_in.is_empty());
    assert_eq!(std::fs::read_dir(&spool).expect("spool").count(), 0);

    // Not retried: the tick asks for nothing, so there is no loop.
    let before = radio.write_count();
    runtime
        .request_missing_chunks(now_secs() + CHUNK_REQUEST_QUIET_SECS * 10)
        .await;
    assert_eq!(
        radio.write_count(),
        before,
        "a discarded transfer has nothing left to re-request"
    );

    // And the user is told, because minutes of radio produced nothing.
    let mut degraded = false;
    while let Ok(event) = events.try_recv() {
        if matches!(event, MeshEvent::Degraded(_)) {
            degraded = true;
        }
    }
    assert!(degraded, "a corrupt file is worth saying out loud");
}

#[tokio::test]
async fn a_restart_resumes_a_transfer_instead_of_starting_it_again() {
    // The reason the bitmap is on disk. In memory, resume survives a link
    // drop and dies with the process — which is exactly when it is most
    // wanted: the app was killed, the file is still half there, and the only
    // record of which half was in a map.
    let (mut first, _radio, state, _timeline) = runtime_parts();
    let spool = isolate_spool(&mut first);
    let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
    let identity = register_verified_peer(&state, &sender, "wanderer");

    let bytes = body(5_000);
    let manifest = test_manifest([0x16; 8], &bytes);
    assert_eq!(manifest.chunk_count, 5);
    first.on_file_manifest(
        &manifest_packet(&sender, &identity, &manifest, true),
        &sender,
    );
    for index in [0u32, 1, 2] {
        first.on_file_chunk(
            &chunk_packet(&sender, &identity, &manifest, &bytes, index, true),
            &sender,
        );
    }
    drop(first);

    // A new process over the same spool directory.
    let (mut restarted, _radio2, state2, timeline2) = runtime_parts();
    restarted.files_in = FileStreamSpool::new(spool);
    assert_eq!(
        restarted.files_in.restore(now_secs()),
        1,
        "the partial transfer is back"
    );
    let identity = register_verified_peer(&state2, &sender, "wanderer");
    assert_eq!(
        restarted
            .files_in
            .get(&manifest.transfer_id)
            .expect("resumed")
            .received
            .present(),
        3,
        "three chunks are already here and must not be asked for again"
    );

    // The sender's next manifest resumes rather than truncating.
    restarted.on_file_manifest(
        &manifest_packet(&sender, &identity, &manifest, true),
        &sender,
    );
    assert_eq!(
        restarted
            .files_in
            .get(&manifest.transfer_id)
            .expect("still resumed")
            .received
            .present(),
        3
    );

    for index in [3u32, 4] {
        restarted.on_file_chunk(
            &chunk_packet(&sender, &identity, &manifest, &bytes, index, true),
            &sender,
        );
    }
    assert_eq!(
        timeline2
            .history(&MeshConversation::Room, None, 10)
            .expect("history")
            .len(),
        1,
        "two chunks finished a transfer that had already survived a restart"
    );
}

#[tokio::test]
async fn a_transfer_that_completed_but_was_never_stored_is_finished_on_the_next_sweep() {
    // The one case where every byte is on disk and nothing will retransmit
    // them: the process ended between the last chunk and the store, or the
    // store failed. Losing the file then would waste minutes of radio that
    // nothing spends again.
    let (mut first, _radio, state, _timeline) = runtime_parts();
    let spool = isolate_spool(&mut first);
    let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
    let identity = register_verified_peer(&state, &sender, "wanderer");

    let bytes = body(2_048);
    let manifest = test_manifest([0x1C; 8], &bytes);
    first.on_file_manifest(
        &manifest_packet(&sender, &identity, &manifest, true),
        &sender,
    );
    // Only the first chunk goes through the runtime; the second is written
    // straight into the spool so the completion handler never runs, which is
    // what a kill between the two looks like on disk.
    first.on_file_chunk(
        &chunk_packet(&sender, &identity, &manifest, &bytes, 0, true),
        &sender,
    );
    assert_eq!(
        first
            .files_in
            .accept_chunk(
                &manifest.transfer_id,
                1,
                &bytes[1024..],
                &sender,
                now_secs()
            )
            .expect("accepted"),
        ChunkOutcome::Complete
    );
    drop(first);

    let (mut restarted, _radio2, state2, timeline2) = runtime_parts();
    restarted.files_in = FileStreamSpool::new(spool);
    assert_eq!(restarted.files_in.restore(now_secs()), 1);
    register_verified_peer(&state2, &sender, "wanderer");
    assert_eq!(restarted.files_in.completed(), vec![manifest.transfer_id]);

    restarted.cleanup(now_millis(), now_secs());
    assert_eq!(
        timeline2
            .history(&MeshConversation::Room, None, 10)
            .expect("history")
            .len(),
        1,
        "the sweep stored the file rather than leaving it to expire"
    );
    assert!(restarted.files_in.is_empty());
}
