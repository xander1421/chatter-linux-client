use std::sync::Arc;
use std::sync::atomic::Ordering;

use libsignal_protocol::{DeviceId, IdentityKeyPair, KeyPair, PreKeyBundle, ProtocolAddress, kem};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use super::devices::bootstrap_device_order;
use crate::client::ChatterClient;
use crate::domain::ids::UserId;
use crate::services::message::{MessageError, MessageService};
use crate::storage::repositories::settings_repo::keys;

/// A client with a temp datastore, a local identity, and nothing behind
/// its server URL: any RPC fails, so an outcome other than a network
/// error proves none was made.
fn shield_client(me: UserId) -> (Arc<ChatterClient>, tempfile::TempDir) {
    static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
    MOCK_KEYRING.call_once(|| {
        keyring::set_default_store(crate::system::keyring::test_credential_store());
    });
    let dir = tempfile::tempdir().unwrap();
    let client = ChatterClient::builder()
        .server_url("https://localhost:1")
        .data_dir(dir.path().to_str().unwrap())
        .build()
        .expect("build client");
    let ms = client.messages();
    ms.settings_repo
        .set_string(keys::CURRENT_USER_ID, &me.to_string())
        .unwrap();
    ms.settings_repo
        .set(keys::CURRENT_DEVICE_ID, &1u32)
        .unwrap();
    let identity = IdentityKeyPair::generate(&mut StdRng::seed_from_u64(7));
    ms.signal_service
        .key_store
        .set_local_identity(42, &identity.serialize())
        .unwrap();
    ms.shield_mode.store(true, Ordering::Release);
    (client, dir)
}

/// Cache `peer`'s `device` the way a normal-mode send does: a session.
/// One identity per user, shared by all its devices.
async fn cache_device(
    ms: &MessageService,
    peer: &UserId,
    device: u32,
    rng: &mut StdRng,
) -> libsignal_protocol::InMemSignalProtocolStore {
    use libsignal_protocol::{GenericSignedPreKey, KyberPreKeyStore, SignedPreKeyStore};
    let peer_identity =
        IdentityKeyPair::generate(&mut StdRng::seed_from_u64(peer.as_uuid().as_u128() as u64));
    let signed = KeyPair::generate(rng);
    let kyber = kem::KeyPair::generate(kem::KeyType::Kyber1024, rng);
    let signed_sig = peer_identity
        .private_key()
        .calculate_signature(&signed.public_key.serialize(), rng)
        .unwrap();
    let kyber_sig = peer_identity
        .private_key()
        .calculate_signature(&kyber.public_key.serialize(), rng)
        .unwrap();
    let device_id = DeviceId::new(device as u8).unwrap();
    let registration = rng.random::<u32>() & 0x3fff;
    let signed_id = rng.random::<u32>().into();
    let kyber_id = rng.random::<u32>().into();
    let bundle = PreKeyBundle::new(
        registration,
        device_id,
        None,
        signed_id,
        signed.public_key,
        signed_sig.to_vec(),
        kyber_id,
        kyber.public_key.clone(),
        kyber_sig.to_vec(),
        *peer_identity.identity_key(),
    )
    .unwrap();
    let mut remote =
        libsignal_protocol::InMemSignalProtocolStore::new(peer_identity, registration).unwrap();
    let now = libsignal_protocol::Timestamp::from_epoch_millis(
        crate::util::time::TimestampNanos::now().as_u64() / 1_000_000,
    );
    remote
        .save_signed_pre_key(
            signed_id,
            &libsignal_protocol::SignedPreKeyRecord::new(signed_id, now, &signed, &signed_sig),
        )
        .await
        .unwrap();
    remote
        .save_kyber_pre_key(
            kyber_id,
            &libsignal_protocol::KyberPreKeyRecord::new(kyber_id, now, &kyber, &kyber_sig),
        )
        .await
        .unwrap();
    ms.signal_service
        .actor
        .establish_session(ProtocolAddress::new(peer.to_string(), device_id), bundle)
        .await
        .expect("session");
    remote
}

// Local-only protocol fixture: generates a real sender/server certificate
// under the repository's published development root. Never uses a keyring,
// deployment key, live server, device or credential.
#[cfg(feature = "dev-trust-root")]
fn fixture_sender_certificate(
    client: &ChatterClient,
    me: UserId,
    phone: Option<String>,
) -> libsignal_protocol::SenderCertificate {
    use base64::Engine as _;
    use libsignal_protocol::{PrivateKey, SenderCertificate, ServerCertificate, Timestamp};
    let ms = client.messages();
    let mut rng = StdRng::seed_from_u64(0xCA9);
    // Same public test fixture as sealed_sender_decrypt_labels_unratcheted_plaintext.
    let root = PrivateKey::deserialize(
        &base64::engine::general_purpose::STANDARD
            .decode("ELMk0o1RlhsHCk8vQ/bWbtRDeQvcC4PuN8+U1Xg90Ek=")
            .unwrap(),
    )
    .unwrap();
    let signer = KeyPair::generate(&mut rng);
    let server = ServerCertificate::new(1, signer.public_key, &root, &mut rng).unwrap();
    let (_, identity) = ms
        .signal_service
        .key_store
        .get_local_identity()
        .unwrap()
        .unwrap();
    let identity = IdentityKeyPair::try_from(identity.as_slice()).unwrap();
    let expiration = crate::util::time::TimestampNanos::now().as_u64() + 86_400_000_000_000;
    let cert = SenderCertificate::new(
        me.to_string(),
        phone,
        *identity.public_key(),
        DeviceId::new(1).unwrap(),
        Timestamp::from_epoch_millis(expiration / 1_000_000),
        server,
        &signer.private_key,
        &mut rng,
    )
    .unwrap();
    cert
}

#[cfg(feature = "dev-trust-root")]
fn cache_fixture_sender_certificate(
    client: &ChatterClient,
    me: UserId,
) -> libsignal_protocol::SenderCertificate {
    use base64::Engine as _;
    let ms = client.messages();
    let cert = fixture_sender_certificate(client, me, None);
    let expiration = cert.expiration().unwrap().epoch_millis() * 1_000_000;
    ms.settings_repo
        .set_string(
            keys::SENDER_CERTIFICATE,
            &base64::engine::general_purpose::STANDARD.encode(cert.serialized().unwrap()),
        )
        .unwrap();
    ms.settings_repo
        .set(keys::SENDER_CERTIFICATE_REFRESH_AFTER, &expiration)
        .unwrap();
    ms.settings_repo
        .set(keys::SENDER_CERTIFICATE_SERVER_KEY_ID, &1u32)
        .unwrap();
    cert
}

#[cfg(feature = "dev-trust-root")]
async fn open_fixture_sealed(
    remote: &mut libsignal_protocol::InMemSignalProtocolStore,
    alice: &ProtocolAddress,
    bob: &ProtocolAddress,
    envelope: &[u8],
) -> (Vec<u8>, libsignal_protocol::CiphertextMessageType) {
    use libsignal_protocol::{
        CiphertextMessage, CiphertextMessageType, PreKeySignalMessage, SignalMessage,
    };
    let usmc = libsignal_protocol::sealed_sender_decrypt_to_usmc(envelope, &remote.identity_store)
        .await
        .unwrap();
    let kind = usmc.msg_type().unwrap();
    let message = match kind {
        CiphertextMessageType::PreKey => CiphertextMessage::PreKeySignalMessage(
            PreKeySignalMessage::try_from(usmc.contents().unwrap()).unwrap(),
        ),
        CiphertextMessageType::Whisper => CiphertextMessage::SignalMessage(
            SignalMessage::try_from(usmc.contents().unwrap()).unwrap(),
        ),
        _ => panic!("unexpected sealed message type"),
    };
    let plain = libsignal_protocol::message_decrypt(
        &message,
        alice,
        bob,
        &mut remote.session_store,
        &mut remote.identity_store,
        &mut remote.pre_key_store,
        &remote.signed_pre_key_store,
        &mut remote.kyber_pre_key_store,
        &mut StdRng::seed_from_u64(17),
    )
    .await
    .unwrap();
    (plain, kind)
}

fn cache_sender_certificate(ms: &MessageService, me: &UserId) {
    use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
    use libsignal_protocol::{PrivateKey, SenderCertificate, ServerCertificate, Timestamp};
    let rng = &mut StdRng::seed_from_u64(0xCE27);
    let root = PrivateKey::deserialize(
        &BASE64
            .decode("ELMk0o1RlhsHCk8vQ/bWbtRDeQvcC4PuN8+U1Xg90Ek=")
            .unwrap(),
    )
    .unwrap();
    let server = KeyPair::generate(rng);
    let server_cert = ServerCertificate::new(1, server.public_key, &root, rng).unwrap();
    let identity = IdentityKeyPair::generate(&mut StdRng::seed_from_u64(7));
    let far_future = Timestamp::from_epoch_millis(u64::MAX / 2);
    let cert = SenderCertificate::new(
        me.to_string(),
        None,
        *identity.public_key(),
        DeviceId::new(1).unwrap(),
        far_future,
        server_cert,
        &server.private_key,
        rng,
    )
    .unwrap();
    ms.settings_repo
        .set_string(
            keys::SENDER_CERTIFICATE,
            &BASE64.encode(cert.serialized().unwrap()),
        )
        .unwrap();
    ms.settings_repo
        .set(keys::SENDER_CERTIFICATE_REFRESH_AFTER, &(u64::MAX / 2))
        .unwrap();
}

/// Our side of a conversation whose peer mailbox is gone (expired, or
/// dropped as by v069): a live mailbox of ours and an established key,
/// nothing to send to. Returns our mailbox public key.
fn seed_half_open(
    ms: &MessageService,
    peer: &UserId,
    key: &crate::mailbox::dm::ConversationKey,
    pk: u8,
) -> [u8; 32] {
    let mailbox = crate::mailbox::dm::DmMailbox {
        id: format!("mb-{pk}"),
        public_key: [pk; 32],
        private_key: [pk; 64],
        encryption_private_key: [pk; 32],
        signature: vec![],
        token_id: format!("tok-{pk}"),
        created_at: chrono::Utc::now(),
        expires_at: chrono::Utc::now() + chrono::Duration::hours(40),
        last_polled_at: None,
    };
    // The mailbox's token, already spent: the batch stays empty.
    let conn = ms.database.connection().unwrap();
    conn.execute(
        "INSERT OR IGNORE INTO mailbox_server_keys (key_id, public_key_der, valid_until, created_at) VALUES ('k', x'00', 0, '')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO mailbox_tokens (id, ephemeral_public_key, ephemeral_private_key, signature, server_key_id, created_at, used_at)
         VALUES (?1, ?2, x'00', x'00', 'k', '', 'spent')",
        rusqlite::params![mailbox.token_id, mailbox.public_key.as_slice()],
    )
    .unwrap();
    drop(conn);
    let conversations = crate::mailbox::dm::DmConversationManager::new(ms.database.clone());
    conversations.save_our_mailbox(peer, &mailbox).unwrap();
    conversations.save_conversation_key(peer, key).unwrap();
    mailbox.public_key
}

/// `n` claimable pre-fetched tokens.
fn seed_tokens(ms: &MessageService, n: usize) {
    let conn = ms.database.connection().unwrap();
    conn.execute(
        "INSERT OR IGNORE INTO mailbox_server_keys (key_id, public_key_der, valid_until, created_at) VALUES ('live', x'00', ?1, '')",
        [i64::MAX],
    )
    .unwrap();
    for i in 0..n {
        conn.execute(
            "INSERT INTO mailbox_tokens (id, ephemeral_public_key, ephemeral_private_key, signature, server_key_id, created_at)
             VALUES (?1, ?2, ?3, x'00', 'live', ?4)",
            rusqlite::params![format!("t{i}"), [0xA0 + i as u8; 32].as_slice(), [0xA0 + i as u8; 64].as_slice(), format!("{i:04}")],
        )
        .unwrap();
    }
}

mod fanout;
mod recovery;
#[cfg(feature = "dev-trust-root")]
mod sealed;
mod shield;
