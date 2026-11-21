use super::*;
use crate::error::NetworkError;
use crate::network::grpc::AnonymousKtClient;
use crate::storage::PeerVerification;
use std::collections::HashSet;
use std::time::Duration;

const ONION_KT_DEADLINE: Duration = Duration::from_secs(95);
const MAX_BACKGROUND_LOOKUPS: usize = 256;
tokio::task_local! {
    static DEFER_KT: ();
    static MAILBOX_KT: Option<AnonymousKtClient>;
}

pub(in crate::services::signal) struct KtBackground {
    queued: parking_lot::Mutex<HashSet<(String, Vec<u8>)>>,
    turn: tokio::sync::Mutex<()>,
}

impl Default for KtBackground {
    fn default() -> Self {
        Self {
            queued: parking_lot::Mutex::default(),
            turn: tokio::sync::Mutex::new(()),
        }
    }
}

fn unverified(peer_id: &str) -> ServiceError {
    ServiceError::Crypto(CryptoError::NotInTransparencyLog {
        peer_id: peer_id.to_owned(),
    })
}

fn pending(error: ServiceError) -> ServiceError {
    match error {
        ServiceError::Network(error) => {
            ServiceError::Crypto(CryptoError::KeyVerificationPending(error))
        }
        error => error,
    }
}

fn anchor_error(error: KtError) -> ServiceError {
    error.trace_rejection();
    ServiceError::Validation(format!(
        "Key Transparency consistency check failed: {error}"
    ))
}

impl SignalService {
    pub(crate) async fn without_inline_kt<T>(future: impl std::future::Future<Output = T>) -> T {
        DEFER_KT.scope((), future).await
    }

    pub(crate) async fn with_mailbox_kt<T>(
        pool: &crate::network::grpc::GrpcClientPool,
        future: impl std::future::Future<Output = T>,
    ) -> T {
        MAILBOX_KT.scope(pool.secret_kt_client().ok(), future).await
    }

    async fn check_tree_head_consistency<F, Fut>(
        &self,
        initial_pin: Option<kt_anchor::PinnedTreeHead>,
        mut fetch_consistency: F,
        _recipient_id: &str,
        proof: &chatter_proto::chat_v1::GetProofResponse,
    ) -> Result<(), ServiceError>
    where
        F: FnMut(u64, u64) -> Fut,
        Fut: std::future::Future<
                Output = Result<chatter_proto::chat_v1::GetConsistencyProofResponse, NetworkError>,
            >,
    {
        let sth = proof
            .signed_tree_head
            .as_ref()
            .ok_or_else(|| anchor_error(KtError::Malformed("proof has no signed tree head")))?;
        loop {
            let (pin, old_epoch, new_epoch) = {
                let _guard = kt_anchor::lock_pin().await;
                let pin = kt_anchor::pinned_head(&self.settings_repo).map_err(anchor_error)?;
                // Another lookup advancing during the fetch can make this
                // proof stale. Retry with a fresh inclusion proof; an already
                // stale head at the time we started remains a rollback error.
                if pin.is_some_and(|head| head.epoch > sth.epoch)
                    && initial_pin.is_none_or(|head| head.epoch <= sth.epoch)
                    && pin != initial_pin
                {
                    return Err(ServiceError::Network(NetworkError::Unavailable));
                }
                match kt_anchor::classify_head(&self.settings_repo, sth).map_err(anchor_error)? {
                    kt_anchor::AnchorAction::Settled(_) => return Ok(()),
                    kt_anchor::AnchorAction::NeedsProof {
                        old_epoch,
                        new_epoch,
                    } => (pin, old_epoch, new_epoch),
                }
            };
            let response = kt_anchor::pinned_rpc(fetch_consistency(old_epoch, new_epoch))
                .await
                .map_err(|error| {
                    // A proof that could not be fetched is a retryable check.
                    // Keep transport causes and never accept an invalid proof.
                    let error = if error.is_retryable() || error.is_transport_error() {
                        error
                    } else {
                        NetworkError::Unavailable
                    };
                    ServiceError::Network(error)
                })?;
            let _guard = kt_anchor::lock_pin().await;
            if kt_anchor::pinned_head(&self.settings_repo).map_err(anchor_error)? == pin {
                return kt_anchor::accept_advance(&self.settings_repo, sth, &response)
                    .map(|_| ())
                    .map_err(anchor_error);
            }
        }
    }

    fn has_verified_identity(&self, peer: &str, key: &[u8]) -> Result<bool, ServiceError> {
        Ok(self.key_store.peer_verification(peer, key)? == PeerVerification::Verified)
    }

    fn require_verified_identity(&self, peer: &str, key: &[u8]) -> Result<(), ServiceError> {
        self.has_verified_identity(peer, key)?
            .then_some(())
            .ok_or_else(|| {
                ServiceError::Validation("KT proof could not authenticate the key".into())
            })
    }

    pub(super) fn session_identity_for_kt(&self, peer: &str) -> Result<Vec<u8>, ServiceError> {
        self.get_peer_identity(peer)?
            .ok_or_else(|| unverified(peer))
    }

    fn local_account(&self, peer: &str) -> Result<bool, ServiceError> {
        Ok(self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)?
            .as_deref()
            == Some(peer))
    }

    pub(crate) fn require_sendable_session_identity(&self, peer: &str) -> Result<(), ServiceError> {
        self.refuse_pending_identity_change(peer)?;
        if self.local_account(peer)? {
            return Ok(());
        }
        let key = self.session_identity_for_kt(peer)?;
        self.key_store
            .peer_verification(peer, &key)?
            .permits_encryption()
            .then_some(())
            .ok_or_else(|| unverified(peer))
    }

    pub(crate) async fn verify_existing_session_identity(
        &self,
        auth: &mut AuthClient,
        peer: &str,
    ) -> Result<(), ServiceError> {
        if let Ok(client) = MAILBOX_KT.try_with(Clone::clone) {
            self.refuse_pending_identity_change(peer)?;
            if self.local_account(peer)? {
                return Ok(());
            }
            let key = self.session_identity_for_kt(peer)?;
            return self.check_mailbox_key(client, peer, &key).await;
        }
        self.check_existing_identity(auth.public_kt_client(), peer, kt_anchor::PIN_RPC_DEADLINE)
            .await
    }

    pub(crate) async fn verify_existing_session_identity_anon(
        &self,
        client: &mut AnonymousKtClient,
        peer: &str,
    ) -> Result<(), ServiceError> {
        self.check_existing_identity(client.clone(), peer, ONION_KT_DEADLINE)
            .await
    }

    async fn check_existing_identity(
        &self,
        client: AnonymousKtClient,
        peer: &str,
        deadline: Duration,
    ) -> Result<(), ServiceError> {
        self.refuse_pending_identity_change(peer)?;
        if self.local_account(peer)? {
            return Ok(());
        }
        let key = self.session_identity_for_kt(peer)?;
        self.check_outbound_key(client, peer, &key, deadline).await
    }

    async fn check_outbound_key(
        &self,
        client: AnonymousKtClient,
        peer: &str,
        key: &[u8],
        deadline: Duration,
    ) -> Result<(), ServiceError> {
        let state = self.key_store.peer_verification(peer, key)?;
        match state {
            PeerVerification::Verified | PeerVerification::LegacyUnlisted => Ok(()),
            PeerVerification::Legacy => {
                self.schedule_key_verification(client, peer, key, deadline);
                Ok(())
            }
            PeerVerification::UnverifiedInbound => {
                if DEFER_KT.try_with(|_| ()).is_ok() {
                    self.schedule_key_verification(client, peer, key, deadline);
                    return Err(pending(ServiceError::Network(NetworkError::Unavailable)));
                }
                let result = self.verify_exact_key(client, peer, key, deadline).await;
                self.park_contradiction(peer, key, &result).await?;
                result.map_err(pending)
            }
        }
    }

    /// Mailbox peers must never be named on the identified transport. A
    /// verified or legacy key remains usable even if the proof route is down.
    pub(crate) async fn verify_mailbox_identity(
        &self,
        pool: &crate::network::grpc::GrpcClientPool,
        peer: &str,
    ) -> Result<(), ServiceError> {
        self.refuse_pending_identity_change(peer)?;
        if self.local_account(peer)? {
            return Ok(());
        }
        let key = self.session_identity_for_kt(peer)?;
        self.check_mailbox_key(pool.secret_kt_client().ok(), peer, &key)
            .await
    }

    async fn check_mailbox_key(
        &self,
        client: Option<AnonymousKtClient>,
        peer: &str,
        key: &[u8],
    ) -> Result<(), ServiceError> {
        let state = self.key_store.peer_verification(peer, key)?;
        if matches!(
            state,
            PeerVerification::Verified | PeerVerification::LegacyUnlisted
        ) {
            return Ok(());
        }
        match client {
            Some(client) => {
                self.check_outbound_key(client, peer, key, ONION_KT_DEADLINE)
                    .await
            }
            None if state == PeerVerification::Legacy => Ok(()),
            None => Err(pending(ServiceError::Network(
                NetworkError::SecretChatNeedsTor(
                    "mailbox key verification requires an onion route",
                ),
            ))),
        }
    }

    pub(crate) fn schedule_session_verification(
        &self,
        client: AnonymousKtClient,
        peer: &str,
        onion: bool,
    ) -> Result<(), ServiceError> {
        if self.local_account(peer)? {
            return Ok(());
        }
        if let Some(key) = self.get_peer_identity(peer)? {
            if matches!(
                self.key_store.peer_verification(peer, &key)?,
                PeerVerification::Legacy | PeerVerification::UnverifiedInbound
            ) {
                let deadline = if onion {
                    ONION_KT_DEADLINE
                } else {
                    kt_anchor::PIN_RPC_DEADLINE
                };
                self.schedule_key_verification(client, peer, &key, deadline);
            }
        }
        Ok(())
    }

    fn schedule_key_verification(
        &self,
        client: AnonymousKtClient,
        peer: &str,
        key: &[u8],
        deadline: Duration,
    ) {
        let entry = (peer.to_owned(), key.to_vec());
        {
            let mut queued = self.kt_background.queued.lock();
            if queued.len() >= MAX_BACKGROUND_LOOKUPS || !queued.insert(entry.clone()) {
                return;
            }
        }
        let svc = self.clone();
        let owner = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .ok()
            .flatten();
        tokio::spawn(async move {
            let (peer, key) = &entry;
            for delay in [0, 5, 20, 60] {
                if delay != 0 {
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                }
                let _turn = svc.kt_background.turn.lock().await;
                if owner
                    != svc
                        .settings_repo
                        .get_string(keys::CURRENT_USER_ID)
                        .ok()
                        .flatten()
                    || !svc.get_peer_identity(peer).is_ok_and(|current| {
                        current.as_deref().is_none_or(|current| current == key)
                    })
                    || svc.refuse_pending_identity_change(peer).is_err()
                {
                    break;
                }
                // Pace successful and failed checks below the server's two
                // consistency RPCs per second. The queue is off the send path.
                tokio::time::sleep(Duration::from_millis(550)).await;
                let result = svc
                    .verify_exact_key(client.clone(), peer, key, deadline)
                    .await;
                if matches!(
                    result,
                    Err(ServiceError::Crypto(
                        CryptoError::NotInTransparencyLog { .. }
                    ))
                ) && svc.key_store.mark_legacy_unlisted(peer, key).is_ok()
                {
                    if svc.key_store.peer_verification(peer, key).ok()
                        == Some(PeerVerification::LegacyUnlisted)
                    {
                        tracing::info!(target: "kt_peer", "legacy peer has no transparency entry");
                    }
                    break;
                }
                if !matches!(result, Err(ServiceError::Network(_))) {
                    let _ = svc.park_contradiction(peer, key, &result).await;
                    break;
                }
            }
            svc.kt_background.queued.lock().remove(&entry);
        });
    }

    async fn park_contradiction(
        &self,
        peer: &str,
        key: &[u8],
        result: &Result<(), ServiceError>,
    ) -> Result<(), ServiceError> {
        if let Err(error @ ServiceError::Crypto(CryptoError::UntrustedIdentity { .. })) = result {
            if self.get_peer_identity(peer)?.as_deref() == Some(key) {
                self.note_identity_change(error);
                self.archive_peer_sessions(peer).await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn verify_fetched_identity(
        &self,
        auth: &mut AuthClient,
        peer: &str,
        key: &[u8],
    ) -> Result<(), ServiceError> {
        if self.local_account(peer)? && self.get_identity_public_key().await?.as_slice() == key {
            return Ok(());
        }
        if let Ok(client) = MAILBOX_KT.try_with(Clone::clone) {
            return self.check_mailbox_key(client, peer, key).await;
        }
        self.check_outbound_key(
            auth.public_kt_client(),
            peer,
            key,
            kt_anchor::PIN_RPC_DEADLINE,
        )
        .await
    }

    pub(super) async fn verify_peer_key_transparency(
        &self,
        auth: &mut AuthClient,
        peer: &str,
        bundle: &libsignal_protocol::PreKeyBundle,
    ) -> Result<(), ServiceError> {
        let key = bundle
            .identity_key()
            .map_err(|e| ServiceError::Validation(e.to_string()))?
            .serialize();
        self.verify_fetched_identity(auth, peer, &key).await
    }

    pub(super) async fn verify_peer_key_transparency_anon(
        &self,
        client: &mut AnonymousKtClient,
        peer: &str,
        bundle: &libsignal_protocol::PreKeyBundle,
    ) -> Result<(), ServiceError> {
        let key = bundle
            .identity_key()
            .map_err(|e| ServiceError::Validation(e.to_string()))?
            .serialize();
        self.check_outbound_key(client.clone(), peer, &key, ONION_KT_DEADLINE)
            .await
    }

    async fn verify_exact_key(
        &self,
        client: AnonymousKtClient,
        peer: &str,
        key: &[u8],
        deadline: Duration,
    ) -> Result<(), ServiceError> {
        let lock = self
            .get_encrypt_lock(&format!("kt:{peer}:{}", hex::encode(key)))
            .await;
        let _turn = lock.lock().await;
        self.refuse_pending_identity_change(peer)?;
        if self.has_verified_identity(peer, key)? {
            return Ok(());
        }
        self.fetch_and_judge(client, peer, Some(key), deadline)
            .await?;
        self.require_verified_identity(peer, key)
    }

    /// Membership proofs check the current signed head even if the key had
    /// been verified before. Existing sessions use the exact-key cache.
    pub async fn verify_identity_key_transparency(
        &self,
        auth: &mut AuthClient,
        peer: &str,
        identity_key: &[u8],
    ) -> Result<(), ServiceError> {
        self.fetch_and_judge(
            auth.public_kt_client(),
            peer,
            Some(identity_key),
            kt_anchor::PIN_RPC_DEADLINE,
        )
        .await
        .map(|_| ())
    }

    pub async fn verified_identity_key_from_kt(
        &self,
        auth: &mut AuthClient,
        peer: &str,
    ) -> Result<Vec<u8>, ServiceError> {
        self.fetch_and_judge(
            auth.public_kt_client(),
            peer,
            None,
            kt_anchor::PIN_RPC_DEADLINE,
        )
        .await
    }

    /// Check the authenticated key before spending a blind token for a
    /// first-contact prekey fetch. That bundle must still match this exact key.
    pub(crate) async fn verify_peer_before_anonymous_fetch(
        &self,
        client: AnonymousKtClient,
        peer: &str,
    ) -> Result<Vec<u8>, ServiceError> {
        if let Some(key) = self.get_peer_identity(peer)? {
            self.verify_existing_session_identity_anon(&mut client.clone(), peer)
                .await?;
            return Ok(key);
        }
        let key = self
            .fetch_and_judge(client, peer, None, ONION_KT_DEADLINE)
            .await
            .map_err(pending)?;
        self.require_verified_identity(peer, &key)?;
        Ok(key)
    }

    async fn fetch_and_judge(
        &self,
        mut client: AnonymousKtClient,
        peer: &str,
        expected: Option<&[u8]>,
        deadline: Duration,
    ) -> Result<Vec<u8>, ServiceError> {
        let pin = kt_anchor::pinned_head(&self.settings_repo).map_err(anchor_error)?;
        let fetched = tokio::time::timeout(deadline, client.get_proof(peer))
            .await
            .unwrap_or(Err(NetworkError::Timeout));
        let key = expected
            .map(<[u8]>::to_vec)
            .or_else(|| {
                fetched
                    .as_ref()
                    .ok()?
                    .as_ref()
                    .map(|proof| proof.identity_key.clone())
            })
            .unwrap_or_default();
        self.judge_kt_proof(pin, "KT", peer, &key, fetched, move |old, new| {
            let mut client = client.clone();
            async move { client.get_consistency_proof(old, new).await }
        })
        .await?;
        Ok(key)
    }

    async fn judge_kt_proof<F, Fut>(
        &self,
        pin: Option<kt_anchor::PinnedTreeHead>,
        _path: &'static str,
        peer: &str,
        key: &[u8],
        fetched: Result<Option<chatter_proto::chat_v1::GetProofResponse>, NetworkError>,
        fetch_consistency: F,
    ) -> Result<(), ServiceError>
    where
        F: FnMut(u64, u64) -> Fut,
        Fut: std::future::Future<
                Output = Result<chatter_proto::chat_v1::GetConsistencyProofResponse, NetworkError>,
            >,
    {
        let proof = fetched?.ok_or_else(|| unverified(peer))?;
        match crate::crypto::verify_kt_proof(peer, key, &proof) {
            Ok(()) => {
                self.check_tree_head_consistency(pin, fetch_consistency, peer, &proof)
                    .await?;
                self.key_store.mark_peer_verified(peer, key)?;
                Ok(())
            }
            #[cfg(feature = "dev-trust-root")]
            Err(KtError::Malformed(reason)) => {
                tracing::warn!(
                    reason,
                    _path,
                    "KT proof does not match this dev build's protocol"
                );
                Ok(())
            }
            Err(KtError::IdentityKeyMismatch) => {
                let candidate = proof
                    .inclusion_proof
                    .as_ref()
                    .map(|leaf| leaf.value.clone())
                    .ok_or_else(|| anchor_error(KtError::Malformed("proof has no leaf")))?;
                crate::crypto::verify_kt_proof(peer, &candidate, &proof).map_err(anchor_error)?;
                self.check_tree_head_consistency(pin, fetch_consistency, peer, &proof)
                    .await?;
                Err(ServiceError::Crypto(CryptoError::UntrustedIdentity {
                    peer_id: peer.to_owned(),
                    device_id: 0,
                    new_identity_key: Some(candidate),
                    attribution: crate::error::IdentityChangeSource::ServerClaimed,
                }))
            }
            Err(error) => Err(anchor_error(error)),
        }
    }
}

#[cfg(test)]
mod kt_pin_tests {
    use super::*;
    use chatter_proto::chat_v1::{GetConsistencyProofResponse, GetProofResponse, SignedTreeHead};

    #[tokio::test]
    async fn kt_verified_identity_cache_survives_service_restart_and_matches_exact_key() {
        let Some(manifest) = crate::mailbox::blind_signature::tests::kt_manifest_vector() else {
            return;
        };
        let subject = format!("chatter/mailbox-manifest/v1/{}", manifest.key_id);
        let proof = manifest.kt_proof.unwrap();
        let committed = proof.inclusion_proof.as_ref().unwrap().value.clone();
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            crate::storage::Database::open(&dir.path().join("kt.db"), "testpass").unwrap(),
        );
        db.run_migrations().unwrap();
        let make_service = || {
            SignalService::new(
                Arc::new(crate::storage::KeyStore::new(db.clone())),
                Arc::new(crate::storage::repositories::SettingsRepository::new(
                    db.clone(),
                )),
            )
        };
        let service = make_service();
        let pin = kt_anchor::pinned_head(&service.settings_repo).unwrap();
        let no_fetch =
            |_: u64, _: u64| std::future::ready(Err(crate::error::NetworkError::Unavailable));

        assert!(
            service
                .require_verified_identity(&subject, &committed)
                .is_err()
        );
        let err = service
            .judge_kt_proof(
                pin,
                "KT",
                &subject,
                &[5; 33],
                Ok(Some(proof.clone())),
                no_fetch,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ServiceError::Crypto(CryptoError::UntrustedIdentity { .. })
        ));
        assert!(
            service
                .require_verified_identity(&subject, &[5; 33])
                .is_err()
        );
        assert!(
            service
                .require_verified_identity(&subject, &committed)
                .is_err()
        );

        service
            .judge_kt_proof(pin, "KT", &subject, &committed, Ok(Some(proof)), no_fetch)
            .await
            .unwrap();
        assert!(
            service
                .require_verified_identity(&subject, &committed)
                .is_ok()
        );
        drop(service);
        let service = make_service();
        assert!(
            service
                .require_verified_identity(&subject, &committed)
                .is_ok()
        );
        assert!(
            service
                .require_verified_identity(&subject, &[5; 33])
                .is_err()
        );
        assert!(
            service
                .require_verified_identity("other", &committed)
                .is_err()
        );
    }

    /// The anchor both KT paths (identified and `verify_peer_key_transparency_anon`)
    /// run after the inclusion proof: a head that forks from the pin is refused
    /// without asking the server anything.
    #[tokio::test]
    async fn a_head_forked_from_the_pin_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            crate::storage::Database::open(&dir.path().join("kt.db"), "testpass").unwrap(),
        );
        db.run_migrations().unwrap();
        let settings = Arc::new(crate::storage::repositories::SettingsRepository::new(
            db.clone(),
        ));
        let svc = SignalService::new(
            Arc::new(crate::storage::KeyStore::new(db)),
            settings.clone(),
        );

        let head = |root: u8| GetProofResponse {
            signed_tree_head: Some(SignedTreeHead {
                epoch: 5,
                tree_size: 5,
                root_hash: vec![root; 32],
                ..Default::default()
            }),
            ..Default::default()
        };
        fn no_fetch(
            _: u64,
            _: u64,
        ) -> std::future::Ready<Result<GetConsistencyProofResponse, crate::error::NetworkError>>
        {
            panic!("a fork needs no consistency proof")
        }
        let pin = None;
        svc.check_tree_head_consistency(pin, no_fetch, "bob", &head(1))
            .await
            .unwrap();
        let err = svc
            .check_tree_head_consistency(pin, no_fetch, "bob", &head(2))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("consistency check failed"),
            "{err}"
        );
        assert_eq!(
            kt_anchor::pinned_head(&settings).unwrap().unwrap().root,
            [1u8; 32]
        );
    }

    /// Both KT gates hand their fetch result to `judge_kt_proof`. A server
    /// answering NotFound must get a refused session, not a TOFU downgrade.
    #[tokio::test]
    async fn not_found_refuses_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            crate::storage::Database::open(&dir.path().join("kt.db"), "testpass").unwrap(),
        );
        db.run_migrations().unwrap();
        let settings = Arc::new(crate::storage::repositories::SettingsRepository::new(
            db.clone(),
        ));
        let svc = SignalService::new(
            Arc::new(crate::storage::KeyStore::new(db)),
            settings.clone(),
        );
        fn no_fetch(
            _: u64,
            _: u64,
        ) -> std::future::Ready<Result<GetConsistencyProofResponse, crate::error::NetworkError>>
        {
            panic!("no proof, nothing to hold")
        }

        let pin = None;
        for path in ["KT", "KT(anon)"] {
            let err = svc
                .judge_kt_proof(pin, path, "bob", &[5u8; 33], Ok(None), no_fetch)
                .await
                .unwrap_err();
            assert!(
                matches!(&err, ServiceError::Crypto(CryptoError::NotInTransparencyLog { peer_id }) if peer_id == "bob"),
                "{path}: {err}"
            );
        }
        assert!(kt_anchor::pinned_head(&settings).unwrap().is_none());
    }
}
