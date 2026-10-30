use super::*;

impl InboxEventProcessor {
    pub(super) async fn process_sync(&self, event: &InboxEvent) {
        match event {
            InboxEvent::SavedMessageSync { sender_id, sync } => {
                // A peer must never write into our Saved Messages.
                let own = self.message_service.current_user_id().ok();
                if own.as_ref() != Some(sender_id) {
                    tracing::warn!(sender_id = %sender_id, "SavedMessageSync from another user dropped");
                    return;
                }
                let Some(own) = own else { return };
                // The snapshot is applied to storage first; the events then
                // carry the stored row, the same ones a local edit or pin
                // emits, so state and UI take them on the one path.
                match self.message_service.apply_saved_message_sync(sync) {
                    Ok(outcome) => self.publish_saved_snapshot(own, outcome),
                    Err(e) => {
                        tracing::warn!(message_id = %sync.message_id, error = %e, "Failed to apply synced saved message")
                    }
                }
            }

            InboxEvent::SavedSnapshotApplied { sender_id, outcome } => {
                self.publish_saved_snapshot(*sender_id, outcome.clone());
            }

            InboxEvent::ReadSync { sender_id, sync } => {
                // A peer must never mark our messages read: only our own
                // devices send a ReadSync.
                if self.message_service.current_user_id().ok().as_ref() != Some(sender_id) {
                    tracing::warn!(sender_id = %sender_id, "ReadSync from another user dropped");
                    return;
                }
                match self.message_service.apply_read_sync(sync) {
                    Ok(conversation_ids) if !conversation_ids.is_empty() => {
                        self.event_bus.emit_domain(DomainEvent::ConversationsRead {
                            conversation_ids,
                            from_other_device: true,
                        });
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "ReadSync: marking messages read failed"),
                }
            }

            InboxEvent::ProfileKeySync { sender_id, sync } => {
                // A peer must never set our profile key: only our own
                // devices send a ProfileKeySync.
                if self.message_service.current_user_id().ok().as_ref() != Some(sender_id) {
                    tracing::warn!(sender_id = %sender_id, "ProfileKeySync from another user dropped");
                    return;
                }
                match crate::services::profile_visibility::apply_sync(
                    &self.message_service.settings_repo,
                    sync,
                ) {
                    Ok(changed) => tracing::info!(key_changed = changed, "ProfileKeySync applied"),
                    Err(e) => tracing::warn!(error = %e, "ProfileKeySync: storing the key failed"),
                }
            }

            InboxEvent::ConfigurationSync { sender_id, sync } => {
                // `apply` drops one that is not from our own user: a peer
                // must never change our privacy settings.
                match crate::services::configuration_sync::apply(
                    &self.message_service.settings_repo,
                    sender_id,
                    sync,
                ) {
                    Ok(true) => tracing::info!("ConfigurationSync applied"),
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "ConfigurationSync: storing the settings failed")
                    }
                }
            }

            InboxEvent::ChannelDeleteForMe {
                sender_id,
                conversation_id,
                target_author_aci,
                target_sent_timestamp,
                saved_message_id,
                saved_messages_scope,
            } => {
                let result = if *saved_messages_scope || !saved_message_id.is_empty() {
                    persist_saved_delete_for_me_sync(
                        &self.message_service.message_repo,
                        self.message_service.current_user_id().ok(),
                        *sender_id,
                        conversation_id,
                        target_author_aci,
                        *target_sent_timestamp,
                        saved_message_id,
                        *saved_messages_scope,
                    )
                    .map(|event| {
                        self.event_bus.emit_domain(event);
                    })
                } else {
                    admit_delete_for_me_sync(
                        &self.message_service.message_repo,
                        &self.event_bus,
                        self.message_service.current_user_id().ok(),
                        *sender_id,
                        conversation_id,
                        target_author_aci,
                        *target_sent_timestamp,
                    )
                };
                if let Err(error) = result {
                    tracing::warn!(%error, "DeleteForMeSync rejected");
                }
            }

            InboxEvent::ChannelReceipt {
                channel_id,
                sender_id,
                sender_device_id,
                receipt_type,
                timestamps,
            } => {
                if let Err(error) = self.message_service.apply_peer_receipt(
                    *channel_id,
                    self.message_service.conversation_type(channel_id),
                    *sender_id,
                    crate::domain::ids::DeviceId(*sender_device_id),
                    *receipt_type,
                    timestamps,
                ) {
                    tracing::warn!(%error, "Failed to persist channel receipt");
                }
            }

            InboxEvent::ChannelReceiptTargetsSynced {
                channel_id,
                timestamp,
            } => {
                if let (Ok(me), Ok(Some(message))) = (
                    self.message_service.current_user_id(),
                    self.message_service
                        .message_repo
                        .get_by_conversation_timestamp(channel_id, *timestamp),
                ) {
                    if message.sender_id == Some(me)
                        && message.conversation_type
                            == crate::domain::message::ConversationType::Channel
                    {
                        self.event_bus
                            .emit_domain(DomainEvent::MessageDeliveryChanged {
                                message_id: message.id,
                                channel_id: *channel_id,
                                status: message.delivery_status,
                            });
                    }
                }
            }

            _ => unreachable!("event routed to wrong inbox concern"),
        }
    }
    pub(super) fn publish_saved_snapshot(
        &self,
        own: UserId,
        outcome: crate::services::message::SavedSyncOutcome,
    ) {
        use crate::services::message::SavedSyncOutcome;
        match outcome {
            SavedSyncOutcome::Created(message) => {
                self.event_bus
                    .emit_domain(DomainEvent::MessageSnapshotChanged { message });
            }
            SavedSyncOutcome::Updated {
                message,
                body_changed,
                pin_changed,
                quote_hydrated,
            } => {
                if quote_hydrated {
                    self.event_bus.emit_domain(DomainEvent::MessageSnapshotChanged {
                        message: message.clone(),
                    });
                }
                if body_changed {
                    self.event_bus.emit_domain(DomainEvent::MessageEdited {
                        message_id: message.id,
                        new_content: message.content.clone(),
                        channel_id: message.conversation_id,
                        conversation_type: message.conversation_type.clone(),
                        author_id: own,
                        target_sent_timestamp: message.sent_at,
                    });
                }
                match pin_changed {
                    Some(true) => {
                        self.event_bus.emit_domain(DomainEvent::MessagePinned {
                            message_id: message.id,
                            pinner_id: own,
                            channel_id: message.conversation_id,
                            conversation_type: message.conversation_type,
                            target_sent_timestamp: message.sent_at,
                        });
                    }
                    Some(false) => {
                        self.event_bus.emit_domain(DomainEvent::MessageUnpinned {
                            message_id: message.id,
                            unpinner_id: own,
                            channel_id: message.conversation_id,
                            conversation_type: message.conversation_type,
                            target_sent_timestamp: message.sent_at,
                        });
                    }
                    None => {}
                }
            }
            SavedSyncOutcome::Unchanged => {}
        }
    }
}

/// The additive exact identity is exclusively an authenticated own Saved control.
pub(crate) fn persist_saved_delete_for_me_sync(
    repo: &crate::storage::MessageRepository,
    own: Option<UserId>,
    sender: UserId,
    conversation_id: &str,
    author: &str,
    sent_at: u64,
    message_id: &str,
    saved_scope: bool,
) -> Result<DomainEvent, crate::error::StorageError> {
    use crate::domain::ids::{ChannelId, MessageId};
    use crate::error::StorageError;
    let invalid = || StorageError::InvalidData("Invalid exact own Saved deletion".into());
    let own = own.filter(|own| *own == sender).ok_or_else(invalid)?;
    let conversation: ChannelId = conversation_id.parse().map_err(|_| invalid())?;
    let author: UserId = author.parse().map_err(|_| invalid())?;
    let id: MessageId = message_id.parse().map_err(|_| invalid())?;
    if !saved_scope
        || conversation != ChannelId::from(own)
        || author != own
        || sent_at == 0
        || sent_at > i64::MAX as u64
    {
        return Err(invalid());
    }
    // Transaction rechecks current persisted account and any existing row's
    // identity before recording a scoped tombstone, even when no row exists.
    repo.delete_own_saved_exact(&own, &id, sent_at)?;
    Ok(DomainEvent::DeleteForMeSync {
        conversation_id: conversation,
        target_author_id: own,
        target_message_id: id,
        timestamp: TimestampNanos::now().as_u64(),
        saved_deletion_persisted: true,
    })
}

/// A delete-for-me sync carries no storage-kind field (the sender,
/// `services/conversation.rs`, sends none). Resolve only within its exact ID,
/// author and clock, and refuse collisions rather than guessing a kind.
fn admit_delete_for_me_sync(
    repo: &crate::storage::MessageRepository,
    bus: &EventBus,
    own: Option<UserId>,
    sender: UserId,
    conversation_id: &str,
    author: &str,
    sent_at: u64,
) -> Result<(), crate::error::StorageError> {
    use crate::domain::message::ConversationType as CT;
    use crate::domain::reaction::ReactionScope;
    use crate::error::StorageError;
    let invalid = || StorageError::InvalidData("Invalid or ambiguous delete-for-me target".into());
    let own = own.filter(|own| *own == sender).ok_or_else(invalid)?;
    if sent_at == 0 || sent_at > i64::MAX as u64 {
        return Err(invalid());
    }
    let id: crate::domain::ids::ChannelId = conversation_id.parse().map_err(|_| invalid())?;
    let author: UserId = author.parse().map_err(|_| invalid())?;
    let mut target = None;
    // Unsupported kinds participate only in collision detection. They never
    // become deletion targets through this own-device server sync protocol.
    for kind in [
        CT::Channel,
        CT::ServerDm,
        CT::SavedMessages,
        CT::AnonDm,
        CT::AnonGroup,
        CT::Mesh,
        CT::MeshDm,
    ] {
        let scope = ReactionScope::new(id, kind);
        if let Some(message) = repo.get_by_reaction_scope(&scope, &author, sent_at)? {
            if target.is_some() {
                return Err(invalid());
            }
            target = Some(message);
        }
    }
    let Some(target) = target else {
        return Ok(());
    };
    match target.conversation_type {
        CT::Channel | CT::ServerDm => {}
        CT::SavedMessages if id == crate::domain::ids::ChannelId::from(own) && author == own => {}
        _ => return Err(invalid()),
    }
    bus.emit_domain(DomainEvent::DeleteForMeSync {
        conversation_id: id,
        target_author_id: author,
        target_message_id: target.id,
        timestamp: TimestampNanos::now().as_u64(),
        saved_deletion_persisted: false,
    });
    Ok(())
}

#[cfg(test)]
#[path = "../inbox_delete_for_me_tests.rs"]
mod inbox_delete_for_me_tests;
