use super::*;

impl ChatterClient {
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
            .ok_or_else(|| ClientError::Validation("Not logged in".into()))?
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
        Ok(self
            .pending_identity_change(peer_user_id)?
            .map(|attributed| {
                IdentityChangePrompt::new(attributed, &self.display_name(peer_user_id))
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
        let (new_identity_key, _source) = self
            .services
            .signal_service
            .pending_identity_change(&peer_user_id.to_string())
            .map_err(|e| ClientError::Validation(e.to_string()))?
            .ok_or_else(|| {
                ClientError::Validation("no identity change is pending for this peer".into())
            })?;

        self.services
            .signal_service
            .accept_peer_identity_change(
                peer_user_id,
                &new_identity_key,
                &self.services.channel_repo,
            )
            .map_err(|e| ClientError::Validation(e.to_string()))
    }

}
