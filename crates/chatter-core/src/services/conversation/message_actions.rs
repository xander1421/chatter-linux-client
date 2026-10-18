use super::*;

impl ConversationHandle {
    /// Send a pin/unpin and persist locally. A Saved pin is written here and
    /// snapshot-synced to our own devices only ([`Self::pin_saved_message`]);
    /// the other kinds send the control.
    pub async fn send_pin(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
        unpin: bool,
    ) -> ConversationResult {
        // One account snapshot resolves the scope and, for Saved, owns the
        // write: the scope's own-user and the writer are the same user.
        let auth = self.client.auth_state().get();
        let scope = self.storage_scope_as(auth.current_user_id)?;
        if scope.kind == ConversationType::SavedMessages {
            let own = auth.current_user_id.ok_or_else(|| {
                ConversationError::Other("Pinning requires a signed-in user".into())
            })?;
            return self
                .pin_saved_message(
                    &scope,
                    own,
                    auth.session_revision,
                    target_author,
                    target_sent_timestamp,
                    unpin,
                )
                .await;
        }
        let new_pinned = !unpin;
        // Sub-services on the proto side still take &str.
        let target_author_str = target_author.to_string();
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                match self.client.anon_dms().set_message_pinned(
                    id,
                    target_sent_timestamp,
                    new_pinned,
                ) {
                    Ok(false) => {
                        tracing::warn!(conversation_id = %id, target_sent_timestamp, "AnonDm pin: 0 rows affected")
                    }
                    Err(e) => {
                        tracing::warn!(conversation_id = %id, error = %e, "AnonDm pin: local persist failed")
                    }
                    _ => {}
                }
                self.client
                    .anon_dms()
                    .send_pin(id, &target_author_str, target_sent_timestamp, unpin)
                    .await?;
                Ok(())
            }
            ActiveConversation::AnonGroup(id) => {
                match self.client.anon_groups().set_message_pinned(
                    id,
                    target_sent_timestamp,
                    new_pinned,
                ) {
                    Ok(false) => {
                        tracing::warn!(group_id = %id, target_sent_timestamp, "AnonGroup pin: 0 rows affected")
                    }
                    Err(e) => {
                        tracing::warn!(group_id = %id, error = %e, "AnonGroup pin: local persist failed")
                    }
                    _ => {}
                }
                self.client
                    .anon_groups()
                    .send_pin(id, &target_author_str, target_sent_timestamp, unpin)
                    .await?;
                Ok(())
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
            _ => {
                let msg = self.lookup_message(target_author, target_sent_timestamp)?;
                let author_aci = msg.sender_id.map(|id| id.to_string()).unwrap_or_default();
                let content = crate::services::build_pin_content_with_channel(
                    &author_aci,
                    target_sent_timestamp,
                    unpin,
                    self.active_channel_id().as_deref(),
                );
                self.dispatch_content(content).await?;
                let _ = self
                    .client
                    .messages()
                    .set_message_pinned(&msg.id, new_pinned);
                Ok(())
            }
        }
    }

    /// Pin or unpin one of our Saved records, then sync it. The stored row
    /// is read in the Saved scope, gated by [`pinned_saved_record`], and its
    /// pin columns written at that key together with the record's pin clock
    /// (`SavedSyncRepository::apply_local_pin`: one transaction, never the
    /// whole row, so a body edit landing in between is kept); a storage
    /// failure or a vanished row is this call's error, never a silent
    /// success. Nothing goes to the server — the Saved id is our own user
    /// id, not a channel a pin control may be sent to. The row as stored
    /// then goes to our linked devices as a `SavedMessageSync` snapshot
    /// (`MessageService::sync_saved_record`, durable retry: a device that was
    /// unreachable remains queued for lifecycle recovery). The emitted
    /// `MessagePinned`/`MessageUnpinned` carries the real row id and the
    /// `SavedMessages` kind, so a same-id channel or DM view never takes it.
    /// `own` and `session_revision` are the snapshot the scope was resolved
    /// under; the write happens under the same ones or not at all.
    pub(super) async fn pin_saved_message(
        &self,
        scope: &crate::domain::reaction::ReactionScope,
        own: UserId,
        session_revision: u64,
        target_author: &UserId,
        target_sent_timestamp: u64,
        unpin: bool,
    ) -> ConversationResult {
        use crate::events::DomainEvent;
        use crate::storage::repositories::{PinWrite, SavedSyncRepository};
        if *target_author != own {
            return Err(ConversationError::Other(
                "Only your own Saved message can be pinned".into(),
            ));
        }
        let messages = self.client.messages();
        let repo = &messages.message_repo;
        let stored = scoped_stored_message(repo, scope, &own, target_sent_timestamp)?;
        let row = pinned_saved_record(&stored, own, !unpin)?;
        let saved_session =
            crate::services::gateway::submission::SendSession::capture(self.client.auth_state());
        let now = self.client.auth_state().get();
        if now.current_user_id != Some(own) || now.session_revision != session_revision {
            return Err(ConversationError::Other(
                "Account changed while pinning".into(),
            ));
        }
        let now_ns = crate::util::time::TimestampNanos::now().as_u64();
        match crate::services::gateway::submission::SEND_SESSION.sync_scope(
            saved_session.clone(),
            || {
                SavedSyncRepository::new(repo.db()).apply_local_pin(
                    scope,
                    &own,
                    target_sent_timestamp,
                    &row.id,
                    row.is_pinned,
                    now_ns,
                )
            },
        )? {
            PinWrite::Applied { .. } => {}
            PinWrite::NoRow | PinWrite::Stale => {
                return Err(ConversationError::Other(
                    "Saved message is no longer there".into(),
                ));
            }
        }
        let event = if unpin {
            DomainEvent::MessageUnpinned {
                message_id: row.id,
                unpinner_id: own,
                channel_id: scope.id,
                conversation_type: ConversationType::SavedMessages,
                target_sent_timestamp,
            }
        } else {
            DomainEvent::MessagePinned {
                message_id: row.id,
                pinner_id: own,
                channel_id: scope.id,
                conversation_type: ConversationType::SavedMessages,
                target_sent_timestamp,
            }
        };
        self.client.event_bus().emit_domain(event);
        // The row as it is now stored, pin clock included, is what siblings get.
        match repo.get(&row.id) {
            Ok(Some(written)) => {
                crate::services::gateway::submission::SEND_SESSION
                    .scope(saved_session, messages.sync_saved_record(&written))
                    .await;
            }
            Ok(None) => {
                tracing::warn!(message_id = %row.id, "Saved local write succeeded but sync reread found no row; snapshot not sent")
            }
            Err(error) => {
                tracing::error!(message_id = %row.id, %error, "Saved local write succeeded but sync reread failed; snapshot not sent")
            }
        }
        Ok(())
    }

    /// Send an edit and persist locally.
    ///
    /// Emits `DomainEvent::MessageEdited` on success so UI subscribers
    /// refresh uniformly across conversation kinds — no UI-side branching
    /// on anon vs DM/channel needed.
    ///
    /// `target_author` is the key the menu was opened with. A Saved edit
    /// refuses any author but the signed-in user before looking anything up,
    /// so a stale or malformed menu key cannot pick a Saved row by clock
    /// alone. The other kinds keep their own-message lookup (`persist_edit`)
    /// and the peer's authorship check; an anonymous group's menu key is not
    /// byte-identical to its stored sender id (see `stored_message`).
    pub async fn send_edit(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
        new_text: &str,
    ) -> ConversationResult {
        let scope = self.storage_scope()?;
        if scope.kind == ConversationType::SavedMessages {
            if self.client.auth_state().get().current_user_id != Some(*target_author) {
                return Err(ConversationError::Other(
                    "Only your own Saved message can be edited".into(),
                ));
            }
            return self
                .edit_saved_message(&scope, target_sent_timestamp, new_text)
                .await;
        }
        let result = self.send_edit_inner(target_sent_timestamp, new_text).await;
        if result.is_ok() {
            self.emit_local_edit_event(target_sent_timestamp, new_text);
        }
        result
    }

    /// Edit one of our Saved records in place, then sync it. The stored row
    /// is read in the Saved scope, rewritten by [`edited_saved_record`] and
    /// its body columns written back with a monotonic clock in one transaction,
    /// so a storage failure is this call's error, never a silent success.
    /// Nothing goes to the server — the Saved id is our own user id, not a
    /// channel a sender-key control may be sent to. The row as stored then
    /// goes to our linked devices as a `SavedMessageSync` snapshot whose
    /// `edited_at` orders it against their copy
    /// (`MessageService::sync_saved_record`, durable retry: a device that was
    /// unreachable remains queued for lifecycle recovery). The emitted `MessageEdited`
    /// carries the real row id and the `SavedMessages` kind, so a same-id
    /// channel or DM row is never patched.
    pub(super) async fn edit_saved_message(
        &self,
        scope: &crate::domain::reaction::ReactionScope,
        target_sent_timestamp: u64,
        new_text: &str,
    ) -> ConversationResult {
        let auth = self.client.auth_state().get();
        let own = auth
            .current_user_id
            .ok_or_else(|| ConversationError::Other("Editing requires a signed-in user".into()))?;
        let messages = self.client.messages();
        let repo = &messages.message_repo;
        let stored = scoped_stored_message(repo, scope, &own, target_sent_timestamp)?;
        let edited = edited_saved_record(&stored, own, new_text)?;
        // The scope was resolved under this account and session; the write
        // happens under the same ones or not at all.
        let saved_session =
            crate::services::gateway::submission::SendSession::capture(self.client.auth_state());
        let now = self.client.auth_state().get();
        if now.current_user_id != Some(own) || now.session_revision != auth.session_revision {
            return Err(ConversationError::Other(
                "Account changed while editing".into(),
            ));
        }
        // Body columns only, at the Saved key: a pin of this row landing
        // between the read above and this write keeps its pin.
        if crate::services::gateway::submission::SEND_SESSION
            .sync_scope(saved_session.clone(), || {
                repo.write_saved_body(
                    scope,
                    &own,
                    target_sent_timestamp,
                    &edited.id,
                    new_text.trim(),
                    crate::util::time::TimestampNanos::now().as_u64(),
                    true,
                )
            })?
            .is_none()
        {
            return Err(ConversationError::Other(
                "Saved message is no longer there".into(),
            ));
        }
        self.client
            .event_bus()
            .emit_domain(crate::events::DomainEvent::MessageEdited {
                message_id: edited.id,
                new_content: edited.content,
                channel_id: scope.id,
                conversation_type: ConversationType::SavedMessages,
                author_id: own,
                target_sent_timestamp,
            });
        // The row as it is now stored, edit clock included, is what siblings get.
        match repo.get(&edited.id) {
            Ok(Some(written)) => {
                crate::services::gateway::submission::SEND_SESSION
                    .scope(saved_session, messages.sync_saved_record(&written))
                    .await;
            }
            Ok(None) => {
                tracing::warn!(message_id = %edited.id, "Saved local write succeeded but sync reread found no row; snapshot not sent")
            }
            Err(error) => {
                tracing::error!(message_id = %edited.id, %error, "Saved local write succeeded but sync reread failed; snapshot not sent")
            }
        }
        Ok(())
    }

    pub(super) fn emit_local_edit_event(&self, target_sent_timestamp: u64, new_text: &str) {
        use crate::domain::ids::{ChannelId, MessageId};
        use crate::domain::message::{ConversationType as CT, MessageContent};
        use crate::events::DomainEvent;

        let (channel_id, conversation_type) = match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                let cid: ChannelId = (*id).into();
                (cid, CT::AnonDm)
            }
            ActiveConversation::AnonGroup(id) => {
                let cid: ChannelId = (*id).into();
                (cid, CT::AnonGroup)
            }
            ActiveConversation::ServerDm { peer_id } => (ChannelId::from(*peer_id), CT::ServerDm),
            ActiveConversation::Channel(channel_id) => (*channel_id, CT::Channel),
            ActiveConversation::None => return,
        };
        let Some(author_id) = self.client.auth_state().get().current_user_id else {
            return;
        };

        self.client
            .event_bus()
            .emit_domain(DomainEvent::MessageEdited {
                message_id: MessageId::new(),
                new_content: MessageContent::Text(new_text.to_string()),
                channel_id,
                conversation_type,
                author_id,
                target_sent_timestamp,
            });
    }

    pub(super) async fn send_edit_inner(
        &self,
        target_sent_timestamp: u64,
        new_text: &str,
    ) -> ConversationResult {
        self.require_remote_message_actions()?;
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                // anon_dms().send_edit() persists internally
                self.client
                    .anon_dms()
                    .send_edit(id, target_sent_timestamp, new_text)
                    .await?;
                Ok(())
            }
            ActiveConversation::AnonGroup(id) => {
                // anon_groups().send_edit() persists internally
                self.client
                    .anon_groups()
                    .send_edit(id, target_sent_timestamp, new_text)
                    .await?;
                Ok(())
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
            _ => {
                let content = crate::services::gateway::builders::build_edit_content_with_channel(
                    target_sent_timestamp,
                    new_text,
                    self.active_channel_id().as_deref(),
                );
                self.dispatch_content(content).await?;
                self.persist_edit(target_sent_timestamp, new_text);
                Ok(())
            }
        }
    }

    /// Prepare the same scoped control the send path dispatches, from a locally
    /// selected stored record. The peer never resolves our local timestamp.
    pub(crate) fn prepare_delete_for_everyone(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> ConversationResult<(crate::domain::Message, crate::proto::content_v1::Content)> {
        let target = self.stored_message(target_author, target_sent_timestamp)?;
        if !self.can_delete_for_everyone(target_author, target_sent_timestamp)? {
            return Err(ConversationError::Other(
                "cannot delete this message for everyone".into(),
            ));
        }
        let content = if matches!(
            target.conversation_type,
            ConversationType::ServerDm | ConversationType::AnonDm
        ) {
            crate::services::gateway::builders::build_stored_dm_delete_content(&target)?
        } else {
            crate::services::gateway::builders::build_targeted_delete_content(
                target_sent_timestamp,
                Some(&target_author.to_string()),
                self.active_channel_id().as_deref(),
            )
        };
        Ok((target, content))
    }

    /// Send a delete-for-everyone and persist locally.
    ///
    /// `target_author` is the message's sender. In a 1:1 DM this may be the
    /// peer; channels still only succeed for our own messages.
    pub async fn send_delete(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> ConversationResult {
        let (target, prepared_content) =
            self.prepare_delete_for_everyone(target_author, target_sent_timestamp)?;
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                self.client
                    .anon_dms()
                    .send_targeted_delete_for_everyone(id, target_author, target_sent_timestamp)
                    .await?;
                let _ = self.client.anon_dms().delete_message_by_timestamp(
                    id,
                    target_author,
                    target_sent_timestamp,
                );
                Ok(())
            }
            ActiveConversation::AnonGroup(id) => {
                self.client
                    .anon_groups()
                    .send_targeted_delete_for_everyone(id, target_author, target_sent_timestamp)
                    .await?;
                let _ = self.client.anon_groups().delete_message_by_timestamp(
                    id,
                    target_author,
                    target_sent_timestamp,
                );
                Ok(())
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
            _ => {
                let content = prepared_content;
                self.dispatch_content(content).await?;
                if target.conversation_type == ConversationType::ServerDm
                    && crate::storage::repositories::message_modifier_repo::canonical_call_record_id(&target).is_none()
                {
                    if let Some(author) = target.sender_id {
                        crate::storage::repositories::message_modifier_repo::MessageModifierRepository::new(self.client.messages().database.clone())
                            .queue_dm_delete(&author, target.sent_at, Some(target.conversation_id), crate::util::time::TimestampNanos::now().as_u64())?;
                    }
                }
                if let Some(call_id) =
                    crate::storage::repositories::message_modifier_repo::canonical_call_record_id(
                        &target,
                    )
                {
                    crate::storage::repositories::message_modifier_repo::apply_call_delete(
                        self.client.messages().database.clone(),
                        &self.client.messages().message_repo,
                        &crate::domain::reaction::ReactionScope::of(&target),
                        &call_id.to_string(),
                    )?;
                } else {
                    // Explicit delete-for-everyone: a still-unconfirmed channel
                    // send may now remove its server copy (`channel_send.rs`).
                    self.client
                        .messages()
                        .delete_channel_for_everyone_local(&target.id)?;
                }
                Ok(())
            }
        }
    }

    pub(super) fn deletion_retry_scope(&self) -> ConversationResult<String> {
        Ok(match self.conversation {
            ActiveConversation::Channel(id) => format!("channel:{id}"),
            ActiveConversation::ServerDm { peer_id } => format!("server_dm:{peer_id}"),
            ActiveConversation::AnonDm(id) => format!("anon_dm:{id}"),
            ActiveConversation::AnonGroup(id) => format!("anon_group:{id}"),
            ActiveConversation::None => return Err(ConversationError::NoConversation),
        })
    }

    /// Persist the immutable original batch before any remote action. Recovery
    /// uses exact saved controls and minimal cleanup identities, never live rows.
    pub async fn send_delete_batch(&self, targets: &[(UserId, u64)]) -> ConversationResult {
        let auth = self.client.auth_state().get();
        let account = auth
            .current_user_id
            .ok_or(crate::services::MessageError::NotLoggedIn)?;
        self.send_delete_batch_in_session(targets, Some((account, auth.session_revision)))
            .await
    }

    /// UI callers pass the account/revision captured at the original click,
    /// before spawning a task, so a delayed first poll cannot adopt a new login.
    pub async fn send_delete_batch_fenced(
        &self,
        targets: &[(UserId, u64)],
        account: UserId,
        revision: u64,
    ) -> ConversationResult {
        self.send_delete_batch_in_session(targets, Some((account, revision)))
            .await
    }

    /// Delete a display-ordered selection under the login that submitted it.
    /// Confirmed local deletions are emitted immediately so a presenter can
    /// remove each row; for-everyone emits only after the whole batch succeeds.
    pub async fn delete_selected(
        &self,
        ids: &[String],
        scope: SelectedDeleteScope,
        account: UserId,
        revision: u64,
        completed: tokio::sync::mpsc::UnboundedSender<Vec<(UserId, u64)>>,
    ) -> SelectedDeleteResult {
        if self.require_deletion_session(&account, revision).is_err() {
            return SelectedDeleteResult::fail_all(
                ids,
                SelectedDeleteFailureReason::SessionChanged,
            );
        }
        let Some(targets) = self.client.selected_message_targets(self.conversation, ids) else {
            return SelectedDeleteResult::fail_all(ids, SelectedDeleteFailureReason::Unavailable);
        };
        if scope == SelectedDeleteScope::ForEveryone
            && !targets
                .iter()
                .all(|(author, ts)| self.can_delete_for_everyone(author, *ts).unwrap_or(false))
        {
            return SelectedDeleteResult::fail_all(ids, SelectedDeleteFailureReason::NotEligible);
        }
        if scope == SelectedDeleteScope::ForEveryone {
            return match self
                .send_delete_batch_fenced(&targets, account, revision)
                .await
            {
                Ok(()) => {
                    let _ = completed.send(targets);
                    SelectedDeleteResult {
                        deleted_ids: ids.to_vec(),
                        ..Default::default()
                    }
                }
                Err(error) => SelectedDeleteResult::fail_all(
                    ids,
                    SelectedDeleteFailureReason::Operation(error),
                ),
            };
        }

        let mut result = SelectedDeleteResult::default();
        for (index, ((author, timestamp), id)) in targets.iter().zip(ids).enumerate() {
            let deletion = if self.require_deletion_session(&account, revision).is_err() {
                Err(SelectedDeleteFailureReason::SessionChanged)
            } else {
                self.delete_for_me(author, *timestamp)
                    .await
                    .map_err(SelectedDeleteFailureReason::Operation)
            };
            if let Err(reason) = deletion {
                result.failed_ids.push(SelectedDeleteFailure {
                    id: id.clone(),
                    reason,
                });
                result
                    .failed_ids
                    .extend(
                        ids[index + 1..]
                            .iter()
                            .cloned()
                            .map(|id| SelectedDeleteFailure {
                                id,
                                reason: SelectedDeleteFailureReason::NotAttempted,
                            }),
                    );
                result.partial = !result.deleted_ids.is_empty();
                break;
            }
            result.deleted_ids.push(id.clone());
            let _ = completed.send(vec![(*author, *timestamp)]);
        }
        result
    }

    pub(super) async fn send_delete_batch_in_session(
        &self,
        targets: &[(UserId, u64)],
        expected: Option<(UserId, u64)>,
    ) -> ConversationResult {
        use crate::storage::repositories::delete_batch_retry_repo::{
            DELETE_BATCH_LOCK, DeleteBatchRetryRepository, PreparedBatch, PreparedDelete,
        };
        use prost::Message as _;
        if targets.is_empty() {
            return Ok(());
        }
        if targets.len() > 500 {
            return Err(ConversationError::Other(
                "at most 500 messages may be deleted at once".into(),
            ));
        }
        let _execution = DELETE_BATCH_LOCK.lock().await;
        let auth = self.client.auth_state().get();
        let account = auth
            .current_user_id
            .ok_or(crate::services::MessageError::NotLoggedIn)?;
        let revision = auth.session_revision;
        if self.client.messages().current_user_id()? != account
            || expected.is_some_and(|e| e != (account, revision))
        {
            return Err(crate::services::MessageError::NotLoggedIn.into());
        }
        let scope = self.deletion_retry_scope()?;
        let journal = DeleteBatchRetryRepository::new(self.client.messages().database.clone());
        let selection = DeleteBatchRetryRepository::selection_key(targets)?;
        let mut batch = if let Some(saved) = journal.get(&account, &scope, &selection)? {
            saved
        } else {
            let mut seen = std::collections::HashSet::new();
            let mut prepared = Vec::with_capacity(targets.len());
            for &(author, timestamp) in targets {
                if !seen.insert((author, timestamp)) {
                    continue;
                }
                let (message, content) = self.prepare_delete_for_everyone(&author, timestamp)?;
                prepared.push(PreparedDelete {
                    author,timestamp,message_id:message.id,conversation_id:message.conversation_id,
                    conversation_type:message.conversation_type.clone(),
                    call_id:crate::storage::repositories::message_modifier_repo::canonical_call_record_id(&message).map(|id|id.to_string()),
                    control:content.encode_to_vec(),control_sent:false,local_completed:false,
                });
            }
            let batch = PreparedBatch {
                id: uuid::Uuid::new_v4().to_string(),
                targets: prepared,
                server_committed: false,
            };
            journal.insert(&account, &scope, &selection, &batch)?;
            batch
        };
        // Cancel all remaining network/local work if logout or a new login
        // changes the account/session; recovery never adopts another account.
        let result = tokio::select! {
            biased;
            _ = self.deletion_session_changed(account,revision) => Err(crate::services::MessageError::NotLoggedIn.into()),
            result = tokio::time::timeout(std::time::Duration::from_secs(120), self.execute_prepared_delete_batch(&account,revision,&scope,&journal,&mut batch)) => {
                match result { Ok(result) => result, Err(_) => Err(crate::services::MessageError::Network(crate::error::NetworkError::Timeout).into()) }
            },
        };
        if let Err(error) = &result {
            let permanent = matches!(
                error,
                ConversationError::Message(crate::services::MessageError::Network(
                    crate::error::NetworkError::PermissionDenied
                        | crate::error::NetworkError::InvalidRequest(_)
                        | crate::error::NetworkError::DataLoss(_)
                )) | ConversationError::AnonDm(
                    crate::services::AnonDmError::ConversationBlocked(_)
                        | crate::services::AnonDmError::UserBlocked(_)
                        | crate::services::AnonDmError::ConversationNotFound(_)
                        | crate::services::AnonDmError::DeliveryUnconfirmed
                ) | ConversationError::AnonGroup(
                    crate::services::AnonGroupError::InsufficientPermissions(_)
                        | crate::services::AnonGroupError::GroupBlocked(_)
                        | crate::services::AnonGroupError::UserBlocked(_)
                        | crate::services::AnonGroupError::GroupNotFound(_)
                        | crate::services::AnonGroupError::DeliveryUnconfirmed
                )
            );
            journal.defer(&account, &scope, &batch.id, permanent)?;
        }
        result
    }

    pub(super) fn require_deletion_session(
        &self,
        account: &UserId,
        revision: u64,
    ) -> ConversationResult {
        let state = self.client.auth_state().get();
        if state.current_user_id != Some(*account) || state.session_revision != revision {
            return Err(crate::services::MessageError::NotLoggedIn.into());
        }
        Ok(())
    }

    pub(super) async fn deletion_session_changed(&self, account: UserId, revision: u64) {
        let mut changes = self.client.auth_state().subscribe();
        loop {
            let state = self.client.auth_state().get();
            if state.current_user_id != Some(account) || state.session_revision != revision {
                return;
            }
            if changes.recv().await.is_err() {
                return;
            }
        }
    }

    pub(super) async fn execute_prepared_delete_batch(
        &self,
        account: &UserId,
        revision: u64,
        scope: &str,
        journal: &crate::storage::repositories::delete_batch_retry_repo::DeleteBatchRetryRepository,
        batch: &mut crate::storage::repositories::delete_batch_retry_repo::PreparedBatch,
    ) -> ConversationResult {
        use prost::Message as _;
        self.require_deletion_session(account, revision)?;
        if !batch.server_committed {
            // Preserve the entire original selected set in this one RPC. Never
            // combine journal rows from different submissions or split a batch.
            let server_targets = batch
                .targets
                .iter()
                .filter(|_| matches!(self.conversation, ActiveConversation::Channel(_)))
                .map(|t| (t.message_id.to_string(), t.conversation_id.to_string()))
                .collect::<Vec<_>>();
            if !server_targets.is_empty() {
                self.client
                    .messages()
                    .message_client()
                    .delete_messages(server_targets)
                    .await
                    .map_err(crate::services::MessageError::from)?;
            }
            self.require_deletion_session(account, revision)?;
            batch.server_committed = true;
            journal.progress(account, scope, batch)?;
        }
        for i in 0..batch.targets.len() {
            self.require_deletion_session(account, revision)?;
            let target = &batch.targets[i];
            if target.control_sent || target.local_completed {
                continue;
            }
            let content = crate::proto::content_v1::Content::decode(target.control.as_slice())
                .map_err(|e| {
                    ConversationError::Other(format!("invalid prepared deletion control: {e}"))
                })?;
            match &self.conversation {
                ActiveConversation::AnonDm(id) => {
                    self.client.anon_dms().send_message(id, content).await?
                }
                ActiveConversation::AnonGroup(id) => {
                    self.client
                        .anon_groups()
                        .send_ephemeral_message(id, content)
                        .await?
                }
                _ => self.dispatch_content(content).await?,
            }
            self.require_deletion_session(account, revision)?;
            batch.targets[i].control_sent = true;
            batch.targets[i].control.clear();
            journal.progress(account, scope, batch)?;
        }
        for i in 0..batch.targets.len() {
            self.require_deletion_session(account, revision)?;
            if batch.targets[i].local_completed {
                continue;
            }
            self.complete_prepared_delete(&batch.targets[i])?;
            batch.targets[i].local_completed = true;
            journal.progress(account, scope, batch)?;
        }
        self.require_deletion_session(account, revision)?;
        journal.finish(account, scope, batch)?;
        for target in &batch.targets {
            self.client
                .event_bus()
                .emit_domain(crate::events::DomainEvent::MessageDeleted {
                    message_id: target.message_id,
                    author_id: target.author,
                    channel_id: Some(target.conversation_id),
                    target_sent_timestamp: target.timestamp,
                });
        }
        Ok(())
    }

    pub(super) fn complete_prepared_delete(
        &self,
        target: &crate::storage::repositories::delete_batch_retry_repo::PreparedDelete,
    ) -> ConversationResult {
        use crate::storage::repositories::message_modifier_repo::{
            MessageModifierRepository, apply_call_delete,
        };
        if let Some(call_id) = &target.call_id {
            apply_call_delete(
                self.client.messages().database.clone(),
                &self.client.messages().message_repo,
                &crate::domain::reaction::ReactionScope::new(
                    target.conversation_id,
                    target.conversation_type.clone(),
                ),
                call_id,
            )?;
            return Ok(());
        }
        if matches!(
            target.conversation_type,
            ConversationType::ServerDm | ConversationType::AnonDm
        ) {
            MessageModifierRepository::new(self.client.messages().database.clone())
                .queue_typed_dm_delete(
                    &target.author,
                    target.timestamp,
                    Some(target.conversation_id),
                    target.conversation_type.clone(),
                    crate::util::time::TimestampNanos::now().as_u64(),
                )?;
        }
        if let ActiveConversation::AnonGroup(id) = self.conversation {
            self.client.anon_groups().delete_message_by_timestamp(
                &id,
                &target.author,
                target.timestamp,
            )?;
            return Ok(());
        }
        match self
            .client
            .messages()
            .delete_channel_for_everyone_local(&target.message_id)
        {
            Ok(())
            | Err(crate::services::MessageError::Storage(crate::error::StorageError::NotFound(
                _,
            ))) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Delete-for-me: delete locally + sync to own devices.
    pub async fn delete_for_me(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> ConversationResult {
        if matches!(self.conversation, ActiveConversation::Channel(_))
            && self.storage_scope()?.kind == ConversationType::SavedMessages
        {
            let session = crate::services::gateway::submission::SendSession::capture(
                self.client.auth_state(),
            );
            let auth = self.client.auth_state().get();
            let owner = auth
                .current_user_id
                .ok_or(crate::services::MessageError::NotLoggedIn)?;
            if *target_author != owner {
                return Err(crate::services::MessageError::NotAuthor.into());
            }
            if target_sent_timestamp == 0 || target_sent_timestamp > i64::MAX as u64 {
                return Err(ConversationError::Other(
                    "Invalid Saved deletion timestamp".into(),
                ));
            }
            let messages = self.client.messages();
            let journal =
                crate::storage::repositories::SavedSyncRepository::new(messages.database.clone());
            let (id, prepare) = match journal.deleted_identity(&owner, target_sent_timestamp)? {
                Some(id) => (id, false),
                None => (
                    self.lookup_message(target_author, target_sent_timestamp)?
                        .id,
                    true,
                ),
            };
            let acknowledged = crate::services::gateway::submission::SEND_SESSION
                .scope(
                    session,
                    messages.submit_saved_delete(owner, id, target_sent_timestamp, prepare),
                )
                .await?;
            let now = self.client.auth_state().get();
            if acknowledged
                && now.current_user_id == Some(owner)
                && now.session_revision == auth.session_revision
            {
                return Ok(());
            }
            return Err(ConversationError::Other(
                "Saved deletion was not confirmed on linked devices; retry when connected".into(),
            ));
        }
        match &self.conversation {
            ActiveConversation::AnonDm(id) => self
                .client
                .anon_dms()
                .delete_message_by_timestamp(id, target_author, target_sent_timestamp)
                .map(|_| ())
                .map_err(|e| ConversationError::Other(e.to_string())),
            ActiveConversation::AnonGroup(id) => self
                .client
                .anon_groups()
                .delete_message_by_timestamp(id, target_author, target_sent_timestamp)
                .map(|_| ())
                .map_err(|e| ConversationError::Other(e.to_string())),
            ActiveConversation::None => Err(ConversationError::NoConversation),
            _ => {
                let msg = self.lookup_message(target_author, target_sent_timestamp)?;
                let author_aci = msg.sender_id.map(|id| id.to_string()).unwrap_or_default();
                let content = crate::services::build_delete_for_me_content(
                    &self.dm_or_channel_id(),
                    &author_aci,
                    target_sent_timestamp,
                );
                // Our own other devices only: a peer's (or channel member's)
                // device would apply it and delete its own copy.
                let messages = self.client.messages();
                messages
                    .sync_to_own_devices(&mut messages.auth_client(), content)
                    .await;
                messages
                    .delete_message_local(&msg.id)
                    .map_err(ConversationError::Message)
            }
        }
    }
}
