//! Sender-key lifecycle between two real `SignalService`s (Alice sends, Bob
//! receives), pinning the libsignal semantics the channel send path relies on.
//!
//! - An SKDM carries the sender's CURRENT iteration, so it unlocks only
//!   messages encrypted after it was built — the reason the send path hands
//!   the key to new members BEFORE encrypting (`services/message/group.rs`,
//!   `distribute_sender_key_to_new_members`).
//! - Processing the same SKDM twice is harmless.
//! - Rotating OUR key must not delete the keys other members sent us.

use std::sync::Arc;
use tempfile::NamedTempFile;

use chatter_core::domain::ids::{ChannelId, UserId};
use chatter_core::services::SignalService;
use chatter_core::storage::{settings_keys, Database, KeyStore, SettingsRepository};

struct Party {
    svc: SignalService,
    user_id: UserId,
    _db: NamedTempFile,
}

fn party() -> Party {
    let db_file = NamedTempFile::new().unwrap();
    let db = Arc::new(Database::open(db_file.path(), "test_passphrase").unwrap());
    db.run_migrations().unwrap();
    let key_store = Arc::new(KeyStore::new(db.clone()));
    let settings = Arc::new(SettingsRepository::new(db));
    let user_id = UserId::new();
    settings.set(settings_keys::CURRENT_USER_ID, &user_id.to_string()).unwrap();
    settings.set(settings_keys::CURRENT_DEVICE_ID, &1u32).unwrap();
    Party { svc: SignalService::new(key_store, settings), user_id, _db: db_file }
}

async fn send(from: &Party, channel: &ChannelId, body: &[u8]) -> chatter_core::proto::content_v1::GroupEncrypted {
    from.svc.encrypt_for_group(channel, body).await.unwrap().0
}

#[tokio::test]
async fn skdm_unlocks_only_messages_sent_after_it() {
    let (alice, bob) = (party(), party());
    let channel = ChannelId::new();

    // Alice's first send creates her key; its SKDM never reaches Bob (the
    // device failure mode: "dev in channel", then "tokenless one").
    let m0 = send(&alice, &channel, b"m0").await;
    let m1 = send(&alice, &channel, b"m1").await;

    // Bob asks; Alice answers with an SKDM built NOW — at iteration 2.
    let skdm = alice.svc.create_sender_key(&channel).await.unwrap();
    bob.svc.process_sender_key(&alice.user_id, 1, &skdm).await.unwrap();

    for (m, iter) in [(&m0, 0), (&m1, 1)] {
        let err = bob.svc.decrypt_from_group(&alice.user_id, 1, m).await.unwrap_err();
        assert!(
            err.to_string().contains(&format!("old counter 2 / {iter}")),
            "pre-SKDM message must be permanently undecryptable, got: {err}"
        );
    }

    // Everything after the SKDM decrypts.
    let m2 = send(&alice, &channel, b"m2").await;
    assert_eq!(bob.svc.decrypt_from_group(&alice.user_id, 1, &m2).await.unwrap(), b"m2");
}

#[tokio::test]
async fn skdm_sent_before_the_message_makes_it_decryptable() {
    let (alice, bob) = (party(), party());
    let channel = ChannelId::new();

    // Alice already has a key and has used it (earlier members read m0).
    let _m0 = send(&alice, &channel, b"m0").await;

    // Bob joins: the send path hands him the current key BEFORE encrypting.
    let skdm = alice.svc.create_sender_key(&channel).await.unwrap();
    bob.svc.process_sender_key(&alice.user_id, 1, &skdm).await.unwrap();
    let m1 = send(&alice, &channel, b"m1").await;
    assert_eq!(bob.svc.decrypt_from_group(&alice.user_id, 1, &m1).await.unwrap(), b"m1");
}

#[tokio::test]
async fn duplicate_skdm_is_a_no_op() {
    let (alice, bob) = (party(), party());
    let channel = ChannelId::new();

    let skdm = alice.svc.create_sender_key(&channel).await.unwrap();
    bob.svc.process_sender_key(&alice.user_id, 1, &skdm).await.unwrap();
    let m0 = send(&alice, &channel, b"m0").await;
    let m1 = send(&alice, &channel, b"m1").await;
    assert_eq!(bob.svc.decrypt_from_group(&alice.user_id, 1, &m1).await.unwrap(), b"m1");

    // The same SKDM again (19 answers to 19 requests on device) keeps Bob's
    // advanced chain and its skipped key for m0 — libsignal
    // `add_sender_key_state` keeps an existing state with the same chain id.
    bob.svc.process_sender_key(&alice.user_id, 1, &skdm).await.unwrap();
    assert_eq!(bob.svc.decrypt_from_group(&alice.user_id, 1, &m0).await.unwrap(), b"m0");

    // A later SKDM of the same chain (at a higher iteration) is also a no-op.
    let later = alice.svc.create_sender_key(&channel).await.unwrap();
    bob.svc.process_sender_key(&alice.user_id, 1, &later).await.unwrap();
    let m2 = send(&alice, &channel, b"m2").await;
    assert_eq!(bob.svc.decrypt_from_group(&alice.user_id, 1, &m2).await.unwrap(), b"m2");
}

#[tokio::test]
async fn rotating_our_key_keeps_the_keys_members_sent_us() {
    let (alice, bob) = (party(), party());
    let channel = ChannelId::new();

    let skdm = alice.svc.create_sender_key(&channel).await.unwrap();
    bob.svc.process_sender_key(&alice.user_id, 1, &skdm).await.unwrap();

    // Bob has his own key, handed to Alice; then rotates it (member left).
    let _ = send(&bob, &channel, b"bob0").await;
    bob.svc.mark_sender_key_delivered(&channel, &alice.user_id.to_string()).unwrap();
    let old_bob_skdm = bob.svc.create_sender_key(&channel).await.unwrap();
    let new_bob_skdm = bob.svc.rotate_sender_key(&channel).await.unwrap();
    assert_ne!(old_bob_skdm.distribution_message, new_bob_skdm.distribution_message);

    // A fresh key has reached nobody yet.
    assert!(bob.svc.sender_key_recipients(&channel).unwrap().is_empty());

    // Alice's chain did not rotate, so Bob still reads her.
    let m = send(&alice, &channel, b"after bob rotated").await;
    assert_eq!(
        bob.svc.decrypt_from_group(&alice.user_id, 1, &m).await.unwrap(),
        b"after bob rotated"
    );
}
