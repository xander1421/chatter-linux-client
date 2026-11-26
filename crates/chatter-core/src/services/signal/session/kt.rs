use super::*;
use crate::error::NetworkError;
use crate::events::{DomainEvent, EventBus};
use crate::network::grpc::{AnonymousKtClient, client_pool::GrpcClientPool};
use crate::state::AuthState;
use crate::storage::peer_kt_heads::PeerKtHead;
use crate::storage::peer_verification::{KtJob, KtRoute, KtUpdate, KtVerdict};
use crate::services::signal::kt_anchor::gossip::{self, Verdict};
use futures::StreamExt;
use parking_lot::Mutex;
use std::time::Duration;

pub(crate) fn spawn_peer_verifier(
    service: Arc<SignalService>,
    pool: Arc<GrpcClientPool>,
    events: EventBus,
    auth: AuthState,
    transport_epoch: tokio::sync::watch::Receiver<u64>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let last_history_warning = Mutex::new(None);
        tokio::join!(
            run_peer_verifier(
                &service,
                &pool,
                &events,
                &auth,
                KtRoute::Normal,
                transport_epoch.clone(),
                &last_history_warning,
            ),
            run_peer_verifier(
                &service,
                &pool,
                &events,
                &auth,
                KtRoute::AnonymousTor,
                transport_epoch,
                &last_history_warning,
            ),
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
    last_history_warning: &Mutex<Option<(crate::domain::UserId, u64)>>,
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
        let heads = match service
            .key_store
            .due_peer_kt_heads(route, chrono::Utc::now().timestamp())
        {
            Ok(heads) => heads,
            Err(error) => {
                tracing::warn!(%error, "Could not load contacts' transparency heads");
                Vec::new()
            }
        };
        let owner_key = &owner_key;
        // At most one page of heads per tick, checked one at a time.
        let reconcile = async move {
            for job in heads {
                let verdict = service.reconcile_peer_head(pool, &job).await;
                let state = auth.get();
                if state.current_user_id != Some(owner)
                    || state.session_ended
                    || state.session_revision != revision
                {
                    return;
                }
                settle_peer_head(service, events, job, verdict);
            }
        };
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
                        if matches!(
                            &error,
                            ServiceError::Crypto(CryptoError::KeyTransparencyHistoryUnverified)
                        ) {
                            let mut warned = last_history_warning.lock();
                            if *warned != Some((owner, revision)) {
                                *warned = Some((owner, revision));
                                events.emit_domain(DomainEvent::KeyTransparencyHistoryUnverified);
                            }
                        }
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
            // Two selects, not one join: rustc cannot prove the joined future
            // Send under tokio::spawn (rust-lang/rust#100013).
            let changed = tokio::select! {
                biased;
                _ = transport_epoch.changed() => true,
                _ = verify => false,
            };
            if !changed {
                tokio::select! {
                    biased;
                    _ = transport_epoch.changed() => {}
                    _ = reconcile => {}
                }
            }
        } else {
            verify.await;
            reconcile.await;
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

/// Record a contact-head verdict. A split blocks sends to the peer and raises
/// the conversation's identity prompt; an unanswered check stays queued.
fn settle_peer_head(
    service: &SignalService,
    events: &EventBus,
    job: PeerKtHead,
    verdict: Result<Verdict, ServiceError>,
) {
    let split = match verdict {
        Ok(Verdict::Split) => true,
        Ok(Verdict::Consistent) => false,
        Ok(Verdict::Invalid) => {
            tracing::warn!(peer = %job.peer, "Contact shared a transparency head without a valid signature");
            false
        }
        Err(error) => {
            tracing::debug!(%error, peer = %job.peer, "Contact head check will retry");
            match service
                .key_store
                .delay_peer_kt_head(&job, chrono::Utc::now().timestamp())
            {
                // An operator withholding the proof looks like an outage.
                Ok(3) => {
                    events.emit_domain(DomainEvent::KeyTransparencyHistoryUnverified);
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "Could not reschedule a contact head check"),
            }
            return;
        }
    };
    match service.key_store.settle_peer_kt_head(&job, split) {
        Ok(true) => {
            tracing::error!(peer = %job.peer, route = ?job.route, "Key transparency split view detected");
            if let Ok(peer_user_id) = job.peer.parse() {
                events.emit_domain(DomainEvent::PeerIdentityChanged {
                    peer_user_id,
                    attributed: false,
                });
            }
        }
        Ok(false) => {}
        Err(error) => tracing::warn!(%error, "Could not store a contact head verdict"),
    }
}

fn job_client(pool: &GrpcClientPool, route: KtRoute) -> Result<AnonymousKtClient, NetworkError> {
    match route {
        KtRoute::Normal => Ok(pool.account_kt_client()),
        KtRoute::AnonymousTor => pool.secret_kt_client(),
    }
}

impl SignalService {
    /// Network verification boundary for the durable exact-key queue.
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
        let (scope, deadline) = match route {
            KtRoute::Normal => (kt_anchor::PinScope::Account, kt_anchor::PIN_RPC_DEADLINE),
            KtRoute::AnonymousTor => (kt_anchor::PinScope::Anonymous, kt_anchor::TOR_RPC_DEADLINE),
        };
        match self.lookup_identity(&mut client, scope, peer, deadline).await? {
            Some(actual) if actual == key => Ok(KtVerdict::Verified),
            Some(actual) => Ok(KtVerdict::Mismatch(actual)),
            None => Ok(KtVerdict::NotInLog),
        }
    }

    /// Check a contact's shared head against this device's pin on its route.
    async fn reconcile_peer_head(
        &self,
        pool: &GrpcClientPool,
        job: &PeerKtHead,
    ) -> Result<Verdict, ServiceError> {
        use prost::Message;
        if job.route == KtRoute::Normal && self.actor.kt_route() == KtRoute::AnonymousTor {
            return Err(NetworkError::Unavailable.into());
        }
        let head = chatter_proto::content_v1::KtHeadGossip::decode(job.head.as_slice())
            .map_err(|_| ServiceError::Validation("invalid stored contact head".into()))?;
        let mut client = job_client(pool, job.route)?;
        let deadline = match job.route {
            KtRoute::Normal => kt_anchor::PIN_RPC_DEADLINE,
            KtRoute::AnonymousTor => kt_anchor::TOR_RPC_DEADLINE,
        };
        Ok(gossip::reconcile(
            &mut client,
            &self.settings_repo,
            gossip::scope(job.route),
            &head,
            deadline,
        )
        .await?)
    }

    /// Verify the key claimed by a signed channel membership change.
    pub async fn verify_identity_key_transparency(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        identity_key: &[u8],
    ) -> Result<(), ServiceError> {
        let actual = self
            .verified_identity_key_from_kt(auth_client, recipient_id)
            .await?;
        if actual != identity_key {
            return Err(ServiceError::Validation(
                "identity key differs from the verified transparency value".into(),
            ));
        }
        Ok(())
    }

    /// Return the key committed in this device's pinned transparency log.
    pub async fn verified_identity_key_from_kt(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
    ) -> Result<Vec<u8>, ServiceError> {
        self.lookup_identity(
            auth_client.key_transparency_client(),
            kt_anchor::PinScope::Account,
            recipient_id,
            kt_anchor::PIN_RPC_DEADLINE,
        )
        .await?
        .ok_or_else(|| {
            CryptoError::NotInTransparencyLog {
                peer_id: recipient_id.to_owned(),
            }
            .into()
        })
    }

    async fn lookup_identity(
        &self,
        client: &mut AnonymousKtClient,
        scope: kt_anchor::PinScope,
        label: &str,
        deadline: Duration,
    ) -> Result<Option<Vec<u8>>, ServiceError> {
        let value = kt_anchor::search(client, &self.settings_repo, scope, label, deadline).await?;
        if let Some(value) = value.as_deref() {
            libsignal_protocol::IdentityKey::try_from(value)
                .map_err(|_| CryptoError::InvalidKeyFormat)?;
        }
        Ok(value)
    }
}

#[cfg(test)]
mod kt_pin_tests {
    use super::*;

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
    async fn a_contact_head_split_blocks_sends_until_the_user_accepts() {
        use prost::Message;
        let (_dir, service, owner) = fixture();
        let peer = crate::domain::UserId::new();
        let peer_key = peer.to_string();
        let key = [5; 33];
        service.key_store.save_identity(&peer_key, &key).unwrap();
        service.key_store.mark_peer_verified(&peer_key, &key).unwrap();
        service.require_sendable_key(&peer_key, &key).unwrap();

        let head = chatter_proto::content_v1::KtHeadGossip {
            tree_size: 4,
            timestamp: 1,
            signature: vec![1; 64],
            root: vec![2; 32],
        }
        .encode_to_vec();
        // Our own devices' heads are not contact evidence.
        service.key_store.record_peer_kt_head(&owner.to_string(), KtRoute::Normal, &head).unwrap();
        assert!(service.key_store.due_peer_kt_heads(KtRoute::Normal, i64::MAX).unwrap().is_empty());

        service.key_store.record_peer_kt_head(&peer_key, KtRoute::Normal, &head).unwrap();
        let events = EventBus::new();
        let mut rx = events.subscribe_domain();
        // An unanswered check stays queued and warns on the third failure.
        for attempt in 1..=3 {
            let job = service.key_store.due_peer_kt_heads(KtRoute::Normal, i64::MAX).unwrap().remove(0);
            assert_eq!(job.attempts, attempt - 1);
            settle_peer_head(&service, &events, job, Err(NetworkError::Timeout.into()));
        }
        assert!(matches!(rx.try_recv().unwrap(), DomainEvent::KeyTransparencyHistoryUnverified));
        service.require_sendable_key(&peer_key, &key).unwrap();

        let job = service.key_store.due_peer_kt_heads(KtRoute::Normal, i64::MAX).unwrap().remove(0);
        settle_peer_head(&service, &events, job, Ok(Verdict::Split));
        assert!(matches!(
            rx.try_recv().unwrap(),
            DomainEvent::PeerIdentityChanged { peer_user_id, attributed: false } if peer_user_id == peer
        ));
        assert!(matches!(
            service.require_sendable_key(&peer_key, &key),
            Err(CryptoError::UntrustedIdentity { new_identity_key: None, .. })
        ));

        // The checked head does not queue again, so accepting is not undone by
        // the peer's next message; a different head is checked again.
        service.key_store.record_peer_kt_head(&peer_key, KtRoute::Normal, &head).unwrap();
        assert!(service.key_store.due_peer_kt_heads(KtRoute::Normal, i64::MAX).unwrap().is_empty());
        assert!(service.key_store.clear_peer_kt_split(&peer_key).unwrap());
        service.require_sendable_key(&peer_key, &key).unwrap();
        let mut next = chatter_proto::content_v1::KtHeadGossip::decode(head.as_slice()).unwrap();
        next.tree_size = 5;
        service.key_store.record_peer_kt_head(&peer_key, KtRoute::Normal, &next.encode_to_vec()).unwrap();
        assert_eq!(service.key_store.due_peer_kt_heads(KtRoute::Normal, i64::MAX).unwrap().len(), 1);
        assert!(service.key_store.due_peer_kt_heads(KtRoute::AnonymousTor, i64::MAX).unwrap().is_empty());
    }

    #[tokio::test]
    async fn shield_and_secret_verification_never_fall_back_when_onion_is_missing() {
        use std::sync::atomic::Ordering;
        let (_dir, service, _) = fixture();
        let pool = GrpcClientPool::new("https://127.0.0.1:1").unwrap();
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
        service.settings_repo.database().connection().unwrap().execute(
            "UPDATE peer_key_transparency SET state='legacy' WHERE account=?1",
            [&peer],
        ).unwrap();

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
        service.key_store.enqueue_kt(&peer, &[5; 33], KtRoute::AnonymousTor).unwrap();
        let pool = GrpcClientPool::new("https://127.0.0.1:1").unwrap();
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
        assert!(service.key_store.due_kt_jobs(KtRoute::Normal, i64::MAX).unwrap()
            .iter().any(|job| job.peer == peer && job.key == [5; 33]));
        let other = crate::domain::UserId::new().to_string();
        service.key_store.enqueue_kt(&other, &[6; 33], KtRoute::Normal).unwrap();
        assert!(matches!(
            service.for_anonymous_route().require_bundle_lookup_ready(&other),
            Err(ServiceError::Crypto(CryptoError::KeyVerificationPending { peer_id })) if peer_id == other
        ));
        assert!(service.key_store.due_kt_jobs(KtRoute::AnonymousTor, i64::MAX).unwrap()
            .iter().any(|job| job.peer == other && job.key == [6; 33]));
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
            .enqueue_archived_kt(&peer, &pinned, &archived, KtRoute::AnonymousTor, 0)
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
            .enqueue_archived_kt(&peer, &pinned, &archived, KtRoute::AnonymousTor, 0)
            .unwrap();
        assert!(
            service
                .key_store
                .due_kt_jobs(KtRoute::AnonymousTor, i64::MAX)
                .unwrap()
                .is_empty()
        );
    }
}
