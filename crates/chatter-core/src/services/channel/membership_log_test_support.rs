//! In-memory server log, signer and KT for exercising [`MembershipLog`]
//! without a server. The fake server accepts a change only if the client's
//! own verifier accepts it against the head (and refuses a stale one with the
//! `ABORTED` the real server sends), so a round trip through it is a round
//! trip through W2's verifier.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use libsignal_protocol::IdentityKeyPair;

use crate::services::channel::membership_log::{LogBackend, LogError, MembershipLog};
use crate::services::channel::verified_state::{self as vs, IdentityKeyVerifier, KeyBytes, MembershipState, VerifyError};
use crate::domain::ids::ChannelId;
use crate::error::NetworkError;
use crate::proto::chat_v1 as pb;
use crate::storage::repositories::verified_membership_repo::VerifiedMembershipRepository;
use crate::storage::Database;

pub(crate) struct Who {
    pub id: String,
    pub kp: IdentityKeyPair,
}

impl Who {
    pub fn key(&self) -> KeyBytes {
        self.kp.public_key().serialize().as_ref().try_into().unwrap()
    }
}

pub(crate) fn who(n: u32) -> Who {
    Who { id: format!("{n:08x}-0000-4000-8000-{n:012x}"), kp: IdentityKeyPair::generate(&mut rand::rng()) }
}

#[derive(Default)]
pub(crate) struct FakeServer {
    pub log: parking_lot::Mutex<Vec<pb::ChannelChange>>,
    pub states: parking_lot::Mutex<Vec<MembershipState>>,
    /// Calls to `append` (accepted or not).
    pub submits: std::sync::atomic::AtomicUsize,
}

impl FakeServer {
    pub fn head(&self) -> Option<MembershipState> {
        self.states.lock().last().cloned()
    }

    /// Append if it chains and verifies; `ABORTED` if it is not head + 1.
    pub fn append(&self, channel_id: &ChannelId, change: pb::ChannelChange) -> Result<u64, NetworkError> {
        self.submits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let head = self.head();
        match vs::check_change(&channel_id.to_string(), head.as_ref(), &change) {
            Ok(p) => {
                let rev = p.new_state.revision;
                self.states.lock().push(p.new_state);
                self.log.lock().push(change);
                Ok(rev)
            }
            Err(VerifyError::Gap { .. } | VerifyError::Stale { .. } | VerifyError::PrevHashMismatch) => {
                Err(NetworkError::ServerError("aborted: change conflict".into()))
            }
            Err(e) => Err(NetworkError::InvalidRequest(e.to_string())),
        }
    }
}

pub(crate) struct FakeBackend {
    pub me: (String, KeyBytes),
    pub kp: IdentityKeyPair,
    pub server: Arc<FakeServer>,
    pub directory: HashMap<String, KeyBytes>,
    pub rotations: parking_lot::Mutex<Vec<ChannelId>>,
}

#[async_trait]
impl LogBackend for FakeBackend {
    async fn me(&self) -> Result<(String, KeyBytes), LogError> {
        Ok(self.me.clone())
    }

    async fn sign(&self, actions: pb::channel_change::Actions) -> Result<(Vec<u8>, Vec<u8>), LogError> {
        crate::crypto::signal::identity::sign_channel_change(&self.kp, &actions).map_err(|e| LogError::Other(e.to_string()))
    }

    async fn changes(&self, _: &str, from: u64, _: &[u8]) -> Result<pb::GetChannelChangesResponse, NetworkError> {
        let log = self.server.log.lock();
        let from = from.max(1) as usize;
        Ok(pb::GetChannelChangesResponse {
            changes: log.iter().skip(from - 1).take(100).cloned().collect(),
            current_revision: log.len() as u64,
        })
    }

    async fn state(&self, _: &str, revision: u64, _: &[u8]) -> Result<pb::MembershipState, NetworkError> {
        let states = self.server.states.lock();
        let s = if revision == 0 { states.last() } else { states.get(revision as usize - 1) };
        s.map(MembershipState::to_proto).ok_or_else(|| NetworkError::NotFound("state".into()))
    }

    async fn mark_for_rotation(&self, channel_id: &ChannelId) {
        self.rotations.lock().push(*channel_id);
    }

    async fn claimed_identity_key(&self, user_id: &str) -> Result<KeyBytes, LogError> {
        self.directory.get(user_id).copied().ok_or_else(|| LogError::Other("unknown user".into()))
    }
}

/// KT that knows every test user's one true key.
pub(crate) struct FakeKt(pub HashMap<String, KeyBytes>);

#[async_trait]
impl IdentityKeyVerifier for FakeKt {
    async fn is_current_identity_key(&self, user_id: &str, key: &KeyBytes) -> Result<bool, VerifyError> {
        Ok(self.0.get(user_id) == Some(key))
    }
}

pub(crate) fn directory(people: &[&Who]) -> HashMap<String, KeyBytes> {
    people.iter().map(|w| (w.id.clone(), w.key())).collect()
}

pub(crate) fn test_db() -> Arc<Database> {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::open(&dir.path().join("membership_log_test.db"), "testpass").unwrap());
    db.run_migrations().unwrap();
    Box::leak(Box::new(dir));
    db
}

/// `me`'s device: its own database, signing with `me`'s key, talking to `server`.
pub(crate) fn device(me: &Who, server: &Arc<FakeServer>, people: &[&Who]) -> (Arc<MembershipLog>, Arc<FakeBackend>, Arc<Database>) {
    device_on(test_db(), me, server, people)
}

/// [`device`] on an existing database.
pub(crate) fn device_on(
    db: Arc<Database>,
    me: &Who,
    server: &Arc<FakeServer>,
    people: &[&Who],
) -> (Arc<MembershipLog>, Arc<FakeBackend>, Arc<Database>) {
    let backend = Arc::new(FakeBackend {
        me: (me.id.clone(), me.key()),
        kp: me.kp,
        server: server.clone(),
        directory: directory(people),
        rotations: Default::default(),
    });
    let log = Arc::new(MembershipLog::new(
        backend.clone(),
        Arc::new(FakeKt(directory(people))),
        VerifiedMembershipRepository::new(db.clone()),
    ));
    (log, backend, db)
}
