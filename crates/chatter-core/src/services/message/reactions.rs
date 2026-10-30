use super::*;

impl MessageService {
    /// Persist a reaction. Does no conversation binding: a peer's reaction
    /// from the inbox goes through [`Self::record_inbound_reaction`].
    ///
    /// Returns `true` if the reaction was newly inserted, `false` if it was
    /// already known (from a prior poll cycle, or because the reaction was
    /// already attached to the message snapshot we fetched). Inbox handlers
    /// gate `DomainEvent::ReactionAdded` on this so the UI doesn't double-
    /// count when the snapshot + delta both carry the same reaction.
    pub fn record_reaction(
        &self,
        scope: &crate::domain::reaction::ReactionScope,
        target_sender: &UserId,
        target_sent_at_ns: u64,
        reactor: &UserId,
        emoji: &str,
        remove: bool,
    ) -> Result<bool, MessageError> {
        let action_ts = self.message_repo.allocate_reaction_clock(reactor)?;
        self.record_reaction_at(
            scope,
            target_sender,
            target_sent_at_ns,
            reactor,
            emoji,
            remove,
            action_ts,
        )
    }

    pub fn record_reaction_at(
        &self,
        scope: &crate::domain::reaction::ReactionScope,
        target_sender: &UserId,
        target_sent_at_ns: u64,
        reactor: &UserId,
        emoji: &str,
        remove: bool,
        action_ts: u64,
    ) -> Result<bool, MessageError> {
        let current = self.current_user_id()?;
        if remove {
            self.message_repo
                .remove_reaction_at(
                    scope,
                    target_sender,
                    target_sent_at_ns,
                    reactor,
                    &current,
                    emoji,
                    action_ts,
                )
                .map_err(MessageError::from)
        } else {
            self.message_repo
                .add_reaction_at(
                    scope,
                    target_sender,
                    target_sent_at_ns,
                    reactor,
                    &current,
                    emoji,
                    action_ts,
                )
                .map_err(MessageError::from)
        }
    }

    /// Outgoing completion requires action evidence, not a visible INSERT/DELETE.
    pub(crate) fn record_reaction_completion_at(
        &self,
        scope: &crate::domain::reaction::ReactionScope,
        target_sender: &UserId,
        target_sent_at_ns: u64,
        reactor: &UserId,
        emoji: &str,
        remove: bool,
        action_ts: u64,
    ) -> Result<bool, MessageError> {
        let current = self.current_user_id()?;
        Self::record_reaction_completion_with_repo(
            &self.message_repo,
            &current,
            scope,
            target_sender,
            target_sent_at_ns,
            reactor,
            emoji,
            remove,
            action_ts,
        )
    }

    // Shared with credential-free completion regression fixtures.
    pub(crate) fn record_reaction_completion_with_repo(
        repo: &MessageRepository,
        current: &UserId,
        scope: &crate::domain::reaction::ReactionScope,
        target_sender: &UserId,
        target_sent_at_ns: u64,
        reactor: &UserId,
        emoji: &str,
        remove: bool,
        action_ts: u64,
    ) -> Result<bool, MessageError> {
        let outcome = if remove {
            repo.remove_reaction_outcome_at(
                scope,
                target_sender,
                target_sent_at_ns,
                reactor,
                current,
                emoji,
                action_ts,
            )
        } else {
            repo.add_reaction_outcome_at(
                scope,
                target_sender,
                target_sent_at_ns,
                reactor,
                current,
                emoji,
                action_ts,
            )
        }?;
        Ok(outcome.completed)
    }

    /// Resolve the target of an inbound peer reaction or pin, or `None` when
    /// it is not here or must be dropped.
    ///
    /// `(target_author, sent_at)` names a message in *any* conversation, so a
    /// peer who learned one could otherwise react to or pin it from a
    /// conversation they are not in. Signal-Desktop
    /// (`ts/messageModifiers/Reactions.preload.ts`, `isMessageAMatchForReaction`)
    /// accepts a modifier from our own devices for any message we hold, and from
    /// anyone else only if they are a valid sender in the target's
    /// conversation. Here the target must also live in `conversation` — the
    /// one the modifier arrived in (`ChannelId::from(peer)` for a friend DM,
    /// the carried channel id for a channel). A modifier whose target is not
    /// here yet is held by [`Self::record_inbound_reaction`] /
    /// [`Self::resolve_inbound_pin`]; `message_modifier_repo::may_modify`
    /// applies this same rule when the target arrives.
    pub fn resolve_inbound_target(
        &self,
        conversation: &ChannelId,
        actor: &UserId,
        target_author: &UserId,
        target_sent_at_ns: u64,
    ) -> Result<Option<Message>, MessageError> {
        Ok(
            match self.lookup_inbound_target(
                conversation,
                actor,
                target_author,
                target_sent_at_ns,
            )? {
                InboundTarget::Allowed(target) => Some(target),
                InboundTarget::NotHeld | InboundTarget::Refused => None,
            },
        )
    }

    fn lookup_inbound_target(
        &self,
        conversation: &ChannelId,
        actor: &UserId,
        target_author: &UserId,
        target_sent_at_ns: u64,
    ) -> Result<InboundTarget, MessageError> {
        let Some(target) = self.get_by_sender_timestamp(target_author, target_sent_at_ns)? else {
            return Ok(InboundTarget::NotHeld);
        };
        if *actor == self.current_user_id()? {
            return Ok(InboundTarget::Allowed(target));
        }
        let participant = target.conversation_id == *conversation
            && match target.conversation_type {
                ConversationType::ServerDm => *conversation == ChannelId::from(*actor),
                ConversationType::Channel => self
                    .channel_repo
                    .get_membership(conversation, actor)?
                    .is_some(),
                _ => false,
            };
        if !participant {
            tracing::debug!(
                %actor, %conversation, target_conversation = %target.conversation_id,
                "Inbound reaction/pin: target outside the arrival conversation, or sender not in it — dropped"
            );
            return Ok(InboundTarget::Refused);
        }
        Ok(InboundTarget::Allowed(target))
    }

    /// Where a reaction or pin from `actor` arriving in `conversation` may
    /// land once its target arrives: that conversation — or, from our own
    /// device, anywhere (as [`Self::lookup_inbound_target`] lets it).
    fn held_modifier_scope(
        &self,
        conversation: &ChannelId,
        actor: &UserId,
    ) -> Result<Option<ChannelId>, MessageError> {
        Ok((*actor != self.current_user_id()?).then_some(*conversation))
    }

    /// Authenticate a DM destination claim against the actual envelope sender.
    pub fn dm_reaction_admission(
        &self,
        transport_peer: Option<UserId>,
        reactor: UserId,
        target: &str,
    ) -> Result<(crate::domain::reaction::ReactionScope, UserId), MessageError> {
        let own = self.current_user_id()?;
        let (claimed, author) = crate::domain::reaction::decode_dm_reaction_target(target)?;
        let conversation_peer = if reactor == own {
            claimed
        } else {
            if claimed != own || transport_peer.is_some_and(|peer| peer != reactor) {
                return Err(MessageError::InvalidContent(
                    "Reaction destination differs from authenticated transport".into(),
                ));
            }
            reactor
        };
        if conversation_peer == own {
            return Err(MessageError::InvalidContent(
                "Reaction destination is not a peer conversation".into(),
            ));
        }
        Ok((
            crate::domain::reaction::ReactionScope::new(
                ChannelId::from(conversation_peer),
                ConversationType::ServerDm,
            ),
            author,
        ))
    }

    /// Persist a peer's reaction that arrived in `conversation`, after binding
    /// its target to that conversation ([`Self::resolve_inbound_target`]).
    /// One whose target is not here yet is held until it arrives (Signal
    /// `Reactions.preload.ts` keeps it likewise), ordered against other held
    /// actions on the same emoji by `reaction_ts`. `Ok(false)` when held,
    /// dropped or already recorded.
    #[allow(clippy::too_many_arguments)]
    pub fn record_inbound_reaction(
        &self,
        scope: &crate::domain::reaction::ReactionScope,
        reactor: &UserId,
        target_author: &UserId,
        target_sent_at_ns: u64,
        emoji: &str,
        remove: bool,
        reaction_ts: u64,
    ) -> Result<bool, MessageError> {
        let allowed = *reactor == self.current_user_id()?
            || match scope.kind {
                ConversationType::ServerDm => scope.id == ChannelId::from(*reactor),
                ConversationType::Channel => self
                    .channel_repo
                    .get_membership(&scope.id, reactor)?
                    .is_some(),
                _ => false,
            };
        if !allowed {
            return Ok(false);
        }
        let Some(_target) =
            self.message_repo
                .get_by_reaction_scope(scope, target_author, target_sent_at_ns)?
        else {
            crate::storage::repositories::message_modifier_repo::MessageModifierRepository::new(
                self.database.clone(),
            )
            .queue_reaction(
                target_author,
                target_sent_at_ns,
                reactor,
                emoji,
                remove,
                scope,
                reaction_ts,
                crate::util::time::TimestampNanos::now().as_u64(),
            )?;
            return Ok(false);
        };
        self.record_reaction_at(
            scope,
            target_author,
            target_sent_at_ns,
            reactor,
            emoji,
            remove,
            reaction_ts,
        )
    }

    /// Resolve a channel transport target only within its actual channel ID.
    /// Encrypted server-channel rows use AnonGroup storage. Never search another ID.
    pub fn channel_reaction_scope(
        &self,
        channel: ChannelId,
        _author: &UserId,
        _sent_at: u64,
    ) -> Result<crate::domain::reaction::ReactionScope, MessageError> {
        let mut found = None;
        for kind in [ConversationType::Channel, ConversationType::AnonGroup] {
            let candidate = crate::domain::reaction::ReactionScope::new(channel, kind);
            if self
                .message_repo
                .get_by_reaction_scope(&candidate, _author, _sent_at)?
                .is_some()
            {
                if found.is_some() {
                    return Err(MessageError::InvalidContent(
                        "Ambiguous channel reaction storage kind".into(),
                    ));
                }
                found = Some(candidate);
            }
        }
        if let Some(scope) = found {
            return Ok(scope);
        }
        let channel_row = self.channel_repo.get(&channel)?.ok_or_else(|| {
            MessageError::InvalidContent("Reaction channel metadata unavailable".into())
        })?;
        Ok(crate::domain::reaction::ReactionScope::new(
            channel,
            if channel_row.config.require_encryption {
                ConversationType::AnonGroup
            } else {
                ConversationType::Channel
            },
        ))
    }

    /// The target of a pin (or `unpin`) that arrived in `conversation`, for
    /// the caller to announce ([`Self::resolve_inbound_target`]). One whose
    /// target is not here yet is held and applied to it on arrival, the latest
    /// `pin_ts` winning; `None` then, as when dropped.
    #[allow(clippy::too_many_arguments)]
    pub fn resolve_inbound_pin(
        &self,
        conversation: &ChannelId,
        pinner: &UserId,
        target_author: &UserId,
        target_sent_at_ns: u64,
        unpin: bool,
        pin_ts: u64,
    ) -> Result<Option<Message>, MessageError> {
        match self.lookup_inbound_target(conversation, pinner, target_author, target_sent_at_ns)? {
            InboundTarget::Allowed(target) => Ok(Some(target)),
            InboundTarget::Refused => Ok(None),
            InboundTarget::NotHeld => {
                tracing::debug!(%pinner, %target_author, target_sent_at_ns, "Pin before its message — held");
                crate::storage::repositories::message_modifier_repo::MessageModifierRepository::new(self.database.clone())
                    .queue_pin(
                        target_author, target_sent_at_ns, pinner, unpin,
                        self.held_modifier_scope(conversation, pinner)?, pin_ts,
                        crate::util::time::TimestampNanos::now().as_u64(),
                    )?;
                Ok(None)
            }
        }
    }

    /// Validate server reaction projection identity without replacing the
    /// independent sealed peer-control ledger or resolving historical ambiguity.
    pub fn reconcile_channel_reactions(
        &self,
        message: &crate::proto::chat_v1::Message,
        channel_id: ChannelId,
    ) -> Result<(), MessageError> {
        let id = message
            .id
            .parse::<crate::domain::ids::MessageId>()
            .map_err(|_| MessageError::InvalidContent("invalid message id".into()))?;
        let local = self.get_message_local(&id)?;
        let author = if message.author_id.is_empty() {
            None
        } else {
            Some(message.author_id.parse::<UserId>().map_err(|_| {
                MessageError::InvalidContent("invalid reaction snapshot author".into())
            })?)
        };
        // An uncached own encrypted row has no authenticated message clock.
        // Preserve its backup limitation while validating all supplied identities.
        let encrypted = matches!(
            message
                .content
                .as_ref()
                .and_then(|content| content.r#type.as_ref()),
            Some(crate::proto::chat_v1::message::content::Type::Encrypted(_))
        );
        let target_timestamp = local
            .as_ref()
            .map(|row| row.sent_at)
            .or_else(|| (!encrypted).then_some(message.timestamp));
        // Validate the entire snapshot before any early return or ledger mutation.
        let _validated: Vec<(UserId, String)> = message
            .reactions
            .iter()
            .map(|reaction| {
                let reactor = reaction
                    .reactor_id
                    .parse::<UserId>()
                    .map_err(|_| MessageError::InvalidContent("invalid reactor id".into()))?;
                let target = reaction.target_author_aci.parse::<UserId>().map_err(|_| {
                    MessageError::InvalidContent("invalid reaction target author".into())
                })?;
                if Some(target) != author
                    || target_timestamp
                        .is_some_and(|timestamp| reaction.target_sent_timestamp != timestamp)
                {
                    return Err(MessageError::InvalidContent(
                        "reaction target mismatch".into(),
                    ));
                }
                Ok((reactor, reaction.emoji.clone()))
            })
            .collect::<Result<_, MessageError>>()?;
        let Some(local) = local else {
            return Ok(());
        };
        if local.conversation_id != channel_id
            || local.conversation_type != ConversationType::Channel
        {
            return Err(MessageError::InvalidContent(
                "Reaction snapshot conversation kind/id mismatch".into(),
            ));
        }
        if local.is_deleted {
            return Ok(());
        }
        if local.sender_id != author {
            return Err(MessageError::InvalidContent(
                "reaction snapshot author mismatch".into(),
            ));
        }
        // Client channel reactions travel as sealed peer controls. A server
        // Message.reactions projection is not a complete snapshot of that ledger,
        // even when nonempty, and cannot clear ambiguity or tombstones.
        // Retain verified local votes/projections; peer controls remain authority.
        Ok(())
    }

}
