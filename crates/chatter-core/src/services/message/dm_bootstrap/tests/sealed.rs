use super::*;

#[cfg(feature = "dev-trust-root")]
#[tokio::test]
async fn actual_sealed_batch_checks_fresh_kyber_and_established_devices_before_commit() {
    use crate::crypto::signal::store::StagedSignalStore;
    use crate::crypto::{SealedSenderManager, SignalStore};
    use crate::services::gateway::submission::track_submission;
    use libsignal_protocol::CiphertextMessageType;
    let me = UserId::from_uuid(uuid::Uuid::from_u128(1));
    let friend = UserId::from_uuid(uuid::Uuid::max());
    let (client, _dir) = shield_client(me);
    let ms = client.messages();
    let mut rng = StdRng::seed_from_u64(0xC500);
    let mut remote = cache_device(ms, &friend, 1, &mut rng).await;
    cache_device(ms, &friend, 2, &mut rng).await;
    let cert = cache_fixture_sender_certificate(&client, me);
    let alice = ProtocolAddress::new(me.to_string(), DeviceId::new(1).unwrap());
    let bob = ProtocolAddress::new(friend.to_string(), DeviceId::new(1).unwrap());
    let fresh = ProtocolAddress::new(friend.to_string(), DeviceId::new(2).unwrap());
    // Establish device 1 with actual encrypted request/reply; device 2 keeps
    // its unacknowledged fresh Kyber session. No transport or service runs.
    let first = ms
        .signal_service
        .actor
        .encrypt_sealed_bounded_batch(
            crate::network::validation::protovalidate::test_plaintext("handshake"),
            cert.clone(),
            vec![bob.clone()],
            None,
        )
        .await
        .unwrap();
    let (plain, kind) = open_fixture_sealed(&mut remote, &alice, &bob, &first[0].1).await;
    assert_eq!(
        plain,
        crate::network::validation::protovalidate::test_plaintext("handshake")
    );
    assert_eq!(kind, CiphertextMessageType::PreKey);
    let reply = libsignal_protocol::message_encrypt(
        b"ack",
        &alice,
        &bob,
        &mut remote.session_store,
        &mut remote.identity_store,
        std::time::SystemTime::now(),
        &mut rng,
    )
    .await
    .unwrap();
    assert_eq!(
        ms.signal_service
            .actor
            .decrypt_identified(reply.serialize().to_vec(), bob.clone())
            .await
            .unwrap(),
        b"ack"
    );
    let base = SignalStore::new(ms.signal_service.key_store.clone());
    let before = ms
        .signal_service
        .key_store
        .list_sessions_for_user(&friend.to_string())
        .unwrap();
    // Compare actual libsignal envelopes from independent disposable
    // overlays; find a body fitting device 1 but not fresh Kyber device 2.
    let mut near_cap = None;
    for size in (6000..8000).step_by(128) {
        let content = crate::services::build_data_message_content(&"N".repeat(size), vec![], 1);
        let bytes = prost::Message::encode_to_vec(&content);
        let mut lengths = vec![];
        for address in [&bob, &fresh] {
            let mut stage = StagedSignalStore::new(base.clone());
            let mut identities = stage.clone();
            lengths.push(
                SealedSenderManager::encrypt(&bytes, &cert, address, &mut stage, &mut identities)
                    .await
                    .unwrap()
                    .len(),
            );
        }
        if lengths[0] <= 8192 && lengths[1] > 8192 {
            near_cap = Some(bytes);
            break;
        }
    }
    let bytes =
        near_cap.expect("established and fresh Kyber wrappers must exercise mixed cap admission");
    let (result, started, _) = track_submission(
        ms.signal_service.actor.encrypt_sealed_bounded_batch(
            bytes,
            cert.clone(),
            vec![bob.clone(), fresh],
            None,
        ),
        None,
    )
    .await;
    assert!(matches!(
        result,
        Err(crate::error::ServiceError::Validation(_))
    ));
    assert!(
        !started,
        "later oversized device must not admit earlier device"
    );
    assert_eq!(
        before,
        ms.signal_service
            .key_store
            .list_sessions_for_user(&friend.to_string())
            .unwrap()
    );
    // A legitimate 6000-byte Content on the established session still
    // encrypts, decrypts byte-exact and marks the original future boundary.
    let content = crate::services::build_data_message_content(&"界".repeat(2000), vec![], 1);
    let bytes = prost::Message::encode_to_vec(&content);
    let (result, started, _) = track_submission(
        ms.signal_service.actor.encrypt_sealed_bounded_batch(
            bytes.clone(),
            cert,
            vec![bob.clone()],
            None,
        ),
        None,
    )
    .await;
    assert!(started);
    let accepted = result.unwrap();
    assert!(accepted[0].1.len() <= 8192);
    let (plain, kind) = open_fixture_sealed(&mut remote, &alice, &bob, &accepted[0].1).await;
    assert_eq!(plain, bytes);
    assert_eq!(kind, CiphertextMessageType::Whisper);
}

/// Reach the literal server cap with a real wrapped session and variable
/// serialized certificate, rather than testing a guessed plaintext limit.
#[cfg(feature = "dev-trust-root")]
#[tokio::test]
async fn actual_sealed_envelope_exact_cap_is_admitted_and_cap_plus_one_is_not() {
    use crate::crypto::signal::store::StagedSignalStore;
    use crate::crypto::{SealedSenderManager, SignalStore};
    use crate::services::gateway::submission::track_submission;
    let me = UserId::from_uuid(uuid::Uuid::from_u128(1));
    let friend = UserId::from_uuid(uuid::Uuid::max());
    let (client, _dir) = shield_client(me);
    let ms = client.messages();
    cache_device(ms, &friend, 1, &mut StdRng::seed_from_u64(0x8192)).await;
    let address = ProtocolAddress::new(friend.to_string(), DeviceId::new(1).unwrap());
    let base = SignalStore::new(ms.signal_service.key_store.clone());
    let small = crate::network::validation::protovalidate::test_plaintext("small");
    for target in [8192, 8193] {
        let before = ms
            .signal_service
            .key_store
            .list_sessions_for_user(&friend.to_string())
            .unwrap();
        let baseline = {
            let cert = fixture_sender_certificate(&client, me, None);
            let mut staged = StagedSignalStore::new(base.clone());
            let mut identities = staged.clone();
            SealedSenderManager::encrypt(&small, &cert, &address, &mut staged, &mut identities)
                .await
                .unwrap()
                .len()
        };
        let gap = target - baseline;
        let mut exact = None;
        // Certificate length prefixes add a few bytes; vary the actual
        // signed fixture instead of inventing an overhead allowance.
        for extra in gap.saturating_sub(32)..=gap {
            let cert = fixture_sender_certificate(&client, me, Some("1".repeat(extra)));
            let mut staged = StagedSignalStore::new(base.clone());
            let mut identities = staged.clone();
            let wrapped =
                SealedSenderManager::encrypt(&small, &cert, &address, &mut staged, &mut identities)
                    .await
                    .unwrap();
            if wrapped.len() == target {
                exact = Some(cert);
                break;
            }
        }
        let cert = exact.expect("actual envelope must hit the exact cap boundary");
        let (result, started, _) = track_submission(
            ms.signal_service.actor.encrypt_sealed_bounded_batch(
                small.clone(),
                cert,
                vec![address.clone()],
                None,
            ),
            None,
        )
        .await;
        if target == 8192 {
            assert!(started);
            assert_eq!(result.unwrap()[0].1.len(), 8192);
        } else {
            assert!(!started);
            assert!(matches!(
                result,
                Err(crate::error::ServiceError::Validation(_))
            ));
            assert_eq!(
                before,
                ms.signal_service
                    .key_store
                    .list_sessions_for_user(&friend.to_string())
                    .unwrap()
            );
        }
    }
}

/// Public send, real SQLite admission, actual PQXDH sealed envelope and
/// sender certificate: near-cap refusal must leave BOTH ratchets intact.
#[cfg(feature = "dev-trust-root")]
#[tokio::test]
async fn public_sealed_envelope_refusal_releases_admission_without_advancing_devices() {
    use crate::state::ActiveConversation;
    let me = UserId::from_uuid(uuid::Uuid::from_u128(1));
    let friend = UserId::from_uuid(uuid::Uuid::max());
    let (client, _dir) = shield_client(me);
    let ms = client.messages();
    ms.shield_mode.store(false, Ordering::Release);
    client.auth_state().set_logged_in(me, 1, "me".into());
    let mut rng = StdRng::seed_from_u64(0xC49);
    for device in [1, 2] {
        cache_device(ms, &friend, device, &mut rng).await;
    }
    ms.signal_service
        .unrestricted_access
        .lock()
        .unwrap()
        .insert(friend.to_string(), false);
    cache_fixture_sender_certificate(&client, me);
    let before = ms
        .signal_service
        .key_store
        .list_sessions_for_user(&friend.to_string())
        .unwrap();
    let conversation = ActiveConversation::ServerDm { peer_id: friend };
    // Below 8192 in encoded Content, but the real fresh Kyber wrapper is over.
    let content = ms
        .server_dm_content(&friend, &"N".repeat(8000), vec![], 1, None)
        .unwrap()
        .0;
    assert!(prost::Message::encoded_len(&content) < 8192);
    let refused = client
        .send_text(conversation, &"N".repeat(8000))
        .await
        .unwrap();
    assert!(matches!(
        refused.send_error,
        Some(MessageError::InvalidContent(_))
    ));
    assert_eq!(refused.message.unwrap().delivery_status, "failed");
    client.ensure_send_available(conversation).unwrap();
    let after = ms
        .signal_service
        .key_store
        .list_sessions_for_user(&friend.to_string())
        .unwrap();
    assert_eq!(
        before, after,
        "no device ratchet may advance on size refusal"
    );
    // Small real encryption is admitted. Its deliberately offline deposit
    // loses confirmation; the outbox keeps the exact envelope to replay.
    let small = client
        .send_text(conversation, "small after cap refusal")
        .await
        .unwrap();
    assert!(small.send_error.is_none());
    let small = small.message.unwrap();
    assert_eq!(small.delivery_status, "sending");
    assert_ne!(
        before,
        ms.signal_service
            .key_store
            .list_sessions_for_user(&friend.to_string())
            .unwrap()
    );
    let envelope = |id: &crate::domain::ids::MessageId| -> Option<Vec<u8>> {
        ms.database
            .read_connection()
            .unwrap()
            .query_row(
                "SELECT request FROM dm_outbox WHERE message_id=?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .unwrap()
    };
    let retained = envelope(&small.id).expect("exact envelope stored before the deposit");
    // A replayable envelope is not a reason to hold the conversation: a
    // new send is a new message, and the queued one is never re-encrypted.
    client.ensure_send_available(conversation).unwrap();
    let another = client.send_text(conversation, "another").await.unwrap();
    assert_eq!(another.message.unwrap().delivery_status, "sending");
    assert_eq!(envelope(&small.id), Some(retained));
}
