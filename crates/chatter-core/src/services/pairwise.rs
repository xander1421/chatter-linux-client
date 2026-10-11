//! Pairwise Signal Protocol encryption for secret chats (anonymous DMs and
//! anonymous groups).
//!
//! Every network call here rides the secret (Tor) channel and fails closed:
//! session establishment is an anonymous prekey fetch + KT check, and the only
//! identified request — refreshing our own sender certificate — goes over Tor
//! too. Nothing here uses the normal pool transport. Delivery to mailboxes is
//! the calling context's job; DMs and server groups use `SignalService`
//! directly.

use std::sync::Arc;

use crate::domain::UserId;
use crate::error::NetworkError;
use crate::mailbox::token_manager::{MailboxToken, TokenManager};
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::services::signal::SignalService;

/// Errors from pairwise encryption operations.
#[derive(Debug, thiserror::Error)]
pub enum PairwiseError {
    /// Session establishment failed.
    #[error("session error: {0}")]
    SessionError(String),

    /// The peer's identity key is not in the Key Transparency log, so no
    /// session is established (never trust-on-first-use).
    #[error("can't verify this contact's security key ({peer_id})")]
    RecipientKeyUnverified { peer_id: String },

    /// Encryption failed.
    #[error("encryption failed: {0}")]
    EncryptionFailed(String),

    /// Decryption failed.
    #[error("decryption failed: {0}")]
    DecryptionFailed(String),

    /// Network error during key fetch.
    #[error("network error: {0}")]
    Network(#[from] NetworkError),

    /// The peer device this secret chat is bound to no longer exists (or the
    /// server offered a different one). The chat is never moved to another of
    /// the peer's devices.
    #[error("this contact's device for the secret chat is gone")]
    PeerDeviceGone,

    /// No pre-fetched blind-signature token left for the anonymous request.
    #[error("no anonymous token available: {0}")]
    NoToken(String),

    /// Local state the caller records around a send could not be written.
    #[error("storage error: {0}")]
    Storage(#[from] crate::error::StorageError),
}

/// Reusable pairwise encryption service using Signal Protocol.
///
/// Handles Signal Protocol session management and Double Ratchet
/// encryption without any delivery-specific logic.
#[derive(Clone)]
pub struct PairwiseService {
    signal_service: Arc<SignalService>,
    grpc_pool: Arc<GrpcClientPool>,
}

impl PairwiseService {
    /// Create a new PairwiseService.
    pub fn new(signal_service: Arc<SignalService>, grpc_pool: Arc<GrpcClientPool>) -> Self {
        Self {
            signal_service,
            grpc_pool,
        }
    }

    /// Authenticated receive journal replay precedes another ratchet decrypt.
    pub(crate) async fn decrypt_reaction_durable(
        &self,
        ciphertext: &[u8],
        key: &crate::storage::receive_artifact::ReceiveKey,
    ) -> Result<crate::services::signal::actor::DurableReceive, crate::error::ServiceError> {
        self.signal_service
            .decrypt_reaction_durable(ciphertext, key)
            .await
            .map_err(|error| match error {
                crate::services::signal::actor::DurableReceiveError::Failure(source) => source,
                crate::services::signal::actor::DurableReceiveError::PlaintextRejected => {
                    crate::error::ServiceError::Validation(
                        "Unratcheted anonymous plaintext rejected".into(),
                    )
                }
            })
    }

    // =========================================================================
    // Secret chats (anon DMs, anon groups) — Tor only, no identified request
    // on the normal transport, ever
    // =========================================================================

    /// Whether a usable session exists with `device_id` of `peer_id`.
    pub async fn has_session(
        &self,
        peer_id: &UserId,
        device_id: u32,
    ) -> Result<bool, PairwiseError> {
        self.signal_service
            .has_session(&peer_id.to_string(), device_id)
            .await
            .map_err(|e| PairwiseError::SessionError(e.to_string()))
    }

    /// Ensure our own sealed-sender certificate is cached.
    ///
    /// Secret-chat encryption uses the cached certificate only. When it is
    /// missing or due for refresh, the ONE identified request a secret send may
    /// make — fetching our own certificate, which names no recipient — goes
    /// over Tor (`secret_route_auth_client`), never the normal pool client;
    /// with no Tor route it fails closed. No-op once cached.
    pub async fn ensure_sender_certificate(&self) -> Result<(), PairwiseError> {
        if self
            .signal_service
            .get_cached_sender_certificate()
            .map_err(|e| PairwiseError::SessionError(e.to_string()))?
            .is_some()
        {
            return Ok(());
        }
        let mut auth_client = self.grpc_pool.secret_route_auth_client()?;
        self.signal_service
            .get_or_request_sender_certificate(&mut auth_client)
            .await
            .map(|_| ())
            .map_err(|e| PairwiseError::SessionError(e.to_string()))
    }

    /// Encrypt to the session we already hold with `device_id` of `peer_id`.
    ///
    /// Local only: the certificate refresh above is the only possible network
    /// call. If the session vanished since the caller checked, this fails
    /// (`SessionUnusable`) instead of re-establishing it — the caller decides
    /// whether to go through [`Self::encrypt_anonymous`].
    pub async fn encrypt_existing_session(
        &self,
        peer_id: &UserId,
        device_id: u32,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, PairwiseError> {
        self.ensure_sender_certificate().await?;
        self.signal_service
            .encrypt_message_anonymous(&peer_id.to_string(), device_id, plaintext)
            .await
            .map_err(|e| PairwiseError::EncryptionFailed(e.to_string()))
    }

    /// Establish a session anonymously if needed, then encrypt.
    ///
    /// The prekey bundle is fetched with a blind-signature token and its
    /// identity key checked against Key Transparency, both over the secret
    /// (Tor) channel; no authenticated request names the peer.
    ///
    /// `device_id` is the peer device the chat is bound to. `None` (first
    /// contact) fetches every device (`*`; the server requires a device id)
    /// and binds ONE of them, the lowest device id the server serves a
    /// bundle for ([`single_device_bundle`]); that device is returned so the
    /// caller can pin it. A pinned device the
    /// server no longer has fails ([`PairwiseError::PeerDeviceGone`]) — the
    /// chat never moves to another of the peer's devices.
    pub async fn encrypt_anonymous(
        &self,
        peer_id: &UserId,
        device_id: Option<u32>,
        plaintext: &[u8],
        token: &MailboxToken,
    ) -> Result<(Vec<u8>, u32), PairwiseError> {
        self.ensure_sender_certificate().await?;
        let peer_id_str = peer_id.to_string();

        let mut anon_auth = self.grpc_pool.secret_auth_client()?;
        let bundle_response = anon_auth
            .get_pre_key_bundle_anonymous(
                &peer_id_str,
                &prekey_request_device(device_id),
                &token.public_key,
                &token.signature,
            )
            .await
            .map_err(|e| match (device_id, &e) {
                (Some(_), NetworkError::NotFound(_)) => PairwiseError::PeerDeviceGone,
                _ => PairwiseError::SessionError(format!("Anonymous prekey fetch failed: {}", e)),
            })?;
        let bundle = single_device_bundle(bundle_response.result, device_id)?;
        let bundle_device = bundle.device_id;
        let bundle_response = crate::proto::chat_v1::GetPreKeyBundleResponse {
            result: Some(
                crate::proto::chat_v1::get_pre_key_bundle_response::Result::Bundle(bundle),
            ),
        };

        // KT gate: the peer's identity key is verified against its signed Key
        // Transparency proof, fetched over the same anonymous channel, before
        // the session is established. A server-forged identity key is
        // REJECTED here.
        let mut anon_kt_client = self.grpc_pool.secret_kt_client()?;
        self.signal_service
            .ensure_session_with_bundle(
                &mut anon_kt_client,
                &peer_id_str,
                bundle_device,
                bundle_response,
            )
            .await
            .map_err(|e| match e {
                crate::error::ServiceError::Crypto(
                    crate::error::CryptoError::NotInTransparencyLog { peer_id },
                ) => PairwiseError::RecipientKeyUnverified { peer_id },
                e => PairwiseError::SessionError(e.to_string()),
            })?;

        let ciphertext = self
            .signal_service
            .encrypt_message_anonymous(&peer_id_str, bundle_device, plaintext)
            .await
            .map_err(|e| PairwiseError::EncryptionFailed(e.to_string()))?;
        Ok((ciphertext, bundle_device))
    }

    /// Deliver `plaintext` to ONE of a peer's device inboxes, fully
    /// anonymously: anonymous prekey fetch (blind token #1), sealed sender,
    /// blind-token send (#2), over Tor only. Carries secret-chat bootstraps —
    /// anon DM requests and anon-group invites — so the request is bound to
    /// the one device that can open it, and so accept it.
    ///
    /// `device_id` as in [`Self::encrypt_anonymous`]. `before_send` runs with
    /// the sealed-to device after encryption, right before the send RPC (the
    /// caller records that the request may now have left); its error aborts
    /// the send. `request_id` is the server's idempotency key
    /// ([`AnonymousMessageClient::send_with_anonymous_credential`]). Returns
    /// the device.
    ///
    /// [`AnonymousMessageClient::send_with_anonymous_credential`]: crate::network::grpc::anonymous_client::AnonymousMessageClient::send_with_anonymous_credential
    #[allow(clippy::too_many_arguments)]
    pub async fn send_to_device_inbox(
        &self,
        tokens: &TokenManager,
        request_id: &str,
        peer_id: &UserId,
        device_id: Option<u32>,
        plaintext: &[u8],
        before_send: impl FnOnce(u32) -> Result<(), PairwiseError>,
    ) -> Result<u32, PairwiseError> {
        // Over Tor or not at all; refuse before any token is spent.
        let mut anon_client = self.grpc_pool.secret_message_client()?;
        let claim = || {
            tokens
                .claim_prefetched_token()
                .map_err(|e| PairwiseError::NoToken(e.to_string()))
        };
        let (sealed_body, device) = self
            .encrypt_anonymous(peer_id, device_id, plaintext, &claim()?)
            .await?;
        let send_token = claim()?;
        before_send(device)?;
        anon_client
            .send_with_anonymous_credential(
                request_id,
                &peer_id.to_string(),
                vec![crate::proto::chat_v1::SealedDeviceMessage {
                    device_id: device,
                    registration_id: 0,
                    sealed_message: Some(crate::proto::chat_v1::SealedSenderMessage {
                        sealed_body,
                        server_key_id: self.signal_service.get_server_key_id(),
                        timestamp: crate::util::time::TimestampNanos::now_anonymized().as_u64(),
                    }),
                }],
                crate::proto::chat_v1::SealedDelivery::NamedDevices,
                0,
                &send_token.public_key,
                &send_token.signature,
            )
            .await?;
        Ok(device)
    }
}

/// The `GetPreKeyBundleRequest.device_id` for an anonymous fetch: the bound
/// device, or `*` (all devices) on first contact. Never empty: the server
/// requires a device id (`auth.proto` `min_len: 1`).
fn prekey_request_device(device_id: Option<u32>) -> String {
    device_id.map_or_else(|| "*".to_string(), |d| d.to_string())
}

/// The ONE bundle a secret chat binds to. A pinned fetch must answer with
/// that device's bundle. A first-contact (`*`) fetch answers with every
/// device that has Kyber material; the lowest device id is taken, so the
/// choice is deterministic.
// Known cost: a `*` fetch also pops one one-time prekey on each device not
// chosen (as Signal's all-device fetch does); a per-device listing RPC would
// avoid that if one-time prekey exhaustion shows up.
fn single_device_bundle(
    result: Option<crate::proto::chat_v1::get_pre_key_bundle_response::Result>,
    pinned: Option<u32>,
) -> Result<crate::proto::chat_v1::SignalPreKeyBundle, PairwiseError> {
    use crate::proto::chat_v1::get_pre_key_bundle_response::Result as R;
    match (result, pinned) {
        (Some(R::Bundle(bundle)), Some(device)) if bundle.device_id != device => {
            Err(PairwiseError::PeerDeviceGone)
        }
        (Some(R::Bundle(bundle)), _) => Ok(bundle),
        (Some(R::Devices(list)), None) => {
            let mut bundle = list
                .bundles
                .into_iter()
                .min_by_key(|b| b.device_id)
                .ok_or_else(|| {
                    PairwiseError::SessionError("peer has no device with prekeys".into())
                })?;
            if bundle.identity_key.is_empty() {
                bundle.identity_key = list.identity_key;
            }
            Ok(bundle)
        }
        _ => Err(PairwiseError::SessionError(
            "anonymous prekey fetch returned no single-device bundle".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// First contact must name a device the server accepts: an empty
    /// device_id failed validation, so "Start secret chat" never worked.
    #[test]
    fn first_contact_prekey_request_names_a_valid_device() {
        let first = prekey_request_device(None);
        assert!(!first.is_empty());
        assert_eq!(first, "*");
        assert_eq!(prekey_request_device(Some(3)), "3");
    }

    #[test]
    fn first_contact_binds_the_lowest_device_and_a_pin_holds() {
        use crate::proto::chat_v1::SignalPreKeyBundle;
        use crate::proto::chat_v1::{
            PreKeyBundleDeviceList, get_pre_key_bundle_response::Result as R,
        };
        let b = |device_id| SignalPreKeyBundle {
            device_id,
            ..Default::default()
        };
        let list = || {
            Some(R::Devices(PreKeyBundleDeviceList {
                bundles: vec![b(7), b(2), b(5)],
                identity_key: vec![9; 33],
            }))
        };
        let chosen = single_device_bundle(list(), None).unwrap();
        assert_eq!(chosen.device_id, 2);
        assert_eq!(
            chosen.identity_key,
            vec![9; 33],
            "list identity key fills an empty one"
        );
        assert!(single_device_bundle(Some(R::Devices(Default::default())), None).is_err());
        // A pinned chat takes only its device's bundle.
        assert_eq!(
            single_device_bundle(Some(R::Bundle(b(4))), Some(4))
                .unwrap()
                .device_id,
            4
        );
        assert!(matches!(
            single_device_bundle(Some(R::Bundle(b(5))), Some(4)),
            Err(PairwiseError::PeerDeviceGone)
        ));
        assert!(single_device_bundle(list(), Some(2)).is_err());
        assert!(single_device_bundle(None, None).is_err());
    }

    /// A secret send must never issue a request on the normal (direct, or
    /// Shield) transport: every secret-path entry point fails closed on a pool
    /// with no Tor route, and the pool's normal channel is never even built.
    #[tokio::test]
    async fn secret_paths_never_touch_the_normal_transport() {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(crate::system::keyring::test_credential_builder());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = crate::client::ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        // A fresh pool: nothing has built its normal channel yet.
        let pool = Arc::new(GrpcClientPool::new("https://localhost:1").unwrap());
        assert!(pool.try_channel().is_none());
        let pairwise = PairwiseService::new(
            client.anon_dms().pairwise_service().signal_service.clone(),
            pool.clone(),
        );
        let peer = UserId::new();
        let token = MailboxToken {
            id: "t".into(),
            public_key: vec![1; 32],
            private_key: vec![2; 64],
            signature: vec![3; 64],
        };
        let no_tor = |r: &PairwiseError| {
            matches!(
                r,
                PairwiseError::Network(NetworkError::SecretChatNeedsTor(_))
            )
        };

        // No cached certificate: the refresh is the one identified request,
        // and it has no Tor route, so it fails rather than going direct.
        let e = pairwise.ensure_sender_certificate().await.unwrap_err();
        assert!(no_tor(&e), "{e}");
        let e = pairwise
            .encrypt_existing_session(&peer, 2, b"x")
            .await
            .unwrap_err();
        assert!(no_tor(&e), "{e}");
        let e = pairwise
            .encrypt_anonymous(&peer, Some(2), b"x", &token)
            .await
            .unwrap_err();
        assert!(no_tor(&e), "{e}");
        let tokens = client.anon_dms().token_manager.clone();
        let e = pairwise
            .send_to_device_inbox(&tokens, "r", &peer, None, b"x", |_| {
                panic!("nothing may be marked sent")
            })
            .await
            .unwrap_err();
        assert!(no_tor(&e), "{e}");

        assert!(
            pool.try_channel().is_none(),
            "a secret-chat path built the normal transport channel"
        );
    }
}
