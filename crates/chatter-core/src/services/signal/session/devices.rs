use super::*;

impl SignalService {
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
        self.ensure_device_sessions(
            &mut PreKeySource::Identified(auth_client), recipient_id, need_access_policy, false,
        ).await
    }

    /// Reuse admitted sessions for a private-channel send. The anonymous
    /// submit reports missing or stale devices for reconciliation.
    pub(crate) async fn require_group_device_sessions(
        &self,
        source: &mut PreKeySource<'_>,
        recipient_id: &str,
    ) -> Result<Vec<u32>, ServiceError> {
        if let Some(key) = self.get_peer_identity(recipient_id)? {
            self.require_private_post_key(recipient_id, &key)?;
        }
        self.ensure_device_sessions(source, recipient_id, false, true)
            .await.map(|(devices, _)| devices)
    }

    pub(crate) async fn ensure_anonymous_device_sessions(
        &self,
        source: &mut PreKeySource<'_>,
        recipient_id: &str,
    ) -> Result<Vec<u32>, ServiceError> {
        self.ensure_device_sessions(source, recipient_id, false, false)
            .await.map(|(devices, _)| devices)
    }

    async fn ensure_device_sessions(
        &self,
        source: &mut PreKeySource<'_>,
        recipient_id: &str,
        need_access_policy: bool,
        strict_discovery: bool,
    ) -> Result<(Vec<u32>, bool), ServiceError> {
        use chatter_proto::chat_v1::get_pre_key_bundle_response;

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

        let refresh_echo = strict_discovery && this_device.is_some()
            && self.private_echo_registration()?.is_none();
        if strict_discovery && !refresh_echo && let Some(device) = this_device {
            return Ok((vec![device], known_policy.unwrap_or(false)));
        }

        match self.require_bundle_lookup_ready(recipient_id) {
            Err(ServiceError::Crypto(CryptoError::KeyVerificationPending { .. }))
                if !with_session.is_empty() =>
            {
                return Ok((with_session, known_policy.unwrap_or(false)));
            }
            result => result?,
        }

        // Fetch prekey bundles for ALL devices using wildcard
        let response = match source.fetch(recipient_id, "*").await {
            Ok(r) => r,
            // A throttled policy read can reuse the established sessions.
            // The caller applies its credential policy to the resulting send.
            Err(crate::error::NetworkError::RateLimited)
                if !with_session.is_empty() => {
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

                let bundles = without_device(device_list.bundles, this_device.filter(|_| !refresh_echo));
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

                    if refresh_echo && this_device == Some(device_id) {
                        self.restore_private_echo(address, Self::convert_prekey_bundle(proto_bundle)?).await?;
                        established_device_ids.push(device_id);
                        continue;
                    }
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
                            let result = if strict_discovery {
                                self.establish_private_post_device(address, bundle).await
                            } else {
                                self.actor.establish_session(address, bundle).await
                            };
                            match result {
                                Ok(_) => established_device_ids.push(device_id),
                                Err(e) => {
                                    tracing::warn!(
                                        recipient = %recipient_id,
                                        device_id,
                                        error = %e,
                                        "Failed to establish session with device"
                                    );
                                    if strict_discovery {
                                        return Err(e);
                                    }
                                    if last_error.is_none() || matches!(e, ServiceError::Crypto(CryptoError::KeyVerificationPending { .. })) {
                                        last_error = Some(e);
                                    }
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
                            if strict_discovery {
                                return Err(e);
                            }
                            if last_error.is_none() {
                                last_error = Some(e);
                            }
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
                    if refresh_echo {
                        self.restore_private_echo(self.get_our_address()?, Self::convert_prekey_bundle(single_bundle)?).await?;
                        return Ok((vec![device_id], unrestricted));
                    }
                    return Ok((Vec::new(), unrestricted));
                }
                let id = DeviceId::try_from(device_id)
                    .map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
                let address = ProtocolAddress::new(recipient_id.to_owned(), id);
                if !self.actor.has_session(address.clone()).await? {
                    let bundle = Self::convert_prekey_bundle(single_bundle)?;
                    if strict_discovery {
                        self.establish_private_post_device(address, bundle).await?;
                    } else {
                        self.actor.establish_session(address, bundle).await
                            .inspect_err(|error| self.note_identity_change(error))?;
                    }
                }
                Ok((vec![device_id], unrestricted))
            }
        }
    }

    /// The recipient's `unrestricted_unidentified_access` flag from the last
    /// bundle fetch this run, if any.
    pub(crate) fn unrestricted_access(&self, recipient_id: &str) -> Option<bool> {
        self.unrestricted_access
            .lock()
            .ok()?
            .get(recipient_id)
            .copied()
    }

    pub(super) fn record_unrestricted_access(&self, recipient_id: &str, unrestricted: bool) {
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
        self.reconcile_devices_with_source(
            &mut PreKeySource::Identified(auth_client), recipient_id, missing, extra, stale,
        ).await
    }

    pub(crate) async fn reconcile_devices_with_source(
        &self,
        source: &mut PreKeySource<'_>,
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
            self.ensure_session_with_source(source, recipient_id, device_id)
                .await?;
        }
        Ok(())
    }

    /// Keep the echo's registration and archive state for this linked
    /// device, independently of a particular post or channel.
    pub(crate) fn private_echo_registration(&self) -> Result<Option<u32>, ServiceError> {
        let (key, registration) = self.private_echo_key()?;
        Ok(self.settings_repo.get::<Option<u32>>(&key)?.unwrap_or(Some(registration)))
    }

    fn private_echo_key(&self) -> Result<(String, u32), ServiceError> {
        let address = self.get_our_address()?;
        let (registration, _) = self.key_store.get_local_identity()?.ok_or(CryptoError::NoIdentity)?;
        Ok((format!("signal.private-post-echo:{address}:{registration}"), registration))
    }

    fn require_private_post_key(&self, peer: &str, key: &[u8]) -> Result<(), ServiceError> {
        let route = self.actor.kt_route();
        let mut store = crate::crypto::SignalStore::new(self.key_store.clone());
        store.set_kt_route(route);
        if route == crate::storage::peer_verification::KtRoute::AnonymousTor {
            // In Shield, run a cold lookup's KT proof on its separate
            // connection after a jittered delay.
            let delay = crate::app::core_services::jittered_delay().as_secs() as i64;
            store.defer_outbound_kt_until(chrono::Utc::now().timestamp() + delay);
        }
        Ok(store.require_sendable_key(peer, key)?)
    }

    async fn establish_private_post_device(
        &self,
        address: ProtocolAddress,
        bundle: libsignal_protocol::PreKeyBundle,
    ) -> Result<(), ServiceError> {
        let peer = address.name();
        let key = bundle.identity_key().map_err(|_| CryptoError::InvalidKeyFormat)?.serialize();
        let pinned = if peer == self.get_our_address()?.name() {
            let (_, local) = self.key_store.get_local_identity()?.ok_or(CryptoError::NoIdentity)?;
            let local = libsignal_protocol::IdentityKeyPair::try_from(local.as_slice())
                .map_err(|_| CryptoError::InvalidKeyFormat)?;
            Some(local.identity_key().serialize().to_vec())
        } else {
            self.get_peer_identity(peer)?
        };
        // Match the echo: a bundle contradicting a pinned identity never
        // starts KT or opens a key-change workflow from the repair path.
        if pinned.as_deref().is_some_and(|pinned| pinned != key.as_ref()) {
            return Err(CryptoError::InvalidKeyFormat.into());
        }
        self.require_private_post_key(peer, &key)?;
        self.actor.establish_session(address, bundle).await
    }

    async fn restore_private_echo(
        &self,
        address: ProtocolAddress,
        bundle: libsignal_protocol::PreKeyBundle,
    ) -> Result<(), ServiceError> {
        let (key, _) = self.private_echo_key()?;
        let registration = self.actor.validate_private_echo_bundle(address, bundle).await?;
        self.settings_repo.set(&key, &Some(registration))?;
        Ok(())
    }

    /// Archive and refresh the posting device in the same steps as a peer.
    /// Its registration is durable, and the real identity and keys stay local.
    pub(crate) async fn reconcile_private_post_devices(
        &self,
        source: &mut PreKeySource<'_>,
        recipient: &str,
        devices: &chatter_proto::chat_v1::MismatchedDevices,
    ) -> Result<(), ServiceError> {
        let local = self.get_our_address()?;
        let is_echo = |device| recipient == local.name() && device == u32::from(local.device_id());
        // Each device has one disposition. In particular, repeated hints
        // must not produce multiple echo lookups after a peer's session is ready.
        let mut seen = std::collections::HashSet::new();
        for &device in devices.extra_devices.iter().chain(&devices.stale_devices).chain(&devices.missing_devices) {
            if DeviceId::try_from(device).is_err() || !seen.insert(device) {
                return Err(crate::error::NetworkError::InvalidRequest("invalid private device mismatch".into()).into());
            }
        }
        for &device in devices.extra_devices.iter().chain(&devices.stale_devices).chain(&devices.missing_devices) {
            if is_echo(device) {
                let (key, _) = self.private_echo_key()?;
                self.settings_repo.set(&key, &Option::<u32>::None)?;
            } else {
                self.actor.archive_session_for_resend(recipient.to_owned(), device, None).await?;
            }
        }
        for &device in devices.missing_devices.iter().chain(&devices.stale_devices) {
            let bundle = Self::fetch_device_bundle(source, recipient, device).await?;
            if is_echo(device) {
                self.restore_private_echo(local.clone(), bundle).await?;
            } else {
                let address = ProtocolAddress::new(
                    recipient.to_owned(), DeviceId::try_from(device)
                        .map_err(|_| ServiceError::Validation("Invalid device ID".into()))?,
                );
                self.establish_private_post_device(address, bundle).await?;
            }
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
    /// Devices of `recipient_id` we hold a usable session with, ascending.
    /// Local only — no RPC. The session store doubles as our cache of a
    /// peer's device list: the normal-mode all-devices bundle fetch
    /// (`ensure_sessions_and_access_policy_all_devices`) and the per-device
    /// fetches of `reconcile_devices` leave a session per current device,
    /// which is what Shield may use without naming the peer.
    pub async fn session_devices(&self, recipient_id: &str) -> Result<Vec<u32>, ServiceError> {
        Ok(self
            .session_device_registrations(recipient_id)
            .await?
            .into_iter()
            .map(|(id, _)| id)
            .collect())
    }

    /// One actor turn reads and validates each stored session once. This is a
    /// discovery snapshot; encryption still loads authoritative ratchet state.
    pub(crate) async fn session_device_registrations(
        &self,
        recipient_id: &str,
    ) -> Result<Vec<(u32, Option<u32>)>, ServiceError> {
        self.actor.session_devices(recipient_id.to_owned()).await
    }
}

/// The fan-out targets minus `skip` (self-sync skips the sending device), so a
/// skipped device never gets a session built or a ratchet step taken for it.
fn without_device(
    mut bundles: Vec<chatter_proto::chat_v1::SignalPreKeyBundle>,
    skip: Option<u32>,
) -> Vec<chatter_proto::chat_v1::SignalPreKeyBundle> {
    bundles.retain(|b| Some(b.device_id) != skip);
    bundles
}

#[cfg(test)]
mod tests {
    use super::without_device;
    use chatter_proto::chat_v1::SignalPreKeyBundle;

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
