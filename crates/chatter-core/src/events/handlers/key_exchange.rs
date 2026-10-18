use super::EventHandlers;
use crate::inbox::handlers::DomainEvent;
use tracing::{trace, warn};

impl EventHandlers {
    pub(super) fn handle_key_exchange_event(&self, event: DomainEvent) {
        match event {
            // Key Exchange Events
            // ================================================================
            DomainEvent::KeyExchangeReceived {
                sender_id,
                conversation_id,
                ..
            } => {
                // Key exchange is handled by crypto layer, not state
                trace!(?sender_id, ?conversation_id, "Key exchange received (handled by crypto layer)");
            }

            DomainEvent::SenderKeyReceived {
                sender_id,
                channel_id,
                ..
            } => {
                // Sender key is processed in the polling layer before this event is emitted.
                trace!(?sender_id, ?channel_id, "Sender key received (already processed)");
            }

            DomainEvent::GroupMailboxReceived {
                sender_id,
                channel_id,
                mailbox_public_key,
                timestamp,
            } => {
                // Process NDSS '21 mailbox announcement bundled with SKDM
                if let Some(ref coordinator) = self.group_coordinator {
                    if let Err(e) = coordinator.process_mailbox_key(
                        sender_id,
                        &channel_id,
                        mailbox_public_key,
                        timestamp,
                    ) {
                        warn!(?sender_id, ?channel_id, error = %e, "Failed to process group mailbox from SKDM");
                    } else {
                        trace!(?sender_id, ?channel_id, "Processed group mailbox from SKDM (NDSS '21)");
                    }
                } else {
                    trace!(?sender_id, ?channel_id, "GroupMailboxReceived event ignored (no coordinator)");
                }
            }

            // ================================================================
            // Signal-protocol sync events
            // ================================================================
            // Handled by the Signal service / inbox handlers that own the
            // ratchet + prekey stores. Nothing to mirror into UI state.
            DomainEvent::DmSessionHealed { .. }
            // The change is already parked in `pending_identity_changes`, which
            // is where the prompt reads it from; there is no state store to
            // mutate. The event exists so an OPEN conversation can raise the
            // prompt without waiting for a reload.
            | DomainEvent::PeerIdentityChanged { .. }
            | DomainEvent::PeerDevicesChanged { .. } => {}

            _ => unreachable!("event dispatched to wrong handler category"),
        }
    }
}
