use super::*;

impl MessageService {
    /// Persist an edit of one of OUR messages made on this device (update
    /// body + set is_edited flag). Anyone else's edit is persisted, ordered
    /// and dated by [`apply_dm_edit`](Self::apply_dm_edit) /
    /// [`apply_channel_edit`](Self::apply_channel_edit) when it arrives; the
    /// `MessageEdited` it announces must not be stored again here with
    /// `edited_at = now`.
    pub fn update_message_body(
        &self,
        message_id: &crate::domain::ids::MessageId,
        new_content: &crate::domain::message::MessageContent,
    ) -> Result<(), MessageError> {
        let me = self.current_user_id()?;
        if let Some(mut message) = self.message_repo.get(message_id)? {
            if message.sender_id != Some(me)
                || (message.is_edited && message.content == *new_content)
            {
                return Ok(());
            }
            message.content = new_content.clone();
            message.is_edited = true;
            message.edited_at = Some(chrono::Utc::now());
            message.updated_at = chrono::Utc::now();
            self.message_repo.update(&message)?;
        }
        Ok(())
    }

    /// How far from its message an edit may be and still apply
    /// ([`message_modifier_repo::EDIT_MAX_AGE_NS`](crate::storage::repositories::message_modifier_repo::EDIT_MAX_AGE_NS)).
    pub const EDIT_MAX_AGE_NS: u64 =
        crate::storage::repositories::message_modifier_repo::EDIT_MAX_AGE_NS;

    /// Where a modifier from `author` in a friend DM may land: the DM with
    /// them — or, when the author is us (a copy from our own other device),
    /// whichever DM our message is in.
    fn dm_modifier_scope(&self, author: &UserId) -> Option<ChannelId> {
        (self.current_user_id().ok().as_ref() != Some(author)).then(|| ChannelId::from(*author))
    }

    /// A friend DM edit from `peer` of its message sent at `target_sent_at`;
    /// see [`apply_edit`](Self::apply_edit).
    pub fn apply_dm_edit(
        &self,
        peer: &UserId,
        target_sent_at: u64,
        new_body: &str,
        edit_ts: u64,
        server_ts: u64,
    ) -> Result<Option<Message>, MessageError> {
        self.apply_edit(
            peer,
            self.dm_modifier_scope(peer),
            target_sent_at,
            new_body,
            edit_ts,
            server_ts,
        )
    }

    /// A channel edit from `sender` of its message sent at `target_sent_at`;
    /// see [`apply_edit`](Self::apply_edit).
    pub fn apply_channel_edit(
        &self,
        channel_id: &ChannelId,
        sender: &UserId,
        target_sent_at: u64,
        new_body: &str,
        edit_ts: u64,
        server_ts: u64,
    ) -> Result<Option<Message>, MessageError> {
        self.apply_edit(
            sender,
            Some(*channel_id),
            target_sent_at,
            new_body,
            edit_ts,
            server_ts,
        )
    }

    /// Receive `author`'s edit of its own message sent at `target_sent_at`:
    /// [`MessageModifierRepository::apply_received_edit`](crate::storage::repositories::message_modifier_repo::MessageModifierRepository::apply_received_edit),
    /// the one choke point every received edit goes through.
    fn apply_edit(
        &self,
        author: &UserId,
        conversation: Option<ChannelId>,
        target_sent_at: u64,
        new_body: &str,
        edit_ts: u64,
        server_ts: u64,
    ) -> Result<Option<Message>, MessageError> {
        Ok(
            crate::storage::repositories::message_modifier_repo::MessageModifierRepository::new(
                self.database.clone(),
            )
            .apply_received_edit(
                &self.message_repo,
                author,
                conversation,
                target_sent_at,
                new_body,
                edit_ts,
                server_ts,
            )?,
        )
    }

    /// Explicit call identity never falls back to timestamp semantics on refusal.
    pub(crate) async fn apply_scoped_call_or_timestamp_delete_received(
        &self,
        sender: &UserId,
        target_author: Option<&str>,
        destination: Option<&str>,
        target_call_id: Option<&str>,
        timestamp: u64,
    ) -> Result<Option<Message>, MessageError> {
        let Some(claim) = target_call_id else {
            return self
                .apply_scoped_dm_delete_received(sender, target_author, destination, timestamp)
                .await;
        };
        let me = self.current_user_id()?;
        if sender.as_uuid().is_nil() {
            return Ok(None);
        }
        let peer = if *sender == me {
            let (Some(destination), Some(author)) = (destination, target_author) else {
                return Ok(None);
            };
            let (Ok(peer), Ok(author)) = (destination.parse::<UserId>(), author.parse::<UserId>())
            else {
                return Ok(None);
            };
            if peer == me || peer.as_uuid().is_nil() || (author != me && author != peer) {
                return Ok(None);
            }
            peer
        } else {
            if destination.is_some() {
                return Ok(None);
            }
            if target_author.is_some_and(|value| {
                value
                    .parse::<UserId>()
                    .ok()
                    .is_none_or(|a| a != me && a != *sender)
            }) {
                return Ok(None);
            }
            *sender
        };
        Ok(
            crate::storage::repositories::message_modifier_repo::apply_call_delete(
                self.database.clone(),
                &self.message_repo,
                &crate::domain::reaction::ReactionScope::new(
                    ChannelId::from(peer),
                    ConversationType::ServerDm,
                ),
                claim,
            )?,
        )
    }

    /// Route using the authenticated sender, accepting a destination only on
    /// self-device copies. No missing self-copy destination, no author guessing.
    pub async fn apply_scoped_dm_delete_received(
        &self,
        sender: &UserId,
        target_author: Option<&str>,
        destination: Option<&str>,
        target_sent_at: u64,
    ) -> Result<Option<Message>, MessageError> {
        let me = self.current_user_id()?;
        let peer = if *sender == me {
            let (Some(destination), Some(author)) = (destination, target_author) else {
                return Ok(None);
            };
            let Ok(peer) = destination.parse::<UserId>() else {
                return Ok(None);
            };
            if peer == me
                || author
                    .parse::<UserId>()
                    .ok()
                    .is_none_or(|author| author != me && author != peer)
            {
                return Ok(None);
            }
            peer
        } else {
            if destination.is_some() {
                return Ok(None);
            }
            *sender
        };
        self.apply_targeted_dm_delete_received(&peer, target_author, target_sent_at)
            .await
    }

    pub async fn apply_targeted_dm_delete_received(
        &self,
        peer: &UserId,
        target_author: Option<&str>,
        target_sent_at: u64,
    ) -> Result<Option<Message>, MessageError> {
        self.apply_targeted_dm_delete(peer, target_author, target_sent_at)
    }

    fn admitted_dm_delete_author(
        &self,
        peer: &UserId,
        target_author: Option<&str>,
    ) -> Result<Option<UserId>, MessageError> {
        let me = self.current_user_id()?;
        // Self-device deletes need a destination claim; this route has none.
        if *peer == me {
            return Ok(None);
        }
        let author = match target_author {
            Some(value) => match value.parse::<UserId>() {
                Ok(author) => author,
                Err(_) => return Ok(None),
            },
            None => return Ok(None),
        };
        Ok((author == *peer || author == me).then_some(author))
    }

    fn targeted_dm_delete_target(
        &self,
        peer: &UserId,
        target_author: Option<&str>,
        timestamp: u64,
    ) -> Result<Option<Message>, MessageError> {
        let Some(author) = self.admitted_dm_delete_author(peer, target_author)? else {
            return Ok(None);
        };
        Ok(self.message_repo.get_by_reaction_scope(
            &crate::domain::reaction::ReactionScope::new(
                ChannelId::from(*peer),
                ConversationType::ServerDm,
            ),
            &author,
            timestamp,
        )?)
    }

    /// Exact author and exact arrival conversation/type; never guess a second author.
    pub fn apply_targeted_dm_delete(
        &self,
        peer: &UserId,
        target_author: Option<&str>,
        timestamp: u64,
    ) -> Result<Option<Message>, MessageError> {
        let Some(author) = self.admitted_dm_delete_author(peer, target_author)? else {
            return Ok(None);
        };
        let target = self.targeted_dm_delete_target(peer, target_author, timestamp)?;
        crate::storage::repositories::message_modifier_repo::MessageModifierRepository::new(
            self.database.clone(),
        )
        .queue_dm_delete(
            &author,
            timestamp,
            Some(ChannelId::from(*peer)),
            crate::util::time::TimestampNanos::now().as_u64(),
        )?;
        if let Some(message) = &target {
            self.message_repo.delete(&message.id)?;
        }
        Ok(target)
    }

    /// Apply an authenticated channel author's delete in its exact channel scope.
    /// Retain a typed tombstone even for a held target, so history replay cannot
    /// resurrect it and collisions in other conversations or types survive.
    pub fn apply_channel_delete(
        &self,
        channel_id: &ChannelId,
        sender: &UserId,
        target_sent_at: u64,
    ) -> Result<Option<Message>, MessageError> {
        crate::storage::repositories::message_modifier_repo::MessageModifierRepository::new(
            self.database.clone(),
        )
        .queue_channel_delete(
            sender,
            target_sent_at,
            *channel_id,
            crate::util::time::TimestampNanos::now().as_u64(),
        )?;
        let target = self.message_repo.get_by_reaction_scope(
            &crate::domain::reaction::ReactionScope::new(*channel_id, ConversationType::Channel),
            sender,
            target_sent_at,
        )?;
        if let Some(message) = &target {
            // Storage must reflect deletion before the caller announces it.
            self.message_repo.delete(&message.id)?;
        }
        Ok(target)
    }

}
