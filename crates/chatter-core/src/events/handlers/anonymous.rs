use super::EventHandlers;
use crate::inbox::handlers::DomainEvent;
use tracing::{trace, warn};

impl EventHandlers {
    pub(super) fn handle_anonymous_event(&self, event: DomainEvent) {
        match event {
            // ================================================================
            // Anonymous Group Events
            // ================================================================
            DomainEvent::AnonGroupInviteReceived {
                group_id,
                group_name,
                inviter_id,
                ..
            } => {
                // Anon group invite is handled by UI thread via event bus subscription.
                trace!(
                    ?group_id,
                    ?group_name,
                    ?inviter_id,
                    "AnonGroupInviteReceived: handled by UI thread"
                );
            }

            DomainEvent::AnonGroupJoined { group_id } => {
                // Anon group joined is handled by UI thread via event bus subscription.
                trace!(?group_id, "AnonGroupJoined: handled by UI thread");
            }

            DomainEvent::AnonGroupMemberJoined {
                group_id, user_id, ..
            } => {
                // Anon group member joined is handled by UI thread via event bus subscription.
                trace!(
                    ?group_id,
                    ?user_id,
                    "AnonGroupMemberJoined: handled by UI thread"
                );
            }

            DomainEvent::AnonGroupMemberLeft { group_id, user_id } => {
                // Anon group member left is handled by UI thread via event bus subscription.
                trace!(
                    ?group_id,
                    ?user_id,
                    "AnonGroupMemberLeft: handled by UI thread"
                );
            }

            DomainEvent::AnonGroupMemberKicked {
                group_id,
                user_id,
                kicked_by,
            } => {
                // Anon group member kicked is handled by UI thread via event bus subscription.
                trace!(
                    ?group_id,
                    ?user_id,
                    ?kicked_by,
                    "AnonGroupMemberKicked: handled by UI thread"
                );
            }

            DomainEvent::AnonGroupBootstrapComplete { group_id } => {
                // Anon group bootstrap complete is handled by UI thread via event bus subscription.
                trace!(
                    ?group_id,
                    "AnonGroupBootstrapComplete: handled by UI thread"
                );
            }

            DomainEvent::AnonGroupMemberListReceived {
                group_id,
                ref members,
            } => {
                // Profile sharing is NOT triggered here — Signal Protocol model:
                // profiles are shared once at join time (invite + JoinAck), and the
                // owner's member list broadcast already includes all profile data.
                // Re-sharing on every member list update is unnecessary and wasteful.
                trace!(
                    ?group_id,
                    member_count = members.len(),
                    "AnonGroupMemberListReceived: UI handles display refresh"
                );
            }

            // ================================================================
            // Anonymous DM Events
            // ================================================================
            DomainEvent::AnonDmRequestReceived {
                conversation_id,
                sender_id,
                ..
            } => {
                // Anon DM request is handled by UI thread via event bus subscription.
                trace!(
                    ?conversation_id,
                    ?sender_id,
                    "AnonDmRequestReceived: handled by UI thread"
                );
            }

            DomainEvent::AnonDmRequestAccepted {
                conversation_id,
                peer_id,
                ..
            } => {
                // Anon DM acceptance is handled by UI thread via event bus subscription.
                trace!(
                    ?conversation_id,
                    ?peer_id,
                    "AnonDmRequestAccepted: handled by UI thread"
                );
            }

            DomainEvent::AnonDmEnded { conversation_id } => {
                trace!(?conversation_id, "AnonDmEnded: handled by UI thread");
            }

            DomainEvent::AnonDmDeclined { conversation_id } => {
                trace!(?conversation_id, "AnonDmDeclined: handled by UI thread");
            }

            DomainEvent::AnonDmInitReceived {
                conversation_id,
                sender_id,
                sender_device_id,
                sender_display_name,
                sender_avatar,
                peer_mailbox_pk,
                peer_identity_key,
                peer_delivery_token,
                ref intro_message,
                ..
            } => {
                // NDSS'21 ECIES init — create conversation in DB (was previously in UI)
                if let Some(ref svc) = self.anon_dm_service {
                    let mut dm = crate::domain::anon_dm::AnonDm::from_request(
                        conversation_id,
                        sender_id,
                        peer_mailbox_pk,
                    )
                    .with_display_name(&sender_display_name)
                    .with_avatar_opt(sender_avatar);
                    // Bound to the initiator's device named in the init: our
                    // acceptance is sealed to it alone.
                    dm.peer_device_id = Some(sender_device_id);

                    match svc.create_incoming_conversation(
                        &dm,
                        &peer_identity_key,
                        &peer_delivery_token,
                    ) {
                        Ok(true) => {
                            // Both sealed requests and private friend-code init
                            // requests publish the same durable arrival event.
                            if let Some(bus) = self.domain_tx.as_ref().and_then(|tx| tx.upgrade()) {
                                let _ = bus.send(DomainEvent::AnonDmRequestReceived {
                                    conversation_id,
                                    sender_id,
                                    sender_mailbox_pk: peer_mailbox_pk,
                                    intro_message: Some(intro_message.clone()),
                                    profile_key: None,
                                    sender_display_name: dm.peer_display_name,
                                    sender_avatar: dm.peer_avatar,
                                });
                            }
                        }
                        Ok(false) => {} // Already stored, including catch-up redelivery.
                        Err(e) => warn!(
                            conversation_id = %conversation_id,
                            error = %e,
                            "Failed to create anon DM conversation from ECIES init"
                        ),
                    }
                } else {
                    trace!(
                        ?conversation_id,
                        intro_len = intro_message.len(),
                        "AnonDmInitReceived: no anon_dm_service configured"
                    );
                }
            }

            // ================================================================
            DomainEvent::AnonDmProfileUpdated {
                conversation_id,
                user_id,
                display_name,
                avatar,
                avatar_url,
                bio,
                timestamp,
            } => {
                // Profile updates for anonymous DMs
                trace!(
                    ?conversation_id,
                    ?user_id,
                    ?display_name,
                    has_avatar = avatar.is_some(),
                    ?avatar_url,
                    ?bio,
                    ?timestamp,
                    "Anonymous DM profile updated"
                );

                // The profile update is already stored by the inbox handler
                // This event is just for notifying other components
            }

            _ => unreachable!("event dispatched to wrong handler category"),
        }
    }
}
