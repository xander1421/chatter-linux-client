use super::*;

// -------------------------------------------------------- durability

#[test]
fn nothing_plaintext_reaches_storage() {
    let storage = Arc::new(MemoryStorage::new());
    let mut store = OutboxStore::new(storage.clone(), Arc::new(TestSealer::new()));
    store.load().expect("cold load");
    let secret = "meet me at the usual place";
    assert!(store
        .enqueue(&peer("aa"), QueuedMessage::new(secret, "nick", "m1", 100))
        .is_none());
    store.persist().expect("persist");

    let blob = storage
        .load(StorageKey::Outbox)
        .expect("load")
        .expect("something was written");
    assert!(
        !blob.windows(secret.len()).any(|w| w == secret.as_bytes()),
        "the sealed blob must not contain message plaintext"
    );
}

#[test]
fn persistence_round_trips_every_field() {
    let storage = Arc::new(MemoryStorage::new());
    let sealer = Arc::new(TestSealer::new());
    let p = peer("aa");

    let mut original = QueuedMessage::new("hello ☃", "nick ☃", "m1", 1_700_000_000);
    original.send_attempts = 4;
    original.deposited_courier_keys.insert(key(7));
    original.deposited_courier_keys.insert(key(9));

    let mut store = OutboxStore::new(storage.clone(), sealer.clone());
    store.load().expect("cold load");
    assert!(store.enqueue(&p, original.clone()).is_none());
    store.persist().expect("persist");

    let mut reloaded = OutboxStore::new(storage.clone(), sealer);
    assert!(reloaded.load().expect("reload").is_empty());
    assert_eq!(reloaded.queued(&p, "m1"), Some(&original));
}

#[test]
fn persist_is_free_when_nothing_changed() {
    let (_storage, mut store) = new_store();
    assert!(!store.is_dirty());
    store.persist().expect("persist");
    assert!(store.enqueue(&peer("aa"), message("m1", 100)).is_none());
    assert!(store.is_dirty());
    store.persist().expect("persist");
    assert!(!store.is_dirty());
}

#[test]
fn an_emptied_outbox_wipes_its_blob() {
    let (storage, mut store) = new_store();
    assert!(store.enqueue(&peer("aa"), message("m1", 100)).is_none());
    store.persist().expect("persist");
    assert!(storage.load(StorageKey::Outbox).expect("load").is_some());

    assert!(
        store
            .mark_acked(&peer("aa"), "m1", AckKind::Delivered)
            .cleared
    );
    store.persist().expect("persist");
    assert_eq!(
        storage.load(StorageKey::Outbox).expect("load"),
        None,
        "an empty outbox leaves nothing behind"
    );
}

#[test]
fn an_unreadable_store_is_never_overwritten() {
    let storage = Arc::new(BlindStorage::new());
    let mut store = OutboxStore::new(storage.clone(), Arc::new(TestSealer::new()));

    assert!(store.load().is_err(), "storage could not be read");
    assert!(!store.is_loaded());
    assert!(store.enqueue(&peer("aa"), message("m1", 100)).is_none());
    store.persist().expect("persist defers rather than failing");
    assert_eq!(storage.writes(), 0, "a deferred write must not clobber");
    assert!(store.is_dirty(), "the write is owed, not lost");
}

#[test]
fn a_locked_sealer_defers_instead_of_starting_empty() {
    let storage = Arc::new(MemoryStorage::new());
    // Something durable exists, but the key is not available yet.
    storage
        .save(StorageKey::Outbox, b"sealed-by-someone-else")
        .expect("save");

    let mut store = OutboxStore::new(storage.clone(), Arc::new(LockedSealer));
    assert!(store.load().is_err());
    assert!(!store.is_loaded());
    assert!(store.enqueue(&peer("aa"), message("m1", 100)).is_none());
    store.persist().expect("persist defers");
    assert_eq!(
        storage.load(StorageKey::Outbox).expect("load"),
        Some(b"sealed-by-someone-else".to_vec()),
        "unreadable mail must survive the locked window"
    );
}

#[test]
fn recovery_after_unlock_merges_and_flushes_the_owed_write() {
    // The whole cold-locked-launch story in one test: a durable queue exists,
    // the wake cannot read it, work is accepted anyway, and the unlock has to
    // end with both halves on disk.
    let storage = Arc::new(MemoryStorage::new());
    let p = peer("aa");

    let mut writer = OutboxStore::new(storage.clone(), Arc::new(TestSealer::new()));
    writer.load().expect("load");
    assert!(writer.enqueue(&p, message("durable", 100)).is_none());
    writer.persist().expect("persist");
    let durable_blob = storage.load(StorageKey::Outbox).expect("load");

    let sealer = Arc::new(UnlockableSealer::locked());
    let mut store = OutboxStore::new(storage.clone(), sealer.clone());
    assert!(store.load().is_err(), "the key is not available yet");
    assert!(store.enqueue(&p, message("local", 200)).is_none());
    store.persist().expect("a deferred write is not a failure");
    assert_eq!(
        storage.load(StorageKey::Outbox).expect("load"),
        durable_blob,
        "the unreadable blob must not be overwritten"
    );

    sealer.unlock();
    assert!(store.load().expect("recovery load").is_empty());
    assert!(store.is_loaded());
    assert!(!store.is_dirty(), "the owed write lands with the recovery");

    let mut verifier = OutboxStore::new(storage, Arc::new(TestSealer::new()));
    assert!(verifier.load().expect("load").is_empty());
    let ids: Vec<&str> = verifier
        .queue(&p)
        .iter()
        .map(|m| m.message_id.as_str())
        .collect();
    assert_eq!(
        ids,
        vec!["durable", "local"],
        "recovery merges; replacing would discard the locked-window work"
    );
}

#[test]
fn load_merges_when_called_again() {
    let storage = Arc::new(MemoryStorage::new());
    let sealer = Arc::new(TestSealer::new());
    let p = peer("aa");

    let mut writer = OutboxStore::new(storage.clone(), sealer.clone());
    writer.load().expect("load");
    assert!(writer.enqueue(&p, message("durable", 100)).is_none());
    writer.persist().expect("persist");

    let mut store = OutboxStore::new(storage.clone(), sealer);
    assert!(store.load().expect("first load").is_empty());
    assert_eq!(store.queue(&p).len(), 1);
    // A second load must be idempotent, not a duplicator.
    assert!(store.load().expect("second load").is_empty());
    assert_eq!(store.queue(&p).len(), 1);
}

#[test]
fn a_corrupt_blob_starts_empty_and_is_replaceable() {
    let storage = Arc::new(MemoryStorage::new());
    storage
        .save(StorageKey::Outbox, b"\x00not sealed by us")
        .expect("save");

    let mut store = OutboxStore::new(storage.clone(), Arc::new(TestSealer::new()));
    assert!(
        store
            .load()
            .expect("a corrupt blob is not fatal")
            .is_empty(),
        "losing retained mail is bad; refusing to start is worse"
    );
    assert!(store.is_loaded());

    assert!(store.enqueue(&peer("aa"), message("m1", 100)).is_none());
    store.persist().expect("persist");
    let mut reloaded = OutboxStore::new(storage, Arc::new(TestSealer::new()));
    assert!(reloaded.load().expect("load").is_empty());
    assert_eq!(reloaded.queue(&peer("aa")).len(), 1);
}

#[test]
fn a_truncated_blob_is_malformed_not_a_panic() {
    let full = encode(&{
        let mut snapshot = Snapshot::new();
        snapshot.insert(peer("aa"), vec![message("m1", 100)]);
        snapshot
    })
    .expect("encode");

    for cut in 0..full.len() {
        let truncated = full.get(..cut).expect("in range");
        assert!(
            decode(truncated).is_err(),
            "a blob cut at {cut} must be rejected"
        );
    }
    assert!(decode(&full).is_ok());
}

#[test]
fn a_foreign_blob_is_rejected() {
    assert!(decode(b"XXX\x01").is_err(), "wrong magic");
    assert!(decode(b"OBX\x02").is_err(), "wrong version");
}

#[test]
fn wipe_leaves_nothing_behind() {
    let (storage, mut store) = new_store();
    assert!(store.enqueue(&peer("aa"), message("m1", 100)).is_none());
    store.persist().expect("persist");
    assert!(store.mark_acked(&peer("aa"), "m1", AckKind::Read).cleared);
    assert!(store.enqueue(&peer("aa"), message("m2", 100)).is_none());

    store.wipe().expect("wipe");
    assert!(store.is_empty());
    assert_eq!(store.ack_state("m1"), None, "acked IDs are residue too");
    assert_eq!(storage.load(StorageKey::Outbox).expect("load"), None);
}

#[test]
fn iteration_covers_every_peer() {
    let (_storage, mut store) = new_store();
    assert!(store.enqueue(&peer("aa"), message("m1", 100)).is_none());
    assert!(store.enqueue(&peer("bb"), message("m2", 100)).is_none());
    assert!(store.enqueue(&peer("bb"), message("m3", 100)).is_none());

    let mut peers = store.peers();
    peers.sort();
    assert_eq!(peers, vec![peer("aa"), peer("bb")]);
    let total: usize = store.iter().map(|(_, q)| q.len()).sum();
    assert_eq!(total, 3);
    assert_eq!(store.len(), 3);
}
