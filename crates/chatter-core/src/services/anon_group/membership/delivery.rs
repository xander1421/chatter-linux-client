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
        use crate::services::gateway::submission::{self, Admission};

        let plaintext = content.encode_to_vec();
        let owner = submission::user_post(&content).then(|| self.current_user_id()).transpose()?;
        if let Some(owner) = owner {
            if let Some(existing) = submission::existing(
                self.settings_repo.database(), &owner.to_string(), &peer_id.to_string(), &plaintext,
            )? {
                submission::mark_existing_submission();
                return match existing {
                    Admission::Confirmed => Ok(()),
                    _ => Err(AnonGroupError::DeliveryUnconfirmed),
                };
            }
        }
        #[cfg(test)]
        if let Ok((ready, resume)) = RECIPIENT_PREPARED.try_with(Clone::clone) {
            ready.notify_one();
            resume.notified().await;
        }
        self.check_recipient_session(None)?;
        let device = peer_device.ok_or_else(|| AnonGroupError::InvalidData("recipient is not bound to a device".into()))?;
        let has_session = self.pairwise_service.has_session(peer_id, device).await?;
        let expected_key = if has_session {
            self.pairwise_service.verify_existing_recipient(peer_id).await?;
            None
        } else {
            Some(self.pairwise_service.verify_anonymous_prekey_recipient(peer_id).await?)
        };
        self.grpc_pool.secret_channel().map_err(|e| AnonGroupError::MessageError(e.to_string()))?;
        self.pairwise_service.ensure_sender_certificate().await?;
        self.check_recipient_session(None)?;
        let retained = if let Some(owner) = owner {
            match submission::prepare(self.settings_repo.database(), &owner.to_string(), &peer_id.to_string(), &plaintext)? {
                Admission::First(key) => Some(key),
                Admission::Confirmed => {
                    submission::mark_existing_submission();
                    return Ok(());
                }
                Admission::Unconfirmed => {
                    submission::mark_existing_submission();
                    return Err(AnonGroupError::DeliveryUnconfirmed);
                }
            }
        } else {
            None
        };
        self.check_recipient_session(retained.as_deref())?;
        let encryption = async {
            self.check_group_recipient(&content, peer_id, mailbox_pk)?;
            let sealed = if let Some(key) = expected_key {
                let token = self.claim_token()?;
                self.pairwise_service.encrypt_anonymous(peer_id, Some(device), &plaintext, &token, &key).await?.0
            } else {
                self.pairwise_service.encrypt_admitted_session(peer_id, device, &plaintext).await?
            };
            Ok::<_, AnonGroupError>(sealed)
        }.await;
        let ciphertext = encryption.map_err(|e| {
            if let Some(key) = retained.as_ref() {
                crate::services::gateway::submission::discard_before_encryption(self.settings_repo.database(), key);
            }
            e
        })?;
        self.admit_recipient_encryption(retained.as_deref())?;

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
        crate::state::send_session::check_send_session().map_err(|e| {
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
