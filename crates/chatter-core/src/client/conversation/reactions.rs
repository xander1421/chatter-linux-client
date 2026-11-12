use super::*;

impl ConversationHandle {
    /// Read the persisted message at this exact conversation/author/timestamp.
    /// Encrypted server-channel messages use the AnonGroup storage kind while
    /// retaining the real server ChannelId; unrelated kinds never match.
    pub fn reaction_snapshot(
        &self,
        author: &UserId,
        sent_at: u64,
    ) -> ConversationResult<Option<ReactionSnapshot>> {
        let id = self
            .conversation
            .channel_id()
            .ok_or(ConversationError::NoConversation)?;
        let kinds = match self.conversation {
            ActiveConversation::Channel(_) => {
                vec![ConversationType::Channel, ConversationType::AnonGroup]
            }
            ActiveConversation::ServerDm { .. } => vec![ConversationType::ServerDm],
            ActiveConversation::AnonDm(_) => vec![ConversationType::AnonDm],
            ActiveConversation::AnonGroup(_) => vec![ConversationType::AnonGroup],
            ActiveConversation::None => return Err(ConversationError::NoConversation),
        };
        let mut found = None;
        for kind in kinds {
            let scope = crate::domain::reaction::ReactionScope::new(id, kind);
            if let Some(message) = self
                .client
                .messages()
                .message_repo
                .get_by_reaction_scope(&scope, author, sent_at)?
            {
                if found.is_some() {
                    return Err(ConversationError::Other(
                        "Ambiguous reaction storage kind for channel".into(),
                    ));
                }
                found = Some(message);
            }
        }
        let Some(message) = found else {
            return Ok(None);
        };
        let scope = crate::domain::reaction::ReactionScope::of(&message);
        Ok(Some(ReactionSnapshot {
            scope,
            conversation: self.conversation,
            author: *author,
            sent_at: message.sent_at,
            summary: crate::domain::message::ReactionCount::summary_string(
                &message.reaction_counts,
            ),
            counts: message.reaction_counts,
            own_emojis: message.current_user_reactions,
        }))
    }

    /// Bind a domain event to this exact persisted scope before exposing a UI snapshot.
    pub fn reaction_snapshot_for_scope(
        &self,
        expected: &crate::domain::reaction::ReactionScope,
        author: &UserId,
        sent_at: u64,
    ) -> ConversationResult<Option<ReactionSnapshot>> {
        Ok(self
            .reaction_snapshot(author, sent_at)?
            .filter(|snapshot| snapshot.scope == *expected))
    }

    /// Toggle a reaction: query local state, decide add vs remove, then
    /// dispatch. Returns `true` if the new state is "reacted".
    ///
    /// UI callers should prefer this over [`send_reaction`] — it eliminates
    /// the "query `user_has_reacted` then send remove flag" pattern that
    /// used to live in the UI.
    pub async fn toggle_reaction(
        &self,
        emoji: &str,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> Result<bool, ConversationError> {
        self.require_remote_message_actions()?;
        // Per-user check — the summary aggregate would flip other users'
        // reactions off.
        let snapshot = self
            .reaction_snapshot(target_author, target_sent_timestamp)?
            .ok_or_else(|| {
                ConversationError::Other(
                    "Reaction target is not stored in this conversation".into(),
                )
            })?;
        self.toggle_reaction_for_scope(&snapshot.scope, emoji, target_author, target_sent_timestamp)
            .await
    }

    /// Toggle only the message scope captured by the caller. A switched or
    /// replaced conversation cannot redirect an already queued UI intent.
    pub async fn toggle_reaction_for_scope(
        &self,
        scope: &crate::domain::reaction::ReactionScope,
        emoji: &str,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> Result<bool, ConversationError> {
        let snapshot = self
            .reaction_snapshot_for_scope(scope, target_author, target_sent_timestamp)?
            .ok_or_else(|| {
                ConversationError::Other(
                    "Reaction target no longer stored in captured scope".into(),
                )
            })?;
        let remove = snapshot.own_emojis.iter().any(|own| own == emoji);
        self.send_reaction_for_scope(scope, emoji, target_author, target_sent_timestamp, remove)
            .await?;
        Ok(!remove)
    }

    /// Send a reaction.
    pub async fn send_reaction(
        &self,
        emoji: &str,
        target_author: &UserId,
        target_sent_timestamp: u64,
        remove: bool,
    ) -> ConversationResult {
        self.require_remote_message_actions()?;
        let scope = self
            .reaction_snapshot(target_author, target_sent_timestamp)?
            .ok_or_else(|| {
                ConversationError::Other("Reaction target not stored in this conversation".into())
            })?
            .scope;
        self.send_reaction_for_scope(&scope, emoji, target_author, target_sent_timestamp, remove)
            .await
    }

    pub(super) async fn send_reaction_for_scope(
        &self,
        scope: &crate::domain::reaction::ReactionScope,
        emoji: &str,
        target_author: &UserId,
        target_sent_timestamp: u64,
        remove: bool,
    ) -> ConversationResult {
        self.require_remote_message_actions()?;
        require_remote_message_kind(&scope.kind)?;
        self.reaction_snapshot_for_scope(scope, target_author, target_sent_timestamp)?
            .ok_or_else(|| {
                ConversationError::Other(
                    "Reaction target no longer stored in captured scope".into(),
                )
            })?;
        let target_author_str = target_author.to_string();
        if self.conversation == ActiveConversation::None {
            return Err(ConversationError::NoConversation);
        }
        let auth = self.client.auth_state().get();
        let session_revision = auth.session_revision;
        let user_id = auth
            .current_user_id
            .ok_or_else(|| ConversationError::Other("Reaction requires a signed-in user".into()))?;
        let action_ts = self
            .client
            .messages()
            .message_repo
            .allocate_reaction_clock(&user_id)?;
        let dispatch = async {
            match &self.conversation {
                ActiveConversation::AnonDm(id) => {
                    self.client
                        .anon_dms()
                        .send_reaction_at(
                            id,
                            emoji,
                            &target_author_str,
                            target_sent_timestamp,
                            remove,
                            action_ts,
                        )
                        .await?;
                }
                ActiveConversation::AnonGroup(id) => {
                    self.client
                        .anon_groups()
                        .send_reaction_at(
                            id,
                            emoji,
                            &target_author_str,
                            target_sent_timestamp,
                            remove,
                            action_ts,
                        )
                        .await?;
                }
                ActiveConversation::None => return Err(ConversationError::NoConversation),
                // DM (mailbox/server) and channel share build + persist; only the
                // channel embeds a channel_id and the transport differs.
                _ => {
                    // Scope and local storage were validated before any remote effect.
                    let wire_author = match self.conversation {
                        ActiveConversation::ServerDm { peer_id } => {
                            crate::domain::reaction::encode_dm_reaction_target(
                                &peer_id,
                                target_author,
                            )
                        }
                        _ => target_author_str.clone(),
                    };
                    let content =
                        crate::services::gateway::builders::build_reaction_content_with_channel_at(
                            emoji,
                            remove,
                            &wire_author,
                            target_sent_timestamp,
                            self.active_channel_id().as_deref(),
                            action_ts,
                        );
                    self.dispatch_content(content).await?;
                }
            }
            Ok(())
        };
        complete_reaction_dispatch(action_ts, dispatch, |clock| {
            let auth = self.client.auth_state().get();
            if auth.session_revision != session_revision || auth.current_user_id != Some(user_id) {
                return Err(ConversationError::Other(
                    "Reaction auth session changed while sending".into(),
                ));
            }

            self.client
                .messages()
                .record_reaction_completion_at(
                    &scope,
                    target_author,
                    target_sent_timestamp,
                    &user_id,
                    emoji,
                    remove,
                    clock,
                )
                .map_err(ConversationError::Message)
        })
        .await
    }

    /// Send a typing indicator.
    ///
    /// Privacy gate: honours the local "show typing indicators" setting. When
    /// the user has disabled it (`typing_indicators_enabled() == false`) we do
    /// NOT emit typing to the peer. Fail-safe default is ON (the shipped
    /// default and the getter's `unwrap_or(true)`), so a settings read error
    /// keeps the current behaviour rather than silently muting a user who never
    /// opted out.
    pub async fn send_typing(&self, started: bool) -> ConversationResult {
        if !self.client.typing_indicators_enabled().unwrap_or(true) {
            return Ok(());
        }
        if matches!(
            self.conversation,
            ActiveConversation::ServerDm { .. } | ActiveConversation::Channel(_)
        ) && !self
            .client
            .offline_state()
            .connection_state()
            .is_connected()
        {
            return Ok(());
        }
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                self.client
                    .anon_dms()
                    .send_typing_indicator(id, started)
                    .await?;
                Ok(())
            }
            ActiveConversation::AnonGroup(id) => {
                self.client
                    .anon_groups()
                    .send_typing_indicator(id, started)
                    .await?;
                Ok(())
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
            _ => {
                let content = crate::services::build_typing_content_with_channel(
                    started,
                    self.active_channel_id().as_deref(),
                );
                self.dispatch_content(content).await
            }
        }
    }
}

/// Shared by actual dispatch and regression transport injection. The caller has
/// already allocated the action clock; completion cannot allocate another one.
pub(crate) async fn complete_reaction_dispatch<F, P>(
    action_ts: u64,
    dispatch: F,
    persist: P,
) -> ConversationResult
where
    F: std::future::Future<Output = ConversationResult>,
    P: FnOnce(u64) -> Result<bool, ConversationError>,
{
    dispatch.await?;
    if !persist(action_ts)? {
        return Err(ConversationError::Other(
            "Reaction action superseded while sending".into(),
        ));
    }
    Ok(())
}
