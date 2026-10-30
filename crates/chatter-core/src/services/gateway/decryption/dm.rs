use super::*;

impl MessageGateway {
    /// Process an incoming DM message with layered decryption.
    ///
    /// DM messages have two encryption layers:
    /// 1. Mailbox layer (XChaCha20-Poly1305) - provides sender anonymity from server
    /// 2. Signal layer (sealed sender) - provides forward secrecy via Double Ratchet
    ///
    /// This method decrypts both layers, parses the Content proto,
    /// and returns the parsed content.
    ///
    /// # Arguments
    ///
    /// * `mailbox_encrypted` - The doubly-encrypted envelope from the mailbox
    /// * `shared_secret` - The X25519 shared secret for mailbox layer decryption
    /// * `server_timestamp_ns` - The mailbox message's server timestamp (sender
    ///   certificate validation time)
    ///
    /// # Returns
    ///
    /// A tuple of (sender_user_id, content) on success.
    pub async fn process_dm_message(
        &self,
        mailbox_encrypted: &[u8],
        shared_secret: &[u8; 32],
        server_timestamp_ns: u64,
    ) -> Result<(UserId, DmContent), ServiceError> {
        // Layer 1: Decrypt mailbox layer (XChaCha20-Poly1305)
        let signal_ciphertext = decrypt_dm_bytes(mailbox_encrypted, shared_secret)?;

        // Layer 2: Decrypt Signal layer (sealed sender with Double Ratchet)
        let (sender_address, mut plaintext, msg_type) = self
            .signal_service
            .decrypt_message(&signal_ciphertext, server_timestamp_ns)
            .await?;

        // No DEM ride-along is expected on the DM path, so `Plaintext` here is
        // simply ratchet-bypassing bytes. Refuse before `Content::decode`.
        if msg_type == CiphertextMessageType::Plaintext {
            plaintext.zeroize();
            return Err(unratcheted_plaintext_rejected(sender_address.name()));
        }

        // Extract sender ID from protocol address
        let sender_id: UserId = sender_address.name().parse().map_err(|e| {
            ServiceError::Validation(format!("Invalid sender ID in Signal address: {}", e))
        })?;

        // Block check before any parse, as on the other two paths. The mailbox
        // this envelope arrived in belongs to one peer, but the sealed sender
        // certificate names who actually sent it — so the authenticated
        // `sender_id` is what gets checked, not the mailbox owner.
        if sender_is_blocked(&self.user_repo, &self.settings_repo, &sender_id)? {
            plaintext.zeroize();
            tracing::info!(
                sender_id = %sender_id,
                "DM_DECRYPT: dropping mailbox message from a blocked sender"
            );
            return Ok((sender_id, DmContent::DroppedBlockedSender));
        }

        // Try to parse as Content proto first (preferred)
        if let Ok(content_proto) = crate::network::validation::protovalidate::decode_inbound_content(
            plaintext.as_slice(),
            "mailbox_dm",
        ) {
            // Log content type before parsing
            let ct_name = content_type_name(&content_proto.content);
            tracing::info!(
                sender_id = %sender_id,
                content_type = ct_name,
                timestamp = content_proto.timestamp,
                "DM_DECRYPT: Parsed Content proto from peer"
            );

            let mut dm_content = Self::parse_content(content_proto)?;
            if let DmContent::KeyExchange {
                sender_device_id, ..
            } = &mut dm_content
            {
                *sender_device_id = u32::from(sender_address.device_id());
            }
            tracing::info!(sender_id = %sender_id, dm_content_type = ?std::mem::discriminant(&dm_content), "DM_DECRYPT: Content parsed to DmContent");
            // Zeroize plaintext before returning
            plaintext.zeroize();
            return Ok((sender_id, dm_content));
        }

        let len = plaintext.len();
        plaintext.zeroize();
        Err(not_content(len))
    }

    /// Parse Content proto into DmContent.
    pub(crate) fn parse_content(content_proto: Content) -> Result<DmContent, ServiceError> {
        match content_proto.content {
            Some(content::Content::DataMessage(msg)) => {
                // Check if this is a delete request
                if let Some(delete) = msg.delete {
                    if !msg.channel_id.is_empty() {
                        return Err(ServiceError::Validation(
                            "DM delete carries channel context".into(),
                        ));
                    }
                    return Ok(DmContent::DeleteForEveryone {
                        target_author_aci: delete.target_author_aci,
                        destination_user_id: delete.destination_user_id,
                        target_call_id: delete.target_call_id,
                        target_sent_timestamp: delete.target_sent_timestamp,
                    });
                }

                let expire_timer = crate::services::disappearing::message_ttl(&msg);
                let stated_timer = crate::services::disappearing::stated_timer(&msg);
                let timer_update = crate::services::disappearing::is_timer_update(&msg);
                // Extract profile key if present (32 bytes for display_name decryption)
                let profile_key = msg.profile_key.filter(|k| k.len() == 32);
                if profile_key.is_some() {
                    tracing::debug!("parse_content: DataMessage contains profile key");
                }

                // Log timestamp for debugging
                tracing::debug!(
                    dm_timestamp = msg.timestamp,
                    content_timestamp = content_proto.timestamp,
                    body_len = msg.body.len(),
                    has_profile_key = profile_key.is_some(),
                    "parse_content: DataMessage extracted"
                );
                // Otherwise it's a text message (possibly with attachments)
                let thread_id = (!msg.thread_id.is_empty()).then_some(msg.thread_id);
                Ok(DmContent::Text {
                    body: msg.body,
                    timestamp: msg.timestamp,
                    attachments: msg.attachments,
                    profile_key,
                    quote: msg.quote,
                    thread_id,
                    expire_timer,
                    stated_timer,
                    timer_update,
                })
            }
            Some(content::Content::Typing(msg)) => {
                tracing::info!(
                    action = msg.action,
                    action_name = if msg.action == 1 {
                        "STARTED"
                    } else {
                        "STOPPED"
                    },
                    timestamp = msg.timestamp,
                    "DM_DECRYPT: Parsed Typing content from peer"
                );
                Ok(DmContent::Typing {
                    action: msg.action,
                    timestamp: msg.timestamp,
                })
            }
            Some(content::Content::Receipt(msg)) => Ok(DmContent::Receipt {
                receipt_type: msg.r#type,
                timestamps: msg.timestamps,
            }),
            Some(content::Content::Reaction(msg)) => Ok(DmContent::Reaction {
                emoji: msg.emoji,
                remove: msg.remove,
                target_author_aci: msg.target_author_aci,
                target_sent_timestamp: msg.target_sent_timestamp,
                timestamp: msg.timestamp,
            }),
            Some(content::Content::Pin(msg)) => Ok(DmContent::Pin {
                target_author_aci: msg.target_author_aci,
                target_sent_timestamp: msg.target_sent_timestamp,
                unpin: msg.unpin,
                timestamp: msg.timestamp,
            }),
            Some(content::Content::Edit(msg)) => {
                let (new_body, edit_timestamp) = msg
                    .data_message
                    .map(|dm| (dm.body, dm.timestamp))
                    .unwrap_or_default();
                Ok(DmContent::Edit {
                    target_sent_timestamp: msg.target_sent_timestamp,
                    new_body,
                    edit_timestamp,
                })
            }
            Some(content::Content::DeleteForMeSync(_)) => {
                // Own-device only (`sync_to_own_devices`, device inbox): on
                // the mailbox it can only come from a peer.
                Err(ServiceError::Validation(
                    "DeleteForMeSync received on DM mailbox path".into(),
                ))
            }
            Some(content::Content::SavedMessageSync(_)) => {
                // Own-device only (saved.rs sends it to our device inbox): on
                // the mailbox it can only come from a peer.
                Err(ServiceError::Validation(
                    "SavedMessageSync received on DM mailbox path".into(),
                ))
            }
            Some(content::Content::KeyExchange(kx)) => {
                // Key exchange received in DM mailbox (bootstrap response from peer)
                Ok(DmContent::KeyExchange {
                    // Only the decrypting caller knows it; `process_dm_message` sets it.
                    sender_device_id: 0,
                    conversation_id: kx.conversation_id,
                    ephemeral_pk: kx.ephemeral_pk,
                    timestamp: kx.timestamp,
                })
            }
            Some(content::Content::CallKey(call_key)) => {
                // Call key exchange with ephemeral keys for PFS
                // key_material contains ephemeral X25519 public key (32 bytes)
                let ephemeral_pk: [u8; 32] =
                    call_key.key_material.as_slice().try_into().map_err(|_| {
                        ServiceError::Validation(format!(
                            "Invalid ephemeral key length: {} (expected 32)",
                            call_key.key_material.len()
                        ))
                    })?;

                if call_key.is_offer {
                    // Offer from caller - they're initiating a call
                    Ok(DmContent::CallKeyOffer {
                        call_id: call_key.call_id,
                        ephemeral_public_key: ephemeral_pk,
                        timestamp: call_key.timestamp,
                        call_type: call_key.call_type,
                    })
                } else {
                    // Answer from callee - response to our offer
                    Ok(DmContent::CallKeyAnswer {
                        call_id: call_key.call_id,
                        ephemeral_public_key: ephemeral_pk,
                        timestamp: call_key.timestamp,
                    })
                }
            }
            Some(content::Content::GroupCallSignal(msg)) => {
                use crate::domain::call::GroupCallSignal;
                use crate::proto::content_v1::group_call_signal::Signal;
                let signal = match msg.signal {
                    Some(Signal::Ring(_)) => GroupCallSignal::Ring,
                    Some(Signal::KeyRequest(_)) => GroupCallSignal::KeyRequest,
                    Some(Signal::Key(update)) => {
                        let key: [u8; 32] =
                            update.key_material.as_slice().try_into().map_err(|_| {
                                ServiceError::Validation(format!(
                                    "Invalid group call key length: {} (expected 32)",
                                    update.key_material.len()
                                ))
                            })?;
                        // Epoch 0 is a participant's own starting key, never
                        // sent: `key_epoch >= 1` is declared in content.proto
                        // and enforced where the Content was decoded.
                        GroupCallSignal::Key {
                            key,
                            epoch: update.key_epoch,
                        }
                    }
                    None => return Err(ServiceError::Validation("empty group call signal".into())),
                };
                Ok(DmContent::GroupCallSignal {
                    call_id: msg.call_id,
                    channel_id: msg.channel_id,
                    signal,
                    timestamp: msg.timestamp,
                })
            }
            Some(content::Content::CallHangup(hangup)) => {
                use crate::proto::content_v1::call_hangup;
                // Call hangup notification received
                let reason = match call_hangup::Reason::try_from(hangup.reason) {
                    Ok(call_hangup::Reason::Normal) => CallHangupReason::Normal,
                    Ok(call_hangup::Reason::Declined) => CallHangupReason::Declined,
                    Ok(call_hangup::Reason::Busy) => CallHangupReason::Busy,
                    Ok(call_hangup::Reason::Timeout) => CallHangupReason::Timeout,
                    Ok(call_hangup::Reason::NetworkError) => CallHangupReason::NetworkError,
                    Ok(call_hangup::Reason::AcceptedElsewhere) => {
                        CallHangupReason::AcceptedElsewhere
                    }
                    Ok(call_hangup::Reason::DeclinedElsewhere) => {
                        CallHangupReason::DeclinedElsewhere
                    }
                    _ => CallHangupReason::Normal,
                };
                Ok(DmContent::CallHangup {
                    call_id: hangup.call_id,
                    reason,
                    timestamp: hangup.timestamp,
                })
            }
            Some(content::Content::SenderKey(_))
            | Some(content::Content::GroupEncrypted(_))
            | Some(content::Content::SenderKeyRequest(_)) => {
                // Group messages shouldn't come through DM path
                Err(ServiceError::Validation(
                    "Group message received on DM path".into(),
                ))
            }
            // Anonymous group messages (NDSS'21 style - sent via pairwise DM)
            // Secret-chat bootstraps travel only over the secret channel (sealed
            // to one device inbox, or a secret mailbox), never a friend DM.
            Some(content::Content::AnonGroupInvite(_))
            | Some(content::Content::AnonGroupJoinAck(_))
            | Some(content::Content::AnonDmRequest(_))
            | Some(content::Content::AnonDmAccept(_)) => Err(ServiceError::Validation(
                "Secret-chat bootstrap over a friend DM refused".into(),
            )),
            Some(content::Content::AnonGroupMemberList(list)) => {
                Ok(DmContent::AnonGroupMemberList {
                    group_id: list.group_id,
                    members: list.members,
                    timestamp: list.timestamp,
                })
            }
            Some(content::Content::AnonGroupLeave(leave)) => Ok(DmContent::AnonGroupLeave {
                group_id: leave.group_id,
                timestamp: leave.timestamp,
            }),
            Some(content::Content::AnonGroupKick(kick)) => Ok(DmContent::AnonGroupKick {
                group_id: kick.group_id,
                kicked_user_id: kick.kicked_user_id,
                admin_signature: kick.admin_signature,
                timestamp: kick.timestamp,
            }),
            Some(content::Content::AnonGroupUpdate(update)) => {
                let timestamp = update.timestamp;
                Ok(DmContent::AnonGroupUpdate {
                    group_id: update.group_id.clone(),
                    update,
                    timestamp,
                })
            }
            Some(content::Content::AnonGroupMessage(msg)) => Ok(DmContent::AnonGroupMessage {
                group_id: msg.group_id,
                inner_content: msg.inner_content.ok_or_else(|| {
                    ServiceError::Validation("AnonGroupMessage missing inner_content".into())
                })?,
            }),
            // Secret chats are delivered only by their own mailbox poll
            // (inbox/polling/anon_dm.rs), which enforces the device binding.
            Some(content::Content::AnonDmMessage(_)) => Ok(DmContent::SecretChatOffMailbox),
            // L4 mailbox rotation announce: peer minted a new mailbox and wants
            // future sends directed there. Does not touch the ratchet.
            Some(content::Content::DmMailboxRotation(rotation)) => Ok(DmContent::MailboxRotation {
                new_mailbox_pk: rotation.new_mailbox_pk,
            }),
            // Session switch after the peer reset its sessions with us: the
            // decrypt that produced this already adopted the new session.
            Some(content::Content::NullMessage(_)) => Ok(DmContent::NullMessage),
            _ => Err(ServiceError::Validation(
                "Unknown or empty content type".into(),
            )),
        }
    }
}

#[cfg(test)]
mod thread_id_tests {
    //! Regression test for a real bug: `DmContent::Text` (and, downstream,
    //! `dispatch_dm_content` in `inbox/polling/dm.rs`) silently dropped
    //! `DataMessage.thread_id` on the mailbox (Shield friend DM) receive
    //! path. The ServerDm/channel receive path (`gateway/mod.rs`) already
    //! carried this through -- see its `thread_id: data_message.thread_id
    //! .parse().ok()` and comment. The mailbox path never got the same fix, so a
    //! thread reply received over that transport was filed into the main
    //! conversation instead of the open thread panel.
    use super::*;
    use crate::proto::content_v1::DataMessage;

    /// An edit keeps its own timestamp (`EditMessage.data_message.timestamp`):
    /// edits order and date by it.
    #[test]
    fn parse_content_carries_the_edit_timestamp() {
        let content = Content {
            content: Some(content::Content::Edit(
                crate::proto::content_v1::EditMessage {
                    target_sent_timestamp: 100,
                    data_message: Some(DataMessage {
                        body: "fixed".into(),
                        timestamp: 150,
                        ..Default::default()
                    }),
                },
            )),
            ..Default::default()
        };
        match MessageGateway::parse_content(content).expect("parse_content failed") {
            DmContent::Edit {
                target_sent_timestamp,
                new_body,
                edit_timestamp,
            } => {
                assert_eq!(
                    (target_sent_timestamp, new_body.as_str(), edit_timestamp),
                    (100, "fixed", 150)
                );
            }
            other => panic!("expected DmContent::Edit, got {other:?}"),
        }
    }

    /// §124 1k: a ring and a key request parse with no key; a key parses
    /// only as a 32-byte rotation at epoch >= 1 (epoch 0 is a participant's
    /// own starting key and is never sent).
    #[test]
    fn group_call_signals_parse_and_epoch_zero_keys_are_refused() {
        use crate::domain::call::GroupCallSignal;
        use crate::services::gateway::build_group_call_signal_content;
        let parse =
            |s| MessageGateway::parse_content(build_group_call_signal_content("c", "ch", s));
        for signal in [
            GroupCallSignal::Ring,
            GroupCallSignal::KeyRequest,
            GroupCallSignal::Key {
                key: [3; 32],
                epoch: 2,
            },
        ] {
            match parse(signal).expect("parse_content failed") {
                DmContent::GroupCallSignal {
                    call_id,
                    channel_id,
                    signal: parsed,
                    ..
                } => {
                    assert_eq!((call_id.as_str(), channel_id.as_str()), ("c", "ch"));
                    assert_eq!(parsed, signal);
                }
                other => panic!("expected DmContent::GroupCallSignal, got {other:?}"),
            }
        }
        // Epoch 0 is refused by content.proto's `key_epoch >= 1`, where the
        // Content is decoded, before parse_content ever sees it.
        use prost::Message as _;
        let zero = build_group_call_signal_content(
            &uuid::Uuid::new_v4().to_string(),
            &uuid::Uuid::new_v4().to_string(),
            GroupCallSignal::Key {
                key: [3; 32],
                epoch: 0,
            },
        );
        assert!(
            matches!(
                crate::network::validation::protovalidate::decode_inbound_content(
                    &zero.encode_to_vec(),
                    "test"
                ),
                Err(
                    crate::network::validation::protovalidate::ContentValidationError::Violation {
                        rule: "uint32.gte",
                        ..
                    }
                )
            ),
            "epoch-0 key accepted"
        );
    }

    #[test]
    fn parse_content_refuses_secret_chat_bootstrap_over_friend_dm() {
        use crate::proto::content_v1::{
            AnonDmAccept, AnonDmRequest, AnonGroupInvite, AnonGroupJoinAck,
        };
        for inner in [
            content::Content::AnonDmRequest(AnonDmRequest::default()),
            content::Content::AnonDmAccept(AnonDmAccept::default()),
            content::Content::AnonGroupInvite(AnonGroupInvite::default()),
            content::Content::AnonGroupJoinAck(AnonGroupJoinAck::default()),
        ] {
            let parsed = MessageGateway::parse_content(Content {
                content: Some(inner),
                ..Default::default()
            });
            assert!(
                matches!(parsed, Err(ServiceError::Validation(_))),
                "a secret-chat bootstrap on the friend-DM path must be refused, got {parsed:?}"
            );
        }
    }

    #[test]
    fn parse_content_carries_thread_id_through() {
        let content = Content {
            content: Some(content::Content::DataMessage(DataMessage {
                body: "a reply".to_string(),
                thread_id: "11111111-1111-1111-1111-111111111111".to_string(),
                ..Default::default()
            })),
            ..Default::default()
        };

        let parsed = MessageGateway::parse_content(content).expect("parse_content failed");
        match parsed {
            DmContent::Text { thread_id, .. } => {
                assert_eq!(
                    thread_id,
                    Some("11111111-1111-1111-1111-111111111111".to_string()),
                    "thread_id must survive parse_content, or a mailbox-delivered reply \
                     is indistinguishable from a top-level message"
                );
            }
            other => panic!("expected DmContent::Text, got {other:?}"),
        }
    }

    #[test]
    fn parse_content_thread_id_is_none_when_not_a_reply() {
        let content = Content {
            content: Some(content::Content::DataMessage(DataMessage {
                body: "not a reply".to_string(),
                thread_id: String::new(),
                ..Default::default()
            })),
            ..Default::default()
        };

        let parsed = MessageGateway::parse_content(content).expect("parse_content failed");
        match parsed {
            DmContent::Text { thread_id, .. } => {
                assert_eq!(
                    thread_id, None,
                    "empty wire thread_id must map to None, not Some(\"\")"
                );
            }
            other => panic!("expected DmContent::Text, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod null_message_tests {
    use super::*;

    /// A peer's post-reset `NullMessage` is handled content with nothing in
    /// it — not the "unknown content" error the mailbox poll loop reports as
    /// a decrypt failure (`DmDecryptionFailed` + a delivery-issue marker).
    #[test]
    fn null_message_parses_as_handled_no_op() {
        let content = Content {
            content: Some(content::Content::NullMessage(
                crate::proto::content_v1::NullMessage::default(),
            )),
            padding: vec![0; 200],
            ..Default::default()
        };
        assert_eq!(content_type_name(&content.content), "NullMessage");
        let parsed =
            MessageGateway::parse_content(content).expect("NullMessage must not be an error");
        assert!(matches!(parsed, DmContent::NullMessage), "got {parsed:?}");
    }

    /// t5a: a secret chat's message carried by an account DM mailbox would
    /// skip the chat's device binding (and Tor); it is refused, not routed,
    /// and not reported as a decrypt failure.
    #[test]
    fn secret_chat_content_in_account_mailbox_is_refused() {
        let content = Content {
            content: Some(content::Content::AnonDmMessage(Box::new(
                crate::proto::content_v1::AnonDmMessage {
                    conversation_id: vec![7; 16],
                    inner_content: Some(Box::new(Content::default())),
                },
            ))),
            ..Default::default()
        };
        let parsed = MessageGateway::parse_content(content).expect("refusal is not an error");
        assert!(
            matches!(parsed, DmContent::SecretChatOffMailbox),
            "got {parsed:?}"
        );
    }
}
