/// Maximum number of encrypt locks to keep in cache.
const MAX_ENCRYPT_LOCKS: usize = 500;

use super::*;

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
        group_id: Option<&[u8]>,
    ) -> Result<(HashMap<u32, DeviceCiphertext>, bool), ServiceError> {
        let (devices, unrestricted) = self
            .ensure_sessions_and_access_policy_all_devices(
                auth_client,
                recipient_id,
                need_access_policy,
            )
            .await?;
        let ciphertexts = self
            .encrypt_prepared_devices_bounded(
                auth_client,
                recipient_id,
                &devices,
                plaintext,
                group_id,
                false,
            )
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
        group_id: Option<&[u8]>,
    ) -> Result<HashMap<u32, DeviceCiphertext>, ServiceError> {
        self.encrypt_prepared_devices_bounded(
            auth_client,
            recipient_id,
            devices,
            plaintext,
            group_id,
            true,
        )
        .await
    }

    /// Fresh discovery already checked usable sessions. Exact-recipient retries
    /// additionally establish any missing sessions; both paths still validate
    /// authoritative state and trust inside the actor before batch commit.
    async fn encrypt_prepared_devices_bounded(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        devices: &[u32],
        plaintext: &[u8],
        group_id: Option<&[u8]>,
        ensure_missing: bool,
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
            let id = DeviceId::try_from(device)
                .map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
            destinations.push(ProtocolAddress::new(recipient_id.to_string(), id));
        }
        self.refuse_pending_identity_change(recipient_id)?;
        if ensure_missing {
            let usable = self.session_devices(recipient_id).await?;
            for destination in &destinations {
                let device = u32::from(destination.device_id());
                if !usable.contains(&device) {
                    self.ensure_session(auth_client, recipient_id, device)
                        .await?;
                }
            }
        }
        let certificate = self.get_or_request_sender_certificate(auth_client).await?;
        let batch = self
            .actor
            .encrypt_sealed_bounded_batch(
                plaintext.to_vec(),
                certificate,
                destinations,
                group_id.map(<[u8]>::to_vec),
            )
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

        // Same send-side identity-change wedge as `ensure_session`: never
        // encrypt under a stale ratchet the peer's new install cannot open.
        self.refuse_pending_identity_change(recipient_id)?;

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

/// One device's sealed ciphertext and the registration id of the session
/// that encrypted it, from [`SignalService::encrypt_message_all_devices_with_access_policy`].
pub struct DeviceCiphertext {
    pub ciphertext: Vec<u8>,
    pub registration_id: u32,
}
