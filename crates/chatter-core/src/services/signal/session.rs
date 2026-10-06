//! Session management and encryption/decryption operations.
//!
//! Handles establishing Signal Protocol sessions and encrypting/decrypting
//! messages using both sealed sender and identified sender modes.
//! All operations are delegated to the SignalActor for thread-safety.

use libsignal_protocol::{
    CiphertextMessageType, DeviceId, ProtocolAddress, PublicKey, SenderCertificate,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::{SignalService, kt_anchor};
#[cfg(feature = "dev-trust-root")]
use crate::crypto::keytransparency::KtError;
use crate::error::{CryptoError, ServiceError};
use crate::network::grpc::auth_client::AuthClient;
use crate::storage::repositories::settings_repo::keys;

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

        locks
            .entry(recipient_key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
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
        proof: &crate::proto::chat_v1::GetProofResponse,
    ) -> Result<(), ServiceError>
    where
        F: FnOnce(u64, u64) -> Fut,
        Fut: std::future::Future<
                Output = Result<
                    crate::proto::chat_v1::GetConsistencyProofResponse,
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
                tracing::error!(
                    recipient = %recipient_id,
                    offered_epoch = sth.epoch,
                    error = %e,
                    "KT REJECT: the offered tree head is not consistent with the head this \
                     device pinned — fork, rollback, split view or rewritten log"
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
        self.verify_identity_key_transparency(auth_client, recipient_id, &identity_key)
            .await
    }

    /// The KT gate on a bare identity key: `Ok(())` iff `identity_key` is
    /// `user_id`'s key in the pinned, append-only transparency log. Used for
    /// prekey bundles and for the authors of signed channel membership changes.
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

    /// The verdict both KT gates (identified and anonymous) reach once the
    /// proof fetch has returned. `path` labels the log lines.
    async fn judge_kt_proof<F, Fut>(
        &self,
        pin: &kt_anchor::PinGuard,
        path: &'static str,
        recipient_id: &str,
        identity_key: &[u8],
        fetched: Result<
            Option<crate::proto::chat_v1::GetProofResponse>,
            crate::error::NetworkError,
        >,
        fetch_consistency: F,
    ) -> Result<(), ServiceError>
    where
        F: FnOnce(u64, u64) -> Fut,
        Fut: std::future::Future<
                Output = Result<
                    crate::proto::chat_v1::GetConsistencyProofResponse,
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
    pub async fn has_session(
        &self,
        recipient_id: &str,
        device_id: u32,
    ) -> Result<bool, ServiceError> {
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
    /// `ensure_sessions_and_access_policy_all_devices` (every device — the path a real DM
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
        let device_id_struct = DeviceId::try_from(device_id)
            .map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
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

        let result = bundle_response
            .result
            .ok_or(ServiceError::Validation("Missing bundle".into()))?;
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

        self.ensure_session(auth_client, recipient_id, device_id)
            .await?;

        let device_id_struct = DeviceId::try_from(device_id)
            .map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let address = ProtocolAddress::new(recipient_id.to_string(), device_id_struct);

        let sender_cert = self.get_or_request_sender_certificate(auth_client).await?;

        self.actor
            .encrypt_sealed(plaintext.to_vec(), sender_cert, address)
            .await
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

        self.ensure_session(auth_client, recipient_id, device_id)
            .await?;

        let device_id_struct = DeviceId::try_from(device_id)
            .map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let address = ProtocolAddress::new(recipient_id.to_string(), device_id_struct);

        self.actor
            .encrypt_identified(plaintext.to_vec(), address)
            .await
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
    ///
    /// `server_timestamp_ns` is the envelope's server timestamp; the sender
    /// certificate's expiry is judged against it, not the local clock, so a
    /// message read late (device offline) or under clock skew still opens.
    /// Zero means the server sent none and is refused: it would disable the
    /// expiry check outright.
    pub async fn decrypt_message(
        &self,
        ciphertext: &[u8],
        server_timestamp_ns: u64,
    ) -> Result<(ProtocolAddress, Vec<u8>, CiphertextMessageType), ServiceError> {
        if server_timestamp_ns == 0 {
            return Err(ServiceError::Validation(
                "sealed sender envelope has no server timestamp".into(),
            ));
        }
        let validation_time =
            libsignal_protocol::Timestamp::from_epoch_millis(server_timestamp_ns / 1_000_000);
        let local_uuid = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)?
            .ok_or(ServiceError::NotLoggedIn)?;
        let local_device_id = self
            .settings_repo
            .get::<u32>(keys::CURRENT_DEVICE_ID)?
            .ok_or(ServiceError::NotLoggedIn)?;
        let device_id_struct = DeviceId::new(
            local_device_id
                .try_into()
                .map_err(|_| ServiceError::Validation("ID too large".into()))?,
        )
        .map_err(|_| ServiceError::Validation("Invalid ID".into()))?;

        self.actor
            .decrypt_sealed(
                ciphertext.to_vec(),
                self.trust_root,
                validation_time,
                local_uuid,
                device_id_struct,
            )
            .await
            .inspect_err(|e| self.note_identity_change(e))
    }

    /// Anonymous reaction receive journal. Caller supplies the actual transport
    /// scope/envelope binding; the authenticated account/device are checked here
    /// and again by the actor's SQLite commit/replay boundary.
    pub async fn decrypt_reaction_durable(
        &self,
        ciphertext: &[u8],
        key: &crate::storage::receive_artifact::ReceiveKey,
    ) -> Result<super::actor::DurableReceive, super::actor::DurableReceiveError> {
        key.validate()?;
        let account = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)?
            .ok_or(ServiceError::NotLoggedIn)?;
        let device = self
            .settings_repo
            .get::<u32>(keys::CURRENT_DEVICE_ID)?
            .ok_or(ServiceError::NotLoggedIn)?;
        if account != key.local_account || device != key.local_device {
            return Err(ServiceError::Validation("receive account/device mismatch".into()).into());
        }
        self.actor
            .decrypt_reaction_durable(ciphertext.to_vec(), self.trust_root, key.clone())
            .await
            .inspect_err(|e| {
                if let super::actor::DurableReceiveError::Failure(source) = e {
                    self.note_identity_change(source)
                }
            })
    }

    /// Decrypt an incoming identified sender message.
    pub async fn decrypt_message_identified(
        &self,
        ciphertext: &[u8],
        sender_id: &str,
        sender_device_id: u32,
    ) -> Result<Vec<u8>, ServiceError> {
        let device_id_struct = DeviceId::try_from(sender_device_id)
            .map_err(|_| ServiceError::Validation("Invalid ID".into()))?;
        let sender_address = ProtocolAddress::new(sender_id.to_string(), device_id_struct);

        self.actor
            .decrypt_identified(ciphertext.to_vec(), sender_address)
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
        if let Some(cert) = self.get_cached_sender_certificate()? {
            return Ok(cert);
        }

        let cert_response = auth_client.request_sender_certificate().await?;
        let proto_cert = cert_response
            .certificate
            .ok_or(ServiceError::Validation("Missing certificate".into()))?;
        let sender_cert = SenderCertificate::deserialize(&proto_cert.certificate)
            .map_err(|_| ServiceError::Validation("Invalid cert".into()))?;

        use base64::Engine;
        let cert_b64 = base64::engine::general_purpose::STANDARD.encode(&proto_cert.certificate);
        let _ = self.settings_repo.set(keys::SENDER_CERTIFICATE, &cert_b64);
        let _ = self.settings_repo.set(
            keys::SENDER_CERTIFICATE_REFRESH_AFTER,
            &cert_response.refresh_after,
        );
        let _ = self.settings_repo.set(
            keys::SENDER_CERTIFICATE_SERVER_KEY_ID,
            &proto_cert.server_key_id,
        );

        Ok(sender_cert)
    }

    /// Convert Protobuf PreKeyBundle to libsignal PreKeyBundle.
    pub(crate) fn convert_prekey_bundle(
        proto: crate::proto::chat_v1::SignalPreKeyBundle,
    ) -> Result<libsignal_protocol::PreKeyBundle, ServiceError> {
        let registration_id = proto.registration_id;
        let device_id = DeviceId::try_from(proto.device_id)
            .map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let identity_key = libsignal_protocol::IdentityKey::decode(&proto.identity_key)
            .map_err(|_| ServiceError::Validation("Invalid identity key".into()))?;
        let signed_prekey = proto
            .signed_prekey
            .ok_or(ServiceError::Validation("Missing signed prekey".into()))?;
        let signed_prekey_id = libsignal_protocol::SignedPreKeyId::from(signed_prekey.id);
        let signed_prekey_public = PublicKey::deserialize(&signed_prekey.public_key)
            .map_err(|_| ServiceError::Validation("Invalid signed prekey public key".into()))?;
        let signed_prekey_signature = signed_prekey.signature;

        let pre_key_option = if let Some(pk) = proto.one_time_prekey {
            Some((
                libsignal_protocol::PreKeyId::from(pk.id),
                PublicKey::deserialize(&pk.public_key)
                    .map_err(|_| ServiceError::Validation("Invalid one-time prekey".into()))?,
            ))
        } else {
            None
        };

        let kyber_prekey = proto
            .kyber_prekey
            .ok_or(ServiceError::Validation("Missing Kyber prekey".into()))?;
        let kyber_id = libsignal_protocol::KyberPreKeyId::from(kyber_prekey.id);
        let kyber_public =
            libsignal_protocol::kem::Key::try_from(kyber_prekey.public_key.as_slice())
                .map_err(|_| ServiceError::Validation("Invalid Kyber key".into()))?;
        let kyber_signature = kyber_prekey.signature;

        libsignal_protocol::PreKeyBundle::new(
            registration_id,
            device_id,
            pre_key_option,
            signed_prekey_id,
            signed_prekey_public,
            signed_prekey_signature,
            kyber_id,
            kyber_public,
            kyber_signature,
            identity_key,
        )
        .map_err(|e| ServiceError::Validation(format!("Invalid bundle: {}", e)))
    }

    // =========================================================================
    // Multi-Device Operations
    // =========================================================================

    /// Ensure sessions exist for the devices of a recipient, the way
    /// Signal-Desktop's `sendToServiceId` does: the devices we hold a session
    /// with are used as they are, and only a recipient we hold no session
    /// with at all gets a `"*"` prekey-bundle fetch. Which devices the
    /// recipient has now is the server's answer to the send itself
    /// (`SealedDelivery::AllDevices`, `NetworkError::MismatchedDevices`),
    /// not a re-fetch on a timer.
    ///
    /// Returns the device ids we hold a session with afterwards (possibly
    /// none: the recipient has no device to fetch a bundle for, or it is our
    /// own user and this is our only device).
    pub async fn ensure_sessions_all_devices(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
    ) -> Result<Vec<u32>, ServiceError> {
        Ok(self
            .ensure_sessions_and_access_policy_all_devices(auth_client, recipient_id, false)
            .await?
            .0)
    }

    /// Same as [`Self::ensure_sessions_all_devices`], but also returns whether
    /// the recipient accepts sealed sender from anyone
    /// (`SignalPreKeyBundle.unrestricted_unidentified_access`, Signal's
    /// `unrestrictedUnidentifiedAccess`), as last read from a bundle fetch.
    /// The server never serves the UAK itself — a sender derives it from the
    /// recipient's profile key (`MessageService::sealed_access_key`).
    ///
    /// `need_access_policy`: the caller has no profile key for the recipient,
    /// so the flag decides whether the send can be sealed. When it was not
    /// read yet this run, the `"*"` fetch is made even though we hold
    /// sessions — it pops one one-time prekey per device, so it is made only
    /// for a caller that needs the flag.
    ///
    /// When the recipient is our own user, THIS device is dropped before any
    /// session work: there is no session with ourselves, and libsignal
    /// refuses to seal to ourselves. Every own-user fan-out (self-sync, SKDM,
    /// SenderKeyRequest) routes through here, so no caller has to remember it.
    pub async fn ensure_sessions_and_access_policy_all_devices(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        need_access_policy: bool,
    ) -> Result<(Vec<u32>, bool), ServiceError> {
        use crate::proto::chat_v1::get_pre_key_bundle_response;

        // Same refusal as `ensure_session`, and it matters MORE here: this is
        // the path `encrypt_message_all_devices` takes, so it is the one a real
        // DM send goes through. Note the loop below short-circuits on
        // `has_session` per device exactly like the single-device path, so
        // without this the stale post-reinstall session is reused silently.
        self.refuse_pending_identity_change(recipient_id)?;

        // Single-flight per recipient. Concurrent first sends to one peer (a
        // new friend's prewarm and profile-key send) each fetched a bundle —
        // popping a one-time prekey apiece — and built a session over the
        // other's. The one that waits here finds the sessions the first built.
        let build_lock = self
            .get_encrypt_lock(&format!("{recipient_id}:establish"))
            .await;
        let _build = build_lock.lock().await;

        let this_device = (self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .ok()
            .flatten()
            .as_deref()
            == Some(recipient_id))
        .then(|| {
            self.settings_repo
                .get::<u32>(keys::CURRENT_DEVICE_ID)
                .ok()
                .flatten()
        })
        .flatten();

        let with_session: Vec<u32> = self
            .session_devices(recipient_id)
            .await?
            .into_iter()
            .filter(|d| Some(*d) != this_device)
            .collect();
        let known_policy = self.unrestricted_access(recipient_id);
        if !with_session.is_empty() && (known_policy.is_some() || !need_access_policy) {
            return Ok((with_session, known_policy.unwrap_or(false)));
        }

        // Fetch prekey bundles for ALL devices using wildcard
        let response = match auth_client
            .get_pre_key_bundles_all_devices(recipient_id)
            .await
        {
            Ok(r) => r,
            // A throttled policy read must not fail a send we can already
            // encrypt: it goes identified, as it would have without the flag.
            Err(crate::error::NetworkError::RateLimited) if !with_session.is_empty() => {
                tracing::warn!(
                    recipient = %recipient_id,
                    devices = ?with_session,
                    "Access-policy fetch throttled; sending to the devices we hold sessions with"
                );
                return Ok((with_session, false));
            }
            Err(e) => return Err(e.into()),
        };

        let Some(result) = response.result else {
            // No bundle at all: nothing to fetch a session from. The send
            // still asks the server, which names any device we are missing.
            return Ok((with_session, false));
        };

        match result {
            // Multiple devices response
            get_pre_key_bundle_response::Result::Devices(device_list) => {
                // The policy is per user; every bundle carries the same flag.
                let unrestricted = device_list
                    .bundles
                    .iter()
                    .any(|b| b.unrestricted_unidentified_access == Some(true));
                self.record_unrestricted_access(recipient_id, unrestricted);

                let bundles = without_device(device_list.bundles, this_device);
                if bundles.is_empty() {
                    return Ok((Vec::new(), unrestricted));
                }

                let mut established_device_ids = Vec::with_capacity(bundles.len());
                let mut last_error = None;
                for proto_bundle in bundles {
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
                        established_device_ids.push(device_id);
                        continue;
                    }

                    // Establish new session from bundle. A device that fails
                    // here is left out; the server names it missing on the
                    // send, and `reconcile_devices` tries it once more on its
                    // own (Signal-Desktop fails the recipient if that fails
                    // too: `OutgoingMessage.doSendMessage` -> `registerError`).
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
                                last_error = Some(e);
                                continue;
                            }
                            match self.actor.establish_session(address, bundle).await {
                                Ok(_) => established_device_ids.push(device_id),
                                Err(e) => {
                                    tracing::warn!(
                                        recipient = %recipient_id,
                                        device_id,
                                        error = %e,
                                        "Failed to establish session with device"
                                    );
                                    last_error = Some(e);
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
                            last_error = Some(e);
                        }
                    }
                }

                if established_device_ids.is_empty() {
                    return Err(last_error.unwrap_or_else(|| {
                        ServiceError::Validation(
                            "Failed to establish session with any device".into(),
                        )
                    }));
                }

                tracing::info!(
                    recipient = %recipient_id,
                    device_count = established_device_ids.len(),
                    device_ids = ?established_device_ids,
                    "Established sessions with all devices"
                );
                Ok((established_device_ids, unrestricted))
            }

            // Single device fallback (server doesn't support multi-device query)
            get_pre_key_bundle_response::Result::Bundle(single_bundle) => {
                let device_id = single_bundle.device_id;
                let unrestricted = single_bundle.unrestricted_unidentified_access == Some(true);
                self.record_unrestricted_access(recipient_id, unrestricted);
                if this_device == Some(device_id) {
                    return Ok((Vec::new(), unrestricted));
                }
                self.ensure_session(auth_client, recipient_id, device_id)
                    .await?;
                Ok((vec![device_id], unrestricted))
            }
        }
    }

    /// The recipient's `unrestricted_unidentified_access` flag from the last
    /// bundle fetch this run, if any.
    fn unrestricted_access(&self, recipient_id: &str) -> Option<bool> {
        self.unrestricted_access
            .lock()
            .ok()?
            .get(recipient_id)
            .copied()
    }

    fn record_unrestricted_access(&self, recipient_id: &str, unrestricted: bool) {
        if let Ok(mut flags) = self.unrestricted_access.lock() {
            flags.insert(recipient_id.to_string(), unrestricted);
        }
    }

    /// Bring our sessions with `recipient_id` in line with what the server
    /// reported for a send (`NetworkError::MismatchedDevices`), as
    /// Signal-Desktop does on a 409 / 410: sessions of `extra` and `stale`
    /// devices are archived, and a prekey bundle is fetched for each `missing`
    /// and `stale` device — those devices only.
    ///
    /// A `missing` device we do hold a session with is one we failed to
    /// encrypt for, so its session is archived and replaced too.
    pub async fn reconcile_devices(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        missing: &[u32],
        extra: &[u32],
        stale: &[u32],
    ) -> Result<(), ServiceError> {
        for &device_id in extra.iter().chain(stale).chain(missing) {
            self.actor
                .archive_session_for_resend(recipient_id.to_string(), device_id, None)
                .await?;
        }
        for &device_id in missing.iter().chain(stale) {
            self.ensure_session(auth_client, recipient_id, device_id)
                .await?;
        }
        Ok(())
    }

    /// Archive the current session state with every device of `recipient_id`
    /// we hold a session record for, through the same actor path as
    /// [`Self::reconcile_devices`]. The next all-devices send then finds no
    /// usable session and fetches fresh bundles. Returns how many were archived.
    pub async fn archive_peer_sessions(&self, recipient_id: &str) -> Result<usize, ServiceError> {
        let devices = session_device_ids(
            recipient_id,
            self.key_store
                .list_sessions_for_user(recipient_id)
                .map_err(ServiceError::Storage)?
                .iter()
                .map(|(address, _)| address.as_str()),
        );
        let mut archived = 0;
        for device_id in devices {
            if self
                .actor
                .archive_session_for_resend(recipient_id.to_string(), device_id, None)
                .await?
            {
                archived += 1;
            }
        }
        Ok(archived)
    }

    /// The registration id the current session with a device was established
    /// against — libsignal records it from the bundle when we initiate and
    /// from the PreKey message when they do (`SessionRecord::
    /// remote_registration_id`). `None` when there is no current session or
    /// it holds no id: Signal never names a device without its real id
    /// (Signal-Desktop `OutgoingMessage` `destinationRegistrationId`), and 0
    /// is not a registration id.
    pub fn session_registration_id(&self, recipient_id: &str, device_id: u32) -> Option<u32> {
        self.key_store
            .load_session(&format!("{recipient_id}:{device_id}"))
            .ok()
            .flatten()
            .and_then(|bytes| libsignal_protocol::SessionRecord::deserialize(&bytes).ok())
            .and_then(|record| record.remote_registration_id().ok())
            .filter(|&id| id != 0)
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
            .encrypt_message_all_devices_with_access_policy(
                auth_client,
                recipient_id,
                plaintext,
                false,
            )
            .await?
            .0
            .into_iter()
            .map(|(device_id, sealed)| (device_id, sealed.ciphertext))
            .collect())
    }

    /// Same as [`Self::encrypt_message_all_devices`], but each ciphertext
    /// carries the registration id of the session it was encrypted with (the
    /// server-checked device set), and it also returns whether
    /// the recipient accepts sealed sender from anyone (see
    /// [`Self::ensure_sessions_and_access_policy_all_devices`]), which the
    /// caller needs to pick the `Unidentified-Access-Key` for the deposit.
    ///
    /// To our own user this device is excluded. An empty map — no device to
    /// encrypt for — is `Ok`; the server-checked send asks the server about it.
    ///
    /// `need_access_policy`: see
    /// [`Self::ensure_sessions_and_access_policy_all_devices`].
    pub async fn encrypt_message_all_devices_with_access_policy(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        plaintext: &[u8],
        need_access_policy: bool,
    ) -> Result<(HashMap<u32, DeviceCiphertext>, bool), ServiceError> {
        // First ensure we have sessions with all devices
        let (device_ids, unrestricted) = self
            .ensure_sessions_and_access_policy_all_devices(
                auth_client,
                recipient_id,
                need_access_policy,
            )
            .await?;
        if device_ids.is_empty() {
            return Ok((HashMap::new(), unrestricted));
        }

        // Get sender certificate once (used for all encryptions). The
        // server picks the device id from the authenticated session.
        let sender_cert = self.get_or_request_sender_certificate(auth_client).await?;

        // Encrypt to each device
        let mut ciphertexts: HashMap<u32, DeviceCiphertext> =
            HashMap::with_capacity(device_ids.len());
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

            match self
                .actor
                .encrypt_sealed(plaintext.to_vec(), sender_cert.clone(), address)
                .await
            {
                // Read under the encrypt lock, from the session that just
                // encrypted. A session that cannot name its device's
                // registration id is not sent to: listing the device with 0
                // would switch off the server's stale-device check.
                Ok(ciphertext) => match self.session_registration_id(recipient_id, device_id) {
                    Some(registration_id) => {
                        ciphertexts.insert(
                            device_id,
                            DeviceCiphertext {
                                ciphertext,
                                registration_id,
                            },
                        );
                    }
                    None => {
                        tracing::warn!(
                            recipient = %recipient_id,
                            device_id,
                            "Encrypted session has no registration id — device not sent to"
                        );
                        failure_count += 1;
                    }
                },
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

        Ok((ciphertexts, unrestricted))
    }

    /// Server-DM envelope admission: actual ciphertexts for the entire current
    /// device set are checked before any durable ratchet write. The ordinary
    /// uncapped helper remains available for non-sealed-body transports.
    pub(crate) async fn encrypt_message_all_devices_bounded_with_access_policy(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        plaintext: &[u8],
        need_access_policy: bool,
    ) -> Result<(HashMap<u32, DeviceCiphertext>, bool), ServiceError> {
        let (devices, unrestricted) = self
            .ensure_sessions_and_access_policy_all_devices(
                auth_client,
                recipient_id,
                need_access_policy,
            )
            .await?;
        let ciphertexts = self
            .encrypt_message_devices_bounded(auth_client, recipient_id, &devices, plaintext)
            .await?;
        Ok((ciphertexts, unrestricted))
    }

    /// Original-recipient retry uses precisely these devices. Never replace
    /// them with today's roster or commit some devices before checking others.
    pub(crate) async fn encrypt_message_devices_bounded(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        devices: &[u32],
        plaintext: &[u8],
    ) -> Result<HashMap<u32, DeviceCiphertext>, ServiceError> {
        let mut devices = devices.to_vec();
        devices.sort_unstable();
        devices.dedup();
        if devices.is_empty() {
            return Ok(HashMap::new());
        }
        let mut destinations = Vec::with_capacity(devices.len());
        let mut guards = Vec::with_capacity(devices.len());
        for device in devices {
            let lock = self
                .get_encrypt_lock(&format!("{recipient_id}:{device}"))
                .await;
            guards.push(lock.lock_owned().await);
            self.ensure_session(auth_client, recipient_id, device)
                .await?;
            let id = DeviceId::try_from(device)
                .map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
            destinations.push(ProtocolAddress::new(recipient_id.to_string(), id));
        }
        let certificate = self.get_or_request_sender_certificate(auth_client).await?;
        let batch = self
            .actor
            .encrypt_sealed_bounded_batch(plaintext.to_vec(), certificate, destinations)
            .await?;
        Ok(batch
            .into_iter()
            .map(|(address, ciphertext, registration_id)| {
                (
                    u32::from(address.device_id()),
                    DeviceCiphertext {
                        ciphertext,
                        registration_id,
                    },
                )
            })
            .collect())
    }

    /// Devices of `recipient_id` we hold a usable session with, ascending.
    /// Local only — no RPC. The session store doubles as our cache of a
    /// peer's device list: the normal-mode all-devices bundle fetch
    /// (`ensure_sessions_and_access_policy_all_devices`) and the per-device
    /// fetches of `reconcile_devices` leave a session per current device,
    /// which is what Shield may use without naming the peer.
    pub async fn session_devices(&self, recipient_id: &str) -> Result<Vec<u32>, ServiceError> {
        let ids = session_device_ids(
            recipient_id,
            self.key_store
                .list_sessions_for_user(recipient_id)
                .map_err(ServiceError::Storage)?
                .iter()
                .map(|(address, _)| address.as_str()),
        );
        let mut usable = Vec::with_capacity(ids.len());
        for device_id in ids {
            let Ok(device) = DeviceId::try_from(device_id) else {
                continue;
            };
            if self
                .actor
                .has_session(ProtocolAddress::new(recipient_id.to_string(), device))
                .await?
            {
                usable.push(device_id);
            }
        }
        Ok(usable)
    }

    /// Encrypt for `device_ids` of `recipient_id` — devices we already hold a
    /// usable session with (from [`Self::session_devices`], filtered by the
    /// caller) — with no prekey-bundle fetch, so no RPC that names the
    /// recipient. Errors when none of them encrypts.
    pub async fn encrypt_message_session_devices(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        device_ids: Vec<u32>,
        plaintext: &[u8],
    ) -> Result<HashMap<u32, Vec<u8>>, ServiceError> {
        self.refuse_pending_identity_change(recipient_id)?;
        let sender_cert = self.get_or_request_sender_certificate(auth_client).await?;

        let mut ciphertexts = HashMap::with_capacity(device_ids.len());
        for device_id in device_ids {
            let Ok(device) = DeviceId::try_from(device_id) else {
                continue;
            };
            let address = ProtocolAddress::new(recipient_id.to_string(), device);
            let lock = self
                .get_encrypt_lock(&format!("{recipient_id}:{device_id}"))
                .await;
            let _guard = lock.lock().await;
            match self
                .actor
                .encrypt_sealed(plaintext.to_vec(), sender_cert.clone(), address)
                .await
            {
                Ok(ciphertext) => {
                    ciphertexts.insert(device_id, ciphertext);
                }
                Err(e) => tracing::warn!(
                    recipient = %recipient_id,
                    device_id,
                    error = %e,
                    "Failed to encrypt for device"
                ),
            }
        }
        if ciphertexts.is_empty() {
            return Err(ServiceError::Validation(format!(
                "no usable session with any device of {recipient_id}"
            )));
        }
        Ok(ciphertexts)
    }

    // =========================================================================
    // Anonymous Operations (for AnonDM - no authenticated requests)
    // =========================================================================

    /// Key Transparency gate for the ANONYMOUS-DM path (Phase 3a).
    ///
    /// The anonymous analog of `verify_peer_key_transparency`. Same semantics
    /// (verify the bundle's identity key is the one committed in the signed KT
    /// log under the pinned tree-head key; REJECT a mismatch; REJECT a
    /// transport/server error and `NotFound`), but it fetches the
    /// proof over the isolated ANONYMOUS channel via `AnonymousKtClient` — no
    /// authenticated identity — because the anon path has none.
    ///
    /// This closes the anon-path MITM hole: before Phase 3a the anon path
    /// trusted peer identity keys blindly (no KT call at all). Its tree head
    /// is held to this device's pin exactly like the identified path's.
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
        // Same pin discipline as the identified path (`kt_anchor::lock_pin`).
        let pin = kt_anchor::lock_pin().await;
        let fetched = kt_anchor::pinned_rpc(anon_kt_client.get_proof(recipient_id)).await;
        // The consistency proof comes over the same anonymous channel: without
        // it a server could fork the log for anonymous lookups alone.
        self.judge_kt_proof(
            &pin,
            "KT(anon)",
            recipient_id,
            &identity_key,
            fetched,
            |old, new| anon_kt_client.get_consistency_proof(old, new),
        )
        .await
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
        let cached_cert_b64: Option<String> =
            self.settings_repo.get_string(keys::SENDER_CERTIFICATE)?;
        let cached_refresh_after: Option<u64> = self
            .settings_repo
            .get::<u64>(keys::SENDER_CERTIFICATE_REFRESH_AFTER)?;

        if let (Some(cert_b64), Some(refresh_after)) = (&cached_cert_b64, cached_refresh_after) {
            if !sender_cert_refresh_due(std::time::SystemTime::now(), refresh_after) {
                use base64::Engine;
                if let Ok(cert_bytes) = base64::engine::general_purpose::STANDARD.decode(cert_b64) {
                    if let Ok(cert) = SenderCertificate::deserialize(&cert_bytes) {
                        // The certificate names the sender's user AND device.
                        // Every login is a new device id, so a cert cached
                        // before a Log Out -> Log In names a device that no
                        // longer exists: recipients looked up the wrong session
                        // and every sealed message failed to decrypt. Only a
                        // cert for the current user and device is reusable.
                        //
                        // It must also still chain to the trust root this build
                        // pins: a cert cached under an older server signing key
                        // (a key rotation, or a dev stack re-seeded) is rejected
                        // by every recipient, who then drops the message
                        // silently. Refetch instead of sending into that.
                        if self.cert_is_for_this_device(&cert)
                            && crate::crypto::signal::sealed_sender::validate_trust_root(&cert)
                        {
                            return Ok(Some(cert));
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    fn cert_is_for_this_device(&self, cert: &SenderCertificate) -> bool {
        let user = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .ok()
            .flatten();
        let device: Option<u32> = self
            .settings_repo
            .get(keys::CURRENT_DEVICE_ID)
            .ok()
            .flatten();
        match (user, device, cert.sender_uuid(), cert.sender_device_id()) {
            (Some(u), Some(d), Ok(cu), Ok(cd)) => u == cu && u32::from(cd) == d,
            _ => false,
        }
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
        let sender_cert = self.get_cached_sender_certificate()?.ok_or_else(|| {
            ServiceError::Validation(
                "No cached sender certificate - ensure pre-fetch during login".into(),
            )
        })?;

        self.actor
            .encrypt_sealed(plaintext.to_vec(), sender_cert, address)
            .await
    }
}

/// The fan-out targets minus `skip` (self-sync skips the sending device), so a
/// skipped device never gets a session built or a ratchet step taken for it.
fn without_device(
    mut bundles: Vec<crate::proto::chat_v1::SignalPreKeyBundle>,
    skip: Option<u32>,
) -> Vec<crate::proto::chat_v1::SignalPreKeyBundle> {
    bundles.retain(|b| Some(b.device_id) != skip);
    bundles
}

/// Whether a cached sender certificate is due for refresh. `refresh_after_ns`
/// is the server's `RequestSenderCertificateResponse.refresh_after`, Unix
/// NANOSECONDS (auth/service/sealed_sender.go: `refreshAfter.UnixNano()`).
/// Comparing it against milliseconds made "now" ~10^6 times too small, so the
/// cache never refreshed early and was only replaced once recipients had
/// started rejecting the expired certificate.
fn sender_cert_refresh_due(now: std::time::SystemTime, refresh_after_ns: u64) -> bool {
    let now_ns = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    now_ns >= refresh_after_ns
}

#[cfg(test)]
mod sender_cert_refresh_tests {
    use super::sender_cert_refresh_due;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn refresh_after_is_compared_in_nanoseconds() {
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let now_ns = 1_800_000_000u64 * 1_000_000_000;
        // A refresh point one hour in the past (as the server sends it, in ns)
        // is due; the old millisecond comparison called it 10^6 x in the future.
        assert!(sender_cert_refresh_due(now, now_ns - 3_600 * 1_000_000_000));
        assert!(sender_cert_refresh_due(now, now_ns));
        // One hour ahead is not due yet.
        assert!(!sender_cert_refresh_due(
            now,
            now_ns + 3_600 * 1_000_000_000
        ));
    }

    #[test]
    fn a_millisecond_scale_value_is_long_past() {
        // Guards the regression direction: a value on the ms scale for "now"
        // is ~1970 in nanoseconds, so it must read as due, never as fresh.
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert!(sender_cert_refresh_due(now, 1_800_000_000_000));
    }
}

#[cfg(test)]
mod tests {
    use super::without_device;
    use crate::proto::chat_v1::SignalPreKeyBundle;

    fn bundles(ids: &[u32]) -> Vec<SignalPreKeyBundle> {
        ids.iter()
            .map(|&device_id| SignalPreKeyBundle {
                device_id,
                ..Default::default()
            })
            .collect()
    }

    #[test]
    fn self_fanout_with_only_this_device_has_no_targets() {
        assert!(without_device(bundles(&[1]), Some(1)).is_empty());
    }

    #[test]
    fn skip_drops_only_the_named_device() {
        let ids: Vec<u32> = without_device(bundles(&[1, 2, 3]), Some(2))
            .iter()
            .map(|b| b.device_id)
            .collect();
        assert_eq!(ids, [1, 3]);
        assert_eq!(without_device(bundles(&[1, 2]), None).len(), 2);
    }
}

/// One device's sealed ciphertext and the registration id of the session
/// that encrypted it, from [`SignalService::encrypt_message_all_devices_with_access_policy`].
pub struct DeviceCiphertext {
    pub ciphertext: Vec<u8>,
    pub registration_id: u32,
}

/// Device ids of `recipient_id`'s stored session addresses (`"user:device"`).
fn session_device_ids<'a>(
    recipient_id: &str,
    addresses: impl Iterator<Item = &'a str>,
) -> Vec<u32> {
    let prefix = format!("{recipient_id}:");
    let mut ids: Vec<u32> = addresses
        .filter_map(|a| a.strip_prefix(prefix.as_str())?.parse().ok())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

#[cfg(test)]
mod kt_pin_tests {
    use super::*;
    use crate::proto::chat_v1::{GetConsistencyProofResponse, GetProofResponse, SignedTreeHead};

    /// The anchor both KT paths (identified and `verify_peer_key_transparency_anon`)
    /// run after the inclusion proof: a head that forks from the pin is refused
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
        assert_eq!(kt_anchor::pinned_head(&settings).unwrap().root, [1u8; 32]);
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
        assert!(kt_anchor::pinned_head(&settings).is_none());
    }
}

#[cfg(test)]
mod session_device_ids_tests {
    #[test]
    fn parses_only_this_recipients_devices() {
        let addrs = ["u1:1", "u1:6", "u10:2", "u1:x", "u1:6"];
        assert_eq!(
            super::session_device_ids("u1", addrs.into_iter()),
            vec![1, 6]
        );
    }
}
