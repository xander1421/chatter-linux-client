use super::*;
use crate::crypto::keytransparency::KtError;
use crate::error::NetworkError;
use crate::events::{DomainEvent, EventBus};
use crate::network::grpc::{AnonymousKtClient, client_pool::GrpcClientPool};
use crate::state::AuthState;
use crate::storage::peer_verification::{KtJob, KtRoute, KtUpdate, KtVerdict};
use futures::StreamExt;
use std::time::Duration;

const ANONYMOUS_DEADLINE: Duration = Duration::from_secs(95);

pub(crate) fn spawn_peer_verifier(
    service: Arc<SignalService>,
    pool: Arc<GrpcClientPool>,
    events: EventBus,
    auth: AuthState,
    transport_epoch: tokio::sync::watch::Receiver<u64>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::join!(
            run_peer_verifier(&service, &pool, &events, &auth, KtRoute::Normal, transport_epoch.clone()),
            run_peer_verifier(&service, &pool, &events, &auth, KtRoute::AnonymousTor, transport_epoch),
        );
    })
}

async fn run_peer_verifier(
    service: &SignalService,
    pool: &GrpcClientPool,
    events: &EventBus,
    auth: &AuthState,
    route: KtRoute,
    mut transport_epoch: tokio::sync::watch::Receiver<u64>,
) {
    let mut recovered_session = None;
    let mut ticks = tokio::time::interval(Duration::from_secs(3));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = ticks.tick() => {}
            _ = service.key_store.kt_work().notified() => {}
        }
        let state = auth.get();
        let Some(owner) = state.current_user_id.filter(|_| !state.session_ended) else {
            continue;
        };
        let revision = state.session_revision;
        let owner_key = owner.to_string();
        if route == KtRoute::AnonymousTor && recovered_session != Some((owner, revision)) {
            if let Err(error) = service.key_store.recover_kt_jobs(&owner_key) {
                tracing::warn!(%error, "Could not recover pending key verification");
                continue;
            }
            recovered_session = Some((owner, revision));
        }
        transport_epoch.borrow_and_update();
        if route == KtRoute::Normal && service.actor.kt_route() == KtRoute::AnonymousTor {
            continue;
        }
        let jobs = match service
            .key_store
            .due_kt_jobs(route, chrono::Utc::now().timestamp())
        {
            Ok(jobs) => jobs,
            Err(error) => {
                tracing::warn!(%error, "Could not load pending key verification");
                continue;
            }
        };
        let owner_key = &owner_key;
        let verify = futures::stream::iter(jobs)
            .for_each_concurrent(4, |job| async move {
                let current = || {
                    let state = auth.get();
                    state.current_user_id == Some(owner)
                        && !state.session_ended
                        && state.session_revision == revision
                };
                if !current() {
                    return;
                }
                let verdict = service
                    .verify_peer_key(pool, &job.peer, &job.key, job.route)
                    .await;
                if !current() {
                    return;
                }
                match verdict {
                    Ok(verdict) => {
                        match settle_peer_verification(service, events, owner_key, job, verdict)
                            .await
                        {
                            Ok(()) => {}
                            Err(error) => {
                                tracing::warn!(%error, "Could not store peer verification")
                            }
                        }
                    }
                    Err(error) => {
                        tracing::debug!(%error, ?route, "Peer key verification will retry");
                        if let Err(error) = service
                            .key_store
                            .delay_kt_job(&job, chrono::Utc::now().timestamp())
                        {
                            tracing::warn!(%error, "Could not reschedule peer verification");
                        }
                    }
                }
            });
        if route == KtRoute::Normal {
            // Stop polling in-flight direct RPCs, including a proof's follow-up
            // consistency request, when the caller changes privacy modes.
            tokio::select! {
                biased;
                _ = transport_epoch.changed() => {}
                _ = verify => {}
            }
        } else {
            verify.await;
        }
    }
}

async fn settle_peer_verification(
    service: &SignalService,
    events: &EventBus,
    owner: &str,
    job: KtJob,
    verdict: KtVerdict,
) -> Result<(), ServiceError> {
    let peer = job.peer.parse();
    let update = service
        .actor
        .complete_kt_job(owner.to_owned(), job, verdict)
        .await?;
    if let Ok(peer_user_id) = peer {
        if update == KtUpdate::Changed {
            events.emit_domain(DomainEvent::PeerIdentityChanged {
                peer_user_id,
                attributed: false,
            });
        }
        if update != KtUpdate::Stale {
            events.emit_domain(DomainEvent::PeerKeyVerificationUpdated { peer_user_id });
        }
    }
    Ok(())
}

fn job_client(pool: &GrpcClientPool, route: KtRoute) -> Result<AnonymousKtClient, NetworkError> {
    match route {
        KtRoute::Normal => Ok(AnonymousKtClient::new(pool.channel())),
        KtRoute::AnonymousTor => pool.secret_kt_client(),
    }
}

impl SignalService {
    /// Network verification boundary. Delivery paths enqueue these inputs in
    /// the store. The lookup implementation may change independently of admission.
    async fn verify_peer_key(
        &self,
        pool: &GrpcClientPool,
        peer: &str,
        key: &[u8],
        route: KtRoute,
    ) -> Result<KtVerdict, ServiceError> {
        if route == KtRoute::Normal && self.actor.kt_route() == KtRoute::AnonymousTor {
            return Err(NetworkError::Unavailable.into());
        }
        let mut client = job_client(pool, route)?;
        let deadline = match route {
            KtRoute::Normal => kt_anchor::PIN_RPC_DEADLINE,
            KtRoute::AnonymousTor => ANONYMOUS_DEADLINE,
        };
        tokio::time::timeout(deadline, async {
            let initial_pin = kt_anchor::pinned_head(&self.settings_repo).map_err(anchor_error)?;
            let Some(proof) = client.get_proof(peer).await? else {
                return Ok(KtVerdict::NotInLog);
            };
            let actual = proof
                .inclusion_proof
                .as_ref()
                .ok_or_else(|| anchor_error(KtError::Malformed("proof has no leaf")))?
                .value
                .as_slice();
            crate::crypto::verify_kt_proof(peer, actual, &proof).map_err(anchor_error)?;
            libsignal_protocol::IdentityKey::try_from(actual)
                .map_err(|_| ServiceError::Crypto(CryptoError::InvalidKeyFormat))?;
            self.hold_peer_head(initial_pin, &proof, &mut client)
                .await?;
            Ok(if actual == key {
                KtVerdict::Verified
            } else {
                KtVerdict::Mismatch(actual.to_vec())
            })
        })
        .await
        .unwrap_or_else(|_| Err(NetworkError::Timeout.into()))
    }

    async fn hold_peer_head(
        &self,
        initial_pin: Option<kt_anchor::PinnedTreeHead>,
        proof: &chatter_proto::chat_v1::GetProofResponse,
        client: &mut AnonymousKtClient,
    ) -> Result<(), ServiceError> {
        let head = proof
            .signed_tree_head
            .as_ref()
            .ok_or_else(|| anchor_error(KtError::Malformed("proof has no signed tree head")))?;
        loop {
            let (pin, old, new) = {
                let _guard = kt_anchor::lock_pin().await;
                let pin = kt_anchor::pinned_head(&self.settings_repo).map_err(anchor_error)?;
                if pin.is_some_and(|p| p.epoch > head.epoch)
                    && initial_pin.is_none_or(|p| p.epoch <= head.epoch)
                {
                    return Err(NetworkError::Unavailable.into());
                }
                match kt_anchor::classify_head(&self.settings_repo, head).map_err(anchor_error)? {
                    kt_anchor::AnchorAction::Settled(_) => return Ok(()),
                    kt_anchor::AnchorAction::NeedsProof {
                        old_epoch,
                        new_epoch,
                    } => (pin, old_epoch, new_epoch),
                }
            };
            let consistency = client.get_consistency_proof(old, new).await?;
            let _guard = kt_anchor::lock_pin().await;
            if kt_anchor::pinned_head(&self.settings_repo).map_err(anchor_error)? == pin {
                kt_anchor::accept_advance(&self.settings_repo, head, &consistency)
                    .map_err(anchor_error)?;
                return Ok(());
            }
        }
    }

    /// Key Transparency gate (Phase 2).
    ///
    /// Before trusting the identity key inside a peer's prekey bundle, fetch the
    /// server's signed KT inclusion proof for that peer and verify that the
    /// bundle's identity key is exactly the one committed in the append-only KT
    /// log, under the client-pinned tree-head public key
    /// (`config::KT_TREE_HEAD_PUBLIC_KEY_BYTES`).
    ///
    /// No-proof policy (deliberate, LOUD, not silent):
    /// * peer HAS a leaf, key matches  -> Ok (verified).
    /// * peer HAS a leaf, key differs   -> REJECT (F9 forged/MITM identity key).
    /// * peer has NO leaf (NotFound)    -> REJECT with
    ///   [`CryptoError::NotInTransparencyLog`]. The server logs every identity
    ///   key in the transaction that stores it, so "no leaf" is never honest
    ///   for a peer with a bundle; accepting it would let a server skip KT for
    ///   any peer it wants to MITM.
    /// * any other transport/server error -> REJECT (an error is not evidence of
    ///   an absent leaf; a malicious server must not be able to error its way
    ///   past the check).
    /// Hold the KT log to one history over time.
    ///
    /// `verify_kt_proof` verifies a peer's key against the head in the SAME
    /// response, so a server that signs heads can serve a different tree per
    /// user or per request and every proof still verifies. This compares the
    /// offered head against the highest one this device ever accepted and, when
    /// it is newer, requires a consistency proof from the pinned head to it.
    ///
    /// Failures REJECT the session rather than warn. The server can rebuild
    /// state at any epoch by replaying its log (`Service.replay`), so it has no honest
    /// reason to fail — and a server able to suppress this check by erroring
    /// could fork the log undetectably, which is the same reasoning that
    /// already makes a transport error on `GetProof` a reject rather than a
    /// "no leaf".
    ///
    /// `fetch_consistency` is the channel's own consistency-proof RPC, so the
    /// anonymous path never falls back to an identified request.
    async fn check_tree_head_consistency<F, Fut>(
        &self,
        pin: &kt_anchor::PinGuard,
        fetch_consistency: F,
        recipient_id: &str,
        proof: &chatter_proto::chat_v1::GetProofResponse,
    ) -> Result<(), ServiceError>
    where
        F: FnOnce(u64, u64) -> Fut,
        Fut: std::future::Future<
                Output = Result<
                    chatter_proto::chat_v1::GetConsistencyProofResponse,
                    crate::error::NetworkError,
                >,
            >,
    {
        let Some(sth) = proof.signed_tree_head.as_ref() else {
            return Err(ServiceError::Validation(
                "KT proof has no signed tree head".to_string(),
            ));
        };

        match kt_anchor::hold_head_locked(pin, &self.settings_repo, sth, fetch_consistency).await {
            Ok(kt_anchor::AnchorOutcome::FirstPin) => {
                tracing::info!(
                    epoch = sth.epoch,
                    "KT: pinned the transparency log's tree head for the first time"
                );
                Ok(())
            }
            Ok(kt_anchor::AnchorOutcome::Advanced) => {
                tracing::info!(
                    new_epoch = sth.epoch,
                    "KT: advanced the pinned tree head; log verified append-only"
                );
                Ok(())
            }
            Ok(kt_anchor::AnchorOutcome::Unchanged) => Ok(()),
            Err(e) => {
                e.trace_rejection();
                tracing::error!(
                    recipient = %recipient_id,
                    offered_epoch = sth.epoch,
                    error = %e,
                    "KT REJECT: could not establish continuity with the pinned tree head"
                );
                Err(ServiceError::Validation(format!(
                    "Key Transparency consistency check failed for {recipient_id}: {e}"
                )))
            }
        }
    }

    /// The KT gate on a bare identity key: `Ok(())` iff `identity_key` is
    /// `user_id`'s key in the pinned, append-only transparency log. Used for
    /// the authors of signed channel membership changes.
    pub async fn verify_identity_key_transparency(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        identity_key: &[u8],
    ) -> Result<(), ServiceError> {
        // Held from the proof fetch through the pin update, so this head is
        // never older than a pin a concurrent check just advanced
        // (`kt_anchor::lock_pin`).
        let pin = kt_anchor::lock_pin().await;
        let fetched =
            kt_anchor::pinned_rpc(auth_client.get_key_transparency_proof(recipient_id)).await;
        self.judge_kt_proof(
            &pin,
            "KT",
            recipient_id,
            identity_key,
            fetched,
            |old, new| auth_client.get_consistency_proof(old, new),
        )
        .await
    }

    /// Return the identity key from one proof only after the same inclusion
    /// and pinned-head checks used for prekey bundles have accepted it.
    pub async fn verified_identity_key_from_kt(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
    ) -> Result<Vec<u8>, ServiceError> {
        let pin = kt_anchor::lock_pin().await;
        let fetched =
            kt_anchor::pinned_rpc(auth_client.get_key_transparency_proof(recipient_id)).await;
        let identity_key = fetched
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .map(|proof| proof.identity_key.clone())
            .unwrap_or_default();
        self.judge_kt_proof(
            &pin,
            "KT",
            recipient_id,
            &identity_key,
            fetched,
            |old, new| auth_client.get_consistency_proof(old, new),
        )
        .await?;
        Ok(identity_key)
    }

    /// The verdict both KT gates (identified and anonymous) reach once the
    /// proof fetch has returned. `path` labels the log lines.
    async fn judge_kt_proof<F, Fut>(
        &self,
        pin: &kt_anchor::PinGuard,
        path: &'static str,
        recipient_id: &str,
        identity_key: &[u8],
        fetched: Result<
            Option<chatter_proto::chat_v1::GetProofResponse>,
            crate::error::NetworkError,
        >,
        fetch_consistency: F,
    ) -> Result<(), ServiceError>
    where
        F: FnOnce(u64, u64) -> Fut,
        Fut: std::future::Future<
                Output = Result<
                    chatter_proto::chat_v1::GetConsistencyProofResponse,
                    crate::error::NetworkError,
                >,
            >,
    {
        let proof = match fetched {
            Ok(Some(proof)) => proof,
            Ok(None) => {
                tracing::error!(
                    recipient = %recipient_id,
                    "{path} REJECT: server says the peer has no Key Transparency entry — every \
                     stored identity key is logged, so this is the server skipping KT; refusing \
                     session"
                );
                return Err(ServiceError::Crypto(CryptoError::NotInTransparencyLog {
                    peer_id: recipient_id.to_string(),
                }));
            }
            Err(e) => {
                tracing::error!(
                    recipient = %recipient_id,
                    error = %e,
                    "{path}: proof fetch failed (transport/server error or deadline) — refusing \
                     session (an error is not evidence of an absent leaf)"
                );
                return Err(ServiceError::Network(e));
            }
        };

        match crate::crypto::verify_kt_proof(recipient_id, identity_key, &proof) {
            Ok(()) => {
                let epoch = proof
                    .signed_tree_head
                    .as_ref()
                    .map(|s| s.epoch)
                    .unwrap_or_default();
                tracing::debug!(
                    recipient = %recipient_id,
                    epoch,
                    "{path}: peer identity key verified against the signed transparency log"
                );
                // The inclusion proof says the key is in the tree the server
                // just signed. It says nothing about WHICH tree, so on its own
                // it cannot see a fork or a rollback. Pin the head across time
                // as well.
                self.check_tree_head_consistency(pin, fetch_consistency, recipient_id, &proof)
                    .await
            }
            // `Malformed` means the proof doesn't even have the shape this
            // verifier expects (e.g. missing prefix_root) — that's a
            // protocol-version mismatch (client speaks KT v2, two-tree/log-sealed;
            // this server still speaks v1, single-tree), not evidence of a forged
            // key. Every OTHER variant is a real cryptographic verification
            // failure against a structurally-valid v2 proof and must keep
            // hard-rejecting even here.
            #[cfg(feature = "dev-trust-root")]
            Err(KtError::Malformed(reason)) => {
                tracing::warn!(
                    recipient = %recipient_id,
                    reason,
                    "{path} DOWNGRADE (dev-trust-root, protocol version mismatch): server's KT \
                     proof doesn't match the v2 shape this client verifies (server is still on \
                     v1) — trusting the bundle identity key WITHOUT a KT proof. NEVER hit \
                     outside a dev-trust-root build."
                );
                Ok(())
            }
            Err(e) => {
                e.trace_rejection();
                tracing::error!(
                    recipient = %recipient_id,
                    error = %e,
                    "{path} REJECT: refusing session — bundle identity key is NOT the key \
                     committed in the Key Transparency log (possible forged/MITM key, F9)"
                );
                Err(ServiceError::Validation(format!(
                    "Key Transparency verification failed for {recipient_id}: {e}"
                )))
            }
        }
    }
}

fn anchor_error(error: KtError) -> ServiceError {
    error.trace_rejection();
    ServiceError::Validation(format!("Key Transparency verification failed: {error}"))
}

#[cfg(test)]
mod kt_pin_tests {
    use super::*;
    use chatter_proto::chat_v1::{GetConsistencyProofResponse, GetProofResponse, SignedTreeHead};

    fn fixture() -> (tempfile::TempDir, Arc<SignalService>, crate::domain::UserId) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            crate::storage::Database::open(&dir.path().join("keys.db"), "fixture").unwrap(),
        );
        db.run_migrations().unwrap();
        let owner = crate::domain::UserId::new();
        let settings = Arc::new(crate::storage::SettingsRepository::new(db.clone()));
        settings
            .set_string(keys::CURRENT_USER_ID, &owner.to_string())
            .unwrap();
        let service = Arc::new(SignalService::new(
            Arc::new(crate::storage::KeyStore::new(db)),
            settings,
        ));
        (dir, service, owner)
    }

    #[tokio::test]
    async fn shield_and_secret_verification_never_fall_back_when_onion_is_missing() {
        use std::sync::atomic::Ordering;
        let (_dir, service, _) = fixture();
        let pool = GrpcClientPool::new("http://127.0.0.1:1").unwrap();
        assert!(job_client(&pool, KtRoute::Normal).is_ok());
        let peer = crate::domain::UserId::new().to_string();
        let shield = service.actor.shield_mode();
        shield.store(true, Ordering::Release);
        let shield_route = service.actor.kt_route();
        shield.store(false, Ordering::Release);
        let secret_route = service.actor.for_anonymous_route().kt_route();
        for route in [shield_route, secret_route] {
            assert_eq!(route, KtRoute::AnonymousTor);
            assert!(matches!(
                service.verify_peer_key(&pool, &peer, &[5; 33], route).await,
                Err(ServiceError::Network(NetworkError::SecretChatNeedsTor(_)))
            ));
        }
    }

    #[tokio::test]
    async fn queued_exact_keys_and_their_routes_survive_restart() {
        let (dir, service, owner) = fixture();
        let peer = crate::domain::UserId::new().to_string();
        let key = [5; 33];
        for route in [
            KtRoute::AnonymousTor,
            KtRoute::Normal,
            KtRoute::AnonymousTor,
        ] {
            service.key_store.enqueue_kt(&peer, &key, route).unwrap();
        }
        drop(service);
        let db = Arc::new(
            crate::storage::Database::open(&dir.path().join("keys.db"), "fixture").unwrap(),
        );
        let keys = crate::storage::KeyStore::new(db);
        keys.recover_kt_jobs(&owner.to_string()).unwrap();
        for route in [KtRoute::Normal, KtRoute::AnonymousTor] {
            let jobs = keys.due_kt_jobs(route, i64::MAX).unwrap();
            assert_eq!(jobs.len(), 1);
            assert_eq!(jobs[0].peer, peer);
            assert_eq!(jobs[0].key, key);
            assert_eq!(jobs[0].route, route);
        }
    }

    #[tokio::test]
    async fn unsigned_not_found_keeps_legacy_keys_eligible_for_rechecking() {
        use crate::storage::peer_verification::PeerVerification;
        let (_dir, service, owner) = fixture();
        let peer = crate::domain::UserId::new().to_string();
        let key = [5; 33];
        service.key_store.save_identity(&peer, &key).unwrap();
        let conn = service.settings_repo.database().connection().unwrap();
        conn.execute(
            "UPDATE peer_key_transparency SET state='legacy-unlisted' WHERE account=?1",
            [&peer],
        ).unwrap();
        drop(conn);

        service.require_sendable_key(&peer, &key).unwrap();
        let job = service.key_store.due_kt_jobs(KtRoute::Normal, i64::MAX).unwrap().remove(0);
        assert_eq!(
            service.actor.complete_kt_job(owner.to_string(), job, KtVerdict::NotInLog).await.unwrap(),
            KtUpdate::Stale,
        );
        assert_eq!(
            service.key_store.peer_verification(&peer, &key).unwrap(),
            PeerVerification::Legacy,
        );
        service.require_sendable_key(&peer, &key).unwrap();
        assert!(!service.key_store.has_blocking_kt_job(&peer, KtRoute::Normal).unwrap());
        assert!(service.key_store.due_kt_jobs(KtRoute::Normal, chrono::Utc::now().timestamp()).unwrap().is_empty());
        assert_eq!(service.key_store.due_kt_jobs(KtRoute::Normal, i64::MAX).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn pinning_a_rejected_key_inbound_schedules_the_identity_prompt() {
        use crate::storage::peer_verification::PeerVerification;
        let (dir, service, owner) = fixture();
        let peer = crate::domain::UserId::new();
        let rejected = [5; 33];
        let logged = [6; 33];
        service.key_store.enqueue_kt(&peer.to_string(), &rejected, KtRoute::Normal).unwrap();
        let job = service.key_store.due_kt_jobs(KtRoute::Normal, i64::MAX).unwrap().remove(0);
        assert_eq!(
            service.actor.complete_kt_job(owner.to_string(), job, KtVerdict::Mismatch(logged.to_vec())).await.unwrap(),
            KtUpdate::Refused,
        );
        assert_eq!(
            service.key_store.peer_verification(&peer.to_string(), &rejected).unwrap(),
            PeerVerification::Rejected,
        );

        // The inbound commit pins the rejected key. The routed and deferred
        // receive paths must both be able to schedule it.
        service.key_store.save_routed_identity(&peer.to_string(), &rejected, Some(KtRoute::Normal)).unwrap();
        assert_eq!(service.key_store.due_kt_jobs(KtRoute::Normal, i64::MAX).unwrap().len(), 1);
        let db = crate::storage::Database::open(&dir.path().join("keys.db"), "fixture").unwrap();
        db.connection().unwrap().execute("DELETE FROM peer_kt_jobs", []).unwrap();
        service.key_store.recover_kt_jobs(&owner.to_string()).unwrap();
        let job = service.key_store.due_kt_jobs(KtRoute::AnonymousTor, i64::MAX).unwrap().remove(0);
        let events = EventBus::new();
        let mut rx = events.subscribe_domain();
        settle_peer_verification(&service, &events, &owner.to_string(), job, KtVerdict::Mismatch(logged.to_vec())).await.unwrap();
        assert!(matches!(
            rx.try_recv().unwrap(),
            DomainEvent::PeerIdentityChanged { peer_user_id, attributed: false } if peer_user_id == peer
        ));
        assert_eq!(service.pending_identity_change(&peer.to_string()).unwrap().unwrap().0, logged);
    }

    #[tokio::test]
    async fn pending_key_blocks_single_and_wildcard_bundle_refetch() {
        let (_dir, service, _) = fixture();
        let peer = crate::domain::UserId::new().to_string();
        service.key_store.enqueue_kt(&peer, &[5; 33], KtRoute::Normal).unwrap();
        let pool = GrpcClientPool::new("http://127.0.0.1:1").unwrap();
        let mut auth = pool.auth_client();
        for result in [
            service.ensure_session(&mut auth, &peer, 1).await,
            service.ensure_sessions_all_devices(&mut auth, &peer).await.map(|_| ()),
        ] {
            assert!(matches!(
                result,
                Err(ServiceError::Crypto(CryptoError::KeyVerificationPending { peer_id })) if peer_id == peer
            ));
        }
    }

    #[tokio::test]
    async fn proven_mismatch_parks_the_candidate_and_emits_the_existing_prompt() {
        let (_dir, service, owner) = fixture();
        let peer = crate::domain::UserId::new();
        let old = [5; 33];
        let new = [6; 33];
        service
            .key_store
            .save_identity(&peer.to_string(), &old)
            .unwrap();
        service
            .key_store
            .enqueue_kt(&peer.to_string(), &old, KtRoute::Normal)
            .unwrap();
        let mut jobs = service
            .key_store
            .due_kt_jobs(KtRoute::Normal, i64::MAX)
            .unwrap();
        let events = EventBus::new();
        let mut rx = events.subscribe_domain();
        settle_peer_verification(
            &service,
            &events,
            &owner.to_string(),
            jobs.remove(0),
            KtVerdict::Mismatch(new.to_vec()),
        )
        .await
        .unwrap();

        assert!(
            matches!(rx.try_recv().unwrap(), DomainEvent::PeerIdentityChanged {
            peer_user_id, attributed: false,
        } if peer_user_id == peer)
        );
        assert!(
            matches!(rx.try_recv().unwrap(), DomainEvent::PeerKeyVerificationUpdated {
            peer_user_id,
        } if peer_user_id == peer)
        );
        assert_eq!(
            service
                .get_peer_identity(&peer.to_string())
                .unwrap()
                .unwrap(),
            old
        );
        assert_eq!(
            service
                .pending_identity_change(&peer.to_string())
                .unwrap()
                .unwrap()
                .0,
            new
        );
    }

    #[tokio::test]
    async fn a_late_archived_message_does_not_prompt_to_restore_its_old_key() {
        let (_dir, service, owner) = fixture();
        let peer = crate::domain::UserId::new().to_string();
        let archived = [5; 33];
        let pinned = [6; 33];
        service.key_store.save_identity(&peer, &pinned).unwrap();
        service
            .key_store
            .enqueue_archived_kt(&peer, &pinned, &archived, KtRoute::AnonymousTor)
            .unwrap();
        let mut jobs = service
            .key_store
            .due_kt_jobs(KtRoute::AnonymousTor, i64::MAX)
            .unwrap();
        let events = EventBus::new();
        let mut rx = events.subscribe_domain();
        settle_peer_verification(
            &service,
            &events,
            &owner.to_string(),
            jobs.remove(0),
            KtVerdict::Mismatch(pinned.to_vec()),
        )
        .await
        .unwrap();
        assert!(rx.try_recv().is_err());
        assert!(service.pending_identity_change(&peer).unwrap().is_none());
        service
            .key_store
            .enqueue_archived_kt(&peer, &pinned, &archived, KtRoute::AnonymousTor)
            .unwrap();
        assert!(
            service
                .key_store
                .due_kt_jobs(KtRoute::AnonymousTor, i64::MAX)
                .unwrap()
                .is_empty()
        );
    }

    /// The anchor signed membership checks run after the inclusion proof: a head that forks from the pin is refused
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
        let pin = kt_anchor::lock_pin().await;
        svc.check_tree_head_consistency(&pin, no_fetch, "bob", &head(1))
            .await
            .unwrap();
        let err = svc
            .check_tree_head_consistency(&pin, no_fetch, "bob", &head(2))
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

        let pin = kt_anchor::lock_pin().await;
        for path in ["KT", "KT(anon)"] {
            let err = svc
                .judge_kt_proof(&pin, path, "bob", &[5u8; 33], Ok(None), no_fetch)
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
