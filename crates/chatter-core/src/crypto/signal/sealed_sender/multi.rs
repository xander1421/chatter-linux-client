//! Sealed Sender v2 multi-recipient encryption for anonymous private-channel
//! sends (libsignal `sealed_sender_multi_recipient_encrypt`; Signal's group
//! send). One shared AES-GCM-SIV ciphertext of the USMC, plus per recipient
//! ACI a key wrapped to its identity key and its (device, registration id)
//! list; the server fans it out (`SendGroupMessage`).
//!
//! Chatter's server knows each channel's members, so the message must address
//! EVERY device of EVERY member, our own sending device included: a member
//! with one device missing would be the sender. libsignal takes each
//! destination's registration id from its session, and there is no session
//! with ourselves, so for our own device one is built here in memory from our
//! own prekey records (`initiate_session` on a throwaway store). It is
//! never persisted and never used to encrypt anything: multi-recipient
//! encryption reads only its registration id. The echo comes back to this
//! device and is dropped by the decrypt path's SealedSenderSelfSend check.

use std::time::SystemTime;

use async_trait::async_trait;
use libsignal_protocol::{
    Direction, GenericSignedPreKey, IdentityChange, IdentityKey, IdentityKeyPair,
    IdentityKeyStore, InMemSignalProtocolStore, KyberPreKeyStore, ProtocolAddress, SessionRecord,
    SessionStore, SignalProtocolError, SignedPreKeyStore, initiate_session,
    sealed_sender_multi_recipient_encrypt,
};

type SignalResult<T> = Result<T, SignalProtocolError>;
use rand::TryRngCore;
use rand::rngs::OsRng;

use crate::crypto::SignalStore;
use crate::error::CryptoError;

use super::{SealedSenderManager, UnidentifiedSenderMessageContent};

/// The real identity store, answering for our own address with our own key.
struct WithSelf<'a> {
    inner: &'a SignalStore,
    local: &'a ProtocolAddress,
    ours: IdentityKey,
}

#[async_trait(?Send)]
impl IdentityKeyStore for WithSelf<'_> {
    async fn get_identity_key_pair(&self) -> SignalResult<IdentityKeyPair> {
        self.inner.get_identity_key_pair().await
    }
    async fn get_local_registration_id(&self) -> SignalResult<u32> {
        self.inner.get_local_registration_id().await
    }
    async fn save_identity(&mut self, _: &ProtocolAddress, _: &IdentityKey) -> SignalResult<IdentityChange> {
        Err(SignalProtocolError::InvalidState(
            "multi_recipient",
            "read-only identity view".into(),
        ))
    }
    async fn is_trusted_identity(&self, a: &ProtocolAddress, k: &IdentityKey, d: Direction) -> SignalResult<bool> {
        if a == self.local {
            return Ok(*k == self.ours);
        }
        self.inner.is_trusted_identity(a, k, d).await
    }
    async fn get_identity(&self, a: &ProtocolAddress) -> SignalResult<Option<IdentityKey>> {
        if a == self.local {
            return Ok(Some(self.ours));
        }
        self.inner.get_identity(a).await
    }
}

fn protocol(e: impl std::fmt::Display) -> CryptoError {
    CryptoError::Protocol(e.to_string())
}

/// A session record with our own device, carrying our registration id, built
/// from our current signed and Kyber prekeys.
async fn self_session(
    store: &SignalStore,
    local: &ProtocolAddress,
    echo_registration: Option<u32>,
) -> Result<SessionRecord, CryptoError> {
    let identity = store.get_identity_key_pair().await.map_err(protocol)?;
    let registration_id = store.get_local_registration_id().await.map_err(protocol)?;
    let keys = store.key_store();
    let signed_id = keys.max_signed_prekey_id().map_err(CryptoError::Storage)?;
    let kyber_id = keys.max_kyber_prekey_id().map_err(CryptoError::Storage)?;
    let signed = store.get_signed_pre_key(signed_id.into()).await.map_err(protocol)?;
    let kyber = store.get_kyber_pre_key(kyber_id.into()).await.map_err(protocol)?;
    let bundle = libsignal_protocol::PreKeyBundle::new(
        echo_registration.unwrap_or(registration_id),
        local.device_id(),
        None,
        signed.id().map_err(protocol)?,
        signed.public_key().map_err(protocol)?,
        signed.signature().map_err(protocol)?,
        kyber.id().map_err(protocol)?,
        kyber.public_key().map_err(protocol)?,
        kyber.signature().map_err(protocol)?,
        *identity.identity_key(),
    )
    .map_err(protocol)?;
    let mut scratch = InMemSignalProtocolStore::new(identity, registration_id).map_err(protocol)?;
    let mut rng = OsRng.unwrap_err();
    initiate_session(
        local,
        local,
        &mut scratch.session_store,
        &mut scratch.identity_store,
        &bundle,
        SystemTime::now(),
        &mut rng,
    )
    .await
    .map_err(protocol)?;
    scratch
        .session_store
        .load_session(local)
        .await
        .map_err(protocol)?
        .ok_or_else(|| CryptoError::Protocol("self session not created".into()))
}

impl SealedSenderManager {
    /// Run the same PQXDH admission as a remote repair in a throwaway store.
    /// The posting device must retain its real identity and device keys.
    pub(crate) async fn validate_private_echo_bundle(
        store: &SignalStore,
        local: &ProtocolAddress,
        bundle: libsignal_protocol::PreKeyBundle,
    ) -> Result<u32, CryptoError> {
        let ours = store.get_identity_key_pair().await.map_err(protocol)?;
        if bundle.identity_key().map_err(protocol)? != ours.identity_key() {
            return Err(CryptoError::InvalidKeyFormat);
        }
        let registration = bundle.registration_id().map_err(protocol)?;
        let local_registration = store.get_local_registration_id().await.map_err(protocol)?;
        let mut scratch = InMemSignalProtocolStore::new(ours, local_registration).map_err(protocol)?;
        crate::crypto::SessionManager::establish_session(
            local, local, &mut scratch.session_store, &mut scratch.identity_store,
            &bundle, &mut OsRng.unwrap_err(),
        ).await?;
        Ok(registration)
    }

    /// Encrypt `usmc` to every destination (grouped by user, as libsignal
    /// expects), our own address included. A destination we hold no session
    /// with is `CryptoError::SessionNotFound`-style: establish one and retry.
    pub(crate) async fn encrypt_multi_recipient(
        usmc: &UnidentifiedSenderMessageContent,
        destinations: &[ProtocolAddress],
        local: &ProtocolAddress,
        store: &SignalStore,
        echo_registration: Option<u32>,
    ) -> Result<Vec<u8>, CryptoError> {
        let mut sorted: Vec<&ProtocolAddress> = destinations.iter().collect();
        sorted.sort_by(|a, b| (a.name(), u32::from(a.device_id())).cmp(&(b.name(), u32::from(b.device_id()))));
        sorted.dedup();
        let mut sessions = Vec::with_capacity(sorted.len());
        for d in &sorted {
            let record = if *d == local {
                self_session(store, local, echo_registration).await?
            } else {
                store
                    .load_session(d)
                    .await
                    .map_err(protocol)?
                    .ok_or_else(|| CryptoError::Protocol(format!("no session with {d}")))?
            };
            sessions.push(record);
        }
        let ours = *store.get_identity_key_pair().await.map_err(protocol)?.identity_key();
        let view = WithSelf { inner: store, local, ours };
        let session_refs: Vec<&SessionRecord> = sessions.iter().collect();
        let mut rng = OsRng.unwrap_err();
        sealed_sender_multi_recipient_encrypt(
            &sorted,
            &session_refs,
            std::iter::empty(),
            usmc,
            &view,
            &mut rng,
        )
        .await
        .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use libsignal_protocol::{
        CiphertextMessageType, ContentHint, DeviceId, KeyPair, SealedSenderV2SentMessage,
        SenderCertificate, ServerCertificate, Timestamp, sealed_sender_decrypt_to_usmc,
    };
    use rand::SeedableRng;

    use super::*;
    use crate::crypto::signal::{PreKeyManager, SessionManager};
    use crate::storage::{Database, KeyStore};

    struct Party {
        _dir: tempfile::TempDir,
        store: SignalStore,
        identity: IdentityKeyPair,
        address: ProtocolAddress,
        bundle: libsignal_protocol::PreKeyBundle,
        registration_id: u32,
    }

    fn party(name: &str, registration_id: u32, rng: &mut rand::rngs::StdRng) -> Party {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(&dir.path().join(format!("{name}.db")), "multi-recipient-test").unwrap());
        db.run_migrations().unwrap();
        let ks = Arc::new(KeyStore::new(db));
        let identity = IdentityKeyPair::generate(rng);
        ks.set_local_identity(registration_id, &identity.serialize()).unwrap();
        let pm = PreKeyManager::new(ks.clone());
        let signed = pm.generate_signed_prekey(rng, 2, &identity).unwrap();
        let kyber = pm.generate_kyber_prekey(3, &identity).unwrap();
        let bundle = pm
            .create_prekey_bundle(registration_id, 1, None, &signed, &kyber, *identity.identity_key())
            .unwrap();
        Party {
            _dir: dir,
            store: SignalStore::new(ks),
            identity,
            address: ProtocolAddress::new(uuid::Uuid::new_v4().to_string(), DeviceId::try_from(1u32).unwrap()),
            bundle,
            registration_id,
        }
    }

    fn cert(sender: &Party, rng: &mut rand::rngs::StdRng) -> SenderCertificate {
        let root = KeyPair::generate(rng);
        let signer = KeyPair::generate(rng);
        let server = ServerCertificate::new(1, signer.public_key, &root.private_key, rng).unwrap();
        SenderCertificate::new(
            sender.address.name().to_string(),
            None,
            *sender.identity.public_key(),
            sender.address.device_id(),
            Timestamp::from_epoch_millis(u64::MAX / 2),
            server,
            &signer.private_key,
            rng,
        )
        .unwrap()
    }

    /// Alice seals one sender-key message for Bob and for her own sending
    /// device; the SentMessage lists both with their real registration ids,
    /// and each recipient's ReceivedMessage opens to the same USMC, sent by
    /// Alice, of type SenderKey, for the channel.
    #[tokio::test]
    async fn seals_for_members_and_our_own_device() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(914);
        let mut alice = party("alice", 1111, &mut rng);
        let bob = party("bob", 2222, &mut rng);
        let mut ids = alice.store.clone();
        alice.store.key_store().mark_peer_verified(bob.address.name(), &bob.identity.identity_key().serialize()).unwrap();
        SessionManager::establish_session(&bob.address, &alice.address, &mut alice.store, &mut ids, &bob.bundle, &mut rng)
            .await
            .unwrap();

        let channel = b"channel-id-bytes".to_vec();
        let usmc = SealedSenderManager::create_content(
            CiphertextMessageType::SenderKey,
            cert(&alice, &mut rng),
            b"group-encrypted".to_vec(),
            ContentHint::Resendable,
            Some(channel.clone()),
        )
        .unwrap();
        let sent = SealedSenderManager::encrypt_multi_recipient(
            &usmc,
            &[bob.address.clone(), alice.address.clone()],
            &alice.address,
            &alice.store,
            None,
        )
        .await
        .unwrap();

        let parsed = SealedSenderV2SentMessage::parse(&sent).unwrap();
        assert_eq!(parsed.recipients.len(), 2, "Bob and Alice herself");
        for (party, store) in [(&bob, &bob.store), (&alice, &alice.store)] {
            let sid = libsignal_protocol::ServiceId::parse_from_service_id_string(party.address.name()).unwrap();
            let r = &parsed.recipients[&sid];
            assert_eq!(r.devices.len(), 1);
            assert_eq!(u32::from(r.devices[0].1), party.registration_id, "registration id from the session");
            let received = parsed.received_message_parts_for_recipient(r).as_ref().concat();
            let opened = sealed_sender_decrypt_to_usmc(&received, store).await.unwrap();
            assert_eq!(opened.msg_type().unwrap(), CiphertextMessageType::SenderKey);
            assert_eq!(opened.contents().unwrap(), b"group-encrypted");
            assert_eq!(opened.group_id().unwrap(), Some(channel.as_slice()));
            assert_eq!(opened.sender().unwrap().sender_uuid().unwrap(), alice.address.name());
        }
    }

    #[tokio::test]
    async fn refuses_a_member_without_a_session() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(915);
        let alice = party("alice", 1111, &mut rng);
        let carol = party("carol", 3333, &mut rng);
        let usmc = SealedSenderManager::create_content(
            CiphertextMessageType::SenderKey,
            cert(&alice, &mut rng),
            b"x".to_vec(),
            ContentHint::Resendable,
            None,
        )
        .unwrap();
        assert!(
            SealedSenderManager::encrypt_multi_recipient(&usmc, &[carol.address.clone()], &alice.address, &alice.store, None)
                .await
                .is_err()
        );
    }
}
