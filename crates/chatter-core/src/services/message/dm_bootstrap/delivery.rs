use super::*;

impl MessageService {
    /// Seal `payload` (bootstrap MESSAGE or REQUEST) to EVERY device in
    /// `devices`, each under its own session, and deposit them all in ONE
    /// `SendSealedMessage` under one pre-fetched anonymous token: queued for
    /// every device or none. Returns the devices it reached.
    ///
    /// Every device, not one: each device that answers keeps its own half of
    /// the conversation (`DmConversationManager::record_peer_device`), so an
    /// offline device answering hours later cannot take an online one's
    /// mailbox over, and picking one device for a multi-device peer would
    /// leave the message waiting on whichever was picked.
    ///
    /// The server's answer is handled once, then the send is retried with a
    /// fresh token: a device it reports unregistered (`extra`) is archived,
    /// marked retired and dropped from the conversation; with `AllDevices`,
    /// missing and stale devices get fresh sessions (`reconcile_devices`)
    /// and the payload is sealed again for the live set.
    ///
    /// The token comes from the pre-fetched batch only — requesting one here
    /// would tie the authenticated request to this anonymous send in time
    /// (NDSS'21 §VI-F). An empty batch stops the send.
    pub(in crate::services::message) async fn send_bootstrap_payload(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        devices: &[u32],
        delivery: SealedDelivery,
        payload: &[u8],
        grpc_pool: &Arc<GrpcClientPool>,
    ) -> Result<Vec<u32>, MessageError> {
        let peer_id_str = peer_id.to_string();
        let mut devices = devices.to_vec();
        let mut last_error = None;

        for attempt in 0..2 {
            let mut messages = Vec::with_capacity(devices.len());
            for &peer_device_id in &devices {
                crate::services::gateway::submission::mark_submission_started()?;
                match self
                    .signal_service
                    .encrypt_message(auth_client, &peer_id_str, peer_device_id, payload)
                    .await
                {
                    Ok(sealed_body) => messages.push(SealedDeviceMessage {
                        device_id: peer_device_id,
                        registration_id: self
                            .signal_service
                            .session_registration_id(&peer_id_str, peer_device_id)
                            .unwrap_or(0),
                        sealed_message: Some(SealedSenderMessage {
                            sealed_body,
                            server_key_id: self.signal_service.get_server_key_id(),
                            timestamp: TimestampNanos::now_anonymized().as_u64(),
                        }),
                    }),
                    Err(e) => {
                        tracing::warn!(peer_id = %peer_id, peer_device_id, error = %e, "DM bootstrap: encrypt failed for device");
                        last_error = Some(MessageError::EncryptionFailed(format!(
                            "Bootstrap encrypt: {}",
                            e
                        )));
                    }
                }
            }
            if messages.is_empty() && delivery == SealedDelivery::NamedDevices {
                break;
            }

            let credential_token = match self.token_manager.claim_prefetched_token() {
                Ok(token) => token,
                Err(e) => {
                    last_error = Some(MessageError::DmError(format!(
                        "No anonymous token for the DM bootstrap ({e}); try again shortly"
                    )));
                    break;
                }
            };

            // Send to peer's device inbox via anonymous client (no AuthInterceptor)
            // to avoid dual-auth rejection — GRA-436
            let mut message_client = if self.shield_mode.load(std::sync::atomic::Ordering::Acquire)
            {
                grpc_pool
                    .secret_message_client()
                    .map_err(MessageError::Network)?
            } else {
                grpc_pool.anonymous_message_client()
            };
            match message_client
                .send_with_anonymous_credential(
                    &uuid::Uuid::new_v4().to_string(),
                    &peer_id_str,
                    messages,
                    delivery,
                    0,
                    &credential_token.public_key,
                    &credential_token.signature,
                )
                .await
            {
                Ok(response) => {
                    let manager =
                        crate::mailbox::dm::DmConversationManager::new(self.database.clone());
                    for &peer_device_id in &response.delivered_to_devices {
                        tracing::info!(peer_id = %peer_id, peer_device_id, "DM bootstrap payload sent to peer's device inbox");
                        if let Err(e) = manager.mark_device_asked(peer_id, peer_device_id) {
                            tracing::warn!(peer_id = %peer_id, peer_device_id, error = %e, "Failed to record the bootstrap ask");
                        }
                    }
                    if !response.delivered_to_devices.is_empty() {
                        return Ok(response.delivered_to_devices);
                    }
                    break;
                }
                Err(crate::error::NetworkError::MismatchedDevices {
                    missing,
                    extra,
                    stale,
                }) => {
                    tracing::info!(peer_id = %peer_id, ?missing, ?extra, ?stale, "DM bootstrap: peer's devices changed");
                    for &peer_device_id in &extra {
                        self.retire_peer_device(peer_id, peer_device_id).await;
                    }
                    last_error = Some(MessageError::Network(
                        crate::error::NetworkError::MismatchedDevices {
                            missing: missing.clone(),
                            extra: extra.clone(),
                            stale: stale.clone(),
                        },
                    ));
                    if attempt > 0 {
                        break;
                    }
                    if delivery == SealedDelivery::AllDevices {
                        // Registered after all: no longer retired here.
                        {
                            let mut retired = self.retired_peer_devices.lock();
                            for &d in missing.iter().chain(&stale) {
                                retired.remove(&(*peer_id, d));
                            }
                        }
                        if let Err(e) = self
                            .signal_service
                            .reconcile_devices(auth_client, &peer_id_str, &missing, &[], &stale)
                            .await
                        {
                            last_error = Some(MessageError::from_signal_send(
                                e,
                                "DM bootstrap device reconciliation",
                            ));
                            break;
                        }
                        devices = self.live_session_devices(peer_id).await?;
                    } else {
                        devices.retain(|d| !extra.contains(d));
                    }
                }
                Err(e) => {
                    tracing::warn!(peer_id = %peer_id, error = %e, "DM bootstrap: send failed");
                    last_error = Some(MessageError::DmError(format!(
                        "Failed to send bootstrap: {}",
                        e
                    )));
                    break;
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            if self.shield_mode.load(std::sync::atomic::Ordering::Acquire) {
                MessageError::NotReachablePrivately(*peer_id)
            } else {
                MessageError::DmError(format!("every known device of {peer_id} is retired"))
            }
        }))
    }

    /// The server refused `peer_device_id` as unknown: mark it retired,
    /// archive its session and drop it from the mailbox conversation.
    async fn retire_peer_device(&self, peer_id: &UserId, peer_device_id: u32) {
        self.retired_peer_devices
            .lock()
            .insert((*peer_id, peer_device_id), std::time::Instant::now());
        if let Err(e) = self
            .signal_service
            .actor
            .archive_session_for_resend(peer_id.to_string(), peer_device_id, None)
            .await
        {
            tracing::warn!(peer_id = %peer_id, peer_device_id, error = %e, "Failed to archive session for retired device");
        }
        if let Err(e) = crate::mailbox::dm::DmConversationManager::new(self.database.clone())
            .forget_peer_device(peer_id, peer_device_id)
        {
            tracing::warn!(peer_id = %peer_id, peer_device_id, error = %e, "Failed to drop retired device from the mailbox conversation");
        }
    }

    /// Ask one peer device that has no half of the conversation yet to make
    /// one: the lower UUID re-announces its live mailbox and key to it (a
    /// bootstrap MESSAGE), the higher sends it a REQUEST. One token.
    pub(in crate::services::message) async fn prompt_peer_device(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        peer_device_id: u32,
        grpc_pool: &Arc<GrpcClientPool>,
    ) -> Result<(), MessageError> {
        let our_user_id = self.settings_repo.current_user_id()?;
        let payload = if our_user_id.as_uuid() < peer_id.as_uuid() {
            let mut mailbox_client = self.bootstrap_mailbox_client(grpc_pool)?;
            DmBootstrap::new(self.database.clone())
                .initiate_dm(*peer_id, &mut mailbox_client)
                .await
                .map_err(|e| {
                    MessageError::DmError(format!("re-announce for device {peer_device_id}: {e}"))
                })?
                .bootstrap_message
        } else {
            bootstrap_request(&our_user_id)
        };
        self.send_bootstrap_payload(
            auth_client,
            peer_id,
            &[peer_device_id],
            SealedDelivery::NamedDevices,
            &payload,
            grpc_pool,
        )
        .await
        .map(|_| ())
    }
}

/// A bootstrap REQUEST: a KeyExchange with an empty ephemeral.
pub(super) fn bootstrap_request(our_user_id: &UserId) -> Vec<u8> {
    crate::proto::content_v1::Content {
        content: Some(crate::proto::content_v1::content::Content::KeyExchange(
            crate::proto::content_v1::KeyExchange {
                ephemeral_pk: vec![], // 0 bytes = bootstrap REQUEST
                conversation_id: our_user_id.to_string(),
                timestamp: TimestampNanos::now().as_u64(),
            },
        )),
        ephemeral_reply_to: None,
        timestamp: TimestampNanos::now().as_u64(),
        padding: vec![],
    }
    .encode_to_vec()
}
