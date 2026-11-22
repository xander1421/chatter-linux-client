//! Chatter transport and durable state for libsignal's KT verifier.

use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use chatter_proto::chat_v1::{KtConsistency, KtSearchRequest, KtSearchResponse};
use libsignal_keytrans::{
    CondensedTreeSearchResponse, DeploymentMode, FullSearchResponse, FullTreeHead, KeyTransparency,
    LastTreeHead, MonitoringData, PublicConfig, SearchContext, SlimSearchRequest,
    StoredMonitoringData, StoredTreeHead, VerifiedSearchResult, VerifyingKey, VrfPublicKey,
};
use prost::Message;

use crate::error::{NetworkError, ServiceError, StorageError};
use crate::network::grpc::AnonymousKtClient;
use crate::storage::repositories::SettingsRepository;

const HEAD: &str = "kt.libsignal.head";
const DISTINGUISHED_HEAD: &str = "kt.libsignal.distinguished";
const DISTINGUISHED: &[u8] = b"distinguished";
const CHECKPOINT_REFRESH: Duration = Duration::from_secs(6 * 60 * 60);
pub(crate) const PIN_RPC_DEADLINE: Duration = Duration::from_secs(10);

static PIN_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
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
}

impl From<KtError> for ServiceError {
    fn from(error: KtError) -> Self {
        match error {
            KtError::Network(error) => Self::Network(error),
            KtError::Storage(error) => Self::Storage(error),
            error => Self::Validation(format!("key transparency: {error}")),
        }
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

/// Owns the lock from loading baselines through verifying and persisting the
/// response, so concurrent routes use the latest verified head.
pub(crate) struct Pin<'a> {
    _lock: tokio::sync::MutexGuard<'static, ()>,
    settings: &'a SettingsRepository,
    last: Option<LastTreeHead>,
    distinguished: LastTreeHead,
    refreshed_checkpoint: bool,
}

impl<'a> Pin<'a> {
    pub(crate) async fn load(
        client: &mut AnonymousKtClient,
        settings: &'a SettingsRepository,
    ) -> Result<Self, KtError> {
        let lock = PIN_LOCK.lock().await;
        let last = load_head(settings, HEAD)?;
        let distinguished = load_head(settings, DISTINGUISHED_HEAD)?;
        let now = SystemTime::now();
        let refresh = distinguished.as_ref().is_none_or(|head| {
            signed_time(head.0.timestamp)
                .ok()
                .and_then(|time| now.duration_since(time).ok())
                .is_some_and(|age| age >= CHECKPOINT_REFRESH)
        });
        let distinguished = if refresh {
            let response = client
                .search(request(DISTINGUISHED, None, distinguished.as_ref()))
                .await?
                .ok_or(KtError::Invalid("distinguished checkpoint is missing"))?;
            let verified = verify_search(
                DISTINGUISHED,
                response,
                SearchContext {
                    last_distinguished_tree_head: distinguished.as_ref(),
                    ..Default::default()
                },
                now,
            )?;
            LastTreeHead(
                verified.state_update.tree_head,
                verified.state_update.tree_root,
            )
        } else {
            distinguished.ok_or(KtError::Invalid("distinguished checkpoint is missing"))?
        };
        Ok(Self {
            _lock: lock,
            settings,
            last,
            distinguished,
            refreshed_checkpoint: refresh,
        })
    }

    pub(crate) fn consistency(&self) -> KtConsistency {
        consistency(self.last.as_ref(), Some(&self.distinguished))
    }

    fn observations(&self, key: &str, label: &[u8]) -> Result<Option<MonitoringData>, KtError> {
        let Some(bytes) = load_setting(self.settings, key)? else {
            return Ok(None);
        };
        let data = StoredMonitoringData::decode(bytes.as_slice())?;
        let positions_valid = self.last.as_ref().is_some_and(|head| {
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
        Ok(Some(data.into()))
    }

    /// Persist the verified head and monitoring observations together.
    pub(crate) fn verify(
        &self,
        label: &[u8],
        response: KtSearchResponse,
        expected: Option<&[u8]>,
    ) -> Result<Vec<u8>, KtError> {
        let state_key = label_state_key(label);
        let now = SystemTime::now();
        let verified = verify_search(
            label,
            response,
            SearchContext {
                last_tree_head: self.last.as_ref(),
                last_distinguished_tree_head: Some(&self.distinguished),
                data: self.observations(&state_key, label)?,
            },
            now,
        )?;
        if expected.is_some_and(|value| value != verified.value) {
            return Err(KtError::Invalid(
                "transparency proof commits a different value",
            ));
        }

        let update = verified.state_update;
        let head = encode_head(LastTreeHead(update.tree_head, update.tree_root), now);
        let conn = self.settings.database().connection()?;
        let tx = conn.unchecked_transaction().map_err(StorageError::from)?;
        SettingsRepository::set_with_connection(&tx, HEAD, &head)?;
        if self.refreshed_checkpoint {
            let checkpoint = encode_head(self.distinguished.clone(), now);
            SettingsRepository::set_with_connection(&tx, DISTINGUISHED_HEAD, &checkpoint)?;
        }
        if let Some(data) = update.monitoring_data {
            let stored = StoredMonitoringData {
                index: data.index.to_vec(),
                pos: data.pos,
                ptrs: data.ptrs,
                owned: data.owned,
                search_key: label.to_vec(),
                max_observed_version: data.max_observed_version,
            };
            SettingsRepository::set_with_connection(
                &tx,
                &state_key,
                &hex::encode(stored.encode_to_vec()),
            )?;
        }
        tx.commit().map_err(StorageError::from)?;
        Ok(verified.value)
    }
}

/// Fetches and verifies an identity's current value on the caller's route.
/// Only an explicit RPC NotFound produces an unlisted result.
pub(crate) async fn search(
    client: &mut AnonymousKtClient,
    settings: &SettingsRepository,
    label: &str,
    deadline: Duration,
) -> Result<Option<Vec<u8>>, KtError> {
    tokio::time::timeout(deadline, async {
        let pin = Pin::load(client, settings).await?;
        let response = client
            .search(request(
                label.as_bytes(),
                pin.last.as_ref(),
                Some(&pin.distinguished),
            ))
            .await?;
        match response {
            Some(response) => pin.verify(label.as_bytes(), response, None).map(Some),
            None if pin
                .observations(&label_state_key(label.as_bytes()), label.as_bytes())?
                .is_some() =>
            {
                Err(KtError::Invalid("previously verified label is missing"))
            }
            None => Ok(None),
        }
    })
    .await
    .unwrap_or(Err(NetworkError::Timeout.into()))
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
    let verified = verify_search(label, response, SearchContext::default(), time)?;
    if verified.value != expected {
        return Err(KtError::Invalid(
            "transparency proof commits a different value",
        ));
    }
    Ok(())
}

fn verify_search(
    label: &[u8],
    response: KtSearchResponse,
    context: SearchContext<'_>,
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
    Ok(verifier()?.verify_search(
        SlimSearchRequest::new(label.to_vec()),
        FullSearchResponse::new(condensed, &full_head),
        context,
        false,
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

fn load_head(settings: &SettingsRepository, key: &str) -> Result<Option<LastTreeHead>, KtError> {
    let Some(bytes) = load_setting(settings, key)? else {
        return Ok(None);
    };
    let head = StoredTreeHead::decode(bytes.as_slice())?
        .into_last_tree_head()
        .ok_or(KtError::Invalid("invalid stored tree head"))?;
    if head.0.tree_size == 0 || head.0.tree_size > (1 << 62) {
        return Err(KtError::Invalid("invalid stored tree size"));
    }
    signed_time(head.0.timestamp)?;
    Ok(Some(head))
}

fn encode_head(head: LastTreeHead, now: SystemTime) -> String {
    hex::encode(head.into_stored(now).encode_to_vec())
}

fn label_state_key(label: &[u8]) -> String {
    format!("kt.libsignal.label.{}", hex::encode(label))
}

fn load_setting(settings: &SettingsRepository, key: &str) -> Result<Option<Vec<u8>>, KtError> {
    settings
        .get_string(key)?
        .map(|encoded| {
            hex::decode(encoded).map_err(|_| KtError::Invalid("invalid stored transparency state"))
        })
        .transpose()
}

#[cfg(test)]
mod vectors;
