//! Session management and encryption/decryption operations.
//!
//! Handles establishing Signal Protocol sessions and encrypting/decrypting
//! messages using both sealed sender and identified sender modes.
//! All operations are delegated to the SignalActor for thread-safety.

use std::collections::HashMap;
use std::sync::Arc;
use libsignal_protocol::{CiphertextMessageType, ProtocolAddress, DeviceId, SenderCertificate, PublicKey};
use tokio::sync::Mutex;

use crate::crypto::signal::sesame::ConvergenceResult;
use crate::network::grpc::auth_client::AuthClient;
use crate::storage::repositories::settings_repo::keys;
use crate::error::{CryptoError, ServiceError};
use super::{kt_anchor, SignalService};
use crate::crypto::keytransparency::KtError;

/// Maximum number of encrypt locks to keep in cache.
const MAX_ENCRYPT_LOCKS: usize = 500;

impl SignalService {
    /// Get or create a lock for encrypting to a specific recipient.
    pub(crate) async fn get_encrypt_lock(&self, recipient_key: &str) -> Arc<Mutex<()>> {
        let mut locks = self.encrypt_locks.lock().await;
        let mut lru = self.encrypt_lock_lru.lock().await;

        lru.retain(|key| key != recipient_key);
        lru.push_back(recipient_key.to_string());

        while lru.len() > MAX_ENCRYPT_LOCKS {
            if let Some(oldest) = lru.pop_front() {
                if let Some(lock) = locks.get(&oldest) {
                    if Arc::strong_count(lock) == 1 {
                        locks.remove(&oldest);
                    } else {
                        lru.push_front(oldest);
                        break;
                    }
                }
            }
        }

        locks.entry(recipient_key.to_string()).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
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
    /// * peer has NO leaf (NotFound)    -> TOFU fallback with a `warn!` naming the
    ///   downgrade — the ~1500 pre-KT users have no leaf and would otherwise be
    ///   unreachable; their keys are simply not transparency-verified yet.
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
    async fn check_tree_head_consistency(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        proof: &crate::proto::chat_v1::GetProofResponse,
    ) -> Result<(), ServiceError> {
        let Some(sth) = proof.signed_tree_head.as_ref() else {
            return Err(ServiceError::Validation(
                "KT proof has no signed tree head".to_string(),
            ));
        };

        let action = kt_anchor::classify_head(&self.settings_repo, sth).map_err(|e| {
            tracing::error!(
                recipient = %recipient_id,
                offered_epoch = sth.epoch,
                error = %e,
                "KT REJECT: the offered tree head contradicts the head this device already \
                 accepted — fork, rollback or split view"
            );
            ServiceError::Validation(format!(
                "Key Transparency consistency check failed for {recipient_id}: {e}"
            ))
        })?;

        let (old_epoch, new_epoch) = match action {
            kt_anchor::AnchorAction::Settled(kt_anchor::AnchorOutcome::FirstPin) => {
                tracing::info!(
                    epoch = sth.epoch,
                    "KT: pinned the transparency log's tree head for the first time"
                );
                return Ok(());
            }
            kt_anchor::AnchorAction::Settled(_) => return Ok(()),
            kt_anchor::AnchorAction::NeedsProof { old_epoch, new_epoch } => (old_epoch, new_epoch),
        };

        let resp = auth_client
            .get_consistency_proof(old_epoch, new_epoch)
            .await
            .map_err(|e| {
                tracing::error!(
                    old_epoch, new_epoch, error = %e,
                    "KT REJECT: could not fetch the consistency proof"
                );
                ServiceError::Validation(format!(
                    "Key Transparency consistency check failed for {recipient_id}: {}",
                    KtError::ConsistencyUnavailable
                ))
            })?;

        match kt_anchor::accept_advance(&self.settings_repo, sth, &resp) {
            Ok(_) => {
                tracing::info!(
                    old_epoch,
                    new_epoch,
                    "KT: advanced the pinned tree head; log verified append-only"
                );
                Ok(())
            }
            Err(e) => {
                tracing::error!(
                    recipient = %recipient_id,
                    old_epoch, new_epoch, error = %e,
                    "KT REJECT: the transparency log is NOT append-only between the head this \
                     device pinned and the one now offered — the log was rewritten"
                );
                Err(ServiceError::Validation(format!(
                    "Key Transparency consistency check failed for {recipient_id}: {e}"
                )))
            }
        }
    }

    async fn verify_peer_key_transparency(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        bundle: &libsignal_protocol::PreKeyBundle,
    ) -> Result<(), ServiceError> {
        let identity_key = bundle
            .identity_key()
            .map_err(|e| ServiceError::Validation(format!("bundle missing identity key: {e}")))?
            .serialize();

        match auth_client.get_key_transparency_proof(recipient_id).await {
            Ok(Some(proof)) => {
                match crate::crypto::verify_kt_proof(recipient_id, &identity_key, &proof) {
                    Ok(()) => {
                        let epoch = proof
                            .signed_tree_head
                            .as_ref()
                            .map(|s| s.epoch)
                            .unwrap_or_default();
                        tracing::debug!(
                            recipient = %recipient_id,
                            epoch,
                            "KT: peer identity key verified against the signed transparency log"
                        );
                        // The inclusion proof says the key is in the tree the
                        // server just signed. It says nothing about WHICH tree,
                        // so on its own it cannot see a fork or a rollback.
                        // Pin the head across time as well.
                        self.check_tree_head_consistency(auth_client, recipient_id, &proof).await
                    }
                    Err(e) => {
                        tracing::error!(
                            recipient = %recipient_id,
                            error = %e,
                            "KT REJECT: refusing session — bundle identity key is NOT the key \
                             committed in the Key Transparency log (possible forged/MITM key, F9)"
                        );
                        Err(ServiceError::Validation(format!(
                            "Key Transparency verification failed for {recipient_id}: {e}"
                        )))
                    }
                }
            }
            Ok(None) => {
                tracing::warn!(
                    recipient = %recipient_id,
                    "KT DOWNGRADE (TOFU): peer has no leaf in the Key Transparency log (pre-KT \
                     user); trusting the bundle identity key WITHOUT a KT proof — this key is \
                     NOT transparency-verified"
                );
                Ok(())
            }
            Err(e) => {
                tracing::error!(
                    recipient = %recipient_id,
                    error = %e,
                    "KT: proof fetch failed (transport/server error, not NotFound) — refusing \
                     session (an error is not evidence of an absent leaf)"
                );
                Err(ServiceError::Network(e))
            }
        }
    }

    /// Establish a session with a remote recipient if one doesn't exist or is unusable.
    ///
    /// **Important**: This checks if a usable session already exists before fetching prekeys.
    /// If we already have a valid session (e.g., from receiving a message from this peer),
    /// we reuse it rather than creating a new one. This is critical for the bootstrap
    /// protocol where one party receives a PreKey message and should use that session
    /// for the response, not fetch new prekeys.
    ///
    /// Sessions that are structurally corrupted (fail to serialize) are treated as
    /// non-existent and trigger fresh prekey bundle fetch.
    /// Returns true if a *usable* Signal session already exists for
    /// (recipient, device) — structurally valid, not merely present (the same
    /// check `ensure_session` uses to decide whether to skip the prekey fetch).
    ///
    /// Lets callers avoid an authenticated prekey fetch on first contact by
    /// branching to an anonymous session-establishment path instead.
    pub async fn has_session(&self, recipient_id: &str, device_id: u32) -> Result<bool, ServiceError> {
        let device_id_struct = DeviceId::try_from(device_id)
            .map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let address = ProtocolAddress::new(recipient_id.to_string(), device_id_struct);
        self.actor.has_session(address).await
    }

    /// Refuse to open or reuse a session with a peer whose identity change is
    /// still parked and unaccepted.
    ///
    /// Lives on its own because there are TWO send-side entry points and only
    /// one of them used to check: `ensure_session` (single device) and
    /// `ensure_sessions_and_uak_all_devices` (every device — the path a real DM
    /// send actually takes, via `encrypt_message_all_devices`). Guarding only
    /// the first left the hot path free to encrypt under the dead ratchet, so
    /// the user was never told anything and the message left undeliverable.
    ///
    /// Per-account, not per-device: identity keys are pinned per account, so a
    /// change parked against any device wedges all of them.
    fn refuse_pending_identity_change(&self, recipient_id: &str) -> Result<(), ServiceError> {
        let Some((key, source, device_id)) = self
            .key_store
            .get_pending_identity_change(recipient_id)
            .map_err(ServiceError::Storage)?
        else {
            return Ok(());
        };
        Err(ServiceError::Crypto(CryptoError::UntrustedIdentity {
            peer_id: recipient_id.to_string(),
            device_id,
            new_identity_key: Some(key),
            attribution: crate::error::IdentityChangeSource::from_db_str(&source),
        }))
    }

    pub async fn ensure_session(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        device_id: u32,
    ) -> Result<(), ServiceError> {
        let device_id_struct = DeviceId::try_from(device_id).map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let address = ProtocolAddress::new(recipient_id.to_string(), device_id_struct);

        // SEND HALF of the identity-change wedge, and it is NOT
        // `establish_session` in the common case.
        //
        // After a peer reinstall a stale session is usually still on disk, so
        // `has_session` is true, the prekey fetch is skipped, and
        // `establish_session` is never reached. libsignal's send-side trust
        // check (session_cipher.rs:137) then compares the SESSION's stored
        // remote identity against the pinned identity — both are the peer's OLD
        // key, so it passes and we happily encrypt under a ratchet the peer no
        // longer has. The message goes out undeliverable, with nothing to
        // signal that it is wrong.
        //
        // Nothing local can detect that; the change is only knowable once an
        // inbound message or a fresh bundle has surfaced it. So this consults
        // what those paths parked, and refuses to reuse the stale session until
        // the user decides.
        self.refuse_pending_identity_change(recipient_id)?;

        // Check if we already have a usable session with this peer
        // has_session now validates structural integrity (not just existence)
        if self.actor.has_session(address.clone()).await? {
            tracing::debug!(
                recipient = %recipient_id,
                device_id,
                "Usable session already exists, skipping prekey fetch"
            );
            return Ok(());
        }

        // No usable session — either doesn't exist or is corrupted
        tracing::debug!(
            recipient = %recipient_id,
            device_id,
            "No usable session exists, fetching prekey bundle"
        );

        let bundle_response = auth_client
            .get_pre_key_bundle(recipient_id, &device_id.to_string())
            .await?;

        let result = bundle_response.result.ok_or(ServiceError::Validation("Missing bundle".into()))?;
        let proto_bundle = match result {
            crate::proto::chat_v1::get_pre_key_bundle_response::Result::Bundle(b) => b,
            _ => return Err(ServiceError::Validation("Unexpected response type".into())),
        };

        let bundle = Self::convert_prekey_bundle(proto_bundle)?;
        // KT gate: verify the peer's identity key against the transparency log
        // before establishing (rejects a server-forged identity key).
        self.verify_peer_key_transparency(auth_client, recipient_id, &bundle)
            .await?;
        self.actor
            .establish_session(address, bundle)
            .await
            .inspect_err(|e| self.note_identity_change(e))
    }

    /// Encrypt a message using Sealed Sender.
    pub async fn encrypt_message(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        device_id: u32,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, ServiceError> {
        let lock_key = format!("{}:{}", recipient_id, device_id);
        let lock = self.get_encrypt_lock(&lock_key).await;
        let _guard = lock.lock().await;

        self.ensure_session(auth_client, recipient_id, device_id).await?;

        let device_id_struct = DeviceId::try_from(device_id).map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let address = ProtocolAddress::new(recipient_id.to_string(), device_id_struct);

        let sender_cert = self.get_or_request_sender_certificate(auth_client).await?;

        self.actor.encrypt_sealed(plaintext.to_vec(), sender_cert, address).await
    }

    /// Encrypt a message using basic Signal Protocol (Identified).
    pub async fn encrypt_message_identified(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        device_id: u32,
        plaintext: &[u8],
    ) -> Result<(Vec<u8>, u8), ServiceError> {
        let lock_key = format!("{}:{}", recipient_id, device_id);
        let lock = self.get_encrypt_lock(&lock_key).await;
        let _guard = lock.lock().await;

        self.ensure_session(auth_client, recipient_id, device_id).await?;

        let device_id_struct = DeviceId::try_from(device_id).map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let address = ProtocolAddress::new(recipient_id.to_string(), device_id_struct);

        self.actor.encrypt_identified(plaintext.to_vec(), address).await
    }

    /// Seal a serialized `DecryptionErrorMessage` for one recipient device.
    ///
    /// The DEM rides as session-LESS `PlaintextContent` inside sealed sender —
    /// deliberately not ratchet-encrypted, because the session with the
    /// recipient is exactly what the report says is broken. Only the
    /// recipient's identity key (already pinned) and our sender certificate
    /// are needed.
    pub async fn encrypt_decryption_error_report(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        device_id: u32,
        dem_serialized: Vec<u8>,
    ) -> Result<Vec<u8>, ServiceError> {
        let device_id_struct = DeviceId::try_from(device_id)
            .map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let address = ProtocolAddress::new(recipient_id.to_string(), device_id_struct);
        let sender_cert = self.get_or_request_sender_certificate(auth_client).await?;
        self.actor
            .encrypt_decryption_error_sealed(dem_serialized, sender_cert, address)
            .await
    }

    /// Decrypt an incoming Sealed Sender message.
    ///
    /// Returns the authenticated sender, the plaintext, and the inner
    /// `CiphertextMessageType`. Callers MUST reject
    /// `CiphertextMessageType::Plaintext` unless they are the
    /// DecryptionErrorMessage handler: those bytes skipped the Double Ratchet,
    /// so nothing checked the sender's identity key, replayed counters, or
    /// forward secrecy.
    pub async fn decrypt_message(
        &self,
        ciphertext: &[u8],
    ) -> Result<(ProtocolAddress, Vec<u8>, CiphertextMessageType), ServiceError> {
        let local_uuid = self.settings_repo.get_string(keys::CURRENT_USER_ID)?.ok_or(ServiceError::NotLoggedIn)?;
        let local_device_id = self.settings_repo.get::<u32>(keys::CURRENT_DEVICE_ID)?.ok_or(ServiceError::NotLoggedIn)?;
        let device_id_struct = DeviceId::new(local_device_id.try_into().map_err(|_| ServiceError::Validation("ID too large".into()))?).map_err(|_| ServiceError::Validation("Invalid ID".into()))?;

        self.actor
            .decrypt_sealed(ciphertext.to_vec(), self.trust_root, local_uuid, device_id_struct)
            .await
            .inspect_err(|e| self.note_identity_change(e))
    }

    /// Decrypt an incoming identified sender message.
    pub async fn decrypt_message_identified(
        &self,
        ciphertext: &[u8],
        sender_id: &str,
        sender_device_id: u32,
    ) -> Result<Vec<u8>, ServiceError> {
        let device_id_struct = DeviceId::try_from(sender_device_id).map_err(|_| ServiceError::Validation("Invalid ID".into()))?;
        let sender_address = ProtocolAddress::new(sender_id.to_string(), device_id_struct);

        self.actor
            .decrypt_identified(ciphertext.to_vec(), sender_address)
            .await
            .inspect_err(|e| self.note_identity_change(e))
    }

    /// Decrypt a message with session convergence (Sesame).
    pub async fn decrypt_with_convergence(
        &self,
        envelope: &[u8],
        sender_id: &str,
        sender_device_id: u32,
        sessions: Vec<(Vec<u8>, libsignal_protocol::SessionRecord)>,
    ) -> Result<ConvergenceResult, ServiceError> {
        self.actor
            .decrypt_with_convergence(
                envelope.to_vec(),
                sender_id.to_string(),
                sender_device_id,
                sessions,
            )
            .await
            .inspect_err(|e| self.note_identity_change(e))
    }

    /// Get sender certificate from cache or request a new one.
    ///
    /// The server picks the device id from the authenticated session, so
    /// this no longer takes a device_id argument.
    pub(crate) async fn get_or_request_sender_certificate(
        &self,
        auth_client: &mut AuthClient,
    ) -> Result<SenderCertificate, ServiceError> {
        let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;

        let cached_cert_b64: Option<String> = self.settings_repo.get_string(keys::SENDER_CERTIFICATE)?;
        let cached_refresh_after: Option<u64> = self.settings_repo.get::<u64>(keys::SENDER_CERTIFICATE_REFRESH_AFTER)?;

        if let (Some(cert_b64), Some(refresh_after)) = (&cached_cert_b64, cached_refresh_after) {
            if now_ms < refresh_after {
                use base64::Engine;
                if let Ok(cert_bytes) = base64::engine::general_purpose::STANDARD.decode(cert_b64) {
                    if let Ok(cert) = SenderCertificate::deserialize(&cert_bytes) {
                        return Ok(cert);
                    }
                }
            }
        }

        let cert_response = auth_client.request_sender_certificate(None).await?;
        let proto_cert = cert_response.certificate.ok_or(ServiceError::Validation("Missing certificate".into()))?;
        let sender_cert = SenderCertificate::deserialize(&proto_cert.certificate).map_err(|_| ServiceError::Validation("Invalid cert".into()))?;

        use base64::Engine;
        let cert_b64 = base64::engine::general_purpose::STANDARD.encode(&proto_cert.certificate);
        let _ = self.settings_repo.set(keys::SENDER_CERTIFICATE, &cert_b64);
        let _ = self.settings_repo.set(keys::SENDER_CERTIFICATE_REFRESH_AFTER, &cert_response.refresh_after);
        let _ = self.settings_repo.set(keys::SENDER_CERTIFICATE_SERVER_KEY_ID, &proto_cert.server_key_id);

        Ok(sender_cert)
    }

    /// Convert Protobuf PreKeyBundle to libsignal PreKeyBundle.
    pub(crate) fn convert_prekey_bundle(proto: crate::proto::chat_v1::SignalPreKeyBundle) -> Result<libsignal_protocol::PreKeyBundle, ServiceError> {
        let registration_id = proto.registration_id;
        let device_id = DeviceId::try_from(proto.device_id).map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let identity_key = libsignal_protocol::IdentityKey::decode(&proto.identity_key).map_err(|_| ServiceError::Validation("Invalid identity key".into()))?;
        let signed_prekey = proto.signed_prekey.ok_or(ServiceError::Validation("Missing signed prekey".into()))?;
        let signed_prekey_id = libsignal_protocol::SignedPreKeyId::from(signed_prekey.id);
        let signed_prekey_public = PublicKey::deserialize(&signed_prekey.public_key).map_err(|_| ServiceError::Validation("Invalid signed prekey public key".into()))?;
        let signed_prekey_signature = signed_prekey.signature;

        let pre_key_option = if let Some(pk) = proto.one_time_prekey {
            Some((libsignal_protocol::PreKeyId::from(pk.id), PublicKey::deserialize(&pk.public_key).map_err(|_| ServiceError::Validation("Invalid one-time prekey".into()))?))
        } else {
            None
        };

        let kyber_prekey = proto.kyber_prekey.ok_or(ServiceError::Validation("Missing Kyber prekey".into()))?;
        let kyber_id = libsignal_protocol::KyberPreKeyId::from(kyber_prekey.id);
        let kyber_public = libsignal_protocol::kem::Key::try_from(kyber_prekey.public_key.as_slice()).map_err(|_| ServiceError::Validation("Invalid Kyber key".into()))?;
        let kyber_signature = kyber_prekey.signature;

        libsignal_protocol::PreKeyBundle::new(
            registration_id, device_id, pre_key_option,
            signed_prekey_id, signed_prekey_public, signed_prekey_signature,
            kyber_id, kyber_public, kyber_signature, identity_key,
        ).map_err(|e| ServiceError::Validation(format!("Invalid bundle: {}", e)))
    }

    pub fn set_trust_root(&mut self, key: PublicKey) {
        self.trust_root = key;
    }

    // =========================================================================
    // Multi-Device Operations
    // =========================================================================

    /// Ensure sessions exist for ALL devices of a recipient.
    ///
    /// Fetches prekey bundles for all devices and establishes sessions.
    /// Returns the list of device IDs for which sessions were established.
    ///
    /// This is used to send messages to all of a recipient's devices,
    /// ensuring they receive the message on any device they're using.
    pub async fn ensure_sessions_all_devices(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
    ) -> Result<Vec<u32>, ServiceError> {
        Ok(self
            .ensure_sessions_and_uak_all_devices(auth_client, recipient_id)
            .await?
            .0)
    }

    /// Same as [`Self::ensure_sessions_all_devices`], but also returns the
    /// recipient's Unidentified Access Key as carried on their prekey bundle.
    ///
    /// The UAK rides along on the bundle the server already returns
    /// (`SignalPreKeyBundle.unidentified_access_key`, set server-side in
    /// `internal/services/auth/service/signal_protocol.go`). Harvesting it from
    /// the fetch we are already making is deliberate: a second bundle fetch
    /// purely to read the UAK would burn one of the recipient's one-time
    /// prekeys per message and drain their OPK pool.
    ///
    /// Returns `None` for the UAK only if the server omitted it, which callers
    /// must treat as a sealed-sender degradation rather than a normal outcome.
    pub async fn ensure_sessions_and_uak_all_devices(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
    ) -> Result<(Vec<u32>, Option<[u8; 16]>), ServiceError> {
        use crate::proto::chat_v1::get_pre_key_bundle_response;

        // Same refusal as `ensure_session`, and it matters MORE here: this is
        // the path `encrypt_message_all_devices` takes, so it is the one a real
        // DM send goes through. Note the loop below short-circuits on
        // `has_session` per device exactly like the single-device path, so
        // without this the stale post-reinstall session is reused silently.
        self.refuse_pending_identity_change(recipient_id)?;

        /// The UAK is a fixed 16 bytes (`uakSize` in internal/middleware/auth.go).
        /// Anything else is a malformed bundle, not a usable key.
        fn uak_from_bundle(uak: &Option<Vec<u8>>) -> Option<[u8; 16]> {
            uak.as_ref()?.as_slice().try_into().ok()
        }

        // Fetch prekey bundles for ALL devices using wildcard
        let response = auth_client.get_pre_key_bundles_all_devices(recipient_id).await?;

        let result = response.result.ok_or(ServiceError::Validation(
            "No device bundles returned for recipient".into(),
        ))?;

        match result {
            // Multiple devices response
            get_pre_key_bundle_response::Result::Devices(device_list) => {
                if device_list.bundles.is_empty() {
                    return Err(ServiceError::Validation(
                        "No devices registered for recipient".into(),
                    ));
                }

                let mut established_device_ids = Vec::with_capacity(device_list.bundles.len());

                // Every bundle carries the same per-user UAK; take the first
                // well-formed one.
                let recipient_uak = device_list
                    .bundles
                    .iter()
                    .find_map(|b| uak_from_bundle(&b.unidentified_access_key));

                for proto_bundle in device_list.bundles {
                    let device_id = proto_bundle.device_id;
                    let device_id_struct = match DeviceId::try_from(device_id) {
                        Ok(d) => d,
                        Err(_) => {
                            tracing::warn!(
                                recipient = %recipient_id,
                                device_id,
                                "Invalid device ID, skipping"
                            );
                            continue;
                        }
                    };
                    let address = ProtocolAddress::new(recipient_id.to_string(), device_id_struct);

                    // Check if a usable session already exists
                    if self.actor.has_session(address.clone()).await? {
                        tracing::debug!(
                            recipient = %recipient_id,
                            device_id,
                            "Usable session already exists for device"
                        );
                        established_device_ids.push(device_id);
                        continue;
                    }

                    // Establish new session from bundle
                    match Self::convert_prekey_bundle(proto_bundle) {
                        Ok(bundle) => {
                            // KT gate: a device whose identity key is not committed
                            // in the transparency log is skipped (a forged key fails
                            // for every device, leaving established_device_ids empty
                            // -> the whole recipient is refused below).
                            if let Err(e) = self
                                .verify_peer_key_transparency(auth_client, recipient_id, &bundle)
                                .await
                            {
                                tracing::warn!(
                                    recipient = %recipient_id,
                                    device_id,
                                    error = %e,
                                    "KT verification failed for device bundle, skipping device"
                                );
                                continue;
                            }
                            match self.actor.establish_session(address, bundle).await {
                                Ok(_) => {
                                    tracing::debug!(
                                        recipient = %recipient_id,
                                        device_id,
                                        "Established session with device"
                                    );
                                    established_device_ids.push(device_id);
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        recipient = %recipient_id,
                                        device_id,
                                        error = %e,
                                        "Failed to establish session with device"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                recipient = %recipient_id,
                                device_id,
                                error = %e,
                                "Failed to convert prekey bundle for device"
                            );
                        }
                    }
                }

                if established_device_ids.is_empty() {
                    return Err(ServiceError::Validation(
                        "Failed to establish session with any device".into(),
                    ));
                }

                tracing::info!(
                    recipient = %recipient_id,
                    device_count = established_device_ids.len(),
                    device_ids = ?established_device_ids,
                    "Established sessions with all devices"
                );

                Ok((established_device_ids, recipient_uak))
            }

            // Single device fallback (server doesn't support multi-device query)
            get_pre_key_bundle_response::Result::Bundle(single_bundle) => {
                let device_id = single_bundle.device_id;
                let recipient_uak = uak_from_bundle(&single_bundle.unidentified_access_key);
                self.ensure_session(auth_client, recipient_id, device_id).await?;
                Ok((vec![device_id], recipient_uak))
            }
        }
    }

    /// Encrypt a message for ALL devices of a recipient.
    ///
    /// Returns a map of device_id -> ciphertext for each device that was
    /// successfully encrypted to. If encryption fails for some devices,
    /// those devices are skipped and a warning is logged.
    ///
    /// # Errors
    ///
    /// Returns an error only if ALL devices fail to encrypt. Partial
    /// success is considered acceptable (the message will reach some devices).
    pub async fn encrypt_message_all_devices(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        plaintext: &[u8],
    ) -> Result<HashMap<u32, Vec<u8>>, ServiceError> {
        Ok(self
            .encrypt_message_all_devices_with_uak(auth_client, recipient_id, plaintext)
            .await?
            .0)
    }

    /// Same as [`Self::encrypt_message_all_devices`], but also returns the
    /// recipient's UAK from their prekey bundle so the caller can authorize the
    /// deposit with `Unidentified-Access-Key` instead of Basic auth — which is
    /// what actually keeps the sender out of the server's delivery log.
    ///
    /// See [`Self::ensure_sessions_and_uak_all_devices`] for why the UAK is
    /// harvested from the bundle fetch rather than fetched separately.
    pub async fn encrypt_message_all_devices_with_uak(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        plaintext: &[u8],
    ) -> Result<(HashMap<u32, Vec<u8>>, Option<[u8; 16]>), ServiceError> {
        // First ensure we have sessions with all devices
        let (device_ids, recipient_uak) = self
            .ensure_sessions_and_uak_all_devices(auth_client, recipient_id)
            .await?;

        // Get sender certificate once (used for all encryptions). The
        // server picks the device id from the authenticated session.
        let sender_cert = self.get_or_request_sender_certificate(auth_client).await?;

        // Encrypt to each device
        let mut ciphertexts: HashMap<u32, Vec<u8>> = HashMap::with_capacity(device_ids.len());
        let mut failure_count = 0;

        for device_id in device_ids {
            let lock_key = format!("{}:{}", recipient_id, device_id);
            let lock = self.get_encrypt_lock(&lock_key).await;
            let _guard = lock.lock().await;

            let device_id_struct = match DeviceId::try_from(device_id) {
                Ok(d) => d,
                Err(_) => {
                    tracing::warn!(device_id, "Invalid device ID during encryption");
                    failure_count += 1;
                    continue;
                }
            };
            let address = ProtocolAddress::new(recipient_id.to_string(), device_id_struct);

            match self.actor.encrypt_sealed(plaintext.to_vec(), sender_cert.clone(), address).await {
                Ok(ciphertext) => {
                    ciphertexts.insert(device_id, ciphertext);
                }
                Err(e) => {
                    tracing::warn!(
                        recipient = %recipient_id,
                        device_id,
                        error = %e,
                        "Failed to encrypt for device"
                    );
                    failure_count += 1;
                }
            }
        }

        if ciphertexts.is_empty() {
            return Err(ServiceError::Validation(format!(
                "Failed to encrypt for any device ({} failures)",
                failure_count
            )));
        }

        tracing::info!(
            recipient = %recipient_id,
            success_count = ciphertexts.len(),
            failure_count,
            "Encrypted message for recipient's devices"
        );

        Ok((ciphertexts, recipient_uak))
    }

    // =========================================================================
    // Anonymous Operations (for AnonDM - no authenticated requests)
    // =========================================================================

    /// Key Transparency gate for the ANONYMOUS-DM path (Phase 3a).
    ///
    /// The anonymous analog of `verify_peer_key_transparency`. Same semantics
    /// (verify the bundle's identity key is the one committed in the signed KT
    /// log under the pinned tree-head key; REJECT a mismatch; REJECT a
    /// transport/server error; TOFU-warn only on `NotFound`), but it fetches the
    /// proof over the isolated ANONYMOUS channel via `AnonymousKtClient` — no
    /// authenticated identity — because the anon path has none.
    ///
    /// This closes the anon-path MITM hole: before Phase 3a the anon path
    /// trusted peer identity keys blindly (no KT call at all).
    async fn verify_peer_key_transparency_anon(
        &self,
        anon_kt_client: &mut crate::network::grpc::AnonymousKtClient,
        recipient_id: &str,
        bundle: &libsignal_protocol::PreKeyBundle,
    ) -> Result<(), ServiceError> {
        let identity_key = bundle
            .identity_key()
            .map_err(|e| ServiceError::Validation(format!("bundle missing identity key: {e}")))?
            .serialize();

        match anon_kt_client.get_proof(recipient_id).await {
            Ok(Some(proof)) => {
                match crate::crypto::verify_kt_proof(recipient_id, &identity_key, &proof) {
                    Ok(()) => {
                        let epoch = proof
                            .signed_tree_head
                            .as_ref()
                            .map(|s| s.epoch)
                            .unwrap_or_default();
                        tracing::debug!(
                            recipient = %recipient_id,
                            epoch,
                            "KT(anon): peer identity key verified against the signed transparency log"
                        );
                        Ok(())
                    }
                    Err(e) => {
                        tracing::error!(
                            recipient = %recipient_id,
                            error = %e,
                            "KT(anon) REJECT: refusing anonymous session — bundle identity key is \
                             NOT the key committed in the Key Transparency log (possible \
                             forged/MITM key, F9 on the anon path)"
                        );
                        Err(ServiceError::Validation(format!(
                            "Key Transparency verification failed for {recipient_id}: {e}"
                        )))
                    }
                }
            }
            Ok(None) => {
                tracing::warn!(
                    recipient = %recipient_id,
                    "KT(anon) DOWNGRADE (TOFU): peer has no leaf in the Key Transparency log \
                     (pre-KT user); trusting the bundle identity key WITHOUT a KT proof — this \
                     key is NOT transparency-verified"
                );
                Ok(())
            }
            Err(e) => {
                tracing::error!(
                    recipient = %recipient_id,
                    error = %e,
                    "KT(anon): proof fetch failed (transport/server error, not NotFound) — \
                     refusing session (an error is not evidence of an absent leaf)"
                );
                Err(ServiceError::Network(e))
            }
        }
    }

    /// Establish a session using a pre-fetched prekey bundle.
    ///
    /// This allows establishing sessions without making an authenticated
    /// prekey fetch request (the bundle was fetched anonymously).
    ///
    /// KT gate (Phase 3a): before trusting the bundle's identity key, verify it
    /// against the peer's signed Key Transparency proof, fetched over the
    /// anonymous channel (`anon_kt_client`). A server-forged identity key is
    /// REJECTED here, exactly as the identified path rejects it in
    /// `ensure_session`. Without this the anon path was the last KT-ungated
    /// session-establishment path — a MITM hole on the metadata-private path.
    pub async fn ensure_session_with_bundle(
        &self,
        anon_kt_client: &mut crate::network::grpc::AnonymousKtClient,
        recipient_id: &str,
        device_id: u32,
        bundle_response: crate::proto::chat_v1::GetPreKeyBundleResponse,
    ) -> Result<(), ServiceError> {
        let device_id_struct = DeviceId::try_from(device_id)
            .map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let address = ProtocolAddress::new(recipient_id.to_string(), device_id_struct);

        // Check if we already have a usable session
        if self.actor.has_session(address.clone()).await? {
            tracing::debug!(
                recipient = %recipient_id,
                device_id,
                "Usable session already exists, skipping bundle processing"
            );
            return Ok(());
        }

        let result = bundle_response
            .result
            .ok_or(ServiceError::Validation("Missing bundle".into()))?;
        let proto_bundle = match result {
            crate::proto::chat_v1::get_pre_key_bundle_response::Result::Bundle(b) => b,
            _ => return Err(ServiceError::Validation("Unexpected response type".into())),
        };

        let bundle = Self::convert_prekey_bundle(proto_bundle)?;
        // KT gate (anon path): reject a forged peer identity key before it is
        // ever trusted. Mirrors the identified path's verify_peer_key_transparency.
        self.verify_peer_key_transparency_anon(anon_kt_client, recipient_id, &bundle)
            .await?;
        self.actor.establish_session(address, bundle).await
    }

    /// Get cached sender certificate without making auth request.
    ///
    /// Returns None if no valid cached certificate exists.
    pub fn get_cached_sender_certificate(&self) -> Result<Option<SenderCertificate>, ServiceError> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let cached_cert_b64: Option<String> =
            self.settings_repo.get_string(keys::SENDER_CERTIFICATE)?;
        let cached_refresh_after: Option<u64> =
            self.settings_repo.get::<u64>(keys::SENDER_CERTIFICATE_REFRESH_AFTER)?;

        if let (Some(cert_b64), Some(refresh_after)) = (&cached_cert_b64, cached_refresh_after) {
            if now_ms < refresh_after {
                use base64::Engine;
                if let Ok(cert_bytes) =
                    base64::engine::general_purpose::STANDARD.decode(cert_b64)
                {
                    if let Ok(cert) = SenderCertificate::deserialize(&cert_bytes) {
                        return Ok(Some(cert));
                    }
                }
            }
        }

        Ok(None)
    }

    /// Get the cached server key ID from the sender certificate.
    ///
    /// Returns the server_key_id that was cached when the sender certificate
    /// was fetched. Falls back to 1 if not yet cached (pre-first-fetch).
    pub fn get_server_key_id(&self) -> u32 {
        self.settings_repo
            .get::<u32>(keys::SENDER_CERTIFICATE_SERVER_KEY_ID)
            .ok()
            .flatten()
            .unwrap_or(1)
    }

    /// Encrypt a message anonymously (using cached sender cert, no auth requests).
    ///
    /// Pre-conditions:
    /// - Session must already be established (call ensure_session_with_bundle first)
    /// - Sender certificate must be cached (pre-fetched during login)
    pub async fn encrypt_message_anonymous(
        &self,
        recipient_id: &str,
        device_id: u32,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, ServiceError> {
        let lock_key = format!("{}:{}", recipient_id, device_id);
        let lock = self.get_encrypt_lock(&lock_key).await;
        let _guard = lock.lock().await;

        let device_id_struct = DeviceId::try_from(device_id)
            .map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let address = ProtocolAddress::new(recipient_id.to_string(), device_id_struct);

        // Verify a usable session exists (should have been established with anonymous bundle)
        if !self.actor.has_session(address.clone()).await? {
            return Err(ServiceError::Crypto(
                crate::error::CryptoError::SessionUnusable {
                    address: format!("{}:{}", recipient_id, device_id),
                    reason: "no usable session — call ensure_session_with_bundle first".into(),
                },
            ));
        }

        // Use cached sender certificate (must be pre-fetched during login)
        let sender_cert = self
            .get_cached_sender_certificate()?
            .ok_or_else(|| ServiceError::Validation(
                "No cached sender certificate - ensure pre-fetch during login".into(),
            ))?;

        self.actor
            .encrypt_sealed(plaintext.to_vec(), sender_cert, address)
            .await
    }
}
