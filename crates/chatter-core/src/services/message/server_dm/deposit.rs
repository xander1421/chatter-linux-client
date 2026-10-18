use super::*;

impl MessageService {
    /// Encrypt `plaintext` for every device of `recipient_id` and deposit
    /// all the ciphertexts in ONE `SendSealedMessage` (`SealedDelivery::
    /// AllDevices`), UAK-authorized: the server queues every device's copy or
    /// none. To our own user this device is left out; when it is our only
    /// device the server answers the empty send with nothing to deliver and
    /// the outcome is `delivered: 0` with no error.
    ///
    /// Returns how many devices were delivered to, and how many of those were
    /// delivered in the DEGRADED identified-sender mode — see [`DepositOutcome`].
    ///
    /// Shared by 1:1 DMs and by channel-member traffic (SKDMs, sender-key
    /// requests, control fanout — see `group.rs` / `core.rs`): both go to
    /// recipients who already know who we are, so they spend no anonymous
    /// mailbox token (OPEN-ITEMS §120).
    ///
    /// Signal's device-list reconciliation (Signal-Desktop
    /// `OutgoingMessage.doSendMessage`, 409 / 410): the request names the
    /// whole device set with registration ids, the server answers a set that
    /// is not the recipient's registered devices with the difference and
    /// queues nothing, and we then fix our sessions
    /// (`SignalService::reconcile_devices`) and send again, once, under the
    /// same message GUID. A second mismatch is a failed send. A device we
    /// cannot open a session with fails the send to this recipient, as in
    /// Signal-Desktop (`registerError` for the service id) — a typed error,
    /// never a silent partial delivery.
    pub(in crate::services::message) async fn deposit_sealed_to_all_devices(
        &self,
        auth_client: &mut AuthClient,
        message_client: &mut MessageClient,
        recipient_id: &str,
        plaintext: &[u8],
        group_id: Option<&[u8]>,
    ) -> DepositOutcome {
        let client_request_id =
            super::dm_outbox::request_id().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let mismatch = match self
            .deposit_sealed_to_session_devices(
                auth_client,
                message_client,
                recipient_id,
                plaintext,
                group_id,
                &client_request_id,
            )
            .await
        {
            Ok(outcome) => return outcome,
            Err(mismatch) => mismatch,
        };
        tracing::info!(
            recipient = recipient_id,
            %mismatch,
            "Sealed send: recipient's devices changed — reconciling sessions and retrying once"
        );
        let crate::error::NetworkError::MismatchedDevices {
            missing,
            extra,
            stale,
        } = &mismatch
        else {
            return DepositOutcome {
                error: Some(MessageError::Network(mismatch)),
                ..Default::default()
            };
        };
        if let Err(e) = self
            .signal_service
            .reconcile_devices(auth_client, recipient_id, missing, extra, stale)
            .await
        {
            let rate_limited = matches!(
                e,
                crate::error::service_error::ServiceError::Network(
                    crate::error::NetworkError::RateLimited
                )
            );
            tracing::warn!(
                recipient = recipient_id,
                error = %e,
                rate_limited,
                "Sealed send: could not reconcile the recipient's devices"
            );
            return DepositOutcome {
                error: Some(MessageError::from_signal_send(e, "device reconciliation")),
                ..Default::default()
            };
        }
        match self
            .deposit_sealed_to_session_devices(
                auth_client,
                message_client,
                recipient_id,
                plaintext,
                group_id,
                &client_request_id,
            )
            .await
        {
            Ok(outcome) => outcome,
            Err(mismatch) => {
                tracing::warn!(
                    recipient = recipient_id,
                    %mismatch,
                    "Sealed send: devices still mismatched after reconciling — not sent"
                );
                DepositOutcome {
                    error: Some(MessageError::Network(mismatch)),
                    ..Default::default()
                }
            }
        }
    }

    /// One attempt of [`Self::deposit_sealed_to_all_devices`]. `Err` is the
    /// server's `NetworkError::MismatchedDevices`: nothing was queued.
    async fn deposit_sealed_to_session_devices(
        &self,
        auth_client: &mut AuthClient,
        message_client: &mut MessageClient,
        recipient_id: &str,
        plaintext: &[u8],
        group_id: Option<&[u8]>,
        client_request_id: &str,
    ) -> Result<DepositOutcome, crate::error::NetworkError> {
        // To our own user (self-sync, SKDM, SenderKeyRequest) the deposit is
        // identified on the authenticated channel, never UAK on the anonymous
        // one — see `sync_to_own_devices`. That is not a degradation.
        let is_self = self
            .current_user_id()
            .is_ok_and(|me| me.to_string() == recipient_id);
        let has_profile_key = recipient_id
            .parse::<UserId>()
            .ok()
            .and_then(|peer| {
                let me = self.settings_repo.current_user_id().ok()?;
                self.user_repo
                    .get_friend_profile_key(&me, &peer)
                    .ok()
                    .flatten()
            })
            .is_some();

        let (sealed, unrestricted) = match self
            .signal_service
            .encrypt_message_all_devices_bounded_with_access_policy(
                auth_client,
                recipient_id,
                plaintext,
                !is_self && !has_profile_key,
                group_id,
            )
            .await
        {
            Ok(c) => c,
            Err(e) => {
                let rate_limited = matches!(
                    e,
                    crate::error::service_error::ServiceError::Network(
                        crate::error::NetworkError::RateLimited
                    )
                );
                tracing::warn!(
                    recipient = recipient_id,
                    error = %e,
                    rate_limited,
                    "Sealed DM: failed to encrypt for recipient's devices"
                );
                return Ok(DepositOutcome {
                    error: Some(MessageError::from_signal_send(e, "device encryption")),
                    ..Default::default()
                });
            }
        };
        if let (Ok(peer), Ok(content)) = (
            recipient_id.parse::<UserId>(),
            crate::proto::content_v1::Content::decode(plaintext),
        ) {
            self.snapshot_dm_content_targets(
                peer,
                &content,
                &sealed.keys().copied().collect::<Vec<_>>(),
            )
            .map_err(|error| {
                crate::error::NetworkError::ServerError(format!("persist receipt targets: {error}"))
            })?;
        }
        // Each device under the registration id of the session that
        // encrypted to it — never 0, which the server reads as stale.
        let registration_ids: std::collections::HashMap<u32, u32> = sealed
            .iter()
            .map(|(&device_id, s)| (device_id, s.registration_id))
            .collect();
        let timestamp = crate::util::time::TimestampNanos::now_anonymized().as_u64();
        let server_key_id = self.signal_service.get_server_key_id();
        let mut messages: Vec<SealedDeviceMessage> = sealed
            .into_iter()
            .map(|(device_id, s)| SealedDeviceMessage {
                device_id,
                registration_id: s.registration_id,
                sealed_message: Some(SealedSenderMessage {
                    sealed_body: s.ciphertext,
                    server_key_id,
                    timestamp,
                }),
            })
            .collect();
        messages.sort_by_key(|m| m.device_id);

        let recipient_uak = if is_self {
            None
        } else {
            self.sealed_access_key(recipient_id, unrestricted)
        };

        // DEGRADATION, not a normal path: we hold no profile key for this
        // recipient and they do not accept sealed sender from anyone. We can
        // still deliver — but only over Basic auth, which makes the server
        // stamp our user id into `device_messages.source_user_id` and defeats
        // sealed sender entirely. Logged loudly and per-recipient on purpose:
        // a silent fallback here would report "sealed DM sent" while shipping
        // the exact opposite.
        if recipient_uak.is_none() && !is_self {
            tracing::warn!(
                recipient = recipient_id,
                reason = "no profile key held for the recipient to derive their UAK",
                "SEALED SENDER DEGRADED: falling back to identified (Basic auth) \
                 delivery — the server WILL record us as the sender of this message"
            );
        }

        let (response, uak_rejected) = match self
            .send_sealed_batch(
                message_client,
                recipient_id,
                messages,
                SealedDelivery::AllDevices,
                recipient_uak.as_ref(),
                client_request_id,
            )
            .await
        {
            Ok(r) => r,
            Err(e @ crate::error::NetworkError::MismatchedDevices { .. }) => return Err(e),
            Err(e) => {
                tracing::warn!(recipient = recipient_id, error = %e, "Sealed send: deposit failed, no device reached");
                return Ok(DepositOutcome {
                    error: Some(MessageError::Network(e)),
                    ..Default::default()
                });
            }
        };
        let mut outcome = DepositOutcome::delivered(&response);
        if !is_self && (recipient_uak.is_none() || uak_rejected) {
            outcome.degraded = outcome.delivered;
        }
        outcome.registration_ids = registration_ids;
        Ok(outcome)
    }

    /// The `Unidentified-Access-Key` to seal a deposit to `recipient_id` with,
    /// as Signal's `UnidentifiedAccessUtil` picks it: derived from the profile
    /// key they shared with us; else, when they accept sealed sender from
    /// anyone, random bytes the server does not check; else `None` (deliver
    /// identified). The server never serves a UAK — holding the profile key is
    /// what authorizes a sealed sender.
    pub(in crate::services::message) fn sealed_access_key(
        &self,
        recipient_id: &str,
        unrestricted: bool,
    ) -> Option<[u8; 16]> {
        let profile_key = recipient_id.parse::<UserId>().ok().and_then(|peer| {
            let me = self.settings_repo.current_user_id().ok()?;
            self.user_repo
                .get_friend_profile_key(&me, &peer)
                .ok()
                .flatten()
        });
        pick_sealed_access_key(profile_key.as_ref(), unrestricted)
    }

    /// Deposit ciphertexts the caller encrypted for devices it chose itself
    /// (a resend to a message's original devices, a NullMessage after a
    /// reset, own-device sync in Shield) in ONE `SendSealedMessage`
    /// (`SealedDelivery::NamedDevices`): UAK-authorized on the anonymous
    /// channel with `uak`, identified on `message_client` without — or when
    /// the server rejects the UAK (our copy of the recipient's profile key is
    /// stale: they rotated it). The server checks only that each named device
    /// is registered; one that is not (retired by Log Out / unlink) gets its
    /// session archived and the rest are sent again, once.
    pub(in crate::services::message) async fn deposit_sealed_ciphertexts(
        &self,
        message_client: &mut MessageClient,
        recipient_id: &str,
        ciphertexts: std::collections::HashMap<u32, Vec<u8>>,
        uak: Option<&[u8; 16]>,
    ) -> DepositOutcome {
        let timestamp = crate::util::time::TimestampNanos::now_anonymized().as_u64();
        let server_key_id = self.signal_service.get_server_key_id();
        let mut messages: Vec<SealedDeviceMessage> = ciphertexts
            .into_iter()
            .map(|(device_id, sealed_body)| SealedDeviceMessage {
                device_id,
                registration_id: 0,
                sealed_message: Some(SealedSenderMessage {
                    sealed_body,
                    server_key_id,
                    timestamp,
                }),
            })
            .collect();
        messages.sort_by_key(|m| m.device_id);
        let client_request_id =
            super::dm_outbox::request_id().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let mut error = None;
        for _attempt in 0..2 {
            if messages.is_empty() {
                break;
            }
            match self
                .send_sealed_batch(
                    message_client,
                    recipient_id,
                    messages.clone(),
                    SealedDelivery::NamedDevices,
                    uak,
                    &client_request_id,
                )
                .await
            {
                Ok((response, uak_rejected)) => {
                    let mut outcome = DepositOutcome::delivered(&response);
                    if uak_rejected {
                        outcome.degraded = outcome.delivered;
                    }
                    outcome.error = error;
                    return outcome;
                }
                // The server no longer knows these devices (retired by Log
                // Out / unlink). Signal's 410 handling: archive the session
                // so we stop encrypting to it, and send to the rest.
                Err(e) if e.is_unknown_destination_device() => {
                    let crate::error::NetworkError::MismatchedDevices { extra, .. } = &e else {
                        unreachable!()
                    };
                    for &device_id in extra {
                        tracing::info!(
                            recipient = recipient_id,
                            device_id,
                            "Sealed send: device retired on server — archiving its session"
                        );
                        if let Err(e) = self
                            .signal_service
                            .actor
                            .archive_session_for_resend(recipient_id.to_string(), device_id, None)
                            .await
                        {
                            tracing::warn!(
                                recipient = recipient_id,
                                device_id,
                                error = %e,
                                "Failed to archive session for retired device"
                            );
                        }
                    }
                    messages.retain(|m| !extra.contains(&m.device_id));
                    record_deposit_error(&mut error, MessageError::Network(e));
                }
                Err(e) => {
                    tracing::warn!(
                        recipient = recipient_id,
                        error = %e,
                        "Sealed send: deposit to the named devices failed"
                    );
                    record_deposit_error(&mut error, MessageError::Network(e));
                    break;
                }
            }
        }
        DepositOutcome {
            error,
            ..Default::default()
        }
    }

    /// One `SendSealedMessage`: with `uak` on the anonymous channel (no
    /// `authorization` header — `message_client` is interceptor-wrapped and
    /// would attach Basic auth next to the UAK, which the server rejects as
    /// multiple auth methods), identified on `message_client` without one,
    /// or when the server rejects the UAK. The `bool` is that rejection: the
    /// send went identified although we meant to seal it.
    ///
    /// Friend DMs are sealed even though the server already stores the
    /// friendship: a static friendship graph is not a behavioural delivery
    /// log. Knowing "A and B are friends" is a fact B chose to publish to the
    /// server; knowing "A sent B a message at 14:03, then three more by
    /// 14:05" is a timing-and-frequency record neither of them agreed to.
    /// Signal seals to known contacts for exactly this reason, and so do we.
    pub(in crate::services::message) async fn send_sealed_batch(
        &self,
        message_client: &mut MessageClient,
        recipient_id: &str,
        messages: Vec<SealedDeviceMessage>,
        delivery: SealedDelivery,
        uak: Option<&[u8; 16]>,
        client_request_id: &str,
    ) -> Result<(SendSealedMessageResponse, bool), crate::error::NetworkError> {
        crate::services::gateway::submission::check_send_session()
            .map_err(|error| crate::error::NetworkError::InvalidRequest(error.to_string()))?;
        // A queued DM stores these exact envelopes before the first byte goes.
        super::dm_outbox::retain_request(recipient_id, &messages, delivery, uak, client_request_id)
            .map_err(|error| crate::error::NetworkError::ServerError(error.to_string()))?;
        // Shield turned on during a queued DM's attempt: this transport names
        // the recipient, so the stored envelope waits instead.
        let shield_fence = || {
            if super::dm_outbox::request_id().is_some()
                && self.shield_mode.load(std::sync::atomic::Ordering::Acquire)
            {
                return Err(crate::error::NetworkError::Unavailable);
            }
            Ok(())
        };
        shield_fence()?;
        let mut uak_rejected = false;
        if let Some(uak) = uak {
            match self
                .grpc_pool
                .anonymous_message_client()
                .send_sealed_messages_with_uak(
                    recipient_id,
                    messages.clone(),
                    delivery,
                    client_request_id,
                    uak,
                )
                .await
            {
                Err(e) if e.is_auth_error() => {
                    tracing::warn!(
                        recipient = recipient_id,
                        reason = "UAK from our stored profile key rejected",
                        "SEALED SENDER DEGRADED: delivering identified (Basic auth)"
                    );
                    uak_rejected = true;
                }
                other => return other.map(|response| (response, false)),
            }
        }
        crate::services::gateway::submission::check_send_session()
            .map_err(|error| crate::error::NetworkError::InvalidRequest(error.to_string()))?;
        shield_fence()?;
        message_client
            .send_sealed_messages(recipient_id, messages, delivery, client_request_id)
            .await
            .map(|response| (response, uak_rejected))
    }
}

// If any destination failed transiently, a retry can still make progress.
pub(in crate::services::message) fn record_deposit_error(
    current: &mut Option<MessageError>,
    error: MessageError,
) {
    if current.is_none() || error.permanent_send_failure_reason().is_none() {
        *current = Some(error);
    }
}

/// See [`MessageService::sealed_access_key`].
pub(super) fn pick_sealed_access_key(
    profile_key: Option<&[u8; 32]>,
    unrestricted: bool,
) -> Option<[u8; 16]> {
    match profile_key {
        Some(pk) => Some(crate::crypto::derive_uak(pk)),
        None if unrestricted => {
            let mut key = [0u8; 16];
            rand::RngCore::fill_bytes(&mut rand::rngs::OsRng.unwrap_err(), &mut key);
            Some(key)
        }
        None => None,
    }
}

/// Result of depositing one sealed DM to a recipient's devices.
///
/// `degraded` is tracked separately from `delivered` so a caller can never read
/// "it worked" without also being able to see that the sender was exposed to the
/// server on some of those deliveries.
#[derive(Debug, Default)]
pub(in crate::services::message) struct DepositOutcome {
    /// Devices the message actually reached.
    pub(in crate::services::message) delivered: usize,
    /// Preserve the typed failure when no device received the message.
    pub(in crate::services::message) error: Option<MessageError>,
    /// Of those, how many were sent identified (Basic auth) because no UAK was
    /// available — i.e. sealed sender did NOT engage and the server recorded us.
    pub(in crate::services::message) degraded: usize,
    /// `(device, server timestamp in epoch nanos)` of each deposit: the
    /// timestamp that device's inbox hands it the envelope under.
    pub(in crate::services::message) deposits: Vec<(u32, u64)>,
    /// Registration id of the session each device was encrypted with, for
    /// a deposit that named its device set
    /// ([`MessageService::deposit_sealed_to_all_devices`]); empty otherwise.
    pub(in crate::services::message) registration_ids: std::collections::HashMap<u32, u32>,
}

impl DepositOutcome {
    /// The devices a successful `SendSealedMessage` queued for, each under
    /// the response's server timestamp.
    pub(super) fn delivered(response: &SendSealedMessageResponse) -> Self {
        DepositOutcome {
            delivered: response.delivered_to_devices.len(),
            deposits: response
                .delivered_to_devices
                .iter()
                .map(|&device_id| (device_id, response.server_timestamp))
                .collect(),
            ..Default::default()
        }
    }
}
