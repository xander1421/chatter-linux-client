use super::*;

impl SignalService {
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
    pub(super) fn refuse_pending_identity_change(
        &self,
        recipient_id: &str,
    ) -> Result<(), ServiceError> {
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
}
