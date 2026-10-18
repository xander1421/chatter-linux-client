use super::*;

impl ConversationHandle {
    // ========================================================================
    // Conversation-level operations
    // ========================================================================

    /// Mute or unmute this conversation's notifications.
    ///
    /// For anon groups, uses the group status (Muted vs Active). Everything
    /// else stores a per-conversation settings key.
    pub fn mute_conversation(&self, muted: bool) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                let key = format!("conv.muted.anon_dm.{}", id);
                self.client
                    .channels()
                    .settings_repo
                    .set(&key, &muted)
                    .map_err(|e| ConversationError::Other(e.to_string()))
            }
            ActiveConversation::AnonGroup(id) => {
                if muted {
                    self.client.anon_groups().mute_group(id)?;
                } else {
                    self.client.anon_groups().unmute_group(id)?;
                }
                Ok(())
            }
            ActiveConversation::ServerDm { peer_id } => {
                // Channel-free DM: no membership row to flip. Mirrors the
                // AnonDm pattern — a per-peer settings key.
                let key = format!("conv.muted.server_dm.{}", peer_id);
                self.client
                    .channels()
                    .settings_repo
                    .set(&key, &muted)
                    .map_err(|e| ConversationError::Other(e.to_string()))
            }
            ActiveConversation::Channel(channel_id) => {
                // Not the membership's `is_muted`: that column is the
                // moderator mute (server `ChannelMember.muted`, moderation
                // events), and sharing it let either one overwrite the other.
                let key = format!("conv.muted.channel.{}", channel_id);
                self.client
                    .channels()
                    .settings_repo
                    .set(&key, &muted)
                    .map_err(|e| ConversationError::Other(e.to_string()))
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    /// Read this conversation's persisted mute state (the inverse of
    /// [`Self::mute_conversation`]). Reads exactly the same store the setter
    /// wrote — the group status for anon groups, otherwise the settings key. Defaults to `false` (unmuted) when nothing has been persisted.
    pub fn is_muted(&self) -> Result<bool, ConversationError> {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                let key = format!("conv.muted.anon_dm.{}", id);
                Ok(self
                    .client
                    .channels()
                    .settings_repo
                    .get_bool(&key)
                    .map_err(|e| ConversationError::Other(e.to_string()))?
                    .unwrap_or(false))
            }
            ActiveConversation::AnonGroup(id) => Ok(self
                .client
                .anon_groups()
                .get_group(id)
                .map_err(|e| ConversationError::Other(e.to_string()))?
                .map(|g| g.status == crate::domain::anon_group::AnonGroupStatus::Muted)
                .unwrap_or(false)),
            ActiveConversation::ServerDm { peer_id } => {
                let key = format!("conv.muted.server_dm.{}", peer_id);
                Ok(self
                    .client
                    .channels()
                    .settings_repo
                    .get_bool(&key)
                    .map_err(|e| ConversationError::Other(e.to_string()))?
                    .unwrap_or(false))
            }
            ActiveConversation::Channel(channel_id) => {
                let key = format!("conv.muted.channel.{}", channel_id);
                Ok(self
                    .client
                    .channels()
                    .settings_repo
                    .get_bool(&key)
                    .map_err(|e| ConversationError::Other(e.to_string()))?
                    .unwrap_or(false))
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    /// Leave this conversation (Settings › Leave, the sidebar row menu).
    ///
    /// A channel is left on the server and stays listed, read-only. A private
    /// group tells every other member over their anonymous mailboxes and is
    /// then dropped from this device. DMs have nothing to leave.
    pub async fn leave(&self) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::Channel(id) => Ok(self.client.leave_channel(id).await?),
            ActiveConversation::AnonGroup(id) => {
                self.client.anon_groups().leave_group(id).await?;
                self.purge_anon_group_history(id)?;
                Ok(())
            }
            _ => Err(ConversationError::Other(
                "only channels and groups can be left".into(),
            )),
        }
    }

    /// A voluntary leave removes the group row, but unified history lives in
    /// separate tables. Clear it before a later invitation recreates the row.
    pub(super) fn purge_anon_group_history(
        &self,
        id: &crate::domain::anon_group::AnonGroupId,
    ) -> ConversationResult {
        let channel_id: ChannelId = (*id).into();
        let messages = self
            .client
            .messages()
            .message_repo
            .list_all_for_conversation(&channel_id, &ConversationType::AnonGroup)?;
        let attachments: Vec<_> = messages
            .iter()
            .flat_map(|message| message.attachments.iter().copied())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let ids: Vec<_> = messages.into_iter().map(|message| message.id).collect();
        for batch in ids.chunks(500) {
            self.client.messages().message_repo.delete_batch(batch)?;
        }
        self.client
            .attachments()
            .attachment_repo()
            .delete_unreferenced(&attachments)?;
        self.client
            .messages()
            .message_repo
            .purge_anon_group_orphans(&channel_id, &ids)?;
        self.client
            .notifications()
            .cancel_conversation(&channel_id)
            .map_err(|error| ConversationError::Other(error.to_string()))?;
        self.client
            .clear_composer_conversation_messages(self.conversation, &ids);
        Ok(())
    }

    /// Remove `user` from this conversation (Settings › Members › Remove).
    ///
    /// A channel kick is authorized by the server. A private-group kick is
    /// refused here unless we may kick (admin/owner) and the target is not the
    /// owner; it is signed with our group mailbox key, and every member's
    /// client verifies that signature before applying it (`handle_kick`).
    pub async fn remove_member(&self, user: &UserId) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::Channel(id) => Ok(self.client.kick_user(id, user, None).await?),
            ActiveConversation::AnonGroup(id) => {
                Ok(self.client.anon_groups().kick_member(id, user).await?)
            }
            _ => Err(ConversationError::Other(
                "only channels and groups have members to remove".into(),
            )),
        }
    }

    /// Hand this channel or private group to `new_owner`; we become an admin
    /// and may then leave. The last owner of a group with other members
    /// cannot leave without doing this first (`leave_group`).
    pub async fn transfer_ownership(&self, new_owner: &UserId) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::Channel(id) => Ok(self
                .client
                .transfer_channel_ownership(id, new_owner)
                .await?),
            ActiveConversation::AnonGroup(id) => Ok(self
                .client
                .anon_groups()
                .transfer_ownership(id, new_owner)
                .await?),
            _ => Err(ConversationError::Other(
                "only channels and groups have owners".into(),
            )),
        }
    }

    /// Block or unblock this private group on this device: a blocked group's
    /// content is dropped on receive (`process_incoming_content`).
    pub fn set_blocked(&self, blocked: bool) -> ConversationResult {
        let ActiveConversation::AnonGroup(id) = &self.conversation else {
            return Err(ConversationError::Other(
                "only private groups can be blocked here".into(),
            ));
        };
        let groups = self.client.anon_groups();
        Ok(if blocked {
            groups.block_group(id)?
        } else {
            groups.unblock_group(id)?
        })
    }

    /// Block or unblock `user` within this private group: their posts,
    /// reactions, typing, calls and profile shares are dropped; their leave
    /// still applies.
    pub fn set_member_blocked(&self, user: &UserId, blocked: bool) -> ConversationResult {
        let ActiveConversation::AnonGroup(id) = &self.conversation else {
            return Err(ConversationError::Other(
                "only private-group members can be blocked here".into(),
            ));
        };
        let groups = self.client.anon_groups();
        Ok(if blocked {
            groups.block_user_in_group(id, user, None)?
        } else {
            groups.unblock_user_in_group(id, user)?
        })
    }

    /// Delete a DM on this device (the sidebar's "Delete chat"). Local only;
    /// the peer is not told, as in Signal.
    ///
    /// A friend DM loses its history and stays listed (the friendship is what
    /// lists it); an anon DM goes entirely, its peer block staying; a channel
    /// we are no longer in goes entirely. Groups and live channels are
    /// refused: a membership outlives a local delete — the next sync or group
    /// message brings it back — so those are left (Channel Settings › Leave),
    /// never deleted here.
    pub async fn delete_conversation(&self) -> ConversationResult {
        let result = match &self.conversation {
            // A secret chat ends for both sides (Telegram-style).
            ActiveConversation::AnonDm(id) => {
                Ok(self.client.anon_dms().end_conversation(id).await?)
            }
            ActiveConversation::ServerDm { peer_id } => {
                // Channel-free DM: no channel/membership row to cascade —
                // delete the local message history directly, then the
                // attachments only this chat carried (row and file).
                let channel_id: ChannelId = (*peer_id).into();
                let messages = self
                    .client
                    .messages()
                    .get_messages_local(&channel_id, &ConversationType::ServerDm, None, u32::MAX)
                    .map_err(ConversationError::Message)?;
                let attachments: Vec<_> = messages
                    .iter()
                    .flat_map(|m| m.attachments.iter().copied())
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect();
                let ids: Vec<_> = messages.into_iter().map(|m| m.id).collect();
                let storage =
                    |e: crate::error::StorageError| ConversationError::Other(e.to_string());
                self.client
                    .messages()
                    .message_repo
                    .delete_batch(&ids)
                    .map_err(storage)?;
                self.client
                    .attachments()
                    .attachment_repo()
                    .delete_unreferenced(&attachments)
                    .map_err(storage)
            }
            // A channel we are no longer in: its history, then the channel.
            ActiveConversation::Channel(channel_id)
                if self.client.is_former_channel(channel_id) =>
            {
                let storage =
                    |e: crate::error::StorageError| ConversationError::Other(e.to_string());
                let messages = self
                    .client
                    .messages()
                    .get_messages_local(channel_id, &ConversationType::Channel, None, u32::MAX)
                    .map_err(ConversationError::Message)?;
                let attachments: Vec<_> = messages
                    .iter()
                    .flat_map(|m| m.attachments.iter().copied())
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect();
                let ids: Vec<_> = messages.into_iter().map(|m| m.id).collect();
                self.client
                    .messages()
                    .message_repo
                    .delete_batch(&ids)
                    .map_err(storage)?;
                self.client
                    .attachments()
                    .attachment_repo()
                    .delete_unreferenced(&attachments)
                    .map_err(storage)?;
                self.client
                    .channels()
                    .channel_repo
                    .delete_channel_cascade(channel_id)
                    .map_err(storage)
            }
            ActiveConversation::AnonGroup(id)
                if self.client.get_anon_group(id).is_some_and(|g| {
                    g.status == crate::domain::anon_group::AnonGroupStatus::Removed
                }) =>
            {
                let storage =
                    |e: crate::error::StorageError| ConversationError::Other(e.to_string());
                let channel_id: ChannelId = (*id).into();
                let messages = self
                    .client
                    .messages()
                    .get_messages_local(&channel_id, &ConversationType::AnonGroup, None, u32::MAX)
                    .map_err(ConversationError::Message)?;
                let attachments: Vec<_> = messages
                    .iter()
                    .flat_map(|m| m.attachments.iter().copied())
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect();
                let ids: Vec<_> = messages.into_iter().map(|m| m.id).collect();
                self.client
                    .messages()
                    .message_repo
                    .delete_batch(&ids)
                    .map_err(storage)?;
                self.client
                    .attachments()
                    .attachment_repo()
                    .delete_unreferenced(&attachments)
                    .map_err(storage)?;
                self.client.anon_groups().delete_removed_group(id)?;
                Ok(())
            }
            ActiveConversation::AnonGroup(_) | ActiveConversation::Channel(_) => Err(
                ConversationError::Other("a group is left, not deleted".into()),
            ),
            ActiveConversation::None => Err(ConversationError::NoConversation),
        };
        if result.is_ok() {
            self.client.clear_composer_conversation(self.conversation);
        }
        result
    }

    /// This conversation's disappearing-messages timer in seconds (`0` = off).
    ///
    /// Friend DMs hold it per conversation (`services::disappearing`); a
    /// channel's is its server-held `config.ttl`.
    pub fn disappearing_timer(&self) -> u32 {
        match &self.conversation {
            ActiveConversation::ServerDm { peer_id } => {
                crate::services::disappearing::server_dm_timer(
                    &self.client.channels().settings_repo,
                    peer_id,
                )
            }
            ActiveConversation::AnonDm(id) => crate::services::disappearing::anon_dm_state(
                &self.client.anon_dms().settings_repo,
                id,
            )
            .effective_seconds(crate::util::time::TimestampNanos::now().as_u64()),
            ActiveConversation::Channel(channel_id) => self
                .client
                .channels()
                .channel_repo
                .get(channel_id)
                .ok()
                .flatten()
                .and_then(|c| c.config.default_ttl_seconds)
                .unwrap_or(0),
            _ => 0,
        }
    }

    /// Persisted activation state; channels have an indefinite server-held TTL.
    pub fn disappearing_state(&self) -> crate::services::disappearing::DmTimer {
        match &self.conversation {
            ActiveConversation::ServerDm { peer_id } => {
                crate::services::disappearing::server_dm_state(
                    &self.client.channels().settings_repo,
                    peer_id,
                )
            }
            ActiveConversation::AnonDm(id) => crate::services::disappearing::anon_dm_state(
                &self.client.anon_dms().settings_repo,
                id,
            ),
            _ => crate::services::disappearing::DmTimer {
                seconds: self.disappearing_timer(),
                version: crate::services::disappearing::INITIAL_TIMER_VERSION,
                enabled_until: 0,
                activation_seconds: 0,
            },
        }
    }

    pub async fn set_disappearing_window(
        &self,
        seconds: u32,
        activation_seconds: u32,
    ) -> ConversationResult {
        let ActiveConversation::ServerDm { peer_id } = &self.conversation else {
            return Err(ConversationError::Other(
                "activation windows are available for friend DMs".into(),
            ));
        };
        let notice = self
            .client
            .messages()
            .set_server_dm_disappearing_window(peer_id, seconds, activation_seconds)
            .await
            .map_err(ConversationError::Message)?;
        if let Some(message) = notice {
            self.client
                .event_bus()
                .emit_domain(crate::inbox::handlers::DomainEvent::MessageReceived { message });
        }
        Ok(())
    }

    /// Friend and secret DMs let either participant change the timer. A
    /// channel's timer is fixed when it is created.
    pub fn can_set_disappearing_timer(&self) -> bool {
        matches!(
            self.conversation,
            ActiveConversation::ServerDm { .. } | ActiveConversation::AnonDm(_)
        )
    }

    /// Set this conversation's disappearing-messages timer (`0` = off).
    ///
    /// Friend and secret DMs adopt locally and send an encrypted timer update
    /// to the peer; secret chats also display the change as a system notice.
    ///
    /// Channel: writes the local copy of the channel's ttl only. The server
    /// holds the real value and refuses to change it after creation, so this
    /// exists for the V29 proof (`tests/validated.rs`), not for the UI —
    /// `can_set_disappearing_timer` is false for channels.
    pub async fn set_disappearing_timer(&self, seconds: u32) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                let service = self.client.anon_dms();
                let ready = service
                    .get_conversation(id)
                    .map_err(|e| ConversationError::Other(e.to_string()))?
                    .is_some_and(|dm| dm.is_ready());
                if !ready {
                    return Err(ConversationError::Other(
                        "secret chat is not active yet".into(),
                    ));
                }
                let me = service
                    .current_user_id()
                    .map_err(|e| ConversationError::Other(e.to_string()))?;
                let sent_at = crate::util::time::TimestampNanos::now().as_u64();
                let Some((notice, timer)) = crate::services::disappearing::change_anon_dm_timer(
                    &service.settings_repo,
                    &service.message_repo,
                    id,
                    &me,
                    seconds,
                    sent_at,
                )
                .map_err(|e| ConversationError::Other(e.to_string()))?
                else {
                    return Ok(());
                };
                self.client.event_bus().emit_domain(
                    crate::inbox::handlers::DomainEvent::MessageReceived { message: notice },
                );
                service
                    .send_message(
                        id,
                        crate::services::disappearing::timer_update_content(timer, sent_at),
                    )
                    .await
                    .map_err(|e| ConversationError::Other(e.to_string()))?;
                Ok(())
            }
            ActiveConversation::ServerDm { peer_id } => {
                let notice = self
                    .client
                    .messages()
                    .set_server_dm_disappearing_timer(peer_id, seconds)
                    .await
                    .map_err(ConversationError::Message)?;
                if let Some(message) = notice {
                    self.client.event_bus().emit_domain(
                        crate::inbox::handlers::DomainEvent::MessageReceived { message },
                    );
                }
                Ok(())
            }
            ActiveConversation::Channel(channel_id) => {
                let ttl = if seconds == 0 { None } else { Some(seconds) };
                self.client
                    .channels()
                    .channel_repo
                    .set_default_ttl(channel_id, ttl)
                    .map_err(|e| ConversationError::Other(e.to_string()))
            }
            ActiveConversation::AnonGroup(_) => Err(ConversationError::Other(
                "disappearing messages are not available in anonymous conversations".into(),
            )),
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }
}
