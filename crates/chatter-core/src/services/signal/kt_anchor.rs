//! Chatter transport and durable state for libsignal's KT verifier.

use std::collections::HashMap;
use std::future::Future;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use chatter_proto::chat_v1::{
    KtConsistency, KtMonitorKey, KtMonitorRequest, KtMonitorResponse, KtSearchRequest,
    KtSearchResponse,
};
use libsignal_keytrans::{
    CondensedTreeSearchResponse, DeploymentMode, FullSearchResponse, FullTreeHead, KeyTransparency,
    LastTreeHead, MonitorContext, MonitorRequest, MonitorResponse, MonitoringData, PublicConfig,
    SearchContext, SlimSearchRequest, StoredMonitoringData, StoredTreeHead, VerifiedSearchResult,
    VerifyingKey, VrfPublicKey,
};
use prost::Message;
use rusqlite::TransactionBehavior;
use serde::{Deserialize, Serialize};

use crate::error::{CryptoError, NetworkError, ServiceError, StorageError};
use crate::network::grpc::AnonymousKtClient;
use crate::storage::repositories::SettingsRepository;

const HEAD: &str = "kt.libsignal.head";
const DISTINGUISHED_HEAD: &str = "kt.libsignal.distinguished";
/// Keep the last head, checkpoint, and per-label observations on the route
/// that fetched them. Each value can appear in a subsequent proof request.
#[derive(Clone, Copy)]
pub(crate) enum PinScope {
    Account,
    Anonymous,
}

impl PinScope {
    fn head(self) -> &'static str {
        match self {
            Self::Account => HEAD,
            Self::Anonymous => "kt.libsignal.anonymous.head",
        }
    }

    fn distinguished(self) -> &'static str {
        match self {
            Self::Account => DISTINGUISHED_HEAD,
            Self::Anonymous => "kt.libsignal.anonymous.distinguished",
        }
    }

    fn verified_at(self) -> &'static str {
        match self {
            Self::Account => "kt.libsignal.verified_at",
            Self::Anonymous => "kt.libsignal.anonymous.verified_at",
        }
    }

    fn label_key(self, field: &str, label: &[u8]) -> String {
        let prefix = match self {
            Self::Account => "kt.libsignal",
            Self::Anonymous => "kt.libsignal.anonymous",
        };
        format!("{prefix}.{field}.{}", hex::encode(label))
    }
}

const DISTINGUISHED: &[u8] = b"distinguished";
const CHECKPOINT_REFRESH: Duration = Duration::from_secs(6 * 60 * 60);
pub(crate) const PIN_RPC_DEADLINE: Duration = Duration::from_secs(10);
pub(crate) const TOR_RPC_DEADLINE: Duration = Duration::from_secs(95);

static VERIFIER: OnceLock<Option<KeyTransparency>> = OnceLock::new();

#[derive(Debug, thiserror::Error)]
pub(crate) enum KtError {
    #[error(transparent)]
    Network(#[from] NetworkError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Proof(#[from] libsignal_keytrans::Error),
    #[error(transparent)]
    Decode(#[from] prost::DecodeError),
    #[error("{0}")]
    Invalid(&'static str),
    #[error("transparency state advanced during verification")]
    PinChanged,
}

impl From<KtError> for ServiceError {
    fn from(error: KtError) -> Self {
        match error {
            KtError::Network(error) => Self::Network(error),
            KtError::Storage(error) => Self::Storage(error),
            KtError::Proof(error) if history_unverified(&error) => {
                CryptoError::KeyTransparencyHistoryUnverified.into()
            }
            error => Self::Validation(format!("key transparency: {error}")),
        }
    }
}

// Libsignal's pinned verifier reports consistency failures as text within
// its two error variants. Preserve the background worker's history warning
// for those failures. Every other verifier error still fails verification.
fn history_unverified(error: &libsignal_keytrans::Error) -> bool {
    match error {
        libsignal_keytrans::Error::BadData(reason) => matches!(
            reason.as_str(),
            "root is different but tree size is same"
                | "tree size is the same but timestamps differ"
                | "current tree size is less than previous tree size"
                | "current timestamp is less than previous timestamp"
        ),
        libsignal_keytrans::Error::VerificationFailed(reason) => matches!(
            reason.as_str(),
            "Proof mismatch: first root does not match proof"
                | "Proof mismatch: second root does not match proof"
        ),
        _ => false,
    }
}

fn verifier() -> Result<&'static KeyTransparency, KtError> {
    use crate::config::{KT_TREE_HEAD_PUBLIC_KEY_BYTES, KT_VRF_PUBLIC_KEY_BYTES};
    VERIFIER
        .get_or_init(|| {
            let signature_key = VerifyingKey::from_bytes(&KT_TREE_HEAD_PUBLIC_KEY_BYTES).ok()?;
            let vrf_key = VrfPublicKey::try_from(KT_VRF_PUBLIC_KEY_BYTES).ok()?;
            Some(KeyTransparency {
                config: PublicConfig {
                    mode: DeploymentMode::ContactMonitoring,
                    signature_key,
                    vrf_key,
                },
            })
        })
        .as_ref()
        .ok_or(KtError::Invalid("invalid pinned key transparency keys"))
}

pub(crate) trait KtTransport {
    fn search(
        &mut self,
        request: KtSearchRequest,
    ) -> impl Future<Output = Result<Option<KtSearchResponse>, NetworkError>> + Send;

    fn monitor(
        &mut self,
        request: KtMonitorRequest,
    ) -> impl Future<Output = Result<KtMonitorResponse, NetworkError>> + Send;
}

impl KtTransport for AnonymousKtClient {
    async fn search(
        &mut self,
        request: KtSearchRequest,
    ) -> Result<Option<KtSearchResponse>, NetworkError> {
        self.search(request).await
    }

    async fn monitor(
        &mut self,
        request: KtMonitorRequest,
    ) -> Result<KtMonitorResponse, NetworkError> {
        self.monitor(request).await
    }
}

struct PinnedHead {
    encoded: String,
    head: LastTreeHead,
    stored_at_ms: u64,
}

struct Observations {
    key: String,
    encoded: Option<String>,
    data: Option<MonitoringData>,
}

/// In-flight verification against a snapshot of the durable pin. There is no
/// network I/O under a lock: routes may verify concurrently, then compare their
/// baselines in the transaction that publishes a result. A stale request must
/// retry with a consistency proof from the current pin.
pub(crate) struct Pin<'a> {
    settings: &'a SettingsRepository,
    scope: PinScope,
    verifier: &'a KeyTransparency,
    now: SystemTime,
    last: Option<PinnedHead>,
    distinguished: PinnedHead,
}

impl<'a> Pin<'a> {
    pub(crate) async fn load(
        client: &mut impl KtTransport,
        settings: &'a SettingsRepository,
    ) -> Result<Self, KtError> {
        Self::load_at(client, settings, PinScope::Account, verifier()?, SystemTime::now()).await
    }

    async fn load_at(
        client: &mut impl KtTransport,
        settings: &'a SettingsRepository,
        scope: PinScope,
        verifier: &'a KeyTransparency,
        now: SystemTime,
    ) -> Result<Self, KtError> {
        let checkpoint = load_head(settings, scope.distinguished())?;
        let refresh = checkpoint.as_ref().is_none_or(|head| {
            let now_ms = now
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            now_ms.saturating_sub(head.stored_at_ms.into()) >= CHECKPOINT_REFRESH.as_millis()
        });
        let distinguished = if refresh {
            let previous = checkpoint.as_ref().map(|pinned| &pinned.head);
            let response = client
                .search(request(DISTINGUISHED, None, previous))
                .await?
                .ok_or(KtError::Invalid("distinguished checkpoint is missing"))?;
            let verified = verify_search(
                verifier,
                DISTINGUISHED,
                response,
                SearchContext {
                    last_distinguished_tree_head: previous,
                    ..Default::default()
                },
                false,
                now,
            )?;
            let update = verified.state_update;
            let head = LastTreeHead(update.tree_head, update.tree_root);
            let encoded = encode_head(head, now);
            // A checkpoint has its own verified consistency proof. Keep it even
            // when the next RPC returns an unsigned NotFound.
            let mut conn = settings.database().connection()?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(StorageError::from)?;
            check_setting(&tx, scope.distinguished(), encoded_head(checkpoint.as_ref()))?;
            SettingsRepository::set_with_connection(&tx, scope.distinguished(), &encoded)?;
            tx.commit().map_err(StorageError::from)?;
            decode_head(encoded)?
        } else {
            checkpoint.ok_or(KtError::Invalid("distinguished checkpoint is missing"))?
        };
        Ok(Self {
            settings,
            scope,
            verifier,
            now,
            last: load_head(settings, scope.head())?,
            distinguished,
        })
    }

    pub(crate) fn consistency(&self) -> KtConsistency {
        consistency(self.last_head(), Some(&self.distinguished.head))
    }

    fn last_head(&self) -> Option<&LastTreeHead> {
        self.last.as_ref().map(|pinned| &pinned.head)
    }

    fn observations(&self, label: &[u8]) -> Result<Observations, KtError> {
        let key = self.scope.label_key("label", label);
        let conn = self.settings.database().read_connection()?;
        let tx = conn.unchecked_transaction().map_err(StorageError::from)?;
        check_setting(&tx, self.scope.head(), encoded_head(self.last.as_ref()))?;
        check_setting(&tx, self.scope.distinguished(), Some(&self.distinguished.encoded))?;
        let encoded: Option<String> = SettingsRepository::get_with_connection(&tx, &key)?;
        let data = encoded
            .as_deref()
            .map(|encoded| self.decode_observations(encoded, label))
            .transpose()?;
        Ok(Observations { key, encoded, data })
    }

    fn decode_observations(&self, encoded: &str, label: &[u8]) -> Result<MonitoringData, KtError> {
        let data = StoredMonitoringData::decode(decode_setting(encoded)?.as_slice())?;
        let positions_valid = self.last_head().is_some_and(|head| {
            let size = head.0.tree_size;
            data.pos < size && data.ptrs.keys().all(|&pos| data.pos <= pos && pos < size)
        });
        if data.index.len() != 32
            || data.search_key != label
            || data.ptrs.is_empty()
            || !positions_valid
        {
            return Err(KtError::Invalid("invalid stored transparency observations"));
        }
        Ok(data.into())
    }

    fn verify_response(
        &self,
        label: &[u8],
        response: KtSearchResponse,
        observations: &Observations,
        owned: bool,
    ) -> Result<VerifiedSearchResult, KtError> {
        let mut result = verify_search(
            self.verifier,
            label,
            response,
            SearchContext {
                last_tree_head: self.last_head(),
                last_distinguished_tree_head: Some(&self.distinguished.head),
                data: observations.data.clone(),
            },
            owned,
            self.now,
        )?;
        if let Some(data) = result.state_update.monitoring_data.as_mut() {
            data.search_key = label.to_vec();
        }
        Ok(result)
    }

    /// Verify and persist a manifest proof only if it commits to the digest.
    pub(crate) fn verify(
        &self,
        label: &[u8],
        response: KtSearchResponse,
        expected: &[u8],
    ) -> Result<(), KtError> {
        let observations = self.observations(label)?;
        let verified = self.verify_response(label, response, &observations, false)?;
        if expected != verified.value {
            return Err(KtError::Invalid(
                "transparency proof commits a different value",
            ));
        }
        self.save_search(observations, verified, |_| Ok(()))
    }

    async fn search_label(
        &self,
        client: &mut impl KtTransport,
        label: &[u8],
    ) -> Result<Option<KtSearchResponse>, KtError> {
        Ok(client
            .search(request(
                label,
                self.last_head(),
                Some(&self.distinguished.head),
            ))
            .await?)
    }

    async fn monitor_label(
        &self,
        client: &mut impl KtTransport,
        label: &[u8],
        data: &MonitoringData,
    ) -> Result<(LastTreeHead, MonitoringData), KtError> {
        let request = KtMonitorRequest {
            keys: vec![KtMonitorKey {
                search_key: label.to_vec(),
                entry_position: data.latest_log_position(),
                commitment_index: data.index.to_vec(),
            }],
            consistency: Some(self.consistency()),
        };
        let response = match client.monitor(request.clone()).await {
            Err(NetworkError::NotFound(_)) => {
                return Err(KtError::Invalid("previously verified label is missing"));
            }
            result => result?,
        };
        let request = MonitorRequest::decode(request.encode_to_vec().as_slice())?;
        let response = MonitorResponse::decode(response.encode_to_vec().as_slice())?;
        let mut update = self.verifier.verify_monitor(
            &request,
            &response,
            MonitorContext {
                last_tree_head: self.last_head(),
                last_distinguished_tree_head: &self.distinguished.head,
                data: HashMap::from([(label.to_vec(), data.clone())]),
            },
            self.now,
        )?;
        let data = update
            .monitoring_data
            .remove(label)
            .unwrap_or_else(|| data.clone());
        Ok((LastTreeHead(update.tree_head, update.tree_root), data))
    }

    fn save_search(
        &self,
        observations: Observations,
        verified: VerifiedSearchResult,
        update_settings: impl FnOnce(&rusqlite::Connection) -> Result<(), KtError>,
    ) -> Result<(), KtError> {
        let update = verified.state_update;
        self.save(
            LastTreeHead(update.tree_head, update.tree_root),
            observations,
            update.monitoring_data,
            update_settings,
        )
    }

    fn save(
        &self,
        head: LastTreeHead,
        observations: Observations,
        data: Option<MonitoringData>,
        update_settings: impl FnOnce(&rusqlite::Connection) -> Result<(), KtError>,
    ) -> Result<(), KtError> {
        let mut conn = self.settings.database().connection()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StorageError::from)?;
        let current: Option<String> = SettingsRepository::get_with_connection(&tx, self.scope.head())?;
        let head_advanced = current.as_deref() != encoded_head(self.last.as_ref());
        // A concurrent request may already have pinned this exact verified
        // head. Its proof and ours authenticated the same root. Merge only
        // this label's unchanged observations, keeping the recorded head.
        if head_advanced
            && current
                .map(decode_head)
                .transpose()?
                .is_none_or(|current| current.head != head)
        {
            return Err(KtError::PinChanged);
        }
        check_setting(&tx, self.scope.distinguished(), Some(&self.distinguished.encoded))?;
        check_setting(&tx, &observations.key, observations.encoded.as_deref())?;

        if !head_advanced
            && self
                .last_head()
                .is_none_or(|last| last.0.tree_size != head.0.tree_size)
        {
            SettingsRepository::set_with_connection(&tx, self.scope.head(), &encode_head(head, self.now))?;
        }
        if let Some(data) = data.filter(|data| Some(data) != observations.data.as_ref()) {
            let stored = StoredMonitoringData {
                index: data.index.to_vec(),
                pos: data.pos,
                ptrs: data.ptrs,
                owned: data.owned,
                search_key: data.search_key,
                max_observed_version: data.max_observed_version,
            };
            SettingsRepository::set_with_connection(
                &tx,
                &observations.key,
                &hex::encode(stored.encode_to_vec()),
            )?;
        }
        update_settings(&tx)?;
        let verified_at = self
            .now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        SettingsRepository::set_with_connection(&tx, self.scope.verified_at(), &verified_at)?;
        tx.commit().map_err(StorageError::from)?;
        Ok(())
    }
}

/// When a proof on `scope`'s pin last verified, in Unix seconds.
pub(crate) fn verified_at(settings: &SettingsRepository, scope: PinScope) -> Result<Option<i64>, KtError> {
    Ok(settings.get::<i64>(scope.verified_at())?)
}

fn check_setting(
    conn: &rusqlite::Connection,
    key: &str,
    expected: Option<&str>,
) -> Result<(), KtError> {
    let current = SettingsRepository::get_with_connection::<String>(conn, key)?;
    if current.as_deref() != expected {
        return Err(KtError::PinChanged);
    }
    Ok(())
}

/// Verify an identity on the caller's route. As in libsignal's contact flow,
/// monitor an observed version first and search when a new value is needed.
pub(crate) async fn search(
    client: &mut AnonymousKtClient,
    settings: &SettingsRepository,
    scope: PinScope,
    label: &str,
    deadline: Duration,
) -> Result<Option<Vec<u8>>, KtError> {
    let verifier = verifier()?;
    with_deadline(deadline, async {
        loop {
            match search_contact(
                client,
                settings,
                scope,
                label.as_bytes(),
                verifier,
                SystemTime::now(),
            )
            .await
            {
                Err(KtError::PinChanged) => tokio::task::yield_now().await,
                result => return result,
            }
        }
    })
    .await
}

async fn search_contact(
    client: &mut impl KtTransport,
    settings: &SettingsRepository,
    scope: PinScope,
    label: &[u8],
    verifier: &KeyTransparency,
    now: SystemTime,
) -> Result<Option<Vec<u8>>, KtError> {
    let mut pin = Pin::load_at(client, settings, scope, verifier, now).await?;
    let mut observations = pin.observations(label)?;
    let value_key = scope.label_key("value", label);
    if let Some(data) = observations.data.as_ref() {
        let previous = data.greatest_version();
        let (head, data) = pin.monitor_label(client, label, data).await?;
        let version = data.greatest_version();
        pin.save(head, observations, Some(data), |_| Ok(()))?;
        if let Some((saved_version, value)) = settings.get::<(u32, Vec<u8>)>(&value_key)? {
            if saved_version == previous && version == previous {
                return Ok(Some(value));
            }
        }
        pin = Pin::load_at(client, settings, scope, verifier, now).await?;
        observations = pin.observations(label)?;
    }
    let Some(response) = pin.search_label(client, label).await? else {
        if settings.get_string(&observations.key)?.is_some() {
            return Err(KtError::Invalid("previously verified label is missing"));
        }
        return Ok(None);
    };
    let verified = pin.verify_response(label, response, &observations, false)?;
    let value = verified.value.clone();
    let version = verified
        .state_update
        .monitoring_data
        .as_ref()
        .ok_or(KtError::Invalid(
            "identity search did not produce monitoring data",
        ))?
        .greatest_version();
    pin.save_search(observations, verified, |conn| {
        SettingsRepository::set_with_connection(conn, &value_key, &(version, &value))?;
        Ok(())
    })?;
    Ok(Some(value))
}

/// Once the local key is verified, every increase in its log version is
/// evidence of an unexpected write, even when a later write restores the key.
pub(crate) async fn monitor_own(
    client: &mut AnonymousKtClient,
    settings: &SettingsRepository,
    label: &str,
    expected: &[u8],
    deadline: Duration,
) -> Result<bool, KtError> {
    let verifier = verifier()?;
    with_deadline(deadline, async {
        loop {
            match check_own(
                client,
                settings,
                label.as_bytes(),
                expected,
                verifier,
                SystemTime::now(),
            )
            .await
            {
                Err(KtError::PinChanged) => tokio::task::yield_now().await,
                result => return result,
            }
        }
    })
    .await
}

async fn with_deadline<T>(
    deadline: Duration,
    request: impl Future<Output = Result<T, KtError>>,
) -> Result<T, KtError> {
    tokio::time::timeout(deadline, request)
        .await
        .unwrap_or(Err(NetworkError::Timeout.into()))
}

#[derive(Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
struct OwnerState {
    baseline: Option<OwnerBaseline>,
    authorized: Option<OwnerAuthorization>,
    observed_version: Option<u32>,
    mismatch: bool,
}

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
struct OwnerBaseline {
    key: String,
    version: u32,
}

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
struct OwnerAuthorization {
    key: String,
    next_version: Option<u32>,
}

impl OwnerState {
    fn load(conn: &rusqlite::Connection, label: &[u8]) -> Result<Self, StorageError> {
        Ok(SettingsRepository::get_with_connection(
            conn,
            &format!("kt.libsignal.owner.{}", hex::encode(label)),
        )?
        .unwrap_or_default())
    }

    fn store(&self, conn: &rusqlite::Connection, label: &[u8]) -> Result<(), StorageError> {
        SettingsRepository::set_with_connection(
            conn,
            &format!("kt.libsignal.owner.{}", hex::encode(label)),
            self,
        )
    }
}

/// Record the exact public key that completed a local republish or device
/// link. The next verified search consumes this permission. A repeat publish
/// of a key we already monitor keeps its history and any warning.
pub(crate) fn authorize_own_identity(
    settings: &SettingsRepository,
    label: &str,
    key: &[u8],
) -> Result<(), StorageError> {
    use crate::storage::repositories::settings_repo::keys;

    let mut conn = settings.database().connection()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = SettingsRepository::get_with_connection::<String>(&tx, keys::CURRENT_USER_ID)?;
    if current.as_deref() != Some(label) {
        return Err(StorageError::InvalidData(
            "transparency owner changed during publication".into(),
        ));
    }
    let mut state = OwnerState::load(&tx, label.as_bytes())?;
    let key = hex::encode(key);
    if state
        .baseline
        .as_ref()
        .is_some_and(|baseline| baseline.key == key)
    {
        state.authorized = None;
    } else if state
        .authorized
        .as_ref()
        .is_none_or(|pending| pending.key != key)
    {
        // An observed version accounts for earlier unexpected writes too.
        // On a fresh installation the authenticated local key is the only
        // baseline available, so its exact value anchors the initial version.
        let next_version = state
            .observed_version
            .map(|version| {
                version.checked_add(1).ok_or_else(|| {
                    StorageError::InvalidData("own identity transparency version exhausted".into())
                })
            })
            .transpose()?;
        state.authorized = Some(OwnerAuthorization { key, next_version });
    }
    state.store(&tx, label.as_bytes())?;
    tx.commit()?;
    Ok(())
}

async fn check_own(
    client: &mut impl KtTransport,
    settings: &SettingsRepository,
    label: &[u8],
    expected: &[u8],
    verifier: &KeyTransparency,
    now: SystemTime,
) -> Result<bool, KtError> {
    let previous = {
        let conn = settings.database().read_connection()?;
        OwnerState::load(&conn, label)?
    };
    let mut state = previous.clone();
    let expected_hex = hex::encode(expected);
    if state
        .authorized
        .as_ref()
        .is_some_and(|pending| pending.key != expected_hex)
    {
        return Err(KtError::Invalid(
            "local owner key changed during transparency verification",
        ));
    }
    let pin = Pin::load_at(client, settings, PinScope::Account, verifier, now).await?;
    let observations = pin.observations(label)?;
    let save_owner = |conn: &rusqlite::Connection, state: &OwnerState| -> Result<(), KtError> {
        if OwnerState::load(conn, label)? != previous {
            return Err(KtError::PinChanged);
        }
        if *state != previous {
            state.store(conn, label)?;
        }
        Ok(())
    };
    let baseline = state
        .baseline
        .as_ref()
        .filter(|baseline| baseline.key == expected_hex);
    if let (Some(baseline), Some(data)) = (
        baseline,
        observations.data.as_ref().filter(|data| data.owned),
    ) {
        let (head, data) = pin.monitor_label(client, label, data).await?;
        let version = data.greatest_version();
        state.mismatch |= version != baseline.version;
        state.observed_version = Some(version);
        pin.save(head, observations, Some(data), |conn| {
            save_owner(conn, &state)
        })?;
        return Ok(state.mismatch);
    }

    let response = pin
        .search_label(client, label)
        .await?
        .ok_or(KtError::Invalid(
            "own identity key is missing from transparency",
        ))?;
    let verified = pin.verify_response(label, response, &observations, true)?;
    let version = verified
        .state_update
        .monitoring_data
        .as_ref()
        .ok_or(KtError::Invalid(
            "own identity search did not produce monitoring data",
        ))?
        .greatest_version();

    state.observed_version = Some(version);
    if verified.value != expected {
        state.mismatch = true;
        // The first verified result spends the local authorization. A later
        // server write restoring the expected key cannot clear this alarm.
        state.authorized = None;
    } else if let Some(pending) = state.authorized.take() {
        state.baseline = Some(OwnerBaseline {
            key: expected_hex,
            version,
        });
        state.mismatch = pending.next_version.is_some_and(|next| next != version);
    } else if state.baseline.is_none() {
        state.baseline = Some(OwnerBaseline {
            key: expected_hex,
            version,
        });
        state.mismatch |= version != 0;
    } else {
        state.mismatch |= baseline.is_none_or(|baseline| baseline.version != version);
    }
    pin.save_search(observations, verified, |conn| save_owner(conn, &state))?;
    Ok(state.mismatch)
}

pub(crate) fn own_mismatch(settings: &SettingsRepository, label: &[u8]) -> Result<bool, KtError> {
    let conn = settings.database().read_connection()?;
    Ok(OwnerState::load(&conn, label)?.mismatch)
}

/// Verify the inclusion in an already accepted, persisted manifest. Its
/// signed timestamp fixes the time of this historical proof. The manifest
/// still has to pass its own schedule and current validity checks.
pub(crate) fn verify_cached(
    label: &[u8],
    mut response: KtSearchResponse,
    expected: &[u8],
) -> Result<(), KtError> {
    let full = response
        .tree_head
        .as_mut()
        .ok_or(KtError::Invalid("missing full tree head"))?;
    let head = full
        .tree_head
        .as_ref()
        .ok_or(KtError::Invalid("missing tree head"))?;
    let time = signed_time(head.timestamp)?;
    full.last.clear();
    full.distinguished.clear();
    let verified = verify_search(
        verifier()?,
        label,
        response,
        SearchContext::default(),
        false,
        time,
    )?;
    if verified.value != expected {
        return Err(KtError::Invalid(
            "transparency proof commits a different value",
        ));
    }
    Ok(())
}

fn verify_search(
    verifier: &KeyTransparency,
    label: &[u8],
    response: KtSearchResponse,
    context: SearchContext<'_>,
    owned: bool,
    now: SystemTime,
) -> Result<VerifiedSearchResult, KtError> {
    let full_head = response
        .tree_head
        .ok_or(KtError::Invalid("missing full tree head"))?;
    let condensed = response
        .condensed
        .ok_or(KtError::Invalid("missing search proof"))?;
    // The Chatter transport messages have the upstream protobuf field layout.
    // Decode to libsignal's own types before making any verification decision.
    let full_head = FullTreeHead::decode(full_head.encode_to_vec().as_slice())?;
    let condensed = CondensedTreeSearchResponse::decode(condensed.encode_to_vec().as_slice())?;
    Ok(verifier.verify_search(
        SlimSearchRequest::new(label.to_vec()),
        FullSearchResponse::new(condensed, &full_head),
        context,
        owned,
        now,
    )?)
}

fn request(
    label: &[u8],
    last: Option<&LastTreeHead>,
    distinguished: Option<&LastTreeHead>,
) -> KtSearchRequest {
    KtSearchRequest {
        search_key: label.to_vec(),
        version: None,
        consistency: Some(consistency(last, distinguished)),
    }
}

fn consistency(last: Option<&LastTreeHead>, distinguished: Option<&LastTreeHead>) -> KtConsistency {
    KtConsistency {
        last: last.map(|head| head.0.tree_size),
        distinguished: distinguished.map(|head| head.0.tree_size),
    }
}

fn signed_time(timestamp: i64) -> Result<SystemTime, KtError> {
    let ms = u64::try_from(timestamp).map_err(|_| KtError::Invalid("invalid tree timestamp"))?;
    SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_millis(ms))
        .ok_or(KtError::Invalid("invalid tree timestamp"))
}

fn load_head(settings: &SettingsRepository, key: &str) -> Result<Option<PinnedHead>, KtError> {
    settings.get_string(key)?.map(decode_head).transpose()
}

fn decode_head(encoded: String) -> Result<PinnedHead, KtError> {
    let stored = StoredTreeHead::decode(decode_setting(&encoded)?.as_slice())?;
    let stored_at_ms = stored.stored_at_ms;
    let head = stored
        .into_last_tree_head()
        .ok_or(KtError::Invalid("invalid stored tree head"))?;
    if head.0.tree_size == 0 || head.0.tree_size > (1 << 62) {
        return Err(KtError::Invalid("invalid stored tree size"));
    }
    signed_time(head.0.timestamp)?;
    Ok(PinnedHead {
        encoded,
        head,
        stored_at_ms,
    })
}

fn encode_head(head: LastTreeHead, now: SystemTime) -> String {
    hex::encode(head.into_stored(now).encode_to_vec())
}

fn encoded_head(head: Option<&PinnedHead>) -> Option<&str> {
    head.map(|head| head.encoded.as_str())
}

fn decode_setting(encoded: &str) -> Result<Vec<u8>, KtError> {
    hex::decode(encoded).map_err(|_| KtError::Invalid("invalid stored transparency state"))
}

pub(crate) mod gossip;

#[cfg(test)]
mod vectors;
