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

use crate::error::{CryptoError, NetworkError, ServiceError, StorageError};
use crate::network::grpc::AnonymousKtClient;
use crate::storage::repositories::SettingsRepository;

const HEAD: &str = "kt.libsignal.head";
const DISTINGUISHED_HEAD: &str = "kt.libsignal.distinguished";
const DISTINGUISHED: &[u8] = b"distinguished";
const CHECKPOINT_REFRESH: Duration = Duration::from_secs(6 * 60 * 60);
pub(crate) const PIN_RPC_DEADLINE: Duration = Duration::from_secs(10);

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
        Self::load_at(client, settings, verifier()?, SystemTime::now()).await
    }

    async fn load_at(
        client: &mut impl KtTransport,
        settings: &'a SettingsRepository,
        verifier: &'a KeyTransparency,
        now: SystemTime,
    ) -> Result<Self, KtError> {
        let checkpoint = load_head(settings, DISTINGUISHED_HEAD)?;
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
            check_setting(&tx, DISTINGUISHED_HEAD, encoded_head(checkpoint.as_ref()))?;
            SettingsRepository::set_with_connection(&tx, DISTINGUISHED_HEAD, &encoded)?;
            tx.commit().map_err(StorageError::from)?;
            decode_head(encoded)?
        } else {
            checkpoint.ok_or(KtError::Invalid("distinguished checkpoint is missing"))?
        };
        Ok(Self {
            settings,
            verifier,
            now,
            last: load_head(settings, HEAD)?,
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
        let key = label_state_key(label);
        let conn = self.settings.database().read_connection()?;
        let tx = conn.unchecked_transaction().map_err(StorageError::from)?;
        check_setting(&tx, HEAD, encoded_head(self.last.as_ref()))?;
        check_setting(&tx, DISTINGUISHED_HEAD, Some(&self.distinguished.encoded))?;
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
        self.save_search::<()>(observations, verified, None)
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

    fn save_search<T: serde::Serialize>(
        &self,
        observations: Observations,
        verified: VerifiedSearchResult,
        extra: Option<(&str, &T)>,
    ) -> Result<(), KtError> {
        let update = verified.state_update;
        self.save(
            LastTreeHead(update.tree_head, update.tree_root),
            observations,
            update.monitoring_data,
            extra,
        )
    }

    fn save<T: serde::Serialize>(
        &self,
        head: LastTreeHead,
        observations: Observations,
        data: Option<MonitoringData>,
        extra: Option<(&str, &T)>,
    ) -> Result<(), KtError> {
        let mut conn = self.settings.database().connection()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StorageError::from)?;
        check_setting(&tx, HEAD, encoded_head(self.last.as_ref()))?;
        check_setting(&tx, DISTINGUISHED_HEAD, Some(&self.distinguished.encoded))?;
        check_setting(&tx, &observations.key, observations.encoded.as_deref())?;

        if self
            .last_head()
            .is_none_or(|last| last.0.tree_size != head.0.tree_size)
        {
            SettingsRepository::set_with_connection(&tx, HEAD, &encode_head(head, self.now))?;
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
        if let Some((key, value)) = extra {
            SettingsRepository::set_with_connection(&tx, key, value)?;
        }
        tx.commit().map_err(StorageError::from)?;
        Ok(())
    }
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
    label: &str,
    deadline: Duration,
) -> Result<Option<Vec<u8>>, KtError> {
    let verifier = verifier()?;
    with_deadline(deadline, async {
        loop {
            match search_contact(
                client,
                settings,
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
    label: &[u8],
    verifier: &KeyTransparency,
    now: SystemTime,
) -> Result<Option<Vec<u8>>, KtError> {
    let mut pin = Pin::load_at(client, settings, verifier, now).await?;
    let mut observations = pin.observations(label)?;
    let value_key = format!("kt.libsignal.value.{}", hex::encode(label));
    if let Some(data) = observations.data.as_ref() {
        let previous = data.greatest_version();
        let (head, data) = pin.monitor_label(client, label, data).await?;
        let version = data.greatest_version();
        pin.save::<()>(head, observations, Some(data), None)?;
        if let Some((saved_version, value)) = settings.get::<(u32, Vec<u8>)>(&value_key)? {
            if saved_version == previous && version == previous {
                return Ok(Some(value));
            }
        }
        pin = Pin::load_at(client, settings, verifier, now).await?;
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
    pin.save_search(
        observations,
        verified,
        Some((&value_key, &(version, &value))),
    )?;
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

async fn check_own(
    client: &mut impl KtTransport,
    settings: &SettingsRepository,
    label: &[u8],
    expected: &[u8],
    verifier: &KeyTransparency,
    now: SystemTime,
) -> Result<bool, KtError> {
    if own_mismatch(settings, label)? {
        return Ok(true);
    }
    let alert_key = own_mismatch_key(label);
    let pin = Pin::load_at(client, settings, verifier, now).await?;
    let observations = pin.observations(label)?;
    let own_key = format!("kt.libsignal.own-key.{}", hex::encode(label));
    let expected_hex = hex::encode(expected);
    let verified_key = settings.get_string(&own_key)?;
    if let Some(data) = observations
        .data
        .as_ref()
        .filter(|data| data.owned && verified_key.as_ref() == Some(&expected_hex))
    {
        let (head, data) = pin.monitor_label(client, label, data).await?;
        let changed = data.greatest_version() > 0;
        pin.save(
            head,
            observations,
            Some(data),
            changed.then_some((&alert_key, &true)),
        )?;
        return Ok(changed);
    }
    let response = pin
        .search_label(client, label)
        .await?
        .ok_or(KtError::Invalid(
            "own identity key is missing from transparency",
        ))?;
    let verified = pin.verify_response(label, response, &observations, true)?;
    let data = verified
        .state_update
        .monitoring_data
        .as_ref()
        .ok_or(KtError::Invalid(
            "own identity search did not produce monitoring data",
        ))?;
    // Chatter assigns one identity key to an account, including its linked
    // devices. A nonzero version on the first check exposes earlier writes,
    // for which this installation has no locally trusted history.
    let changed = verified.value != expected || data.greatest_version() > 0;
    if changed {
        pin.save_search(observations, verified, Some((&alert_key, &true)))?;
    } else {
        pin.save_search(observations, verified, Some((&own_key, &expected_hex)))?;
    }
    Ok(changed)
}

pub(crate) fn own_mismatch(settings: &SettingsRepository, label: &[u8]) -> Result<bool, KtError> {
    Ok(settings
        .get::<bool>(&own_mismatch_key(label))?
        .unwrap_or(false))
}

fn own_mismatch_key(label: &[u8]) -> String {
    format!("kt.libsignal.own-mismatch.{}", hex::encode(label))
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

fn label_state_key(label: &[u8]) -> String {
    format!("kt.libsignal.label.{}", hex::encode(label))
}

fn decode_setting(encoded: &str) -> Result<Vec<u8>, KtError> {
    hex::decode(encoded).map_err(|_| KtError::Invalid("invalid stored transparency state"))
}

#[cfg(test)]
mod vectors;
