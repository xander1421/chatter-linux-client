use super::*;

impl ChatterClient {
    /// Whether this account has a durably recorded transparency mismatch.
    pub fn own_identity_change_detected(&self) -> Result<bool, crate::error::ServiceError> {
        let Some(owner) = self.auth_snapshot().current_user_id else {
            return Ok(false);
        };
        let owner = owner.to_string();
        // Our own devices' shared heads are checked like a contact's; a block
        // on our account cannot stop sends to ourselves, so it warns here.
        let own_block = self
            .signal()
            .key_store
            .peer_kt_block(&owner)
            .map_err(crate::error::ServiceError::Storage)?;
        Ok(own_block.is_some()
            || crate::services::signal::kt_anchor::own_mismatch(
                &self.signal().settings_repo,
                owner.as_bytes(),
            )?)
    }

    pub(super) fn spawn_own_identity_monitor(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        use crate::error::ServiceError;
        use crate::network::grpc::client_pool::TransportMode;
        use crate::services::signal::kt_anchor;
        use std::time::Duration;

        let client = self.clone();
        let session = self.auth_snapshot();
        tokio::spawn(async move {
            let Some(user_id) = session.current_user_id else {
                return;
            };
            let label = user_id.to_string();
            let mut last_mismatch = None;
            let mut consecutive_failures = 0u8;
            while client.session_revision() == session.session_revision {
                let check = async {
                    let expected = client.signal().get_identity_public_key().await?;
                    let pool = &client.services.grpc_pool;
                    let deadline = match pool.transport_mode() {
                        TransportMode::Direct => kt_anchor::PIN_RPC_DEADLINE,
                        TransportMode::Onion => kt_anchor::TOR_RPC_DEADLINE,
                    };
                    // The label names our account. Use the account's transport,
                    // which keeps it off the isolated anonymous circuit.
                    let mut auth = pool.auth_client();
                    Ok::<_, ServiceError>(
                        kt_anchor::monitor_own(
                            auth.key_transparency_client(),
                            &client.signal().settings_repo,
                            &label,
                            &expected,
                            deadline,
                        )
                        .await?,
                    )
                };
                let result = check.await;
                if client.session_revision() != session.session_revision {
                    return;
                }
                let delay = match result {
                    Ok(mismatch) => {
                        consecutive_failures = 0;
                        if last_mismatch != Some(mismatch) {
                            last_mismatch = Some(mismatch);
                            client
                                .event_bus()
                                .emit_domain(DomainEvent::OwnIdentityChanged { user_id });
                        }
                        Duration::from_secs(60 * 60)
                    }
                    Err(error) => {
                        consecutive_failures = consecutive_failures.saturating_add(1);
                        tracing::warn!(%error, consecutive_failures, "Own identity transparency check failed");
                        if consecutive_failures == 3 {
                            client
                                .event_bus()
                                .emit_domain(DomainEvent::KeyTransparencyHistoryUnverified);
                        }
                        Duration::from_secs(5 * 60)
                    }
                };
                tokio::time::sleep(delay).await;
            }
        })
    }

    // ── Safety number verification ────────────────────────────────────

    /// Get the safety number for a peer (60-digit numeric fingerprint).
    ///
    /// Both users see the same number when their identity keys match.
    /// Returns a formatted string with 12 groups of 5 digits.
    pub async fn get_safety_number(
        &self,
        peer_user_id: &crate::domain::ids::UserId,
    ) -> Result<String, ClientError> {
        let local_user_id = self
            .services
            .auth_state
            .get()
            .current_user_id
            .as_ref()
            .ok_or_else(|| ClientError::Validation(chatter_i18n::tr("Not logged in").into()))?
            .to_string();

        let safety_number = self
            .services
            .signal_service
            .generate_safety_number(&local_user_id, &peer_user_id.to_string())
            .await
            .map_err(|e| ClientError::Validation(e.to_string()))?;

        Ok(safety_number)
    }

    /// Check if a peer has been marked as verified.
    pub fn is_peer_verified(
        &self,
        peer_user_id: &crate::domain::ids::UserId,
    ) -> Result<bool, ClientError> {
        self.services
            .signal_service
            .is_peer_verified(&peer_user_id.to_string())
            .map_err(|e| ClientError::Validation(e.to_string()))
    }

    /// Mark a peer as verified with their current safety number.
    pub async fn mark_peer_verified(
        &self,
        peer_user_id: &crate::domain::ids::UserId,
    ) -> Result<(), ClientError> {
        tracing::info!("Marking peer safety number as verified");
        let safety_number = self.get_safety_number(peer_user_id).await?;
        self.services
            .signal_service
            .set_peer_verified(&peer_user_id.to_string(), &safety_number, true)
            .map_err(|e| ClientError::Validation(e.to_string()))?;
        tracing::info!("Peer safety number marked as verified");
        Ok(())
    }

    /// Mark a peer as unverified.
    pub fn mark_peer_unverified(
        &self,
        peer_user_id: &crate::domain::ids::UserId,
    ) -> Result<(), ClientError> {
        // Get existing safety number to preserve it
        if let Ok(Some((safety_number, _))) = self
            .services
            .signal_service
            .key_store
            .get_peer_verification(&peer_user_id.to_string())
        {
            self.services
                .signal_service
                .set_peer_verified(&peer_user_id.to_string(), &safety_number, false)
                .map_err(|e| ClientError::Validation(e.to_string()))?;
        }
        Ok(())
    }

    /// Whether this peer has an identity change waiting on the user's decision,
    /// and whether the peer was cryptographically attributed when it surfaced.
    ///
    /// `Some(attributed)` means the conversation is wedged in both directions.
    /// `attributed == false` means the peer id came from server-supplied
    /// envelope metadata and nothing proved it — the UI MUST NOT present that
    /// as "X's safety number changed" (see
    /// [`crate::error::IdentityChangeSource`]).
    ///
    /// The key itself deliberately does not leave core. The UI has no use for
    /// it, and keeping it here is what lets `accept_peer_identity_change` below
    /// take no key argument at all.
    pub fn pending_identity_change(
        &self,
        peer_user_id: &crate::domain::ids::UserId,
    ) -> Result<Option<bool>, ClientError> {
        self.services
            .signal_service
            .pending_identity_change(&peer_user_id.to_string())
            .map(|opt| opt.map(|(_key, source)| source.is_authenticated()))
            .map_err(|e| ClientError::Validation(e.to_string()))
    }

    /// The prompt to put in front of the user for a parked identity change, or
    /// `None` when the conversation is fine.
    ///
    /// The wording lives here, not in `chatter-ui`, because the choice it makes
    /// is a security claim and not a style one: whether the peer may be NAMED.
    /// The display name is resolved here too, and only READ when the change was
    /// cryptographically attributed — an unattributed one stays anonymous,
    /// because the only thing that tied that name to this key was the server.
    pub fn identity_change_prompt(
        &self,
        peer_user_id: &crate::domain::ids::UserId,
    ) -> Result<Option<IdentityChangePrompt>, ClientError> {
        if let Some(attributed) = self.pending_identity_change(peer_user_id)? {
            return Ok(Some(IdentityChangePrompt::new(
                attributed,
                &self.display_name(peer_user_id),
            )));
        }
        use crate::storage::peer_kt_heads::KtBlock;
        let block = self
            .services
            .signal_service
            .key_store
            .peer_kt_block(&peer_user_id.to_string())
            .map_err(|e| ClientError::Validation(e.to_string()))?;
        Ok(block.map(|block| match block {
            KtBlock::Split => IdentityChangePrompt::split_view(),
            KtBlock::Unverified => IdentityChangePrompt::unverified_view(),
        }))
    }

    /// Accept the peer's new identity key, unwedging the conversation in both
    /// directions. Returns the number of stale sessions dropped.
    ///
    /// **A trust decision. Only ever call this from an explicit user action** —
    /// a reinstall and an active MITM are indistinguishable from here.
    ///
    /// Takes no key: it reads the parked one, so the UI cannot accept a key the
    /// user was never shown, and a server that swaps a fresh key in behind the
    /// prompt still gets refused by
    /// [`crate::services::SignalService::accept_peer_identity_change`]'s own
    /// match check. Errors with `Validation` when nothing is pending, because
    /// an accept with no prompt behind it is a bug, not a no-op.
    pub fn accept_peer_identity_change(
        &self,
        peer_user_id: &crate::domain::ids::UserId,
    ) -> Result<u32, ClientError> {
        let pending = self
            .services
            .signal_service
            .pending_identity_change(&peer_user_id.to_string())
            .map_err(|e| ClientError::Validation(e.to_string()))?;
        // The prompt shows a key change before a split view, so each accept
        // answers the prompt the user is looking at.
        let Some((new_identity_key, _source)) = pending else {
            let cleared = self
                .services
                .signal_service
                .key_store
                .accept_peer_kt_block(&peer_user_id.to_string())
                .map_err(|e| ClientError::Validation(e.to_string()))?;
            if !cleared {
                return Err(ClientError::Validation(
                    chatter_i18n::tr("no identity change is pending for this peer").into(),
                ));
            }
            tracing::warn!(peer = %peer_user_id, "ACCEPTED a conversation blocked by a key transparency check");
            self.event_bus()
                .emit_domain(crate::events::DomainEvent::PeerKeyVerificationUpdated {
                    peer_user_id: *peer_user_id,
                });
            return Ok(0);
        };

        let dropped = self
            .services
            .signal_service
            .accept_peer_identity_change(
                peer_user_id,
                &new_identity_key,
                &self.services.channel_repo,
            )
            .map_err(|e| ClientError::Validation(e.to_string()))?;
        self.event_bus()
            .emit_domain(crate::events::DomainEvent::PeerKeyVerificationUpdated {
                peer_user_id: *peer_user_id,
            });
        Ok(dropped)
    }

    /// Compute the local Signal identity fingerprint (displayed in settings /
    /// safety-number flows).
    pub async fn my_identity_fingerprint(&self) -> Result<String, crate::error::ServiceError> {
        self.signal().get_identity_fingerprint().await
    }
}
