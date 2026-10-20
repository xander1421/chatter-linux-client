use super::*;

impl AnonGroupService {
    /// Send content to a member's anonymous group mailbox.
    ///
    /// Uses pairwise Signal Protocol encryption (forward secrecy) and delivers
    /// directly to the member's anonymous mailbox. Does NOT use DM infrastructure.
    ///
    /// # Arguments
    /// * `peer_id` - The recipient's user ID (for Signal Protocol session)
    /// * `peer_device` - The recipient device that owns `mailbox_pk`
    ///   ([`AnonGroupMember::device_id`]); `None` refuses — the only device that
    ///   could open the message is unknown, and no other device is guessed.
    /// * `mailbox_pk` - The recipient's anonymous group mailbox public key
    /// * `content` - The Content proto to send
    pub(crate) async fn send_to_anon_mailbox(
        &self,
        peer_id: &UserId,
        peer_device: Option<u32>,
        mailbox_pk: &[u8; 32],
        content: Content,
    ) -> Result<(), AnonGroupError> {
        self.check_group_recipient(&content, peer_id, mailbox_pk)?;
        // 1. Serialize content
        let plaintext = content.encode_to_vec();
        let retained = if crate::services::gateway::submission::user_post(&content) {
            let owner = self.current_user_id()?;
            match crate::services::gateway::submission::prepare(
                self.settings_repo.database(),
                &owner.to_string(),
                &peer_id.to_string(),
                &plaintext,
            )? {
                crate::services::gateway::submission::Admission::First(key) => Some(key),
                crate::services::gateway::submission::Admission::Confirmed => {
                    crate::services::gateway::submission::mark_existing_submission();
                    return Ok(());
                }
                crate::services::gateway::submission::Admission::Unconfirmed => {
                    crate::services::gateway::submission::mark_existing_submission();
                    return Err(AnonGroupError::DeliveryUnconfirmed);
                }
            }
        } else {
            None
        };

        // No known recipient device: nothing could open the message, and no
        // other device of theirs is guessed. Placed after the submission
        // lookup so an already-claimed post still reports its own state.
        let Some(device) = peer_device else {
            if let Some(key) = retained.as_ref() {
                crate::services::gateway::submission::discard_before_encryption(
                    self.settings_repo.database(),
                    key,
                );
            }
            return Err(AnonGroupError::InvalidData(
                "recipient is not bound to a device".into(),
            ));
        };
        // 2. Encrypt with Signal Protocol (pairwise, no DM-specific layers).
        //
        // If a usable session already exists, encrypt locally — no prekey
        // fetch, no server contact, no token spent. On FIRST contact with a
        // member there is no session yet; establishing one via the normal
        // AUTHENTICATED prekey fetch would let the server observe "user X is
        // starting a session with member Y", and over a fan-out that
        // reconstructs the group roster this feature promises to hide. So we
        // bootstrap the session ANONYMOUSLY (blind token + anonymous prekey
        // fetch), matching the anon-DM design. Cost stays at ~1 token per new
        // peer (first contact only), not per message.
        let has_session = self
            .pairwise_service
            .has_session(peer_id, device)
            .await
            .map_err(|e| {
                if let Some(key) = retained.as_ref() {
                    crate::services::gateway::submission::discard_before_encryption(
                        self.settings_repo.database(),
                        key,
                    );
                }
                AnonGroupError::CryptoError(e.to_string())
            })?;
        #[cfg(test)]
        if let Ok((ready, resume)) = RECIPIENT_PREPARED.try_with(Clone::clone) {
            ready.notify_one();
            resume.notified().await;
        }
        self.check_recipient_session(retained.as_deref())?;
        // Secret-chat traffic goes over Tor or not at all; refuse before any
        // network contact, token spend or encryption. Placed after the
        // submission lookup so an already-claimed post still reports its own
        // (local) state, and a fresh claim is released here.
        if let Err(e) = self.grpc_pool.secret_channel() {
            if let Some(key) = retained.as_ref() {
                crate::services::gateway::submission::discard_before_encryption(
                    self.settings_repo.database(),
                    key,
                );
            }
            return Err(AnonGroupError::MessageError(e.to_string()));
        }
        // Our own sealed-sender certificate first (fetched over Tor if it
        // must be — never the normal transport), so a missing one fails before
        // any token is spent. Names no recipient.
        self.pairwise_service
            .ensure_sender_certificate()
            .await
            .map_err(|e| {
                if let Some(key) = retained.as_ref() {
                    crate::services::gateway::submission::discard_before_encryption(
                        self.settings_repo.database(),
                        key,
                    );
                }
                AnonGroupError::from(e)
            })?;
        let ciphertext = if has_session {
            self.check_group_recipient(&content, peer_id, mailbox_pk)?;
            self.admit_recipient_encryption(retained.as_deref())?;
            // Local only; a session that vanished since the check fails here
            // rather than being re-established.
            self.pairwise_service
                .encrypt_existing_session(peer_id, device, &plaintext)
                .await?
        } else {
            // Refuse an expired originating session before consuming a token.
            self.check_recipient_session(retained.as_deref())?;
            let token = self.claim_token().map_err(|e| {
                if let Some(key) = retained.as_ref() {
                    crate::services::gateway::submission::discard_before_encryption(
                        self.settings_repo.database(),
                        key,
                    );
                }
                e
            })?;
            self.check_group_recipient(&content, peer_id, mailbox_pk)?;
            self.admit_recipient_encryption(retained.as_deref())?;
            self.pairwise_service
                .encrypt_anonymous(peer_id, Some(device), &plaintext, &token)
                .await?
                .0
        };

        self.check_group_recipient(&content, peer_id, mailbox_pk)?;
        // 3. Send directly to anonymous mailbox
        if let Some(key) = retained.as_ref() {
            crate::services::gateway::submission::retain_deposit(
                self.settings_repo.database(),
                key,
                &ciphertext,
                mailbox_pk,
                &content,
            )
            .map_err(|_| AnonGroupError::DeliveryUnconfirmed)?;
        }
        self.mailbox_manager
            .send_to_secret_mailbox(mailbox_pk, ciphertext, EnvelopeType::SealedSender)
            .await
            .map_err(|e| {
                if let Some(key) = retained.as_ref() {
                    let _ = crate::services::gateway::submission::retry_later(
                        self.settings_repo.database(),
                        key,
                    );
                    AnonGroupError::DeliveryUnconfirmed
                } else {
                    AnonGroupError::MessageError(e.to_string())
                }
            })?;
        if let Some(key) = retained.as_ref() {
            crate::services::gateway::submission::confirm(self.settings_repo.database(), key)
                .map_err(|_| AnonGroupError::DeliveryUnconfirmed)?;
        }

        Ok(())
    }

    /// Revalidate snapshots at encryption and delivery boundaries, including
    /// queue drains. Removal controls deliberately still reach the removed peer.
    fn check_group_recipient(
        &self,
        content: &Content,
        peer: &UserId,
        key: &[u8; 32],
    ) -> Result<(), AnonGroupError> {
        use content_v1::content::Content as Kind;
        let scope = match content.content.as_ref() {
            Some(Kind::AnonGroupMessage(message)) => Some((&message.group_id, content.timestamp)),
            Some(Kind::AnonGroupJoinAck(ack)) => Some((&ack.group_id, ack.timestamp)),
            Some(Kind::AnonGroupMemberList(list)) => Some((&list.group_id, list.timestamp)),
            Some(Kind::AnonGroupProfileShare(profile)) => {
                Some((&profile.group_id, profile.timestamp))
            }
            _ => None,
        };
        if let Some((id, timestamp)) = scope {
            let group = AnonGroupId::try_from_slice(id)
                .ok_or_else(|| AnonGroupError::InvalidData("Invalid recipient group".into()))?;
            // The post path fixed eligibility in its send ledger before
            // parking. A selected invitee may only acquire this mailbox at a
            // later JoinAck, so recheck the active member and exact key here
            // without comparing admission to the post's original timestamp.
            let post = matches!(content.content.as_ref(),
                Some(Kind::AnonGroupMessage(message)) if matches!(message.inner_content.as_deref().and_then(|inner| inner.content.as_ref()), Some(Kind::DataMessage(_))));
            let current = if post {
                self.anon_group_repo
                    .current_post_recipient(&group, peer, key)?
            } else {
                self.anon_group_repo
                    .current_recipient(&group, peer, key, timestamp)?
            };
            if !current {
                return Err(AnonGroupError::MemberNotFound(*peer));
            }
        }
        Ok(())
    }

    // A new lower claim can be discarded only before encryption. An existing
    // unknown claim marks upper uncertainty before reaching these helpers.
    fn check_recipient_session(&self, retained: Option<&str>) -> Result<(), AnonGroupError> {
        crate::services::gateway::submission::check_send_session().map_err(|e| {
            if let Some(key) = retained {
                crate::services::gateway::submission::discard_before_encryption(
                    self.settings_repo.database(),
                    key,
                );
            }
            e.into()
        })
    }

    fn admit_recipient_encryption(&self, retained: Option<&str>) -> Result<(), AnonGroupError> {
        crate::services::gateway::submission::mark_submission_started().map_err(|e| {
            if let Some(key) = retained {
                crate::services::gateway::submission::discard_before_encryption(
                    self.settings_repo.database(),
                    key,
                );
            }
            e.into()
        })
    }

    /// Claim a blind-signature token from the pre-fetched batch.
    ///
    /// Mirrors `AnonDmService::claim_token`. Used to fetch a member's prekey
    /// bundle anonymously on first contact so the server never learns the
    /// group membership graph from session establishment. Never requests
    /// tokens (NDSS'21 §VI-F); an empty batch fails this member's send.
    pub(crate) fn claim_token(
        &self,
    ) -> Result<crate::mailbox::token_manager::MailboxToken, AnonGroupError> {
        self.token_manager
            .claim_prefetched_token()
            .map_err(|e| AnonGroupError::CryptoError(e.to_string()))
    }
}
