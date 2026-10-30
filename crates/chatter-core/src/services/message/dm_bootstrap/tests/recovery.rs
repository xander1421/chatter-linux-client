use super::*;

/// F51: with neither side holding the other's mailbox, the higher-UUID
/// side must ask (send a REQUEST) rather than queue and wait for a peer
/// that has nothing to send. With an empty token pool, park before any
/// REQUEST submission; the independent retry can ask once tokens exist.
/// Never queue silently behind a bootstrap that has not been requested.
#[tokio::test]
async fn higher_uuid_without_a_peer_mailbox_waits_for_prefetched_tokens() {
    let me = UserId::from_uuid(uuid::Uuid::max());
    let friend = UserId::from_uuid(uuid::Uuid::from_u128(1));
    let (client, _dir) = shield_client(me);
    let ms = client.messages();
    cache_device(ms, &friend, 2, &mut StdRng::seed_from_u64(11)).await;
    cache_sender_certificate(ms, &me);
    let (mut ours, mut theirs) = (
        crate::mailbox::dm::ConversationKey::new(),
        crate::mailbox::dm::ConversationKey::new(),
    );
    ours.derive_shared_secret(theirs.our_ephemeral_public)
        .unwrap();
    theirs
        .derive_shared_secret(ours.our_ephemeral_public)
        .unwrap();
    seed_half_open(ms, &friend, &ours, 5);

    let sent = ms
        .send_friend_dm_content(&friend, crate::services::build_text_content("hi", 1))
        .await;
    assert!(
        matches!(sent, Err(MessageError::ShieldNotReady)),
        "got {sent:?}"
    );
    assert!(ms.pending_dms.drain(&friend).unwrap().is_empty());
}

/// F51 recovery: the lower-UUID side re-announces its live mailbox and
/// key (no token, no new mailbox), and the higher side — established on
/// that same key but missing the peer mailbox — takes it: it records the
/// mailbox for the announcing device and can send again. F53: a later
/// re-bootstrap with a new ephemeral (the lower side's mailbox expired)
/// replaces the working conversation's keys rather than being refused.
#[tokio::test]
async fn half_open_conversation_recovers_by_re_announce() {
    use crate::error::MailboxError;
    use crate::mailbox::dm::{ConversationKey, DmBootstrap, DmConversationManager};
    use prost::Message as _;

    let lower = UserId::from_uuid(uuid::Uuid::from_u128(1));
    let higher = UserId::from_uuid(uuid::Uuid::max());
    let (lower_client, _d1) = shield_client(lower);
    let (higher_client, _d2) = shield_client(higher);
    let (lo, hi) = (lower_client.messages(), higher_client.messages());

    let (mut lo_key, mut hi_key) = (ConversationKey::new(), ConversationKey::new());
    lo_key
        .derive_shared_secret(hi_key.our_ephemeral_public)
        .unwrap();
    hi_key
        .derive_shared_secret(lo_key.our_ephemeral_public)
        .unwrap();
    let lo_mailbox = seed_half_open(lo, &higher, &lo_key, 5);
    let hi_mailbox = seed_half_open(hi, &lower, &hi_key, 6);

    // Lower side: re-announce, with an empty token batch (a fresh mailbox would fail).
    let announced = DmBootstrap::new(lo.database.clone())
        .initiate_dm(higher, &mut lo.grpc_pool.anonymous_mailbox_client())
        .await
        .expect("re-announce needs no token and no RPC");
    assert_eq!(announced.our_mailbox.public_key, lo_mailbox);
    assert_eq!(announced.our_ephemeral_public, lo_key.our_ephemeral_public);
    let content =
        crate::proto::content_v1::Content::decode(announced.bootstrap_message.as_slice()).unwrap();
    let Some(crate::proto::content_v1::content::Content::KeyExchange(kx)) = content.content else {
        panic!("bootstrap MESSAGE is a KeyExchange");
    };

    // Higher side takes it from the lower side's device 3.
    let bootstrap = DmBootstrap::new(hi.database.clone());
    let response = bootstrap
        .handle_incoming_bootstrap(
            lower,
            3,
            &kx.ephemeral_pk,
            &mut hi.grpc_pool.anonymous_mailbox_client(),
        )
        .await
        .expect("accepted")
        .expect("answered with our mailbox");
    assert_eq!(response.our_mailbox_public_key, hi_mailbox);
    let conv = DmConversationManager::new(hi.database.clone())
        .load_conversation(&lower)
        .unwrap()
        .unwrap();
    assert!(conv.is_ready_to_send());
    let peer_mailbox = &conv.peer_mailboxes[0];
    assert_eq!(
        (peer_mailbox.public_key, peer_mailbox.device_id),
        (lo_mailbox, 3)
    );
    assert_eq!(
        conv.device_key(peer_mailbox).unwrap().send_key(),
        lo_key.recv_key()
    );

    // F53: the lower side's mailbox expires, so it re-bootstraps with a
    // fresh mailbox and ephemeral. The higher side still holds a working
    // conversation, and takes the new keys anyway: the sender is the
    // authenticated peer (the Signal layer refuses any other identity).
    let mut new_lo_key = ConversationKey::new();
    let new_lo_mailbox = [0x5Au8; 32];
    let (_, new_lo_encryption_pk) =
        crate::crypto::envelope_crypto::generate_mailbox_encryption_key();
    let rebootstrap = crate::mailbox::dm::bootstrap::MailboxAnnouncement {
        mailbox_pk: new_lo_mailbox,
        conversation_pk: new_lo_key.our_ephemeral_public,
        mailbox_encryption_pk: new_lo_encryption_pk,
    }
    .encode();
    let response = bootstrap
        .handle_incoming_bootstrap(
            lower,
            3,
            &rebootstrap,
            &mut hi.grpc_pool.anonymous_mailbox_client(),
        )
        .await
        .expect("re-bootstrap accepted")
        .expect("answered with our mailbox");
    assert_eq!(response.peer_mailbox_public_key, new_lo_mailbox);
    assert_eq!(response.peer_mailbox_encryption_key, new_lo_encryption_pk);
    new_lo_key
        .derive_shared_secret(response.our_ephemeral_public)
        .unwrap();
    let conv = DmConversationManager::new(hi.database.clone())
        .load_conversation(&lower)
        .unwrap()
        .unwrap();
    assert!(conv.is_ready_to_send());
    assert_eq!(
        conv.peer_mailboxes.len(),
        1,
        "device 3's half replaced, not added"
    );
    assert_eq!(conv.peer_mailboxes[0].public_key, new_lo_mailbox);
    let hi_key = conv.device_key(&conv.peer_mailboxes[0]).unwrap();
    assert_eq!(hi_key.send_key(), new_lo_key.recv_key());
    assert_ne!(
        hi_key.send_key(),
        lo_key.recv_key(),
        "the old secret is gone"
    );

    // A bootstrap from any other identity is filed under that identity:
    // it cannot touch the conversation with `lower`.
    let mallory = UserId::from_uuid(uuid::Uuid::from_u128(2));
    let hostile = crate::mailbox::dm::bootstrap::MailboxAnnouncement {
        mailbox_pk: [0x66u8; 32],
        conversation_pk: ConversationKey::new().our_ephemeral_public,
        mailbox_encryption_pk: [0x67u8; 32],
    }
    .encode();
    match bootstrap
        .handle_incoming_bootstrap(
            mallory,
            3,
            &hostile,
            &mut hi.grpc_pool.anonymous_mailbox_client(),
        )
        .await
    {
        // No conversation with mallory: needs a fresh mailbox, and the token batch is empty.
        Err(MailboxError::NoTokensAvailable) => {}
        other => panic!(
            "expected the empty token batch, got {:?}",
            other.map(|r| r.is_some())
        ),
    }
    let conv = DmConversationManager::new(hi.database.clone())
        .load_conversation(&lower)
        .unwrap()
        .unwrap();
    assert_eq!(conv.peer_mailboxes[0].public_key, new_lo_mailbox);
    assert_eq!(
        conv.device_key(&conv.peer_mailboxes[0]).unwrap().send_key(),
        new_lo_key.recv_key()
    );
}
