use super::*;
use crate::error::NetworkError;
use crate::events::{DomainEvent, EventBus};
use crate::network::grpc::{AnonymousKtClient, client_pool::GrpcClientPool};
use crate::state::AuthState;
use crate::storage::peer_kt_heads::{HeadSettle, PeerKtHead};
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
        // Identity lookups on the Normal route wait out Shield. Head checks
        // carry no identity and follow Shield onto Tor.
        let jobs = if route == KtRoute::Normal && service.actor.kt_route() == KtRoute::AnonymousTor {
            Vec::new()
        } else {
            match service
                .key_store
                .due_kt_jobs(route, chrono::Utc::now().timestamp())
            {
                Ok(jobs) => jobs,
                Err(error) => {
                    tracing::warn!(%error, "Could not load pending key verification");
                    continue;
                }
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
        // One batch of due heads on one fresh connection, identical heads
        // checked once, the largest first.
        let reconcile = async move {
            if heads.is_empty() {
                return;
            }
            let (mut client, scope) = match head_client(service, pool, route) {
                Ok(client) => client,
                Err(error) => {
                    tracing::debug!(%error, ?route, "Contact head checks wait for their route");
                    for job in &heads {
                        delay_peer_head(service, job);
                    }
                    return;
                }
            };
            for (head, jobs) in batch_heads(heads) {
                let verdict = service
                    .reconcile_peer_head(&mut client, scope, &head, owner_key)
                    .await;
                let state = auth.get();
                if state.current_user_id != Some(owner)
                    || state.session_ended
                    || state.session_revision != revision
                {
                    return;
                }
                for job in jobs {
                    settle_peer_head(service, events, owner, job, &verdict);
                }
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

/// Group due heads by their exact bytes, largest tree first: one check covers
/// every account that shared the same head.
fn batch_heads(heads: Vec<PeerKtHead>) -> Vec<(Vec<u8>, Vec<PeerKtHead>)> {
    use prost::Message;
    let mut batch: Vec<(Vec<u8>, Vec<PeerKtHead>)> = Vec::new();
    for job in heads {
        match batch.iter_mut().find(|(head, _)| *head == job.head) {
            Some((_, jobs)) => jobs.push(job),
            None => batch.push((job.head.clone(), vec![job])),
        }
    }
    batch.sort_by_key(|(head, _)| {
        std::cmp::Reverse(
            chatter_proto::content_v1::KtHeadGossip::decode(head.as_slice())
                .map(|head| head.tree_size)
                .unwrap_or(0),
        )
    });
    batch
}

fn delay_peer_head(service: &SignalService, job: &PeerKtHead) {
    if let Err(error) = service
        .key_store
        .delay_peer_kt_head(job, chrono::Utc::now().timestamp())
    {
        tracing::warn!(%error, "Could not reschedule a contact head check");
    }
}

/// Record a contact-head verdict. Events fire only when the block changes.
fn settle_peer_head(
    service: &SignalService,
    events: &EventBus,
    owner: crate::domain::UserId,
    job: PeerKtHead,
    verdict: &Result<Verdict, ServiceError>,
) {
    let outcome = match verdict {
        Ok(Verdict::Consistent) => HeadSettle::Consistent,
        Ok(Verdict::Split) => HeadSettle::Split,
        Ok(Verdict::Withheld) => HeadSettle::Withheld,
        Ok(Verdict::Invalid) => {
            tracing::warn!(peer = %job.peer, "An account shared a transparency head without a valid signature");
            HeadSettle::Invalid
        }
        Ok(Verdict::Skipped) => {
            delay_peer_head(service, &job);
            return;
        }
        Ok(Verdict::Undecided) | Err(_) => {
            if let Err(error) = verdict {
                tracing::debug!(%error, peer = %job.peer, "Contact head check will retry");
            }
            // The account pin is only fetched over identified requests.
            let identified_at = kt_anchor::verified_at(&service.settings_repo, kt_anchor::PinScope::Account)
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, "Could not read when identified transparency last verified");
                    None
                });
            HeadSettle::Undecided { identified_at }
        }
    };
    match service
        .key_store
        .settle_peer_kt_head(&job, outcome, chrono::Utc::now().timestamp())
    {
        Ok(true) => {
            tracing::warn!(peer = %job.peer, route = ?job.route, ?outcome, "Key transparency block changed");
            let Ok(peer_user_id) = job.peer.parse::<crate::domain::UserId>() else {
                return;
            };
            if peer_user_id == owner {
                events.emit_domain(DomainEvent::OwnIdentityChanged { user_id: owner });
            } else {
                events.emit_domain(DomainEvent::PeerIdentityChanged {
                    peer_user_id,
                    attributed: false,
                });
                // A raised or cleared streak warning shows in the account banner.
                if matches!(outcome, HeadSettle::Consistent | HeadSettle::Undecided { .. }) {
                    events.emit_domain(DomainEvent::OwnIdentityChanged { user_id: owner });
                }
            }
        }
        Ok(false) => {}
        Err(error) => tracing::warn!(%error, "Could not store a contact head verdict"),
    }
}

/// The connection and pin for one batch of head checks. They never share a
/// connection with identified queries. Normal heads use a fresh
/// unauthenticated connection per batch: direct from the user's IP outside
/// Shield, its own Tor circuit in Shield. Anonymous heads use the secret Tor
/// route. Over Tor the check uses and advances only the anonymous pin, so no
/// account-route size ever reaches a Tor check.
fn head_client(
    service: &SignalService,
    pool: &GrpcClientPool,
    route: KtRoute,
) -> Result<(AnonymousKtClient, kt_anchor::PinScope), NetworkError> {
    use crate::network::grpc::client_pool::TransportMode;
    match route {
        KtRoute::AnonymousTor => Ok((pool.secret_kt_client()?, kt_anchor::PinScope::Anonymous)),
        KtRoute::Normal => {
            let onion = pool.transport_mode() == TransportMode::Onion;
            let shield = service.actor.kt_route() == KtRoute::AnonymousTor;
            if onion != shield {
                // Mid-switch: neither pin matches the connection yet.
                return Err(NetworkError::Unavailable);
            }
            let scope = if onion {
                kt_anchor::PinScope::Anonymous
            } else {
                kt_anchor::PinScope::Account
            };
            Ok((pool.fresh_anonymous_kt_client(), scope))
        }
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

    /// Check a shared head against this device's pin on the route the sender
    /// attached it for.
    async fn reconcile_peer_head(
        &self,
        client: &mut AnonymousKtClient,
        scope: kt_anchor::PinScope,
        head: &[u8],
        owner: &str,
    ) -> Result<Verdict, ServiceError> {
        use prost::Message;
        let head = chatter_proto::content_v1::KtHeadGossip::decode(head)
            .map_err(|_| ServiceError::Validation("invalid stored contact head".into()))?;
        let step = match scope {
            kt_anchor::PinScope::Account => kt_anchor::PIN_RPC_DEADLINE,
            kt_anchor::PinScope::Anonymous => kt_anchor::TOR_RPC_DEADLINE,
        };
        Ok(gossip::reconcile(client, &self.settings_repo, scope, &head, owner, step).await?)
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

    fn head(size: u64) -> Vec<u8> {
        use prost::Message;
        chatter_proto::content_v1::KtHeadGossip {
            tree_size: size,
            timestamp: 1,
            signature: vec![1; 64],
            root: vec![2; 32],
            anonymous: false,
        }
        .encode_to_vec()
    }

    fn due_on(service: &SignalService, route: KtRoute) -> crate::storage::peer_kt_heads::PeerKtHead {
        service.key_store.due_peer_kt_heads(route, i64::MAX).unwrap().remove(0)
    }

    fn due(service: &SignalService) -> crate::storage::peer_kt_heads::PeerKtHead {
        due_on(service, KtRoute::Normal)
    }

    fn sendable_peer() -> (tempfile::TempDir, Arc<SignalService>, crate::domain::UserId, crate::domain::UserId) {
        let (dir, service, owner) = fixture();
        let peer = crate::domain::UserId::new();
        service.key_store.save_identity(&peer.to_string(), &[5; 33]).unwrap();
        service.key_store.mark_peer_verified(&peer.to_string(), &[5; 33]).unwrap();
        service.require_sendable_key(&peer.to_string(), &[5; 33]).unwrap();
        (dir, service, owner, peer)
    }

    /// The gate for one send, as an actor encryption command sees it.
    fn sends_as(service: &SignalService, peer: &crate::domain::UserId, message: &[u8], user: bool) -> bool {
        let mut store = crate::crypto::SignalStore::new(service.key_store.clone());
        store.set_send(message, user);
        store.require_sendable_key(&peer.to_string(), &[5; 33]).is_ok()
    }

    fn sends(service: &SignalService, peer: &crate::domain::UserId, message: &[u8]) -> bool {
        sends_as(service, peer, message, true)
    }

    fn blocked(service: &SignalService, peer: &crate::domain::UserId) -> bool {
        matches!(
            service.require_sendable_key(&peer.to_string(), &[5; 33]),
            Err(CryptoError::UntrustedIdentity { new_identity_key: None, .. })
        )
    }

    fn changed_for(rx: &mut tokio::sync::broadcast::Receiver<DomainEvent>, peer: crate::domain::UserId) -> bool {
        matches!(
            rx.try_recv(),
            Ok(DomainEvent::PeerIdentityChanged { peer_user_id, attributed: false }) if peer_user_id == peer
        )
    }

    #[tokio::test]
    async fn withheld_checks_fail_closed_and_send_anyway_covers_one_message() {
        let (_dir, service, owner, peer) = sendable_peer();
        let events = EventBus::new();
        let mut rx = events.subscribe_domain();
        let withheld = || Ok(Verdict::Withheld);

        service.key_store.record_peer_kt_head(&peer.to_string(), KtRoute::Normal, &head(4), 0).unwrap();
        // A short outage teaches nothing and counts toward nothing.
        settle_peer_head(&service, &events, owner, due(&service), &Err(NetworkError::Timeout.into()));
        settle_peer_head(&service, &events, owner, due(&service), &Ok(Verdict::Undecided));
        for _ in 0..2 {
            settle_peer_head(&service, &events, owner, due(&service), &withheld());
        }
        assert!(rx.try_recv().is_err());
        assert!(!blocked(&service, &peer));

        // The peer's next head keeps the count, so the third check fails closed.
        service.key_store.record_peer_kt_head(&peer.to_string(), KtRoute::Normal, &head(5), 0).unwrap();
        settle_peer_head(&service, &events, owner, due(&service), &withheld());
        assert!(changed_for(&mut rx, peer));
        assert!(blocked(&service, &peer));
        assert!(!sends(&service, &peer, b"first"));
        settle_peer_head(&service, &events, owner, due(&service), &withheld());
        assert!(rx.try_recv().is_err(), "a standing block does not warn again");

        // Send anyway once: the first message spends the permit; its other
        // devices and retries still go, the next message does not.
        assert!(service.key_store.accept_peer_kt_block(&peer.to_string()).unwrap());
        // Receipts, typing and group sends never use the permit.
        assert!(!sends_as(&service, &peer, b"receipt", false));
        assert!(sends(&service, &peer, b"first"));
        assert!(sends(&service, &peer, b"first"));
        assert!(!sends(&service, &peer, b"second"));
        assert!(!blocked(&service, &peer), "session setup neither needs nor spends it");

        // A consistent check on another route does not clear this block.
        service.key_store.record_peer_kt_head(&peer.to_string(), KtRoute::AnonymousTor, &head(6), 0).unwrap();
        for size in 6..9 {
            service.key_store.record_peer_kt_head(&peer.to_string(), KtRoute::AnonymousTor, &head(size), 0).unwrap();
            settle_peer_head(&service, &events, owner, due_on(&service, KtRoute::AnonymousTor), &Ok(Verdict::Consistent));
        }
        assert!(rx.try_recv().is_err());
        assert!(!sends(&service, &peer, b"third"));

        // Three consistent checks on the blocking route clear it and the permit.
        for (index, size) in (9..12).enumerate() {
            service.key_store.record_peer_kt_head(&peer.to_string(), KtRoute::Normal, &head(size), 0).unwrap();
            settle_peer_head(&service, &events, owner, due(&service), &Ok(Verdict::Consistent));
            assert_eq!(changed_for(&mut rx, peer), index == 2);
        }
        assert!(sends(&service, &peer, b"third"));
        service.key_store.record_peer_kt_head(&peer.to_string(), KtRoute::Normal, &head(12), 0).unwrap();
        settle_peer_head(&service, &events, owner, due(&service), &withheld());
        assert!(sends(&service, &peer, b"fourth"));
    }

    #[test]
    fn a_long_undecided_streak_only_warns_and_withheld_checks_still_block() {
        use crate::storage::peer_kt_heads::{HeadSettle, UNDECIDED_LIMIT, UNDECIDED_WINDOW_SECS, WITHHELD_LIMIT};
        let (_dir, service, _owner, peer) = sendable_peer();
        let keys = &service.key_store;
        keys.record_peer_kt_head(&peer.to_string(), KtRoute::Normal, &head(4), 0).unwrap();
        let streak = |start: i64, identified_at: Option<i64>| {
            let mut changed = false;
            for step in 0..UNDECIDED_LIMIT as i64 {
                let at = start + step * UNDECIDED_WINDOW_SECS / (UNDECIDED_LIMIT as i64 - 1);
                changed |= keys
                    .settle_peer_kt_head(&due(&service), HeadSettle::Undecided { identified_at }, at)
                    .unwrap();
            }
            changed
        };
        // No identified request verified since the streak began: an outage.
        assert!(!streak(0, None));
        assert!(!streak(1_000_000, Some(-1)));
        assert!(!keys.kt_suspect_exists().unwrap());
        // The account route kept verifying: a warning, never a block.
        assert!(streak(2_000_000, Some(2_000_001)));
        assert!(keys.kt_suspect_exists().unwrap());
        assert!(!blocked(&service, &peer));
        assert!(!streak(3_000_000, Some(3_000_001)), "a standing warning does not warn again");
        // Control-bracketed withheld checks still fail closed and replace it.
        for _ in 0..WITHHELD_LIMIT {
            keys.settle_peer_kt_head(&due(&service), HeadSettle::Withheld, 4_000_000).unwrap();
        }
        assert!(blocked(&service, &peer));
        assert!(!keys.kt_suspect_exists().unwrap());
    }

    #[tokio::test]
    async fn a_split_blocks_until_accepted_and_our_own_devices_are_checked_too() {
        let (_dir, service, owner, peer) = sendable_peer();
        let events = EventBus::new();
        let mut rx = events.subscribe_domain();

        service.key_store.record_peer_kt_head(&peer.to_string(), KtRoute::Normal, &head(4), 0).unwrap();
        settle_peer_head(&service, &events, owner, due(&service), &Ok(Verdict::Split));
        assert!(changed_for(&mut rx, peer));
        assert!(blocked(&service, &peer));
        // Consistent checks never clear a split.
        for size in 5..9 {
            service.key_store.record_peer_kt_head(&peer.to_string(), KtRoute::Normal, &head(size), 0).unwrap();
            settle_peer_head(&service, &events, owner, due(&service), &Ok(Verdict::Consistent));
        }
        assert!(blocked(&service, &peer));

        // A checked head does not queue again, so accepting is not undone by
        // the peer's next message.
        service.key_store.record_peer_kt_head(&peer.to_string(), KtRoute::Normal, &head(8), 0).unwrap();
        assert!(service.key_store.due_peer_kt_heads(KtRoute::Normal, i64::MAX).unwrap().is_empty());
        assert!(service.key_store.accept_peer_kt_block(&peer.to_string()).unwrap());
        assert!(!blocked(&service, &peer));

        // Our own devices' heads are checked; a split there raises our own warning.
        service.key_store.record_peer_kt_head(&owner.to_string(), KtRoute::Normal, &head(9), 0).unwrap();
        settle_peer_head(&service, &events, owner, due(&service), &Ok(Verdict::Split));
        assert!(matches!(rx.try_recv(), Ok(DomainEvent::OwnIdentityChanged { user_id }) if user_id == owner));
        // We cannot vouch for ourselves, so three consistent checks on that
        // route clear our own split instead of an accept.
        for (index, size) in (10..13).enumerate() {
            service.key_store.record_peer_kt_head(&owner.to_string(), KtRoute::Normal, &head(size), 0).unwrap();
            settle_peer_head(&service, &events, owner, due(&service), &Ok(Verdict::Consistent));
            assert_eq!(
                matches!(rx.try_recv(), Ok(DomainEvent::OwnIdentityChanged { user_id }) if user_id == owner),
                index == 2
            );
        }
        assert!(service.key_store.peer_kt_block(&owner.to_string()).unwrap().is_none());
    }

    #[test]
    fn a_pending_head_keeps_its_due_time_when_a_newer_one_arrives() {
        let (_dir, service, _owner, peer) = sendable_peer();
        let peer = peer.to_string();
        service.key_store.record_peer_kt_head(&peer, KtRoute::Normal, &head(4), 100).unwrap();
        service.key_store.record_peer_kt_head(&peer, KtRoute::Normal, &head(5), 9_000).unwrap();
        assert!(service.key_store.due_peer_kt_heads(KtRoute::Normal, 99).unwrap().is_empty());
        let jobs = service.key_store.due_peer_kt_heads(KtRoute::Normal, 100).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].head, head(5));
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
