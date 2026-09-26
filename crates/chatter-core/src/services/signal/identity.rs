//! Identity and key management operations.
//!
//! Handles identity fingerprints, prekey refresh, and call encryption key derivation.
//! All operations are delegated to the SignalActor for thread-safety.

use libsignal_protocol::ProtocolAddress;
use crate::network::grpc::auth_client::AuthClient;
use crate::domain::ids::UserId;
use crate::error::{CryptoError, IdentityChangeSource, ServiceError};
use crate::storage::ChannelRepository;
use super::SignalService;

/// What a whole-account key republish did on the server.
#[derive(Debug, Clone, Copy)]
pub struct RepublishOutcome {
    /// The account's identity key changed, so every contact will be prompted
    /// about a changed safety number. False after a PIN restore, which
    /// recovers the ORIGINAL key.
    pub identity_key_changed: bool,
    /// How many stale device bundles the server dropped.
    pub devices_reset: u32,
}

impl SignalService {
    /// Get the local identity key fingerprint.
    pub async fn get_identity_fingerprint(&self) -> Result<String, ServiceError> {
        self.actor.get_identity_fingerprint().await
    }

    /// Get the local identity public key (33 bytes - 0x05 + 32-byte Curve25519).
    ///
    /// This is the Signal Protocol format identity key with the type byte prefix.
    pub async fn get_identity_public_key(&self) -> Result<[u8; 33], ServiceError> {
        self.actor.get_identity_public_key().await
    }

    /// Get the local identity private key (32 bytes X25519 scalar).
    ///
    /// This is used for ECIES decryption of NDSS'21 initial contact messages.
    pub async fn get_identity_private_key(&self) -> Result<[u8; 32], ServiceError> {
        self.actor.get_identity_private_key().await
    }

    /// Check if we have a trusted identity key for a peer.
    ///
    /// Returns `Some(key_bytes)` if trusted, `None` if unknown peer.
    pub fn get_peer_identity(&self, peer_id: &str) -> Result<Option<Vec<u8>>, crate::error::StorageError> {
        self.key_store.get_identity_by_uuid(peer_id)
    }

    /// Generate and upload new one-time prekeys (classical OPK + PQOPK in
    /// lockstep). Per Signal PQXDH §3.2 a PQOPK is consumed every time a
    /// classical OPK is, so the two pools must refill together — failing to
    /// upload PQOPKs would force the server to fall back to the medium-term
    /// last-resort PQSPK and weaken PQ forward secrecy.
    pub async fn refresh_prekeys(
        &self,
        auth_client: &mut AuthClient,
        count: u32,
    ) -> Result<u32, ServiceError> {
        let proto_prekeys = self.actor.refresh_prekeys(count).await?;
        let proto_kyber_otpks = self.actor.refresh_kyber_prekeys(count).await?;
        let uploaded_count = proto_prekeys.len() as u32;
        let kyber_count = proto_kyber_otpks.len() as u32;

        auth_client
            .refresh_pre_keys_with_kyber_otpk(proto_prekeys, proto_kyber_otpks)
            .await
            .map_err(ServiceError::Network)?;

        tracing::info!(opk = uploaded_count, pq_opk = kyber_count, "Refreshed prekeys via Actor");

        Ok(uploaded_count)
    }

    /// Rotate signed prekey + Kyber prekey and upload to server.
    ///
    /// Generates a new signed prekey and Kyber prekey (PQXDH pairing),
    /// uploads them via the RefreshPreKeysRequest, and prunes expired
    /// signed prekeys older than 30 days.
    pub async fn rotate_signed_prekey(
        &self,
        auth_client: &mut AuthClient,
    ) -> Result<(), ServiceError> {
        let (spk_proto, kpk_proto) = self.actor.rotate_signed_prekey().await?;

        auth_client
            .refresh_pre_keys_with_rotation(Vec::new(), Some(spk_proto), Some(kpk_proto), Vec::new())
            .await
            .map_err(ServiceError::Network)?;

        // Prune signed + Kyber prekeys older than 30 days (retain for in-flight
        // sessions). Both are rotated together as the PQXDH pair; both must be
        // pruned together to avoid unbounded Kyber key-store growth.
        use crate::crypto::signal::PreKeyManager;
        let prekey_manager = PreKeyManager::new(self.key_store.clone());
        let pruned_spk = prekey_manager.cleanup_expired_signed_prekeys(30)
            .map_err(ServiceError::Crypto)?;
        if pruned_spk > 0 {
            tracing::info!(count = pruned_spk, "Pruned expired signed prekeys");
        }
        let pruned_kpk = prekey_manager.cleanup_expired_kyber_prekeys(30)
            .map_err(ServiceError::Crypto)?;
        if pruned_kpk > 0 {
            tracing::info!(count = pruned_kpk, "Pruned expired Kyber prekeys");
        }

        tracing::info!("Rotated signed prekey + Kyber prekey (PQXDH pair)");
        Ok(())
    }

    /// This device's complete Signal key material, ready to publish: the
    /// bundle (identity key, fresh signed + Kyber prekeys, UAK) and both
    /// one-time pools. Shared by the whole-account republish and the
    /// per-device publish so the two can never build different bundles.
    async fn build_device_key_material(
        &self,
    ) -> Result<
        (
            crate::proto::chat_v1::SignalPreKeyBundle,
            Vec<crate::proto::chat_v1::signal_pre_key_bundle::PreKeyRecord>,
            Vec<crate::proto::chat_v1::signal_pre_key_bundle::KyberPreKeyRecord>,
        ),
        ServiceError,
    > {
        use crate::proto::chat_v1::SignalPreKeyBundle;
        use crate::storage::repositories::settings_repo::keys;

        let identity_key = self.actor.get_identity_public_key().await?;

        let registration_id = crate::crypto::signal::IdentityKeyManager::new(self.key_store.clone())
            .get_registration_id()
            .map_err(ServiceError::Crypto)?;

        // Fresh signed prekey + Kyber prekey, both signed by the identity key
        // that is going up in the same message.
        let (signed_prekey, kyber_prekey) = self.actor.rotate_signed_prekey().await?;

        // Both one-time pools, refilled in lockstep. PQXDH §3.2 has the server
        // pop a PQOPK for every classical OPK, so seeding only one leaves every
        // handshake falling back to the last-resort PQSPK.
        let one_time_prekeys = self.actor.refresh_prekeys(100).await?;
        let kyber_one_time_prekeys = self.actor.refresh_kyber_prekeys(100).await?;

        let device_id: u32 = self
            .settings_repo
            .get(keys::CURRENT_DEVICE_ID)
            .ok()
            .flatten()
            .unwrap_or(1);

        let bundle = SignalPreKeyBundle {
            identity_key: identity_key.to_vec(),
            registration_id,
            device_id,
            signed_prekey: Some(signed_prekey),
            // The bundle's own one-time prekey slot stays empty: the server
            // pops from the pool uploaded alongside it, and duplicating one
            // here would hand the same OPK out twice.
            one_time_prekey: None,
            kyber_prekey: Some(kyber_prekey),
            // Per NDSS '21, first contact goes through blind-signed anonymous
            // credentials rather than unrestricted sealed sender.
            unrestricted_unidentified_access: Some(false),
        };

        Ok((bundle, one_time_prekeys, kyber_one_time_prekeys))
    }

    /// Publish THIS device's bundle and one-time pools under its own device id,
    /// leaving every other device of the account untouched.
    ///
    /// Every login is issued a new device id by the server. A login that
    /// already holds the identity locally (Log Out, then Log In again) skips
    /// the SVR restore and so never reached `republish_identity_bundle`: the
    /// new device id had no bundle, peers kept encrypting to the previous
    /// (signed-out) device, and every incoming DM was silently lost. The
    /// identity key is unchanged here, so the per-device upload's
    /// same-identity check accepts it.
    pub async fn publish_device_bundle(
        &self,
        auth_client: &mut AuthClient,
    ) -> Result<(), ServiceError> {
        let (bundle, one_time_prekeys, kyber_one_time_prekeys) =
            self.build_device_key_material().await?;
        let device_id = bundle.device_id;
        auth_client
            .upload_pre_key_bundle(bundle)
            .await
            .map_err(ServiceError::Network)?;
        auth_client
            .refresh_pre_keys_with_kyber_otpk(one_time_prekeys, kyber_one_time_prekeys)
            .await
            .map_err(ServiceError::Network)?;
        tracing::info!(device_id, "published this device's Signal bundle");
        Ok(())
    }

    /// Replace the account's ENTIRE server-side Signal key material with this
    /// device's, in one atomic RPC.
    ///
    /// # Why this exists
    ///
    /// Two recovery situations leave the server holding key material nobody
    /// can use, and neither had any way to fix itself:
    ///
    ///   - **PIN restore.** The identity key comes back from the SVR backup,
    ///     but the private halves of every prekey the account ever published
    ///     died with the old device. Peers keep fetching those prekeys and
    ///     completing handshakes the new device cannot decrypt.
    ///   - **Identity reset.** No backup existed, so a NEW identity key is the
    ///     only way forward. `UploadPreKeyBundle` refuses a changed identity
    ///     key outright (the cross-device consistency check), and neither
    ///     `LoginRequest` nor `RefreshPreKeysRequest` carries one — so before
    ///     this method there was literally no wire on which to publish it.
    ///
    /// # Why one call
    ///
    /// The identity key and the prekeys signed by it are published together or
    /// not at all. Splitting them is precisely how accounts became permanently
    /// unreachable: the replenisher rotated the signed prekey through
    /// `RefreshPreKeys` while `identity_key` stayed at its old value, so the
    /// server served a signed prekey whose signature could never verify
    /// against the identity key it was served with. The server applies this in
    /// a single transaction for the same reason.
    ///
    /// Not a routine rotation — it changes every contact's safety number.
    /// Reached only from `ChatterClient::login`'s post-restore step and from
    /// the explicit `accept_new_identity`.
    pub async fn republish_identity_bundle(
        &self,
        auth_client: &mut AuthClient,
    ) -> Result<RepublishOutcome, ServiceError> {
        let (bundle, one_time_prekeys, kyber_one_time_prekeys) =
            self.build_device_key_material().await?;

        let response = auth_client
            .republish_identity_key(bundle, one_time_prekeys, kyber_one_time_prekeys)
            .await
            .map_err(ServiceError::Network)?;

        tracing::info!(
            identity_key_changed = response.identity_key_changed,
            devices_reset = response.devices_reset,
            "republished this device's Signal key material"
        );

        Ok(RepublishOutcome {
            identity_key_changed: response.identity_key_changed,
            devices_reset: response.devices_reset,
        })
    }

    /// Derive a shared secret for call encryption with a peer.
    pub async fn derive_call_encryption_key(
        &self,
        peer_address: &ProtocolAddress,
    ) -> Result<[u8; 32], ServiceError> {
        let peer_id = peer_address.name().parse().map_err(|_| ServiceError::Validation("Invalid peer ID".into()))?;
        let device_id = peer_address.device_id().into();
        
        self.actor.derive_call_encryption_key(peer_id, device_id).await
    }

    /// Generate a 60-digit safety number for a peer.
    ///
    /// Returns a numeric string that both parties should compare to verify
    /// they share the same identity keys and are not subject to MITM.
    pub async fn generate_safety_number(
        &self,
        local_user_id: &str,
        peer_user_id: &str,
    ) -> Result<String, ServiceError> {
        self.actor
            .generate_safety_number(local_user_id.to_string(), peer_user_id.to_string())
            .await
    }

    /// Generate scannable fingerprint bytes for QR code verification.
    pub async fn generate_scannable_fingerprint(
        &self,
        local_user_id: &str,
        peer_user_id: &str,
    ) -> Result<Vec<u8>, ServiceError> {
        self.actor
            .generate_scannable_fingerprint(local_user_id.to_string(), peer_user_id.to_string())
            .await
    }

    /// Check if we have previously verified a peer's safety number.
    pub fn is_peer_verified(&self, peer_user_id: &str) -> Result<bool, ServiceError> {
        self.key_store
            .is_peer_verified(peer_user_id)
            .map_err(ServiceError::Storage)
    }

    /// Mark a peer as verified (or unverified) with their current safety number.
    pub fn set_peer_verified(
        &self,
        peer_user_id: &str,
        safety_number: &str,
        verified: bool,
    ) -> Result<(), ServiceError> {
        self.key_store
            .set_peer_verified(peer_user_id, safety_number, verified)
            .map_err(ServiceError::Storage)
    }

    /// Park an identity change surfaced by a decrypt or a bundle fetch, so the
    /// SEND path can see it too.
    ///
    /// No-op for anything else, and for a change that arrived without a key (a
    /// Whisper message carries none — libsignal reads it from the stored
    /// session), because there would be nothing for the user to accept.
    pub(crate) fn note_identity_change(&self, err: &ServiceError) {
        let ServiceError::Crypto(CryptoError::UntrustedIdentity {
            peer_id,
            device_id,
            new_identity_key: Some(key),
            attribution,
        }) = err
        else {
            return;
        };
        if let Err(e) = self.key_store.record_pending_identity_change(
            peer_id,
            key,
            attribution.as_db_str(),
            *device_id,
        ) {
            tracing::warn!(peer = %peer_id, error = %e, "failed to park peer identity change");
        }
    }

    /// The peer's new, unaccepted identity key and how the peer was attributed.
    ///
    /// `None` when there is no pending change. The [`IdentityChangeSource`] is
    /// what lets the UI avoid presenting a server-asserted identity change as
    /// though it had been cryptographically attributed.
    pub fn pending_identity_change(
        &self,
        peer_user_id: &str,
    ) -> Result<Option<(Vec<u8>, IdentityChangeSource)>, ServiceError> {
        Ok(self
            .key_store
            .get_pending_identity_change(peer_user_id)
            .map_err(ServiceError::Storage)?
            .map(|(key, source, _)| (key, IdentityChangeSource::from_db_str(&source))))
    }

    /// Accept a peer's NEW identity key after a reinstall, and unwedge the
    /// conversation in both directions.
    ///
    /// **A trust decision — only ever reach this from an explicit user action.**
    /// A reinstall and an active MITM are indistinguishable from here.
    ///
    /// Mirrors this repo's existing response to a peer identity/device change
    /// (`spawn_notification_listener`'s PEER_DEVICES_CHANGED arm), which is two
    /// phases, not one: pairwise sessions are invalidated AND every shared
    /// channel is marked for sender-key rotation. Without the second phase the
    /// peer keeps holding a live sender key and group messages keep flowing to
    /// the identity that just changed.
    ///
    /// Ordering is deliberate. Rotation marks go first because they are
    /// idempotent and lazy (consumed by `encrypt_for_group` on the next send),
    /// while the pin is the step that must not be half-applied. So a crash
    /// leaves either "marked but still wedged" — which a re-run completes — or
    /// the fully accepted state. Doing the pin first would let a crash strand a
    /// re-run on the already-pinned no-op and skip rotation forever.
    ///
    /// The session delete lives inside the pin's transaction rather than going
    /// through `SignalActorHandle::invalidate_peer_sessions`, because pinning
    /// the new key while old sessions survive is the one hybrid that must be
    /// impossible: inbound would recover while outbound silently kept encrypting
    /// under the dead ratchet. Atomicity is only available inside `KeyStore`,
    /// which owns the writer connection.
    ///
    /// Returns the number of sessions dropped; `0` when the key was already
    /// pinned, so re-running cannot clear a verification the user has re-done.
    pub fn accept_peer_identity_change(
        &self,
        peer_user_id: &UserId,
        new_identity_key: &[u8],
        channel_repo: &ChannelRepository,
    ) -> Result<u32, ServiceError> {
        let peer = peer_user_id.to_string();

        // Reject anything that is not a well-formed Signal identity key BEFORE
        // it is pinned. A malformed key fails to parse in `is_trusted_identity`
        // and turns every later message from the peer into a decode error — a
        // worse wedge than the one being cleared.
        libsignal_protocol::IdentityKey::decode(new_identity_key).map_err(|e| {
            ServiceError::Validation(format!("not a Signal identity key: {e}"))
        })?;

        // The user may only accept the key they were actually shown. If a change
        // is parked, this must be it — otherwise a server that keeps injecting
        // fresh keys could swap one in behind the prompt.
        if let Some((parked, _, _)) = self
            .key_store
            .get_pending_identity_change(&peer)
            .map_err(ServiceError::Storage)?
            && parked != new_identity_key
        {
            return Err(ServiceError::Validation(
                "identity key does not match the pending change for this peer".into(),
            ));
        }

        // Phase 2 (see the ordering note above).
        let channels = channel_repo
            .list_channels_for_user(peer_user_id)
            .map_err(ServiceError::Storage)?;
        for channel_id in &channels {
            self.mark_channel_for_rotation(channel_id)?;
        }

        // Phase 1, atomically: pin the new key, drop the stale verification,
        // delete every session, clear the parked change.
        let dropped = self
            .key_store
            .accept_peer_identity_change(&peer, new_identity_key)
            .map_err(ServiceError::Storage)?;

        tracing::warn!(
            peer = %peer,
            sessions_dropped = dropped,
            channels_marked = channels.len(),
            "ACCEPTED a peer identity key change: new key pinned, verification cleared, \
             sessions dropped, shared channels marked for sender-key rotation"
        );

        Ok(dropped)
    }

    /// Derive a call key for a specific call with a peer.
    pub async fn derive_call_key(
        &self,
        peer_address: &ProtocolAddress,
        call_id: &str,
    ) -> Result<[u8; 32], ServiceError> {
        use crate::crypto::call_key_provider;

        let shared_secret = self.derive_call_encryption_key(peer_address).await?;
        let call_key = call_key_provider::derive_call_key(&shared_secret, call_id)
            .map_err(ServiceError::Crypto)?;

        Ok(call_key)
    }
}
