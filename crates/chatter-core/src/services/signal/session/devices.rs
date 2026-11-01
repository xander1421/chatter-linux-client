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
    pub(super) fn unrestricted_access(&self, recipient_id: &str) -> Option<bool> {
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
    mut bundles: Vec<crate::proto::chat_v1::SignalPreKeyBundle>,
    skip: Option<u32>,
) -> Vec<crate::proto::chat_v1::SignalPreKeyBundle> {
    bundles.retain(|b| Some(b.device_id) != skip);
    bundles
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
