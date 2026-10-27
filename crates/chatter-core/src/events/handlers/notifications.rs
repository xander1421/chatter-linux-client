use super::EventHandlers;
use crate::inbox::handlers::DomainEvent;
use tracing::trace;

impl EventHandlers {
    pub(super) fn handle_notifications_event(&self, event: DomainEvent) {
        match event {
            DomainEvent::DeviceLinked {
                device_id,
                device_name,
                approved_by_self,
                ..
            } => {
                // UI thread surfaces a toast / refreshes Devices modal via
                // event bus subscription. State stores have nothing to update
                // here — the device list lives on the server and is fetched
                // on demand by the modal.
                trace!(
                    device_id,
                    device_name = ?device_name,
                    approved_by_self,
                    "DeviceLinked: handled by UI thread"
                );
            }

            // ================================================================
            // Call Events
            // ================================================================
            DomainEvent::CallHangupReceived {
                call_id, peer_id, ..
            } => {
                // Call hangup is handled by the UI thread in calls.rs via event bus subscription.
                // No state store updates needed here.
                trace!(
                    ?call_id,
                    ?peer_id,
                    "CallHangupReceived: handled by UI thread"
                );
            }

            DomainEvent::CallKeyAnswerReceived {
                call_id, peer_id, ..
            } => {
                // Call key answer is handled by the UI thread in calls.rs via event bus subscription.
                // No state store updates needed here.
                trace!(
                    ?call_id,
                    ?peer_id,
                    "CallKeyAnswerReceived: handled by UI thread"
                );
            }

            DomainEvent::GroupCallSignalReceived {
                call_id,
                sender_id,
                signal,
                ..
            } => {
                // Handled by the call key policy listener (client/mod.rs).
                // No state store updates needed here.
                trace!(
                    ?call_id,
                    ?sender_id,
                    ?signal,
                    "GroupCallSignalReceived: handled by the call key policy"
                );
            }

            // UI-Specific Events (handled by UI thread)
            // ================================================================
            DomainEvent::CallKeyOfferReceived {
                call_id, peer_id, ..
            } => {
                // Incoming call offer - handled by UI thread for call modal.
                trace!(
                    ?call_id,
                    ?peer_id,
                    "CallKeyOfferReceived: handled by UI thread"
                );
            }

            DomainEvent::FriendRequestsUpdated {
                ref received,
                ref sent,
            } => {
                // Friend requests update - handled by UI thread for friend list.
                trace!(
                    received_count = received.len(),
                    sent_count = sent.len(),
                    "FriendRequestsUpdated: handled by UI thread"
                );
            }

            DomainEvent::FriendAccepted { friend_id } => {
                // Friend accepted - handled by UI thread to add DM to sidebar.
                trace!(?friend_id, "FriendAccepted: handled by UI thread");
            }

            DomainEvent::AnonGroupUpdated {
                group_id,
                updated_by,
                ..
            } => {
                // Anon group update - handled by UI thread for group settings.
                trace!(
                    ?group_id,
                    ?updated_by,
                    "AnonGroupUpdated: handled by UI thread"
                );
            }

            // ================================================================
            // User Presence Events
            // ================================================================
            DomainEvent::UserPresenceChanged {
                user_id,
                presence,
                timestamp,
            } => {
                // Presence updates are handled by the UI directly
                trace!(?user_id, ?presence, ?timestamp, "User presence changed");
                // TODO: Consider adding a presence state store if needed for caching
            }

            DomainEvent::UserProfileUpdated {
                user_id,
                display_name,
                status_message,
                timestamp,
                ..
            } => {
                // Already persisted by the user events stream handler; UI subscribes
                // directly to refresh display name + avatar caches.
                trace!(
                    ?user_id,
                    ?display_name,
                    ?status_message,
                    ?timestamp,
                    "Server-managed user profile updated"
                );
            }

            // ================================================================
            // Sync Events
            // ================================================================
            DomainEvent::SyncComplete { .. } => {
                // Sync completion is handled by the UI directly
            }
            // The Calls list and badge are read from storage by the UI.
            DomainEvent::CallHistoryChanged => {}
            DomainEvent::ConversationsRead { .. } => {}

            // ================================================================
            // Push Notifications
            // ================================================================
            DomainEvent::PushWakeup { .. } => {
                // Push wake-ups are consumed by the client (triggers sync) and
                // by the UI directly (e.g., to show a reconnect spinner).
                // No state-store update is required here.
            }

            // ================================================================
            _ => unreachable!("event dispatched to wrong handler category"),
        }
    }
}
