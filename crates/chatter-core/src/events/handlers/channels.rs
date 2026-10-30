use super::EventHandlers;
use crate::inbox::handlers::DomainEvent;
use tracing::trace;

impl EventHandlers {
    pub(super) fn handle_channels_event(&self, event: DomainEvent) {
        match event {
            // Channel Member Events
            // ================================================================
            DomainEvent::ChannelMemberJoined {
                channel_id,
                user_id,
                display_name,
                message,
            } => {
                // Add system message to chat
                trace!(?channel_id, ?user_id, %display_name, "Member joined channel");
                self.message_state.append_message(channel_id, message);
            }

            DomainEvent::ChannelMemberLeft {
                channel_id,
                user_id,
                display_name,
                message,
            } => {
                // Add system message to chat
                trace!(?channel_id, ?user_id, %display_name, "Member left channel");
                self.message_state.append_message(channel_id, message);

                // Mark channel for sender key rotation — the departed member had
                // copies of our sender key, so we MUST rotate before sending any
                // more messages. This matches Signal's behavior: rotate on ANY
                // member departure (voluntary or forced) for forward secrecy.
                if let Some(ref signal_svc) = self.signal_service {
                    if let Err(e) = signal_svc.mark_channel_for_rotation(&channel_id) {
                        tracing::error!(
                            channel_id = %channel_id,
                            departed_user = %user_id,
                            error = %e,
                            "Failed to mark channel for sender key rotation after member departure"
                        );
                    } else {
                        tracing::info!(
                            channel_id = %channel_id,
                            departed_user = %user_id,
                            "Marked channel for sender key rotation after member departure"
                        );
                    }
                }
            }

            // Channel metadata (name/icon) and member-role changes are persisted
            // to the channel repo by ChannelEventService before this event fires;
            // the UI reads channel name / member list from the repo, so there is
            // no in-memory state to mutate here.
            DomainEvent::ChannelUpdated {
                channel_id, name, ..
            } => {
                trace!(?channel_id, %name, "Channel metadata updated");
            }
            DomainEvent::ChannelMemberUpdated {
                channel_id,
                user_id,
                role,
            } => {
                trace!(?channel_id, ?user_id, ?role, "Channel member role changed");
            }

            // Channel lifecycle (INFRA-29). ChannelEventService has already
            // written the authoritative repo state; only in-memory caches are
            // touched here.
            DomainEvent::ChannelCreated {
                channel_id,
                ref name,
            } => {
                trace!(?channel_id, %name, "Channel created");
            }
            DomainEvent::ChannelArchivedChanged {
                channel_id,
                archived,
            } => {
                trace!(?channel_id, archived, "Channel archive state changed");
            }
            DomainEvent::ChannelModerationAction {
                channel_id,
                user_id,
                ref action,
                is_self,
                ..
            } => {
                // A removal keeps the channel's history (it stays listed
                // read-only), so its cached messages stay too.
                trace!(?channel_id, ?user_id, %action, is_self, "Channel moderation action");
            }
            DomainEvent::ChannelInviteReceived {
                channel_id,
                inviter_id,
                ..
            } => {
                trace!(?channel_id, ?inviter_id, "Channel invitation received");
            }
            DomainEvent::ChannelInviteResponded {
                channel_id,
                accepted,
            } => {
                trace!(?channel_id, accepted, "Channel invitation answered");
            }
            DomainEvent::ChannelOperationFailed {
                channel_id,
                ref operation_id,
                ref code,
                ref message,
            } => {
                // Saga compensation: the server rolled the operation back and
                // ChannelEventService already re-read the channel. Log loudly —
                // this used to be discarded entirely, leaving optimistic state
                // wrong forever.
                tracing::warn!(
                    %channel_id, %operation_id, %code, %message,
                    "Channel operation failed server-side (rolled back)"
                );
            }
            DomainEvent::ChannelMembershipNotice {
                channel_id,
                ref notice,
            } => {
                // MembershipLog already recorded the status (send-blocked on
                // a failure or fork) and the exclusion; nothing to mutate here.
                tracing::warn!(%channel_id, ?notice, "Channel membership notice");
            }

            // ================================================================
            _ => unreachable!("event dispatched to wrong handler category"),
        }
    }
}
