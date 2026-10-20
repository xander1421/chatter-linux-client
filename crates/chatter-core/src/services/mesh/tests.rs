use super::mock::{LoopbackConnector, MockLink, OfflineConnector};
use super::*;
use crate::domain::message::ConversationType;
use chatter_mesh_store::MemoryStorage;
use chatter_mesh_store::StoreError;

struct NoopSealer;

impl Sealer for NoopSealer {
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, StoreError> {
        Ok(plaintext.to_vec())
    }

    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, StoreError> {
        Ok(sealed.to_vec())
    }
}

/// A service built without touching the database.
///
/// `MeshIdentityStore` needs a real `Database`, and construction tests are
/// about the wiring rather than about SQLCipher, so this assembles the same
/// parts `MeshService::new` does from an identity handed in directly.
fn service(
    radio: Option<Arc<dyn MeshLink>>,
    connector: Option<Arc<dyn RelayConnector>>,
) -> MeshService {
    let factory = radio.map(|radio| {
        Arc::new(move || Arc::clone(&radio)) as Arc<dyn Fn() -> Arc<dyn MeshLink> + Send + Sync>
    });
    service_with_radio_factory(factory, connector)
}

/// A service that builds its radio per `start`, as production does.
fn service_with_radio_factory(
    radio_factory: Option<Arc<dyn Fn() -> Arc<dyn MeshLink> + Send + Sync>>,
    connector: Option<Arc<dyn RelayConnector>>,
) -> MeshService {
    let identity = Arc::new(MeshIdentity::generate(&mut crate::crypto::os_rng()));
    let config = Arc::new(MeshConfig::default());
    let state = Arc::new(MeshState::new(identity.peer_id()));
    let nostr_state = Arc::new(NostrState::new());
    let timeline = timeline::temp_timeline();
    let identity_store = timeline::temp_identity_store();
    let dir = tempfile::tempdir().expect("mesh stores temp dir");
    let db = Arc::new(
        crate::storage::Database::open(&dir.path().join("mesh_stores.db"), "testpass")
            .expect("mesh stores database"),
    );
    db.run_migrations().expect("mesh stores migrations");
    crate::test_tempdir::hold(dir);
    let events = MeshEvents::new(EVENT_CAPACITY, Arc::clone(&timeline));
    let (ble_bus, ble_rx) = CommandBus::new();
    let (nostr_bus, nostr_rx) = CommandBus::new();
    // Kept alive rather than dropped. Several tests exercise send paths on a
    // service they never start, and a channel with no receiver refuses every
    // send — so dropping these would fail those tests for a reason that has
    // nothing to do with what they assert. `start` rebinds and does not use
    // them; they exist only so the bus has somewhere to put a command.
    Box::leak(Box::new((ble_rx, nostr_rx)));
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let sealer: Arc<dyn Sealer> = Arc::new(NoopSealer);

    let router = Arc::new(RouterHandle::spawn(RouterDeps {
        state: Arc::clone(&state),
        nostr_state: Arc::clone(&nostr_state),
        identity: Arc::clone(&identity),
        storage: Arc::clone(&storage),
        sealer: Arc::clone(&sealer),
        ble: Arc::clone(&ble_bus),
        nostr: Arc::clone(&nostr_bus),
        events: events.clone(),
        bridge_enabled: false,
    }));

    MeshService {
        key_store: Arc::new(KeyStore::new(db.clone())),
        binding_store: Arc::new(MeshBindingStore::new(db.clone())),
        private_channels: Arc::new(PrivateChannelStore::new(db)),
        signal_lock: Arc::new(Mutex::new(())),
        // The test sealer is a no-op that always succeeds, so there is
        // nothing to report.
        sealer_unavailable: None,
        identity,
        identity_store,
        config,
        state,
        nostr_state,
        storage,
        sealer,
        router,
        ble: ble_bus,
        nostr: nostr_bus,
        events,
        timeline,
        has_radio: radio_factory.is_some(),
        radio_factory,
        relay_connector: connector,
        tasks: Mutex::new(Vec::new()),
        running: AtomicBool::new(false),
        identity_wiped: AtomicBool::new(false),
    }
}

#[tokio::test]
async fn the_mesh_can_be_stopped_and_started_again() {
    // What the user's switch needs, and what neither half of this used to do.
    //
    // `start` once consumed a one-shot `Pending`, so a second call returned
    // `Ok(())` having started nothing. That was fixed with `CommandBus` — and
    // the test passed while the feature was still broken, because the service
    // kept *one radio* for its lifetime and handed the same stopped object to
    // the new runtime. A phone refused it outright: "this link layer has been
    // stopped; construct a new AndroidLink to restart".
    //
    // The mock hid that by being more permissive than production. It refuses a
    // second start now, so a service that reuses a radio fails here rather than
    // on somebody's phone — and this test supplies fresh radios, which is what
    // `start` does for real via `platform_radio`.
    let built: Arc<Mutex<Vec<Arc<MockLink>>>> = Arc::new(Mutex::new(Vec::new()));
    let radios = Arc::clone(&built);
    let factory: Arc<dyn Fn() -> Arc<dyn MeshLink> + Send + Sync> = Arc::new(move || {
        let radio = MockLink::shared();
        radios.lock().push(Arc::clone(&radio));
        radio as Arc<dyn MeshLink>
    });
    let service = service_with_radio_factory(
        Some(factory),
        Some(Arc::new(LoopbackConnector::new()) as Arc<dyn RelayConnector>),
    );

    async fn settle() {
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }

    service.start().await.expect("start");
    settle().await;
    assert!(service.is_running());
    assert_eq!(built.lock().len(), 1, "one radio built for the first start");
    assert!(built.lock()[0].is_started(), "and it is up");

    service.shutdown().await;
    assert!(!service.is_running());

    service.start().await.expect("restart");
    settle().await;
    assert!(service.is_running(), "a stopped mesh starts again");
    assert_eq!(
        built.lock().len(),
        2,
        "a *new* radio, because stopping one is terminal on both real link layers"
    );
    assert!(
        built.lock()[1].is_started(),
        "and the radio is up again, not merely the flag"
    );

    // And a command sent after the restart reaches the new runtime rather
    // than the channel the stopped one took with it.
    service
        .send_room("still here")
        .expect("a send after a restart is routed");
    service
        .router
        .send_private(
            "after restart".into(),
            PeerId::from_str("bbbbbbbbbbbbbbbb"),
            "Bob".into(),
            "restart-router-test".into(),
        )
        .await
        .expect("the private-message router also survives restart");

    service.shutdown().await;
    assert!(!service.is_running());
}

#[tokio::test]
async fn it_starts_and_stops_with_both_transports_mocked() {
    let radio = MockLink::shared();
    let service = service(
        Some(Arc::clone(&radio) as Arc<dyn MeshLink>),
        Some(Arc::new(LoopbackConnector::new()) as Arc<dyn RelayConnector>),
    );
    service.start().await.expect("start");
    // Give the runtimes a turn so the radio actually comes up.
    tokio::task::yield_now().await;
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert!(radio.is_started(), "the radio was started");
    service.shutdown().await;
}

#[tokio::test]
async fn starting_twice_is_a_no_op_rather_than_a_second_stack() {
    let radio = MockLink::shared();
    let service = service(Some(Arc::clone(&radio) as Arc<dyn MeshLink>), None);
    service.start().await.expect("first start");
    service.start().await.expect("second start");
    assert_eq!(service.tasks.lock().len(), 1, "one radio task, not two");
    service.shutdown().await;
}

#[tokio::test]
async fn it_constructs_and_runs_with_no_radio_at_all() {
    // The supported configuration on Windows and every other target with no
    // MeshLink implementation.
    let service = service(
        None,
        Some(Arc::new(OfflineConnector) as Arc<dyn RelayConnector>),
    );
    service.start().await.expect("start");
    assert!(matches!(
        service.send_room("hello"),
        Err(MeshError::NoRadio)
    ));
    assert!(matches!(service.announce_now(), Err(MeshError::NoRadio)));
    // The Nostr half still works.
    assert!(service.join_geohash("u4pruy").is_ok());
    service.shutdown().await;
}

#[tokio::test]
async fn it_constructs_and_runs_with_no_relay_connector() {
    let radio = MockLink::shared();
    let service = service(Some(Arc::clone(&radio) as Arc<dyn MeshLink>), None);
    service.start().await.expect("start");
    assert!(matches!(
        service.join_geohash("u4pruy"),
        Err(MeshError::NoRelayConnector)
    ));
    assert!(matches!(
        service.send_geohash("u4pruy", "hi"),
        Err(MeshError::NoRelayConnector)
    ));
    assert!(service.send_room("hello").is_ok());
    service.shutdown().await;
}

#[tokio::test]
async fn empty_messages_never_reach_the_wire() {
    let radio = MockLink::shared();
    let service = service(Some(Arc::clone(&radio) as Arc<dyn MeshLink>), None);
    assert!(matches!(
        service.send_room("   "),
        Err(MeshError::EmptyContent)
    ));
    assert!(matches!(
        service.send_room(""),
        Err(MeshError::EmptyContent)
    ));
    let peer = PeerId::from_str("bbbbbbbbbbbbbbbb");
    assert!(matches!(
        service.send_dm(&peer, "\n\t ").await,
        Err(MeshError::EmptyContent)
    ));
}

/// A file of `bytes` zero bytes, in a temp dir deleted when the test ends.
fn temp_file(name: &str, bytes: usize) -> std::path::PathBuf {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(name);
    std::fs::write(&path, vec![0x5A; bytes]).expect("write");
    crate::test_tempdir::hold(dir);
    path
}

/// A file of `bytes` length that occupies almost no disk.
///
/// The size refusal is made against `metadata().len()` precisely so that a
/// multi-gigabyte pick is refused without being read, which means a sparse
/// file exercises it exactly as a real one would — and a real 16 MiB temp
/// file per test run is 16 MiB of pointless IO.
fn sparse_file(name: &str, bytes: u64) -> std::path::PathBuf {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(name);
    std::fs::File::create(&path)
        .expect("create")
        .set_len(bytes)
        .expect("set_len");
    crate::test_tempdir::hold(dir);
    path
}

#[tokio::test]
async fn a_file_over_one_packet_is_chunked_rather_than_refused_or_truncated() {
    // The P9 size fork. A single `FileTransfer` packet above this size cannot
    // be reassembled inside `FRAGMENT_LIFETIME_SECS` at any goodput this
    // radio reaches — the assembly is evicted at 30 s measured from its first
    // fragment, whatever its progress — so sending one would spend minutes of
    // airtime on a packet that can never complete. Chunking is the only thing
    // that delivers it, and this is where that decision is made.
    let radio = MockLink::shared();
    let service = service(Some(Arc::clone(&radio) as Arc<dyn MeshLink>), None);

    let path = temp_file("chunk-me.bin", MAX_SINGLE_PACKET_FILE_BYTES + 1);
    assert!(
        service.send_file(&path).is_ok(),
        "above one packet's worth is streamed, not refused"
    );

    // Exactly at the threshold still rides one packet, which is what keeps
    // interop with real bitchat for everything small.
    let path = temp_file("one-packet.bin", MAX_SINGLE_PACKET_FILE_BYTES);
    assert!(service.send_file(&path).is_ok());
}

#[tokio::test]
async fn a_file_over_the_time_cap_is_refused_because_nobody_would_wait_for_it() {
    // Once nothing buffers the whole file, memory stops being the ceiling and
    // *time* becomes it. The refusal is against the declared length, before a
    // byte is read.
    let radio = MockLink::shared();
    let service = service(Some(Arc::clone(&radio) as Arc<dyn MeshLink>), None);

    let path = sparse_file("far-too-big.bin", MAX_STREAMED_FILE_BYTES as u64 + 1);
    match service.send_file(&path) {
        Err(MeshError::FileTooLarge { size, max }) => {
            assert_eq!(size, MAX_STREAMED_FILE_BYTES as u64 + 1);
            assert_eq!(max, MAX_STREAMED_FILE_BYTES);
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn the_transfer_estimate_is_honest_about_what_a_large_file_costs() {
    // P7's rule: never offer what the radio cannot deliver. A file that takes
    // twenty minutes should be presented as taking twenty minutes, and this is
    // the number a surface would show.
    let service = service(None, None);
    let (fast, slow) = service.file_transfer_estimate_secs(8 * 1024 * 1024);
    assert!(slow > fast, "the pessimistic figure is the slower one");
    assert!(
        fast > 10 * 60,
        "8 MiB is more than ten minutes even at the optimistic rate, got {fast}s"
    );

    // And a small file is seconds, so the estimate is not uniformly alarming.
    let (fast, _) = service.file_transfer_estimate_secs(4 * 1024);
    assert!(fast < 5, "a 4 KiB file is not a commitment, got {fast}s");
}

#[tokio::test]
async fn an_empty_file_never_reaches_the_wire() {
    // `BitchatFilePacket::decode` rejects an empty content TLV outright, so an
    // empty file is airtime spent on a packet nobody can accept.
    let radio = MockLink::shared();
    let service = service(Some(Arc::clone(&radio) as Arc<dyn MeshLink>), None);
    let path = temp_file("empty.bin", 0);
    assert!(matches!(
        service.send_file(&path),
        Err(MeshError::EmptyContent)
    ));
}

#[tokio::test]
async fn a_file_that_is_not_there_says_so_with_its_path() {
    // The two i/o failures this error covers are opposites — a send cannot
    // read what the user picked, a receive cannot write to the attachments
    // directory — and without the path the message cannot tell them apart.
    let radio = MockLink::shared();
    let service = service(Some(Arc::clone(&radio) as Arc<dyn MeshLink>), None);
    match service.send_file(std::path::Path::new("/nonexistent/nope.bin")) {
        Err(MeshError::FileIo { path, .. }) => {
            assert_eq!(path, "/nonexistent/nope.bin");
        }
        other => panic!("expected an i/o error naming the file, got {other:?}"),
    }
}

#[tokio::test]
async fn a_sent_file_lands_in_the_room_as_a_broadcast_and_not_as_sent() {
    // Same reason as `send_room`: nobody acknowledges a room file, so there is
    // no later event to correct the state with, and a message with no recorded
    // state falls through to the converter's one-to-one word "sent" — which
    // claims an arrival from a recipient the message never had.
    let radio = MockLink::shared();
    let service = service(Some(Arc::clone(&radio) as Arc<dyn MeshLink>), None);
    service.start().await.expect("start");
    let path = temp_file("note.bin", 32);

    service.send_file(&path).expect("queued");
    // The send is a queue submission; the runtime mirrors the row.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let history = service
        .history(&MeshConversation::Room, None, 50)
        .expect("history");
    assert_eq!(history.len(), 1, "the sender's own copy of the file");
    assert_eq!(
        history[0].delivery_status,
        MeshDelivery::Broadcast.as_str(),
        "a room file has no recipient to have been \"sent\" to"
    );
    assert_ne!(history[0].delivery_status, "sent");
    assert_eq!(
        mapping::attachments_of(&history[0]).len(),
        1,
        "the sender must be able to open what they just sent"
    );
    service.shutdown().await;
}

#[tokio::test]
async fn sending_a_file_with_no_radio_fails_instead_of_queueing_forever() {
    let service = service(None, None);
    let path = temp_file("note.bin", 32);
    assert!(matches!(service.send_file(&path), Err(MeshError::NoRadio)));
}

#[tokio::test]
async fn a_geohash_that_is_not_a_geohash_is_refused_before_it_is_published() {
    let service = service(
        None,
        Some(Arc::new(OfflineConnector) as Arc<dyn RelayConnector>),
    );
    assert!(matches!(
        service.join_geohash("not a geohash!"),
        Err(MeshError::InvalidGeohash(_))
    ));
    assert!(matches!(
        service.send_geohash("!!!", "hi"),
        Err(MeshError::InvalidGeohash(_))
    ));
}

#[tokio::test]
async fn a_dm_to_an_unroutable_peer_fails_instead_of_queueing_forever() {
    // No radio, no relay connector: nothing can carry anything.
    let service = service(None, None);
    let peer = PeerId::from_str("bbbbbbbbbbbbbbbb");
    assert!(matches!(
        service.send_dm(&peer, "hello").await,
        Err(MeshError::Unroutable(_))
    ));
}

#[tokio::test]
async fn a_dm_is_mirrored_into_the_timeline_before_it_is_routed() {
    let radio = MockLink::shared();
    let service = service(Some(Arc::clone(&radio) as Arc<dyn MeshLink>), None);
    let mut events = service.subscribe();
    let peer = PeerId::from_str("bbbbbbbbbbbbbbbb");

    let delivery = service.send_dm(&peer, "hello").await.expect("routed");
    // Nothing is connected, so nothing can carry it — and the router says so
    // rather than pretending.
    assert_eq!(delivery, MeshDelivery::Unreachable);

    match events.try_recv() {
        Ok(MeshEvent::Message(message)) => {
            assert!(message.is_local);
            assert_eq!(message.conversation, MeshConversation::Dm(peer.clone()));
            assert_eq!(
                message.message.conversation_type,
                ConversationType::MeshDm,
                "a 1:1 mesh conversation is MeshDm, not Mesh"
            );
            assert_eq!(mapping::text_of(&message.message), Some("hello"));
        }
        other => panic!("expected the local mirror first, got {other:?}"),
    }
}

#[tokio::test]
async fn a_sent_dm_is_on_disk_by_the_time_send_returns() {
    // No server echo will ever bring this message back, so the local copy is
    // the message — and it has to survive a restart mid-delivery.
    let radio = MockLink::shared();
    let service = service(Some(Arc::clone(&radio) as Arc<dyn MeshLink>), None);
    let peer = PeerId::from_str("bbbbbbbbbbbbbbbb");

    service.send_dm(&peer, "hello").await.expect("routed");

    let conversation = MeshConversation::Dm(peer.clone());
    let history = service.history(&conversation, None, 50).expect("history");
    assert_eq!(history.len(), 1);
    assert_eq!(mapping::text_of(&history[0]), Some("hello"));
    assert_eq!(history[0].conversation_type, ConversationType::MeshDm);
    // The state is the mesh's own, and it is durable: it used to live only
    // on the event stream, so reopening the conversation brought a message
    // back as a plain server-style "sent" no matter what had really happened
    // to it. Nothing here can carry the message, so it is waiting.
    assert_eq!(
        history[0].delivery_status,
        MeshDelivery::Unreachable.as_str(),
        "mesh delivery state must survive a reload, in the mesh's vocabulary"
    );
    assert_ne!(
        history[0].delivery_status, "sent",
        "the server's \"sent\" means a server accepted it, which never happened"
    );

    // Typing the same text twice is two messages, not a duplicate: each send
    // mints its own wire ID. Dedup keys on that ID, and a resend carrying an
    // ID we already stored collapses onto the existing row instead — see
    // `timeline::tests::the_same_message_arriving_three_ways_is_stored_once`.
    service.send_dm(&peer, "hello").await.expect("routed");
    let history = service.history(&conversation, None, 50).expect("history");
    assert_eq!(history.len(), 2);
}

#[tokio::test]
async fn the_public_room_is_a_conversation_before_anything_has_happened() {
    let service = service(None, None);
    let conversations = service.conversations().expect("conversations");
    assert_eq!(conversations.len(), 1, "just the public room");
    assert_eq!(conversations[0].conversation_type, ConversationType::Mesh);
    assert!(
        conversations[0].owner_id.is_none(),
        "no server account owns the mesh room"
    );
    assert!(conversations[0].my_role.is_empty());
}

#[tokio::test]
async fn a_dm_shows_up_in_the_sidebar_once_it_has_been_used() {
    let radio = MockLink::shared();
    let service = service(Some(Arc::clone(&radio) as Arc<dyn MeshLink>), None);
    let peer = PeerId::from_str("bbbbbbbbbbbbbbbb");
    service.send_dm(&peer, "hello").await.expect("routed");

    let conversations = service.conversations().expect("conversations");
    assert_eq!(conversations.len(), 2, "the room and the dm");
    let dm = conversations
        .iter()
        .find(|c| c.conversation_type == ConversationType::MeshDm)
        .expect("the dm");
    assert_eq!(
        dm.id,
        MeshConversation::Dm(peer).conversation_id().to_string()
    );
    assert_eq!(dm.unread_count, 0, "our own message is already read");
}

#[tokio::test]
async fn a_message_is_trimmed_before_it_is_sent() {
    let radio = MockLink::shared();
    let service = service(Some(Arc::clone(&radio) as Arc<dyn MeshLink>), None);
    let mut events = service.subscribe();
    let peer = PeerId::from_str("bbbbbbbbbbbbbbbb");
    service.send_dm(&peer, "  hello  ").await.expect("routed");
    match events.try_recv() {
        Ok(MeshEvent::Message(message)) => {
            assert_eq!(mapping::text_of(&message.message), Some("hello"));
        }
        other => panic!("expected a message, got {other:?}"),
    }
}

#[tokio::test]
async fn read_receipts_refuse_a_peer_no_transport_can_reach() {
    let service = service(None, None);
    let peer = PeerId::from_str("bbbbbbbbbbbbbbbb");
    assert!(matches!(
        service.mark_read(&peer, "MSG-1"),
        Err(MeshError::Unroutable(_))
    ));
}

#[tokio::test]
async fn the_local_identity_is_stable_and_self_consistent() {
    let service = service(None, None);
    assert_eq!(service.local_peer(), service.local_peer());
    assert_eq!(service.fingerprint().len(), 64, "SHA-256 as hex");
    assert!(service.local_peer().is_hex());
}

#[tokio::test]
async fn message_ids_are_unique_and_uppercase() {
    let a = new_message_id();
    let b = new_message_id();
    assert_ne!(a, b);
    assert_eq!(a, a.to_uppercase());
}

#[tokio::test]
async fn a_fresh_service_reports_nothing_connected() {
    let service = service(None, None);
    let status = service.status();
    assert!(!status.radio_running);
    assert_eq!(status.connected_links, 0);
    assert_eq!(status.connected_relays, 0);
    assert!(service.peers().is_empty());
    assert!(service.joined_geohashes().is_empty());
}
