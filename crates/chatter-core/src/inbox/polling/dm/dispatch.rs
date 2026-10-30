use super::*;

/// Dispatch DM content based on type.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_dm_content(
    dm_content: DmContent,
    sender_id: &UserId,
    // The peer device whose mailbox-layer key opened this envelope.
    sender_device_id: u32,
    peer_id: UserId,
    message_id: &str,
    server_timestamp: u64,
    current_user_id: &Option<UserId>,
    event_tx: &broadcast::Sender<InboxEvent>,
    message_repo: &Arc<MessageRepository>,
    attachment_repo: &Arc<AttachmentRepository>,
    user_repo: &Arc<UserRepository>,
    conversation_manager: &DmConversationManager,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
) -> AckDecision {
    // Log the content type being dispatched
    let content_type = match &dm_content {
        DmContent::Text { .. } => "Text",
        DmContent::Typing { .. } => "Typing",
        DmContent::Receipt { .. } => "Receipt",
        DmContent::Reaction { .. } => "Reaction",
        DmContent::Pin { .. } => "Pin",
        DmContent::Edit { .. } => "Edit",
        DmContent::DeleteForEveryone { .. } => "DeleteForEveryone",
        DmContent::KeyExchange { .. } => "KeyExchange",
        DmContent::MailboxRotation { .. } => "MailboxRotation",
        DmContent::NullMessage => "NullMessage",
        _ => "Other",
    };
    tracing::info!(
        peer_id = %peer_id,
        sender_id = %sender_id,
        content_type = content_type,
        message_id = %message_id,
        "DM_DISPATCH: dispatch_dm_content called"
    );

    match dm_content {
        DmContent::Text {
            body,
            timestamp,
            attachments,
            profile_key,
            quote,
            thread_id,
            ..
        } => {
            let mut profile_key_save_failed = false;
            // Store sender's profile key if present (for decrypting their display_name)
            if let Some(ref key) = profile_key {
                if key.len() == 32 {
                    if let Some(current_user) = current_user_id {
                        let key_array: [u8; 32] = key.as_slice().try_into().unwrap();
                        match user_repo.set_friend_profile_key(current_user, sender_id, &key_array)
                        {
                            Ok(changed) => {
                                tracing::info!(
                                    sender_id = %sender_id,
                                    changed,
                                    "Stored sender's profile key for display_name decryption"
                                );
                                // New or rotated key: refresh their profile now,
                                // as the handshake path does, instead of leaving
                                // the fallback name up until a restart.
                                if changed {
                                    let _ = event_tx.send(InboxEvent::ProfileKeyReceived {
                                        sender_id: *sender_id,
                                    });
                                }
                            }
                            Err(e) => {
                                profile_key_save_failed = true;
                                tracing::debug!(
                                    sender_id = %sender_id,
                                    error = %e,
                                    "Failed to store profile key (sender may not be a friend)"
                                );
                            }
                        }
                    } else {
                        profile_key_save_failed = true;
                    }
                }
            }

            // Pure profile-key handshake carrier (`send_profile_key_handshake`
            // goes over this mailbox in Shield mode): the key above was the
            // whole point — no message row, no bubble. Mirrors the ServerDm
            // inbound path (gateway `store_data_message`).
            if body.is_empty() && attachments.is_empty() {
                return if profile_key_save_failed {
                    AckDecision::Retain
                } else {
                    AckDecision::Ack
                };
            }

            // A friend DM is ONE conversation whichever transport carried it:
            // filed exactly where the sealed-sender path files it
            // (`ChannelId::from(peer)`, `ServerDm`), so the chat list, badge,
            // reload and history read it — a mailbox-only key was never read
            // back, and these messages vanished on reload.
            let dm_channel_id = ChannelId::from(*sender_id);

            // Store message in local database for persistence
            if current_user_id.is_some() {
                let msg_id = message_id
                    .parse::<MessageId>()
                    .unwrap_or_else(|_| MessageId::new());

                // Use sender's timestamp for Signal-style dedup (sender_id + sent_at)
                // Both timestamp and server_timestamp are in nanoseconds
                let sent_at = if timestamp > 0 {
                    timestamp
                } else {
                    server_timestamp
                };
                tracing::debug!(
                    proto_timestamp = timestamp,
                    server_timestamp = server_timestamp,
                    chosen_sent_at = sent_at,
                    peer_id = %peer_id,
                    "dispatch_dm_content: timestamp selection"
                );
                let mut message = Message::new_text(
                    dm_channel_id,
                    ConversationType::ServerDm,
                    Some(*sender_id),
                    body.clone(),
                    sent_at,
                );
                message.id = msg_id;
                // Thread root rides on the wire (DataMessage.thread_id).
                // Mirrors the ServerDm inbound path in gateway/mod.rs — see
                // that comment for why this matters: without it, a thread
                // reply gets filed into the main channel instead of the
                // open thread panel.
                message.thread_id = thread_id.as_deref().and_then(|s| s.parse().ok());

                // Add attachment IDs to message (attachments are stored separately in attachment_repo)
                message.attachments = attachments
                    .iter()
                    .filter(|p| !p.cdn_key.is_empty())
                    .map(|p| AttachmentId::from_cdn_key(&p.cdn_key))
                    .collect();

                // Carry the reply quote so the receiver's bubble keeps its
                // reply context. Shared with the ServerDm inbound path.
                crate::services::gateway::inbound::apply_inbound_quote(
                    &mut message,
                    quote.as_ref(),
                    message_repo,
                );

                // Announce the row as stored (an early edit, reaction or pin
                // applied); an early delete-for-everyone stores nothing.
                match message_repo.insert_or_ignore_received(&message) {
                    Ok(Some(message)) => {
                        tracing::debug!(
                            peer_id = %peer_id,
                            channel_id = %dm_channel_id,
                            message_id = %msg_id,
                            "Stored received DM in database"
                        );

                        // Emit domain event so the notification listener,
                        // UI message-state handler, and any other domain
                        // subscribers learn about the new message. Fresh
                        // (non-duplicate) inserts only — duplicates fall
                        // through the Ok(None) arm. Previously this
                        // branch only emitted the lower-level
                        // InboxEvent::DmReceived, which the notification
                        // listener doesn't subscribe to, so identified
                        // DM texts never raised a desktop toast.
                        if let Some(tx) = domain_tx {
                            let _ = tx.send(DomainEvent::MessageReceived {
                                message: message.clone(),
                            });
                        }
                    }
                    Ok(None) => {
                        tracing::debug!(peer_id = %peer_id, "DM duplicate, or deleted before it arrived — not announced");
                    }
                    Err(e) => {
                        tracing::warn!(peer_id = %peer_id, error = %e, "Failed to store DM");
                        return AckDecision::Retain;
                    }
                }
            } else {
                tracing::warn!(peer_id = %peer_id, "Current user unavailable; retaining DM");
                return AckDecision::Retain;
            }

            // Create attachment records from pointers (service layer responsibility)
            if !attachments.is_empty() {
                let attachment_service = AttachmentService::new_local(attachment_repo.clone());
                match attachment_service.create_from_pointers(&attachments) {
                    Ok(created_ids) => tracing::debug!(
                        peer_id = %peer_id,
                        attachment_count = created_ids.len(),
                        "Created attachment records from pointers"
                    ),
                    Err(error) => tracing::warn!(%error, "Failed to store attachment records"),
                }
            }

            let _ = event_tx.send(InboxEvent::DmReceived {
                peer_id,
                text: body,
                // Both timestamp and server_timestamp are in nanoseconds
                timestamp: if timestamp > 0 {
                    timestamp
                } else {
                    server_timestamp
                },
                message_id: message_id.to_string(),
                attachments,
            });
        }

        DmContent::Typing { action, timestamp } => {
            // Emit typing event - the UI layer handles stale indicators with 15s auto-clear timers
            // (Signal-style: each STARTED resets a 15s timer, timer expiry clears indicator)
            tracing::info!(
                peer_id = %peer_id,
                action,
                action_name = if action == 1 { "STARTED" } else { "STOPPED" },
                "DM_MAILBOX: Emitting DmTyping event"
            );
            let _ = event_tx.send(InboxEvent::DmTyping {
                peer_id,
                action,
                timestamp,
            });
        }

        DmContent::Receipt {
            receipt_type,
            timestamps,
        } => {
            tracing::debug!(peer_id = %peer_id, receipt_type, count = timestamps.len(), "DM receipt");
            let _ = event_tx.send(InboxEvent::DmReceipt {
                sender_device_id,
                peer_id,
                receipt_type,
                timestamps,
            });
        }

        DmContent::Reaction {
            emoji,
            remove,
            target_author_aci,
            target_sent_timestamp,
            timestamp,
        } => {
            tracing::debug!(peer_id = %peer_id, emoji = %emoji, remove, "DM reaction");
            let _ = event_tx.send(InboxEvent::DmReaction {
                conversation_peer_id: Some(peer_id),
                peer_id,
                emoji,
                remove,
                target_author_aci,
                target_sent_timestamp,
                timestamp: if timestamp > 0 {
                    timestamp
                } else {
                    server_timestamp
                },
            });
        }

        DmContent::Pin {
            target_author_aci,
            target_sent_timestamp,
            unpin,
            timestamp,
        } => {
            tracing::debug!(peer_id = %peer_id, unpin, "DM pin");
            let _ = event_tx.send(InboxEvent::DmPin {
                peer_id,
                target_author_aci,
                target_sent_timestamp,
                unpin,
                timestamp: if timestamp > 0 {
                    timestamp
                } else {
                    server_timestamp
                },
            });
        }

        DmContent::Edit {
            target_sent_timestamp,
            new_body,
            edit_timestamp,
        } => {
            tracing::debug!(peer_id = %peer_id, target = target_sent_timestamp, "DM edit");
            let _ = event_tx.send(InboxEvent::DmEdit {
                peer_id,
                target_sent_timestamp,
                new_body,
                edit_timestamp,
                server_timestamp,
            });
        }

        DmContent::DeleteForEveryone {
            target_author_aci,
            destination_user_id,
            target_call_id,
            target_sent_timestamp,
        } => {
            tracing::debug!(
                peer_id = %peer_id,
                target_sent_timestamp = target_sent_timestamp,
                "DM delete for everyone received"
            );
            let _ = event_tx.send(InboxEvent::DmDeleteForEveryone {
                target_author_aci,
                destination_user_id,
                target_call_id,
                // Scope follows the authenticated decrypted sender, not mailbox routing metadata.
                peer_id: *sender_id,
                target_sent_timestamp,
            });
        }

        DmContent::KeyExchange {
            sender_device_id,
            conversation_id,
            ephemeral_pk,
            timestamp,
        } => {
            // Key exchange received in DM mailbox (bootstrap response from peer)
            tracing::debug!(
                peer_id = %peer_id,
                conversation_id = %conversation_id,
                key_len = ephemeral_pk.len(),
                "DM bootstrap key exchange received"
            );

            // Emit domain event for the bootstrap handler to process
            if let Some(tx) = domain_tx {
                // Parse conversation_id as ChannelId
                let conv_id: ChannelId = conversation_id.parse().unwrap_or_else(|_| {
                    // Fallback: derive from peer
                    if let Some(current_user) = current_user_id {
                        ChannelId::from_dm_participants(current_user, sender_id)
                    } else {
                        ChannelId::new()
                    }
                });

                let _ = tx.send(DomainEvent::KeyExchangeReceived {
                    sender_id: *sender_id,
                    sender_device_id,
                    conversation_id: conv_id,
                    ephemeral_public_key: ephemeral_pk,
                    timestamp,
                });
                tracing::info!(
                    peer_id = %peer_id,
                    "Emitted KeyExchangeReceived domain event from DM mailbox"
                );
            } else {
                tracing::warn!(
                    peer_id = %peer_id,
                    "Received KeyExchange in DM mailbox but no domain event sender configured"
                );
            }
        }

        DmContent::MailboxRotation { new_mailbox_pk } => {
            // L4 rotation announce: the sending peer device minted a new
            // mailbox and wants future sends directed there. Only that
            // device's mailbox moves — the key that opened the envelope
            // names it. Does NOT touch the ratchet / conversation key.
            let pk: [u8; 32] = match new_mailbox_pk.as_slice().try_into() {
                Ok(pk) => pk,
                Err(_) => {
                    tracing::warn!(
                        peer_id = %peer_id,
                        len = new_mailbox_pk.len(),
                        "MailboxRotation announce has invalid pk length (expected 32), ignoring"
                    );
                    return AckDecision::Ack;
                }
            };
            match conversation_manager.rotate_peer_mailbox(&peer_id, sender_device_id, pk) {
                Ok(true) => {
                    tracing::info!(
                        peer_id = %peer_id,
                        sender_device_id,
                        "MailboxRotation: peer device mailbox updated to rotated address"
                    );
                }
                Ok(false) => {
                    tracing::warn!(
                        peer_id = %peer_id,
                        sender_device_id,
                        "MailboxRotation announce from a device with no mailbox on record, retaining"
                    );
                    return AckDecision::Retain;
                }
                Err(e) => {
                    tracing::error!(
                        peer_id = %peer_id,
                        error = %e,
                        "MailboxRotation: failed to save rotated peer mailbox"
                    );
                    return AckDecision::Retain;
                }
            }
        }

        DmContent::CallKeyOffer {
            call_id,
            ephemeral_public_key,
            timestamp,
            call_type,
        } => {
            // Call key offer received (ephemeral key for PFS)
            tracing::info!(
                peer_id = %peer_id,
                call_id = %call_id,
                "Call key offer received (incoming call with ephemeral key)"
            );

            let resolved_call_type = if call_type.is_empty() {
                "Voice Call".to_string()
            } else {
                call_type
            };

            let _ = event_tx.send(InboxEvent::CallKeyOfferReceived {
                peer_id,
                call_id,
                ephemeral_public_key,
                call_type: resolved_call_type,
                timestamp: if server_timestamp > 0 {
                    server_timestamp
                } else {
                    timestamp
                },
            });
        }

        DmContent::CallKeyAnswer {
            call_id,
            ephemeral_public_key,
            timestamp,
        } => {
            // Call key answer received (response to our offer)
            tracing::info!(
                peer_id = %peer_id,
                call_id = %call_id,
                "Call key answer received (peer's ephemeral key)"
            );

            let _ = event_tx.send(InboxEvent::CallKeyAnswerReceived {
                peer_id,
                call_id,
                ephemeral_public_key,
                timestamp,
            });
        }

        DmContent::GroupCallSignal {
            call_id,
            channel_id,
            signal,
            timestamp,
        } => {
            tracing::info!(
                peer_id = %peer_id,
                call_id = %call_id,
                ?signal,
                "Group call signal received via pairwise distribution"
            );
            let Ok(channel_id) = channel_id.parse::<ChannelId>() else {
                tracing::warn!(peer_id = %peer_id, "Group call signal with invalid channel_id");
                return AckDecision::Ack;
            };
            let _ = event_tx.send(InboxEvent::GroupCallSignalReceived {
                sender_id: *sender_id,
                call_id,
                channel_id,
                signal,
                timestamp,
            });
        }

        DmContent::CallHangup {
            call_id,
            reason,
            timestamp,
        } => {
            // Call hangup notification via sealed sender
            tracing::info!(
                peer_id = %peer_id,
                call_id = %call_id,
                reason = ?reason,
                "Call hangup notification received"
            );

            let _ = event_tx.send(InboxEvent::CallHangup {
                peer_id,
                call_id,
                reason,
                timestamp,
            });
        }

        // ====================================================================
        // Anonymous Group Messages (NDSS'21 style)
        // ====================================================================
        DmContent::AnonGroupMemberList {
            group_id,
            members,
            timestamp,
        } => {
            tracing::info!(
                peer_id = %peer_id,
                member_count = members.len(),
                "Anonymous group member list received"
            );
            let _ = event_tx.send(InboxEvent::AnonGroupMemberList {
                peer_id,
                group_id,
                members,
                timestamp,
            });
        }

        DmContent::AnonGroupLeave {
            group_id,
            timestamp,
        } => {
            tracing::info!(
                peer_id = %peer_id,
                "Anonymous group leave received"
            );
            let _ = event_tx.send(InboxEvent::AnonGroupLeave {
                peer_id,
                group_id,
                timestamp,
            });
        }

        DmContent::AnonGroupKick {
            group_id,
            kicked_user_id,
            admin_signature,
            timestamp,
        } => {
            tracing::info!(
                peer_id = %peer_id,
                "Anonymous group kick received"
            );
            let _ = event_tx.send(InboxEvent::AnonGroupKick {
                peer_id,
                group_id,
                kicked_user_id,
                admin_signature,
                timestamp,
            });
        }

        DmContent::AnonGroupUpdate {
            group_id,
            update,
            timestamp,
        } => {
            tracing::info!(
                peer_id = %peer_id,
                "Anonymous group update received"
            );
            let _ = event_tx.send(InboxEvent::AnonGroupUpdate {
                peer_id,
                group_id,
                update,
                timestamp,
            });
        }

        DmContent::AnonGroupMessage {
            group_id,
            inner_content,
        } => {
            tracing::info!(
                peer_id = %peer_id,
                "Anonymous group message received"
            );
            let _ = event_tx.send(InboxEvent::AnonGroupMessage {
                peer_id,
                group_id,
                inner_content,
            });
        }
        // Refused: secret-chat content never rides an account DM mailbox.
        DmContent::SecretChatOffMailbox => {
            tracing::warn!("Dropping secret-chat content that arrived outside its mailbox");
        }
        // Unreachable in practice — the poll loop routes this to
        // `ack_policy::drop_and_ack` before it ever calls dispatch, because the
        // ack decision is the only thing left to make. The arm exists because
        // this match is exhaustive on purpose, and because "dispatch a blocked
        // sender's message" must have exactly one possible meaning if a future
        // caller does reach here: do nothing.
        DmContent::DroppedBlockedSender => {}
        // The peer reset its sessions with us; decrypting this moved us onto
        // the new one. Nothing to store, nothing for the UI — the poll loop acks.
        DmContent::NullMessage => {}
    }
    AckDecision::Ack
}
