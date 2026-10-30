//! The client side of the signed channel membership log (Design B): authoring
//! and submitting changes, ingesting the ones the server relays, catching up
//! after a gap, and bootstrapping a channel this device has no state for.
//!
//! Every state that reaches `verified_membership_repo` went through
//! `verified_state::verify_change` (or `bootstrap`) here. The verifier itself
//! is pure; this module owns the I/O around it, behind [`LogBackend`] so the
//! chain logic is testable without a server.
//!
//! Ingestion and authoring for one channel are serialised by a per-channel
//! lock, so a change relayed while we author cannot interleave with our commit.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use libsignal_protocol::{KeyPair, PrivateKey};
use rand::TryRngCore;

use super::verified_state::canonical::{INVITE_DOMAIN, signed_message};
use super::verified_state::{
    self as vs, CachedKtVerifier, IdentityKeyVerifier, KeyBytes, MemberStatus, MembershipState,
    UnsignedDisposition, UnsignedEvent, VerificationStatus, VerifiedChange, VerifiedChannel,
    VerifyError, rules,
};
use crate::domain::ids::ChannelId;
use crate::error::{NetworkError, ServiceError, StorageError};
use crate::network::grpc::channel_client::is_change_conflict;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::proto::chat_v1 as pb;
use crate::services::signal::SignalService;
use crate::storage::repositories::settings_repo::SettingsRepository;
use crate::storage::repositories::verified_membership_repo::VerifiedMembershipRepository;
use pb::channel_action::Action;

/// A submit refused as `ABORTED` (another change landed first) is retried
/// this many times in total, each after catching up and re-running intent.
pub const MAX_SUBMIT_ATTEMPTS: usize = 3;

/// A change dated further ahead of our clock than this is refused. Its
/// timestamp is the author's (it is what invite expiry is judged against, and
/// the server checks expiry by its own clock); a far-future one is a forged or
/// broken author clock.
pub const MAX_FUTURE_SKEW: Duration = Duration::from_secs(3600);

/// How long a positive KT answer for an author key is reused.
pub const KT_CACHE_TTL: Duration = Duration::from_secs(600);

const PAGE: i32 = 100;
/// Upper bound on pages one catch-up reads (100 changes each).
const MAX_PAGES: usize = 1000;

#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error(transparent)]
    Network(#[from] NetworkError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// A received change (or server state) failed verification. The channel
    /// is marked `VerificationFailed` and the user told.
    #[error("membership could not be verified: {0}")]
    Verify(VerifyError),
    /// Our own intended change breaks a rule of the current state (we lack
    /// the role, the target is gone, ...). Nothing was signed.
    #[error("change not allowed: {0}")]
    Intent(VerifyError),
    /// The author's KT lookup could not complete; nothing was decided.
    #[error("key transparency unavailable: {0}")]
    KtUnavailable(String),
    #[error("{0}")]
    Other(String),
}

impl From<ServiceError> for LogError {
    fn from(e: ServiceError) -> Self {
        match e {
            ServiceError::Network(n) => Self::Network(n),
            ServiceError::Storage(s) => Self::Storage(s),
            other => Self::Other(other.to_string()),
        }
    }
}

/// What a channel's members are told about their verified state. Rendered by
/// the UI as banners/notices; never decided there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MembershipNotice {
    /// A change failed verification: the channel is send-blocked.
    VerificationFailed { reason: String },
    /// The server showed a different change at a revision we hold.
    Fork,
    /// The server (unsigned) says this user joined; they get no keys.
    UnverifiedMember { user_id: String },
    /// The server (unsigned) says this user left; they are out of the
    /// distribution set until a signed change says otherwise.
    RemovedByServer { user_id: String },
    /// A verified change cleared an earlier verification failure: sending
    /// resumes.
    Verified,
}

impl MembershipNotice {
    /// Whether the notice reports a problem (as opposed to a recovery).
    pub fn is_problem(&self) -> bool {
        !matches!(self, Self::Verified)
    }

    /// The line shown to the user.
    pub fn user_text(&self) -> String {
        match self {
            Self::VerificationFailed { .. } => {
                "Channel membership could not be verified. Sending is paused.".to_string()
            }
            Self::Fork => "The server showed this channel's members differently to different people. Sending is paused.".to_string(),
            Self::UnverifiedMember { .. } => {
                "The server listed a member nobody in the channel added. They will not receive messages.".to_string()
            }
            Self::RemovedByServer { .. } => "The server removed a member without a signed change.".to_string(),
            Self::Verified => "Channel membership verified again. Sending resumed.".to_string(),
        }
    }
}

/// A verified change that was committed, for the display layer.
#[derive(Debug, Clone)]
pub struct Applied {
    pub channel_id: ChannelId,
    pub prev: Option<MembershipState>,
    pub new: MembershipState,
    pub author: String,
    /// Author's clock, Unix nanos.
    pub timestamp: u64,
    pub actions: Vec<pb::ChannelAction>,
    /// Applied while replaying a log this device had no state for (a fresh
    /// install catching up from genesis): history, not news. The display
    /// layer writes the roster but files no notices.
    pub backfill: bool,
}

/// The I/O the log needs: who we are, the identity-key signer, and the
/// server's log. Production: [`SignalLogBackend`].
#[async_trait]
pub trait LogBackend: Send + Sync {
    /// Our user id (lowercase UUID) and identity public key.
    async fn me(&self) -> Result<(String, KeyBytes), LogError>;
    /// Sign `actions` with our identity key: `(actions_bytes, signature)`.
    async fn sign(
        &self,
        actions: pb::channel_change::Actions,
    ) -> Result<(Vec<u8>, Vec<u8>), LogError>;
    async fn changes(
        &self,
        channel_id: &str,
        from_revision: u64,
        invite_pub: &[u8],
    ) -> Result<pb::GetChannelChangesResponse, NetworkError>;
    async fn state(
        &self,
        channel_id: &str,
        revision: u64,
        invite_pub: &[u8],
    ) -> Result<pb::MembershipState, NetworkError>;
    /// A verified change took someone out of the distribution set.
    async fn mark_for_rotation(&self, channel_id: &ChannelId);
    /// The identity key the server claims for `user_id` (KT-checked by the
    /// caller before it is vouched for in an `AddMember`).
    async fn claimed_identity_key(&self, user_id: &str) -> Result<KeyBytes, LogError>;

    async fn vouched_identity_key(
        &self,
        user_id: &str,
        kt: &dyn IdentityKeyVerifier,
    ) -> Result<KeyBytes, LogError> {
        let key = self.claimed_identity_key(user_id).await?;
        match kt.is_current_identity_key(user_id, &key).await {
            Ok(true) => Ok(key),
            Ok(false) => Err(LogError::Verify(VerifyError::KtMismatch)),
            Err(VerifyError::KtUnavailable(m)) => Err(LogError::KtUnavailable(m)),
            Err(e) => Err(LogError::Verify(e)),
        }
    }
}

/// Told about every committed change and every notice
/// (`ChannelEventService`: roster display, system messages, banners).
pub trait LogObserver: Send + Sync {
    fn applied(&self, applied: &Applied);
    fn notice(&self, channel_id: &ChannelId, notice: MembershipNotice);
}

/// Who removed us from a channel and why, from a verified signed change
/// ([`MembershipLog::verify_own_removal`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnRemoval {
    pub author: String,
    pub reason: pb::remove_member::Reason,
    /// The change's (author's) timestamp, Unix nanos: the notice's identity,
    /// shared with the live path so either files it once.
    pub timestamp: u64,
}

/// The state an authored change builds on.
#[derive(Clone, Copy)]
pub enum AuthorBase<'a> {
    /// Revision 1 of a new channel.
    Genesis,
    /// Our verified state (caught up from the server first if we have none).
    Verified,
    /// The server's current state, unverified: joining a channel we are not
    /// in. `invite_pub` authorises the read for a join-link holder. The join
    /// commits only to this state; peers' `(revision, state_hash)` stamps
    /// cross-check it later.
    Server { invite_pub: &'a [u8] },
}

/// What [`MembershipLog::author`]'s `intent` sees.
pub struct IntentContext<'a> {
    pub me: &'a str,
    pub state: Option<&'a MembershipState>,
}

pub struct MembershipLog {
    backend: Arc<dyn LogBackend>,
    kt: Arc<dyn IdentityKeyVerifier>,
    repo: VerifiedMembershipRepository,
    observer: parking_lot::RwLock<Option<Weak<dyn LogObserver>>>,
    locks: parking_lot::Mutex<HashMap<ChannelId, Arc<tokio::sync::Mutex<()>>>>,
}

/// One received change's fate.
enum Step {
    Applied,
    Duplicate,
    /// Revisions are missing before it: catch up.
    Gap,
}

fn now_nanos() -> u64 {
    chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u64
}

fn decode_actions(change: &pb::ChannelChange) -> Result<pb::channel_change::Actions, VerifyError> {
    <pb::channel_change::Actions as prost::Message>::decode(change.actions.as_slice())
        .map_err(|e| VerifyError::Malformed(format!("actions: {e}")))
}

/// Whether `actions` makes `me` a member-to-be: the only changes a device
/// with no state for the channel bootstraps from (`GetChannelState(N-1)` +
/// N). Anything else is caught up from genesis.
fn names_me(actions: &pb::channel_change::Actions, me: &str) -> bool {
    actions.actions.iter().any(|a| match &a.action {
        Some(Action::AddMember(m)) => m.user_id == me,
        Some(Action::JoinViaInvite(_) | Action::AcceptInvite(_)) => actions.author_user_id == me,
        _ => false,
    })
}

impl MembershipLog {
    pub fn new(
        backend: Arc<dyn LogBackend>,
        kt: Arc<dyn IdentityKeyVerifier>,
        repo: VerifiedMembershipRepository,
    ) -> Self {
        Self {
            backend,
            kt,
            repo,
            observer: parking_lot::RwLock::new(None),
            locks: Default::default(),
        }
    }

    pub fn set_observer(&self, observer: Weak<dyn LogObserver>) {
        *self.observer.write() = Some(observer);
    }

    fn observer(&self) -> Option<Arc<dyn LogObserver>> {
        self.observer.read().as_ref().and_then(Weak::upgrade)
    }

    fn notify(&self, channel_id: &ChannelId, notice: MembershipNotice) {
        if let Some(o) = self.observer() {
            o.notice(channel_id, notice);
        }
    }

    fn lock_for(&self, channel_id: &ChannelId) -> Arc<tokio::sync::Mutex<()>> {
        self.locks.lock().entry(*channel_id).or_default().clone()
    }

    /// The channel's verified state, if any.
    pub fn load(&self, channel_id: &ChannelId) -> Result<Option<VerifiedChannel>, StorageError> {
        self.repo.load(channel_id)
    }

    /// A relayed change (`ChannelEvent.channel_change`, or one from a
    /// membership RPC's stream): verify and commit it, catching up first
    /// when revisions are missing. A duplicate is a no-op.
    pub async fn ingest(
        &self,
        channel_id: &ChannelId,
        change: &pb::ChannelChange,
    ) -> Result<(), LogError> {
        let lock = self.lock_for(channel_id);
        let _guard = lock.lock().await;
        if let Step::Gap = self.step(channel_id, change, &[], false).await? {
            self.catch_up_locked(channel_id, &[]).await?;
        }
        Ok(())
    }

    /// Fetch and verify every change past our revision (from genesis if we
    /// have none). Reconnects, gap fills and a peer stamping a higher
    /// revision all land here.
    pub async fn catch_up(&self, channel_id: &ChannelId) -> Result<(), LogError> {
        let lock = self.lock_for(channel_id);
        let _guard = lock.lock().await;
        self.catch_up_locked(channel_id, &[]).await
    }

    async fn catch_up_locked(
        &self,
        channel_id: &ChannelId,
        invite_pub: &[u8],
    ) -> Result<(), LogError> {
        let cid = channel_id.to_string();
        let backfill = self.repo.load(channel_id)?.is_none();
        for _ in 0..MAX_PAGES {
            let local = self.repo.load(channel_id)?.map_or(0, |v| v.state.revision);
            let page = self.backend.changes(&cid, local + 1, invite_pub).await?;
            if page.changes.is_empty() {
                return Ok(());
            }
            for change in &page.changes {
                match self.step(channel_id, change, invite_pub, backfill).await? {
                    Step::Applied | Step::Duplicate => {}
                    Step::Gap => {
                        return Err(self.fail(
                            channel_id,
                            VerifyError::Malformed("server log is not contiguous".into()),
                        ));
                    }
                }
            }
            let now = self.repo.load(channel_id)?.map_or(0, |v| v.state.revision);
            if now >= page.current_revision || now == local {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Mark a verification failure and surface it.
    fn fail(&self, channel_id: &ChannelId, e: VerifyError) -> LogError {
        tracing::error!(%channel_id, error = %e, code = e.code(), "Channel membership change failed verification");
        if let Err(se) = self
            .repo
            .set_status(channel_id, VerificationStatus::VerificationFailed)
        {
            tracing::warn!(%channel_id, error = %se, "Could not record verification failure");
        }
        self.notify(
            channel_id,
            MembershipNotice::VerificationFailed {
                reason: e.to_string(),
            },
        );
        LogError::Verify(e)
    }

    /// Verify one change against local state and commit it.
    async fn step(
        &self,
        channel_id: &ChannelId,
        change: &pb::ChannelChange,
        invite_pub: &[u8],
        backfill: bool,
    ) -> Result<Step, LogError> {
        let cid = channel_id.to_string();
        let actions = decode_actions(change).map_err(|e| self.fail(channel_id, e))?;
        if actions.timestamp > now_nanos().saturating_add(MAX_FUTURE_SKEW.as_nanos() as u64) {
            return Err(self.fail(
                channel_id,
                VerifyError::Malformed("timestamp too far in the future".into()),
            ));
        }
        let local = self.repo.load(channel_id)?;

        // No state here yet: bootstrap from N-1 when N is about us, else
        // replay from genesis.
        let (prev, verified) = match &local {
            None if actions.revision > 1 => {
                let (me, _) = self.backend.me().await?;
                if !names_me(&actions, &me) {
                    return Ok(Step::Gap);
                }
                let base = self
                    .backend
                    .state(&cid, actions.revision - 1, invite_pub)
                    .await?;
                match vs::bootstrap(&cid, Some(&base), change, self.kt.as_ref()).await {
                    Ok(ok) => ok,
                    Err(VerifyError::KtUnavailable(m)) => return Err(LogError::KtUnavailable(m)),
                    Err(e) => return Err(self.fail(channel_id, e)),
                }
            }
            _ => {
                let prev = local.as_ref().map(|v| &v.state);
                match vs::verify_change(&cid, prev, change, self.kt.as_ref()).await {
                    Ok(v) => (prev.cloned(), v),
                    Err(VerifyError::Gap { .. }) => return Ok(Step::Gap),
                    Err(VerifyError::Stale { got, .. }) => {
                        return match self.repo.change_at(channel_id, got)? {
                            Some(held)
                                if held.actions == change.actions
                                    && held.signature == change.signature =>
                            {
                                Ok(Step::Duplicate)
                            }
                            // Never stored (accepted before a bootstrap): nothing to compare.
                            None => Ok(Step::Duplicate),
                            Some(_) => {
                                tracing::error!(%channel_id, revision = got, "Membership FORK: the server relayed a different change at a revision we hold");
                                self.repo.set_status(channel_id, VerificationStatus::Fork)?;
                                self.notify(channel_id, MembershipNotice::Fork);
                                Ok(Step::Duplicate)
                            }
                        };
                    }
                    Err(VerifyError::KtUnavailable(m)) => return Err(LogError::KtUnavailable(m)),
                    Err(e) => return Err(self.fail(channel_id, e)),
                }
            }
        };
        self.commit(channel_id, change, prev, verified, actions, backfill)
            .await?;
        Ok(Step::Applied)
    }

    async fn commit(
        &self,
        channel_id: &ChannelId,
        change: &pb::ChannelChange,
        prev: Option<MembershipState>,
        verified: VerifiedChange,
        actions: pb::channel_change::Actions,
        backfill: bool,
    ) -> Result<(), LogError> {
        if self.repo.commit(channel_id, change, &verified)? {
            self.notify(channel_id, MembershipNotice::Verified);
        }
        let (me, _) = self.backend.me().await?;
        let left: Vec<String> = verified
            .left_active(prev.as_ref())
            .into_iter()
            .filter(|u| *u != me)
            .collect();
        let still_in = verified
            .0
            .new_state
            .members
            .get(&me)
            .is_some_and(|m| m.status == MemberStatus::Active);
        if !left.is_empty() && still_in {
            tracing::info!(%channel_id, revision = verified.revision(), ?left, "Verified removal: sender key rotation required");
            self.backend.mark_for_rotation(channel_id).await;
        }
        if let Some(o) = self.observer() {
            o.applied(&Applied {
                channel_id: *channel_id,
                prev,
                new: verified.0.new_state,
                author: verified.0.author_user_id,
                timestamp: actions.timestamp,
                actions: actions.actions,
                backfill,
            });
        }
        Ok(())
    }

    /// Author a change and submit it, retrying on a conflict.
    ///
    /// `intent` builds the actions from the state they will apply to; it is
    /// re-run after every catch-up, so a retry re-validates what the user
    /// asked for against the newer state (and can refuse, e.g. the target
    /// already left). A `Verified` base is caught up before the first
    /// attempt. The change is previewed with the verifier's own rules
    /// (`rules::preview`) to fill `new_state_hash`, signed by the identity key
    /// through the Signal actor, and handed to `submit`. A submit refused as
    /// `ABORTED` catches up and tries again, at most [`MAX_SUBMIT_ATTEMPTS`]
    /// times. On success our own change is ingested at once.
    pub async fn author<T, I, F, Fut>(
        &self,
        channel_id: &ChannelId,
        base: AuthorBase<'_>,
        invite_key: Option<&PrivateKey>,
        intent: I,
        mut submit: F,
    ) -> Result<T, LogError>
    where
        I: Fn(&IntentContext<'_>) -> Result<Vec<pb::ChannelAction>, LogError>,
        F: FnMut(pb::ChannelChange) -> Fut,
        Fut: std::future::Future<Output = Result<T, NetworkError>>,
    {
        let lock = self.lock_for(channel_id);
        let _guard = lock.lock().await;
        let cid = channel_id.to_string();
        let (me, my_key) = self.backend.me().await?;
        let read_pub: Vec<u8> = match base {
            AuthorBase::Server { invite_pub } => invite_pub.to_vec(),
            _ => Vec::new(),
        };

        for attempt in 1..=MAX_SUBMIT_ATTEMPTS {
            let state: Option<MembershipState> = match base {
                AuthorBase::Genesis => None,
                AuthorBase::Verified => {
                    // Intent is judged against the newest state we can verify:
                    // a stale view would refuse (or sign) the wrong thing.
                    if attempt == 1 {
                        self.catch_up_locked(channel_id, &[]).await?;
                    }
                    let state = self.repo.load(channel_id)?.map(|v| v.state);
                    if state.is_none() {
                        return Err(LogError::Other(format!(
                            "no membership log for channel {channel_id}"
                        )));
                    }
                    state
                }
                AuthorBase::Server { invite_pub } => {
                    let p = self.backend.state(&cid, 0, invite_pub).await?;
                    Some(MembershipState::from_proto(&p).map_err(LogError::Verify)?)
                }
            };
            let actions = intent(&IntentContext {
                me: &me,
                state: state.as_ref(),
            })?;
            let revision = state.as_ref().map_or(1, |s| s.revision + 1);
            let timestamp = now_nanos();
            let new = rules::preview(
                state.as_ref(),
                &actions,
                &cid,
                revision,
                &me,
                my_key,
                timestamp,
            )
            .map_err(LogError::Intent)?;
            let (bytes, signature) = self
                .backend
                .sign(pb::channel_change::Actions {
                    channel_id: cid.clone(),
                    revision,
                    prev_state_hash: state
                        .as_ref()
                        .map(|s| s.hash().to_vec())
                        .unwrap_or_default(),
                    author_user_id: me.clone(),
                    author_identity_key: my_key.to_vec(),
                    timestamp,
                    actions,
                    new_state_hash: new.hash().to_vec(),
                })
                .await?;
            let invite_proof = match invite_key {
                Some(k) => k
                    .calculate_signature(&signed_message(INVITE_DOMAIN, &bytes), &mut rand::rng())
                    .map_err(|e| LogError::Other(format!("invite proof: {e}")))?
                    .to_vec(),
                None => Vec::new(),
            };
            let change = pb::ChannelChange {
                actions: bytes,
                signature,
                invite_proof,
            };

            match submit(change.clone()).await {
                Ok(out) => {
                    // Our own change, now accepted: commit it (the relayed
                    // copy will be a duplicate). A failure here is logged, not
                    // returned: the server has it, and a later catch-up
                    // brings it.
                    let committed = match self.step(channel_id, &change, &read_pub, false).await {
                        Ok(Step::Gap) => self.catch_up_locked(channel_id, &read_pub).await,
                        other => other.map(|_| ()),
                    };
                    if let Err(e) = committed {
                        tracing::warn!(%channel_id, error = %e, "Own accepted change not committed locally yet");
                    }
                    return Ok(out);
                }
                Err(e) if is_change_conflict(&e) && attempt < MAX_SUBMIT_ATTEMPTS => {
                    tracing::info!(%channel_id, attempt, "Membership change conflicted; catching up and retrying");
                    if matches!(base, AuthorBase::Verified) {
                        self.catch_up_locked(channel_id, &[]).await?;
                    }
                }
                Err(e) => return Err(LogError::Network(e)),
            }
        }
        Err(LogError::Other("membership change kept conflicting".into()))
    }

    /// `user_id`'s identity key, KT-verified: what an `AddMember` vouches for.
    pub async fn vouched_identity_key(&self, user_id: &str) -> Result<KeyBytes, LogError> {
        self.backend.vouched_identity_key(user_id, self.kt.as_ref()).await
    }

    /// The asymmetric rule for a membership fact only the server vouches for
    /// (roster sync): an add is ignored for keys and surfaced; a removal of a
    /// verified member excludes them from the distribution set.
    pub fn apply_unsigned(
        &self,
        channel_id: &ChannelId,
        event: UnsignedEvent,
    ) -> Result<UnsignedDisposition, StorageError> {
        let Some(v) = self.repo.load(channel_id)? else {
            // Nothing verified to protect yet; the first change bootstraps.
            return Ok(UnsignedDisposition::NoOp);
        };
        let disposition = vs::classify_unsigned(&v.state, &event);
        match (&disposition, event) {
            (UnsignedDisposition::IgnoreAndSurface, UnsignedEvent::Added { user_id }) => {
                tracing::warn!(%channel_id, %user_id, "Unsigned member add ignored: not in the verified state");
                self.notify(channel_id, MembershipNotice::UnverifiedMember { user_id });
            }
            (UnsignedDisposition::ExcludeFromDistribution, UnsignedEvent::Removed { user_id }) => {
                if !v.excluded.contains(&user_id) {
                    self.repo.exclude(channel_id, &user_id)?;
                    self.notify(channel_id, MembershipNotice::RemovedByServer { user_id });
                }
            }
            _ => {}
        }
        Ok(disposition)
    }

    /// Verify the signed change that removed US, which `GetDeparture` hands a
    /// former member because the log is closed to them (F79). Returns who
    /// removed us and why only when the change verifies in full — signature,
    /// the author's KT-verified key, the rules, both state hashes — against:
    ///  * our own verified state, when the removal is its next revision;
    ///  * otherwise `prev_state` (revision N-1), which the author's signed
    ///    `prev_state_hash` authenticates. Changes between our state and N-1
    ///    stay unseen: the removal is authenticated, not chained to our state.
    ///
    /// A removal not newer than our state (a replay of an earlier one), one
    /// that does not take us out of ACTIVE, or one that fails verification is
    /// `None`: the caller shows no reason. Nothing is committed and no banner
    /// is raised — we are out of the channel either way.
    pub async fn verify_own_removal(
        &self,
        channel_id: &ChannelId,
        change: &pb::ChannelChange,
        prev_state: Option<&pb::MembershipState>,
    ) -> Result<Option<OwnRemoval>, LogError> {
        let cid = channel_id.to_string();
        let (me, _) = self.backend.me().await?;
        let Ok(actions) = decode_actions(change) else {
            return Ok(None);
        };
        if actions.timestamp > now_nanos().saturating_add(MAX_FUTURE_SKEW.as_nanos() as u64) {
            return Ok(None);
        }
        let local = self.repo.load(channel_id)?.map(|v| v.state);
        let local_revision = local.as_ref().map_or(0, |s| s.revision);
        if actions.revision <= local_revision {
            tracing::warn!(%channel_id, revision = actions.revision, local_revision, "Departure change is not newer than our verified state");
            return Ok(None);
        }
        let base = if local.is_some() && actions.revision == local_revision + 1 {
            local
        } else {
            match prev_state.map(MembershipState::from_proto).transpose() {
                Ok(base) => base,
                Err(e) => {
                    tracing::warn!(%channel_id, error = %e, "Departure base state is invalid");
                    return Ok(None);
                }
            }
        };
        // A genesis removes no one: no base, no removal.
        let Some(base) = base else { return Ok(None) };
        let verified = match vs::verify_change(&cid, Some(&base), change, self.kt.as_ref()).await {
            Ok(v) => v,
            Err(VerifyError::KtUnavailable(m)) => return Err(LogError::KtUnavailable(m)),
            Err(e) => {
                tracing::warn!(%channel_id, error = %e, "The change said to have removed us failed verification");
                return Ok(None);
            }
        };
        if !verified.left_active(Some(&base)).contains(&me) {
            return Ok(None);
        }
        let reason = actions.actions.iter().find_map(|a| match &a.action {
            Some(Action::RemoveMember(r)) if r.user_id == me => {
                pb::remove_member::Reason::try_from(r.reason).ok()
            }
            _ => None,
        });
        Ok(reason.map(|reason| OwnRemoval {
            author: verified.0.author_user_id,
            reason,
            timestamp: actions.timestamp,
        }))
    }

    /// The user compared the state code and chose to resume sending on the
    /// state this device verified (§127). Sender keys still go only to that
    /// verified set; the next contradiction forks the channel again.
    pub fn acknowledge_fork(&self, channel_id: &ChannelId) -> Result<bool, StorageError> {
        let acknowledged = self.repo.acknowledge_fork(channel_id)?;
        if acknowledged {
            tracing::warn!(%channel_id, "Membership fork acknowledged by the user; sending resumes");
            self.notify(channel_id, MembershipNotice::Verified);
        }
        Ok(acknowledged)
    }

    /// Forget a channel's verified state (it was deleted).
    pub fn forget(&self, channel_id: &ChannelId) -> Result<(), StorageError> {
        self.repo.delete(channel_id)
    }
}

/// A channel the W3 cross-check found us behind on.
#[async_trait]
impl crate::services::message::ChannelStateSync for MembershipLog {
    async fn fetch_channel_changes(&self, channel_id: ChannelId, peer_revision: u64) {
        match self.catch_up(&channel_id).await {
            Ok(()) => {
                tracing::debug!(%channel_id, peer_revision, "Caught up after a peer showed a newer revision")
            }
            Err(e) => {
                tracing::warn!(%channel_id, peer_revision, error = %e, "Catch-up after a peer's newer revision failed")
            }
        }
    }
}

// ============================================================ invite keys

/// A join link: the channel and the private half of one `InviteCapability`.
/// Only `invite_pub` is ever in the state or on the server.
#[derive(Clone)]
pub struct InviteLink {
    pub channel_id: ChannelId,
    pub invite_key: PrivateKey,
}

impl std::fmt::Debug for InviteLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InviteLink")
            .field("channel_id", &self.channel_id)
            .finish_non_exhaustive()
    }
}

impl InviteLink {
    pub const URL_PREFIX: &'static str = "chatter://join/";

    /// A fresh capability key for a new link.
    pub fn generate(channel_id: ChannelId) -> Self {
        Self {
            channel_id,
            invite_key: KeyPair::generate(&mut rand::rngs::OsRng.unwrap_err()).private_key,
        }
    }

    pub fn invite_pub(&self) -> KeyBytes {
        invite_pub_of(&self.invite_key)
    }

    /// `chatter://join/<channel uuid>#<base64url(invite private key)>`. The
    /// key sits in the fragment so a link opened in a browser never sends it.
    pub fn to_url(&self) -> String {
        use base64::Engine;
        let key =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(self.invite_key.serialize());
        format!("{}{}#{}", Self::URL_PREFIX, self.channel_id, key)
    }

    pub fn parse(url: &str) -> Option<Self> {
        use base64::Engine;
        let rest = url.trim().strip_prefix(Self::URL_PREFIX)?;
        let (id, key) = rest.split_once('#')?;
        let channel_id = ChannelId::from(uuid::Uuid::parse_str(id).ok()?);
        let key = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(key)
            .ok()?;
        let key: [u8; 32] = key.try_into().ok()?;
        Some(Self {
            channel_id,
            invite_key: PrivateKey::deserialize(&key).ok()?,
        })
    }
}

pub fn invite_pub_of(key: &PrivateKey) -> KeyBytes {
    key.public_key()
        .expect("a valid private key has a public key")
        .serialize()
        .as_ref()
        .try_into()
        .expect("a DJB public key serializes to 33 bytes")
}

/// The invite key of a password channel: `Argon2id(password, salt =
/// channel_id bytes)` as the private key seed. The server stores only its
/// own hash of the password, so it cannot join with a password it does not
/// know; offline brute force against that hash remains the residual.
/// Changing these parameters breaks every password channel.
pub fn password_invite_key(channel_id: &ChannelId, password: &str) -> Result<PrivateKey, LogError> {
    let params =
        argon2::Params::new(65536, 3, 1, Some(32)).map_err(|e| LogError::Other(e.to_string()))?;
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut seed = zeroize::Zeroizing::new([0u8; 32]);
    argon
        .hash_password_into(password.as_bytes(), channel_id.0.as_bytes(), seed.as_mut())
        .map_err(|e| LogError::Other(e.to_string()))?;
    PrivateKey::deserialize(seed.as_ref()).map_err(|e| LogError::Other(e.to_string()))
}

// ============================================================ production I/O

/// [`LogBackend`] over the gRPC pool and the Signal actor.
pub struct SignalLogBackend {
    grpc_pool: Arc<GrpcClientPool>,
    signal_service: Arc<SignalService>,
    settings_repo: Arc<SettingsRepository>,
}

impl SignalLogBackend {
    pub fn new(
        grpc_pool: Arc<GrpcClientPool>,
        signal_service: Arc<SignalService>,
        settings_repo: Arc<SettingsRepository>,
    ) -> Self {
        Self {
            grpc_pool,
            signal_service,
            settings_repo,
        }
    }
}

#[async_trait]
impl LogBackend for SignalLogBackend {
    async fn me(&self) -> Result<(String, KeyBytes), LogError> {
        let me = self
            .settings_repo
            .current_user_id()
            .map_err(|e| LogError::Other(e.to_string()))?;
        Ok((
            me.to_string(),
            self.signal_service.get_identity_public_key().await?,
        ))
    }

    async fn sign(
        &self,
        actions: pb::channel_change::Actions,
    ) -> Result<(Vec<u8>, Vec<u8>), LogError> {
        Ok(self.signal_service.sign_channel_change(actions).await?)
    }

    async fn changes(
        &self,
        channel_id: &str,
        from_revision: u64,
        invite_pub: &[u8],
    ) -> Result<pb::GetChannelChangesResponse, NetworkError> {
        self.grpc_pool
            .channel_client()
            .get_channel_changes(channel_id, from_revision, PAGE, invite_pub)
            .await
    }

    async fn state(
        &self,
        channel_id: &str,
        revision: u64,
        invite_pub: &[u8],
    ) -> Result<pb::MembershipState, NetworkError> {
        self.grpc_pool
            .channel_client()
            .get_channel_state(channel_id, revision, invite_pub)
            .await
    }

    async fn mark_for_rotation(&self, channel_id: &ChannelId) {
        if let Err(e) = self.signal_service.mark_channel_for_rotation(channel_id) {
            tracing::error!(%channel_id, error = %e, "Failed to mark channel for rotation after a verified removal");
        }
    }

    async fn claimed_identity_key(&self, user_id: &str) -> Result<KeyBytes, LogError> {
        let proof = self
            .grpc_pool
            .auth_client()
            .get_key_transparency_proof(user_id)
            .await?
            .ok_or_else(|| {
                LogError::Other(format!(
                    "{user_id} has no identity key in the transparency log"
                ))
            })?;
        vs::key33(&proof.identity_key).map_err(LogError::Verify)
    }

    async fn vouched_identity_key(
        &self,
        user_id: &str,
        kt: &dyn IdentityKeyVerifier,
    ) -> Result<KeyBytes, LogError> {
        if self
            .settings_repo
            .current_user_id()
            .is_ok_and(|me| me.to_string() == user_id)
        {
            let key = self.claimed_identity_key(user_id).await?;
            return match kt.is_current_identity_key(user_id, &key).await {
                Ok(true) => Ok(key),
                Ok(false) => Err(LogError::Verify(VerifyError::KtMismatch)),
                Err(VerifyError::KtUnavailable(m)) => Err(LogError::KtUnavailable(m)),
                Err(e) => Err(LogError::Verify(e)),
            };
        }
        let mut auth = self.grpc_pool.auth_client();
        let key = self
            .signal_service
            .verified_identity_key_from_kt(&mut auth, user_id)
            .await?;
        vs::key33(&key).map_err(LogError::Verify)
    }
}

/// Check 3 of verification: the author key is the user's current key in the
/// Key Transparency log (`SignalService::verify_identity_key_transparency`,
/// the same gate prekey bundles pass). Our own key is known locally.
pub struct SignalKtVerifier {
    grpc_pool: Arc<GrpcClientPool>,
    signal_service: Arc<SignalService>,
    settings_repo: Arc<SettingsRepository>,
}

impl SignalKtVerifier {
    pub fn cached(
        grpc_pool: Arc<GrpcClientPool>,
        signal_service: Arc<SignalService>,
        settings_repo: Arc<SettingsRepository>,
    ) -> CachedKtVerifier<Self> {
        CachedKtVerifier::new(
            Self {
                grpc_pool,
                signal_service,
                settings_repo,
            },
            KT_CACHE_TTL,
        )
    }
}

#[async_trait]
impl IdentityKeyVerifier for SignalKtVerifier {
    async fn is_current_identity_key(
        &self,
        user_id: &str,
        identity_key: &KeyBytes,
    ) -> Result<bool, VerifyError> {
        if self
            .settings_repo
            .current_user_id()
            .is_ok_and(|me| me.to_string() == user_id)
        {
            let own = self
                .signal_service
                .get_identity_public_key()
                .await
                .map_err(|e| VerifyError::KtUnavailable(e.to_string()))?;
            return Ok(own == *identity_key);
        }
        let mut auth = self.grpc_pool.auth_client();
        match self
            .signal_service
            .verify_identity_key_transparency(&mut auth, user_id, identity_key)
            .await
        {
            Ok(()) => Ok(true),
            Err(ServiceError::Network(e)) => Err(VerifyError::KtUnavailable(e.to_string())),
            Err(e) => {
                tracing::warn!(user_id, error = %e, "Channel change author key is not the KT key");
                Ok(false)
            }
        }
    }
}

#[cfg(test)]
#[path = "membership_log_test_support.rs"]
pub(crate) mod test_support;

#[cfg(test)]
#[path = "membership_log_tests.rs"]
mod tests;
