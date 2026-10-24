use super::*;

/// Exact Saved controls must commit before the encrypted inbox envelope is
/// acknowledged. Storage failures retain it for retry; malformed/foreign
/// controls are permanently rejected without touching any message.
pub(super) fn persist_exact_saved_delete_before_ack(
    inner: &content::Content,
    sender: UserId,
    gateway: &MessageGateway,
) -> Result<(), ServiceError> {
    use crate::services::current_user::CurrentUserProvider;
    if let content::Content::DeleteForMeSync(sync) = inner
        && (sync.saved_messages_scope || !sync.saved_message_id.is_empty())
    {
        let own = gateway.settings_repo.current_user_id()?;
        crate::services::inbox_processor::sync::persist_saved_delete_for_me_sync(
            &gateway.message_repo,
            Some(own),
            sender,
            &sync.conversation_id,
            &sync.target_author_aci,
            sync.target_sent_timestamp,
            &sync.saved_message_id,
            sync.saved_messages_scope,
        )
        .map_err(ServiceError::Storage)?;
    }
    Ok(())
}

/// Dispatch channel control content based on type.
///
/// A secret-chat bootstrap sealed to this device inbox becomes its request or
/// invite event, bound to the device that sent it (`sender_device_id`, from
/// the sender certificate): only this device received it, so only this device
/// can accept it.
pub(super) fn dispatch_secret_chat_bootstrap(
    inner: content::Content,
    sender_id: UserId,
    sender_device_id: u32,
    event_tx: &broadcast::Sender<InboxEvent>,
    anon_group_repo: &crate::storage::repositories::AnonGroupRepository,
) -> Result<(), crate::error::StorageError> {
    let event = match inner {
        content::Content::AnonDmRequest(request)
            if request.disappearing_seconds
                == crate::services::disappearing::SECRET_CHAT_DEFAULT_SECONDS =>
        {
            InboxEvent::AnonDmRequest {
                peer_id: sender_id,
                sender_device_id,
                conversation_id: request.conversation_id,
                initiator_mailbox_pk: request.initiator_mailbox_pk,
                intro_message: (!request.intro_message.is_empty()).then_some(request.intro_message),
                profile_key: request.profile_key,
                encrypted_display_name: request.encrypted_display_name,
                avatar: request.avatar,
                timestamp: request.timestamp,
            }
        }
        content::Content::AnonGroupInvite(invite) => InboxEvent::AnonGroupInvite {
            peer_id: sender_id,
            sender_device_id,
            group_id: invite.group_id,
            group_name: invite.group_name,
            inviter_mailbox_pk: invite.inviter_mailbox_pk,
            timestamp: invite.timestamp,
            profile_key: invite.profile_key,
            encrypted_display_name: invite.encrypted_display_name,
            avatar: invite.avatar,
        },
        _ => return Ok(()),
    };
    if let InboxEvent::AnonGroupInvite {
        peer_id,
        sender_device_id,
        group_id,
        group_name,
        inviter_mailbox_pk,
        timestamp,
        profile_key,
        encrypted_display_name,
        ..
    } = &event
    {
        if let (Some(group_id), Ok(mailbox_pk)) = (
            crate::domain::anon_group::AnonGroupId::try_from_slice(group_id),
            <[u8; 32]>::try_from(inviter_mailbox_pk.as_slice()),
        ) {
            let key = profile_key
                .as_ref()
                .and_then(|key| <[u8; 32]>::try_from(key.as_slice()).ok());
            let name =
                key.as_ref()
                    .zip(encrypted_display_name.as_ref())
                    .and_then(|(key, encrypted)| {
                        crate::crypto::profile_crypto::decrypt_display_name(key, encrypted).ok()
                    });
            anon_group_repo.save_pending_invite(
                &crate::storage::repositories::anon_group_repo::PendingAnonGroupInvite {
                    group_id,
                    invite_epoch: *timestamp,
                    group_name: group_name.clone(),
                    inviter_id: *peer_id,
                    inviter_device_id: *sender_device_id,
                    inviter_mailbox_pk: mailbox_pk,
                    profile_key: key,
                    inviter_display_name: name,
                    inviter_avatar: None,
                },
            )?;
        }
    }
    let _ = event_tx.send(event);
    Ok(())
}

/// Parses the Content proto inner type and emits appropriate channel events.
pub fn dispatch_control_content(
    inner: content::Content,
    sender_id: &UserId,
    sender_device_id: u32,
    message_guid: &str,
    // When the server received the envelope (ns); ages call offers and edits.
    server_timestamp: u64,
    event_tx: &broadcast::Sender<InboxEvent>,
    stats: &Arc<RwLock<InboxStats>>,
) {
    match inner {
        content::Content::Typing(typing) => {
            tracing::info!(
                sender_id = %sender_id,
                action = typing.action,
                channel_id = %typing.channel_id,
                "DEVICE_INBOX: Received Typing content in control dispatch"
            );
            if typing.channel_id.is_empty() {
                // Empty channel_id => channel-free DM (ServerDm). Emit the DM event.
                let _ = event_tx.send(InboxEvent::DmTyping {
                    peer_id: *sender_id,
                    action: typing.action,
                    timestamp: typing.timestamp,
                });
            } else if let Ok(channel_id) = typing.channel_id.parse::<ChannelId>() {
                tracing::info!(
                    sender_id = %sender_id,
                    channel_id = %channel_id,
                    action = typing.action,
                    "DEVICE_INBOX: Emitting ChannelTyping event"
                );
                let _ = event_tx.send(InboxEvent::ChannelTyping {
                    channel_id,
                    sender_id: *sender_id,
                    action: typing.action,
                    timestamp: typing.timestamp,
                });
            } else {
                tracing::warn!(
                    sender_id = %sender_id,
                    channel_id = %typing.channel_id,
                    "DEVICE_INBOX: Channel typing has invalid channel_id"
                );
            }
            stats.write().messages_processed += 1;
        }
        content::Content::Reaction(reaction) => {
            tracing::debug!(
                sender_id = %sender_id,
                emoji = %reaction.emoji,
                remove = reaction.remove,
                target_author = %reaction.target_author_aci,
                target_ts = reaction.target_sent_timestamp,
                "Channel reaction received"
            );
            let reaction_ts = if reaction.timestamp > 0 {
                reaction.timestamp
            } else {
                server_timestamp
            };
            // Parse channel_id from encoded target_author_aci if present
            if let Some((channel_id_str, actual_author)) =
                parse_channel_reaction_author(&reaction.target_author_aci)
            {
                if let Ok(channel_id) = channel_id_str.parse::<ChannelId>() {
                    let _ = event_tx.send(InboxEvent::ChannelReaction {
                        channel_id,
                        sender_id: *sender_id,
                        emoji: reaction.emoji,
                        remove: reaction.remove,
                        target_author_aci: actual_author,
                        target_sent_timestamp: reaction.target_sent_timestamp,
                        timestamp: reaction_ts,
                    });
                } else {
                    tracing::warn!(
                        sender_id = %sender_id,
                        channel_id = %channel_id_str,
                        "Channel reaction received with invalid channel_id"
                    );
                }
            } else {
                // No "channel:" prefix => channel-free DM (ServerDm) reaction.
                let _ = event_tx.send(InboxEvent::DmReaction {
                    conversation_peer_id: None,
                    peer_id: *sender_id,
                    emoji: reaction.emoji,
                    remove: reaction.remove,
                    target_author_aci: reaction.target_author_aci,
                    target_sent_timestamp: reaction.target_sent_timestamp,
                    timestamp: reaction_ts,
                });
            }
            stats.write().messages_processed += 1;
        }
        content::Content::Pin(pin) => {
            let pin_ts = if pin.timestamp > 0 {
                pin.timestamp
            } else {
                server_timestamp
            };
            if pin.channel_id.is_empty() {
                // Empty channel_id => channel-free DM (ServerDm) pin.
                let _ = event_tx.send(InboxEvent::DmPin {
                    peer_id: *sender_id,
                    target_author_aci: pin.target_author_aci,
                    target_sent_timestamp: pin.target_sent_timestamp,
                    unpin: pin.unpin,
                    timestamp: pin_ts,
                });
            } else if let Ok(channel_id) = pin.channel_id.parse::<ChannelId>() {
                tracing::debug!(
                    sender_id = %sender_id,
                    channel_id = %channel_id,
                    target_author = %pin.target_author_aci,
                    target_ts = pin.target_sent_timestamp,
                    unpin = pin.unpin,
                    "Channel pin received"
                );
                let _ = event_tx.send(InboxEvent::ChannelPin {
                    channel_id,
                    sender_id: *sender_id,
                    target_author_aci: pin.target_author_aci,
                    target_sent_timestamp: pin.target_sent_timestamp,
                    unpin: pin.unpin,
                    timestamp: pin_ts,
                });
            } else {
                tracing::warn!(
                    sender_id = %sender_id,
                    channel_id = %pin.channel_id,
                    "Channel pin received with invalid channel_id"
                );
            }
            stats.write().messages_processed += 1;
        }
        content::Content::Edit(edit) => {
            let channel_id_str = edit
                .data_message
                .as_ref()
                .map(|dm| dm.channel_id.clone())
                .unwrap_or_default();
            let (new_body, edit_timestamp) = edit
                .data_message
                .as_ref()
                .map(|dm| (dm.body.clone(), dm.timestamp))
                .unwrap_or_default();
            let new_body_preview = if new_body.len() > 50 {
                format!("{}...", &new_body[..47])
            } else {
                new_body.clone()
            };

            if channel_id_str.is_empty() {
                // Empty channel_id => channel-free DM (ServerDm) edit.
                let _ = event_tx.send(InboxEvent::DmEdit {
                    peer_id: *sender_id,
                    target_sent_timestamp: edit.target_sent_timestamp,
                    new_body,
                    edit_timestamp,
                    server_timestamp,
                });
            } else if let Ok(channel_id) = channel_id_str.parse::<ChannelId>() {
                tracing::debug!(
                    sender_id = %sender_id,
                    channel_id = %channel_id,
                    target_ts = edit.target_sent_timestamp,
                    preview = %new_body_preview,
                    "Channel edit received"
                );
                let _ = event_tx.send(InboxEvent::ChannelEdit {
                    channel_id,
                    sender_id: *sender_id,
                    target_sent_timestamp: edit.target_sent_timestamp,
                    new_body,
                    edit_timestamp,
                    server_timestamp,
                });
            } else {
                tracing::warn!(
                    sender_id = %sender_id,
                    channel_id = %channel_id_str,
                    "Channel edit received with invalid channel_id"
                );
            }
            stats.write().messages_processed += 1;
        }
        content::Content::DataMessage(dm) => {
            if let Some(delete) = dm.delete {
                if dm.channel_id.is_empty() {
                    // Empty channel_id => channel-free DM (ServerDm) delete-for-everyone.
                    let _ = event_tx.send(InboxEvent::DmDeleteForEveryone {
                        target_author_aci: delete.target_author_aci,
                        destination_user_id: delete.destination_user_id,
                        target_call_id: delete.target_call_id,
                        peer_id: *sender_id,
                        target_sent_timestamp: delete.target_sent_timestamp,
                    });
                } else if delete.target_call_id.is_some()
                    || delete.destination_user_id.is_some()
                    || !delete
                        .target_author_aci
                        .as_deref()
                        .is_some_and(|author| author.parse::<UserId>().ok() == Some(*sender_id))
                {
                    return;
                } else if let Ok(channel_id) = dm.channel_id.parse::<ChannelId>() {
                    tracing::debug!(
                        sender_id = %sender_id,
                        channel_id = %channel_id,
                        target_ts = delete.target_sent_timestamp,
                        "Channel delete for everyone received"
                    );
                    let _ = event_tx.send(InboxEvent::ChannelDeleteForEveryone {
                        channel_id,
                        sender_id: *sender_id,
                        target_sent_timestamp: delete.target_sent_timestamp,
                    });
                } else {
                    tracing::warn!(
                        sender_id = %sender_id,
                        channel_id = %dm.channel_id,
                        "Channel delete for everyone received with invalid channel_id"
                    );
                }
                stats.write().messages_processed += 1;
            } else {
                tracing::debug!(
                    message_id = %message_guid,
                    "Unexpected DataMessage in control content (expected delete)"
                );
            }
        }
        content::Content::ReadSync(sync) => {
            let _ = event_tx.send(InboxEvent::ReadSync {
                sender_id: *sender_id,
                sync,
            });
            stats.write().messages_processed += 1;
        }
        content::Content::ProfileKeySync(sync) => {
            let _ = event_tx.send(InboxEvent::ProfileKeySync {
                sender_id: *sender_id,
                sync,
            });
            stats.write().messages_processed += 1;
        }
        content::Content::ConfigurationSync(sync) => {
            let _ = event_tx.send(InboxEvent::ConfigurationSync {
                sender_id: *sender_id,
                sync,
            });
            stats.write().messages_processed += 1;
        }
        content::Content::SavedMessageSync(sync) => {
            let _ = event_tx.send(InboxEvent::SavedMessageSync {
                sender_id: *sender_id,
                sync,
            });
            stats.write().messages_processed += 1;
        }
        content::Content::DeleteForMeSync(sync) => {
            tracing::debug!(
                conversation_id = %sync.conversation_id,
                target_author = %sync.target_author_aci,
                target_ts = sync.target_sent_timestamp,
                "Channel delete for me sync"
            );
            let _ = event_tx.send(InboxEvent::ChannelDeleteForMe {
                sender_id: *sender_id,
                conversation_id: sync.conversation_id,
                target_author_aci: sync.target_author_aci,
                target_sent_timestamp: sync.target_sent_timestamp,
                saved_message_id: sync.saved_message_id,
                saved_messages_scope: sync.saved_messages_scope,
            });
            stats.write().messages_processed += 1;
        }
        content::Content::Receipt(receipt) => {
            if receipt.channel_id.is_empty() {
                // Empty channel_id => channel-free DM (ServerDm) receipt.
                let _ = event_tx.send(InboxEvent::DmReceipt {
                    sender_device_id,
                    peer_id: *sender_id,
                    receipt_type: receipt.r#type,
                    timestamps: receipt.timestamps,
                });
            } else if let Ok(channel_id) = receipt.channel_id.parse::<ChannelId>() {
                tracing::debug!(
                    sender_id = %sender_id,
                    channel_id = %channel_id,
                    receipt_type = receipt.r#type,
                    timestamps = ?receipt.timestamps,
                    "Channel receipt received"
                );
                let _ = event_tx.send(InboxEvent::ChannelReceipt {
                    sender_device_id,
                    channel_id,
                    sender_id: *sender_id,
                    receipt_type: receipt.r#type,
                    timestamps: receipt.timestamps,
                });
            } else {
                tracing::warn!(
                    sender_id = %sender_id,
                    channel_id = %receipt.channel_id,
                    "Channel receipt received with invalid channel_id"
                );
            }
            stats.write().messages_processed += 1;
        }
        content::Content::CallEvent(ev) => {
            use crate::proto::content_v1::call_event::Type;
            use crate::services::message::ChannelCallPhase;
            let phase = match Type::try_from(ev.r#type) {
                Ok(Type::Started) => Some(ChannelCallPhase::Started),
                Ok(Type::Ended) => Some(ChannelCallPhase::Ended),
                _ => None,
            };
            match (phase, ev.channel_id.parse::<ChannelId>()) {
                (Some(phase), Ok(channel_id)) => {
                    let _ = event_tx.send(InboxEvent::ChannelCallEvent {
                        channel_id,
                        sender_id: *sender_id,
                        call_id: ev.call_id,
                        phase,
                        duration_secs: ev.duration_secs,
                        timestamp: if ev.timestamp > 0 {
                            ev.timestamp
                        } else {
                            server_timestamp
                        },
                    });
                    stats.write().messages_processed += 1;
                }
                _ => {
                    tracing::warn!(sender_id = %sender_id, "Malformed channel call event");
                    stats.write().messages_failed += 1;
                }
            }
        }
        // Normal-mode call signaling: friend DMs (and so calls) ride sealed-
        // sender streaming there. Same events as the mailbox path
        // (inbox/polling/dm.rs); a friend DM's conversation id is the peer's.
        inner @ (content::Content::CallKey(_)
        | content::Content::CallHangup(_)
        | content::Content::GroupCallSignal(_)) => {
            use crate::services::gateway::DmContent;
            let parsed = MessageGateway::parse_content(Content {
                content: Some(inner),
                ..Default::default()
            });
            let event = match parsed {
                Ok(DmContent::CallKeyOffer {
                    call_id,
                    ephemeral_public_key,
                    timestamp,
                    call_type,
                }) => InboxEvent::CallKeyOfferReceived {
                    peer_id: *sender_id,
                    call_id,
                    ephemeral_public_key,
                    call_type: if call_type.is_empty() {
                        "Voice Call".to_string()
                    } else {
                        call_type
                    },
                    timestamp: if server_timestamp > 0 {
                        server_timestamp
                    } else {
                        timestamp
                    },
                },
                Ok(DmContent::CallKeyAnswer {
                    call_id,
                    ephemeral_public_key,
                    timestamp,
                }) => InboxEvent::CallKeyAnswerReceived {
                    peer_id: *sender_id,
                    call_id,
                    ephemeral_public_key,
                    timestamp,
                },
                Ok(DmContent::CallHangup {
                    call_id,
                    reason,
                    timestamp,
                }) => InboxEvent::CallHangup {
                    peer_id: *sender_id,
                    call_id,
                    reason,
                    timestamp,
                },
                Ok(DmContent::GroupCallSignal {
                    call_id,
                    channel_id,
                    signal,
                    timestamp,
                }) => {
                    let Ok(channel_id) = channel_id.parse::<ChannelId>() else {
                        tracing::warn!(sender_id = %sender_id, "Group call signal with invalid channel_id");
                        stats.write().messages_failed += 1;
                        return;
                    };
                    InboxEvent::GroupCallSignalReceived {
                        sender_id: *sender_id,
                        call_id,
                        channel_id,
                        signal,
                        timestamp,
                    }
                }
                other => {
                    let error = other.err().map(|e| e.to_string()).unwrap_or_default();
                    tracing::warn!(sender_id = %sender_id, error = %error, "Malformed call signaling content");
                    stats.write().messages_failed += 1;
                    return;
                }
            };
            let _ = event_tx.send(event);
            stats.write().messages_processed += 1;
        }
        _ => {
            tracing::debug!(
                message_id = %message_guid,
                "Unhandled channel control content type"
            );
            stats.write().messages_processed += 1;
        }
    }
}

/// Parse channel_id and actual author from encoded reaction target_author_aci.
///
/// The Reaction proto doesn't have a channel_id field, so for channel reactions
/// we encode the channel context in target_author_aci as "channel:{channel_id}:{author_aci}".
fn parse_channel_reaction_author(target_author_aci: &str) -> Option<(String, String)> {
    if target_author_aci.starts_with("channel:") {
        let parts: Vec<&str> = target_author_aci.splitn(3, ':').collect();
        if parts.len() == 3 {
            return Some((parts[1].to_string(), parts[2].to_string()));
        }
    }
    None
}

#[cfg(test)]
mod call_control_tests {
    use super::*;

    /// Normal mode carries call signaling over sealed-sender streaming, so the
    /// control dispatcher must turn a CallKey offer into the same event the
    /// mailbox path emits — with a friend DM's conversation id (the peer's)
    /// when the offer names none.
    #[test]
    fn streamed_call_offer_rings_like_a_mailbox_one() {
        let (event_tx, mut event_rx) = broadcast::channel(4);
        let stats = Arc::new(RwLock::new(InboxStats::default()));
        let caller = UserId::new();
        let offer = crate::services::build_call_key_offer("call-1", [7u8; 32], "");

        dispatch_control_content(
            offer.content.unwrap(),
            &caller,
            1,
            "guid",
            42,
            &event_tx,
            &stats,
        );

        match event_rx.try_recv().expect("an event") {
            InboxEvent::CallKeyOfferReceived {
                peer_id,
                call_id,
                ephemeral_public_key,
                call_type,
                timestamp,
            } => {
                // Aged by the server's receive stamp, not the caller's clock.
                assert_eq!(timestamp, 42);
                assert_eq!(peer_id, caller);
                assert_eq!(call_id, "call-1");
                assert_eq!(ephemeral_public_key, [7u8; 32]);
                assert_eq!(call_type, "Voice Call");
            }
            other => panic!("expected CallKeyOfferReceived, got {other:?}"),
        }
    }

    /// An edit carries its own timestamp and the server's to the processor;
    /// a reaction or pin with no timestamp of its own takes the server's.
    #[test]
    fn edits_reactions_and_pins_carry_their_timestamps() {
        use crate::proto::content_v1::{EditMessage, PinMessage, Reaction};
        let (event_tx, mut event_rx) = broadcast::channel(8);
        let stats = Arc::new(RwLock::new(InboxStats::default()));
        let peer = UserId::new();
        let channel = ChannelId::new();

        let edit = |channel_id: String| {
            content::Content::Edit(EditMessage {
                target_sent_timestamp: 100,
                data_message: Some(DataMessage {
                    body: "new".into(),
                    timestamp: 150,
                    channel_id,
                    ..Default::default()
                }),
            })
        };
        dispatch_control_content(edit(String::new()), &peer, 1, "g", 42, &event_tx, &stats);
        match event_rx.try_recv().expect("an event") {
            InboxEvent::DmEdit {
                edit_timestamp,
                server_timestamp,
                target_sent_timestamp,
                ..
            } => {
                assert_eq!(
                    (edit_timestamp, server_timestamp, target_sent_timestamp),
                    (150, 42, 100)
                );
            }
            other => panic!("expected DmEdit, got {other:?}"),
        }
        dispatch_control_content(
            edit(channel.to_string()),
            &peer,
            1,
            "g",
            42,
            &event_tx,
            &stats,
        );
        match event_rx.try_recv().expect("an event") {
            InboxEvent::ChannelEdit {
                edit_timestamp,
                server_timestamp,
                channel_id,
                ..
            } => {
                assert_eq!(
                    (edit_timestamp, server_timestamp, channel_id),
                    (150, 42, channel)
                );
            }
            other => panic!("expected ChannelEdit, got {other:?}"),
        }

        let reaction = content::Content::Reaction(Reaction {
            emoji: "x".into(),
            target_author_aci: peer.to_string(),
            target_sent_timestamp: 100,
            ..Default::default()
        });
        dispatch_control_content(reaction, &peer, 1, "g", 42, &event_tx, &stats);
        match event_rx.try_recv().expect("an event") {
            InboxEvent::DmReaction { timestamp, .. } => {
                assert_eq!(timestamp, 42, "unstamped: the server's")
            }
            other => panic!("expected DmReaction, got {other:?}"),
        }
        let pin = content::Content::Pin(PinMessage {
            target_author_aci: peer.to_string(),
            target_sent_timestamp: 100,
            timestamp: 77,
            ..Default::default()
        });
        dispatch_control_content(pin, &peer, 1, "g", 42, &event_tx, &stats);
        match event_rx.try_recv().expect("an event") {
            InboxEvent::DmPin { timestamp, .. } => assert_eq!(timestamp, 77),
            other => panic!("expected DmPin, got {other:?}"),
        }
    }
}
