//! Receive-only write overlay. All clones share CAS observations and mutations.
use super::*;
use crate::storage::receive_artifact::{Observed, ReceiveChanges};
use libsignal_protocol::DeviceId;
use std::{cell::RefCell, rc::Rc};
#[derive(Clone)]
pub(crate) struct ReceiveStore {
    base: SignalStore,
    changes: Rc<RefCell<ReceiveChanges>>,
}
impl ReceiveStore {
    pub(crate) fn new(base: SignalStore) -> Self {
        Self {
            base,
            changes: Default::default(),
        }
    }
    pub(crate) fn changes(&self) -> ReceiveChanges {
        self.changes.borrow().clone()
    }
    fn observe(
        &self,
        table: &'static str,
        key: &str,
        column: &'static str,
    ) -> Result<Option<Vec<u8>>> {
        let value = self
            .base
            .key_store
            .receive_read(table, key, column)
            .map_err(storage_to_signal_error)?;
        let mut c = self.changes.borrow_mut();
        if !c
            .observed
            .iter()
            .any(|o| o.table == table && o.key == key && o.column == column)
        {
            c.observed.push(Observed {
                table,
                key: key.into(),
                column,
                value: value.clone(),
            });
        }
        Ok(value)
    }
    fn read(
        &self,
        table: &'static str,
        key: &str,
        column: &'static str,
    ) -> Result<Option<Vec<u8>>> {
        {
            let c = self.changes.borrow();
            let staged = match table {
                "sessions" => c
                    .sessions
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| Some(v.clone())),
                "remote_identities" => c
                    .identities
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| Some(v.clone())),
                "prekeys" => c
                    .prekeys
                    .iter()
                    .find(|(k, _)| k.to_string() == key)
                    .map(|(_, v)| v.clone()),
                "signed_prekeys" => c
                    .signed
                    .iter()
                    .find(|(k, _)| k.to_string() == key)
                    .map(|(_, v)| Some(v.clone())),
                "kyber_prekeys" => c
                    .kyber
                    .iter()
                    .find(|(k, _)| k.to_string() == key)
                    .map(|(_, v)| Some(v.clone())),
                _ => None,
            };
            if let Some(v) = staged {
                return Ok(v);
            }
        }
        self.observe(table, key, column)
    }
}
#[async_trait(?Send)]
impl IdentityKeyStore for ReceiveStore {
    async fn get_identity_key_pair(&self) -> Result<IdentityKeyPair> {
        let bytes = self
            .observe("local_identity", "1", "identity_key_pair")?
            .ok_or(SignalProtocolError::InvalidState(
                "receive",
                "no local identity".into(),
            ))?;
        IdentityKeyPair::try_from(bytes.as_slice())
    }
    async fn get_local_registration_id(&self) -> Result<u32> {
        let bytes = self
            .observe("local_identity", "1", "CAST(registration_id AS BLOB)")?
            .ok_or(SignalProtocolError::InvalidState(
                "receive",
                "no local registration".into(),
            ))?;
        std::str::from_utf8(&bytes)
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or(SignalProtocolError::InvalidProtobufEncoding)
    }
    async fn get_identity(&self, a: &ProtocolAddress) -> Result<Option<IdentityKey>> {
        let owner = self.observe("settings", "auth.current_user_id", "CAST(value AS BLOB)")?;
        if super::local_account_matches(owner.as_deref(), a.name())? {
            return Ok(Some(*self.get_identity_key_pair().await?.identity_key()));
        }
        self.read("remote_identities", a.name(), "identity_key")?
            .map(|v| IdentityKey::try_from(v.as_slice()))
            .transpose()
    }
    async fn is_trusted_identity(
        &self,
        a: &ProtocolAddress,
        key: &IdentityKey,
        _: Direction,
    ) -> Result<bool> {
        Ok(self.get_identity(a).await?.is_none_or(|old| old == *key))
    }
    async fn save_identity(
        &mut self,
        a: &ProtocolAddress,
        key: &IdentityKey,
    ) -> Result<IdentityChange> {
        if self.get_identity(a).await?.is_some_and(|old| old != *key) {
            return Err(SignalProtocolError::UntrustedIdentity(a.clone()));
        }
        self.observe("remote_identities", a.name(), "identity_key")?;
        let mut c = self.changes.borrow_mut();
        if let Some((_, old)) = c.identities.iter_mut().find(|(k, _)| k == a.name()) {
            *old = key.serialize().to_vec();
        } else {
            c.identities
                .push((a.name().into(), key.serialize().to_vec()));
        }
        Ok(IdentityChange::from_changed(false))
    }
}
#[async_trait(?Send)]
impl SessionStore for ReceiveStore {
    async fn load_session(&self, a: &ProtocolAddress) -> Result<Option<SessionRecord>> {
        self.read("sessions", &address_to_string(a), "record")?
            .map(|v| SessionRecord::deserialize(&v))
            .transpose()
    }
    async fn store_session(&mut self, a: &ProtocolAddress, r: &SessionRecord) -> Result<()> {
        let key = address_to_string(a);
        self.observe("sessions", &key, "record")?;
        let bytes = r.serialize()?;
        let mut c = self.changes.borrow_mut();
        if let Some((_, old)) = c.sessions.iter_mut().find(|(k, _)| *k == key) {
            *old = bytes;
        } else {
            c.sessions.push((key, bytes));
        }
        Ok(())
    }
}
#[async_trait(?Send)]
impl PreKeyStore for ReceiveStore {
    async fn get_pre_key(&self, id: PreKeyId) -> Result<PreKeyRecord> {
        let id: u32 = id.into();
        let bytes = self
            .read("prekeys", &id.to_string(), "record")?
            .ok_or(SignalProtocolError::InvalidPreKeyId)?;
        PreKeyRecord::deserialize(&bytes)
    }
    async fn save_pre_key(&mut self, id: PreKeyId, record: &PreKeyRecord) -> Result<()> {
        let id: u32 = id.into();
        self.observe("prekeys", &id.to_string(), "record")?;
        let bytes = Some(record.serialize()?);
        let mut c = self.changes.borrow_mut();
        if let Some((_, old)) = c.prekeys.iter_mut().find(|(k, _)| *k == id) {
            *old = bytes;
        } else {
            c.prekeys.push((id, bytes));
        }
        Ok(())
    }
    async fn remove_pre_key(&mut self, id: PreKeyId) -> Result<()> {
        let id: u32 = id.into();
        self.observe("prekeys", &id.to_string(), "record")?;
        let mut c = self.changes.borrow_mut();
        if let Some((_, old)) = c.prekeys.iter_mut().find(|(k, _)| *k == id) {
            *old = None;
        } else {
            c.prekeys.push((id, None));
        }
        Ok(())
    }
}
#[async_trait(?Send)]
impl SignedPreKeyStore for ReceiveStore {
    async fn get_signed_pre_key(&self, id: SignedPreKeyId) -> Result<SignedPreKeyRecord> {
        let id: u32 = id.into();
        let bytes = self
            .read("signed_prekeys", &id.to_string(), "record")?
            .ok_or(SignalProtocolError::InvalidSignedPreKeyId)?;
        SignedPreKeyRecord::deserialize(&bytes)
    }
    async fn save_signed_pre_key(
        &mut self,
        id: SignedPreKeyId,
        record: &SignedPreKeyRecord,
    ) -> Result<()> {
        let id: u32 = id.into();
        self.observe("signed_prekeys", &id.to_string(), "record")?;
        let bytes = record.serialize()?;
        let mut c = self.changes.borrow_mut();
        if let Some((_, old)) = c.signed.iter_mut().find(|(k, _)| *k == id) {
            *old = bytes;
        } else {
            c.signed.push((id, bytes));
        }
        Ok(())
    }
}
#[async_trait(?Send)]
impl KyberPreKeyStore for ReceiveStore {
    async fn get_kyber_pre_key(&self, id: KyberPreKeyId) -> Result<KyberPreKeyRecord> {
        let id: u32 = id.into();
        let bytes = self
            .read("kyber_prekeys", &id.to_string(), "record")?
            .ok_or(SignalProtocolError::InvalidKyberPreKeyId)?;
        KyberPreKeyRecord::deserialize(&bytes)
    }
    async fn save_kyber_pre_key(
        &mut self,
        id: KyberPreKeyId,
        record: &KyberPreKeyRecord,
    ) -> Result<()> {
        let id: u32 = id.into();
        self.observe("kyber_prekeys", &id.to_string(), "record")?;
        let bytes = record.serialize()?;
        let mut c = self.changes.borrow_mut();
        if let Some((_, old)) = c.kyber.iter_mut().find(|(k, _)| *k == id) {
            *old = bytes;
        } else {
            c.kyber.push((id, bytes));
        }
        Ok(())
    }
    async fn mark_kyber_pre_key_used(
        &mut self,
        id: KyberPreKeyId,
        ec: SignedPreKeyId,
        base: &PublicKey,
    ) -> Result<()> {
        let id: u32 = id.into();
        let ec: u32 = ec.into();
        let bytes = base.serialize().to_vec();
        self.observe("kyber_prekeys", &id.to_string(), "record")?;
        if self
            .base
            .key_store
            .receive_kyber_used(id, ec, &bytes)
            .map_err(storage_to_signal_error)?
            || self
                .changes
                .borrow()
                .usage
                .iter()
                .any(|(k, e, b)| *k == id && *e == ec && b == &bytes)
        {
            return Err(SignalProtocolError::InvalidMessage(
                CiphertextMessageType::PreKey,
                "reused base key".into(),
            ));
        }
        self.changes.borrow_mut().usage.push((id, ec, bytes));
        Ok(())
    }
}

fn reaction_content(
    content: &crate::proto::content_v1::Content,
    depth: usize,
) -> std::result::Result<bool, crate::error::ServiceError> {
    use crate::proto::content_v1::content::Content;
    if depth > 4 {
        return Err(crate::error::ServiceError::Validation(
            "anonymous receive wrapper depth exceeds bound".into(),
        ));
    }
    match content.content.as_ref() {
        Some(Content::Reaction(_)) => Ok(true),
        Some(Content::AnonDmMessage(m)) => match m.inner_content.as_deref() {
            Some(c) => reaction_content(c, depth + 1),
            None => Ok(false),
        },
        Some(Content::AnonGroupMessage(m)) => match m.inner_content.as_deref() {
            Some(c) => reaction_content(c, depth + 1),
            None => Ok(false),
        },
        _ => Ok(false),
    }
}

fn from_artifact(
    a: crate::storage::receive_artifact::ReceiveArtifact,
) -> std::result::Result<crate::services::signal::actor::DurableReceive, crate::error::ServiceError>
{
    let device = DeviceId::try_from(a.sender_device).map_err(|_| {
        crate::error::ServiceError::Validation("invalid artifact sender device".into())
    })?;
    let msg_type = match a.message_type {
        2 => CiphertextMessageType::PreKey,
        3 => CiphertextMessageType::Whisper,
        _ => {
            return Err(crate::error::ServiceError::Validation(
                "invalid artifact message type".into(),
            ));
        }
    };
    Ok(crate::services::signal::actor::DurableReceive {
        address: ProtocolAddress::new(a.sender.clone(), device),
        content: a.content.clone(),
        msg_type,
        artifact: Some(a),
    })
}
pub(crate) async fn decrypt_durable(
    base: SignalStore,
    ciphertext: &[u8],
    root: &PublicKey,
    key: crate::storage::receive_artifact::ReceiveKey,
) -> std::result::Result<
    crate::services::signal::actor::DurableReceive,
    crate::services::signal::actor::DurableReceiveError,
> {
    use crate::services::signal::actor::DurableReceiveError;
    match decrypt_durable_inner(base, ciphertext, root, key).await {
        Ok(Some(received)) => Ok(received),
        Ok(None) => Err(DurableReceiveError::PlaintextRejected),
        Err(error) => Err(DurableReceiveError::Failure(error)),
    }
}

async fn decrypt_durable_inner(
    base: SignalStore,
    ciphertext: &[u8],
    root: &PublicKey,
    key: crate::storage::receive_artifact::ReceiveKey,
) -> std::result::Result<
    Option<crate::services::signal::actor::DurableReceive>,
    crate::error::ServiceError,
> {
    use crate::error::{CryptoError, ServiceError};
    use crate::storage::receive_artifact::{self, MAX_RECEIVE_CONTENT, ReceiveArtifact};
    use prost::Message;
    key.validate()?;
    // Matches internal/services/mailbox/handlers/handlers.go: MaxMailboxEnvelopeSize.
    // The reaction journal has a smaller retention bound, not a transport bound.
    const MAX_MAILBOX_ENVELOPE: usize = 1024 * 1024;
    if ciphertext.is_empty() || ciphertext.len() > MAX_MAILBOX_ENVELOPE {
        return Err(ServiceError::Validation(
            "receive envelope exceeds bound".into(),
        ));
    }
    let digest = receive_artifact::envelope_digest(ciphertext);
    if let Some(a) = base.key_store.receive_artifact(&key, &digest)? {
        return from_artifact(a).map(Some);
    }
    // Capture admission before staged crypto; full capacity never commits a reaction.
    let capacity = base.key_store.check_receive_capacity();
    let mut identities = ReceiveStore::new(base.clone());
    let mut sessions = identities.clone();
    let mut prekeys = identities.clone();
    let signed = identities.clone();
    let mut kyber = identities.clone();
    let local_device = DeviceId::try_from(key.local_device)
        .map_err(|_| ServiceError::Validation("invalid receive local device".into()))?;
    let (result, msg_type) = crate::crypto::signal::SealedSenderManager::decrypt(
        ciphertext,
        root,
        libsignal_protocol::Timestamp::from_epoch_millis(key.server_timestamp_ns / 1_000_000),
        None,
        key.local_account.clone(),
        local_device,
        &mut identities,
        &mut sessions,
        &mut prekeys,
        &signed,
        &mut kyber,
    )
    .await
    .map_err(ServiceError::Crypto)?;
    if msg_type == CiphertextMessageType::Plaintext {
        return Ok(None);
    }
    if result.message.len() > MAX_MAILBOX_ENVELOPE {
        return Err(ServiceError::Validation(
            "decrypted receive content exceeds bound".into(),
        ));
    }
    let decoded = crate::proto::content_v1::Content::decode(result.message.as_slice()).ok();
    let is_reaction = match decoded.as_ref() {
        Some(c) => reaction_content(c, 0)?,
        None => false,
    };
    let artifact = if is_reaction {
        // Refuse oversized reaction evidence before committing any ratchet writes.
        // Bootstrap/profile content is routed normally and is never journaled.
        if result.message.len() > MAX_RECEIVE_CONTENT {
            return Err(ServiceError::Validation(
                "decrypted reaction content exceeds journal bound".into(),
            ));
        }
        capacity?;
        let message_type = match msg_type {
            CiphertextMessageType::PreKey => 2,
            CiphertextMessageType::Whisper => 3,
            _ => {
                return Err(ServiceError::Crypto(CryptoError::Protocol(
                    "unsupported durable reaction cipher type".into(),
                )));
            }
        };
        Some(ReceiveArtifact::new(
            key.clone(),
            digest,
            result.sender_uuid.clone(),
            u32::from(result.device_id),
            result.message.clone(),
            message_type,
        )?)
    } else {
        None
    };
    let committed =
        base.key_store
            .commit_receive(&key, &identities.changes(), artifact.as_ref())?;
    if let Some(a) = committed {
        return from_artifact(a).map(Some);
    }
    Ok(Some(crate::services::signal::actor::DurableReceive {
        address: ProtocolAddress::new(result.sender_uuid, result.device_id),
        content: result.message,
        msg_type,
        artifact: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::signal::{PreKeyManager, SealedSenderManager, SessionManager};
    use crate::storage::{
        Database,
        receive_artifact::{ReceiveKey, envelope_digest},
    };
    use libsignal_protocol::{
        InMemSignalProtocolStore, KeyPair, SenderCertificate, ServerCertificate, Timestamp,
    };
    use prost::Message;
    use rand::{SeedableRng, TryRngCore};
    struct Fixture {
        _dir: tempfile::TempDir,
        path: std::path::PathBuf,
        db: Arc<Database>,
        store: SignalStore,
        sender: InMemSignalProtocolStore,
        sender_address: ProtocolAddress,
        recipient: ProtocolAddress,
        cert: SenderCertificate,
        root: PublicKey,
        key: ReceiveKey,
        body: Vec<u8>,
    }
    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receive.db");
        let db = Arc::new(Database::open(&path, "fixture-receive-passphrase").unwrap());
        db.run_migrations().unwrap();
        let ks = Arc::new(KeyStore::new(db.clone()));
        let mut rng = rand::rngs::StdRng::seed_from_u64(85);
        let local = IdentityKeyPair::generate(&mut rng);
        let sender_identity = IdentityKeyPair::generate(&mut rng);
        let account = uuid::Uuid::new_v4().to_string();
        let sender_id = uuid::Uuid::new_v4().to_string();
        ks.set_local_identity(1234, &local.serialize()).unwrap();
        {
            let conn = db.connection().unwrap();
            conn.execute(
                "INSERT INTO settings(key,value,updated_at) VALUES(?1,?2,'now')",
                rusqlite::params![
                    crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
                    serde_json::to_string(&account).unwrap()
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO settings(key,value,updated_at) VALUES(?1,'1','now')",
                [crate::storage::repositories::settings_repo::keys::CURRENT_DEVICE_ID],
            )
            .unwrap();
        }
        let pm = PreKeyManager::new(ks.clone());
        let pre = pm.generate_prekey(&mut rng, 1).unwrap();
        let signed = pm.generate_signed_prekey(&mut rng, 2, &local).unwrap();
        let kyber = pm.generate_kyber_prekey(3, &local).unwrap();
        let bundle = pm
            .create_prekey_bundle(1234, 1, Some(&pre), &signed, &kyber, *local.identity_key())
            .unwrap();
        let recipient = ProtocolAddress::new(account.clone(), DeviceId::try_from(1u32).unwrap());
        let sender_address =
            ProtocolAddress::new(sender_id.clone(), DeviceId::try_from(1u32).unwrap());
        let mut sender = InMemSignalProtocolStore::new(sender_identity, 5678).unwrap();
        SessionManager::establish_session(
            &recipient,
            &sender_address,
            &mut sender.session_store,
            &mut sender.identity_store,
            &bundle,
            &mut rng,
        )
        .await
        .unwrap();
        let root = KeyPair::generate(&mut rng);
        let signer = KeyPair::generate(&mut rng);
        let server =
            ServerCertificate::new(1, signer.public_key, &root.private_key, &mut rng).unwrap();
        let now = crate::util::time::TimestampNanos::now().as_u64();
        let cert = SenderCertificate::new(
            sender_id,
            None,
            *sender_identity.public_key(),
            DeviceId::try_from(1u32).unwrap(),
            Timestamp::from_epoch_millis(now / 1_000_000 + 3_600_000),
            server,
            &signer.private_key,
            &mut rng,
        )
        .unwrap();
        let content = crate::proto::content_v1::Content {
            content: Some(crate::proto::content_v1::content::Content::Reaction(
                crate::proto::content_v1::Reaction {
                    emoji: "👍".into(),
                    remove: false,
                    target_author_aci: account.clone(),
                    target_sent_timestamp: 10,
                    timestamp: 11,
                },
            )),
            timestamp: 11,
            ..Default::default()
        };
        Fixture {
            _dir: dir,
            path,
            db,
            store: SignalStore::new(ks),
            sender,
            sender_address,
            recipient,
            cert,
            root: root.public_key,
            key: ReceiveKey {
                local_account: account,
                local_device: 1,
                transport: "anon_dm".into(),
                scope: uuid::Uuid::new_v4().to_string(),
                envelope_id: "first".into(),
                server_timestamp_ns: now,
            },
            body: content.encode_to_vec(),
        }
    }
    async fn encrypt(f: &mut Fixture) -> Vec<u8> {
        SealedSenderManager::encrypt(
            &f.body,
            &f.cert,
            &f.recipient,
            &mut f.sender.session_store,
            &mut f.sender.identity_store,
        )
        .await
        .unwrap()
    }
    fn snapshot(db: &Database) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
        let conn = db.connection().unwrap();
        [
            "local_identity",
            "remote_identities",
            "sessions",
            "prekeys",
            "signed_prekeys",
            "kyber_prekeys",
            "kyber_prekey_usage",
            "reaction_receive_artifacts",
        ]
        .iter()
        .map(|table| {
            let mut stmt = conn
                .prepare(&format!("SELECT * FROM {table} ORDER BY 1"))
                .unwrap();
            let n = stmt.column_count();
            let result = stmt
                .query_map([], |r| {
                    (0..n)
                        .map(|i| r.get(i))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            result
        })
        .collect()
    }
    fn fail(db: &Database, table: &str, operation: &str) {
        db.connection().unwrap().execute_batch(&format!("CREATE TRIGGER receive_fail BEFORE {operation} ON {table} BEGIN SELECT RAISE(ABORT,'fixture receive failure'); END;")).unwrap();
    }
    #[tokio::test]
    async fn actual_fresh_receive_each_commit_failure_rolls_back_then_replays_after_reopen() {
        for (table, operation) in [
            ("remote_identities", "INSERT"),
            ("kyber_prekey_usage", "INSERT"),
            ("prekeys", "DELETE"),
            ("sessions", "INSERT"),
            ("reaction_receive_artifacts", "INSERT"),
        ] {
            let mut f = fixture().await;
            let ciphertext = encrypt(&mut f).await;
            let before = snapshot(&f.db);
            fail(&f.db, table, operation);
            assert!(
                decrypt_durable(f.store.clone(), &ciphertext, &f.root, f.key.clone())
                    .await
                    .is_err(),
                "failure at {table}"
            );
            assert_eq!(snapshot(&f.db), before, "partial receive commit at {table}");
            f.db.connection()
                .unwrap()
                .execute_batch("DROP TRIGGER receive_fail")
                .unwrap();
            let received = decrypt_durable(f.store.clone(), &ciphertext, &f.root, f.key.clone())
                .await
                .unwrap();
            assert_eq!(received.content, f.body);
            assert_eq!(received.msg_type, CiphertextMessageType::PreKey);
            let artifact = received.artifact.unwrap();
            assert!(!artifact.applied);
            assert!(f.store.key_store.get_prekey(1).unwrap().is_none());
            assert_eq!(
                f.db.connection()
                    .unwrap()
                    .query_row("SELECT count(*) FROM kyber_prekey_usage", [], |r| r
                        .get::<_, i64>(0))
                    .unwrap(),
                1
            );
            let committed = snapshot(&f.db);
            // New SQLite/key-store handle models lost actor response/process restart.
            let reopened = Arc::new(Database::open(&f.path, "fixture-receive-passphrase").unwrap());
            let replay = decrypt_durable(
                SignalStore::new(Arc::new(KeyStore::new(reopened.clone()))),
                &ciphertext,
                &f.root,
                f.key.clone(),
            )
            .await
            .unwrap();
            assert_eq!(replay.artifact, Some(artifact.clone()));
            assert_eq!(replay.content, f.body);
            assert_eq!(snapshot(&reopened), committed);
            let mut conflict = f.key.clone();
            conflict.server_timestamp_ns += 1;
            assert!(
                decrypt_durable(f.store.clone(), &ciphertext, &f.root, conflict)
                    .await
                    .is_err()
            );
            let mut altered = ciphertext.clone();
            altered[0] ^= 1;
            assert!(
                decrypt_durable(f.store.clone(), &altered, &f.root, f.key.clone())
                    .await
                    .is_err()
            );
            assert_eq!(snapshot(&f.db), committed);
        }
    }
    async fn establish(f: &mut Fixture) {
        let first = encrypt(f).await;
        decrypt_durable(f.store.clone(), &first, &f.root, f.key.clone())
            .await
            .unwrap();
        // Actual receiver reply acknowledges the PreKey session at the sender.
        let reply = libsignal_protocol::message_encrypt(
            b"session reply",
            &f.sender_address,
            &f.recipient,
            &mut f.store.clone(),
            &mut f.store.clone(),
            std::time::SystemTime::now(),
            &mut rand::rngs::OsRng.unwrap_err(),
        )
        .await
        .unwrap();
        libsignal_protocol::message_decrypt(
            &reply,
            &f.recipient,
            &f.sender_address,
            &mut f.sender.session_store,
            &mut f.sender.identity_store,
            &mut f.sender.pre_key_store,
            &f.sender.signed_pre_key_store,
            &mut f.sender.kyber_pre_key_store,
            &mut rand::rngs::OsRng.unwrap_err(),
        )
        .await
        .unwrap();
        f.key.envelope_id = "established".into();
    }
    #[tokio::test]
    async fn actual_established_receive_failures_and_committed_replay_do_not_advance_twice() {
        for table in [
            "remote_identities",
            "sessions",
            "reaction_receive_artifacts",
        ] {
            let mut f = fixture().await;
            establish(&mut f).await;
            let ciphertext = encrypt(&mut f).await;
            let before = snapshot(&f.db);
            fail(&f.db, table, "INSERT");
            assert!(
                decrypt_durable(f.store.clone(), &ciphertext, &f.root, f.key.clone())
                    .await
                    .is_err()
            );
            assert_eq!(snapshot(&f.db), before);
            f.db.connection()
                .unwrap()
                .execute_batch("DROP TRIGGER receive_fail")
                .unwrap();
            let result = decrypt_durable(f.store.clone(), &ciphertext, &f.root, f.key.clone())
                .await
                .unwrap();
            assert_eq!(result.msg_type, CiphertextMessageType::Whisper);
            let committed = snapshot(&f.db);
            let reopened = Arc::new(Database::open(&f.path, "fixture-receive-passphrase").unwrap());
            let replay = decrypt_durable(
                SignalStore::new(Arc::new(KeyStore::new(reopened.clone()))),
                &ciphertext,
                &f.root,
                f.key.clone(),
            )
            .await
            .unwrap();
            assert_eq!(replay.artifact, result.artifact);
            assert_eq!(snapshot(&reopened), committed);
        }
    }
    #[tokio::test]
    async fn receive_base_state_cas_and_saved_prekey_traits_are_atomic() {
        let f = fixture().await;
        let mut stage = ReceiveStore::new(f.store.clone());
        let pre = stage.get_pre_key(PreKeyId::from(1)).await.unwrap();
        let signed = stage
            .get_signed_pre_key(SignedPreKeyId::from(2))
            .await
            .unwrap();
        let kyber = stage
            .get_kyber_pre_key(KyberPreKeyId::from(3))
            .await
            .unwrap();
        stage.save_pre_key(PreKeyId::from(1), &pre).await.unwrap();
        stage
            .save_signed_pre_key(SignedPreKeyId::from(2), &signed)
            .await
            .unwrap();
        stage
            .save_kyber_pre_key(KyberPreKeyId::from(3), &kyber)
            .await
            .unwrap();
        let a = crate::storage::receive_artifact::ReceiveArtifact::new(
            f.key.clone(),
            envelope_digest(b"fixture envelope"),
            f.sender_address.name().into(),
            1,
            f.body.clone(),
            2,
        )
        .unwrap();
        for table in ["prekeys", "signed_prekeys", "kyber_prekeys"] {
            let before = snapshot(&f.db);
            fail(&f.db, table, "INSERT");
            assert!(
                f.store
                    .key_store
                    .commit_receive(&f.key, &stage.changes(), Some(&a))
                    .is_err()
            );
            assert_eq!(snapshot(&f.db), before);
            f.db.connection()
                .unwrap()
                .execute_batch("DROP TRIGGER receive_fail")
                .unwrap();
        }
        f.store
            .key_store
            .save_prekey(1, b"concurrent replacement")
            .unwrap();
        let after = snapshot(&f.db);
        assert!(
            f.store
                .key_store
                .commit_receive(&f.key, &stage.changes(), Some(&a))
                .is_err()
        );
        assert_eq!(snapshot(&f.db), after);
    }
    #[tokio::test]
    async fn wrapped_reaction_applied_receipt_replay_and_backup_exclusion() {
        let mut f = fixture().await;
        let inner = crate::proto::content_v1::Content::decode(f.body.as_slice()).unwrap();
        f.body = crate::proto::content_v1::Content {
            content: Some(crate::proto::content_v1::content::Content::AnonDmMessage(
                Box::new(crate::proto::content_v1::AnonDmMessage {
                    inner_content: Some(Box::new(inner)),
                    ..Default::default()
                }),
            )),
            ..Default::default()
        }
        .encode_to_vec();
        let ciphertext = encrypt(&mut f).await;
        let received = decrypt_durable(f.store.clone(), &ciphertext, &f.root, f.key.clone())
            .await
            .unwrap();
        let a = received.artifact.unwrap();
        assert_eq!(a.content, f.body);
        {
            let mut conn = f.db.connection().unwrap();
            let tx = conn.transaction().unwrap();
            crate::storage::receive_artifact::mark_applied(&tx, &a).unwrap();
            tx.commit().unwrap();
        }
        assert!(
            f.store
                .key_store
                .pending_receive_artifacts(&f.key.local_account, 1, 1000)
                .unwrap()
                .is_empty()
        );
        let before = snapshot(&f.db);
        let replay = decrypt_durable(f.store.clone(), &ciphertext, &f.root, f.key.clone())
            .await
            .unwrap();
        assert!(replay.artifact.unwrap().applied);
        assert_eq!(snapshot(&f.db), before);
        // Failed/lost ACK: no acknowledgment helper call, exact evidence stays.
        assert!(
            !f.store
                .key_store
                .receive_artifact(&f.key, &a.digest)
                .unwrap()
                .unwrap()
                .acknowledged
        );
        f.store
            .key_store
            .acknowledge_receive_artifact(&f.key, &a.digest)
            .unwrap();
        let acknowledged = decrypt_durable(f.store.clone(), &ciphertext, &f.root, f.key.clone())
            .await
            .unwrap();
        let receipt = acknowledged.artifact.unwrap();
        assert!(receipt.applied && receipt.acknowledged);
        assert!(receipt.content.is_empty());
        assert!(
            acknowledged.content.is_empty(),
            "acknowledged replay is a receipt, never parse/reapply empty content"
        );
        let payload = crate::storage::backup_data::dump_frames(
            &f.db.connection().unwrap(),
            &f.key.local_account,
        )
        .unwrap()
        .0;
        assert!(
            !payload
                .windows(a.id.len())
                .any(|bytes| bytes == a.id.as_bytes())
        );
        assert!(
            !payload
                .windows(b"reaction_receive_artifacts".len())
                .any(|bytes| bytes == b"reaction_receive_artifacts")
        );
    }
    #[tokio::test]
    async fn journal_full_rolls_back_reaction_but_nonreaction_receive_remains_available() {
        let mut f = fixture().await;
        {
            let conn = f.db.connection().unwrap();
            conn.execute("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<1024) INSERT INTO reaction_receive_artifacts(id,local_account,local_device,transport,scope,envelope_id,server_timestamp_ns,digest,sender,sender_device,content,state,message_type) SELECT CAST(x AS TEXT),?1,1,'anon_dm','capacity',CAST(x AS TEXT),1,zeroblob(32),?2,1,X'01','pending',2 FROM n",rusqlite::params![f.key.local_account,f.sender_address.name()]).unwrap();
        }
        let cipher = encrypt(&mut f).await;
        let before = snapshot(&f.db);
        assert!(
            decrypt_durable(f.store.clone(), &cipher, &f.root, f.key.clone())
                .await
                .is_err()
        );
        assert_eq!(snapshot(&f.db), before);
        f.body = crate::proto::content_v1::Content {
            timestamp: 90,
            ..Default::default()
        }
        .encode_to_vec();
        let ordinary = encrypt(&mut f).await;
        let received = decrypt_durable(f.store.clone(), &ordinary, &f.root, f.key.clone())
            .await
            .unwrap();
        assert!(received.artifact.is_none());
        assert_eq!(received.content, f.body);
        assert_eq!(
            f.store
                .key_store
                .pending_receive_artifacts(&f.key.local_account, 1, 1000)
                .unwrap()
                .len(),
            100
        );
    }
    #[tokio::test]
    async fn group_join_ack_above_reaction_and_old_envelope_limits_routes_without_journaling() {
        let mut f = fixture().await;
        f.key.transport = "anon_group".into();
        f.body = crate::proto::content_v1::Content {
            content: Some(
                crate::proto::content_v1::content::Content::AnonGroupJoinAck(
                    crate::proto::content_v1::AnonGroupJoinAck {
                        group_id: uuid::Uuid::new_v4().as_bytes().to_vec(),
                        joiner_mailbox_pk: vec![7; 32],
                        timestamp: f.key.server_timestamp_ns,
                        profile_key: Some(vec![9; 32]),
                        encrypted_avatar: Some(
                            crate::crypto::profile_crypto::encrypt_avatar(
                                &[9; 32],
                                &vec![8; 160 * 1024],
                            )
                            .unwrap(),
                        ),
                        ..Default::default()
                    },
                ),
            ),
            ..Default::default()
        }
        .encode_to_vec();
        let cipher = encrypt(&mut f).await;
        assert!(cipher.len() > 128 * 1024);
        assert!(cipher.len() < 1024 * 1024);
        let received = decrypt_durable(f.store.clone(), &cipher, &f.root, f.key.clone())
            .await
            .unwrap();
        assert_eq!(received.content, f.body);
        assert!(received.artifact.is_none());
        assert!(
            f.store
                .key_store
                .pending_receive_artifacts(&f.key.local_account, 1, 100)
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn oversized_reaction_and_mailbox_envelope_leave_receive_state_unchanged() {
        let mut f = fixture().await;
        let mut content = crate::proto::content_v1::Content::decode(f.body.as_slice()).unwrap();
        content.padding = vec![0; crate::storage::receive_artifact::MAX_RECEIVE_CONTENT];
        f.body = content.encode_to_vec();
        let cipher = encrypt(&mut f).await;
        let before = snapshot(&f.db);
        assert!(
            decrypt_durable(f.store.clone(), &cipher, &f.root, f.key.clone())
                .await
                .is_err()
        );
        assert_eq!(snapshot(&f.db), before);
        let oversized = vec![0; 1024 * 1024 + 1];
        assert!(
            decrypt_durable(f.store.clone(), &oversized, &f.root, f.key.clone())
                .await
                .is_err()
        );
        assert_eq!(snapshot(&f.db), before);
    }

    #[tokio::test]
    async fn unratcheted_plaintext_and_changed_account_never_commit_receive_state() {
        let mut f = fixture().await;
        let usmc = libsignal_protocol::UnidentifiedSenderMessageContent::new(
            CiphertextMessageType::Plaintext,
            f.cert.clone(),
            b"\xc0not a ratcheted reaction".to_vec(),
            libsignal_protocol::ContentHint::Implicit,
            None,
        )
        .unwrap();
        let cipher = libsignal_protocol::sealed_sender_encrypt_from_usmc(
            &f.recipient,
            &usmc,
            &f.sender.identity_store,
            &mut rand::rngs::OsRng.unwrap_err(),
        )
        .await
        .unwrap();
        let before = snapshot(&f.db);
        assert!(
            decrypt_durable(f.store.clone(), &cipher, &f.root, f.key.clone())
                .await
                .is_err()
        );
        assert_eq!(snapshot(&f.db), before);
        let cipher = encrypt(&mut f).await;
        f.db.connection()
            .unwrap()
            .execute(
                "UPDATE settings SET value='\"other account\"' WHERE key=?1",
                [crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID],
            )
            .unwrap();
        assert!(
            decrypt_durable(f.store.clone(), &cipher, &f.root, f.key.clone())
                .await
                .is_err()
        );
        assert_eq!(snapshot(&f.db), before);
    }
    #[tokio::test]
    async fn total_retention_cap_and_commit_time_capacity_recheck_preserve_state() {
        let mut f = fixture().await;
        let cipher = encrypt(&mut f).await;
        let mut identities = ReceiveStore::new(f.store.clone());
        let mut sessions = identities.clone();
        let mut prekeys = identities.clone();
        let signed = identities.clone();
        let mut kyber = identities.clone();
        let (result, _) = SealedSenderManager::decrypt(
            &cipher,
            &f.root,
            Timestamp::from_epoch_millis(f.key.server_timestamp_ns / 1_000_000),
            None,
            f.key.local_account.clone(),
            DeviceId::try_from(1u32).unwrap(),
            &mut identities,
            &mut sessions,
            &mut prekeys,
            &signed,
            &mut kyber,
        )
        .await
        .unwrap();
        let artifact = crate::storage::receive_artifact::ReceiveArtifact::new(
            f.key.clone(),
            envelope_digest(&cipher),
            result.sender_uuid,
            u32::from(result.device_id),
            result.message,
            2,
        )
        .unwrap();
        {
            let mut conn = f.db.connection().unwrap();
            let tx = conn.transaction().unwrap();
            let mut insert=tx.prepare("INSERT INTO reaction_receive_artifacts(id,local_account,local_device,transport,scope,envelope_id,server_timestamp_ns,digest,sender,sender_device,content,state,message_type) VALUES(?1,?2,1,'anon_dm','retained',?1,1,zeroblob(32),?3,1,X'01','applied',2)").unwrap();
            for i in 0..crate::storage::receive_artifact::MAX_RETAINED_ARTIFACTS {
                insert
                    .execute(rusqlite::params![
                        i.to_string(),
                        f.key.local_account,
                        f.sender_address.name()
                    ])
                    .unwrap();
            }
            drop(insert);
            tx.commit().unwrap();
        }
        let before = snapshot(&f.db);
        assert!(
            f.store
                .key_store
                .commit_receive(&f.key, &identities.changes(), Some(&artifact))
                .is_err()
        );
        assert_eq!(
            snapshot(&f.db),
            before,
            "capacity race must rollback every staged receive mutation"
        );
        assert!(f.store.key_store.check_receive_capacity().is_err());
        // No actual ACK success => retain applied-unacked evidence at capacity.
        let retained =
            f.db.connection()
                .unwrap()
                .query_row(
                    "SELECT content FROM reaction_receive_artifacts WHERE id='1'",
                    [],
                    |r| r.get::<_, Vec<u8>>(0),
                )
                .unwrap();
        assert_eq!(retained, vec![1]);
        let ack_key = ReceiveKey {
            scope: "retained".into(),
            envelope_id: "0".into(),
            server_timestamp_ns: 1,
            ..f.key.clone()
        };
        f.store
            .key_store
            .acknowledge_receive_artifact(&ack_key, &[0; 32])
            .unwrap();
        let receipt = f
            .store
            .key_store
            .receive_artifact(&ack_key, &[0; 32])
            .unwrap()
            .unwrap();
        assert!(receipt.applied && receipt.acknowledged);
        assert!(receipt.content.is_empty());
        // Confirmed acknowledgment makes one bounded slot reclaimable. Actual
        // staged libsignal receive can commit without losing unacked evidence.
        let admitted = f
            .store
            .key_store
            .commit_receive(&f.key, &identities.changes(), Some(&artifact))
            .unwrap()
            .unwrap();
        assert_eq!(admitted, artifact);
        assert!(
            f.store
                .key_store
                .receive_artifact(&ack_key, &[0; 32])
                .unwrap()
                .is_none()
        );
        assert_eq!(
            f.db.connection()
                .unwrap()
                .query_row("SELECT count(*) FROM reaction_receive_artifacts", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            crate::storage::receive_artifact::MAX_RETAINED_ARTIFACTS
        );
        assert_eq!(
            f.db.connection()
                .unwrap()
                .query_row(
                    "SELECT content FROM reaction_receive_artifacts WHERE id='1'",
                    [],
                    |r| r.get::<_, Vec<u8>>(0)
                )
                .unwrap(),
            retained
        );
    }
}
